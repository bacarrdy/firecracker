// Copyright 2021 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::ffi::CStr;
use std::fmt::Debug;
use std::fs::File;
use std::os::fd::RawFd;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

use vm_memory::{GuestMemoryBackend, GuestMemoryError};
use vmm_sys_util::eventfd::EventFd;

use crate::devices::virtio::block::virtio::io::direct::{
    Bounce, BounceBuf, BouncePlan, DioAlign, Wait, bounce_buf,
};
use crate::devices::virtio::block::virtio::io::{DIRECT_WRITE_FD, RequestError};
use crate::devices::virtio::block::virtio::{IO_URING_NUM_ENTRIES, PendingRequest};
use crate::io_uring::operation::{Cqe, OpCode, Operation};
use crate::io_uring::restriction::Restriction;
use crate::io_uring::{IoUring, IoUringError};
use crate::logger::{error, log_dev_preview_warning, warn};
use crate::vstate::memory::{GuestAddress, GuestMemoryExtension, GuestMemoryMmap};

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum AsyncIoError {
    /// Not implemented
    NotImplemented,
    /// Discard is not supported with this async backend on the host kernel.
    DiscardUnsupported,
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
    /// Direct I/O request not aligned for the O_DIRECT descriptor (it needs the bounce path)
    DirectUnaligned,
    /// Direct I/O bounce: {0}
    Bounce(std::io::Error),
}

#[derive(Debug)]
pub struct AsyncFileEngine {
    file: File,
    direct_file: Option<File>,
    dio: Option<DioAlign>,
    ring: IoUring<WrappedRequest>,
    completion_evt: EventFd,
    discard_op: Option<AsyncDiscardOp>,
    /// Direct I/O: the drive's bounce buffer (allocated at the first bounce) and the bounced
    /// request in progress, whose transfers go through `ring` one at a time.
    bounce_buf: Option<BounceBuf>,
    bounce: Option<ActiveBounce>,
    /// The next transfer of the bounce in progress that the ring did not take yet (see
    /// `queue_bounce_step`); retried at the end of every completion pass.
    owed: Option<Owed>,
}

/// A transfer of the bounce in progress that is owed to the ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owed {
    /// Not queued: the submission queue was full.
    Push,
    /// Queued, not submitted: `io_uring_enter` failed.
    Submit,
}

/// A guest request in progress on the bounce path.
#[derive(Debug)]
struct ActiveBounce {
    plan: BouncePlan,
    req: PendingRequest,
    started: Instant,
}

/// A completed guest request.
#[derive(Debug)]
pub struct Completion {
    pub req: PendingRequest,
    pub result: Result<u32, std::io::Error>,
    /// Confirmed guest bytes of a failed bounce.
    pub progress: u32,
    /// Set when the request went through the bounce path: how, and how long it took.
    pub bounce: Option<(Bounce, Duration)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AsyncDiscardOp {
    BlockUringCmd,
    Fallocate,
}

#[derive(Debug)]
pub enum WrappedRequest {
    /// A guest request transferred as is (`addr`: guest memory the transfer writes;
    /// `writes`: the backing range a write or discard modifies).
    Guest {
        addr: Option<GuestAddress>,
        writes: Option<(u64, u64)>,
        req: PendingRequest,
    },
    /// One transfer of the bounce in progress.
    BounceStep,
}

impl WrappedRequest {
    fn new(req: PendingRequest) -> Self {
        WrappedRequest::Guest {
            addr: None,
            writes: None,
            req,
        }
    }

    fn new_with_dirty_tracking(addr: GuestAddress, req: PendingRequest) -> Self {
        WrappedRequest::Guest {
            addr: Some(addr),
            writes: None,
            req,
        }
    }

    fn new_modifying(offset: u64, len: u64, req: PendingRequest) -> Self {
        WrappedRequest::Guest {
            addr: None,
            writes: Some((offset, offset.saturating_add(len))),
            req,
        }
    }

