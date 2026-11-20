// Copyright 2021 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;

use vm_memory::{GuestMemoryBackend, GuestMemoryError};

use super::{io_window, segment_iovecs};
use crate::devices::virtio::block::virtio::request::DataSegment;
use crate::vstate::memory::{GuestAddress, GuestMemoryExtension, GuestMemoryMmap};

/// Moves `iovecs` past the first `done` bytes.
pub(super) fn advance_iovecs(iovecs: &mut Vec<libc::iovec>, mut done: usize) {
    let mut drop = 0;
    for iov in iovecs.iter_mut() {
        if done < iov.iov_len {
            iov.iov_base = iov.iov_base.wrapping_add(done);
            iov.iov_len -= done;
            break;
        }
        done -= iov.iov_len;
        drop += 1;
    }
    iovecs.drain(..drop);
}

/// `readv`/`writev` until every byte moved, EOF, or an error. Returns the bytes moved.
fn transfer_all(
    fd: libc::c_int,
    iovecs: &mut [libc::iovec],
    offset: u64,
    write: bool,
) -> Result<u32, (std::io::Error, u32)> {
    transfer_with(iovecs, offset, |window, bytes, pos| {
        // SAFETY: pointers refer to mapped guest memory throughout this synchronous call.
        let ret = unsafe {
            if window.len() == 1 {
                if write {
                    libc::pwrite(fd, window[0].iov_base, bytes, pos)
                } else {
                    libc::pread(fd, window[0].iov_base, bytes, pos)
                }
            } else if write {
                libc::pwritev(
                    fd,
                    window.as_ptr(),
                    i32::try_from(window.len()).expect("window is bounded by MAX_IOVECS"),
                    pos,
                )
            } else {
                libc::preadv(
                    fd,
                    window.as_ptr(),
                    i32::try_from(window.len()).expect("window is bounded by MAX_IOVECS"),
                    pos,
                )
            }
        };
        if ret < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(usize::try_from(ret).expect("syscall result is nonnegative"))
        }
    })
}

fn transfer_with<F>(
    iovecs: &mut [libc::iovec],
    offset: u64,
    mut operation: F,
) -> Result<u32, (std::io::Error, u32)>
where
    F: FnMut(&[libc::iovec], usize, i64) -> Result<usize, std::io::Error>,
{
    let mut total = 0u32;
    let mut window = if iovecs.len() > 1 {
        Vec::with_capacity(iovecs.len().min(super::MAX_IOVECS))
    } else {
        Vec::new()
    };
    let mut index = 0;
    while index < iovecs.len() {
        let remaining = &iovecs[index..];
        let single;
        let (active, bytes) = if remaining.len() == 1 {
            let len = remaining[0].iov_len.min(super::MAX_IO_BYTES);
            single = libc::iovec {
                iov_base: remaining[0].iov_base,
                iov_len: len,
            };
            (std::slice::from_ref(&single), len)
        } else {
            let bytes = io_window(remaining, &mut window);
            (window.as_slice(), bytes)
        };
        let pos = offset
            .checked_add(u64::from(total))
            .and_then(|x| i64::try_from(x).ok())
            .ok_or_else(|| (std::io::Error::other("file offset overflow"), total))?;
        let moved = match operation(active, bytes, pos) {
            Ok(moved) => moved,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err((err, total)),
        };
        if moved == 0 {
            return Err((
                std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "zero progress"),
                total,
            ));
        }
        if moved > bytes {
            return Err((std::io::Error::other("invalid transfer result"), total));
        }
        total = total
            .checked_add(u32::try_from(moved).unwrap())
            .ok_or_else(|| (std::io::Error::other("transfer overflow"), total))?;
        let mut left = moved;
        while index < iovecs.len() && left >= iovecs[index].iov_len {
            left -= iovecs[index].iov_len;
            index += 1;
        }
        if index < iovecs.len() {
            iovecs[index].iov_base = iovecs[index].iov_base.wrapping_add(left);
            iovecs[index].iov_len -= left;
        }
    }
    Ok(total)
}

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum SyncIoError {
    /// Discard: {0}
    Discard(std::io::Error),
    /// Flush: {0}
    Flush(std::io::Error),
    /// SyncAll: {0}
    SyncAll(std::io::Error),
    /// Transfer: {0}
    Transfer(GuestMemoryError),
    /// Vectored transfer after {1} bytes: {0}
    Vectored(std::io::Error, u32),
}

