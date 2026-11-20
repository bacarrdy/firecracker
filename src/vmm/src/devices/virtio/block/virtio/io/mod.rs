// Copyright 2021 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod async_io;
pub mod sync_io;

use std::fmt::Debug;
use std::fs::File;

pub use self::async_io::{AsyncFileEngine, AsyncIoError};
pub use self::sync_io::{SyncFileEngine, SyncIoError};
use crate::devices::virtio::block::virtio::PendingRequest;
use crate::devices::virtio::block::virtio::device::FileEngineType;
use crate::devices::virtio::block::virtio::request::DataSegment;
use crate::vstate::memory::{GuestAddress, GuestMemoryMmap};

// Linux caps a single transfer at MAX_RW_COUNT, which depends on the host page size.
const MAX_IO_BYTES: usize = 0x7fff_0000;
const MAX_IOVECS: usize = 1024;

/// Make a bounded host operation from the remaining raw guest-memory ranges.
fn io_window(iovecs: &[libc::iovec], window: &mut Vec<libc::iovec>) -> usize {
    io_window_with_limit(iovecs, window, MAX_IO_BYTES)
}

fn io_window_with_limit(
    iovecs: &[libc::iovec],
    window: &mut Vec<libc::iovec>,
    limit: usize,
) -> usize {
    window.clear();
    let mut bytes = 0;
    for iov in iovecs.iter().take(MAX_IOVECS) {
        let len = iov.iov_len.min(limit - bytes);
        if len == 0 {
            break;
        }
        window.push(libc::iovec {
            iov_base: iov.iov_base,
            iov_len: len,
        });
        bytes += len;
        if bytes == limit {
            break;
        }
    }
    bytes
}

/// The host iovecs of a request's data segments. Fails if a segment is not inside guest memory
/// (or spans two regions).
fn segment_iovecs(
    mem: &GuestMemoryMmap,
    segments: &[DataSegment],
) -> Result<Vec<libc::iovec>, vm_memory::GuestMemoryError> {
    use vm_memory::GuestMemoryBackend;
    segments
        .iter()
        .filter(|(_, len)| *len != 0)
        .map(|&(addr, len)| {
            let slice = mem.get_slice(addr, len as usize)?;
            Ok(libc::iovec {
                iov_base: slice.ptr_guard_mut().as_ptr().cast(),
                iov_len: len as usize,
            })
        })
        .collect()
}

#[derive(Debug)]
pub struct RequestOk {
    pub req: PendingRequest,
    pub count: u32,
}

#[derive(Debug)]
pub enum FileEngineOk {
    Submitted,
    Executed(RequestOk),
}

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum BlockIoError {
    /// Sync error: {0}
    Sync(SyncIoError),
    /// Async error: {0}
    Async(AsyncIoError),
}

impl BlockIoError {
    /// Bytes known to have completed before an I/O error.
    pub fn progress(&self) -> u32 {
        match self {
            BlockIoError::Sync(SyncIoError::Vectored(_, count)) => *count,
            _ => 0,
        }
    }

    pub fn is_throttling_err(&self) -> bool {
        match self {
            BlockIoError::Async(AsyncIoError::IoUring(err)) => err.is_throttling_err(),
            _ => false,
        }
    }
}

#[derive(Debug)]
pub struct RequestError<E> {
    pub req: PendingRequest,
    pub error: E,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum FileEngine {
    #[allow(unused)]
    Async(AsyncFileEngine),
    Sync(SyncFileEngine),
}

impl FileEngine {
    pub fn from_file(file: File, engine_type: FileEngineType) -> Result<FileEngine, BlockIoError> {
        match engine_type {
            FileEngineType::Async => Ok(FileEngine::Async(
                AsyncFileEngine::from_file(file).map_err(BlockIoError::Async)?,
            )),
            FileEngineType::Sync => Ok(FileEngine::Sync(SyncFileEngine::from_file(file))),
        }
    }

    pub fn update_file_path(&mut self, file: File) -> Result<(), BlockIoError> {
        match self {
            FileEngine::Async(engine) => engine.update_file(file).map_err(BlockIoError::Async)?,
            FileEngine::Sync(engine) => engine.update_file(file),
        };

        Ok(())
    }

