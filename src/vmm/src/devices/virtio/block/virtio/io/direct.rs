// Copyright 2026 VPSnet. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Direct I/O for reads and writes on writable `direct_write` drives.
//!
//! A drive with `direct_write` keeps the host page cache out of its data path entirely: every
//! read and every write goes through the `O_DIRECT` descriptor. Mixing a buffered descriptor
//! with asynchronous direct writes is not coherent on Linux block devices: the asynchronous
//! direct-write completion does not invalidate the page cache, so a page that a buffered read
//! (or its read-ahead) brought in while the write was in flight stays stale for later reads.
//!
//! A guest request that does not meet the backing's `O_DIRECT` alignment (buffer address, file
//! offset or length) is a bounce: it is served on the `O_DIRECT` descriptor through the drive's
//! one bounce buffer ([`BOUNCE_BUF_LEN`], allocated at the first bounce and reused), one chunk
//! of the buffer at a time ([`BouncePlan`]). Host memory per drive is therefore bounded by the
//! buffer, whatever the size of the guest request.
//!
//! A write that covers only part of a backing block is a read-modify-write: the first and the
//! last backing block of the request are read, patched with the guest's bytes and written back
//! with the rest of the request. Its write-back carries the bytes it read around the guest's
//! range, so no other write may land on its span (the whole blocks it covers) between its read
//! and its write: it starts only when no write or discard overlapping its span is in flight
//! ([`Wait::Span`]), and until it completes a write or discard overlapping its span is not
//! submitted (it waits at the head of the queue, `VirtioBlock::bounce_hold`). Reads, and
//! writes elsewhere, go on meanwhile: a read of the span sees the blocks before or after the
//! write-back, which differ only in the guest's range of a request not yet completed. Any
//! other bounce (a read, or a whole-block write from an unaligned guest buffer) moves whole
//! blocks exactly as an aligned request would, so it runs concurrently with aligned requests.
//! Only one bounce is in progress at a time, because there is one buffer ([`Wait::Bounce`]).
//! The `Async` engine performs every transfer of a bounce through io_uring, so the VMM thread
//! never waits for the device; the `Sync` engine performs them in place, as it does every
//! request (nothing is ever in flight on a `Sync` drive).
//!
//! A regular file whose size is not a multiple of [`FILE_ALIGN`] cannot be served exactly in
//! direct mode (the blocks that cover its last sectors run past the end of the file), so such
//! a drive opens no direct descriptor and runs as `direct_write: false` (one buffered
//! descriptor for everything).

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::io::AsRawFd;

use vm_memory::Bytes;

use crate::vstate::memory::{GuestAddress, GuestMemoryMmap};

/// `O_DIRECT` alignment of a backing file or device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DioAlign {
    /// Required alignment of the memory buffer.
    pub mem: u64,
    /// Required alignment of the file offset and of the length.
    pub offset: u64,
}

/// What a request that waits at the head of the queue for the bounce path waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// The bounce in progress to complete: a bounce waiting for the one buffer, or a write or
    /// discard overlapping the span of the read-modify-write in progress.
    Bounce,
    /// A read-modify-write of `[start, end)`: no bounce in progress and no write or discard in
    /// flight overlapping the span.
    Span(u64, u64),
}

/// How a request that did not meet the alignment was served.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bounce {
    /// Read through the bounce buffer.
    Read,
    /// Whole-block write whose guest buffer was not aligned, copied into the bounce buffer.
    Write,
    /// Write of part of a backing block: read-modify-write of the first and last block.
    ReadModifyWrite,
}

/// Alignment used for regular files. There is no `statx(STATX_DIOALIGN)` here (the VMM's
/// seccomp filter allows `fstat` only, and Rust's standard library on musl uses `fstat`), so a
/// fixed 4096-byte bound is used for regular files. Backings requiring a larger direct-I/O
/// alignment are outside the scope of this implementation.
pub const FILE_ALIGN: u64 = 4096;

