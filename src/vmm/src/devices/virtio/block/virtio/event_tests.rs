// Copyright 2026 VPSnet. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

// Event-driven tests of the direct I/O bounce path.
//
// Every test here drives the device the way the VMM event loop does: the guest side writes
// descriptors and kicks the queue eventfd; the device side only reacts to events that really
// fire (the io_uring completion eventfd, the rate limiter's timerfd, a guest kick). No test
// calls `drain` to make progress: a request that waits for an event that can no longer happen
// shows up as a STALL panic.
#![allow(
    clippy::undocumented_unsafe_blocks,
    clippy::cast_possible_truncation,
    clippy::too_many_arguments,
    missing_docs
)]

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use vmm_sys_util::tempfile::TempFile;

use super::CacheType;
use super::device::{FileEngineType, VirtioBlock, VirtioBlockConfig};
use super::io::FileEngine;
use super::request::RequestHeader;
use crate::devices::virtio::block::persist::BlockConstructorArgs;
use crate::devices::virtio::device::VirtioDevice;
use crate::devices::virtio::generated::virtio_blk::{
    VIRTIO_BLK_S_IOERR, VIRTIO_BLK_S_OK, VIRTIO_BLK_T_DISCARD, VIRTIO_BLK_T_FLUSH, VIRTIO_BLK_T_IN,
    VIRTIO_BLK_T_OUT,
};
use crate::devices::virtio::queue::{VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};
use crate::devices::virtio::test_utils::{VirtQueue, default_interrupt};
use crate::logger::IncMetric;
use crate::rate_limiter::RateLimiter;
use crate::snapshot::Persist;
use crate::test_utils::single_region_mem;
use crate::vstate::memory::{Bytes, GuestAddress, GuestMemoryMmap};

const QSIZE: u16 = 256;
const HDR_BASE: u64 = 0x10000; // 0x20 per slot
const STATUS_BASE: u64 = 0x20000; // 0x10 per slot
const DISC_BASE: u64 = 0x30000; // 0x20 per slot (discard range)
const DATA_BASE: u64 = 0x100000;
const SLOT_DATA: u64 = 3 << 20;

static NEXT_ID: AtomicU32 = AtomicU32::new(0);

fn drive_id() -> String {
    format!("ev-{}", NEXT_ID.fetch_add(1, Ordering::SeqCst))
}

/// A backing of `len` bytes filled with `fill`: a regular file in /tmp (O_DIRECT capable).
fn backing(len: usize, fill: u8) -> TempFile {
    let f = TempFile::new().unwrap();
    f.as_file().write_all(&vec![fill; len]).unwrap();
    f.as_file().sync_all().unwrap();
    f
}

fn make_block(
    path: &str,
    et: FileEngineType,
    discard: bool,
    mem: &GuestMemoryMmap,
    vq: &VirtQueue,
) -> VirtioBlock {
    let config = VirtioBlockConfig {
        drive_id: drive_id(),
        path_on_host: path.to_string(),
        is_root_device: false,
        partuuid: None,
        is_read_only: false,
        discard,
        cache_type: CacheType::Writeback,
        rate_limiter: None,
        direct_write: true,
        file_engine_type: et,
        blk_size: Some(512),
        topology: None,
    };
    let mut block = VirtioBlock::new(config).unwrap();
    assert!(
        block.disk.file_engine.dio().is_some(),
        "direct I/O expected"
    );
    let feats = block.avail_features();
    block.set_acked_features(feats);
    block.queues[0] = vq.create_queue();
    block.activate(mem.clone(), default_interrupt()).unwrap();
    block
}

/// The guest side of the one queue.
struct Guest<'a> {
    vq: VirtQueue<'a>,
    mem: &'a GuestMemoryMmap,
    avail: u16,
    used_seen: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Read,
    Write,
    Discard,
    Flush,
}

impl<'a> Guest<'a> {
    fn new(mem: &'a GuestMemoryMmap) -> Self {
        Guest {
            vq: VirtQueue::new(GuestAddress(0), mem, QSIZE),
            mem,
            avail: 0,
            used_seen: 0,
        }
    }

    fn data_addr(slot: u16, misalign: u64) -> u64 {
        DATA_BASE + SLOT_DATA * u64::from(slot) + misalign
    }

    /// Put request `slot` (descriptors 3*slot..) in the avail ring (not kicked).
    fn submit(&mut self, slot: u16, kind: Kind, off: u64, len: u32, addr: u64) {
        let hdr = GuestAddress(HDR_BASE + 0x20 * u64::from(slot));
        let status = GuestAddress(STATUS_BASE + 0x10 * u64::from(slot));
        let t = match kind {
            Kind::Read => VIRTIO_BLK_T_IN,
            Kind::Write => VIRTIO_BLK_T_OUT,
            Kind::Discard => VIRTIO_BLK_T_DISCARD,
            Kind::Flush => VIRTIO_BLK_T_FLUSH,
        };
        let sector = if kind == Kind::Discard { 0 } else { off >> 9 };
        self.mem
            .write_obj(RequestHeader::new(t, sector), hdr)
            .unwrap();
        self.mem.write_obj(0xffu8, status).unwrap();
        let d = 3 * slot;
        match kind {
            Kind::Flush => {
                self.vq.dtable[d as usize].set(hdr.0, 16, VIRTQ_DESC_F_NEXT, d + 2);
            }
            Kind::Discard => {
                let range = GuestAddress(DISC_BASE + 0x20 * u64::from(slot));
                self.mem.write_obj(off >> 9, range).unwrap();
                self.mem
                    .write_obj(len >> 9, GuestAddress(range.0 + 8))
                    .unwrap();
                self.mem
                    .write_obj(0u32, GuestAddress(range.0 + 12))
                    .unwrap();
                self.vq.dtable[d as usize].set(hdr.0, 16, VIRTQ_DESC_F_NEXT, d + 1);
                self.vq.dtable[d as usize + 1].set(range.0, 16, VIRTQ_DESC_F_NEXT, d + 2);
            }
            Kind::Read | Kind::Write => {
                let flags = if kind == Kind::Read {
                    VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE
                } else {
                    VIRTQ_DESC_F_NEXT
                };
                self.vq.dtable[d as usize].set(hdr.0, 16, VIRTQ_DESC_F_NEXT, d + 1);
                self.vq.dtable[d as usize + 1].set(addr, len, flags, d + 2);
            }
        }
        self.vq.dtable[d as usize + 2].set(status.0, 1, VIRTQ_DESC_F_WRITE, 0);
        self.vq.avail.ring[usize::from(self.avail % QSIZE)].set(d);
        self.avail = self.avail.wrapping_add(1);
        self.vq.avail.idx.set(self.avail);
    }

    fn kick(&self, block: &mut VirtioBlock) {
        block.queue_evts[0].write(1).unwrap();
        block.process_queue_event();
    }

    fn status(&self, slot: u16) -> u8 {
        self.mem
            .read_obj(GuestAddress(STATUS_BASE + 0x10 * u64::from(slot)))
            .unwrap()
    }

    /// New used entries: (slot, used len).
    fn used(&mut self) -> Vec<(u16, u32)> {
        let mut out = vec![];
        while self.used_seen != self.vq.used.idx.get() {
            let e = self.vq.used.ring[usize::from(self.used_seen % QSIZE)].get();
            self.used_seen = self.used_seen.wrapping_add(1);
            assert_eq!(e.id % 3, 0, "used id {} is not a request head", e.id);
            out.push(((e.id / 3) as u16, e.len));
        }
        out
    }
}

/// One round of the VMM event loop: wait up to `wait` for the completion eventfd or the rate
/// limiter's timer and handle what fired. Returns whether an event fired.
fn poll_once(block: &mut VirtioBlock, wait: Duration) -> bool {
    let cfd = match &block.disk.file_engine {
        FileEngine::Async(e) => Some(e.completion_evt().as_raw_fd()),
        FileEngine::Sync(_) => None,
    };
    let rfd = block.rate_limiter.as_raw_fd();
    let mut fds = vec![];
    if let Some(c) = cfd {
        fds.push(libc::pollfd {
            fd: c,
            events: libc::POLLIN,
            revents: 0,
        });
    }
    fds.push(libc::pollfd {
        fd: rfd,
        events: libc::POLLIN,
        revents: 0,
    });
    let n = unsafe {
        libc::poll(
            fds.as_mut_ptr(),
            fds.len() as libc::nfds_t,
            wait.as_millis() as i32,
        )
    };
    if n <= 0 {
        return false;
    }
    for p in &fds {
        if p.revents & libc::POLLIN != 0 {
            if Some(p.fd) == cfd {
                block.process_async_completion_event();
            } else {
                block.process_rate_limiter_event();
            }
        }
    }
    true
}