    pub fn file(&self) -> &File {
        match self {
            FileEngine::Async(engine) => engine.file(),
            FileEngine::Sync(engine) => engine.file(),
        }
    }

    pub fn read(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
        req: PendingRequest,
    ) -> Result<FileEngineOk, RequestError<BlockIoError>> {
        // A zero-length payload has no guest address and no host I/O to submit.
        if count == 0 {
            return Ok(FileEngineOk::Executed(RequestOk { req, count: 0 }));
        }
        match self {
            FileEngine::Async(engine) => match engine.push_read(offset, mem, addr, count, req) {
                Ok(_) => Ok(FileEngineOk::Submitted),
                Err(err) => Err(RequestError {
                    req: err.req,
                    error: BlockIoError::Async(err.error),
                }),
            },
            FileEngine::Sync(engine) => match engine.read(offset, mem, addr, count) {
                Ok(count) => Ok(FileEngineOk::Executed(RequestOk { req, count })),
                Err(err) => Err(RequestError {
                    req,
                    error: BlockIoError::Sync(err),
                }),
            },
        }
    }

    pub fn write(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
        req: PendingRequest,
    ) -> Result<FileEngineOk, RequestError<BlockIoError>> {
        match self {
            FileEngine::Async(engine) => match engine.push_write(offset, mem, addr, count, req) {
                Ok(_) => Ok(FileEngineOk::Submitted),
                Err(err) => Err(RequestError {
                    req: err.req,
                    error: BlockIoError::Async(err.error),
                }),
            },
            FileEngine::Sync(engine) => match engine.write(offset, mem, addr, count) {
                Ok(count) => Ok(FileEngineOk::Executed(RequestOk { req, count })),
                Err(err) => Err(RequestError {
                    req,
                    error: BlockIoError::Sync(err),
                }),
            },
        }
    }

    /// Reads into several data segments (a multi-segment request).
    pub fn readv(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        segments: Vec<DataSegment>,
        count: u32,
        req: PendingRequest,
    ) -> Result<FileEngineOk, RequestError<BlockIoError>> {
        match self {
            FileEngine::Async(engine) => match engine.push_readv(offset, mem, segments, req) {
                Ok(_) => Ok(FileEngineOk::Submitted),
                Err(err) => Err(RequestError {
                    req: err.req,
                    error: BlockIoError::Async(err.error),
                }),
            },
            FileEngine::Sync(engine) => match engine.readv(offset, mem, &segments, count) {
                Ok(count) => Ok(FileEngineOk::Executed(RequestOk { req, count })),
                Err(err) => Err(RequestError {
                    req,
                    error: BlockIoError::Sync(err),
                }),
            },
        }
    }

    /// Writes from several data segments (a multi-segment request).
    pub fn writev(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        segments: Vec<DataSegment>,
        count: u32,
        req: PendingRequest,
    ) -> Result<FileEngineOk, RequestError<BlockIoError>> {
        match self {
            FileEngine::Async(engine) => match engine.push_writev(offset, mem, segments, req) {
                Ok(_) => Ok(FileEngineOk::Submitted),
                Err(err) => Err(RequestError {
                    req: err.req,
                    error: BlockIoError::Async(err.error),
                }),
            },
            FileEngine::Sync(engine) => match engine.writev(offset, mem, &segments, count) {
                Ok(count) => Ok(FileEngineOk::Executed(RequestOk { req, count })),
                Err(err) => Err(RequestError {
                    req,
                    error: BlockIoError::Sync(err),
                }),
            },
        }
    }

    pub fn flush(
        &mut self,
        req: PendingRequest,
    ) -> Result<FileEngineOk, RequestError<BlockIoError>> {
        match self {
            FileEngine::Async(engine) => match engine.push_flush(req) {
                Ok(_) => Ok(FileEngineOk::Submitted),
                Err(err) => Err(RequestError {
                    req: err.req,
                    error: BlockIoError::Async(err.error),
                }),
            },
            FileEngine::Sync(engine) => match engine.flush() {
                Ok(_) => Ok(FileEngineOk::Executed(RequestOk { req, count: 0 })),
                Err(err) => Err(RequestError {
                    req,
                    error: BlockIoError::Sync(err),
                }),
            },
        }
    }