#[derive(Debug)]
pub struct SyncFileEngine {
    file: File,
}

// SAFETY: `File` is send and ultimately a POD.
unsafe impl Send for SyncFileEngine {}

const BLKDISCARD: libc::Ioctl = 0x1277;
const FALLOC_FL_KEEP_SIZE: libc::c_int = 0x01;
const FALLOC_FL_PUNCH_HOLE: libc::c_int = 0x02;

pub(super) fn file_discard_range(file: &File, range: (u64, u32)) -> Result<u32, std::io::Error> {
    let file_type = file.metadata()?.file_type();
    let (offset, len) = range;

    if len == 0 {
        return Ok(0);
    }
    let discarded = len;
    let len = u64::from(len);

    if file_type.is_block_device() {
        let mut range = [offset, len];
        // SAFETY: file is a valid fd, BLKDISCARD expects a pointer to two u64 values
        // representing byte offset and byte length.
        let ret = unsafe { libc::ioctl(file.as_raw_fd(), BLKDISCARD, range.as_mut_ptr()) };
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }
    } else {
        let off = libc::off_t::try_from(offset).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "discard offset overflow")
        })?;
        let len = libc::off_t::try_from(len).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "discard length overflow")
        })?;
        // SAFETY: file is a valid fd and fallocate does not retain the passed values.
        let ret = unsafe {
            libc::fallocate(
                file.as_raw_fd(),
                FALLOC_FL_KEEP_SIZE | FALLOC_FL_PUNCH_HOLE,
                off,
                len,
            )
        };
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    Ok(discarded)
}