    /// The guest request of an op that failed to be queued.
    fn into_req(self) -> PendingRequest {
        match self {
            WrappedRequest::Guest { req, .. } => req,
            WrappedRequest::BounceStep => unreachable!("guest requests are queued as Guest"),
        }
    }
}

impl AsyncFileEngine {
    const BLOCK_URING_CMD_DISCARD: u32 = 0x1200;
    const FALLOC_FL_KEEP_SIZE: u32 = 0x01;
    const FALLOC_FL_PUNCH_HOLE: u32 = 0x02;
    const MIN_BLOCK_URING_DISCARD_KERNEL: (u32, u32) = (6, 12);

    fn new_ring(
        file: &File,
        direct_file: Option<&File>,
        completion_fd: RawFd,
        discard_op: Option<AsyncDiscardOp>,
    ) -> Result<IoUring<WrappedRequest>, IoUringError> {
        let mut files = vec![file];
        if let Some(direct_file) = direct_file {
            files.push(direct_file);
        }

        let mut restrictions = vec![
            // Make sure we only allow operations on pre-registered fds.
            Restriction::RequireFixedFds,
            // Allowlist of opcodes.
            Restriction::AllowOpCode(OpCode::Read),
            Restriction::AllowOpCode(OpCode::Write),
            Restriction::AllowOpCode(OpCode::Fsync),
        ];
        let mut required_ops = vec![OpCode::Read, OpCode::Write, OpCode::Fsync];
        match discard_op {
            Some(AsyncDiscardOp::Fallocate) => {
                restrictions.push(Restriction::AllowOpCode(OpCode::Fallocate));
                required_ops.push(OpCode::Fallocate);
            }
            Some(AsyncDiscardOp::BlockUringCmd) => {
                restrictions.push(Restriction::AllowAsync);
                restrictions.push(Restriction::AllowOpCode(OpCode::UringCmd));
                required_ops.push(OpCode::UringCmd);
            }
            None => {}
        }

        IoUring::new_with_required_ops(
            u32::from(IO_URING_NUM_ENTRIES),
            files,
            restrictions,
            Some(completion_fd),
            &required_ops,
        )
    }

    pub fn from_file(
        file: File,
        direct_file: Option<File>,
        discard: bool,
    ) -> Result<AsyncFileEngine, AsyncIoError> {
        log_dev_preview_warning("Async file IO", Option::None);

        let completion_evt = EventFd::new(libc::EFD_NONBLOCK).map_err(AsyncIoError::EventFd)?;
        let discard_op = Self::discard_op(&file, discard)?;
        // A backing that direct I/O cannot serve exactly runs on the buffered descriptor alone.
        let dio = direct_file.as_ref().and_then(DioAlign::probe);
        let direct_file = direct_file.filter(|_| dio.is_some());
        let ring = Self::new_ring(
            &file,
            direct_file.as_ref(),
            completion_evt.as_raw_fd(),
            discard_op,
        )
        .map_err(AsyncIoError::IoUring)?;

        Ok(AsyncFileEngine {
            file,
            direct_file,
            dio,
            ring,
            completion_evt,
            discard_op,
            bounce_buf: None,
            bounce: None,
            owed: None,
        })
    }

    pub fn update_file(
        &mut self,
        file: File,
        direct_file: Option<File>,
    ) -> Result<(), AsyncIoError> {
        // The device completes a bounce in progress before it replaces the backing
        // (`VirtioBlock::update_disk_image`). Should one still be in progress, its transfer must
        // complete before the ring is dropped: the kernel would still write into the bounce
        // buffer. Its guest request is then lost, as every request in flight on a replaced ring.
        if self.bounce.is_some() {
            error!("Backing file replaced while a direct I/O bounce was in progress");
            self.drain(true)?;
        }
        let discard_op = Self::discard_op(&file, self.discard_op.is_some())?;
        let dio = direct_file.as_ref().and_then(DioAlign::probe);
        let direct_file = direct_file.filter(|_| dio.is_some());
        let ring = Self::new_ring(
            &file,
            direct_file.as_ref(),
            self.completion_evt.as_raw_fd(),
            discard_op,
        )
        .map_err(AsyncIoError::IoUring)?;

        self.ring = ring;
        self.file = file;
        self.discard_op = discard_op;
        self.dio = dio;
        self.direct_file = direct_file;
        Ok(())
    }