    pub fn discard(
        &mut self,
        range: (u64, u32),
        req: PendingRequest,
    ) -> Result<FileEngineOk, RequestError<BlockIoError>> {
        match self {
            FileEngine::Async(engine) => match engine.discard(range) {
                Ok(count) => Ok(FileEngineOk::Executed(RequestOk { req, count })),
                Err(err) => Err(RequestError {
                    req,
                    error: BlockIoError::Async(err),
                }),
            },
            FileEngine::Sync(engine) => match engine.discard(range) {
                Ok(count) => Ok(FileEngineOk::Executed(RequestOk { req, count })),
                Err(err) => Err(RequestError {
                    req,
                    error: BlockIoError::Sync(err),
                }),
            },
        }
    }

    pub fn drain(&mut self, discard: bool) -> Result<(), BlockIoError> {
        match self {
            FileEngine::Async(engine) => engine.drain(discard).map_err(BlockIoError::Async),
            FileEngine::Sync(_engine) => Ok(()),
        }
    }

    pub fn drain_and_flush(&mut self, discard: bool) -> Result<(), BlockIoError> {
        match self {
            FileEngine::Async(engine) => {
                engine.drain_and_flush(discard).map_err(BlockIoError::Async)
            }
            FileEngine::Sync(engine) => engine.flush().map_err(BlockIoError::Sync),
        }
    }
}

#[cfg(test)]
pub mod tests {
    #![allow(clippy::undocumented_unsafe_blocks)]
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::ffi::OsStrExt;

    use vm_memory::{GuestMemoryBackend, GuestMemoryRegion};
    use vmm_sys_util::tempfile::TempFile;

    use super::*;
    use crate::devices::virtio::block::virtio::device::FileEngineType;
    use crate::utils::u64_to_usize;
    use crate::vmm_config::machine_config::HugePageConfig;
    use crate::vstate::memory;
    use crate::vstate::memory::{Bitmap, Bytes, GuestMemoryExtension, GuestRegionMmapExt};

    const FILE_LEN: u32 = 1024;
    // 2 pages of memory should be enough to test read/write ops and also dirty tracking.
    const MEM_LEN: usize = 8192;

    macro_rules! assert_sync_execution {
        ($expression:expr, $count:expr) => {
            match $expression {
                Ok(FileEngineOk::Executed(RequestOk { req: _, count })) => {
                    assert_eq!(count, $count)
                }
                other => panic!(
                    "Expected: Ok(FileEngineOk::Executed(UserDataOk {{ user_data: _, count: {} \
                     }})), got: {:?}",
                    $count, other
                ),
            }
        };
    }

    macro_rules! assert_queued {
        ($expression:expr) => {
            assert!(matches!($expression, Ok(FileEngineOk::Submitted)))
        };
    }

    fn assert_async_execution(mem: &GuestMemoryMmap, engine: &mut FileEngine, count: u32) {
        if let FileEngine::Async(engine) = engine {
            engine.drain(false).unwrap();
            assert_eq!(engine.pop(mem).unwrap().unwrap().result().unwrap(), count);
        }
    }

    fn create_mem() -> GuestMemoryMmap {
        GuestMemoryMmap::from_regions(
            memory::anonymous(
                [(GuestAddress(0), MEM_LEN)].into_iter(),
                true,
                HugePageConfig::None,
            )
            .unwrap()
            .into_iter()
            .map(|region| GuestRegionMmapExt::dram_from_mmap_region(region, 0))
            .collect(),
        )
        .unwrap()
    }

    fn check_dirty_mem(mem: &GuestMemoryMmap, addr: GuestAddress, len: u32) {
        let bitmap = mem.find_region(addr).unwrap().bitmap();
        for offset in addr.0..addr.0 + u64::from(len) {
            assert!(bitmap.dirty_at(u64_to_usize(offset)));
        }
    }

    fn check_clean_mem(mem: &GuestMemoryMmap, addr: GuestAddress, len: u32) {
        let bitmap = mem.find_region(addr).unwrap().bitmap();
        for offset in addr.0..addr.0 + u64::from(len) {
            assert!(!bitmap.dirty_at(u64_to_usize(offset)));
        }
    }