/// Run the event loop until `done` holds; panic on a STALL (no event for `stall` while
/// `done` does not hold).
fn run_until(
    block: &mut VirtioBlock,
    g: &mut Guest,
    stall: Duration,
    used: &mut Vec<(u16, u32)>,
    mut done: impl FnMut(&[(u16, u32)]) -> bool,
) {
    used.extend(g.used());
    let mut last = Instant::now();
    while !done(used) {
        if poll_once(block, Duration::from_millis(20)) {
            last = Instant::now();
        } else if last.elapsed() > stall {
            panic!(
                "STALL: no event for {:?}; used so far {:?}; hold={} bounce={} next_avail={}",
                stall,
                used,
                block.bounce_hold.is_some(),
                block.disk.file_engine.bounce_in_progress(),
                block.queues[0].next_avail.0
            );
        }
        used.extend(g.used());
    }
}

fn wait_used(block: &mut VirtioBlock, g: &mut Guest, n: usize) -> Vec<(u16, u32)> {
    let mut used = vec![];
    run_until(block, g, Duration::from_secs(5), &mut used, |u| {
        u.len() >= n
    });
    used
}

/// Wait (without handling it) until the completion eventfd is readable.
fn wait_cqe_posted(block: &VirtioBlock) {
    let FileEngine::Async(e) = &block.disk.file_engine else {
        return;
    };
    let mut p = libc::pollfd {
        fd: e.completion_evt().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let n = unsafe { libc::poll(&mut p, 1, 5000) };
    assert_eq!(n, 1, "the completion was never posted");
}

fn file_bytes(path: &str, off: u64, len: usize) -> Vec<u8> {
    // O_DIRECT: never trust the host page cache of a direct drive's backing.
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
        .unwrap();
    let start = off - off % 4096;
    let end = (off + len as u64).div_ceil(4096) * 4096;
    let layout = std::alloc::Layout::from_size_align((end - start) as usize, 4096).unwrap();
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    let buf = unsafe { std::slice::from_raw_parts_mut(ptr, layout.size()) };
    f.seek(SeekFrom::Start(start)).unwrap();
    f.read_exact(buf).unwrap();
    let out = buf[(off - start) as usize..(off - start) as usize + len].to_vec();
    unsafe { std::alloc::dealloc(ptr, layout) };
    out
}

fn ok(g: &Guest, slots: &[u16]) {
    for s in slots {
        assert_eq!(
            u32::from(g.status(*s)),
            VIRTIO_BLK_S_OK,
            "slot {s} status {}",
            g.status(*s)
        );
    }
}

fn once_each(used: &[(u16, u32)], n: usize) {
    let mut ids: Vec<u16> = used.iter().map(|u| u.0).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), used.len(), "a request completed twice: {used:?}");
    assert_eq!(used.len(), n, "{used:?}");
}

// ---------------------------------------------------------------------------------------------
// 1. Lost wake-ups: a held request must be served by the completions alone, with a guest that
//    submits nothing further.
// ---------------------------------------------------------------------------------------------

/// [W aligned block 1, S 512 B into block 1]: S waits for W; only W's completion event resumes.
#[test]
fn vd2_hold_resumed_by_completion_event_only() {
    let mem = single_region_mem(0x1000000);
    let mut g = Guest::new(&mem);
    let f = backing(1 << 20, 0x11);
    let path = f.as_path().to_str().unwrap().to_string();
    let mut block = make_block(&path, FileEngineType::Async, false, &mem, &g.vq);
    mem.write_slice(&[0xaa; 4096], GuestAddress(Guest::data_addr(0, 0)))
        .unwrap();
    mem.write_slice(&[0x33; 512], GuestAddress(Guest::data_addr(1, 0)))
        .unwrap();
    g.submit(0, Kind::Write, 4096, 4096, Guest::data_addr(0, 0));
    g.submit(1, Kind::Write, 4096 + 1536, 512, Guest::data_addr(1, 0));
    g.kick(&mut block);
    assert!(block.bounce_hold.is_some(), "S must be held behind W");
    let used = wait_used(&mut block, &mut g, 2);
    once_each(&used, 2);
    ok(&g, &[0, 1]);
    let mut want = vec![0xaa; 4096];
    want[1536..2048].copy_from_slice(&[0x33; 512]);
    assert_eq!(file_bytes(&path, 4096, 4096), want);
    assert!(block.bounce_hold.is_none());
}

/// W's completion is already posted (eventfd readable, CQE not popped) when S is processed:
/// S is held on a write that is already complete; the pending event must still resume it.
#[test]
fn vd2_hold_recorded_after_completion_posted() {
    let mem = single_region_mem(0x1000000);
    let mut g = Guest::new(&mem);
    let f = backing(1 << 20, 0x11);
    let path = f.as_path().to_str().unwrap().to_string();
    let mut block = make_block(&path, FileEngineType::Async, false, &mem, &g.vq);
    mem.write_slice(&[0xaa; 4096], GuestAddress(Guest::data_addr(0, 0)))
        .unwrap();
    mem.write_slice(&[0x44; 512], GuestAddress(Guest::data_addr(1, 0)))
        .unwrap();
    g.submit(0, Kind::Write, 8192, 4096, Guest::data_addr(0, 0));
    g.kick(&mut block);
    wait_cqe_posted(&block);
    // The guest submits S now; the device processes it before the completion event.
    g.submit(1, Kind::Write, 8192 + 512, 512, Guest::data_addr(1, 0));
    g.kick(&mut block);
    assert!(
        block.bounce_hold.is_some(),
        "S held on W's slab entry (CQE not popped)"
    );
    let used = wait_used(&mut block, &mut g, 2);
    once_each(&used, 2);
    ok(&g, &[0, 1]);
    let mut want = vec![0xaa; 4096];
    want[512..1024].copy_from_slice(&[0x44; 512]);
    assert_eq!(file_bytes(&path, 8192, 4096), want);
}

/// The only thing in flight is the bounce's own transfer: [S1 RMW block 1, R2 misaligned read
/// of block 5]. R2 waits for the one buffer.
#[test]
fn vd2_hold_on_bounce_only() {
    for et in [FileEngineType::Async, FileEngineType::Sync] {
        let mem = single_region_mem(0x1000000);
        let mut g = Guest::new(&mem);
        let f = backing(1 << 20, 0x11);
        let path = f.as_path().to_str().unwrap().to_string();
        let mut block = make_block(&path, et, false, &mem, &g.vq);
        mem.write_slice(&[0x5e; 512], GuestAddress(Guest::data_addr(0, 0)))
            .unwrap();
        g.submit(0, Kind::Write, 4096 + 3584, 512, Guest::data_addr(0, 0));
        g.submit(
            1,
            Kind::Read,
            5 * 4096 + 512,
            1024,
            Guest::data_addr(1, 0x200),
        );
        g.kick(&mut block);
        let used = wait_used(&mut block, &mut g, 2);
        once_each(&used, 2);
        ok(&g, &[0, 1]);
        let mut got = vec![0u8; 1024];
        mem.read_slice(&mut got, GuestAddress(Guest::data_addr(1, 0x200)))
            .unwrap();
        assert_eq!(got, vec![0x11; 1024], "{et:?}");
    }
}

