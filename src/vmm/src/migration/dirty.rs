// Copyright 2026 VPSnet. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The dirty set of one migration round.
//!
//! Two sources record writes into guest memory:
//! - KVM's dirty log records the vCPUs' writes (and KVM's own: kvmclock, steal time). Without
//!   `KVM_CAP_MANUAL_DIRTY_LOG_PROTECT2` (Firecracker v1.17.0 never enables it),
//!   `KVM_GET_DIRTY_LOG` returns the log of a slot and clears it in the same ioctl, and
//!   write-protects the pages again, so a write after the ioctl is in the next log.
//! - Firecracker's own `AtomicBitmap` records device writes (virtio-blk reads, virtio-net RX,
//!   vsock, entropy, balloon discards). A marker sets its bit with `fetch_or`; the take swaps
//!   every 64-bit word with `fetch_and(0)` (`AtomicBitmap::get_and_reset`). Each mark therefore
//!   lands either in the word before the swap (this round) or after it (the next round), never
//!   in neither.
//!
//! Devices mark after their write (vm-memory, the async block engine at completion) or, for
//! buffers parsed ahead of the write, the caller re-marks the held buffers right after the take
//! (`DeviceManager::mark_virtio_pending_write_buffers_dirty`). Virtio queue rings are written
//! through raw pointers without marks; the caller marks the queue memory before the take, so the
//! rings are in every round.

use vm_memory::{GuestMemoryBackend, GuestMemoryRegion};

use super::{MigrationError, PAGE_SIZE};
use crate::utils::u64_to_usize;
use crate::vstate::memory::{GuestMemoryMmap, GuestRegionMmapExt, GuestRegionType};
use crate::vstate::vm::KvmVm;

/// The pages of one guest memory region selected for a round: bit `i` is the region's page `i`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionPages {
    /// Guest physical address of the region's first byte.
    pub base: u64,
    /// Number of pages in the region.
    pub pages: usize,
    /// One bit per page, 64 pages per word.
    pub bits: Vec<u64>,
}

impl RegionPages {
    /// A set with no page of a region of `pages` pages.
    pub fn none(base: u64, pages: usize) -> Self {
        RegionPages {
            base,
            pages,
            bits: vec![0; pages.div_ceil(64)],
        }
    }

    /// A set with every page of a region of `pages` pages.
    pub fn all(base: u64, pages: usize) -> Self {
        let mut set = Self::none(base, pages);
        set.or_words(0, &vec![u64::MAX; pages.div_ceil(64)]);
        set
    }

    /// Adds the pages set in `words` (bit `i` = page `page_offset + i`). Bits past the region's
    /// last page are ignored.
    pub fn or_words(&mut self, page_offset: usize, words: &[u64]) {
        for (w, &word) in words.iter().enumerate() {
            let mut word = word;
            while word != 0 {
                let bit = word.trailing_zeros() as usize;
                word &= word - 1;
                let page = page_offset + w * 64 + bit;
                if page < self.pages {
                    self.bits[page / 64] |= 1 << (page % 64);
                }
            }
        }
    }

    /// Whether page `page` is in the set.
    pub fn contains(&self, page: usize) -> bool {
        page < self.pages && self.bits[page / 64] & (1 << (page % 64)) != 0
    }

    /// Number of pages in the set.
    pub fn count(&self) -> u64 {
        self.bits.iter().map(|w| u64::from(w.count_ones())).sum()
    }

    /// The runs of consecutive pages in the set, as `(first page, number of pages)`.
    pub fn runs(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        let mut page = 0;
        std::iter::from_fn(move || {
            while page < self.pages {
                let word = self.bits[page / 64] >> (page % 64);
                if word == 0 {
                    page = (page / 64 + 1) * 64;
                    continue;
                }
                page += word.trailing_zeros() as usize;
                if page >= self.pages {
                    break;
                }
                let start = page;
                while page < self.pages && self.contains(page) {
                    page += 1;
                }
                return Some((start, page - start));
            }
            None
        })
    }
}

/// The pages of every guest memory region selected for a round, in guest memory order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoundPages {
    /// One entry per guest memory region.
    pub regions: Vec<RegionPages>,
}

