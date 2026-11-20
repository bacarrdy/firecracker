// Copyright 2021 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::VecDeque;
use std::fmt::Debug;
use std::fs::File;
use std::os::fd::RawFd;
use std::os::unix::io::AsRawFd;

use vm_memory::{GuestMemoryBackend, GuestMemoryError};
use vmm_sys_util::eventfd::EventFd;

use super::{RequestError, io_window_with_limit, segment_iovecs};
use crate::devices::virtio::block::virtio::request::DataSegment;
use crate::devices::virtio::block::virtio::{IO_URING_NUM_ENTRIES, PendingRequest};
use crate::io_uring::operation::{Cqe, OpCode, Operation};
use crate::io_uring::restriction::Restriction;
use crate::io_uring::{IoUring, IoUringError};
use crate::logger::log_dev_preview_warning;
use crate::vstate::memory::{GuestAddress, GuestMemoryExtension, GuestMemoryMmap};

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum AsyncIoError {
    /// Not implemented
    NotImplemented,
    /// IO: {0}
    IO(std::io::Error),
    /// IoUring: {0}
    IoUring(IoUringError),
    /// Submit: {0}
    Submit(std::io::Error),
    /// SyncAll: {0}
    SyncAll(std::io::Error),
    /// EventFd: {0}
    EventFd(std::io::Error),
    /// GuestMemory: {0}
    GuestMemory(GuestMemoryError),
}

#[derive(Debug)]
pub struct AsyncFileEngine {
    file: File,
    ring: IoUring<WrappedRequest>,
    completion_evt: EventFd,
    continuations: VecDeque<WrappedRequest>,
    #[cfg(test)]
    fail_submit_once: bool,
    #[cfg(test)]
    test_window_limit: usize,
}

#[derive(Debug)]
pub struct WrappedRequest {
    req: PendingRequest,
    transfer: Option<Transfer>,
}

#[derive(Debug)]
struct Transfer {
    // Keep the mapping alive until the last kernel completion, independently of device state.
    mem: GuestMemoryMmap,
    dirty_segments: Vec<DataSegment>,
    dirty_scalar: Option<DataSegment>,
    scalar: Option<libc::iovec>,
    remaining: Vec<libc::iovec>,
    window: Vec<libc::iovec>,
    window_bytes: usize,
    window_limit: usize,
    offset: u64,
    completed: u32,
    write: bool,
}

// SAFETY: all raw pointers refer to the retained guest memory mappings; iovecs themselves are
// owned and never modified while the kernel uses them. No Rust mutable references are constructed.
unsafe impl Send for WrappedRequest {}

impl WrappedRequest {
    fn new(req: PendingRequest) -> Self {
        Self {
            req,
            transfer: None,
        }
    }

    // Returning the owner on failure preserves request state without allocating a Box
    // on every operation; this method is on the submission hot path.
    #[allow(clippy::result_large_err)]
    fn operation(mut self) -> Result<Operation<Self>, Self> {
        let t = self.transfer.as_mut().unwrap();
        let Some(offset) = t.offset.checked_add(u64::from(t.completed)) else {
            return Err(self);
        };
        let write = t.write;
        let (ptr, len, vectored) = if let Some(scalar) = &t.scalar {
            t.window_bytes = scalar.iov_len.min(t.window_limit);
            (
                scalar.iov_base as usize,
                u32::try_from(t.window_bytes).expect("window is bounded by MAX_IO_BYTES"),
                false,
            )
        } else {
            t.window_bytes = io_window_with_limit(&t.remaining, &mut t.window, t.window_limit);
            if t.window.is_empty() {
                (0, 0, false)
            } else if t.window.len() == 1 {
                (
                    t.window[0].iov_base as usize,
                    u32::try_from(t.window_bytes).expect("window is bounded by MAX_IO_BYTES"),
                    false,
                )
            } else {
                (
                    t.window.as_ptr() as usize,
                    u32::try_from(t.window.len()).expect("window is bounded by MAX_IOVECS"),
                    true,
                )
            }
        };
        Ok(match (write, vectored) {
            (false, false) => Operation::read(0, ptr, len, offset, self),
            (true, false) => Operation::write(0, ptr, len, offset, self),
            (false, true) => Operation::readv(0, ptr, len, offset, self),
            (true, true) => Operation::writev(0, ptr, len, offset, self),
        })
    }
}