/// A chain of holds of both kinds: [W, S1, W2, S2, Rsub, Wfar] on block 3 (Wfar elsewhere).
/// Serial (queue) order: W, S1, W2, S2; Wfar lands; all once.
#[test]
fn vd2_hold_chain() {
    for et in [FileEngineType::Async, FileEngineType::Sync] {
        let mem = single_region_mem(0x2000000);
        let mut g = Guest::new(&mem);
        let f = backing(1 << 20, 0x11);
        let path = f.as_path().to_str().unwrap().to_string();
        let mut block = make_block(&path, et, false, &mem, &g.vq);
        let b = 3 * 4096;
        let fills = [0xa0u8, 0xa1, 0xa2, 0xa3, 0, 0xa5];
        for (s, v) in fills.iter().enumerate() {
            mem.write_slice(&[*v; 4096], GuestAddress(Guest::data_addr(s as u16, 0)))
                .unwrap();
        }
        g.submit(0, Kind::Write, b, 4096, Guest::data_addr(0, 0));
        g.submit(1, Kind::Write, b + 512, 512, Guest::data_addr(1, 0));
        g.submit(2, Kind::Write, b, 4096, Guest::data_addr(2, 0));
        g.submit(3, Kind::Write, b + 1024, 512, Guest::data_addr(3, 0));
        g.submit(4, Kind::Read, b + 512, 1536, Guest::data_addr(4, 0x200));
        g.submit(5, Kind::Write, 40 * 4096, 4096, Guest::data_addr(5, 0));
        g.kick(&mut block);
        let used = wait_used(&mut block, &mut g, 6);
        once_each(&used, 6);
        ok(&g, &[0, 1, 2, 3, 4, 5]);
        let mut want = vec![0xa2u8; 4096];
        want[1024..1536].copy_from_slice(&[0xa3; 512]);
        assert_eq!(file_bytes(&path, b, 4096), want, "{et:?}");
        assert_eq!(file_bytes(&path, 40 * 4096, 4096), vec![0xa5; 4096]);
        let mut got = vec![0u8; 1536];
        mem.read_slice(&mut got, GuestAddress(Guest::data_addr(4, 0x200)))
            .unwrap();
        // The read went after S2 (queue held) or concurrently with it: before S2's completion
        // its range [512, 2048) of the block is W2 everywhere but S2's sector.
        assert_eq!(&got[..512], &[0xa2; 512][..], "{et:?}");
        assert!(got[512..1024] == [0xa3; 512] || got[512..1024] == [0xa2; 512]);
        assert_eq!(&got[1024..], &[0xa2; 512][..], "{et:?}");
    }
}

/// Snapshot in place (pause, prepare_save, resume) while a request is held, with a guest that
/// submits nothing further. Real resume kicks every device; this test also checks the case
/// without the kick (only the eventfd left signalled by the drained completions).
#[test]
fn vd2_hold_across_snapshot_in_place() {
    for kick in [true, false] {
        let mem = single_region_mem(0x1000000);
        let mut g = Guest::new(&mem);
        let f = backing(1 << 20, 0x11);
        let path = f.as_path().to_str().unwrap().to_string();
        let mut block = make_block(&path, FileEngineType::Async, false, &mem, &g.vq);
        mem.write_slice(&[0x77; 512], GuestAddress(Guest::data_addr(1, 0)))
            .unwrap();
        g.submit(0, Kind::Write, 4096, 4096, Guest::data_addr(0, 0));
        g.submit(1, Kind::Write, 4096 + 512, 512, Guest::data_addr(1, 0));
        g.kick(&mut block);
        assert!(block.bounce_hold.is_some());
        block.prepare_save();
        if kick {
            block.kick();
            block.process_queue_event();
        }
        let used = wait_used(&mut block, &mut g, 2);
        once_each(&used, 2);
        ok(&g, &[0, 1]);
        assert_eq!(file_bytes(&path, 4096 + 512, 512), vec![0x77; 512]);
    }
}

// ---------------------------------------------------------------------------------------------
// 2. Read-modify-write exclusion.
// ---------------------------------------------------------------------------------------------

/// Two RMWs of one block with an aligned write of it between them, all in flight at once; and
/// two RMWs of one block with an aligned write of ANOTHER block between them (the two
/// neighbouring sectors must both survive: no serial order loses either).
#[test]
fn vd2_two_rmw_one_block() {
    for et in [FileEngineType::Async, FileEngineType::Sync] {
        let mem = single_region_mem(0x2000000);
        let mut g = Guest::new(&mem);
        let f = backing(1 << 20, 0x11);
        let path = f.as_path().to_str().unwrap().to_string();
        let mut block = make_block(&path, et, false, &mem, &g.vq);
        let b = 6 * 4096;
        mem.write_slice(&[0xc1; 512], GuestAddress(Guest::data_addr(0, 0)))
            .unwrap();
        mem.write_slice(&[0xbb; 4096], GuestAddress(Guest::data_addr(1, 0)))
            .unwrap();
        mem.write_slice(&[0xc2; 512], GuestAddress(Guest::data_addr(2, 0x37)))
            .unwrap();
        g.submit(0, Kind::Write, b + 512, 512, Guest::data_addr(0, 0));
        g.submit(1, Kind::Write, b, 4096, Guest::data_addr(1, 0));
        g.submit(2, Kind::Write, b + 1024, 512, Guest::data_addr(2, 0x37));
        g.kick(&mut block);
        let used = wait_used(&mut block, &mut g, 3);
        once_each(&used, 3);
        ok(&g, &[0, 1, 2]);
        let got = file_bytes(&path, b, 4096);
        // S2 does not overlap W at the sector level? It does (W is the whole block): any order
        // of W and S2 is legal, but S2's sector must be S2's or W's bytes, never 0x11/0xc1.
        assert!(got[1024..1536] == [0xc2; 512] || got[1024..1536] == [0xbb; 512]);
        assert!(got[512..1024] == [0xc1; 512] || got[512..1024] == [0xbb; 512]);
        for (i, x) in got.iter().enumerate() {
            if !(512..1536).contains(&i) {
                assert_eq!(*x, 0xbb, "{et:?} byte {i}");
            }
        }

        // Neighbours: [S1 sector 1 of block c, W of block d, S2 sector 2 of block c, S3 last
        // sector of block c], all concurrent; disjoint sectors: all must survive.
        let c = 9 * 4096;
        mem.write_slice(&[0xd1; 512], GuestAddress(Guest::data_addr(3, 0)))
            .unwrap();
        mem.write_slice(&[0xdd; 4096], GuestAddress(Guest::data_addr(4, 0)))
            .unwrap();
        mem.write_slice(&[0xd2; 512], GuestAddress(Guest::data_addr(5, 0)))
            .unwrap();
        mem.write_slice(&[0xd3; 512], GuestAddress(Guest::data_addr(6, 1)))
            .unwrap();
        g.submit(3, Kind::Write, c + 512, 512, Guest::data_addr(3, 0));
        g.submit(4, Kind::Write, c + 4096, 4096, Guest::data_addr(4, 0));
        g.submit(5, Kind::Write, c + 1024, 512, Guest::data_addr(5, 0));
        g.submit(6, Kind::Write, c + 3584, 512, Guest::data_addr(6, 1));
        g.kick(&mut block);
        let used = wait_used(&mut block, &mut g, 4);
        once_each(&used, 4);
        ok(&g, &[3, 4, 5, 6]);
        let mut want = vec![0x11u8; 8192];
        want[512..1024].copy_from_slice(&[0xd1; 512]);
        want[1024..1536].copy_from_slice(&[0xd2; 512]);
        want[3584..4096].copy_from_slice(&[0xd3; 512]);
        want[4096..].copy_from_slice(&[0xdd; 4096]);
        assert_eq!(file_bytes(&path, c, 8192), want, "{et:?}");
    }
}