    fn read_file_prefix(file: &mut std::fs::File, len: usize) -> Vec<u8> {
        let mut data = vec![0; len];
        file.seek(SeekFrom::Start(0)).unwrap();
        file.read_exact(&mut data).unwrap();
        data
    }
    fn create_mem_len(len: usize) -> GuestMemoryMmap {
        GuestMemoryMmap::from_regions(
            memory::anonymous(
                [(GuestAddress(0), len)].into_iter(),
                true,
                HugePageConfig::None,
            )
            .unwrap()
            .into_iter()
            .map(|region| GuestRegionMmapExt::dram_from_mmap_region(region, 0))
            .collect(),
        )
        .unwrap()
    }
    #[test]
    fn test_advance_iovecs() {
        let mut buf = [0u8; 64];
        let base = buf.as_mut_ptr();
        let mut iovecs = vec![
            libc::iovec {
                iov_base: base.cast(),
                iov_len: 16,
            },
            libc::iovec {
                iov_base: base.wrapping_add(32).cast(),
                iov_len: 16,
            },
        ];
        sync_io::advance_iovecs(&mut iovecs, 20);
        assert_eq!(iovecs.len(), 1);
        assert_eq!(iovecs[0].iov_base, base.wrapping_add(36).cast());
        assert_eq!(iovecs[0].iov_len, 12);
        sync_io::advance_iovecs(&mut iovecs, 12);
        assert!(iovecs.is_empty());
    }

    // A multi-segment write then read, on both engines: the file gets the segments in order,
    // the read puts them back in other scattered segments and marks exactly those dirty.
    #[test]
    fn test_flush_after_completed_multiwindow_write() {
        use std::os::unix::fs::FileExt;
        let mem = create_mem();
        let file = TempFile::new().unwrap().into_file();
        let mut engine = AsyncFileEngine::from_file(file).unwrap();
        let data = [0x73; 1536];
        mem.write_slice(&data, GuestAddress(0)).unwrap();
        engine.set_test_window_limit(512);
        engine
            .push_write(
                0,
                &mem,
                GuestAddress(0),
                u32::try_from(data.len()).unwrap(),
                PendingRequest::default(),
            )
            .unwrap();
        let write = loop {
            engine.drain(false).unwrap();
            if let Some(cqe) = engine.pop(&mem).unwrap() {
                break cqe;
            }
        };
        assert_eq!(write.result().unwrap(), u32::try_from(data.len()).unwrap());
        // This FLUSH follows the logical parent completion, which includes every child.
        engine.push_flush(PendingRequest::default()).unwrap();
        engine.drain(false).unwrap();
        assert_eq!(engine.pop(&mem).unwrap().unwrap().result().unwrap(), 0);
        let mut disk = [0; 1536];
        engine.file().read_exact_at(&mut disk, 0).unwrap();
        assert_eq!(disk, data);
    }

