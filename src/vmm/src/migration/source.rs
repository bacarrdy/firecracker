// Copyright 2026 VPSnet. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The source side of a migration: sessions and rounds.
//!
//! A round is prepared on the VMM thread (`Vmm::migration_prepare_round`): the dirty set is taken
//! and reset there, between device events, so no device write can fall between the queue marks,
//! the take and the re-mark of held buffers. The copy (`RoundJob::run`) needs only guest memory
//! and the descriptor: a round taken while the vCPUs run is copied on the API thread, so the VMM
//! thread keeps serving the devices; the FINAL round is copied on the VMM thread inside the
//! snapshot call, so no device runs between the state save and the copy.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use vm_memory::{GuestMemoryBackend, GuestMemoryRegion, MemoryRegionAddress};

use super::dirty::{RoundPages, check_memory, take_dirty_pages};
use super::stream::{
    DIGEST_LEN, FLAG_FINAL, FLAG_FIRST, FdStream, RoundHeader, RoundWriter, StreamWrite,
};
use super::{MigrationError, PAGE_SIZE};
use crate::Vmm;
use crate::vstate::memory::GuestMemoryMmap;

/// The state of the source's migration session, shared between the VMM thread (which prepares
/// rounds) and the thread that copies a round.
#[derive(Debug, Default)]
struct SourceState {
    /// The session identifier, from the FIRST round until the FINAL round or a failure.
    session: Option<u64>,
    /// Sequence number of the next round.
    next_seq: u64,
    /// A round is being copied.
    busy: bool,
}

/// The source's migration session.
#[derive(Debug, Default, Clone)]
pub struct SourceSession(Arc<Mutex<SourceState>>);

/// What a round sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoundStats {
    /// Round number in the session.
    pub seq: u64,
    /// The FIRST round of a session.
    pub first: bool,
    /// The FINAL round of a session.
    pub r#final: bool,
    /// Pages in the round's set (for a FIRST round: every page of guest memory).
    pub dirty_pages: u64,
    /// Records sent.
    pub records: u64,
    /// Records with page data.
    pub data_pages: u64,
    /// Records of zero pages.
    pub zero_pages: u64,
    /// Bytes written to the descriptor.
    pub bytes: u64,
    /// Time from the take of the dirty set to the end of the round, in microseconds.
    pub duration_us: u64,
}

/// A round whose dirty set is taken; `run` copies it to the descriptor.
#[derive(Debug)]
pub struct RoundJob {
    mem: GuestMemoryMmap,
    pages: RoundPages,
    header: RoundHeader,
    stream: FdStream,
    session: SourceSession,
    taken_at: Instant,
}

// `VmmData` compares its variants; a job is only ever equal to itself.
impl PartialEq for RoundJob {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Eq for RoundJob {}

struct CountingWriter<'a> {
    inner: &'a mut FdStream,
    bytes: u64,
}

impl StreamWrite for CountingWriter<'_> {
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.inner.write_all(buf)?;
        self.bytes += buf.len() as u64;
        Ok(())
    }
}

impl RoundJob {
    /// The round's header.
    pub fn header(&self) -> &RoundHeader {
        &self.header
    }

    /// Copies the round's pages to the descriptor, then the trailer (with `state_digest` for a
    /// FINAL round). On success the session moves to the next round (or ends after FINAL); on a
    /// failure it ends, and the next round must be a FIRST one: the dirty set taken for this
    /// round was reset and its pages are not all on the other side.
    pub fn run(
        mut self,
        state_digest: Option<[u8; DIGEST_LEN]>,
    ) -> Result<RoundStats, MigrationError> {
        let result = self.copy(state_digest);
        let mut state = self.session.0.lock().expect("Poisoned lock");
        state.busy = false;
        match &result {
            Ok(_) if !self.header.is_final() => state.next_seq = self.header.seq + 1,
            _ => state.session = None,
        }
        result
    }