/// A large RMW (several 1 MiB chunks, partial head and tail block) with neighbours in its
/// head and tail blocks submitted around it, and a discard of a disjoint sector of its tail
/// block: every sector is exact (no chunk of the RMW may write back stale neighbour bytes).
#[test]
fn vd2_large_rmw_with_neighbours() {
    for et in [FileEngineType::Async, FileEngineType::Sync] {
        for order in 0..3 {
            let mem = single_region_mem(0x4000000);
            let mut g = Guest::new(&mem);
            let disk = 8 << 20;
            let f = backing(disk, 0x11);
            let path = f.as_path().to_str().unwrap().to_string();
            let mut block = make_block(&path, et, true, &mem, &g.vq);
            let head = 4096u64; // RMW starts at sector 1 of block 1
            let xoff = head + 512;
            let xlen: u32 = (2 << 20) + 2048; // ends inside a block
            let xend = xoff + u64::from(xlen);
            let tail_blk = xend - xend % 4096;
            let xdata: Vec<u8> = (0..xlen).map(|i| (i % 251) as u8 ^ 0x5a).collect();
            mem.write_slice(&xdata, GuestAddress(Guest::data_addr(1, 0x40)))
                .unwrap();
            mem.write_slice(&[0xe0; 512], GuestAddress(Guest::data_addr(0, 0)))
                .unwrap();
            mem.write_slice(&[0xe2; 512], GuestAddress(Guest::data_addr(2, 0)))
                .unwrap();
            let n0 = (0u16, Kind::Write, head, 512u32, Guest::data_addr(0, 0));
            let x = (1u16, Kind::Write, xoff, xlen, Guest::data_addr(1, 0x40));
            let n2 = (
                2u16,
                Kind::Write,
                xend + 512,
                512u32,
                Guest::data_addr(2, 0),
            );
            let d3 = (3u16, Kind::Discard, tail_blk + 3584, 512u32, 0u64);
            let reqs = match order {
                0 => [n0, x, n2, d3],
                1 => [x, n0, d3, n2],
                _ => [d3, n2, x, n0],
            };
            for (s, k, o, l, a) in reqs {
                g.submit(s, k, o, l, a);
            }
            g.kick(&mut block);
            let used = wait_used(&mut block, &mut g, 4);
            once_each(&used, 4);
            ok(&g, &[0, 1, 2, 3]);
            let lo = head;
            let hi = tail_blk + 4096;
            let got = file_bytes(&path, lo, (hi - lo) as usize);
            let mut want = vec![0x11u8; (hi - lo) as usize];
            want[0..512].copy_from_slice(&[0xe0; 512]);
            want[512..512 + xlen as usize].copy_from_slice(&xdata);
            let n2o = (xend + 512 - lo) as usize;
            want[n2o..n2o + 512].copy_from_slice(&[0xe2; 512]);
            let d3o = (tail_blk + 3584 - lo) as usize;
            want[d3o..d3o + 512].copy_from_slice(&[0; 512]);
            if got != want {
                let first = got.iter().zip(&want).position(|(a, b)| a != b).unwrap();
                panic!(
                    "{et:?} order {order}: first difference at disk offset {}",
                    lo + first as u64
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 3. Errors in the middle of a chunked bounce (RLIMIT_FSIZE turns one chunk's write short,
//    a later one EFBIG; truncation turns a chunk's read short).
// ---------------------------------------------------------------------------------------------

struct FsizeLimit(libc::rlimit);

impl FsizeLimit {
    fn set(limit: u64) -> Self {
        unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) };
        let mut old = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, &mut old) }, 0);
        let new = libc::rlimit {
            rlim_cur: limit,
            rlim_max: old.rlim_max,
        };
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &new) }, 0);
        FsizeLimit(old)
    }
}

impl Drop for FsizeLimit {
    fn drop(&mut self) {
        unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &self.0) };
        unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_DFL) };
    }
}

/// A 4 MiB RMW write at offset 512 whose third chunk gets a short write; queued behind it: a
/// misaligned read (waits for the buffer), an aligned write overlapping the RMW span (waits for
/// the RMW) and an aligned write elsewhere. The RMW must fail (IOERR, never success), every
/// other request must complete OK once, nothing may stay held or in progress.
#[test]
fn vd2_error_in_middle_chunk_of_write() {
    for et in [FileEngineType::Async, FileEngineType::Sync] {
        let mem = single_region_mem(0x4000000);
        let mut g = Guest::new(&mem);
        let f = backing(8 << 20, 0x11);
        let path = f.as_path().to_str().unwrap().to_string();
        let mut block = make_block(&path, et, false, &mem, &g.vq);
        let xlen: u32 = 4 << 20;
        mem.write_slice(
            &vec![0x9c; xlen as usize],
            GuestAddress(Guest::data_addr(0, 0)),
        )
        .unwrap();
        mem.write_slice(&[0x5b; 4096], GuestAddress(Guest::data_addr(3, 0)))
            .unwrap();
        mem.write_slice(&[0x6b; 4096], GuestAddress(Guest::data_addr(4, 0)))
            .unwrap();
        let used = {
            let _lim = FsizeLimit::set((2 << 20) + 4096);
            g.submit(0, Kind::Write, 512, xlen, Guest::data_addr(0, 0));
            g.submit(
                2,
                Kind::Read,
                (6 << 20) + 512,
                512,
                Guest::data_addr(2, 0x200),
            );
            g.submit(3, Kind::Write, 4096, 4096, Guest::data_addr(3, 0));
            g.submit(4, Kind::Write, 1 << 20, 4096, Guest::data_addr(4, 0));
            g.kick(&mut block);
            wait_used(&mut block, &mut g, 4)
        };
        once_each(&used, 4);
        assert_eq!(
            u32::from(g.status(0)),
            VIRTIO_BLK_S_IOERR,
            "{et:?}: a short write must fail the request"
        );
        assert_eq!(
            block.metrics.write_bytes.count(),
            (2 << 20) + 4096 - 512 + 8192
        );
        assert_eq!(used.iter().find(|u| u.0 == 0).unwrap().1, 1);
        ok(&g, &[2, 3, 4]);
        assert_eq!(file_bytes(&path, 4096, 4096), vec![0x5b; 4096], "{et:?}");
        assert!(!block.disk.file_engine.bounce_in_progress());
        assert!(block.bounce_hold.is_none());
        // The drive goes on: a new RMW of the same span succeeds.
        mem.write_slice(&[0x3e; 512], GuestAddress(Guest::data_addr(5, 0)))
            .unwrap();
        g.submit(5, Kind::Write, (3 << 20) + 512, 512, Guest::data_addr(5, 0));
        g.kick(&mut block);
        let used = wait_used(&mut block, &mut g, 1);
        once_each(&used, 1);
        ok(&g, &[5]);
        assert_eq!(file_bytes(&path, (3 << 20) + 512, 512), vec![0x3e; 512]);
    }
}

/// A 3 MiB misaligned read whose second chunk reads short (the backing shrank under the
/// drive): IOERR with the copied prefix, the request behind it served.
#[test]
fn vd2_error_in_middle_chunk_of_read() {
    for et in [FileEngineType::Async, FileEngineType::Sync] {
        let mem = single_region_mem(0x4000000);
        let mut g = Guest::new(&mem);
        let f = backing(8 << 20, 0x11);
        let path = f.as_path().to_str().unwrap().to_string();
        let mut block = make_block(&path, et, false, &mem, &g.vq);
        f.as_file().set_len((1 << 20) + (512 << 10)).unwrap();
        g.submit(0, Kind::Read, 512, 3 << 20, Guest::data_addr(0, 0x200));
        g.submit(2, Kind::Read, 1024, 512, Guest::data_addr(2, 0x200));
        g.kick(&mut block);
        let used = wait_used(&mut block, &mut g, 2);
        once_each(&used, 2);
        assert_eq!(u32::from(g.status(0)), VIRTIO_BLK_S_IOERR, "{et:?}");
        let len0 = used.iter().find(|u| u.0 == 0).unwrap().1;
        let copied = (1 << 20) - 512;
        assert_eq!(len0, copied + 1, "{et:?}: copied guest bytes plus status");
        ok(&g, &[2]);
        assert!(!block.disk.file_engine.bounce_in_progress());
    }
}

// ---------------------------------------------------------------------------------------------
// 4. Snapshot and restore while a request is held, and while a chunked RMW runs.
// ---------------------------------------------------------------------------------------------

fn save_restore(block: VirtioBlock, mem: &GuestMemoryMmap) -> VirtioBlock {
    let mut block = block;
    block.prepare_save();
    assert!(!block.disk.file_engine.bounce_in_progress());
    let state = block.save();
    let bytes = bitcode::serialize(&state).unwrap();
    drop(block);
    let state = bitcode::deserialize(&bytes).unwrap();
    let mut restored =
        VirtioBlock::restore(BlockConstructorArgs { mem: mem.clone() }, &state).unwrap();
    restored.activate(mem.clone(), default_interrupt()).unwrap();
    // Vmm::resume_vm -> kick_virtio_devices
    restored.kick();
    restored.process_queue_event();
    restored
}