/// Size of a drive's bounce buffer: the most host memory one bounce uses, whatever the size of
/// the guest request.
pub const BOUNCE_BUF_LEN: u64 = 1 << 20;

/// Chunk length of a bounce on a backing aligned as `dio`, and length of the bounce buffer:
/// [`BOUNCE_BUF_LEN`] (a multiple of every power-of-two block size up to it), or one block of
/// a larger block size.
pub fn chunk_len(dio: DioAlign) -> u64 {
    BOUNCE_BUF_LEN.max(dio.offset)
}

impl DioAlign {
    /// The alignment of `file`: its logical block size for a block device (read from the device
    /// with `BLKSSZGET`), else [`FILE_ALIGN`]. `None` when direct I/O cannot serve every sector
    /// of `file` exactly: a regular file whose size is not a multiple of [`FILE_ALIGN`] (or
    /// whose size cannot be read). The buffer alignment is never below the offset alignment.
    pub fn probe(file: &File) -> Option<DioAlign> {
        let meta = file.metadata().ok()?;
        let mut align = FILE_ALIGN;
        if meta.file_type().is_block_device() {
            let mut size: libc::c_int = 0;
            // SAFETY: BLKSSZGET writes one c_int through the valid pointer.
            let ret = unsafe { libc::ioctl(file.as_raw_fd(), libc::BLKSSZGET, &mut size) };
            if let Ok(size) = u64::try_from(size)
                && ret == 0
                && size >= 512
                && size.is_power_of_two()
            {
                align = size;
            }
        } else if !meta.len().is_multiple_of(FILE_ALIGN) {
            return None;
        }
        Some(DioAlign {
            mem: align,
            offset: align,
        })
    }

    /// Whether a transfer of `count` bytes at `offset` from/to `buf` can use `O_DIRECT` as is.
    pub fn eligible(&self, buf: usize, offset: u64, count: u32) -> bool {
        count != 0
            && (buf as u64).is_multiple_of(self.mem)
            && offset.is_multiple_of(self.offset)
            && u64::from(count).is_multiple_of(self.offset)
    }
}

/// The bounce buffer of a drive: allocated once (zero-initialised), reused by every bounce.
pub struct BounceBuf {
    ptr: *mut u8,
    layout: Layout,
}

// SAFETY: the buffer is an allocation owned exclusively by this value; nothing else aliases it.
unsafe impl Send for BounceBuf {}

impl std::fmt::Debug for BounceBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "BounceBuf {{ len: {}, align: {} }}",
            self.layout.size(),
            self.layout.align()
        )
    }
}

impl BounceBuf {
    /// A buffer for `dio`: [`chunk_len`] bytes, aligned for `O_DIRECT`.
    pub fn new(dio: DioAlign) -> std::io::Result<BounceBuf> {
        let align = usize::try_from(dio.mem.max(dio.offset))
            .map_err(|_| std::io::Error::other("bounce buffer alignment"))?;
        let len = usize::try_from(chunk_len(dio))
            .map_err(|_| std::io::Error::other("bounce buffer length"))?;
        let layout = Layout::from_size_align(len, align)
            .map_err(|_| std::io::Error::other("bounce buffer layout"))?;
        // SAFETY: the layout has a non-zero size.
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            return Err(std::io::Error::other("bounce buffer allocation"));
        }
        Ok(BounceBuf { ptr, layout })
    }

    /// Whether this buffer can serve bounces on a backing aligned as `dio` (alignment, and
    /// room for a chunk).
    pub fn fits(&self, dio: DioAlign) -> bool {
        self.layout.align() as u64 >= dio.mem.max(dio.offset)
            && self.layout.size() as u64 >= chunk_len(dio)
    }

    /// Length in bytes.
    #[cfg(test)]
    pub fn len(&self) -> u64 {
        self.layout.size() as u64
    }

    /// Address of the first byte (for an io_uring transfer).
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr points to layout.size() initialised bytes owned by self.
        unsafe { std::slice::from_raw_parts(self.ptr, self.layout.size()) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: ptr points to layout.size() initialised bytes owned exclusively by self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }
}

