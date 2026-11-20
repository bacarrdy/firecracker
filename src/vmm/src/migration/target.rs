// Copyright 2026 VPSnet. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The target side of a migration: rounds received before the microVM state is loaded.
//!
//! The target VMM receives every round BEFORE it loads the state. A loaded microVM has live
//! devices even while paused (a frame from the tap fills an RX buffer and moves the used ring,
//! the balloon statistics timer returns its descriptor, restored queues may run pending
//! requests), and a later round would overwrite what they wrote while their state stays: the
//! guest would see its rings go backwards. Before the load nothing exists that writes guest
//! memory, so the rounds land in memory that only the stream writes.
//!
//! The memory is fresh private anonymous memory (the balloon works on it as on any guest
//! memory), created from the layout in the FIRST round. A zero page is dropped with
//! `MADV_DONTNEED` instead of written, so memory the guest freed stays unallocated.

use std::time::Instant;

use serde::Serialize;
use vm_memory::{GuestAddress, GuestMemoryRegion, MemoryRegionAddress};

use super::stream::{DIGEST_LEN, PageSink, RoundReader, StreamRead};
use super::{MigrationError, PAGE_SIZE};
use crate::utils::u64_to_usize;
use crate::vmm_config::machine_config::HugePageConfig;
use crate::vstate::memory::{self, GuestRegionMmap};

/// What a received round carried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReceiveStats {
    /// Round number in the session.
    pub seq: u64,
    /// The FIRST round of a session.
    pub first: bool,
    /// The FINAL round of a session: the state can be loaded now.
    pub r#final: bool,
    /// Records received.
    pub records: u64,
    /// Records with page data.
    pub data_pages: u64,
    /// Records of zero pages.
    pub zero_pages: u64,
    /// Time to receive the round, in microseconds.
    pub duration_us: u64,
}

/// Guest memory received from a complete session, handed to the snapshot load.
#[derive(Debug)]
pub struct ReceivedMemory {
    /// The memory regions, in layout order.
    pub regions: Vec<GuestRegionMmap>,
    /// The layout `(guest physical base, size)` the source sent.
    pub layout: Vec<(u64, u64)>,
    /// SHA-256 of the state file written with the FINAL round.
    pub state_digest: [u8; DIGEST_LEN],
}

/// The receiving end of a migration session, before the state is loaded.
///
/// Any failure (a cut or altered round, a sequence gap, a round of another session) ends it for
/// good: the memory holds part of a round, so neither more rounds nor a load are accepted. The
/// move starts again with a new target process.
#[derive(Debug, Default)]
pub struct MigrationReceiver {
    session: Option<u64>,
    next_seq: u64,
    layout: Vec<(u64, u64)>,
    memory: Vec<GuestRegionMmap>,
    state_digest: Option<[u8; DIGEST_LEN]>,
    closed: Option<String>,
}

/// Writes received pages into the regions; zero pages are dropped in runs.
struct MemorySink<'a> {
    layout: &'a [(u64, u64)],
    regions: &'a [GuestRegionMmap],
    zero_run: Option<(usize, u64, u64)>,
    error: Option<std::io::Error>,
}

impl MemorySink<'_> {
    fn locate(&self, gpa: u64) -> Option<(usize, u64)> {
        self.layout
            .iter()
            .position(|&(base, size)| gpa >= base && gpa - base < size)
            .map(|i| (i, gpa - self.layout[i].0))
    }

    fn flush_zero_run(&mut self) {
        if let Some((index, offset, len)) = self.zero_run.take() {
            let region = &self.regions[index];
            // SAFETY: the range lies within the region's mapping (checked by `locate`), which
            // this process owns and nothing else uses before the load.
            let ret = unsafe {
                libc::madvise(
                    region.as_ptr().add(u64_to_usize(offset)).cast(),
                    u64_to_usize(len),
                    libc::MADV_DONTNEED,
                )
            };
            if ret != 0 && self.error.is_none() {
                self.error = Some(std::io::Error::last_os_error());
            }
        }
    }
}

impl PageSink for MemorySink<'_> {
    fn contains(&self, gpa: u64) -> bool {
        self.locate(gpa).is_some()
    }

    fn write_page(&mut self, gpa: u64, data: &[u8]) {
        let (index, offset) = self.locate(gpa).expect("checked by contains");
        // A data page ends the zero run before it.
        self.flush_zero_run();
        let slice = self.regions[index]
            .get_slice(MemoryRegionAddress(offset), PAGE_SIZE)
            .expect("checked by contains");
        slice.copy_from(data);
    }

    fn zero_page(&mut self, gpa: u64) {
        let (index, offset) = self.locate(gpa).expect("checked by contains");
        match &mut self.zero_run {
            Some((i, o, l)) if *i == index && *o + *l == offset => *l += PAGE_SIZE as u64,
            _ => {
                self.flush_zero_run();
                self.zero_run = Some((index, offset, PAGE_SIZE as u64));
            }
        }
    }

    fn chunk_done(&mut self) {
        self.flush_zero_run();
    }
}