impl RoundPages {
    /// Number of pages in the set.
    pub fn count(&self) -> u64 {
        self.regions.iter().map(RegionPages::count).sum()
    }

    /// Every page of `mem`.
    pub fn all(mem: &GuestMemoryMmap) -> Self {
        RoundPages {
            regions: mem
                .iter()
                .map(|r| RegionPages::all(r.start_addr().0, u64_to_usize(r.len()) / PAGE_SIZE))
                .collect(),
        }
    }
}

/// Takes and clears Firecracker's dirty bitmap of `region`, one atomic swap per 64-page word.
/// `None` when the region has no dirty tracking.
pub fn take_region_bitmap(region: &GuestRegionMmapExt) -> Option<Vec<u64>> {
    (**region)
        .bitmap()
        .as_ref()
        .map(|bitmap| bitmap.get_and_reset())
}

/// Checks that every region of `mem` can be migrated by protocol 1: DRAM only (no virtio-mem
/// hotplug region, whose unplugged slots are PROT_NONE), dirty tracking on.
pub fn check_memory(mem: &GuestMemoryMmap) -> Result<(), MigrationError> {
    for region in mem.iter() {
        if region.region_type != GuestRegionType::Dram {
            return Err(MigrationError::Unsupported(
                "a memory hotplug (virtio-mem) region".into(),
            ));
        }
        if (**region).bitmap().is_none() {
            return Err(MigrationError::DirtyTrackingOff);
        }
        if region.len() % PAGE_SIZE as u64 != 0 || region.start_addr().0 % PAGE_SIZE as u64 != 0 {
            return Err(MigrationError::Unsupported(
                "a memory region not aligned to 4 KiB".into(),
            ));
        }
    }
    Ok(())
}