impl Drop for BounceBuf {
    fn drop(&mut self) {
        // SAFETY: ptr was allocated with this layout.
        unsafe { dealloc(self.ptr, self.layout) }
    }
}

/// The drive's bounce buffer for `dio`, allocated at the first use (or again after the backing
/// changed to a larger alignment).
pub fn bounce_buf(slot: &mut Option<BounceBuf>, dio: DioAlign) -> std::io::Result<&mut BounceBuf> {
    if slot.as_ref().is_some_and(|buf| !buf.fits(dio)) {
        *slot = None;
    }
    if slot.is_none() {
        *slot = Some(BounceBuf::new(dio)?);
    }
    Ok(slot.as_mut().expect("allocated above"))
}

/// One transfer of a bounce on the `O_DIRECT` descriptor: `len` bytes between
/// `buf[buf_off..]` and the backing at `dev_off`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    pub write: bool,
    pub dev_off: u64,
    pub buf_off: usize,
    pub len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Write: read the first block of the chunk (the guest covers only its end).
    ReadHead,
    /// Write: read the last block of the chunk (the guest covers only its start).
    ReadTail,
    /// Write the chunk (guest bytes copied over what was read).
    Write,
    /// Read the chunk (then copy the guest's part out).
    Read,
}

/// A guest request served through the bounce buffer, one chunk at a time. A chunk is a span of
/// whole backing blocks of at most the buffer's length; only the first chunk can start, and
/// only the last can end, inside a block. A read reads each chunk and copies the guest's bytes
/// out. A write reads the partial first and last block (never the whole span), copies the
/// guest's bytes in and writes the chunk.
#[derive(Debug)]
pub struct BouncePlan {
    write: bool,
    /// The guest request: `[offset, end)` on the backing, from/to guest memory at `addr`.
    offset: u64,
    end: u64,
    addr: GuestAddress,
    /// Backing block size and the end of the last block the request touches.
    blk: u64,
    last: u64,
    chunk_len: u64,
    /// The chunk in progress starts at `pos`; `phase` is its transfer in progress.
    pos: u64,
    phase: Phase,
    /// Guest bytes copied to memory (read) or reported written by the backing.
    completed: u32,
}

impl BouncePlan {
    /// The plan of `count` bytes at `offset` from/to guest `addr`, on a backing aligned as
    /// `dio`, with chunks of `chunk_len` bytes (a multiple of the block size; at most the
    /// bounce buffer's length: `chunk_len(dio)` in production).
    pub fn new(
        write: bool,
        offset: u64,
        count: u32,
        addr: GuestAddress,
        dio: DioAlign,
        chunk_len: u64,
    ) -> std::io::Result<BouncePlan> {
        let blk = dio.offset;
        if count == 0 || blk == 0 || chunk_len < blk || !chunk_len.is_multiple_of(blk) {
            return Err(std::io::Error::other("bounce plan parameters"));
        }
        let end = offset
            .checked_add(u64::from(count))
            .ok_or_else(|| std::io::Error::other("request end overflow"))?;
        let last = end
            .div_ceil(blk)
            .checked_mul(blk)
            .ok_or_else(|| std::io::Error::other("request end overflow"))?;
        let mut plan = BouncePlan {
            write,
            offset,
            end,
            addr,
            blk,
            last,
            chunk_len,
            pos: offset - offset % blk,
            phase: Phase::Read,
            completed: 0,
        };
        plan.phase = plan.first_phase();
        Ok(plan)
    }

    /// How the request is served.
    pub fn kind(&self) -> Bounce {
        if !self.write {
            Bounce::Read
        } else if self.offset.is_multiple_of(self.blk) && self.end.is_multiple_of(self.blk) {
            Bounce::Write
        } else {
            Bounce::ReadModifyWrite
        }
    }

    /// Whether this is a read-modify-write (it must not race a write on its span).
    pub fn rmw(&self) -> bool {
        self.kind() == Bounce::ReadModifyWrite
    }