    #[test]
    fn test_empty_read_write_without_guest_region_at_zero() {
        const BASE: u64 = 0x8000_0000;
        let mem = GuestMemoryMmap::from_regions(
            memory::anonymous(
                [(GuestAddress(BASE), 4096)].into_iter(),
                true,
                HugePageConfig::None,
            )
            .unwrap()
            .into_iter()
            .map(|region| GuestRegionMmapExt::dram_from_mmap_region(region, 0))
            .collect(),
        )
        .unwrap();
        mem.get_slice(GuestAddress(0), 0).unwrap_err();
        let file = TempFile::new().unwrap().into_file();
        let mut sync = SyncFileEngine::from_file(file);
        assert_eq!(sync.read(0, &mem, GuestAddress(0), 0).unwrap(), 0);
        assert_eq!(sync.write(0, &mem, GuestAddress(0), 0).unwrap(), 0);
        let file = TempFile::new().unwrap().into_file();
        let mut asynchronous = AsyncFileEngine::from_file(file).unwrap();
        asynchronous
            .push_read(0, &mem, GuestAddress(0), 0, PendingRequest::default())
            .unwrap();
        asynchronous
            .push_write(0, &mem, GuestAddress(0), 0, PendingRequest::default())
            .unwrap();
        asynchronous.drain(false).unwrap();
        assert_eq!(
            asynchronous.pop(&mem).unwrap().unwrap().result().unwrap(),
            0
        );
        assert_eq!(
            asynchronous.pop(&mem).unwrap().unwrap().result().unwrap(),
            0
        );
        assert_eq!(asynchronous.in_flight(), 0);
        for engine_type in [FileEngineType::Sync, FileEngineType::Async] {
            let file = TempFile::new().unwrap().into_file();
            let mut engine = FileEngine::from_file(file, engine_type).unwrap();
            assert_sync_execution!(
                engine.read(0, &mem, GuestAddress(0), 0, PendingRequest::default()),
                0
            );
            let result = engine.write(0, &mem, GuestAddress(0), 0, PendingRequest::default());
            match &mut engine {
                FileEngine::Sync(_) => assert_sync_execution!(result, 0),
                FileEngine::Async(engine) => {
                    assert_queued!(result);
                    engine.drain(false).unwrap();
                    assert_eq!(engine.pop(&mem).unwrap().unwrap().result().unwrap(), 0);
                    assert_eq!(engine.in_flight(), 0);
                }
            }
            assert_eq!(engine.file().metadata().unwrap().len(), 0);
            // A zero-byte OUT still fails when the backing FD is read-only.
            let file = std::fs::File::open("/dev/null").unwrap();
            let mut readonly = FileEngine::from_file(file, engine_type).unwrap();
            let result = readonly.write(0, &mem, GuestAddress(0), 0, PendingRequest::default());
            match &mut readonly {
                FileEngine::Sync(_) => {
                    result.unwrap_err();
                }
                FileEngine::Async(engine) => {
                    assert_queued!(result);
                    engine.drain(false).unwrap();
                    engine.pop(&mem).unwrap().unwrap().result().unwrap_err();
                }
            }
            assert!(
                !mem.find_region(GuestAddress(BASE))
                    .unwrap()
                    .bitmap()
                    .dirty_at(0)
            );
        }
    }

    #[test]
    fn test_async_full_cq_allows_continuation() {
        use std::os::unix::fs::FileExt;

        use crate::devices::virtio::block::virtio::IO_URING_NUM_ENTRIES;
        let mem = create_mem();
        let file = TempFile::new().unwrap().into_file();
        file.write_all_at(&[0x5a; 512], 0).unwrap();
        let mut engine = AsyncFileEngine::from_file(file).unwrap();
        engine
            .push_read(0, &mem, GuestAddress(0), 1024, PendingRequest::default())
            .unwrap();
        for batch in 0..2 {
            let count = if batch == 0 {
                IO_URING_NUM_ENTRIES - 1
            } else {
                IO_URING_NUM_ENTRIES
            };
            for _ in 0..count {
                engine.push_flush(PendingRequest::default()).unwrap();
            }
            engine.drain(false).unwrap();
        }
        assert_eq!(engine.in_flight(), u32::from(IO_URING_NUM_ENTRIES) * 2);
        assert!(!engine.can_accept().unwrap());
        let mut completed = 0;
        let mut short_errors = 0;
        while engine.in_flight() != 0 {
            engine.drain(false).unwrap();
            while let Some(cqe) = engine.pop(&mem).unwrap() {
                completed += 1;
                if cqe.result().is_err() && cqe.progress() == 512 {
                    short_errors += 1;
                }
            }
        }
        assert_eq!(completed, u32::from(IO_URING_NUM_ENTRIES) * 2);
        assert_eq!(short_errors, 1);
    }

    #[test]
    fn test_async_submit_failure_retains_parent_for_retry() {
        let mem = create_mem();
        let file = TempFile::new().unwrap().into_file();
        let mut engine = AsyncFileEngine::from_file(file).unwrap();
        mem.write_slice(&[0x57; 512], GuestAddress(0)).unwrap();
        engine
            .push_write(0, &mem, GuestAddress(0), 512, PendingRequest::default())
            .unwrap();
        engine.inject_submit_failure();
        engine.kick_submission_queue().unwrap_err();
        assert_eq!(engine.in_flight(), 1);
        // The engine retries the retained SQE; there is no second guest request or kick.
        engine.kick_submission_queue().unwrap();
        engine.drain(false).unwrap();
        assert_eq!(engine.pop(&mem).unwrap().unwrap().result().unwrap(), 512);
        assert_eq!(engine.in_flight(), 0);
        let mut data = [0; 512];
        use std::os::unix::fs::FileExt;
        engine.file().read_exact_at(&mut data, 0).unwrap();
        assert_eq!(data, [0x57; 512]);
    }