#[test]
fn vd2_snapshot_restore_while_held_and_while_rmw_runs() {
    // Held: [W block 1, S into block 1].
    let mem = single_region_mem(0x4000000);
    let mut g = Guest::new(&mem);
    let f = backing(8 << 20, 0x11);
    let path = f.as_path().to_str().unwrap().to_string();
    let mut block = make_block(&path, FileEngineType::Async, false, &mem, &g.vq);
    mem.write_slice(&[0xaa; 4096], GuestAddress(Guest::data_addr(0, 0)))
        .unwrap();
    mem.write_slice(&[0x5a; 512], GuestAddress(Guest::data_addr(1, 0)))
        .unwrap();
    g.submit(0, Kind::Write, 4096, 4096, Guest::data_addr(0, 0));
    g.submit(1, Kind::Write, 4096 + 1024, 512, Guest::data_addr(1, 0));
    g.kick(&mut block);
    assert!(block.bounce_hold.is_some());
    let mut block = save_restore(block, &mem);
    let used = wait_used(&mut block, &mut g, 2);
    once_each(&used, 2);
    ok(&g, &[0, 1]);
    let mut want = vec![0xaa; 4096];
    want[1024..1536].copy_from_slice(&[0x5a; 512]);
    assert_eq!(file_bytes(&path, 4096, 4096), want);

    // Running: a 3 MiB RMW, a write of its tail block held behind it.
    let xlen: u32 = (3 << 20) + 1024;
    let xdata: Vec<u8> = (0..xlen).map(|i| (i % 249) as u8).collect();
    mem.write_slice(&xdata, GuestAddress(Guest::data_addr(2, 0)))
        .unwrap();
    mem.write_slice(&[0x7e; 4096], GuestAddress(Guest::data_addr(5, 0)))
        .unwrap();
    let xoff = (1 << 20) + 512;
    let tail = (xoff + u64::from(xlen)) / 4096 * 4096;
    g.submit(2, Kind::Write, xoff, xlen, Guest::data_addr(2, 0));
    g.submit(5, Kind::Write, tail, 4096, Guest::data_addr(5, 0));
    g.kick(&mut block);
    assert!(block.disk.file_engine.bounce_in_progress());
    let mut block = save_restore(block, &mem);
    let used = wait_used(&mut block, &mut g, 2);
    once_each(&used, 2);
    ok(&g, &[2, 5]);
    let got = file_bytes(&path, xoff, (tail - xoff) as usize);
    assert!(got[..] == xdata[..(tail - xoff) as usize]);
    assert_eq!(file_bytes(&path, tail, 4096), vec![0x7e; 4096]);
    drop(block);
}

// ---------------------------------------------------------------------------------------------
// 5. Rate limiter: a deferred request is charged once.
// ---------------------------------------------------------------------------------------------

/// A discard is charged one op and no bytes (`Request::rate_limit`), but a deferred one is
/// refunded its byte length too (`process_queue`): the bandwidth bucket gains bytes nobody
/// paid for.
#[test]
fn vd2_deferred_discard_refunds_bytes_never_charged() {
    let mem = single_region_mem(0x4000000);
    let mut g = Guest::new(&mem);
    let f = backing(8 << 20, 0x11);
    let path = f.as_path().to_str().unwrap().to_string();
    let mut block = make_block(&path, FileEngineType::Async, true, &mem, &g.vq);
    // 1 MiB of bandwidth, refilled over 1000 s (1 byte/ms): drift is negligible.
    block.rate_limiter = RateLimiter::new(1 << 20, 0, 1_000_000, 0, 0, 0);
    // Spend 900 KiB with an aligned write.
    g.submit(0, Kind::Write, 2 << 20, 900 << 10, Guest::data_addr(0, 0));
    g.kick(&mut block);
    let _ = wait_used(&mut block, &mut g, 1);
    let before = block.rate_limiter.bandwidth().unwrap().budget();
    // S: a 512-byte RMW of block 1 (charged 512 bytes); D: a 64 KiB discard from block 1
    // (charged 0 bytes), deferred behind S.
    g.submit(1, Kind::Write, 4096 + 512, 512, Guest::data_addr(1, 0));
    g.submit(2, Kind::Discard, 4096, 64 << 10, 0);
    g.kick(&mut block);
    let deferred = block.metrics.direct_bounce_deferred.count();
    let after_defer = block.rate_limiter.bandwidth().unwrap().budget();
    let _ = wait_used(&mut block, &mut g, 2);
    let after = block.rate_limiter.bandwidth().unwrap().budget();
    println!(
        "bandwidth budget: before {before}, after S charged and D deferred {after_defer}, after \
         both {after} (deferred {deferred}); a correct charge leaves {}",
        before - 512
    );
    assert_eq!(deferred, 1, "D was deferred behind the RMW");
    assert!(
        after <= before - 512 + 4096,
        "the bucket gained {} bytes that were never charged",
        after.cast_signed() - (before.cast_signed() - 512)
    );
}