    /// The whole backing blocks the request covers, `[start, end)`.
    pub fn span(&self) -> (u64, u64) {
        (self.offset - self.offset % self.blk, self.last)
    }

    /// What the bounce waits for before it can start.
    pub fn wait(&self) -> Wait {
        if self.rmw() {
            let (start, end) = self.span();
            Wait::Span(start, end)
        } else {
            Wait::Bounce
        }
    }

    /// Length of the guest request.
    pub fn count(&self) -> u32 {
        u32::try_from(self.end - self.offset).expect("built from a u32 count")
    }

    /// Number of guest data bytes confirmed before a terminal error.
    pub fn completed(&self) -> u32 {
        self.completed
    }

    /// Record a successful prefix of a backing write. Preserved bytes before and after the
    /// guest's part of a read-modify-write are excluded.
    pub fn record_write_end(&mut self, end: u64) {
        self.completed = u32::try_from(end.min(self.end).saturating_sub(self.offset))
            .expect("bounded by the guest request length");
    }

    fn chunk_end(&self) -> u64 {
        self.last.min(self.pos + self.chunk_len)
    }

    /// The guest's bytes of the chunk in progress, as backing offsets `[g0, g1)`.
    fn guest_span(&self) -> (u64, u64) {
        (self.offset.max(self.pos), self.end.min(self.chunk_end()))
    }

    fn first_phase(&self) -> Phase {
        if !self.write {
            return Phase::Read;
        }
        let (g0, g1) = self.guest_span();
        if g0 > self.pos {
            Phase::ReadHead
        } else if g1 < self.chunk_end() {
            Phase::ReadTail
        } else {
            Phase::Write
        }
    }

    /// The transfer to issue now.
    pub fn step(&self) -> Step {
        let ce = self.chunk_end();
        let blk = usize::try_from(self.blk).expect("block size fits usize");
        let span = usize::try_from(ce - self.pos).expect("chunk fits the buffer");
        match self.phase {
            Phase::ReadHead => Step {
                write: false,
                dev_off: self.pos,
                buf_off: 0,
                len: blk,
            },
            Phase::ReadTail => Step {
                write: false,
                dev_off: ce - self.blk,
                buf_off: span - blk,
                len: blk,
            },
            Phase::Write | Phase::Read => Step {
                write: self.phase == Phase::Write,
                dev_off: self.pos,
                buf_off: 0,
                len: span,
            },
        }
    }

    /// The guest's bytes of the chunk in progress: their range in the buffer and their guest
    /// address.
    fn guest_part(&self) -> (std::ops::Range<usize>, GuestAddress) {
        let (g0, g1) = self.guest_span();
        let b0 = usize::try_from(g0 - self.pos).expect("chunk fits the buffer");
        let b1 = usize::try_from(g1 - self.pos).expect("chunk fits the buffer");
        (b0..b1, GuestAddress(self.addr.0 + (g0 - self.offset)))
    }

    /// Before a write transfer: copy the guest's bytes of the chunk into `buf`, over the blocks
    /// read for a read-modify-write.
    pub fn fill(&self, mem: &GuestMemoryMmap, buf: &mut [u8]) -> std::io::Result<()> {
        if self.phase != Phase::Write {
            return Ok(());
        }
        let (range, addr) = self.guest_part();
        mem.read_slice(&mut buf[range], addr)
            .map_err(|e| std::io::Error::other(format!("guest memory: {e}")))
    }