    #[test]
    fn test_async_write_more_than_one_host_limit() {
        const SEGMENT: u32 = 16 * 1024 * 1024;
        let mem = create_mem_len(SEGMENT as usize);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .unwrap();
        let mut engine = AsyncFileEngine::from_file(file).unwrap();
        // Aliasing guest spans keeps this a small test while crossing Linux MAX_RW_COUNT.
        let segments = vec![(GuestAddress(0), SEGMENT); 128];
        engine
            .push_writev(0, &mem, segments, PendingRequest::default())
            .unwrap();
        let cqe = loop {
            engine.drain(false).unwrap();
            if let Some(cqe) = engine.pop(&mem).unwrap() {
                break cqe;
            }
        };
        assert_eq!(cqe.result().unwrap(), 1u32 << 31);
        assert_eq!(engine.in_flight(), 0);
    }

    #[test]
    fn test_async_short_read_continues_from_known_prefix() {
        use std::os::unix::fs::FileExt;
        let mem = create_mem();
        let file = TempFile::new().unwrap().into_file();
        file.write_all_at(&[0x31; 512], 0).unwrap();
        let mut engine = AsyncFileEngine::from_file(file).unwrap();
        engine
            .push_readv(
                0,
                &mem,
                vec![(GuestAddress(0), 256), (GuestAddress(4096), 768)],
                PendingRequest::default(),
            )
            .unwrap();
        // Wait until the initial 1024-byte READV hits the old 512-byte EOF.
        engine.drain(false).unwrap();
        // Extend before harvesting the CQE: the next child must start at byte 512.
        engine.file().write_all_at(&[0x72; 512], 512).unwrap();
        engine.inject_submit_failure();
        // The completed prefix is dirtied, and the child remains owned on EAGAIN.
        engine.pop(&mem).unwrap_err();
        assert_eq!(engine.in_flight(), 1);
        check_dirty_mem(&mem, GuestAddress(0), 256);
        mem.reset_dirty();
        // Resetting the dirty bitmap between completions must preserve the next child's writes.
        engine.kick_submission_queue().unwrap();
        let cqe = loop {
            if let Some(cqe) = engine.pop(&mem).unwrap() {
                break cqe;
            }
            engine.drain(false).unwrap();
        };
        assert_eq!(cqe.result().unwrap(), 1024);
        assert_eq!(engine.in_flight(), 0);
        let mut first = [0; 256];
        let mut rest = [0; 768];
        mem.read_slice(&mut first, GuestAddress(0)).unwrap();
        mem.read_slice(&mut rest, GuestAddress(4096)).unwrap();
        assert_eq!(first, [0x31; 256]);
        assert_eq!(&rest[..256], &[0x31; 256]);
        assert_eq!(&rest[256..], &[0x72; 512]);
        check_dirty_mem(&mem, GuestAddress(0), 256);
        check_dirty_mem(&mem, GuestAddress(4096), 768);
    }

    #[test]
    fn test_async_short_read_then_eof_preserves_progress_and_dirty() {
        use std::os::unix::fs::FileExt;
        let mem = create_mem();
        let file = TempFile::new().unwrap().into_file();
        file.write_all_at(&[0x5a; 512], 0).unwrap();
        let mut engine = AsyncFileEngine::from_file(file).unwrap();
        engine
            .push_readv(
                0,
                &mem,
                vec![(GuestAddress(0), 256), (GuestAddress(4096), 768)],
                PendingRequest::default(),
            )
            .unwrap();
        let cqe = loop {
            engine.drain(false).unwrap();
            if let Some(cqe) = engine.pop(&mem).unwrap() {
                break cqe;
            }
        };
        cqe.result().unwrap_err();
        assert_eq!(cqe.progress(), 512);
        assert_eq!(engine.in_flight(), 0);
        let mut first = [0; 256];
        mem.read_slice(&mut first, GuestAddress(0)).unwrap();
        assert_eq!(first, [0x5a; 256]);
        let mut second = [0; 256];
        mem.read_slice(&mut second, GuestAddress(4096)).unwrap();
        assert_eq!(second, [0x5a; 256]);
        check_dirty_mem(&mem, GuestAddress(0), 256);
        check_dirty_mem(&mem, GuestAddress(4096), 768);
    }

