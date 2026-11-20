// Copyright 2021 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

pub mod async_io;
pub mod direct;
pub mod sync_io;

use std::fmt::Debug;
use std::fs::File;

use vm_memory::GuestMemoryBackend;

pub use self::async_io::{AsyncFileEngine, AsyncIoError};
pub use self::sync_io::{SyncFileEngine, SyncIoError};
use crate::devices::virtio::block::virtio::PendingRequest;
use crate::devices::virtio::block::virtio::device::FileEngineType;
use crate::vstate::memory::{GuestAddress, GuestMemoryMmap};

use self::direct::{Bounce, BouncePlan, DioAlign, Wait};

const DIRECT_WRITE_FD: u32 = 1;

#[derive(Debug)]
pub struct RequestOk {
    pub req: PendingRequest,
    pub count: u32,
    /// Set when a direct-I/O drive served the request through a bounce buffer.
    pub bounce: Option<Bounce>,
}

#[derive(Debug)]
pub enum FileEngineOk {
    Submitted,
    Executed(RequestOk),
    /// Direct I/O: the request cannot go now (it needs the bounce path, or it writes on the
    /// span of the read-modify-write in progress). The caller returns it to the queue and
    /// processes it again once `can_proceed(wait)`; nothing was transferred.
    Deferred(Wait),
}

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum BlockIoError {
    /// Sync error: {0}
    Sync(SyncIoError),
    /// Async error: {0}
    Async(AsyncIoError),
}