    fn discard_op(file: &File, discard: bool) -> Result<Option<AsyncDiscardOp>, AsyncIoError> {
        if !discard {
            return Ok(None);
        }

        if file
            .metadata()
            .map_err(AsyncIoError::IO)?
            .file_type()
            .is_block_device()
        {
            // BLOCK_URING_CMD_DISCARD is introduced for block devices in Linux 6.12.
            // IORING_OP_URING_CMD probing alone is not enough because older kernels can
            // support uring commands for other file operations.
            if !Self::host_kernel_at_least(Self::MIN_BLOCK_URING_DISCARD_KERNEL)
                .map_err(AsyncIoError::IO)?
            {
                return Err(AsyncIoError::DiscardUnsupported);
            }
            Ok(Some(AsyncDiscardOp::BlockUringCmd))
        } else {
            Ok(Some(AsyncDiscardOp::Fallocate))
        }
    }

    fn host_kernel_at_least((major, minor): (u32, u32)) -> Result<bool, std::io::Error> {
        // SAFETY: An all-zeroed value for `libc::utsname` is valid.
        let mut name: libc::utsname = unsafe { std::mem::zeroed() };
        // SAFETY: The passed arg is a valid mutable reference of `libc::utsname`.
        let ret = unsafe { libc::uname(&mut name) };
        if ret != 0 {
            return Err(std::io::Error::last_os_error());
        }

        // SAFETY: The fields of `libc::utsname` are terminated by a null byte.
        let release = unsafe { CStr::from_ptr(name.release.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        Self::kernel_release_at_least(&release, (major, minor)).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid kernel release")
        })
    }

    fn parse_kernel_release(release: &str) -> Option<(u32, u32)> {
        let mut parts = release
            .split(|ch: char| !ch.is_ascii_digit() && ch != '.')
            .next()
            .unwrap_or("")
            .split('.');

        let host_major = parts.next()?.parse::<u32>().ok()?;
        let host_minor = parts.next()?.parse::<u32>().ok()?;

        Some((host_major, host_minor))
    }