    #[test]
    fn test_vectored_read_write() {
        const MEM: usize = 16 * 4096;
        for engine_type in [FileEngineType::Sync, FileEngineType::Async] {
            let mem = create_mem_len(MEM);
            let file = TempFile::new().unwrap().into_file();
            file.set_len(MEM as u64).unwrap();
            let mut check = file.try_clone().unwrap();
            let mut engine = FileEngine::from_file(file, engine_type).unwrap();

            // Three scattered source segments, odd lengths that add up to whole sectors.
            let src: Vec<DataSegment> = vec![
                (GuestAddress(0x3000), 4096),
                (GuestAddress(0x1000), 100),
                (GuestAddress(0x7010), 412),
            ];
            let mut expected = Vec::new();
            for (i, &(addr, len)) in src.iter().enumerate() {
                let bytes: Vec<u8> = (0..len)
                    .map(|b| u8::try_from(b % 256).unwrap() ^ u8::try_from(i * 0x55).unwrap())
                    .collect();
                mem.write_slice(&bytes, addr).unwrap();
                expected.extend(bytes);
            }
            let total = u32::try_from(expected.len()).unwrap();
            let res = engine.writev(512, &mem, src, total, PendingRequest::default());
            match engine_type {
                FileEngineType::Sync => assert_sync_execution!(res, total),
                FileEngineType::Async => {
                    assert_queued!(res);
                    assert_async_execution(&mem, &mut engine, total);
                }
            }
            let on_disk = read_file_prefix(&mut check, 512 + expected.len());
            assert_eq!(&on_disk[512..], expected.as_slice());

            // Read back into two other segments, from a clean bitmap.
            mem.reset_dirty();
            let dst: Vec<DataSegment> = vec![
                (GuestAddress(0xa000), 2048),
                (GuestAddress(0xc000), total - 2048),
            ];
            let res = engine.readv(512, &mem, dst.clone(), total, PendingRequest::default());
            match engine_type {
                FileEngineType::Sync => assert_sync_execution!(res, total),
                FileEngineType::Async => {
                    assert_queued!(res);
                    assert_async_execution(&mem, &mut engine, total);
                }
            }
            let mut got = Vec::new();
            for &(addr, len) in &dst {
                let mut part = vec![0u8; len as usize];
                mem.read_slice(&mut part, addr).unwrap();
                got.extend(part);
                check_dirty_mem(&mem, addr, len);
            }
            assert_eq!(got, expected);
            check_clean_mem(&mem, GuestAddress(0x3000), 4096);

            // A segment outside guest memory fails the request without touching the file.
            let bad: Vec<DataSegment> =
                vec![(GuestAddress(0), 512), (GuestAddress(MEM as u64), 512)];
            engine
                .writev(0, &mem, bad, 1024, PendingRequest::default())
                .unwrap_err();
        }
    }