impl AsyncFileEngine {
    fn new_ring(
        file: &File,
        completion_fd: RawFd,
    ) -> Result<IoUring<WrappedRequest>, IoUringError> {
        IoUring::new(
            u32::from(IO_URING_NUM_ENTRIES),
            vec![file],
            vec![
                // Make sure we only allow operations on pre-registered fds.
                Restriction::RequireFixedFds,
                // Allowlist of opcodes.
                Restriction::AllowOpCode(OpCode::Read),
                Restriction::AllowOpCode(OpCode::Write),
                Restriction::AllowOpCode(OpCode::Fsync),
                Restriction::AllowOpCode(OpCode::Readv),
                Restriction::AllowOpCode(OpCode::Writev),
            ],
            Some(completion_fd),
        )
    }

    pub fn from_file(file: File) -> Result<AsyncFileEngine, AsyncIoError> {
        log_dev_preview_warning("Async file IO", Option::None);

        let completion_evt = EventFd::new(libc::EFD_NONBLOCK).map_err(AsyncIoError::EventFd)?;
        let ring =
            Self::new_ring(&file, completion_evt.as_raw_fd()).map_err(AsyncIoError::IoUring)?;

        Ok(AsyncFileEngine {
            file,
            ring,
            completion_evt,
            continuations: VecDeque::with_capacity(usize::from(IO_URING_NUM_ENTRIES) * 2),
            #[cfg(test)]
            fail_submit_once: false,
            #[cfg(test)]
            test_window_limit: super::MAX_IO_BYTES,
        })
    }