    fn copy(
        &mut self,
        state_digest: Option<[u8; DIGEST_LEN]>,
    ) -> Result<RoundStats, MigrationError> {
        let skip_zero = self.header.is_first();
        let mut out = CountingWriter {
            inner: &mut self.stream,
            bytes: 0,
        };
        let mut writer = RoundWriter::start(&mut out, &self.header)?;
        for (region, set) in self.mem.iter().zip(self.pages.regions.iter()) {
            debug_assert_eq!(region.start_addr().0, set.base);
            for (first_page, count) in set.runs() {
                let mut page = first_page;
                let end = first_page + count;
                while page < end {
                    let n = (end - page).min(writer.room());
                    let offset = page * PAGE_SIZE;
                    let slice = region
                        .get_slice(MemoryRegionAddress(offset as u64), n * PAGE_SIZE)
                        .map_err(|err| MigrationError::Unsupported(err.to_string()))?;
                    slice.copy_to(writer.reserve(n));
                    writer.commit(set.base + offset as u64, skip_zero)?;
                    page += n;
                }
            }
        }
        let totals = writer.finish(state_digest)?;
        Ok(RoundStats {
            seq: self.header.seq,
            first: self.header.is_first(),
            r#final: self.header.is_final(),
            dirty_pages: self.pages.count(),
            records: totals.records,
            data_pages: totals.data_pages,
            zero_pages: totals.zero_pages,
            bytes: out.bytes,
            duration_us: u64::try_from(self.taken_at.elapsed().as_micros()).unwrap_or(u64::MAX),
        })
    }
}

fn new_session_id() -> u64 {
    let mut id = [0u8; 8];
    // A failure of the system's random source leaves a fixed id; the id only has to tell this
    // session's rounds from another session's on the target, which also checks the sequence.
    let _ = aws_lc_rs::rand::fill(&mut id);
    u64::from_le_bytes(id) | 1
}

impl Vmm {
    /// Checks that this microVM can be migrated by protocol 1.
    pub fn migration_check(&self) -> Result<(), MigrationError> {
        let kvm_vm = self
            .vm
            .as_kvm()
            .ok_or_else(|| MigrationError::Unsupported("a VM without KVM".into()))?;
        if self.machine_config.huge_pages.is_hugetlbfs() {
            return Err(MigrationError::Unsupported("hugetlbfs guest memory".into()));
        }
        check_memory(kvm_vm.guest_memory())?;
        self.check_unsnapshottable_devices()
            .map_err(|err| MigrationError::Unsupported(err.to_string()))
    }

    /// The digest of guest memory (`GET /migration/memory-digest`); the microVM must be paused,
    /// so the digest is of one moment.
    pub fn migration_memory_digest(&self) -> Result<super::MemoryDigest, MigrationError> {
        if self.instance_info.state != crate::vmm_config::instance_info::VmState::Paused {
            return Err(MigrationError::Unsupported(
                "a memory digest of a running microVM".into(),
            ));
        }
        let kvm_vm = self
            .vm
            .as_kvm()
            .ok_or_else(|| MigrationError::Unsupported("a VM without KVM".into()))?;
        super::memory_digest(kvm_vm.guest_memory().iter())
    }

    /// Checks that a session is open and no round is being copied, so the FINAL round can be
    /// written after the state is saved.
    pub fn migration_session_ready(&self) -> Result<(), MigrationError> {
        let state = self.migration_source.0.lock().expect("Poisoned lock");
        if state.busy {
            return Err(MigrationError::RoundInProgress);
        }
        state.session.map(|_| ()).ok_or(MigrationError::NoSession)
    }