// ---------------------------------------------------------------------------------------------
// 6. Randomized model test against an in-memory oracle.
// ---------------------------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[derive(Debug, Clone)]
struct Out {
    kind: Kind,
    off: u64,
    len: u32,
    addr: u64,
    expect: Option<Vec<u8>>,
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Random mixes of aligned, sub-block, misaligned-buffer and multi-chunk reads and writes,
/// discards and flushes at queue depth up to 64, against a shadow of the disk. Outstanding
/// requests never overlap at the sector level where one of them writes (a correct guest), but
/// they share backing blocks all the time (half of them hit the first 64 KiB). Every read must
/// return exactly the shadow (no write may be lost or torn, no read may see an older value),
/// every request completes once with OK, and at the end the whole backing equals the shadow.
/// Random in-place snapshots, save/restore into a new device and rate limiting are mixed in.
fn model(et: FileEngineType, seed: u64, iters: u64, dev: Option<&str>, rl: bool) {
    let mem = single_region_mem((DATA_BASE + SLOT_DATA * 64 + 0x10000) as usize);
    let mut g = Guest::new(&mem);
    let (tmp, path, disk_len, dalign) = match dev {
        Some(p) => {
            let mut f = std::fs::File::open(p).unwrap();
            let len = f.seek(SeekFrom::End(0)).unwrap();
            (None, p.to_string(), len, 4096u64)
        }
        None => {
            let f = backing(8 << 20, 0);
            let p = f.as_path().to_str().unwrap().to_string();
            (Some(f), p, 8 << 20, 512u64)
        }
    };
    let disk_len = disk_len.min(16 << 20);
    // Start from a known pattern.
    {
        let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let init: Vec<u8> = (0..disk_len).map(|i| (i / 512 % 253) as u8).collect();
        f.write_all(&init).unwrap();
        f.sync_all().unwrap();
    }
    let mut shadow: Vec<u8> = (0..disk_len).map(|i| (i / 512 % 253) as u8).collect();
    // Drop the page cache of a block device so that the first direct reads are exact.
    if dev.is_some() {
        let f = std::fs::File::open(&path).unwrap();
        unsafe {
            libc::ioctl(f.as_raw_fd(), 0x1261 /* BLKFLSBUF */)
        };
    }
    let mut block = make_block(&path, et, true, &mem, &g.vq);
    if rl {
        block.rate_limiter = if std::env::var("DIO_MODEL_RL").as_deref() == Ok("tight") {
            // 20 MiB/s and 200 ops/s: the limiter blocks all the time, held requests included.
            RateLimiter::new(2 << 20, 0, 100, 20, 0, 100)
        } else {
            RateLimiter::new(64 << 20, 0, 100, 2000, 0, 100)
        };
    }
    let mut rng = Rng(seed | 1);
    let mut out: BTreeMap<u16, Out> = BTreeMap::new();
    let mut free: Vec<u16> = (0..64).collect();
    let mut serial: u64 = 0;
    let mut stats = [0u64; 8];
    let mut last_progress = Instant::now();

    let overlaps = |out: &BTreeMap<u16, Out>, kind: Kind, off: u64, len: u64| -> bool {
        out.values().any(|o| {
            if o.kind == Kind::Flush {
                return false;
            }
            let either_writes = kind != Kind::Read || o.kind != Kind::Read;
            either_writes && o.off < off + len && off < o.off + u64::from(o.len)
        })
    };

    let handle_used = |g: &mut Guest,
                       out: &mut BTreeMap<u16, Out>,
                       free: &mut Vec<u16>,
                       stats: &mut [u64; 8]|
     -> usize {
        let used = g.used();
        for (slot, ulen) in &used {
            let o = out
                .remove(slot)
                .unwrap_or_else(|| panic!("slot {slot} completed but not outstanding"));
            let st = g.status(*slot);
            assert_eq!(u32::from(st), VIRTIO_BLK_S_OK, "{o:?} status {st}");
            if let Some(expect) = &o.expect {
                assert_eq!(*ulen, o.len + 1, "{o:?}");
                let mut got = vec![0u8; o.len as usize];
                g.mem.read_slice(&mut got, GuestAddress(o.addr)).unwrap();
                if got != *expect {
                    let first = got.iter().zip(expect).position(|(a, b)| a != b).unwrap();
                    panic!(
                        "READ MISMATCH {o:?} at disk offset {}: got {:#x} want {:#x}",
                        o.off + first as u64,
                        got[first],
                        expect[first]
                    );
                }
                stats[5] += 1;
            }
            free.push(*slot);
        }
        used.len()
    };

    for _ in 0..iters {
        let r = rng.below(100);
        if r < 50 {
            let k = 1 + rng.below(6);
            for _ in 0..k {
                if free.is_empty() {
                    break;
                }
                let kind = match rng.below(100) {
                    0..=39 => Kind::Read,
                    40..=81 => Kind::Write,
                    82..=89 => Kind::Discard,
                    _ => Kind::Flush,
                };
                let hot = rng.below(2) == 0;
                let span = if hot { 64 << 10 } else { disk_len };
                let (off, len) = match kind {
                    Kind::Flush => (0, 0),
                    Kind::Discard => {
                        let a = dalign;
                        let off = rng.below(span / a) * a;
                        let len = a * (1 + rng.below(16));
                        (off, len.min(disk_len - off))
                    }
                    _ => match rng.below(100) {
                        0..=29 => {
                            let off = rng.below(span / 4096) * 4096;
                            (off, 4096 * (1 + rng.below(8)))
                        }
                        30..=69 => {
                            let off = rng.below(span / 512) * 512;
                            (off, 512 * (1 + rng.below(7)))
                        }
                        70..=96 => {
                            let off = rng.below(span / 512) * 512;
                            (off, 512 * (1 + rng.below(64)))
                        }
                        _ => {
                            let off = rng.below(disk_len / 512 / 2) * 512;
                            (off, (1 << 20) + 512 * rng.below(3000))
                        }
                    },
                };
                let len = len.min(disk_len - off);
                if kind != Kind::Flush && (len == 0 || overlaps(&out, kind, off, len)) {
                    continue;
                }
                let slot = free.swap_remove(rng.below(free.len() as u64) as usize);
                let mis = match rng.below(3) {
                    0 => 0,
                    1 => 512 * rng.below(8),
                    _ => rng.below(4096),
                };
                let addr = Guest::data_addr(slot, mis);
                let mut expect = None;
                match kind {
                    Kind::Write => {
                        serial += 1;
                        let data: Vec<u8> = (0..len)
                            .map(|i| ((serial * 37 + (off + i) / 512 * 3 + i) % 256) as u8)
                            .collect();
                        mem.write_slice(&data, GuestAddress(addr)).unwrap();
                        shadow[off as usize..(off + len) as usize].copy_from_slice(&data);
                        stats[1] += 1;
                        if !(off % 4096 == 0 && len % 4096 == 0) {
                            stats[2] += 1;
                        }
                    }
                    Kind::Read => {
                        mem.write_slice(&vec![0xee; len as usize], GuestAddress(addr))
                            .unwrap();
                        expect = Some(shadow[off as usize..(off + len) as usize].to_vec());
                        stats[0] += 1;
                    }
                    Kind::Discard => {
                        shadow[off as usize..(off + len) as usize].fill(0);
                        stats[3] += 1;
                    }
                    Kind::Flush => stats[4] += 1,
                }
                g.submit(slot, kind, off, len as u32, addr);
                out.insert(
                    slot,
                    Out {
                        kind,
                        off,
                        len: len as u32,
                        addr,
                        expect,
                    },
                );
            }
            g.kick(&mut block);
        } else if r < 90 {
            let wait = Duration::from_millis(rng.below(3));
            poll_once(&mut block, wait);
        } else if r < 94 {
            // Pause, snapshot in place, resume (kick).
            block.prepare_save();
            block.kick();
            block.process_queue_event();
            stats[6] += 1;
        } else if r < 96 {
            // Snapshot and restore into a new device object.
            block = save_restore(block, &mem);
            if rl {
                // The restored limiter keeps its state; nothing to do.
            }
            stats[7] += 1;
        }
        if handle_used(&mut g, &mut out, &mut free, &mut stats) > 0 || out.is_empty() {
            last_progress = Instant::now();
        } else if last_progress.elapsed() > Duration::from_secs(10) {
            panic!(
                "STALL: {} outstanding for 10 s: {:?}; hold={} bounce={}",
                out.len(),
                out,
                block.bounce_hold.is_some(),
                block.disk.file_engine.bounce_in_progress()
            );
        }
    }
    // Let everything complete through events only.
    let t = Instant::now();
    let mut last = Instant::now();
    while !out.is_empty() {
        if poll_once(&mut block, Duration::from_millis(20)) {
            last = Instant::now();
        }
        if handle_used(&mut g, &mut out, &mut free, &mut stats) > 0 {
            last = Instant::now();
        }
        assert!(
            last.elapsed() < Duration::from_secs(10),
            "STALL at the end: {} outstanding: {:?}; hold={} bounce={} t={:?}",
            out.len(),
            out,
            block.bounce_hold.is_some(),
            block.disk.file_engine.bounce_in_progress(),
            t.elapsed()
        );
    }
    assert!(block.bounce_hold.is_none());
    assert!(!block.disk.file_engine.bounce_in_progress());
    let m = &block.metrics;
    println!(
        "model {et:?} seed {seed} dev {dev:?} rl {rl}: reads {} (checked {}) writes {} \
         (sub-block or misaligned {}) discards {} flushes {} snapshots {} restores {}; device: \
         bounce reads {} bounce writes {} rmw {} deferred {} rl-throttled {}",
        stats[0],
        stats[5],
        stats[1],
        stats[2],
        stats[3],
        stats[4],
        stats[6],
        stats[7],
        m.direct_bounce_reads.count(),
        m.direct_bounce_writes.count(),
        m.direct_rmw_writes.count(),
        m.direct_bounce_deferred.count(),
        m.rate_limiter_throttled_events.count()
    );
    drop(block);
    let got = file_bytes(&path, 0, disk_len as usize);
    if got != shadow {
        let first = got.iter().zip(&shadow).position(|(a, b)| a != b).unwrap();
        panic!("FINAL MISMATCH at disk offset {first}");
    }
    drop(tmp);
}

#[test]
fn vd2_model_random() {
    let seeds = env_u64("DIO_MODEL_SEEDS", 4);
    let iters = env_u64("DIO_MODEL_ITERS", 600);
    let base = env_u64("DIO_MODEL_SEED", 0x2026_1120);
    let dev = std::env::var("DIO_MODEL_DEV").ok();
    for s in 0..seeds {
        for et in [FileEngineType::Async, FileEngineType::Sync] {
            for rl in [false, true] {
                let seed = base.wrapping_add(s * 7919);
                model(et, seed, iters, dev.as_deref(), rl);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 7. The ring refuses a bounce's transfer, or the device's own submission: the device comes
//    back to it by itself, without a guest notification.
// ---------------------------------------------------------------------------------------------

fn async_engine(block: &mut VirtioBlock) -> &mut super::io::AsyncFileEngine {
    match &mut block.disk.file_engine {
        FileEngine::Async(e) => e,
        FileEngine::Sync(_) => panic!("Async engine expected"),
    }
}

/// A 512-byte RMW alone; once its first transfer is in flight, the next three submissions fail
/// (EAGAIN). Only events drive the device (no guest kick, no drain): the owed transfer is
/// submitted again from the completion path and the RMW completes once, exactly.
#[test]
fn ev_owed_submit_retried_without_guest() {
    let mem = single_region_mem(0x1000000);
    let mut g = Guest::new(&mem);
    let f = backing(1 << 20, 0x11);
    let path = f.as_path().to_str().unwrap().to_string();
    let mut block = make_block(&path, FileEngineType::Async, false, &mem, &g.vq);
    mem.write_slice(&[0x5e; 512], GuestAddress(Guest::data_addr(0, 0)))
        .unwrap();
    g.submit(0, Kind::Write, 4096 + 512, 512, Guest::data_addr(0, 0));
    g.kick(&mut block);
    assert!(block.disk.file_engine.bounce_in_progress());
    async_engine(&mut block).fail_ring_for_test(0, 3);
    let used = wait_used(&mut block, &mut g, 1);
    once_each(&used, 1);
    ok(&g, &[0]);
    assert_eq!(
        async_engine(&mut block).ring_failures_left_for_test(),
        (0, 0)
    );
    assert!(!async_engine(&mut block).bounce_transfer_owed());
    let mut want = vec![0x11; 4096];
    want[512..1024].copy_from_slice(&[0x5e; 512]);
    assert_eq!(file_bytes(&path, 4096, 4096), want);
}

/// The same with a full submission queue: the next two pushes of the RMW's second transfer
/// fail; it is queued from the completion path later, never ended with a spurious IOERR.
#[test]
fn ev_owed_push_retried_without_guest() {
    let mem = single_region_mem(0x1000000);
    let mut g = Guest::new(&mem);
    let f = backing(1 << 20, 0x11);
    let path = f.as_path().to_str().unwrap().to_string();
    let mut block = make_block(&path, FileEngineType::Async, false, &mem, &g.vq);
    mem.write_slice(&[0x6f; 1024], GuestAddress(Guest::data_addr(0, 0)))
        .unwrap();
    g.submit(0, Kind::Write, 8192 + 1024, 1024, Guest::data_addr(0, 0));
    g.kick(&mut block);
    assert!(block.disk.file_engine.bounce_in_progress());
    async_engine(&mut block).fail_ring_for_test(2, 0);
    let used = wait_used(&mut block, &mut g, 1);
    once_each(&used, 1);
    ok(&g, &[0]);
    assert_eq!(
        async_engine(&mut block).ring_failures_left_for_test(),
        (0, 0)
    );
    let mut want = vec![0x11; 4096];
    want[1024..2048].copy_from_slice(&[0x6f; 1024]);
    assert_eq!(file_bytes(&path, 8192, 4096), want);
}

/// The device's own submission at the end of a queue pass fails: the request is not left to
/// the guest's next notification; the device signals itself and submits again.
#[test]
fn ev_kick_failure_retried_without_guest() {
    let mem = single_region_mem(0x1000000);
    let mut g = Guest::new(&mem);
    let f = backing(1 << 20, 0x11);
    let path = f.as_path().to_str().unwrap().to_string();
    let mut block = make_block(&path, FileEngineType::Async, false, &mem, &g.vq);
    mem.write_slice(&[0x7a; 4096], GuestAddress(Guest::data_addr(0, 0)))
        .unwrap();
    g.submit(0, Kind::Write, 4096, 4096, Guest::data_addr(0, 0));
    g.submit(1, Kind::Read, 0, 4096, Guest::data_addr(1, 0));
    async_engine(&mut block).fail_ring_for_test(0, 1);
    g.kick(&mut block);
    let used = wait_used(&mut block, &mut g, 2);
    once_each(&used, 2);
    ok(&g, &[0, 1]);
    assert_eq!(
        async_engine(&mut block).ring_failures_left_for_test(),
        (0, 0)
    );
    assert_eq!(file_bytes(&path, 4096, 4096), vec![0x7a; 4096]);
    assert!(!block.is_io_engine_throttled);
}

// ---------------------------------------------------------------------------------------------
// 8. PATCH of the backing with requests in flight and a request held. Everything accepted completes on the old backing, once; the held request runs on the
//    new one.
// ---------------------------------------------------------------------------------------------

#[test]
fn ev_patch_completes_requests_in_flight() {
    for et in [FileEngineType::Async, FileEngineType::Sync] {
        let mem = single_region_mem(0x4000000);
        let mut g = Guest::new(&mem);
        let f = backing(1 << 20, 0x11);
        let h = backing(1 << 20, 0x22);
        let path = f.as_path().to_str().unwrap().to_string();
        let path2 = h.as_path().to_str().unwrap().to_string();
        let mut block = make_block(&path, et, false, &mem, &g.vq);
        for s in 0..8u16 {
            mem.write_slice(
                &[0x40 + s as u8; 8192],
                GuestAddress(Guest::data_addr(s, 0)),
            )
            .unwrap();
            g.submit(
                s,
                Kind::Write,
                u64::from(s) * 8192,
                8192,
                Guest::data_addr(s, 0),
            );
        }
        g.kick(&mut block);
        block.update_disk_image(path2.clone()).unwrap();
        let used = wait_used(&mut block, &mut g, 8);
        once_each(&used, 8);
        ok(&g, &(0..8).collect::<Vec<_>>());
        for s in 0..8u64 {
            assert_eq!(
                file_bytes(&path, s * 8192, 8192),
                vec![0x40 + s as u8; 8192],
                "{et:?}"
            );
        }
        assert_eq!(file_bytes(&path2, 0, 65536), vec![0x22; 65536], "{et:?}");
        // Same-path re-open with requests in flight: they complete, their data is there.
        for s in 8..16u16 {
            mem.write_slice(
                &[0x40 + s as u8; 4096],
                GuestAddress(Guest::data_addr(s, 0)),
            )
            .unwrap();
            g.submit(
                s,
                Kind::Write,
                65536 + u64::from(s) * 4096,
                4096,
                Guest::data_addr(s, 0),
            );
        }
        g.kick(&mut block);
        block.update_disk_image(path2.clone()).unwrap();
        let used = wait_used(&mut block, &mut g, 8);
        once_each(&used, 8);
        for s in 8..16u64 {
            assert_eq!(
                file_bytes(&path2, 65536 + s * 4096, 4096),
                vec![0x40 + s as u8; 4096],
                "{et:?}"
            );
        }
    }
}

/// W (aligned, block 1) in flight and S (RMW of block 1) held behind it when the backing is
/// replaced: W completes on the old backing, then S runs on the new one (reading the new
/// backing's block around its sector), each once.
#[test]
fn ev_patch_with_request_held() {
    let mem = single_region_mem(0x1000000);
    let mut g = Guest::new(&mem);
    let f = backing(1 << 20, 0x11);
    let h = backing(1 << 20, 0x22);
    let path = f.as_path().to_str().unwrap().to_string();
    let path2 = h.as_path().to_str().unwrap().to_string();
    let mut block = make_block(&path, FileEngineType::Async, false, &mem, &g.vq);
    mem.write_slice(&[0xaa; 4096], GuestAddress(Guest::data_addr(0, 0)))
        .unwrap();
    mem.write_slice(&[0x33; 512], GuestAddress(Guest::data_addr(1, 0)))
        .unwrap();
    g.submit(0, Kind::Write, 4096, 4096, Guest::data_addr(0, 0));
    g.submit(1, Kind::Write, 4096 + 1536, 512, Guest::data_addr(1, 0));
    g.kick(&mut block);
    assert!(block.bounce_hold.is_some(), "S must be held behind W");
    block.update_disk_image(path2.clone()).unwrap();
    let used = wait_used(&mut block, &mut g, 2);
    once_each(&used, 2);
    ok(&g, &[0, 1]);
    assert_eq!(
        file_bytes(&path, 4096, 4096),
        vec![0xaa; 4096],
        "W on the old backing"
    );
    let mut want = vec![0x22; 4096];
    want[1536..2048].copy_from_slice(&[0x33; 512]);
    assert_eq!(file_bytes(&path2, 4096, 4096), want, "S on the new backing");
    assert!(block.bounce_hold.is_none());
}

/// The first RMW spends half the initial byte burst. An overlapping writer
/// would exhaust both bursts and incur bandwidth debt, but is still in the avail ring.
#[test]
fn deferred_write_preserves_burst_and_debt_until_accepted() {
    let mem = single_region_mem(0x4000000);
    let mut g = Guest::new(&mem);
    let f = backing(4 << 20, 0x11);
    let mut block = make_block(
        f.as_path().to_str().unwrap(),
        FileEngineType::Async,
        false,
        &mem,
        &g.vq,
    );
    block.rate_limiter = RateLimiter::new(4096, 1024, 60_000, 100, 2, 60_000);
    g.submit(0, Kind::Write, 512, 512, Guest::data_addr(0, 0));
    g.submit(1, Kind::Write, 0, 8192, Guest::data_addr(1, 0));
    g.kick(&mut block);
    assert!(block.bounce_hold.is_some());
    let bw = block.rate_limiter.bandwidth().unwrap();
    let ops = block.rate_limiter.ops().unwrap();
    assert_eq!(
        (
            block.rate_limiter.is_blocked(),
            bw.one_time_burst(),
            bw.budget(),
            ops.one_time_burst(),
            ops.budget()
        ),
        (false, 512, 4096, 1, 100)
    );
    let used = wait_used(&mut block, &mut g, 2);
    once_each(&used, 2);
    ok(&g, &[0, 1]);
    assert!(
        block.rate_limiter.is_blocked(),
        "accepted oversized write incurs debt once"
    );
    assert_eq!(block.rate_limiter.bandwidth().unwrap().budget(), 0);
    assert_eq!(block.rate_limiter.ops().unwrap().one_time_burst(), 0);
    assert_eq!(block.rate_limiter.ops().unwrap().budget(), 100);
}

/// A misaligned 2 MiB read served in bounce chunks whose second chunk fails
/// (the backing shrank after the drive opened it). The device has put the first chunk's guest
/// bytes into guest memory; the used length must say so (virtio: the bytes written into the
/// device-writable part, here the data prefix plus the status byte).
#[test]
fn test_bounce_read_used_len_matches_guest_memory() {
    for et in [FileEngineType::Async, FileEngineType::Sync] {
        let mem = single_region_mem(0x4000000);
        let mut g = Guest::new(&mem);
        let f = backing(4 << 20, 0x11);
        let path = f.as_path().to_str().unwrap().to_string();
        let mut block = make_block(&path, et, false, &mem, &g.vq);
        f.as_file().set_len(1 << 20).unwrap();
        let addr = Guest::data_addr(0, 0x200);
        mem.write_slice(&vec![0xee; 2 << 20], GuestAddress(addr))
            .unwrap();
        g.submit(0, Kind::Read, 512, 2 << 20, addr);
        g.kick(&mut block);
        let used = wait_used(&mut block, &mut g, 1);
        assert_eq!(u32::from(g.status(0)), VIRTIO_BLK_S_IOERR, "{et:?}");
        let mut got = vec![0u8; 2 << 20];
        mem.read_slice(&mut got, GuestAddress(addr)).unwrap();
        let copied = got.iter().position(|&b| b != 0x11).unwrap_or(got.len());
        assert!(got[copied..].iter().all(|&b| b == 0xee), "{et:?}");
        println!(
            "read {et:?}: guest bytes written {copied}, used len {}",
            used[0].1
        );
        assert_eq!(
            used[0].1 as usize,
            copied + 1,
            "{et:?}: the used length does not match what the device wrote to guest memory"
        );
    }
}

/// A misaligned 2 MiB write whose second chunk fails (RLIMIT_FSIZE at 1 MiB):
/// the guest's first 1 MiB - 512 bytes are on the backing; the write byte counter must count
/// the confirmed prefix.
#[test]
fn test_bounce_write_counts_landed_bytes() {
    for et in [FileEngineType::Async, FileEngineType::Sync] {
        let mem = single_region_mem(0x4000000);
        let mut g = Guest::new(&mem);
        let f = backing(4 << 20, 0x11);
        let path = f.as_path().to_str().unwrap().to_string();
        let mut block = make_block(&path, et, false, &mem, &g.vq);
        let addr = Guest::data_addr(0, 0);
        mem.write_slice(&vec![0x7c; 2 << 20], GuestAddress(addr))
            .unwrap();
        let used = {
            let _lim = FsizeLimit::set(1 << 20);
            g.submit(0, Kind::Write, 512, 2 << 20, addr);
            g.kick(&mut block);
            wait_used(&mut block, &mut g, 1)
        };
        assert_eq!(u32::from(g.status(0)), VIRTIO_BLK_S_IOERR, "{et:?}");
        let disk = file_bytes(&path, 512, 2 << 20);
        let landed = disk.iter().position(|&b| b != 0x7c).unwrap_or(disk.len());
        println!(
            "write {et:?}: guest bytes on the backing {landed}, write_bytes {}, used len {}",
            block.metrics.write_bytes.count(),
            used[0].1
        );
        assert_eq!(used[0].1, 1, "{et:?}: a write reports only its status byte");
        assert_eq!(
            block.metrics.write_bytes.count(),
            landed as u64,
            "{et:?}: write_bytes does not count the bytes that landed"
        );
    }
}

/// With a one-second bandwidth bucket and no one-time burst, a 256 KiB write deferred behind
/// a read-modify-write must not leave the limiter blocked by a debt it never incurred. Once the
/// RMW completed it is accepted at once (and then incurs its debt once).
#[test]
fn test_deferred_oversized_write_not_delayed_by_phantom_debt() {
    let mem = single_region_mem(0x4000000);
    let mut g = Guest::new(&mem);
    let f = backing(4 << 20, 0x11);
    let path = f.as_path().to_str().unwrap().to_string();
    let mut block = make_block(&path, FileEngineType::Async, false, &mem, &g.vq);
    block.rate_limiter = RateLimiter::new(65536, 0, 1000, 0, 0, 0);
    mem.write_slice(&[0x51; 512], GuestAddress(Guest::data_addr(0, 0)))
        .unwrap();
    mem.write_slice(&vec![0x52; 256 << 10], GuestAddress(Guest::data_addr(1, 0)))
        .unwrap();
    g.submit(0, Kind::Write, 4096 + 512, 512, Guest::data_addr(0, 0));
    g.submit(1, Kind::Write, 0, 256 << 10, Guest::data_addr(1, 0));
    let t0 = Instant::now();
    g.kick(&mut block);
    assert!(block.bounce_hold.is_some(), "the write waits for the RMW");
    let blocked_while_held = block.rate_limiter.is_blocked();
    let used = wait_used(&mut block, &mut g, 2);
    let took = t0.elapsed();
    once_each(&used, 2);
    ok(&g, &[0, 1]);
    println!(
        "debt: limiter blocked while the write was held {blocked_while_held}, both done in {took:?}"
    );
    assert!(!blocked_while_held, "a held write armed a debt timer");
    assert!(
        took < Duration::from_millis(1500),
        "both requests took {took:?}"
    );
    assert_eq!(file_bytes(&path, 0, 256 << 10), vec![0x52; 256 << 10]);
}

/// A deferred request must preserve the available one-time burst.
#[test]
fn test_deferred_write_keeps_the_one_time_burst() {
    let mem = single_region_mem(0x4000000);
    let mut g = Guest::new(&mem);
    let f = backing(4 << 20, 0x11);
    let path = f.as_path().to_str().unwrap().to_string();
    let mut block = make_block(&path, FileEngineType::Async, false, &mem, &g.vq);
    block.rate_limiter = RateLimiter::new(0, 0, 0, 100, 2, 60_000);
    g.submit(0, Kind::Write, 4096 + 512, 512, Guest::data_addr(0, 0));
    g.submit(1, Kind::Write, 4096, 4096, Guest::data_addr(1, 0));
    g.kick(&mut block);
    assert!(block.bounce_hold.is_some(), "the write waits for the RMW");
    let burst = block.rate_limiter.ops().unwrap().one_time_burst();
    println!("burst: ops one-time burst left after the deferral {burst} (want 1)");
    let used = wait_used(&mut block, &mut g, 2);
    once_each(&used, 2);
    assert_eq!(burst, 1, "the deferral consumed the one-time burst");
}