    /// The transfer of `step()` moved all its bytes: copy what a read brought to the guest and
    /// move on. Returns false when the request is complete.
    pub fn advance(&mut self, mem: &GuestMemoryMmap, buf: &[u8]) -> std::io::Result<bool> {
        match self.phase {
            Phase::ReadHead => {
                let (_, g1) = self.guest_span();
                let ce = self.chunk_end();
                // The tail block needs its own read unless it is the head block.
                self.phase = if g1 < ce && ce - self.blk != self.pos {
                    Phase::ReadTail
                } else {
                    Phase::Write
                };
                Ok(true)
            }
            Phase::ReadTail => {
                self.phase = Phase::Write;
                Ok(true)
            }
            Phase::Write | Phase::Read => {
                if self.phase == Phase::Read {
                    let (range, addr) = self.guest_part();
                    let src = &buf[range];
                    let copied = mem
                        .write(src, addr)
                        .map_err(|e| std::io::Error::other(format!("guest memory: {e}")))?;
                    self.completed +=
                        u32::try_from(copied).expect("bounded by the guest request length");
                    if copied != src.len() {
                        return Err(std::io::Error::other("short copy to guest memory"));
                    }
                }
                let ce = self.chunk_end();
                if ce >= self.last {
                    return Ok(false);
                }
                self.pos = ce;
                self.phase = self.first_phase();
                Ok(true)
            }
        }
    }
}

fn read_full_at(mut file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buf)
}