impl BlockIoError {
    pub fn progress(&self) -> u32 {
        match self {
            Self::Sync(SyncIoError::Bounce(_, count)) => *count,
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
    pub fn from_file(
        file: File,
        direct_file: Option<File>,
        engine_type: FileEngineType,
        discard: bool,
    ) -> Result<FileEngine, BlockIoError> {
        match engine_type {
            FileEngineType::Async => Ok(FileEngine::Async(
                AsyncFileEngine::from_file(file, direct_file, discard)
                    .map_err(BlockIoError::Async)?,
            )),
            FileEngineType::Sync => Ok(FileEngine::Sync(SyncFileEngine::from_file(
                file,
                direct_file,
            ))),
        }
    }

    pub fn update_file_path(
        &mut self,
        file: File,
        direct_file: Option<File>,
    ) -> Result<(), BlockIoError> {
        match self {
            FileEngine::Async(engine) => engine
                .update_file(file, direct_file)
                .map_err(BlockIoError::Async)?,
            FileEngine::Sync(engine) => engine.update_file(file, direct_file),
        };

        Ok(())
    }

    pub fn file(&self) -> &File {
        match self {
            FileEngine::Async(engine) => engine.file(),
            FileEngine::Sync(engine) => engine.file(),
        }
    }

    #[cfg(test)]
    pub fn direct_file(&self) -> Option<&File> {
        match self {
            FileEngine::Async(engine) => engine.direct_file(),
            FileEngine::Sync(engine) => engine.direct_file(),
        }
    }

    /// The `O_DIRECT` alignment when the drive does direct I/O (`direct_write`), else `None`.
    pub fn dio(&self) -> Option<DioAlign> {
        match self {
            FileEngine::Async(engine) => engine.dio(),
            FileEngine::Sync(engine) => engine.dio(),
        }
    }

    /// Whether a request deferred for `wait` can go now (`AsyncFileEngine::can_proceed`); the
    /// `Sync` engine runs bounces in place and never defers.
    pub fn can_proceed(&self, wait: Wait) -> bool {
        match self {
            FileEngine::Async(engine) => engine.can_proceed(wait),
            FileEngine::Sync(_) => true,
        }
    }

    /// Async, direct I/O: a write or discard of `[offset, offset + len)` overlapping the span of
    /// the read-modify-write in progress waits for it (it would land between its read and its
    /// write-back).
    fn defer_for_rmw(&self, offset: u64, len: u64) -> Option<FileEngineOk> {
        match self {
            FileEngine::Async(engine)
                if engine.rmw_overlaps(offset, offset.saturating_add(len)) =>
            {
                Some(FileEngineOk::Deferred(Wait::Bounce))
            }
            _ => None,
        }
    }

    /// Whether a bounce is in progress (`Async` only; its transfers complete through `pop`).
    pub fn bounce_in_progress(&self) -> bool {
        match self {
            FileEngine::Async(engine) => engine.bounce_in_progress(),
            FileEngine::Sync(_) => false,
        }
    }

    /// Whether a read-modify-write is in progress.
    #[cfg(test)]
    pub fn rmw_in_progress(&self) -> bool {
        match self {
            FileEngine::Async(engine) => engine.rmw_in_progress(),
            FileEngine::Sync(_) => false,
        }
    }

    /// With direct I/O on, the bounce plan of a request the `O_DIRECT` descriptor cannot take
    /// as is (buffer address, offset or length not aligned).
    fn bounce_plan(
        &self,
        write: bool,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        offset: u64,
        count: u32,
    ) -> Option<BouncePlan> {
        let dio = self.dio()?;
        // A guest memory error is reported by the engine as before.
        let buf = mem
            .get_slice(addr, count as usize)
            .ok()?
            .ptr_guard()
            .as_ptr() as usize;
        if dio.eligible(buf, offset, count) {
            return None;
        }
        BouncePlan::new(write, offset, count, addr, dio, direct::chunk_len(dio)).ok()
    }

    /// Serve a request that `O_DIRECT` cannot take as is through the drive's bounce buffer, on
    /// the direct descriptor; the buffered descriptor is never used for data. `Async`: started
    /// if it can start now, else deferred; it completes through `pop`. `Sync`: in place.
    fn bounce(
        &mut self,
        mut plan: BouncePlan,
        mem: &GuestMemoryMmap,
        req: PendingRequest,
    ) -> Result<FileEngineOk, RequestError<BlockIoError>> {
        match self {
            FileEngine::Async(engine) => {
                if !engine.can_proceed(plan.wait()) {
                    return Ok(FileEngineOk::Deferred(plan.wait()));
                }
                match engine.start_bounce(plan, mem, req) {
                    Ok(()) => Ok(FileEngineOk::Submitted),
                    Err(err) => Err(RequestError {
                        req: err.req,
                        error: BlockIoError::Async(err.error),
                    }),
                }
            }
            FileEngine::Sync(engine) => {
                let (kind, count) = (plan.kind(), plan.count());
                match engine.bounce(&mut plan, mem) {
                    Ok(()) => Ok(FileEngineOk::Executed(RequestOk {
                        req,
                        count,
                        bounce: Some(kind),
                    })),
                    Err(error) => Err(RequestError {
                        req,
                        error: BlockIoError::Sync(error),
                    }),
                }
            }
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
        // Direct I/O: a zero-length transfer completes without I/O, a misaligned one bounces.
        if self.dio().is_some() {
            if count == 0 {
                return Ok(FileEngineOk::Executed(RequestOk {
                    req,
                    count: 0,
                    bounce: None,
                }));
            }
            if let Some(plan) = self.bounce_plan(false, mem, addr, offset, count) {
                return self.bounce(plan, mem, req);
            }
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
                Ok(count) => Ok(FileEngineOk::Executed(RequestOk {
                    req,
                    count,
                    bounce: None,
                })),
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
        // Direct I/O: a zero-length transfer completes without I/O, a misaligned one bounces.
        if self.dio().is_some() {
            if count == 0 {
                return Ok(FileEngineOk::Executed(RequestOk {
                    req,
                    count: 0,
                    bounce: None,
                }));
            }
            if let Some(plan) = self.bounce_plan(true, mem, addr, offset, count) {
                return self.bounce(plan, mem, req);
            }
            if let Some(deferred) = self.defer_for_rmw(offset, u64::from(count)) {
                return Ok(deferred);
            }
        }
        match self {
            FileEngine::Async(engine) => match engine.push_write(offset, mem, addr, count, req) {
                Ok(_) => Ok(FileEngineOk::Submitted),
                Err(err) => Err(RequestError {
                    req: err.req,
                    error: BlockIoError::Async(err.error),
                }),
            },
            FileEngine::Sync(engine) => match engine.write(offset, mem, addr, count) {
                Ok(count) => Ok(FileEngineOk::Executed(RequestOk {
                    req,
                    count,
                    bounce: None,
                })),
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
                Ok(_) => Ok(FileEngineOk::Executed(RequestOk {
                    req,
                    count: 0,
                    bounce: None,
                })),
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
        if let Some(deferred) = self.defer_for_rmw(range.0, u64::from(range.1)) {
            return Ok(deferred);
        }
        match self {
            FileEngine::Async(engine) => match engine.push_discard(range, req) {
                Ok(_) => Ok(FileEngineOk::Submitted),
                Err(err) => Err(RequestError {
                    req: err.req,
                    error: BlockIoError::Async(err.error),
                }),
            },
            FileEngine::Sync(engine) => match engine.discard(range) {
                Ok(count) => Ok(FileEngineOk::Executed(RequestOk {
                    req,
                    count,
                    bounce: None,
                })),
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
    #![allow(clippy::undocumented_unsafe_blocks, clippy::cast_possible_truncation)]
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;

    use vm_memory::{GuestMemoryBackend, GuestMemoryRegion};
    use vmm_sys_util::tempfile::TempFile;

    use super::*;
    use crate::devices::virtio::block::virtio::device::FileEngineType;
    use crate::utils::u64_to_usize;
    use crate::vmm_config::machine_config::HugePageConfig;
    use crate::vstate::memory;
    use crate::vstate::memory::{Bitmap, Bytes, GuestRegionMmapExt};

    const FILE_LEN: u32 = 1024;
    // 2 pages of memory should be enough to test read/write ops and also dirty tracking.
    const MEM_LEN: usize = 8192;

    macro_rules! assert_sync_execution {
        ($expression:expr, $count:expr) => {
            match $expression {
                Ok(FileEngineOk::Executed(RequestOk { req: _, count, .. })) => {
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
            assert_eq!(engine.pop(mem).unwrap().unwrap().result.unwrap(), count);
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

    fn read_file_prefix(file: &mut std::fs::File, len: usize) -> Vec<u8> {
        let mut data = vec![0; len];
        file.seek(SeekFrom::Start(0)).unwrap();
        file.read_exact(&mut data).unwrap();
        data
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

    fn mem_of(len: usize) -> GuestMemoryMmap {
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

    /// Async: wait for everything in flight, a bounce in progress to its end included, and
    /// return the completions in order.
    fn complete_all(engine: &mut FileEngine, mem: &GuestMemoryMmap) -> Vec<async_io::Completion> {
        let mut out = vec![];
        if let FileEngine::Async(e) = engine {
            loop {
                e.drain(false).unwrap();
                while let Some(c) = e.pop(mem).unwrap() {
                    out.push(c);
                }
                if !e.bounce_in_progress() {
                    break;
                }
            }
        }
        out
    }

    /// Run one request to its end (nothing else in flight on `engine`): its count, or the
    /// error, and how it was bounced.
    fn run(
        engine: &mut FileEngine,
        write: bool,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: u64,
        len: u32,
    ) -> (Result<u32, String>, Option<Bounce>) {
        let req = PendingRequest::default();
        let res = if write {
            engine.write(offset, mem, GuestAddress(addr), len, req)
        } else {
            engine.read(offset, mem, GuestAddress(addr), len, req)
        };
        match res {
            Ok(FileEngineOk::Executed(RequestOk { count, bounce, .. })) => (Ok(count), bounce),
            Ok(FileEngineOk::Submitted) => {
                let mut done = complete_all(engine, mem);
                assert_eq!(done.len(), 1, "exactly one completion");
                let c = done.pop().unwrap();
                (c.result.map_err(|e| e.to_string()), c.bounce.map(|b| b.0))
            }
            Ok(FileEngineOk::Deferred(_)) => panic!("deferred with nothing in flight"),
            Err(err) => (Err(format!("{:?}", err.error)), None),
        }
    }

    fn pair(len: u64) -> (std::fs::File, std::fs::File, std::fs::File, std::fs::File) {
        let buffered = TempFile::new().unwrap().into_file();
        let direct = TempFile::new().unwrap().into_file();
        buffered.set_len(len).unwrap();
        direct.set_len(len).unwrap();
        let bc = buffered.try_clone().unwrap();
        let dc = direct.try_clone().unwrap();
        (buffered, direct, bc, dc)
    }

    /// A buffered and an O_DIRECT descriptor of one file of `len` bytes in `DIO_TEST_DIR`
    /// (default /tmp), filled with `fill`; None when that file system has no O_DIRECT.
    fn o_direct_pair(
        name: &str,
        len: usize,
        fill: u8,
    ) -> Option<(std::fs::File, std::fs::File, String)> {
        let dir = std::env::var("DIO_TEST_DIR").unwrap_or_else(|_| "/tmp".to_string());
        let path = format!("{dir}/dio-test-{name}-{}", std::process::id());
        std::fs::write(&path, vec![fill; len]).unwrap();
        let buffered = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_DIRECT)
            .open(&path)
        {
            Ok(direct) => Some((buffered, direct, path)),
            Err(e) => {
                println!("SKIP: O_DIRECT not supported in {dir}: {e}");
                std::fs::remove_file(&path).unwrap();
                None
            }
        }
    }

    #[test]
    fn test_direct_io_alignment() {
        let dio = DioAlign {
            mem: 4096,
            offset: 4096,
        };
        assert!(dio.eligible(0x1000, 0x2000, 0x3000));
        assert!(!dio.eligible(0x1001, 0x2000, 0x3000));
        assert!(!dio.eligible(0x1000, 0x2001, 0x3000));
        assert!(!dio.eligible(0x1000, 0x2000, 0x3001));
        assert!(!dio.eligible(0x1000, 0x2000, 0));
    }

    // The invariant of direct I/O both ways: with a direct descriptor, no read or write of any
    // shape (aligned, unaligned buffer, partial block) carries data through the buffered
    // descriptor. The two descriptors are two different files here, so a use of the buffered one
    // would show as data read from or landing in the wrong file.
    #[test]
    fn test_direct_io_never_uses_buffered_descriptor() {
        for engine_type in [FileEngineType::Sync, FileEngineType::Async] {
            let mem = create_mem();
            let buffered = TempFile::new().unwrap().into_file();
            let direct = TempFile::new().unwrap().into_file();
            buffered.set_len(16384).unwrap();
            let pattern: Vec<u8> = (0..16384u32).map(|i| (i % 251) as u8).collect();
            let mut direct_w = direct.try_clone().unwrap();
            direct_w.write_all(&pattern).unwrap();
            let mut buffered_check = buffered.try_clone().unwrap();
            let mut direct_check = direct.try_clone().unwrap();
            let mut engine =
                FileEngine::from_file(buffered, Some(direct), engine_type, false).unwrap();
            assert!(engine.dio().is_some());

            let mut expect = pattern.clone();
            // (offset, guest address, length): aligned; unaligned buffer; partial block.
            for (i, (offset, gaddr, len)) in [
                (4096u64, 0x0u64, 4096u32),
                (8192, 0x5, 4096),
                (1000, 0x7, 600),
            ]
            .into_iter()
            .enumerate()
            {
                let data = vec![0x11 * (i as u8 + 1); len as usize];
                mem.write_slice(&data, GuestAddress(gaddr)).unwrap();
                let (res, _) = run(&mut engine, true, offset, &mem, gaddr, len);
                assert_eq!(res, Ok(len));
                expect[offset as usize..offset as usize + len as usize].copy_from_slice(&data);
            }
            assert_eq!(read_file_prefix(&mut direct_check, 16384), expect);
            assert_eq!(
                read_file_prefix(&mut buffered_check, 16384),
                vec![0u8; 16384]
            );

            // Reads of every shape come from the direct descriptor.
            for (offset, gaddr, len) in [(0u64, 0x0u64, 8192u32), (4096 + 3, 0x9, 5000)] {
                mem.write_slice(&vec![0u8; len as usize], GuestAddress(gaddr))
                    .unwrap();
                let (res, _) = run(&mut engine, false, offset, &mem, gaddr, len);
                assert_eq!(res, Ok(len));
                let mut back = vec![0u8; len as usize];
                mem.read_slice(&mut back, GuestAddress(gaddr)).unwrap();
                assert_eq!(
                    back,
                    expect[offset as usize..offset as usize + len as usize]
                );
            }
        }
    }

    #[test]
    fn test_direct_write_file_engine() {
        for engine_type in [FileEngineType::Sync, FileEngineType::Async] {
            let file = TempFile::new().unwrap().into_file();
            let direct_file = file.try_clone().unwrap();
            let engine =
                FileEngine::from_file(file, Some(direct_file), engine_type, false).unwrap();

            assert!(engine.direct_file().is_some());
        }
    }

    // The engines never fall back to the buffered descriptor for data. A request the
    // O_DIRECT descriptor cannot take as is is an error at the engine (FileEngine bounces it
    // before it gets there); nothing lands in either file.
    #[test]
    fn test_sync_direct_write_selects_expected_file() {
        const DIRECT_WRITE_LEN: u32 = 4096;

        let mem = create_mem();
        let addr = GuestAddress(0);
        let first_write = vec![0x5a; DIRECT_WRITE_LEN as usize];
        mem.write(&first_write, addr).unwrap();

        let buffered_file = TempFile::new().unwrap().into_file();
        let direct_file = TempFile::new().unwrap().into_file();
        buffered_file.set_len(MEM_LEN as u64).unwrap();
        direct_file.set_len(MEM_LEN as u64).unwrap();
        let mut buffered_check = buffered_file.try_clone().unwrap();
        let mut direct_check = direct_file.try_clone().unwrap();

        let slice = mem.get_slice(addr, DIRECT_WRITE_LEN as usize).unwrap();
        let buf = slice.ptr_guard().as_ptr() as usize;
        assert!(
            DioAlign {
                mem: 4096,
                offset: 4096
            }
            .eligible(buf, 0, DIRECT_WRITE_LEN)
        );

        let mut engine = SyncFileEngine::from_file(buffered_file, Some(direct_file));
        assert_eq!(
            engine.write(0, &mem, addr, DIRECT_WRITE_LEN).unwrap(),
            DIRECT_WRITE_LEN
        );
        assert_eq!(
            read_file_prefix(&mut direct_check, DIRECT_WRITE_LEN as usize),
            first_write
        );
        assert_eq!(
            read_file_prefix(&mut buffered_check, DIRECT_WRITE_LEN as usize),
            vec![0; DIRECT_WRITE_LEN as usize]
        );

        let second_write = vec![0xa5; DIRECT_WRITE_LEN as usize];
        mem.write(&second_write, addr).unwrap();
        assert!(matches!(
            engine.write(1, &mem, addr, DIRECT_WRITE_LEN),
            Err(SyncIoError::DirectUnaligned)
        ));
        assert!(matches!(
            engine.read(1, &mem, addr, DIRECT_WRITE_LEN),
            Err(SyncIoError::DirectUnaligned)
        ));
        assert!(matches!(
            engine.write(0, &mem, addr, 0),
            Err(SyncIoError::DirectUnaligned)
        ));

        assert_eq!(
            read_file_prefix(&mut buffered_check, DIRECT_WRITE_LEN as usize + 1),
            vec![0; DIRECT_WRITE_LEN as usize + 1]
        );
        assert_eq!(
            read_file_prefix(&mut direct_check, DIRECT_WRITE_LEN as usize),
            first_write
        );
    }

    // Check the same behavior through io_uring.
    #[test]
    fn test_async_direct_unaligned_is_an_error() {
        let mem = create_mem();
        let (b, d, mut bc, mut dc) = pair(8192);
        let mut engine = AsyncFileEngine::from_file(b, Some(d), false).unwrap();
        for (offset, addr, len) in [
            (1u64, 0u64, 4096u32),
            (0, 0x200, 4096),
            (0, 0, 512),
            (0, 0, 0),
        ] {
            let r = engine.push_write(
                offset,
                &mem,
                GuestAddress(addr),
                len,
                PendingRequest::default(),
            );
            assert!(matches!(
                r,
                Err(RequestError {
                    error: AsyncIoError::DirectUnaligned,
                    ..
                })
            ));
            let r = engine.push_read(
                offset,
                &mem,
                GuestAddress(addr),
                len,
                PendingRequest::default(),
            );
            assert!(matches!(
                r,
                Err(RequestError {
                    error: AsyncIoError::DirectUnaligned,
                    ..
                })
            ));
        }
        engine.drain(false).unwrap();
        assert!(engine.pop(&mem).unwrap().is_none());
        assert_eq!(read_file_prefix(&mut bc, 8192), vec![0u8; 8192]);
        assert_eq!(read_file_prefix(&mut dc, 8192), vec![0u8; 8192]);
    }

    // A zero-length data request on a direct drive completes without any I/O.
    #[test]
    fn test_direct_zero_length_without_io() {
        for et in [FileEngineType::Sync, FileEngineType::Async] {
            let m = create_mem();
            let (b, d, _bc, _dc) = pair(8192);
            let mut engine = FileEngine::from_file(b, Some(d), et, false).unwrap();
            for write in [false, true] {
                let req = PendingRequest::default();
                let r = if write {
                    engine.write(0, &m, GuestAddress(0x1000), 0, req)
                } else {
                    engine.read(0, &m, GuestAddress(0x1000), 0, req)
                };
                assert!(matches!(
                    r,
                    Ok(FileEngineOk::Executed(RequestOk {
                        count: 0,
                        bounce: None,
                        ..
                    }))
                ));
            }
            if let FileEngine::Async(e) = &mut engine {
                e.drain(false).unwrap();
                assert!(e.pop(&m).unwrap().is_none(), "no op was queued");
            }
        }
    }

    // A read-modify-write is deferred while a write on its span is in flight
    // (it would land between the RMW's read and its write-back); once that write completed, it
    // runs and keeps the written data around the patched sector. Exactly one completion per
    // request.
    #[test]
    fn test_rmw_deferred_while_write_on_its_span_in_flight() {
        let m = mem_of(0x10000);
        let (b, d, mut bc, mut dc) = pair(16384);
        let mut engine = FileEngine::from_file(b, Some(d), FileEngineType::Async, false).unwrap();
        m.write_slice(&[0xaa; 4096], GuestAddress(0x1000)).unwrap();
        let r = engine.write(0, &m, GuestAddress(0x1000), 4096, PendingRequest::default());
        assert!(matches!(r, Ok(FileEngineOk::Submitted)));
        m.write_slice(&[0x33; 512], GuestAddress(0x3200)).unwrap();
        let r = engine.write(
            1536,
            &m,
            GuestAddress(0x3200),
            512,
            PendingRequest::default(),
        );
        assert!(matches!(r, Ok(FileEngineOk::Deferred(Wait::Span(0, 4096)))));
        assert!(!engine.can_proceed(Wait::Span(0, 4096)));
        assert!(
            engine.can_proceed(Wait::Span(4096, 8192)),
            "other blocks are free"
        );
        // The aligned write completes (and is acknowledged): the RMW can start.
        let done = complete_all(&mut engine, &m);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].result.as_ref().unwrap(), &4096);
        assert!(done[0].bounce.is_none());
        assert!(engine.can_proceed(Wait::Span(0, 4096)));
        let (res, kind) = run(&mut engine, true, 1536, &m, 0x3200, 512);
        assert_eq!((res, kind), (Ok(512), Some(Bounce::ReadModifyWrite)));
        let mut want = vec![0xaau8; 4096];
        want[1536..2048].copy_from_slice(&[0x33; 512]);
        assert_eq!(&read_file_prefix(&mut dc, 4096)[..], &want[..]);
        assert_eq!(read_file_prefix(&mut bc, 4096), vec![0u8; 4096]);
    }

    // The exclusion of a read-modify-write is its span. Writes elsewhere
    // in flight do not delay it; while it runs, reads (of its span too) and writes elsewhere
    // go, a write or discard overlapping its span waits for it, and any other bounce waits for
    // the one buffer. A non-RMW bounce never waits for aligned requests.
    #[test]
    fn test_bounce_serialisation_rules() {
        let m = mem_of(0x20000);
        let (b, d, _bc, _dc) = pair(65536);
        let mut engine = FileEngine::from_file(b, Some(d), FileEngineType::Async, true).unwrap();
        let req = PendingRequest::default;
        // An aligned write of block 0 in flight; a misaligned read starts at once.
        assert!(matches!(
            engine.write(0, &m, GuestAddress(0x1000), 4096, req()),
            Ok(FileEngineOk::Submitted)
        ));
        assert!(matches!(
            engine.read(8192 + 512, &m, GuestAddress(0x4000), 512, req()),
            Ok(FileEngineOk::Submitted)
        ));
        assert!(engine.bounce_in_progress() && !engine.rmw_in_progress());
        // Another bounce (read, or whole-block write from a misaligned buffer) waits for it.
        assert!(matches!(
            engine.read(512, &m, GuestAddress(0x5000), 512, req()),
            Ok(FileEngineOk::Deferred(Wait::Bounce))
        ));
        assert!(matches!(
            engine.write(16384, &m, GuestAddress(0x5200), 4096, req()),
            Ok(FileEngineOk::Deferred(Wait::Bounce))
        ));
        // Aligned requests still go.
        assert!(matches!(
            engine.read(4096, &m, GuestAddress(0x2000), 4096, req()),
            Ok(FileEngineOk::Submitted)
        ));
        let done = complete_all(&mut engine, &m);
        assert_eq!(done.len(), 3, "three requests, three completions");
        assert_eq!(done.iter().filter(|c| c.bounce.is_some()).count(), 1);
        assert!(done.iter().all(|c| c.result.is_ok()));

        // A write of block 3 in flight does not delay a read-modify-write of block 1.
        assert!(matches!(
            engine.write(3 * 4096, &m, GuestAddress(0x1000), 4096, req()),
            Ok(FileEngineOk::Submitted)
        ));
        assert!(matches!(
            engine.write(4096 + 512, &m, GuestAddress(0x6000), 512, req()),
            Ok(FileEngineOk::Submitted)
        ));
        assert!(engine.rmw_in_progress());
        // While it runs: reads of its span and writes elsewhere go...
        assert!(matches!(
            engine.read(4096, &m, GuestAddress(0x7000), 4096, req()),
            Ok(FileEngineOk::Submitted)
        ));
        assert!(matches!(
            engine.write(8192, &m, GuestAddress(0x1000), 4096, req()),
            Ok(FileEngineOk::Submitted)
        ));
        // ...a write or a discard overlapping its span, and any bounce, wait.
        assert!(matches!(
            engine.write(4096, &m, GuestAddress(0x1000), 4096, req()),
            Ok(FileEngineOk::Deferred(Wait::Bounce))
        ));
        assert!(matches!(
            engine.write(0, &m, GuestAddress(0x1000), 8192, req()),
            Ok(FileEngineOk::Deferred(Wait::Bounce))
        ));
        assert!(matches!(
            engine.discard((4096 + 1024, 512), req()),
            Ok(FileEngineOk::Deferred(Wait::Bounce))
        ));
        assert!(matches!(
            engine.read(20480 + 512, &m, GuestAddress(0x8000), 512, req()),
            Ok(FileEngineOk::Deferred(Wait::Bounce))
        ));
        let done = complete_all(&mut engine, &m);
        assert_eq!(
            done.len(),
            4,
            "the write of block 3, the RMW, the read, the write of block 2"
        );
        assert_eq!(
            done.iter()
                .filter_map(|c| c.bounce.map(|b| b.0))
                .collect::<Vec<_>>(),
            vec![Bounce::ReadModifyWrite]
        );
        assert!(!engine.bounce_in_progress());
        assert!(
            engine.can_proceed(Wait::Span(0, 1 << 20)),
            "nothing in flight"
        );
    }

    // Eight sub-block writers of one device block plus aligned writes of other blocks in
    // between: every sector ends with its own data, nothing via the buffered descriptor, one
    // completion per request (Async: an RMW waits for the RMW before it).
    #[test]
    fn test_eight_sector_writers_one_block() {
        for et in [FileEngineType::Async, FileEngineType::Sync] {
            let m = mem_of(0x20000);
            let (b, d, mut bc, mut dc) = pair(32768);
            let mut engine = FileEngine::from_file(b, Some(d), et, false).unwrap();
            let mut want = vec![0u8; 32768];
            let (mut submitted, mut completed) = (0, 0);
            for k in 0..8u64 {
                let blk = 1 + k % 3;
                let a = GuestAddress(0x8000 + k * 0x1000);
                m.write_slice(&[0x10 + k as u8; 4096], a).unwrap();
                match engine.write(blk * 4096, &m, a, 4096, PendingRequest::default()) {
                    Ok(FileEngineOk::Submitted) => submitted += 1,
                    Ok(FileEngineOk::Executed(RequestOk { count: 4096, .. })) => {}
                    other => panic!("{other:?}"),
                }
                want[(blk * 4096) as usize..(blk * 4096 + 4096) as usize]
                    .copy_from_slice(&[0x10 + k as u8; 4096]);
                let g = 0x200 + k * 0x400;
                m.write_slice(&[0xc0 + k as u8; 512], GuestAddress(g))
                    .unwrap();
                let off = 5 * 4096 + k * 512;
                let r = engine.write(off, &m, GuestAddress(g), 512, PendingRequest::default());
                match r {
                    // Async: block 5 is not written by the aligned writers: the RMW starts at
                    // once, unless the previous RMW (one buffer) is still in progress.
                    Ok(FileEngineOk::Submitted) => submitted += 1,
                    Ok(FileEngineOk::Deferred(Wait::Span(20480, 24576))) => {
                        completed += complete_all(&mut engine, &m).len();
                        let (res, kind) = run(&mut engine, true, off, &m, g, 512);
                        assert_eq!((res, kind), (Ok(512), Some(Bounce::ReadModifyWrite)));
                    }
                    Ok(FileEngineOk::Executed(RequestOk { count: 512, .. })) => {}
                    other => panic!("{other:?}"),
                }
                want[off as usize..off as usize + 512].copy_from_slice(&[0xc0 + k as u8; 512]);
            }
            completed += complete_all(&mut engine, &m).len();
            assert_eq!(completed, submitted, "{et:?}");
            assert_eq!(read_file_prefix(&mut dc, 32768), want, "{et:?}");
            assert_eq!(read_file_prefix(&mut bc, 32768), vec![0u8; 32768], "{et:?}");
        }
    }

    // A misaligned read after an aligned write of the same range completed returns the written
    // data, and the bounce marks the guest pages it wrote dirty (diff snapshots, live move).
    #[test]
    fn test_unaligned_read_after_aligned_write() {
        for et in [FileEngineType::Async, FileEngineType::Sync] {
            let m = mem_of(0x10000);
            let (b, d, _bc, _dc) = pair(16384);
            let mut engine = FileEngine::from_file(b, Some(d), et, false).unwrap();
            m.write_slice(&[0x5c; 4096], GuestAddress(0x2000)).unwrap();
            let (res, kind) = run(&mut engine, true, 4096, &m, 0x2000, 4096);
            assert_eq!((res, kind), (Ok(4096), None));
            let dst = GuestAddress(0x6200);
            let (res, kind) = run(&mut engine, false, 4096 + 512, &m, dst.0, 1024);
            assert_eq!((res, kind), (Ok(1024), Some(Bounce::Read)));
            let mut got = vec![0u8; 1024];
            m.read_slice(&mut got, dst).unwrap();
            assert_eq!(got, vec![0x5c; 1024]);
            let bitmap = m.find_region(dst).unwrap().bitmap();
            for o in dst.0..dst.0 + 1024 {
                assert!(
                    bitmap.dirty_at(o as usize),
                    "bounce read must mark guest memory dirty"
                );
            }
        }
    }

    // A misaligned read issued while an aligned write of the same block is in flight (not yet
    // acknowledged) is not held back: it returns the block before or after the write, whole.
    #[test]
    fn test_unaligned_read_concurrent_with_aligned_write() {
        let m = mem_of(0x10000);
        let (b, d, _bc, mut dc) = pair(16384);
        dc.write_all(&[0x11; 16384]).unwrap();
        let mut engine = FileEngine::from_file(b, Some(d), FileEngineType::Async, false).unwrap();
        m.write_slice(&[0x22; 4096], GuestAddress(0x2000)).unwrap();
        assert!(matches!(
            engine.write(
                4096,
                &m,
                GuestAddress(0x2000),
                4096,
                PendingRequest::default()
            ),
            Ok(FileEngineOk::Submitted)
        ));
        assert!(matches!(
            engine.read(
                4096 + 512,
                &m,
                GuestAddress(0x6200),
                1024,
                PendingRequest::default()
            ),
            Ok(FileEngineOk::Submitted)
        ));
        let done = complete_all(&mut engine, &m);
        assert_eq!(done.len(), 2);
        assert!(done.iter().all(|c| c.result.is_ok()));
        let mut got = vec![0u8; 1024];
        m.read_slice(&mut got, GuestAddress(0x6200)).unwrap();
        assert!(
            got == vec![0x11; 1024] || got == vec![0x22; 1024],
            "{:x?}",
            &got[..8]
        );
    }

    // A request far larger than the bounce buffer is served chunk by chunk through it, on
    // both engines: the buffer stays 1 MiB, the data is exact.
    #[test]
    fn test_bounce_memory_bounded() {
        const LEN: u32 = 16 * 1048576;
        for et in [FileEngineType::Async, FileEngineType::Sync] {
            let m = mem_of(2 * LEN as usize + 0x10000);
            let (b, d, _bc, mut dc) = pair(u64::from(LEN) + 1048576);
            let mut engine = FileEngine::from_file(b, Some(d), et, false).unwrap();
            let data: Vec<u8> = (0..LEN).map(|i| (i % 239) as u8).collect();
            m.write_slice(&data, GuestAddress(0x200)).unwrap();
            let (res, kind) = run(&mut engine, true, 512, &m, 0x200, LEN);
            assert_eq!((res, kind), (Ok(LEN), Some(Bounce::ReadModifyWrite)));
            let (res, kind) = run(&mut engine, false, 512, &m, u64::from(LEN) + 0x1200, LEN);
            assert_eq!((res, kind), (Ok(LEN), Some(Bounce::Read)));
            let mut back = vec![0u8; LEN as usize];
            m.read_slice(&mut back, GuestAddress(u64::from(LEN) + 0x1200))
                .unwrap();
            assert!(back == data, "{et:?}");
            let file = read_file_prefix(&mut dc, LEN as usize + 1024);
            assert!(file[..512] == [0u8; 512] && file[512..LEN as usize + 512] == data[..]);
            let buf_len = match &engine {
                FileEngine::Async(e) => e.bounce_buf_len(),
                FileEngine::Sync(e) => e.bounce_buf_len(),
            };
            assert_eq!(buf_len, Some(direct::BOUNCE_BUF_LEN), "{et:?}");
        }
    }

    // A request the engine believes aligned (wrong alignment record) hits O_DIRECT and gets
    // EINVAL: the guest must get the error, never a silent buffered retry.
    #[test]
    fn test_einval_on_believed_aligned_request() {
        let Some((buffered, direct, path)) = o_direct_pair("einval", 16384, 0x11) else {
            return;
        };
        let m = mem_of(0x10000);
        m.write_slice(&[0x99; 512], GuestAddress(0x1000)).unwrap();
        let mut e = AsyncFileEngine::from_file(buffered, Some(direct), false).unwrap();
        e.set_dio_for_test(DioAlign { mem: 1, offset: 1 });
        e.push_write(
            100,
            &m,
            GuestAddress(0x1000),
            512,
            PendingRequest::default(),
        )
        .unwrap();
        e.drain(false).unwrap();
        let res = e.pop(&m).unwrap().unwrap().result;
        let data = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(
            res.is_err(),
            "the kernel accepted an unaligned O_DIRECT write here"
        );
        assert_eq!(
            data,
            vec![0x11u8; 16384],
            "no byte may land through any descriptor"
        );
        // The CQE carries the negative errno (-22), printed as "Unknown error -22".
        assert_eq!(res.unwrap_err().raw_os_error().unwrap().abs(), libc::EINVAL);
    }

    #[test]
    fn test_sync_einval_on_believed_aligned_request() {
        let Some((buffered, direct, path)) = o_direct_pair("einval-sync", 16384, 0x11) else {
            return;
        };
        let m = mem_of(0x10000);
        m.write_slice(&[0x99; 512], GuestAddress(0x1000)).unwrap();
        let mut e = SyncFileEngine::from_file(buffered, Some(direct));
        e.set_dio_for_test(DioAlign { mem: 1, offset: 1 });
        let w = e.write(100, &m, GuestAddress(0x1000), 512);
        let r = e.read(100, &m, GuestAddress(0x2000), 512);
        let data = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(w.is_err() && r.is_err());
        assert_eq!(data, vec![0x11u8; 16384]);
    }

    // Every shape through a real O_DIRECT descriptor (the bounce transfers must themselves be
    // valid O_DIRECT: buffer, offset and length aligned), both engines, then the whole file
    // checked against a shadow copy.
    #[test]
    fn test_bounce_on_o_direct_descriptor() {
        for et in [FileEngineType::Async, FileEngineType::Sync] {
            let Some((buffered, direct, path)) = o_direct_pair("shapes", 3 * 1048576, 0x01) else {
                return;
            };
            let m = mem_of(4 * 1048576);
            let mut engine = FileEngine::from_file(buffered, Some(direct), et, false).unwrap();
            let mut shadow = vec![0x01u8; 3 * 1048576];
            // (offset, guest address, length)
            let shapes: [(u64, u64, u32); 8] = [
                (4096, 0x1000, 4096),                   // aligned
                (8192 + 512, 0x3000, 512),              // one sector
                (12288, 0x5200, 8192),                  // misaligned buffer, whole blocks
                (20480 + 1536, 0x9000, 9216),           // span, partial ends
                (65536 + 512, 0x10200, 1048576 + 3072), // span over chunks, partial ends
                (2 * 1048576, 0x200000, 1048576),       // aligned 1 MiB
                (3 * 1048576 - 512, 0x300400, 512),     // last sector of the file
                (0, 0x380000, 512),                     // first sector
            ];
            for (i, (offset, addr, len)) in shapes.into_iter().enumerate() {
                let data: Vec<u8> = (0..len).map(|j| (j as u8) ^ (0x40 + i as u8)).collect();
                m.write_slice(&data, GuestAddress(addr)).unwrap();
                let (res, _) = run(&mut engine, true, offset, &m, addr, len);
                assert_eq!(res, Ok(len), "{et:?} write {i}");
                shadow[offset as usize..offset as usize + len as usize].copy_from_slice(&data);
                // Read it back through a misaligned guest buffer.
                let (res, _) = run(&mut engine, false, offset, &m, 0x3a0200, len.min(65536));
                assert_eq!(res, Ok(len.min(65536)), "{et:?} read {i}");
                let mut back = vec![0u8; len.min(65536) as usize];
                m.read_slice(&mut back, GuestAddress(0x3a0200)).unwrap();
                assert!(back[..] == shadow[offset as usize..offset as usize + back.len()]);
            }
            let data = std::fs::read(&path).unwrap();
            std::fs::remove_file(&path).unwrap();
            assert!(data == shadow, "{et:?}");
        }
    }

    // A regular file whose size is not a multiple of 4096 is not served in direct mode (the
    // blocks covering its last sectors run past the end of the file): the whole drive uses the
    // buffered descriptor, as direct_write: false. Its last sector reads and writes; a 4 KiB
    // read ending at the end of the file too.
    #[test]
    fn test_regular_file_tail_sector() {
        for et in [FileEngineType::Sync, FileEngineType::Async] {
            let len = 3 * 4096 + 512;
            let m = mem_of(0x10000);
            let (buffered, direct, path) = match o_direct_pair("tail", len, 0x77) {
                Some(p) => p,
                None => {
                    // No O_DIRECT here: the decision does not depend on it.
                    let mut f = TempFile::new().unwrap().into_file();
                    f.write_all(&vec![0x77; len]).unwrap();
                    (f.try_clone().unwrap(), f, String::new())
                }
            };
            let mut engine = FileEngine::from_file(buffered, Some(direct), et, false).unwrap();
            assert!(
                engine.dio().is_none(),
                "{et:?}: no direct mode for this file"
            );
            assert!(
                engine.direct_file().is_none(),
                "{et:?}: the direct descriptor is dropped"
            );
            let (res, kind) = run(&mut engine, false, 3 * 4096, &m, 0x1000, 512);
            assert_eq!((res, kind), (Ok(512), None), "{et:?}");
            m.write_slice(&[0x42; 512], GuestAddress(0x2000)).unwrap();
            let (res, _) = run(&mut engine, true, 3 * 4096, &m, 0x2000, 512);
            assert_eq!(res, Ok(512), "{et:?}");
            let (res, _) = run(&mut engine, false, 2 * 4096 + 512, &m, 0x3000, 4096);
            assert_eq!(res, Ok(4096), "{et:?}");
            let mut got = vec![0u8; 4096];
            m.read_slice(&mut got, GuestAddress(0x3000)).unwrap();
            assert_eq!(&got[..3584], &[0x77; 3584][..]);
            assert_eq!(&got[3584..], &[0x42; 512][..]);
            if !path.is_empty() {
                std::fs::remove_file(&path).unwrap();
            }
        }
    }

    // A failed bounce transfer (the backing shrank under the drive) completes its request with
    // an error once, and the engine takes the next bounce.
    #[test]
    fn test_bounce_error_completes_once() {
        for et in [FileEngineType::Async, FileEngineType::Sync] {
            let m = mem_of(0x10000);
            let (b, d, _bc, dc) = pair(16384);
            let mut engine = FileEngine::from_file(b, Some(d), et, false).unwrap();
            dc.set_len(8192).unwrap();
            let (res, _) = run(&mut engine, false, 12288 + 512, &m, 0x1000, 512);
            assert!(res.is_err(), "{et:?}");
            assert!(!engine.bounce_in_progress());
            let (res, kind) = run(&mut engine, false, 512, &m, 0x1000, 512);
            assert_eq!((res, kind), (Ok(512), Some(Bounce::Read)), "{et:?}");
            assert!(complete_all(&mut engine, &m).is_empty());
        }
    }

    #[test]
    fn test_sync() {
        let mem = create_mem();
        // Create backing file.
        let file = TempFile::new().unwrap().into_file();
        let mut engine = FileEngine::from_file(file, None, FileEngineType::Sync, false).unwrap();

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
        let mut engine = FileEngine::from_file(file, None, FileEngineType::Async, false).unwrap();

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