    pub fn update_file(&mut self, file: File) -> Result<(), AsyncIoError> {
        if self.in_flight() != 0 {
            return Err(AsyncIoError::IO(std::io::Error::other(
                "active I/O while changing backing file",
            )));
        }
        let ring = Self::new_ring(&file, self.completion_evt.as_raw_fd())
            .map_err(AsyncIoError::IoUring)?;

        self.file = file;
        self.ring = ring;
        Ok(())
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn completion_evt(&self) -> &EventFd {
        &self.completion_evt
    }

    pub fn push_read(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
        req: PendingRequest,
    ) -> Result<(), RequestError<AsyncIoError>> {
        self.push_scalar(offset, mem, addr, count, false, req)
    }

    pub fn push_write(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
        req: PendingRequest,
    ) -> Result<(), RequestError<AsyncIoError>> {
        self.push_scalar(offset, mem, addr, count, true, req)
    }

    pub fn push_readv(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        segments: Vec<DataSegment>,
        req: PendingRequest,
    ) -> Result<(), RequestError<AsyncIoError>> {
        self.push_transfer(offset, mem, segments, false, req)
    }

    pub fn push_writev(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        segments: Vec<DataSegment>,
        req: PendingRequest,
    ) -> Result<(), RequestError<AsyncIoError>> {
        self.push_transfer(offset, mem, segments, true, req)
    }

    fn push_scalar(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
        write: bool,
        req: PendingRequest,
    ) -> Result<(), RequestError<AsyncIoError>> {
        let ptr = if count == 0 {
            std::ptr::null_mut()
        } else {
            match mem.get_slice(addr, count as usize) {
                Ok(slice) => slice.ptr_guard_mut().as_ptr(),
                Err(error) => {
                    return Err(RequestError {
                        req,
                        error: AsyncIoError::GuestMemory(error),
                    });
                }
            }
        };
        let wrapped = WrappedRequest {
            req,
            transfer: Some(Transfer {
                mem: mem.clone(),
                dirty_segments: Vec::new(),
                dirty_scalar: if write || count == 0 {
                    None
                } else {
                    Some((addr, count))
                },
                scalar: Some(libc::iovec {
                    iov_base: ptr.cast(),
                    iov_len: count as usize,
                }),
                remaining: Vec::new(),
                window: Vec::new(),
                window_bytes: 0,
                window_limit: self.window_limit(),
                offset,
                completed: 0,
                write,
            }),
        };
        let operation = wrapped.operation().map_err(|data| RequestError {
            req: data.req,
            error: AsyncIoError::IO(std::io::Error::other("invalid I/O window")),
        })?;
        self.ring
            .push(operation)
            .map_err(|(error, data)| RequestError {
                req: data.req,
                error: AsyncIoError::IoUring(error),
            })
    }

    fn push_transfer(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        segments: Vec<DataSegment>,
        write: bool,
        req: PendingRequest,
    ) -> Result<(), RequestError<AsyncIoError>> {
        let remaining = match segment_iovecs(mem, &segments) {
            Ok(value) => value,
            Err(error) => {
                return Err(RequestError {
                    req,
                    error: AsyncIoError::GuestMemory(error),
                });
            }
        };
        let capacity = remaining.len().min(super::MAX_IOVECS);
        let wrapped = WrappedRequest {
            req,
            transfer: Some(Transfer {
                mem: mem.clone(),
                dirty_segments: if write { Vec::new() } else { segments },
                dirty_scalar: None,
                scalar: None,
                remaining,
                window: Vec::with_capacity(capacity),
                window_bytes: 0,
                window_limit: self.window_limit(),
                offset,
                completed: 0,
                write,
            }),
        };
        let operation = wrapped.operation().map_err(|data| RequestError {
            req: data.req,
            error: AsyncIoError::IO(std::io::Error::other("invalid I/O window")),
        })?;
        self.ring
            .push(operation)
            .map_err(|(error, data)| RequestError {
                req: data.req,
                error: AsyncIoError::IoUring(error),
            })
    }

    pub fn push_flush(&mut self, req: PendingRequest) -> Result<(), RequestError<AsyncIoError>> {
        let wrapped_user_data = WrappedRequest::new(req);

        self.ring
            .push(Operation::fsync(0, wrapped_user_data))
            .map_err(|(io_uring_error, data)| RequestError {
                req: data.req,
                error: AsyncIoError::IoUring(io_uring_error),
            })
    }

    pub fn discard(&mut self, _range: (u64, u32)) -> Result<u32, AsyncIoError> {
        Err(AsyncIoError::NotImplemented)
    }

    #[cfg(test)]
    pub(crate) fn inject_submit_failure(&mut self) {
        self.fail_submit_once = true;
    }

    fn window_limit(&self) -> usize {
        #[cfg(test)]
        {
            self.test_window_limit
        }
        #[cfg(not(test))]
        {
            super::MAX_IO_BYTES
        }
    }

    #[cfg(test)]
    pub(super) fn set_test_window_limit(&mut self, limit: usize) {
        self.test_window_limit = limit;
    }

    fn submit_ring(&mut self) -> Result<(), AsyncIoError> {
        #[cfg(test)]
        if std::mem::take(&mut self.fail_submit_once) {
            return Err(AsyncIoError::IO(std::io::Error::from_raw_os_error(
                libc::EAGAIN,
            )));
        }
        while self.ring.unsubmitted_sqes() != 0 {
            let submitted = self.ring.submit().map_err(AsyncIoError::IoUring)?;
            if submitted == 0 {
                return Err(AsyncIoError::IO(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "io_uring submission made no progress",
                )));
            }
        }
        Ok(())
    }

    fn submit_pending(&mut self) -> Result<(), AsyncIoError> {
        self.submit_ring()?;
        while let Some(wrapped) = self.continuations.pop_front() {
            let operation = match wrapped.operation() {
                Ok(op) => op,
                Err(_) => unreachable!("continuations are validated before queueing"),
            };
            match self.ring.push(operation) {
                Ok(()) => {}
                Err((error, wrapped)) => {
                    self.continuations.push_front(wrapped);
                    return Err(AsyncIoError::IoUring(error));
                }
            }
            self.submit_ring()?;
        }
        Ok(())
    }

    pub fn kick_submission_queue(&mut self) -> Result<(), AsyncIoError> {
        self.submit_pending()
    }

    pub fn drain(&mut self, discard_cqes: bool) -> Result<(), AsyncIoError> {
        loop {
            self.submit_pending()?;
            self.ring
                .submit_and_wait_all()
                .map_err(AsyncIoError::IoUring)?;
            if !discard_cqes {
                return Ok(());
            }
            while self.pop_inner()?.is_some() {}
            if self.in_flight() == 0 {
                return Ok(());
            }
        }
    }