    #[test]
    fn test_sync() {
        let mem = create_mem();
        // Create backing file.
        let file = TempFile::new().unwrap().into_file();
        let mut engine = FileEngine::from_file(file, FileEngineType::Sync).unwrap();

        let data = vmm_sys_util::rand::rand_alphanumerics(FILE_LEN as usize)
            .as_bytes()
            .to_vec();

        // Partial write
        let partial_len = 50;
        let addr = GuestAddress(MEM_LEN as u64 - u64::from(partial_len));
        mem.write(&data, addr).unwrap();
        assert_sync_execution!(
            engine.write(0, &mem, addr, partial_len, PendingRequest::default()),
            partial_len
        );
        // Partial read
        let mem = create_mem();
        assert_sync_execution!(
            engine.read(0, &mem, addr, partial_len, PendingRequest::default()),
            partial_len
        );
        // Check data
        let mut buf = vec![0u8; partial_len as usize];
        mem.read_slice(&mut buf, addr).unwrap();
        assert_eq!(buf, data[..partial_len as usize]);

        // Offset write
        let offset = 100;
        let partial_len = 50;
        let addr = GuestAddress(0);
        mem.write(&data, addr).unwrap();
        assert_sync_execution!(
            engine.write(offset, &mem, addr, partial_len, PendingRequest::default()),
            partial_len
        );
        // Offset read
        let mem = create_mem();
        assert_sync_execution!(
            engine.read(offset, &mem, addr, partial_len, PendingRequest::default()),
            partial_len
        );
        // Check data
        let mut buf = vec![0u8; partial_len as usize];
        mem.read_slice(&mut buf, addr).unwrap();
        assert_eq!(buf, data[..partial_len as usize]);

        // Full write
        mem.write(&data, GuestAddress(0)).unwrap();
        assert_sync_execution!(
            engine.write(
                0,
                &mem,
                GuestAddress(0),
                FILE_LEN,
                PendingRequest::default()
            ),
            FILE_LEN
        );
        // Full read
        let mem = create_mem();
        assert_sync_execution!(
            engine.read(
                0,
                &mem,
                GuestAddress(0),
                FILE_LEN,
                PendingRequest::default()
            ),
            FILE_LEN
        );
        // Check data
        let mut buf = vec![0u8; FILE_LEN as usize];
        mem.read_slice(&mut buf, GuestAddress(0)).unwrap();
        assert_eq!(buf, data.as_slice());

        // Check other ops
        engine.flush(PendingRequest::default()).unwrap();
        engine.drain(true).unwrap();
        engine.drain_and_flush(true).unwrap();
    }

    #[test]
    fn test_async() {
        // Create backing file.
        let file = TempFile::new().unwrap().into_file();
        let mut engine = FileEngine::from_file(file, FileEngineType::Async).unwrap();

        let data = vmm_sys_util::rand::rand_alphanumerics(FILE_LEN as usize)
            .as_bytes()
            .to_vec();

        // Partial reads and writes cannot really be tested because io_uring will return an error
        // code for trying to write to unmapped memory.

        // Offset write
        let mem = create_mem();
        let offset = 100;
        let partial_len = 50;
        let addr = GuestAddress(0);
        mem.write(&data, addr).unwrap();
        assert_queued!(engine.write(offset, &mem, addr, partial_len, PendingRequest::default()));
        assert_async_execution(&mem, &mut engine, partial_len);
        // Offset read
        let mem = create_mem();
        assert_queued!(engine.read(offset, &mem, addr, partial_len, PendingRequest::default()));
        assert_async_execution(&mem, &mut engine, partial_len);
        // Check data
        let mut buf = vec![0u8; partial_len as usize];
        mem.read_slice(&mut buf, addr).unwrap();
        assert_eq!(buf, data[..partial_len as usize]);
        // check dirty mem
        check_dirty_mem(&mem, addr, partial_len);
        check_clean_mem(&mem, GuestAddress(4096), 4096);

        // Full write
        mem.write(&data, GuestAddress(0)).unwrap();
        assert_queued!(engine.write(0, &mem, addr, FILE_LEN, PendingRequest::default()));
        assert_async_execution(&mem, &mut engine, FILE_LEN);

        // Full read
        let mem = create_mem();
        assert_queued!(engine.read(0, &mem, addr, FILE_LEN, PendingRequest::default()));
        assert_async_execution(&mem, &mut engine, FILE_LEN);
        // Check data
        let mut buf = vec![0u8; FILE_LEN as usize];
        mem.read_slice(&mut buf, GuestAddress(0)).unwrap();
        assert_eq!(buf, data.as_slice());
        // check dirty mem
        check_dirty_mem(&mem, addr, FILE_LEN);
        check_clean_mem(&mem, GuestAddress(4096), 4096);

        // Check other ops
        assert_queued!(engine.flush(PendingRequest::default()));
        assert_async_execution(&mem, &mut engine, 0);

        engine.drain(true).unwrap();
        engine.drain_and_flush(true).unwrap();
    }
}