/// Run `plan` in place on the `O_DIRECT` descriptor `direct` (the `Sync` engine: nothing else
/// is ever in flight on its drive).
pub fn run_sync(
    plan: &mut BouncePlan,
    mut direct: &File,
    mem: &GuestMemoryMmap,
    buf: &mut BounceBuf,
) -> std::io::Result<()> {
    loop {
        let step = plan.step();
        let range = step.buf_off..step.buf_off + step.len;
        if step.write {
            plan.fill(mem, buf.as_mut_slice())?;
            direct.seek(SeekFrom::Start(step.dev_off))?;
            let mut remaining = &buf.as_slice()[range];
            let mut offset = step.dev_off;
            while !remaining.is_empty() {
                match direct.write(remaining) {
                    Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                    Ok(n) => {
                        offset += n as u64;
                        plan.record_write_end(offset);
                        remaining = &remaining[n..];
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
        } else {
            read_full_at(direct, &mut buf.as_mut_slice()[range], step.dev_off)?;
        }
        if !plan.advance(mem, buf.as_slice())? {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};

    use vmm_sys_util::tempfile::TempFile;

    use super::*;
    use crate::devices::virtio::test_utils::default_mem;
    use crate::vmm_config::machine_config::HugePageConfig;
    use crate::vstate::memory;
    use crate::vstate::memory::GuestRegionMmapExt;

    const D4K: DioAlign = DioAlign {
        mem: 4096,
        offset: 4096,
    };

    #[test]
    fn test_eligible() {
        assert!(D4K.eligible(0x1000, 0x2000, 0x3000));
        assert!(!D4K.eligible(0x1200, 0x2000, 0x3000));
        assert!(!D4K.eligible(0x1000, 0x2200, 0x3000));
        assert!(!D4K.eligible(0x1000, 0x2000, 0x3200));
        assert!(!D4K.eligible(0x1000, 0x2000, 0));
        let d = DioAlign {
            mem: 512,
            offset: 512,
        };
        assert!(d.eligible(0x1200, 0x2200, 0x200));
    }

    // Regular files: 4096, unless the size is not a multiple of it (the last blocks would run
    // past the end of the file): then no direct I/O at all.
    #[test]
    fn test_probe_regular_file() {
        let f = TempFile::new().unwrap();
        assert_eq!(DioAlign::probe(f.as_file()), Some(D4K));
        f.as_file().set_len(3 * 4096).unwrap();
        assert_eq!(DioAlign::probe(f.as_file()), Some(D4K));
        f.as_file().set_len(3 * 4096 + 512).unwrap();
        assert_eq!(DioAlign::probe(f.as_file()), None);
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

    fn steps(write: bool, offset: u64, count: u32, blk: u64, chunk: u64) -> Vec<Step> {
        let mem = mem_of(4 << 20);
        let dio = DioAlign {
            mem: blk,
            offset: blk,
        };
        let mut plan = BouncePlan::new(write, offset, count, GuestAddress(0), dio, chunk).unwrap();
        let buf = vec![0u8; usize::try_from(chunk).unwrap()];
        let mut out = vec![plan.step()];
        // advance() copies into the guest only for reads; mem covers the requests.
        while plan.advance(&mem, &buf).unwrap() {
            out.push(plan.step());
        }
        out
    }

    fn r(dev_off: u64, buf_off: usize, len: usize) -> Step {
        Step {
            write: false,
            dev_off,
            buf_off,
            len,
        }
    }

    fn w(dev_off: u64, len: usize) -> Step {
        Step {
            write: true,
            dev_off,
            buf_off: 0,
            len,
        }
    }

    // The transfers of every shape: a read-modify-write reads only its partial first and last
    // block, never the whole span, and every transfer fits the buffer.
    #[test]
    fn test_plan_steps() {
        const M: u64 = 1 << 20;
        // One sector inside a block: read that block, write it.
        assert_eq!(
            steps(true, 4096 + 1536, 512, 4096, M),
            vec![r(4096, 0, 4096), w(4096, 4096)]
        );
        // Partial head and partial tail in two blocks: read both, write both.
        assert_eq!(
            steps(true, 3000, 5000, 4096, M),
            vec![r(0, 0, 4096), r(4096, 4096, 4096), w(0, 8192)]
        );
        // Aligned start, partial tail.
        assert_eq!(
            steps(true, 8192, 4608, 4096, M),
            vec![r(12288, 4096, 4096), w(8192, 8192)]
        );
        // Whole blocks (an unaligned guest buffer): no read.
        assert_eq!(steps(true, 8192, 8192, 4096, M), vec![w(8192, 8192)]);
        // 512-byte blocks: a 512-aligned span with a misaligned buffer is a plain write.
        assert_eq!(steps(true, 512, 1024, 512, M), vec![w(512, 1024)]);
        // A read: whole covering blocks.
        assert_eq!(steps(false, 777, 9000, 4096, M), vec![r(0, 0, 12288)]);
        // A large write spanning chunks: head block read in the first chunk, tail block read
        // in the last chunk, the middle written chunk by chunk.
        let s = steps(true, 512, 3 * 1048576, 4096, M);
        assert_eq!(
            s,
            vec![
                r(0, 0, 4096),
                w(0, 1048576),
                w(1048576, 1048576),
                w(2 * 1048576, 1048576),
                r(3 * 1048576, 0, 4096),
                w(3 * 1048576, 4096),
            ]
        );
        assert_eq!(s.iter().filter(|s| !s.write).count(), 2);
        // A large read, chunk by chunk.
        assert_eq!(
            steps(false, 512, 2 * 1048576, 4096, M),
            vec![
                r(0, 0, 1048576),
                r(1048576, 0, 1048576),
                r(2 * 1048576, 0, 4096)
            ]
        );
        // Small chunks: the head and tail reads of a span of two chunks.
        assert_eq!(
            steps(true, 1000, 10000, 4096, 8192),
            vec![r(0, 0, 4096), w(0, 8192), r(8192, 0, 4096), w(8192, 4096)]
        );
    }

    #[test]
    fn test_plan_kind() {
        let p = |write, offset, count| {
            BouncePlan::new(write, offset, count, GuestAddress(0), D4K, BOUNCE_BUF_LEN).unwrap()
        };
        assert_eq!(p(false, 512, 512).kind(), Bounce::Read);
        assert_eq!(p(true, 4096, 4096).kind(), Bounce::Write);
        assert!(!p(true, 4096, 4096).rmw());
        assert_eq!(p(true, 512, 4096).kind(), Bounce::ReadModifyWrite);
        assert!(p(true, 4096, 512).rmw());
        assert_eq!(p(true, 4096, 512).count(), 512);
        BouncePlan::new(true, 0, 0, GuestAddress(0), D4K, BOUNCE_BUF_LEN).unwrap_err();
        BouncePlan::new(true, u64::MAX - 10, 512, GuestAddress(0), D4K, 4096).unwrap_err();
    }

    #[test]
    fn test_bounce_buf() {
        let mut slot = None;
        let buf = bounce_buf(&mut slot, D4K).unwrap();
        assert_eq!(buf.len(), BOUNCE_BUF_LEN);
        assert!((buf.as_mut_ptr() as usize).is_multiple_of(4096));
        let ptr = buf.as_mut_ptr();
        // Reused for a smaller alignment; replaced for a larger one.
        let d512 = DioAlign {
            mem: 512,
            offset: 512,
        };
        assert_eq!(bounce_buf(&mut slot, d512).unwrap().as_mut_ptr(), ptr);
        let d64k = DioAlign {
            mem: 65536,
            offset: 65536,
        };
        let buf = bounce_buf(&mut slot, d64k).unwrap();
        assert!((buf.as_mut_ptr() as usize).is_multiple_of(65536));
        assert_eq!(buf.len(), BOUNCE_BUF_LEN);
    }

    #[allow(clippy::cast_possible_truncation)]
    // Sub-block reads and writes on a 4096-aligned backing, through the bounce path: the bytes
    // around the guest's range are preserved, the guest's range is exact.
    #[test]
    fn test_bounce_read_modify_write() {
        let tmp = TempFile::new().unwrap();
        let mut f = tmp.into_file();
        let orig: Vec<u8> = (0..16384u32).map(|i| (i % 251) as u8).collect();
        f.write_all(&orig).unwrap();
        let mem = default_mem();
        let mut slot = None;
        let mut run = |write, offset, addr, count| {
            let mut plan = BouncePlan::new(
                write,
                offset,
                count,
                GuestAddress(addr),
                D4K,
                BOUNCE_BUF_LEN,
            )
            .unwrap();
            let kind = plan.kind();
            run_sync(&mut plan, &f, &mem, bounce_buf(&mut slot, D4K).unwrap()).unwrap();
            kind
        };

        // 512-byte write at 4096 + 1536: read-modify-write of one block.
        let data = vec![0xa5u8; 512];
        mem.write_slice(&data, GuestAddress(0x1200)).unwrap();
        assert_eq!(run(true, 4096 + 1536, 0x1200, 512), Bounce::ReadModifyWrite);
        let mut expect = orig.clone();
        expect[4096 + 1536..4096 + 2048].copy_from_slice(&data);

        // Write spanning a block boundary from an unaligned guest buffer.
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 13) as u8).collect();
        mem.write_slice(&data, GuestAddress(0x3003)).unwrap();
        assert_eq!(run(true, 3000, 0x3003, 5000), Bounce::ReadModifyWrite);
        expect[3000..8000].copy_from_slice(&data);

        // Whole-block write from an unaligned buffer: no read needed.
        let data = vec![0x3cu8; 4096];
        mem.write_slice(&data, GuestAddress(0x5001)).unwrap();
        assert_eq!(run(true, 8192, 0x5001, 4096), Bounce::Write);
        expect[8192..12288].copy_from_slice(&data);

        // Unaligned read back into guest memory.
        assert_eq!(run(false, 777, 0x7005, 9000), Bounce::Read);
        let mut back = vec![0u8; 9000];
        mem.read_slice(&mut back, GuestAddress(0x7005)).unwrap();
        assert_eq!(back, expect[777..9777]);

        let mut got = vec![0u8; 16384];
        let mut f = f.try_clone().unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.read_exact(&mut got).unwrap();
        assert_eq!(got, expect);
    }

    // A request far larger than the bounce buffer goes through it chunk by chunk: the
    // buffer stays BOUNCE_BUF_LEN, the data is exact across chunk boundaries, and the bytes
    // around a misaligned write are preserved.
    #[allow(clippy::cast_possible_truncation)]
    #[test]
    fn test_bounce_large_request_bounded_buffer() {
        const LEN: usize = 9 * 1048576 + 1536;
        let mem = mem_of(2 * LEN + 0x10000);
        let tmp = TempFile::new().unwrap();
        let mut f = tmp.into_file();
        let disk_len = 12 * 1048576;
        let orig: Vec<u8> = (0..disk_len as u32).map(|i| (i % 253) as u8).collect();
        f.write_all(&orig).unwrap();
        let mut slot = None;

        let data: Vec<u8> = (0..LEN as u32).map(|i| (i % 241) as u8 ^ 0x5a).collect();
        mem.write_slice(&data, GuestAddress(0x200)).unwrap();
        let offset = 1048576 + 512;
        let mut plan = BouncePlan::new(
            true,
            offset,
            LEN as u32,
            GuestAddress(0x200),
            D4K,
            BOUNCE_BUF_LEN,
        )
        .unwrap();
        run_sync(&mut plan, &f, &mem, bounce_buf(&mut slot, D4K).unwrap()).unwrap();
        assert_eq!(slot.as_ref().unwrap().len(), BOUNCE_BUF_LEN);
        let mut expect = orig.clone();
        expect[offset as usize..offset as usize + LEN].copy_from_slice(&data);
        let mut got = vec![0u8; disk_len];
        f.seek(SeekFrom::Start(0)).unwrap();
        f.read_exact(&mut got).unwrap();
        assert!(got == expect, "misaligned large write");

        let mut plan = BouncePlan::new(
            false,
            offset - 1024,
            LEN as u32,
            GuestAddress(LEN as u64 + 0x400),
            D4K,
            BOUNCE_BUF_LEN,
        )
        .unwrap();
        run_sync(&mut plan, &f, &mem, bounce_buf(&mut slot, D4K).unwrap()).unwrap();
        assert_eq!(slot.as_ref().unwrap().len(), BOUNCE_BUF_LEN);
        let mut back = vec![0u8; LEN];
        mem.read_slice(&mut back, GuestAddress(LEN as u64 + 0x400))
            .unwrap();
        assert!(
            back == expect[offset as usize - 1024..offset as usize - 1024 + LEN],
            "misaligned large read"
        );
    }

    // A transfer that runs past the end of the backing fails (no silent short read).
    #[test]
    fn test_bounce_short_read_is_an_error() {
        let tmp = TempFile::new().unwrap();
        let f = tmp.into_file();
        f.set_len(4096 + 512).unwrap();
        let mem = default_mem();
        let mut slot = None;
        let mut plan =
            BouncePlan::new(false, 4096, 512, GuestAddress(0), D4K, BOUNCE_BUF_LEN).unwrap();
        let err = run_sync(&mut plan, &f, &mem, bounce_buf(&mut slot, D4K).unwrap()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }
    // Run explicitly on a filesystem supporting O_DIRECT. All reads and writes below use
    // the same direct descriptor. The ordinary descriptor only creates and sizes the fixture.
    #[test]
    #[ignore = "requires a temporary filesystem supporting O_DIRECT"]
    fn test_direct_descriptor_bounce_round_trip() {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt;

        let tmp = TempFile::new().unwrap();
        tmp.as_file().set_len(8192).unwrap();
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_DIRECT)
            .open(tmp.as_path())
            .unwrap();
        let dio = DioAlign::probe(&f).unwrap();
        let mem = default_mem();
        let mut buf = BounceBuf::new(dio).unwrap();

        let mut expect = vec![0x5a; 8192];
        mem.write_slice(&expect, GuestAddress(1)).unwrap();
        let mut init = BouncePlan::new(true, 0, 8192, GuestAddress(1), dio, buf.len()).unwrap();
        run_sync(&mut init, &f, &mem, &mut buf).unwrap();

        let payload = [0xa5; 1024];
        mem.write_slice(&payload, GuestAddress(3)).unwrap();
        let mut write = BouncePlan::new(true, 3584, 1024, GuestAddress(3), dio, buf.len()).unwrap();
        assert_eq!(write.kind(), Bounce::ReadModifyWrite);
        run_sync(&mut write, &f, &mem, &mut buf).unwrap();
        expect[3584..4608].copy_from_slice(&payload);

        let mut read = BouncePlan::new(false, 0, 8192, GuestAddress(5), dio, buf.len()).unwrap();
        run_sync(&mut read, &f, &mem, &mut buf).unwrap();
        let mut actual = vec![0; 8192];
        mem.read_slice(&mut actual, GuestAddress(5)).unwrap();
        assert_eq!(actual, expect);
    }
}