    pub fn drain_and_flush(&mut self, discard_cqes: bool) -> Result<(), AsyncIoError> {
        self.drain(discard_cqes)?;

        // Sync data out to physical media on host.
        // We don't need to call flush first since all the ops are performed through io_uring
        // and Rust shouldn't manage any data in its internal buffers.
        self.file.sync_all().map_err(AsyncIoError::SyncAll)?;

        Ok(())
    }

    fn do_pop(&mut self) -> Result<Option<Cqe<WrappedRequest>>, AsyncIoError> {
        self.ring.pop().map_err(AsyncIoError::IoUring)
    }

    /// Number of accepted operations, including completions not yet consumed.
    pub fn in_flight(&self) -> u32 {
        self.ring.num_ops()
            + u32::try_from(self.continuations.len())
                .expect("continuations are bounded by ring capacity")
    }

    pub fn can_accept(&self) -> Result<bool, AsyncIoError> {
        Ok(self.continuations.is_empty() && self.ring.can_push().map_err(AsyncIoError::IoUring)?)
    }

    pub fn pop(
        &mut self,
        _mem: &GuestMemoryMmap,
    ) -> Result<Option<Cqe<PendingRequest>>, AsyncIoError> {
        self.pop_inner()
    }

    fn pop_inner(&mut self) -> Result<Option<Cqe<PendingRequest>>, AsyncIoError> {
        loop {
            let Some(cqe) = self.do_pop()? else {
                return Ok(None);
            };
            let result = cqe.result();
            let mut wrapped = cqe.user_data();
            let Some(transfer) = wrapped.transfer.as_mut() else {
                return Ok(Some(Cqe::new(
                    result.map(i64::from).unwrap_or(-libc::EIO as i64),
                    wrapped.req,
                )));
            };
            // Mark after every CQE, even negative ones, to cover writes to guest memory on errors
            // and a dirty bitmap reset between child completions.
            if let Some((addr, len)) = transfer.dirty_scalar {
                transfer.mem.mark_dirty(addr, len as usize);
            }
            for &(addr, len) in &transfer.dirty_segments {
                transfer.mem.mark_dirty(addr, len as usize);
            }
            if matches!(result, Ok(0))
                && transfer.window_bytes == 0
                && transfer.remaining.is_empty()
            {
                return Ok(Some(Cqe::new(i64::from(transfer.completed), wrapped.req)));
            }
            let count = match result {
                Ok(n) if n > 0 && n as usize <= transfer.window_bytes => n,
                _ => {
                    return Ok(Some(
                        Cqe::new(-i64::from(libc::EIO), wrapped.req)
                            .with_progress(transfer.completed),
                    ));
                }
            };
            transfer.completed = match transfer.completed.checked_add(count) {
                Some(n) => n,
                None => {
                    return Ok(Some(
                        Cqe::new(-i64::from(libc::EIO), wrapped.req)
                            .with_progress(transfer.completed),
                    ));
                }
            };
            if let Some(scalar) = transfer.scalar.as_mut() {
                scalar.iov_base = scalar.iov_base.wrapping_add(count as usize);
                scalar.iov_len -= count as usize;
            } else {
                super::sync_io::advance_iovecs(&mut transfer.remaining, count as usize);
            }
            if transfer
                .scalar
                .as_ref()
                .map_or(transfer.remaining.is_empty(), |s| s.iov_len == 0)
            {
                return Ok(Some(Cqe::new(i64::from(transfer.completed), wrapped.req)));
            }
            if transfer
                .offset
                .checked_add(u64::from(transfer.completed))
                .is_none()
            {
                return Ok(Some(
                    Cqe::new(-i64::from(libc::EIO), wrapped.req).with_progress(transfer.completed),
                ));
            }
            self.continuations.push_back(wrapped);
            self.submit_pending()?;
        }
    }
}

impl Drop for AsyncFileEngine {
    fn drop(&mut self) {
        if self.in_flight() != 0 && (self.drain(true).is_err() || self.in_flight() != 0) {
            // The kernel may still dereference request-owned guest mappings and iovec arrays.
            // Releasing them on a failed drain would be a use-after-free.
            std::process::abort();
        }
    }
}