    /// Takes and resets the dirty set of the next round and returns the job that copies it.
    ///
    /// Runs on the VMM thread. The order is the one that loses no write:
    /// 1. mark the virtio queue memory dirty (devices write the rings without marks), so the
    ///    rings are in every round;
    /// 2. take and reset KVM's dirty logs and Firecracker's bitmap;
    /// 3. re-mark the buffers devices hold and may still write into, so their later writes are
    ///    in the next round.
    ///
    /// A FIRST round starts a new session (ending any previous one) and selects every page; a
    /// FINAL round requires paused vCPUs and quiesced devices (the caller saved the state).
    pub fn migration_prepare_round(
        &mut self,
        first: bool,
        final_round: bool,
        stream: FdStream,
    ) -> Result<RoundJob, MigrationError> {
        self.migration_check()?;
        let kvm_vm = self.vm.as_kvm().expect("checked").clone();
        let mem = kvm_vm.guest_memory();

        let (session, seq) = {
            let mut state = self.migration_source.0.lock().expect("Poisoned lock");
            if state.busy {
                return Err(MigrationError::RoundInProgress);
            }
            if first {
                state.session = Some(new_session_id());
                state.next_seq = 0;
            }
            let session = state.session.ok_or(MigrationError::NoSession)?;
            state.busy = true;
            (session, state.next_seq)
        };

        self.device_manager.mark_virtio_queue_memory_dirty(mem);
        let taken = take_dirty_pages(&kvm_vm);
        self.device_manager
            .mark_virtio_pending_write_buffers_dirty(mem);
        let taken = match taken {
            Ok(taken) => taken,
            Err(err) => {
                let mut state = self.migration_source.0.lock().expect("Poisoned lock");
                state.busy = false;
                state.session = None;
                return Err(err);
            }
        };
        let taken_at = Instant::now();
        let pages = if first { RoundPages::all(mem) } else { taken };

        let regions: Vec<(u64, u64)> = mem.iter().map(|r| (r.start_addr().0, r.len())).collect();
        let header = RoundHeader {
            flags: if first { FLAG_FIRST } else { 0 } | if final_round { FLAG_FINAL } else { 0 },
            session,
            seq,
            mem_bytes: regions.iter().map(|r| r.1).sum(),
            regions: if first { regions } else { Vec::new() },
        };
        Ok(RoundJob {
            mem: mem.clone(),
            pages,
            header,
            stream,
            session: self.migration_source.clone(),
            taken_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::os::fd::FromRawFd;
    use std::thread;

    use vm_memory::{Bytes, GuestAddress};

    use super::*;
    use crate::migration::stream::tests::MapSink;
    use crate::migration::stream::{RoundReader, is_zero_page};
    use crate::test_utils::dirty_tracking_vmm;

    pub(crate) fn pipe() -> (File, File) {
        let mut fds = [0; 2];
        // SAFETY: valid array of two descriptors.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: both descriptors were just created and are owned here.
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
    }

    /// Reads rounds from `rd` into a sparse copy of guest memory until the writer closes.
    fn reader(rd: File, mem_bytes: u64) -> thread::JoinHandle<(MapSink, Vec<RoundHeader>)> {
        thread::spawn(move || {
            let mut input = FdStream::new(rd);
            let mut sink = MapSink {
                mem_bytes,
                ..Default::default()
            };
            let mut headers = Vec::new();
            while let Ok((r, header)) = RoundReader::start(&mut input) {
                r.read_pages(&mut sink).unwrap();
                headers.push(header);
            }
            (sink, headers)
        })
    }

    #[test]
    fn test_rounds_on_a_paused_vm() {
        let (vmm, _em) = dirty_tracking_vmm(None);
        let mut vmm = vmm.lock().unwrap();
        vmm.pause_vm().unwrap();
        let mem = vmm.vm.as_kvm().unwrap().guest_memory().clone();
        let mem_bytes: u64 = mem.iter().map(|r| r.len()).sum();

        // Without a FIRST round there is no session.
        let (_rd, wr) = pipe();
        assert!(matches!(
            vmm.migration_prepare_round(false, false, FdStream::new(wr)),
            Err(MigrationError::NoSession)
        ));

        let (rd, wr) = pipe();
        let reading = reader(rd, mem_bytes);
        let job = vmm
            .migration_prepare_round(true, false, FdStream::new(wr.try_clone().unwrap()))
            .unwrap();
        // One round at a time.
        let (_rd2, wr2) = pipe();
        assert!(matches!(
            vmm.migration_prepare_round(false, false, FdStream::new(wr2)),
            Err(MigrationError::RoundInProgress)
        ));
        let first = job.run(None).unwrap();
        assert!(first.first && first.seq == 0);
        assert_eq!(first.dirty_pages * PAGE_SIZE as u64, mem_bytes);
        assert_eq!(first.zero_pages, 0);

        // Device-style writes after the first round: one page changed, one page zeroed.
        mem.write_obj(0x1234_5678u64, GuestAddress(0x20_0000))
            .unwrap();
        let mut page = [0u8; PAGE_SIZE];
        let nonzero = (0..mem_bytes / PAGE_SIZE as u64)
            .map(|p| p * PAGE_SIZE as u64)
            .find(|&gpa| {
                mem.read_slice(&mut page, GuestAddress(gpa)).unwrap();
                !is_zero_page(&page) && gpa != 0x20_0000
            })
            .unwrap();
        mem.write_slice(&[0u8; PAGE_SIZE], GuestAddress(nonzero))
            .unwrap();

        let second = vmm
            .migration_prepare_round(false, false, FdStream::new(wr.try_clone().unwrap()))
            .unwrap()
            .run(None)
            .unwrap();
        assert_eq!(second.seq, 1);
        assert!(second.dirty_pages >= 2);
        assert!(second.zero_pages >= 1);
        // Nothing written since: only the queue rings (none here) and nothing else.
        let third = vmm
            .migration_prepare_round(false, false, FdStream::new(wr))
            .unwrap()
            .run(None)
            .unwrap();
        assert_eq!((third.seq, third.dirty_pages), (2, 0));

        let (sink, headers) = reading.join().unwrap();
        assert_eq!(headers.len(), 3);
        assert!(headers.iter().all(|h| h.session == headers[0].session));
        // The copy equals guest memory.
        for gpa in (0..mem_bytes).step_by(PAGE_SIZE) {
            if mem.read_slice(&mut page, GuestAddress(gpa)).is_err() {
                continue;
            }
            match sink.pages.get(&gpa) {
                Some(copy) => assert_eq!(copy.as_slice(), &page[..], "page {gpa:#x}"),
                None => assert!(is_zero_page(&page), "page {gpa:#x}"),
            }
        }
    }

    // The pause: the state file and the FINAL round in one call; the round's trailer carries
    // the SHA-256 of exactly the state file written; the session ends with it.
    #[test]
    fn test_state_only_snapshot_writes_the_final_round() {
        use vmm_sys_util::tempfile::TempFile;

        use crate::persist::{VmInfo, create_state_only_snapshot};
        use crate::vmm_config::snapshot::{CreateSnapshotParams, SnapshotType};

        let (vmm, _em) = dirty_tracking_vmm(None);
        let mut vmm = vmm.lock().unwrap();
        let state_file = TempFile::new().unwrap();
        let params = CreateSnapshotParams {
            snapshot_type: SnapshotType::StateOnly,
            snapshot_path: state_file.as_path().to_path_buf(),
            mem_file_path: Default::default(),
            sync_snapshot_files: false,
            migration_stream: None,
        };
        let vm_info = VmInfo::from(&*vmm);
        let mem = vmm.vm.as_kvm().unwrap().guest_memory().clone();
        let mem_bytes: u64 = mem.iter().map(|r| r.len()).sum();

        // Refused while the vCPUs run, and without a session.
        let (_rd, wr) = pipe();
        create_state_only_snapshot(&mut vmm, &vm_info, &params, FdStream::new(wr)).unwrap_err();
        vmm.pause_vm().unwrap();
        let (_rd, wr) = pipe();
        assert!(matches!(
            create_state_only_snapshot(&mut vmm, &vm_info, &params, FdStream::new(wr)),
            Err(crate::persist::CreateSnapshotError::Migration(
                MigrationError::NoSession
            ))
        ));

        let (rd, wr) = pipe();
        let mut input = FdStream::new(rd);
        let writer = {
            let wr = wr.try_clone().unwrap();
            let first = vmm
                .migration_prepare_round(true, false, FdStream::new(wr))
                .unwrap();
            thread::spawn(move || first.run(None).unwrap())
        };
        let mut sink = MapSink {
            mem_bytes,
            ..Default::default()
        };
        let (r, header) = RoundReader::start(&mut input).unwrap();
        assert!(header.is_first());
        r.read_pages(&mut sink).unwrap();
        writer.join().unwrap();

        mem.write_obj(0xfeed_u64, GuestAddress(0x30_0000)).unwrap();
        let reading = thread::spawn(move || {
            let (r, header) = RoundReader::start(&mut input).unwrap();
            let trailer = r.read_pages(&mut sink).unwrap();
            (header, trailer, sink)
        });
        let stats =
            create_state_only_snapshot(&mut vmm, &vm_info, &params, FdStream::new(wr)).unwrap();
        assert!(stats.r#final && stats.seq == 1 && stats.dirty_pages >= 1);
        let (header, trailer, sink) = reading.join().unwrap();
        assert!(header.is_final() && !header.is_first());
        let state = std::fs::read(state_file.as_path()).unwrap();
        assert!(!state.is_empty());
        assert_eq!(
            trailer.state_digest,
            crate::migration::stream::sha256(&state)
        );
        assert_eq!(sink.pages[&0x30_0000][..8], 0xfeed_u64.to_le_bytes());

        // The session ended with the FINAL round.
        let (_rd, wr) = pipe();
        assert!(matches!(
            vmm.migration_prepare_round(false, false, FdStream::new(wr)),
            Err(MigrationError::NoSession)
        ));
    }

    #[test]
    fn test_failed_round_ends_the_session() {
        let (vmm, _em) = dirty_tracking_vmm(None);
        let mut vmm = vmm.lock().unwrap();
        vmm.pause_vm().unwrap();
        let (rd, wr) = pipe();
        drop(rd);
        let job = vmm
            .migration_prepare_round(true, false, FdStream::new(wr))
            .unwrap();
        job.run(None).unwrap_err();
        let (_rd, wr) = pipe();
        assert!(matches!(
            vmm.migration_prepare_round(false, false, FdStream::new(wr)),
            Err(MigrationError::NoSession)
        ));
    }

    #[test]
    fn test_untracked_vm_is_refused() {
        let (vmm, _em) = crate::test_utils::default_vmm(None);
        let (_rd, wr) = pipe();
        assert!(matches!(
            vmm.lock()
                .unwrap()
                .migration_prepare_round(true, false, FdStream::new(wr)),
            Err(MigrationError::DirtyTrackingOff)
        ));
    }
}