    fn kernel_release_at_least(release: &str, (major, minor): (u32, u32)) -> Option<bool> {
        let (host_major, host_minor) = Self::parse_kernel_release(release)?;

        Some(host_major > major || (host_major == major && host_minor >= minor))
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn direct_file(&self) -> Option<&File> {
        self.direct_file.as_ref()
    }

    pub fn completion_evt(&self) -> &EventFd {
        &self.completion_evt
    }

    /// Make the device's completion event fire (to come back to work owed to the ring).
    pub fn signal_completion_event(&self) {
        if let Err(err) = self.completion_evt.write(1) {
            error!("Failed to signal the completion event: {:?}", err);
        }
    }

    /// The `O_DIRECT` alignment when the drive does direct I/O, else `None`.
    pub fn dio(&self) -> Option<DioAlign> {
        self.dio
    }

    /// The descriptor of a data transfer: the O_DIRECT one on a direct-I/O drive (a request it
    /// cannot take as is must go through the bounce path, never through the buffered one).
    fn data_fd(&self, buf: usize, offset: u64, count: u32) -> Result<u32, AsyncIoError> {
        match self.dio {
            None => Ok(0),
            Some(dio) if dio.eligible(buf, offset, count) => Ok(DIRECT_WRITE_FD),
            Some(_) => Err(AsyncIoError::DirectUnaligned),
        }
    }

    /// Whether a request waiting for `wait` can go now (`direct::Wait`).
    pub fn can_proceed(&self, wait: Wait) -> bool {
        match wait {
            Wait::Bounce => self.bounce.is_none(),
            Wait::Span(start, end) => {
                self.bounce.is_none()
                    && !self.ring.pending_user_data().any(|op| match op {
                        WrappedRequest::Guest {
                            writes: Some((s, e)),
                            ..
                        } => *s < end && start < *e,
                        _ => false,
                    })
            }
        }
    }

    /// Whether a write or discard of `[start, end)` must wait: it overlaps the span of the
    /// read-modify-write in progress.
    pub fn rmw_overlaps(&self, start: u64, end: u64) -> bool {
        self.bounce.as_ref().is_some_and(|b| {
            let (s, e) = b.plan.span();
            b.plan.rmw() && s < end && start < e
        })
    }

    /// Whether a bounce is in progress.
    pub fn bounce_in_progress(&self) -> bool {
        self.bounce.is_some()
    }

    /// Whether a read-modify-write is in progress.
    #[cfg(test)]
    pub fn rmw_in_progress(&self) -> bool {
        self.bounce.as_ref().is_some_and(|b| b.plan.rmw())
    }

    #[cfg(test)]
    pub fn bounce_buf_len(&self) -> Option<u64> {
        self.bounce_buf.as_ref().map(BounceBuf::len)
    }

    /// Test hook: pretend the backing has this alignment.
    #[cfg(test)]
    pub fn set_dio_for_test(&mut self, dio: DioAlign) {
        self.dio = Some(dio);
    }

    /// Test hook: the ring refuses the next `pushes` pushes and `submits` submissions.
    #[cfg(test)]
    pub fn fail_ring_for_test(&mut self, pushes: u32, submits: u32) {
        self.ring.fail_pushes = pushes;
        self.ring.fail_submits = submits;
    }

    /// Test hook: the injected ring failures not used yet (pushes, submits).
    #[cfg(test)]
    pub fn ring_failures_left_for_test(&self) -> (u32, u32) {
        (self.ring.fail_pushes, self.ring.fail_submits)
    }

    /// Whether a bounce transfer is owed to the ring.
    #[cfg(test)]
    pub fn bounce_transfer_owed(&self) -> bool {
        self.owed.is_some()
    }

    /// Queue the transfer `plan` is at (for a write, after copying the guest's bytes in).
    fn push_bounce_step(
        ring: &mut IoUring<WrappedRequest>,
        plan: &BouncePlan,
        buf: &mut BounceBuf,
        mem: &GuestMemoryMmap,
    ) -> Result<(), AsyncIoError> {
        let step = plan.step();
        plan.fill(mem, buf.as_mut_slice())
            .map_err(AsyncIoError::Bounce)?;
        // The step lies inside the buffer (BouncePlan chunks are at most its length).
        let addr = buf.as_mut_ptr() as usize + step.buf_off;
        let len = u32::try_from(step.len).expect("a step fits the bounce buffer");
        let op = if step.write {
            Operation::write(
                DIRECT_WRITE_FD,
                addr,
                len,
                step.dev_off,
                WrappedRequest::BounceStep,
            )
        } else {
            Operation::read(
                DIRECT_WRITE_FD,
                addr,
                len,
                step.dev_off,
                WrappedRequest::BounceStep,
            )
        };
        ring.push(op).map_err(|(err, _)| AsyncIoError::IoUring(err))
    }

    /// Start serving a guest request through the bounce buffer: queue its first transfer. The
    /// caller checked `bounce_can_start`; the request completes through `pop`.
    pub fn start_bounce(
        &mut self,
        plan: BouncePlan,
        mem: &GuestMemoryMmap,
        req: PendingRequest,
    ) -> Result<(), RequestError<AsyncIoError>> {
        debug_assert!(self.can_proceed(plan.wait()));
        let dio = self.dio.expect("a bounce implies direct I/O");
        let res = bounce_buf(&mut self.bounce_buf, dio)
            .map_err(AsyncIoError::Bounce)
            .and_then(|buf| Self::push_bounce_step(&mut self.ring, &plan, buf, mem));
        match res {
            Ok(()) => {
                self.bounce = Some(ActiveBounce {
                    plan,
                    req,
                    started: Instant::now(),
                });
                Ok(())
            }
            Err(error) => Err(RequestError { req, error }),
        }
    }

    /// A transfer of the bounce in progress completed with `res`: queue the next one, or
    /// return the guest request's completion.
    fn bounce_step_done(
        &mut self,
        res: Result<u32, std::io::Error>,
        mem: &GuestMemoryMmap,
    ) -> Option<Completion> {
        let Some(bounce) = self.bounce.as_mut() else {
            error!("Direct I/O bounce transfer completed with no bounce in progress");
            return None;
        };
        let buf = self.bounce_buf.as_mut().expect("allocated by start_bounce");
        let step = bounce.plan.step();
        if step.write
            && let Ok(n) = &res
            && *n as usize <= step.len
        {
            bounce.plan.record_write_end(step.dev_off + u64::from(*n));
        }
        let more = match res {
            Ok(n) if n as usize == step.len => bounce
                .plan
                .advance(mem, buf.as_slice())
                .map_err(AsyncIoError::Bounce),
            Ok(n) => Err(AsyncIoError::Bounce(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("short direct transfer: {n} of {} bytes", step.len),
            ))),
            Err(err) => Err(AsyncIoError::IO(err)),
        };
        match more {
            Ok(true) => match self.queue_bounce_step(mem) {
                Ok(()) => None,
                Err(err) => self.finish_bounce(Err(err)),
            },
            Ok(false) => self.finish_bounce(Ok(())),
            Err(err) => self.finish_bounce(Err(err)),
        }
    }