impl SyncFileEngine {
    pub fn from_file(file: File) -> SyncFileEngine {
        SyncFileEngine { file }
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    /// Update the backing file of the engine
    pub fn update_file(&mut self, file: File) {
        self.file = file
    }

    pub fn read(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
    ) -> Result<u32, SyncIoError> {
        if count == 0 {
            return Ok(0);
        }
        let slice = mem
            .get_slice(addr, count as usize)
            .map_err(SyncIoError::Transfer)?;
        let mut iov = [libc::iovec {
            iov_base: slice.ptr_guard_mut().as_ptr().cast(),
            iov_len: count as usize,
        }];
        let result = transfer_all(self.file.as_raw_fd(), &mut iov, offset, false);
        mem.mark_dirty(addr, count as usize);
        result.map_err(|(error, completed)| SyncIoError::Vectored(error, completed))
    }

    pub fn write(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
    ) -> Result<u32, SyncIoError> {
        if count == 0 {
            // Even an empty write must fail on an O_RDONLY backing file. Linux checks
            // file access before treating the zero-byte request as a no-op.
            let off = i64::try_from(offset).map_err(|_| {
                SyncIoError::Vectored(std::io::Error::other("file offset overflow"), 0)
            })?;
            // SAFETY: no bytes are accessed through this null pointer.
            let result = unsafe { libc::pwrite(self.file.as_raw_fd(), std::ptr::null(), 0, off) };
            return if result < 0 {
                Err(SyncIoError::Vectored(std::io::Error::last_os_error(), 0))
            } else {
                Ok(0)
            };
        }
        let slice = mem
            .get_slice(addr, count as usize)
            .map_err(SyncIoError::Transfer)?;
        let mut iov = [libc::iovec {
            iov_base: slice.ptr_guard().as_ptr().cast_mut().cast(),
            iov_len: count as usize,
        }];
        let result = transfer_all(self.file.as_raw_fd(), &mut iov, offset, true);
        result.map_err(|(error, completed)| SyncIoError::Vectored(error, completed))
    }

    pub fn readv(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        segments: &[DataSegment],
        _count: u32,
    ) -> Result<u32, SyncIoError> {
        let mut iovecs = segment_iovecs(mem, segments).map_err(SyncIoError::Transfer)?;
        let result = transfer_all(self.file.as_raw_fd(), &mut iovecs, offset, false);
        // A failing syscall may still have modified memory; conservatively dirty all ranges.
        for &(addr, len) in segments {
            mem.mark_dirty(addr, len as usize);
        }
        result.map_err(|(error, completed)| SyncIoError::Vectored(error, completed))
    }

    pub fn writev(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        segments: &[DataSegment],
        _count: u32,
    ) -> Result<u32, SyncIoError> {
        let mut iovecs = segment_iovecs(mem, segments).map_err(SyncIoError::Transfer)?;
        transfer_all(self.file.as_raw_fd(), &mut iovecs, offset, true)
            .map_err(|(error, completed)| SyncIoError::Vectored(error, completed))
    }

    pub fn flush(&mut self) -> Result<(), SyncIoError> {
        // flush() first to force any cached data out of rust buffers.
        self.file.flush().map_err(SyncIoError::Flush)?;
        // Sync data out to physical media on host.
        self.file.sync_all().map_err(SyncIoError::SyncAll)
    }

    pub fn discard(&mut self, range: (u64, u32)) -> Result<u32, SyncIoError> {
        file_discard_range(&self.file, range).map_err(SyncIoError::Discard)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};

    use vmm_sys_util::tempfile::TempFile;

    use super::file_discard_range;

    #[test]
    fn partial_error_preserves_prefix_and_offset() {
        let iov = |base, len| libc::iovec {
            iov_base: base as *mut _,
            iov_len: len,
        };
        let mut calls = 0;
        let err = super::transfer_with(
            &mut [iov(0x1000, 512), iov(0x3000, 512)],
            4096,
            |window, _, pos| {
                calls += 1;
                match calls {
                    1 => {
                        assert_eq!(pos, 4096);
                        Ok(700)
                    }
                    2 => {
                        assert_eq!(pos, 4796);
                        assert_eq!(window[0].iov_base as usize, 0x3000 + 188);
                        Err(std::io::Error::from_raw_os_error(libc::EIO))
                    }
                    _ => unreachable!(),
                }
            },
        )
        .unwrap_err();
        assert_eq!(err.1, 700);
        assert_eq!(calls, 2);
    }

    #[test]
    fn windows_respect_iovec_and_byte_limits() {
        let iov = || libc::iovec {
            iov_base: 0x1000 as *mut _,
            iov_len: 512,
        };
        let mut sizes = Vec::new();
        let total = super::transfer_with(
            &mut (0..1030).map(|_| iov()).collect::<Vec<_>>(),
            0,
            |window, bytes, _| {
                sizes.push((window.len(), bytes));
                Ok(bytes)
            },
        )
        .unwrap();
        assert_eq!(total, 1030 * 512);
        assert_eq!(sizes, [(1024, 1024 * 512), (6, 6 * 512)]);
        let huge = libc::iovec {
            iov_base: 0x1000 as *mut _,
            iov_len: super::super::MAX_IO_BYTES + 4096,
        };
        let mut lengths = Vec::new();
        let total = super::transfer_with(&mut [huge], 0, |_, bytes, _| {
            lengths.push(bytes);
            Ok(bytes)
        })
        .unwrap();
        assert_eq!(total as usize, super::super::MAX_IO_BYTES + 4096);
        assert_eq!(lengths, [super::super::MAX_IO_BYTES, 4096]);
    }

    #[test]
    fn test_discard_regular_file() {
        let mut file = TempFile::new().unwrap().into_file();
        file.write_all(&vec![0x5a; 4096]).unwrap();

        match file_discard_range(&file, (1024, 2048)) {
            Ok(discarded) => assert_eq!(discarded, 2048),
            // Some filesystems do not support hole punching; that is a host/filesystem
            // capability limitation, not a test failure for the regular discard path.
            Err(err)
                if matches!(
                    err.raw_os_error(),
                    Some(libc::EOPNOTSUPP) | Some(libc::ENOSYS)
                ) =>
            {
                return;
            }
            Err(err) => panic!("discard failed: {err}"),
        }

        file.seek(SeekFrom::Start(0)).unwrap();
        let mut data = vec![0u8; 4096];
        file.read_exact(&mut data).unwrap();

        assert_eq!(&data[..1024], vec![0x5a; 1024].as_slice());
        assert_eq!(&data[1024..3072], vec![0; 2048].as_slice());
        assert_eq!(&data[3072..], vec![0x5a; 1024].as_slice());
    }
}