/// Takes the dirty set of every region of `vm` and resets it, in one step per region: the
/// slots' KVM dirty logs (each read clears it) and Firecracker's bitmap (atomic swap).
///
/// The caller copies the pages AFTER this returns. A write after the take, including one into a
/// page while it is being copied, is in the next take.
pub fn take_dirty_pages(vm: &KvmVm) -> Result<RoundPages, MigrationError> {
    let mem = vm.guest_memory();
    check_memory(mem)?;
    let mut regions = Vec::with_capacity(mem.num_regions());
    for region in mem.iter() {
        let base = region.start_addr().0;
        let pages = u64_to_usize(region.len()) / PAGE_SIZE;
        let mut set = RegionPages::none(base, pages);
        let fc_words = take_region_bitmap(region).ok_or(MigrationError::DirtyTrackingOff)?;
        set.or_words(0, &fc_words);
        for slot in region.plugged_slots() {
            let kvm_words = vm
                .fd()
                .get_dirty_log(slot.slot, slot.slice.len())
                .map_err(|err| MigrationError::DirtyLog(slot.slot, err))?;
            let offset = u64_to_usize(slot.guest_addr.0 - base) / PAGE_SIZE;
            set.or_words(offset, &kvm_words);
        }
        regions.push(set);
    }
    Ok(RoundPages { regions })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use vm_memory::{Bytes, GuestAddress};

    use super::*;
    use crate::vmm_config::machine_config::HugePageConfig;
    use crate::vstate::memory::anonymous;
    use crate::vstate::memory::test_utils::into_region_ext;

    fn tracked_mem(pages: usize) -> GuestMemoryMmap {
        into_region_ext(
            anonymous(
                std::iter::once((GuestAddress(0), pages * PAGE_SIZE)),
                true,
                HugePageConfig::None,
            )
            .unwrap(),
        )
    }

    #[test]
    fn test_region_pages() {
        let mut set = RegionPages::none(0x1000, 130);
        assert_eq!(set.count(), 0);
        assert_eq!(set.runs().count(), 0);
        set.or_words(0, &[0b1011, 0]);
        // offset 62: bits 0 and 1 are pages 62 and 63, bit 64 (word 1, bit 0) is page 126,
        // bit 66 is page 128; page 130 and later do not exist.
        set.or_words(62, &[0b11, 0b1_0101]);
        assert_eq!(
            set.runs().collect::<Vec<_>>(),
            vec![(0, 2), (3, 1), (62, 2), (126, 1), (128, 1)]
        );
        assert_eq!(set.count(), 7);
        assert!(set.contains(128) && !set.contains(129) && !set.contains(130));

        let all = RegionPages::all(0, 130);
        assert_eq!(all.count(), 130);
        assert_eq!(all.runs().collect::<Vec<_>>(), vec![(0, 130)]);
        assert_eq!(
            RegionPages::all(0, 128).runs().collect::<Vec<_>>(),
            vec![(0, 128)]
        );
    }

    // The take never loses a mark: markers set bits while a reader swaps the words out; the
    // union of everything the reader took (plus a last take after the markers stopped) is
    // exactly the set of marked pages.
    #[test]
    fn test_take_loses_no_mark_under_concurrent_markers() {
        const PAGES: usize = 4096;
        const MARKERS: usize = 4;
        let mem = Arc::new(tracked_mem(PAGES));
        let stop = Arc::new(AtomicBool::new(false));

        let markers: Vec<_> = (0..MARKERS)
            .map(|m| {
                let mem = mem.clone();
                thread::spawn(move || {
                    let mut marked = BTreeSet::new();
                    let mut x: u64 = 0x9e37_79b9_7f4a_7c15 ^ (m as u64 + 1);
                    for _ in 0..200_000 {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        let page = u64_to_usize(x % PAGES as u64);
                        let region = mem.iter().next().unwrap();
                        (**region)
                            .bitmap()
                            .as_ref()
                            .unwrap()
                            .set_addr_range(page * PAGE_SIZE, 1);
                        marked.insert(page);
                    }
                    marked
                })
            })
            .collect();

        let reader = {
            let mem = mem.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                let mut taken = RegionPages::none(0, PAGES);
                let mut takes = 0;
                while !stop.load(Ordering::Acquire) {
                    let words = take_region_bitmap(mem.iter().next().unwrap()).unwrap();
                    taken.or_words(0, &words);
                    takes += 1;
                }
                (taken, takes)
            })
        };

        let mut marked = BTreeSet::new();
        for m in markers {
            marked.extend(m.join().unwrap());
        }
        stop.store(true, Ordering::Release);
        let (mut taken, takes) = reader.join().unwrap();
        assert!(takes > 1);
        taken.or_words(0, &take_region_bitmap(mem.iter().next().unwrap()).unwrap());

        let taken: BTreeSet<_> = (0..PAGES).filter(|&p| taken.contains(p)).collect();
        assert_eq!(taken, marked);
    }

    // The round protocol converges with concurrent device-style writers (write, then mark):
    // rounds of take-then-copy while writers run, then a last take-then-copy after they stop,
    // leave the copy byte-identical to the source.
    #[test]
    fn test_take_then_copy_converges_with_concurrent_writers() {
        const PAGES: usize = 512;
        const WRITERS: u64 = 4;
        let mem = Arc::new(tracked_mem(PAGES));
        let stop = Arc::new(AtomicBool::new(false));

        let writers: Vec<_> = (0..WRITERS)
            .map(|w| {
                let mem = mem.clone();
                let stop = stop.clone();
                thread::spawn(move || {
                    let mut x: u64 = 0x2545_f491_4f6c_dd1d ^ (w + 1);
                    let mut writes = 0u64;
                    while !stop.load(Ordering::Acquire) {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        let page = x % PAGES as u64;
                        let offset = (x >> 32) % (PAGE_SIZE as u64 - 8);
                        // vm-memory writes the bytes, then marks the page dirty.
                        mem.write_obj(x, GuestAddress(page * PAGE_SIZE as u64 + offset))
                            .unwrap();
                        writes += 1;
                    }
                    writes
                })
            })
            .collect();

        let mut copy = vec![0u8; PAGES * PAGE_SIZE];
        let copy_pages = |copy: &mut Vec<u8>, set: &RegionPages| {
            for (start, len) in set.runs() {
                let range = start * PAGE_SIZE..(start + len) * PAGE_SIZE;
                mem.read_slice(&mut copy[range.clone()], GuestAddress(range.start as u64))
                    .unwrap();
            }
        };
        // Rounds while the writers run: at least 200, and until the rounds carried pages
        // written concurrently many times over.
        let mut rounds = 0;
        let mut sent = 0;
        while rounds < 200 || sent < 20 * PAGES as u64 {
            let mut set = RegionPages::none(0, PAGES);
            set.or_words(0, &take_region_bitmap(mem.iter().next().unwrap()).unwrap());
            sent += set.count();
            copy_pages(&mut copy, &set);
            rounds += 1;
        }
        stop.store(true, Ordering::Release);
        let writes: u64 = writers.into_iter().map(|w| w.join().unwrap()).sum();
        assert!(writes > 0);

        // The pause: the last round after the writers stopped.
        let mut last = RegionPages::none(0, PAGES);
        last.or_words(0, &take_region_bitmap(mem.iter().next().unwrap()).unwrap());
        copy_pages(&mut copy, &last);

        let mut source = vec![0u8; PAGES * PAGE_SIZE];
        mem.read_slice(&mut source, GuestAddress(0)).unwrap();
        assert!(source == copy, "the copy differs from the source");
    }

    // A page modified after its copy within the same round is in the next round's set; a page
    // not touched again is not.
    #[test]
    fn test_page_modified_after_its_copy_is_in_next_round() {
        let mem = tracked_mem(8);
        mem.write_obj(1u64, GuestAddress(0)).unwrap();
        mem.write_obj(1u64, GuestAddress(3 * PAGE_SIZE as u64))
            .unwrap();

        let mut round1 = RegionPages::none(0, 8);
        round1.or_words(0, &take_region_bitmap(mem.iter().next().unwrap()).unwrap());
        assert_eq!(round1.runs().collect::<Vec<_>>(), vec![(0, 1), (3, 1)]);

        // Page 3 is copied, then written again before the round ends.
        let mut page = [0u8; PAGE_SIZE];
        mem.read_slice(&mut page, GuestAddress(3 * PAGE_SIZE as u64))
            .unwrap();
        mem.write_obj(2u64, GuestAddress(3 * PAGE_SIZE as u64))
            .unwrap();

        let mut round2 = RegionPages::none(0, 8);
        round2.or_words(0, &take_region_bitmap(mem.iter().next().unwrap()).unwrap());
        assert_eq!(round2.runs().collect::<Vec<_>>(), vec![(3, 1)]);
    }

    // On a KVM VM the take reads (and clears) the slots' dirty logs and Firecracker's bitmap.
    #[test]
    fn test_take_dirty_pages_on_kvm_vm() {
        let mut vm = crate::vstate::vm::tests::setup_vm();
        let regions = anonymous(
            [
                (GuestAddress(0), 64 * PAGE_SIZE),
                (GuestAddress(0x10_0000), 32 * PAGE_SIZE),
            ]
            .into_iter(),
            true,
            HugePageConfig::None,
        )
        .unwrap();
        vm.register_dram_memory_regions(regions).unwrap();

        // Registration may leave marks; the first take clears them.
        take_dirty_pages(&vm).unwrap();
        let set = take_dirty_pages(&vm).unwrap();
        assert_eq!(set.count(), 0);
        assert_eq!(set.regions.len(), 2);
        assert_eq!(set.regions[1].base, 0x10_0000);
        assert_eq!(set.regions[1].pages, 32);

        // A device-style write (vm-memory marks Firecracker's bitmap).
        let mem = vm.guest_memory();
        mem.write_obj(7u64, GuestAddress(0x10_0000 + 5 * PAGE_SIZE as u64))
            .unwrap();
        let set = take_dirty_pages(&vm).unwrap();
        assert_eq!(set.count(), 1);
        assert!(set.regions[1].contains(5));
        assert_eq!(take_dirty_pages(&vm).unwrap().count(), 0);

        // Without dirty tracking the take is refused.
        let untracked = crate::vstate::vm::tests::setup_vm_with_memory(16 * PAGE_SIZE);
        assert!(matches!(
            take_dirty_pages(&untracked),
            Err(MigrationError::DirtyTrackingOff)
        ));
    }

    #[test]
    fn test_check_memory() {
        assert!(matches!(
            check_memory(&crate::test_utils::single_region_mem(PAGE_SIZE)),
            Err(MigrationError::DirtyTrackingOff)
        ));
        check_memory(&tracked_mem(4)).unwrap();
        let all = RoundPages::all(&tracked_mem(4));
        assert_eq!(all.count(), 4);
    }
}