    /// Queue the transfer the bounce in progress is at and submit it at once (the completion
    /// path is the only one driving a bounce). A ring that does not take it now (a full
    /// submission queue, a failed `io_uring_enter`) owes it: `retry_owed` takes it up at the
    /// end of every completion pass, and signals the completion event while it is still owed,
    /// so the device itself comes back to it without waiting for the guest.
    fn queue_bounce_step(&mut self, mem: &GuestMemoryMmap) -> Result<(), AsyncIoError> {
        let (Some(bounce), Some(buf)) = (self.bounce.as_ref(), self.bounce_buf.as_mut()) else {
            return Err(AsyncIoError::Bounce(std::io::Error::other(
                "no bounce in progress",
            )));
        };
        match Self::push_bounce_step(&mut self.ring, &bounce.plan, buf, mem) {
            Ok(()) => {}
            Err(AsyncIoError::IoUring(err)) if err.is_throttling_err() => {
                self.owe(Owed::Push, &err);
                return Ok(());
            }
            Err(err) => return Err(err),
        }
        match self.ring.submit() {
            Ok(_) => self.owed = None,
            Err(err) => self.owe(Owed::Submit, &err),
        }
        Ok(())
    }

    fn owe(&mut self, owed: Owed, err: &IoUringError) {
        if self.owed.is_none() {
            error!(
                "Direct I/O bounce transfer not taken by the ring ({:?}: {:?}); retrying",
                owed, err
            );
        }
        self.owed = Some(owed);
    }

    /// Take up a transfer owed to the ring; returns the bounced request's completion if that
    /// failed for good. While it is still owed the completion event is signalled, so that the
    /// event loop runs this again.
    fn retry_owed(&mut self, mem: &GuestMemoryMmap) -> Option<Completion> {
        match self.owed? {
            Owed::Push => {
                if let Err(err) = self.queue_bounce_step(mem) {
                    self.owed = None;
                    return self.finish_bounce(Err(err));
                }
            }
            Owed::Submit => {
                if self.ring.submit().is_ok() {
                    self.owed = None;
                }
            }
        }
        if self.owed.is_some() {
            self.signal_completion_event();
        } else {
            warn!("Direct I/O bounce transfer taken by the ring after a retry");
        }
        None
    }