impl MigrationReceiver {
    /// Whether the FINAL round was received and the state can be loaded.
    pub fn is_complete(&self) -> bool {
        self.closed.is_none() && self.state_digest.is_some()
    }

    /// Receives one round from `input`.
    pub fn receive_round<R: StreamRead>(
        &mut self,
        input: &mut R,
    ) -> Result<ReceiveStats, MigrationError> {
        if let Some(why) = &self.closed {
            return Err(MigrationError::ReceiverClosed(why.clone()));
        }
        if self.state_digest.is_some() {
            return Err(MigrationError::ReceiverClosed(
                "the FINAL round was received; load the state".into(),
            ));
        }
        let result = self.receive(input);
        if let Err(err) = &result {
            self.closed = Some(err.to_string());
        }
        result
    }

    fn receive<R: StreamRead>(&mut self, input: &mut R) -> Result<ReceiveStats, MigrationError> {
        let started = Instant::now();
        let (reader, header) = RoundReader::start(input)?;
        if header.is_first() {
            if self.session.is_some() {
                return Err(MigrationError::Sequence(
                    "a second FIRST round in one target".into(),
                ));
            }
            self.memory = memory::anonymous(
                header
                    .regions
                    .iter()
                    .map(|&(base, size)| (GuestAddress(base), u64_to_usize(size))),
                false,
                HugePageConfig::None,
            )
            .map_err(|err| MigrationError::Memory(err.to_string()))?;
            self.layout = header.regions.clone();
            self.session = Some(header.session);
            self.next_seq = 0;
        }
        match self.session {
            None => return Err(MigrationError::NoSession),
            Some(session) if session != header.session => {
                return Err(MigrationError::Sequence(format!(
                    "round of session {:#x}, expected {session:#x}",
                    header.session
                )));
            }
            Some(_) => {}
        }
        if header.seq != self.next_seq {
            return Err(MigrationError::Sequence(format!(
                "round {} after round {}: a round is missing or repeated",
                header.seq,
                self.next_seq.wrapping_sub(1)
            )));
        }
        if header.mem_bytes != self.layout.iter().map(|r| r.1).sum::<u64>() {
            return Err(MigrationError::Sequence(
                "the round's memory size differs from the session's".into(),
            ));
        }

        let mut sink = MemorySink {
            layout: &self.layout,
            regions: &self.memory,
            zero_run: None,
            error: None,
        };
        let trailer = reader.read_pages(&mut sink);
        sink.flush_zero_run();
        if let Some(err) = sink.error {
            return Err(MigrationError::Memory(err.to_string()));
        }
        let trailer = trailer?;

        self.next_seq += 1;
        if header.is_final() {
            self.state_digest = Some(trailer.state_digest);
        }
        Ok(ReceiveStats {
            seq: header.seq,
            first: header.is_first(),
            r#final: header.is_final(),
            records: trailer.records,
            data_pages: trailer.data_pages,
            zero_pages: trailer.zero_pages,
            duration_us: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        })
    }

    /// The digest of the received memory (`GET /migration/memory-digest` before the load).
    pub fn memory_digest(&self) -> Result<super::MemoryDigest, MigrationError> {
        if self.session.is_none() || self.memory.is_empty() {
            return Err(MigrationError::NoSession);
        }
        super::memory_digest(self.memory.iter().map(|region| &**region))
    }