    /// End the bounce in progress and return its guest request's completion.
    fn finish_bounce(&mut self, res: Result<(), AsyncIoError>) -> Option<Completion> {
        let ActiveBounce { plan, req, started } = self.bounce.take()?;
        self.owed = None;
        let result = match res {
            Ok(()) => Ok(plan.count()),
            Err(AsyncIoError::IO(err)) | Err(AsyncIoError::Bounce(err)) => Err(err),
            Err(err) => Err(std::io::Error::other(err.to_string())),
        };
        Some(Completion {
            req,
            result,
            progress: plan.completed(),
            bounce: Some((plan.kind(), started.elapsed())),
        })
    }

    pub fn push_read(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
        req: PendingRequest,
    ) -> Result<(), RequestError<AsyncIoError>> {
        let buf = match mem.get_slice(addr, count as usize) {
            Ok(slice) => slice.ptr_guard_mut().as_ptr(),
            Err(err) => {
                return Err(RequestError {
                    req,
                    error: AsyncIoError::GuestMemory(err),
                });
            }
        };

        // A direct-I/O drive reads through the O_DIRECT descriptor too.
        let fd = match self.data_fd(buf as usize, offset, count) {
            Ok(fd) => fd,
            Err(error) => return Err(RequestError { req, error }),
        };
        let wrapped_user_data = WrappedRequest::new_with_dirty_tracking(addr, req);
        self.ring
            .push(Operation::read(
                fd,
                buf as usize,
                count,
                offset,
                wrapped_user_data,
            ))
            .map_err(|(io_uring_error, data)| RequestError {
                req: data.into_req(),
                error: AsyncIoError::IoUring(io_uring_error),
            })
    }

    pub fn push_write(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
        req: PendingRequest,
    ) -> Result<(), RequestError<AsyncIoError>> {
        let buf = match mem.get_slice(addr, count as usize) {
            Ok(slice) => slice.ptr_guard_mut().as_ptr(),
            Err(err) => {
                return Err(RequestError {
                    req,
                    error: AsyncIoError::GuestMemory(err),
                });
            }
        };

        let fd = match self.data_fd(buf as usize, offset, count) {
            Ok(fd) => fd,
            Err(error) => return Err(RequestError { req, error }),
        };
        let wrapped_user_data = WrappedRequest::new_modifying(offset, u64::from(count), req);

        self.ring
            .push(Operation::write(
                fd,
                buf as usize,
                count,
                offset,
                wrapped_user_data,
            ))
            .map_err(|(io_uring_error, data)| RequestError {
                req: data.into_req(),
                error: AsyncIoError::IoUring(io_uring_error),
            })
    }

    pub fn push_flush(&mut self, req: PendingRequest) -> Result<(), RequestError<AsyncIoError>> {
        let wrapped_user_data = WrappedRequest::new(req);

        self.ring
            .push(Operation::fsync(0, wrapped_user_data))
            .map_err(|(io_uring_error, data)| RequestError {
                req: data.into_req(),
                error: AsyncIoError::IoUring(io_uring_error),
            })
    }

    pub fn push_discard(
        &mut self,
        range: (u64, u32),
        req: PendingRequest,
    ) -> Result<(), RequestError<AsyncIoError>> {
        let (offset, len) = (range.0, u64::from(range.1));
        let wrapped_user_data = WrappedRequest::new_modifying(offset, len, req);
        let operation = match self.discard_op {
            Some(AsyncDiscardOp::Fallocate) => Operation::fallocate(
                0,
                Self::FALLOC_FL_KEEP_SIZE | Self::FALLOC_FL_PUNCH_HOLE,
                offset,
                len,
                wrapped_user_data,
            ),
            Some(AsyncDiscardOp::BlockUringCmd) => Operation::block_discard(
                0,
                Self::BLOCK_URING_CMD_DISCARD,
                offset,
                len,
                wrapped_user_data,
            ),
            None => {
                return Err(RequestError {
                    req: wrapped_user_data.into_req(),
                    error: AsyncIoError::NotImplemented,
                });
            }
        };

        self.ring
            .push(operation)
            .map_err(|(io_uring_error, data)| RequestError {
                req: data.into_req(),
                error: AsyncIoError::IoUring(io_uring_error),
            })
    }

    pub fn kick_submission_queue(&mut self) -> Result<(), AsyncIoError> {
        self.ring.submit().map_err(AsyncIoError::IoUring)?;
        // Everything queued is submitted, an owed bounce transfer included.
        if self.owed == Some(Owed::Submit) {
            self.owed = None;
        }
        Ok(())
    }

    pub fn drain(&mut self, discard_cqes: bool) -> Result<(), AsyncIoError> {
        self.ring
            .submit_and_wait_all()
            .map_err(AsyncIoError::IoUring)?;
        if self.owed == Some(Owed::Submit) {
            self.owed = None;
        }

        if discard_cqes {
            // Drain the completion queue so that we may deallocate the user_data fields.
            while self.do_pop()?.is_some() {}
            // Every transfer of a bounce in progress completed and was discarded with it.
            self.bounce = None;
            self.owed = None;
        }

        Ok(())
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

    /// The next completed guest request. A completed transfer of the bounce in progress queues
    /// the next one and is not returned; the bounced request is returned when its last
    /// transfer completed (or one failed).
    pub fn pop(&mut self, mem: &GuestMemoryMmap) -> Result<Option<Completion>, AsyncIoError> {
        while let Some(cqe) = self.do_pop()? {
            let count = cqe.count();
            let result = cqe.result();
            match cqe.user_data() {
                WrappedRequest::Guest { addr, req, .. } => {
                    if let Some(addr) = addr {
                        mem.mark_dirty(addr, count as usize)
                    }
                    return Ok(Some(Completion {
                        req,
                        result,
                        progress: 0,
                        bounce: None,
                    }));
                }
                WrappedRequest::BounceStep => {
                    if let Some(done) = self.bounce_step_done(result, mem) {
                        return Ok(Some(done));
                    }
                }
            }
        }
        Ok(self.retry_owed(mem))
    }
}

impl Drop for AsyncFileEngine {
    fn drop(&mut self) {
        // A bounce transfer can still be in flight here only if draining failed: the kernel may
        // write into the buffer after the ring is closed, so the buffer is leaked, not freed.
        if self.bounce.is_some()
            && let Some(buf) = self.bounce_buf.take()
        {
            std::mem::forget(buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use vmm_sys_util::tempfile::TempFile;

    use super::*;

    #[test]
    fn test_kernel_release_at_least() {
        assert_eq!(
            AsyncFileEngine::kernel_release_at_least("6.11.0-1018-aws", (6, 12)),
            Some(false)
        );
        assert_eq!(
            AsyncFileEngine::kernel_release_at_least("6.12.0-1020-aws", (6, 12)),
            Some(true)
        );
        assert_eq!(
            AsyncFileEngine::kernel_release_at_least("6.17.0-29-generic", (6, 12)),
            Some(true)
        );
        assert_eq!(
            AsyncFileEngine::kernel_release_at_least("7.0.2-6-pve", (6, 12)),
            Some(true)
        );
        assert_eq!(
            AsyncFileEngine::kernel_release_at_least("not-a-kernel", (6, 12)),
            None
        );
    }

    #[test]
    fn test_discard_regular_file_uses_fallocate() {
        let file = TempFile::new().unwrap().into_file();
        let engine = AsyncFileEngine::from_file(file, None, true).unwrap();

        assert_eq!(engine.discard_op, Some(AsyncDiscardOp::Fallocate));
    }
}