    /// Hands the received memory to the snapshot load. Only after the FINAL round; only once.
    pub fn take_for_load(&mut self) -> Result<ReceivedMemory, MigrationError> {
        if let Some(why) = &self.closed {
            return Err(MigrationError::ReceiverClosed(why.clone()));
        }
        let state_digest = self.state_digest.ok_or(MigrationError::Incomplete)?;
        self.closed = Some("the received memory was handed to the snapshot load".into());
        Ok(ReceivedMemory {
            regions: std::mem::take(&mut self.memory),
            layout: std::mem::take(&mut self.layout),
            state_digest,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::stream::{
        FLAG_FINAL, FLAG_FIRST, RoundHeader, RoundWriter, is_zero_page, sha256,
    };

    const MEM: u64 = 64 * PAGE_SIZE as u64;
    const HIGH: u64 = 0x10_0000;

    fn header(flags: u32, seq: u64) -> RoundHeader {
        RoundHeader {
            flags,
            session: 0xabc1,
            seq,
            mem_bytes: 2 * MEM,
            regions: if flags & FLAG_FIRST != 0 {
                vec![(0, MEM), (HIGH, MEM)]
            } else {
                vec![]
            },
        }
    }

    fn round(h: &RoundHeader, pages: &[(u64, u8)], state: Option<[u8; DIGEST_LEN]>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut w = RoundWriter::start(&mut out, h).unwrap();
        for &(gpa, fill) in pages {
            let buf = w.reserve(1);
            buf.fill(0);
            if fill != 0 {
                buf[..8].copy_from_slice(&gpa.to_le_bytes());
                buf[PAGE_SIZE - 1] = fill;
            }
            w.commit(gpa, h.is_first()).unwrap();
        }
        w.finish(state).unwrap();
        out
    }

    fn read_page(mem: &[GuestRegionMmap], layout: &[(u64, u64)], gpa: u64) -> Vec<u8> {
        let i = layout
            .iter()
            .position(|&(b, s)| gpa >= b && gpa < b + s)
            .unwrap();
        let mut page = vec![0u8; PAGE_SIZE];
        mem[i]
            .get_slice(MemoryRegionAddress(gpa - layout[i].0), PAGE_SIZE)
            .unwrap()
            .copy_to(&mut page[..]);
        page
    }

    #[test]
    fn test_receive_session() {
        let mut rx = MigrationReceiver::default();
        let mut stream = Vec::new();
        stream.extend(round(
            &header(FLAG_FIRST, 0),
            &[(0, 1), (HIGH + 0x3000, 2), (0x5000, 3)],
            None,
        ));
        // Round 1: page 0 rewritten, page 0x5000 given back (zero), a new page.
        stream.extend(round(
            &header(0, 1),
            &[(0, 9), (0x5000, 0), (HIGH, 4)],
            None,
        ));
        let state = sha256(b"the state");
        stream.extend(round(
            &header(FLAG_FINAL, 2),
            &[(HIGH + 0x3000, 7)],
            Some(state),
        ));

        let mut input = &stream[..];
        let s0 = rx.receive_round(&mut input).unwrap();
        assert!(s0.first && s0.data_pages == 3);
        assert!(!rx.is_complete());
        // Not loadable before the FINAL round.
        assert!(matches!(
            rx.take_for_load(),
            Err(MigrationError::Incomplete)
        ));
        let s1 = rx.receive_round(&mut input).unwrap();
        assert_eq!((s1.seq, s1.data_pages, s1.zero_pages), (1, 2, 1));
        let s2 = rx.receive_round(&mut input).unwrap();
        assert!(s2.r#final && rx.is_complete());
        assert!(input.is_empty());
        // Nothing after the FINAL round.
        rx.receive_round(&mut &round(&header(0, 3), &[], None)[..])
            .unwrap_err();

        // The digest of the received memory is the SHA-256 of both regions in order.
        let digest = rx.memory_digest().unwrap();
        assert_eq!(digest.bytes, 2 * MEM);
        let mut all = Vec::new();
        for gpa in (0..MEM).chain(HIGH..HIGH + MEM).step_by(PAGE_SIZE) {
            all.extend(read_page(&rx.memory, &rx.layout, gpa));
        }
        let expected: String = sha256(&all).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(digest.sha256, expected);

        let received = rx.take_for_load().unwrap();
        assert_eq!(received.state_digest, state);
        assert_eq!(received.layout, vec![(0, MEM), (HIGH, MEM)]);
        let page = |gpa| read_page(&received.regions, &received.layout, gpa);
        assert_eq!(page(0)[PAGE_SIZE - 1], 9);
        assert!(is_zero_page(&page(0x5000)));
        assert_eq!(page(HIGH)[PAGE_SIZE - 1], 4);
        assert_eq!(page(HIGH + 0x3000)[PAGE_SIZE - 1], 7);
        assert!(is_zero_page(&page(0x1000)));
        // Only once.
        rx.take_for_load().unwrap_err();
    }

    // A gap in the sequence, a round of another session, a cut round and an altered round are
    // refused, and close the receiver for good.
    #[test]
    fn test_receive_refusals_close_the_receiver() {
        let first = round(&header(FLAG_FIRST, 0), &[(0, 1)], None);
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("gap", round(&header(0, 2), &[(0, 2)], None)),
            ("other session", {
                let mut h = header(0, 1);
                h.session = 0xdead;
                round(&h, &[(0, 2)], None)
            }),
            ("cut", {
                let r = round(&header(0, 1), &[(0, 2), (PAGE_SIZE as u64, 3)], None);
                r[..r.len() - 200].to_vec()
            }),
            ("altered", {
                let mut r = round(&header(0, 1), &[(0, 2)], None);
                let at = r.len() - 200;
                r[at] ^= 1;
                r
            }),
            (
                "second FIRST",
                round(&header(FLAG_FIRST, 0), &[(0, 2)], None),
            ),
            (
                "outside memory",
                round(&header(0, 1), &[(2 * HIGH, 2)], None),
            ),
        ];
        for (name, bad) in cases {
            let mut rx = MigrationReceiver::default();
            rx.receive_round(&mut &first[..]).unwrap();
            let err = rx.receive_round(&mut &bad[..]);
            assert!(err.is_err(), "{name} accepted");
            // Closed: the right next round, and the load, are refused too.
            let next = round(&header(FLAG_FINAL, 1), &[], Some([0; DIGEST_LEN]));
            assert!(
                matches!(
                    rx.receive_round(&mut &next[..]),
                    Err(MigrationError::ReceiverClosed(_))
                ),
                "{name}"
            );
            assert!(rx.take_for_load().is_err(), "{name}");
        }

        // No FIRST round: no session.
        let mut rx = MigrationReceiver::default();
        assert!(matches!(
            rx.receive_round(&mut &round(&header(0, 0), &[], None)[..]),
            Err(MigrationError::NoSession)
        ));
    }
}
