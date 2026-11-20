// Copyright 2026 VPSnet. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The round stream of migration protocol 1.
//!
//! One round on the wire, all integers little-endian:
//!
//! ```text
//! header   64 B   magic "FCPCRND1" | version u32 = 1 | flags u32 (1 FIRST, 2 FINAL)
//!                 | session u64 | seq u64 | page_size u32 = 4096 | region_count u32
//!                 | mem_bytes u64 | 16 B zero
//! regions  16 B x region_count (FIRST round only): guest physical base u64 | size u64
//! chunk*   count u32 (1..=256) | data_pages u32
//!          | count x tag u64: guest physical address of the page | 1 if the page is zero
//!          | data_pages x 4096 B: the non-zero pages, in tag order
//! end      count u32 = 0 | data_pages u32 = 0
//! trailer  96 B   records u64 | data_pages u64 | zero_pages u64 | state_digest [32]
//!                 | digest [32] | magic "FCPCEND1"
//! ```
//!
//! `digest` is the SHA-256 of every byte of the round before it (header to `state_digest`).
//! `state_digest` is the SHA-256 of the microVM state file written with the FINAL round, zero
//! otherwise. A FIRST round carries only the non-zero pages (the target's fresh memory is zero);
//! later rounds carry every page of their dirty set, a zero page as a tag without data.
//!
//! The reader reads exactly one round and never past it, so several rounds can follow each other
//! on one connection.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

use aws_lc_rs::digest::{Context, SHA256};

use super::PAGE_SIZE;

/// Magic of a round header.
pub const ROUND_MAGIC: [u8; 8] = *b"FCPCRND1";
/// Magic closing a round trailer.
pub const END_MAGIC: [u8; 8] = *b"FCPCEND1";
/// The stream format version.
pub const STREAM_VERSION: u32 = 1;
/// The round is the first of a session: it carries the memory layout and every non-zero page.
pub const FLAG_FIRST: u32 = 1;
/// The round is the last of a session: it was written in the pause with the microVM state.
pub const FLAG_FINAL: u32 = 2;
/// Tag flag: the page is zero and has no data.
pub const TAG_ZERO: u64 = 1;
/// Records per chunk at most.
pub const MAX_CHUNK_RECORDS: usize = 256;
/// Memory regions in a layout at most.
pub const MAX_REGIONS: usize = 64;
/// Length of a round header.
pub const HEADER_LEN: usize = 64;
/// Length of a region descriptor.
pub const REGION_LEN: usize = 16;
/// Length of a chunk header.
pub const CHUNK_HEADER_LEN: usize = 8;
/// Length of a round trailer.
pub const TRAILER_LEN: usize = 96;
/// Length of a SHA-256 digest.
pub const DIGEST_LEN: usize = 32;

const KNOWN_FLAGS: u32 = FLAG_FIRST | FLAG_FINAL;
const TAG_RESERVED: u64 = (PAGE_SIZE as u64 - 1) & !TAG_ZERO;

/// Errors of the round stream.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum StreamError {
    /// I/O error on the migration descriptor: {0}
    Io(#[from] io::Error),
    /// The round is cut: the stream ended inside it
    Cut,
    /// Not a round header (bad magic)
    BadMagic,
    /// Unsupported stream version {0}
    BadVersion(u32),
    /// Unsupported page size {0}
    BadPageSize(u32),
    /// Unknown round flags {0:#x}
    BadFlags(u32),
    /// Bad memory layout in the round header: {0}
    BadLayout(String),
    /// Reserved header bytes are not zero
    BadReserved,
    /// Chunk of {0} records (at most 256)
    BadChunk(u32),
    /// Chunk declares {0} data pages but its tags carry {1}
    BadChunkData(u32, u32),
    /// Bad page tag {0:#x}
    BadTag(u64),
    /// Page {0:#x} is outside guest memory
    OutsideMemory(u64),
    /// The trailer counts do not match the round
    BadTotals,
    /// The round digest does not match: the round was altered or reordered
    BadDigest,
    /// Not a round trailer (bad end magic)
    BadEndMagic,
}

/// The header of a round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundHeader {
    /// `FLAG_FIRST` and/or `FLAG_FINAL`.
    pub flags: u32,
    /// Random identifier of the migration session, the same in every round of it.
    pub session: u64,
    /// Round number: 0 for the FIRST round, then +1 per round.
    pub seq: u64,
    /// Total guest memory size in bytes.
    pub mem_bytes: u64,
    /// Guest memory layout `(guest physical base, size)`, FIRST round only.
    pub regions: Vec<(u64, u64)>,
}

impl RoundHeader {
    /// Whether this is a FIRST round.
    pub fn is_first(&self) -> bool {
        self.flags & FLAG_FIRST != 0
    }

    /// Whether this is a FINAL round.
    pub fn is_final(&self) -> bool {
        self.flags & FLAG_FINAL != 0
    }

    /// Encodes the header and its region descriptors.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_LEN + self.regions.len() * REGION_LEN);
        buf.extend_from_slice(&ROUND_MAGIC);
        buf.extend_from_slice(&STREAM_VERSION.to_le_bytes());
        buf.extend_from_slice(&self.flags.to_le_bytes());
        buf.extend_from_slice(&self.session.to_le_bytes());
        buf.extend_from_slice(&self.seq.to_le_bytes());
        buf.extend_from_slice(&u32::try_from(PAGE_SIZE).unwrap().to_le_bytes());
        buf.extend_from_slice(&u32::try_from(self.regions.len()).unwrap().to_le_bytes());
        buf.extend_from_slice(&self.mem_bytes.to_le_bytes());
        buf.extend_from_slice(&[0u8; 16]);
        for (base, size) in &self.regions {
            buf.extend_from_slice(&base.to_le_bytes());
            buf.extend_from_slice(&size.to_le_bytes());
        }
        buf
    }

    /// Checks the layout carried by a FIRST round: non-empty, page aligned, ascending,
    /// non-overlapping, sizes adding up to `mem_bytes`.
    pub fn check_layout(&self) -> Result<(), StreamError> {
        let bad = |msg: &str| Err(StreamError::BadLayout(msg.to_string()));
        if self.is_first() != !self.regions.is_empty() {
            return bad("a FIRST round carries the layout, other rounds none");
        }
        if !self.is_first() {
            return Ok(());
        }
        if self.regions.len() > MAX_REGIONS {
            return bad("too many regions");
        }
        let mut end = 0u64;
        let mut total = 0u64;
        for &(base, size) in &self.regions {
            if size == 0 || base % PAGE_SIZE as u64 != 0 || size % PAGE_SIZE as u64 != 0 {
                return bad("empty or unaligned region");
            }
            if base < end {
                return bad("regions overlap or are not ascending");
            }
            end = match base.checked_add(size) {
                Some(end) => end,
                None => return bad("region beyond the address space"),
            };
            total = total.saturating_add(size);
        }
        if total != self.mem_bytes {
            return bad("region sizes do not add up to the memory size");
        }
        Ok(())
    }
}

/// The counts and digests closing a round.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoundTrailer {
    /// Records (pages) in the round.
    pub records: u64,
    /// Records with data.
    pub data_pages: u64,
    /// Records of zero pages.
    pub zero_pages: u64,
    /// SHA-256 of the microVM state written with a FINAL round; zero otherwise.
    pub state_digest: [u8; DIGEST_LEN],
}

/// Where a round is written.
pub trait StreamWrite {
    /// Writes all of `buf`.
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()>;
}

/// Where a round is read from.
pub trait StreamRead {
    /// Fills `buf` completely; `io::ErrorKind::UnexpectedEof` when the stream ends first.
    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()>;
}

impl StreamWrite for Vec<u8> {
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.extend_from_slice(buf);
        Ok(())
    }
}

impl StreamRead for &[u8] {
    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        io::Read::read_exact(self, buf)
    }
}

/// The migration descriptor passed on the API socket (a pipe or a connected socket), read and
/// written with plain `read`/`write`. A non-blocking descriptor is waited for with `ppoll`, so the
/// VMM never changes the descriptor's flags (the file description is shared with the sender).
/// There is no timeout: the sender ends a stuck round by closing its end, which fails the read
/// or write.
#[derive(Debug)]
pub struct FdStream {
    file: File,
}

impl FdStream {
    /// Wraps a descriptor received from the API socket.
    pub fn new(file: File) -> Self {
        FdStream { file }
    }

    fn wait(&self, events: libc::c_short) -> io::Result<()> {
        let mut pfd = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events,
            revents: 0,
        };
        loop {
            // SAFETY: one valid pollfd, no timeout, no signal mask change.
            let ret = unsafe { libc::ppoll(&mut pfd, 1, std::ptr::null(), std::ptr::null()) };
            if ret >= 0 {
                return Ok(());
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
}

impl StreamWrite for FdStream {
    fn write_all(&mut self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            // SAFETY: `buf` is a valid readable buffer of `buf.len()` bytes.
            let ret = unsafe { libc::write(self.file.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
            if ret > 0 {
                buf = &buf[usize::try_from(ret).unwrap()..];
                continue;
            }
            if ret == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            let err = io::Error::last_os_error();
            match err.kind() {
                io::ErrorKind::Interrupted => {}
                io::ErrorKind::WouldBlock => self.wait(libc::POLLOUT)?,
                _ => return Err(err),
            }
        }
        Ok(())
    }
}

impl StreamRead for FdStream {
    fn read_exact(&mut self, mut buf: &mut [u8]) -> io::Result<()> {
        while !buf.is_empty() {
            // SAFETY: `buf` is a valid writable buffer of `buf.len()` bytes.
            let ret =
                unsafe { libc::read(self.file.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if ret > 0 {
                buf = &mut buf[usize::try_from(ret).unwrap()..];
                continue;
            }
            if ret == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let err = io::Error::last_os_error();
            match err.kind() {
                io::ErrorKind::Interrupted => {}
                io::ErrorKind::WouldBlock => self.wait(libc::POLLIN)?,
                _ => return Err(err),
            }
        }
        Ok(())
    }
}

/// Whether a page holds only zero bytes.
pub fn is_zero_page(page: &[u8]) -> bool {
    // SAFETY: u128 has no invalid bit patterns; align_to only splits the slice.
    let (head, body, tail) = unsafe { page.align_to::<u128>() };
    head.iter().all(|&b| b == 0)
        && tail.iter().all(|&b| b == 0)
        && body
            .chunks(16)
            .all(|c| c.iter().fold(0u128, |acc, &w| acc | w) == 0)
}

/// Writes one round. The caller copies guest pages into `reserve`d space and `commit`s them.
pub struct RoundWriter<'a, W: StreamWrite> {
    out: &'a mut W,
    hash: Context,
    tags: Vec<u8>,
    data: Vec<u8>,
    chunk_records: usize,
    chunk_data: usize,
    reserved: usize,
    totals: RoundTrailer,
}

impl<W: StreamWrite> std::fmt::Debug for RoundWriter<'_, W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoundWriter")
            .field("chunk_records", &self.chunk_records)
            .field("totals", &self.totals)
            .finish()
    }
}

impl<'a, W: StreamWrite> RoundWriter<'a, W> {
    /// Writes the header of a round.
    pub fn start(out: &'a mut W, header: &RoundHeader) -> Result<Self, StreamError> {
        header.check_layout()?;
        let mut hash = Context::new(&SHA256);
        let bytes = header.encode();
        hash.update(&bytes);
        out.write_all(&bytes)?;
        Ok(RoundWriter {
            out,
            hash,
            tags: Vec::with_capacity(MAX_CHUNK_RECORDS * 8),
            data: Vec::with_capacity(MAX_CHUNK_RECORDS * PAGE_SIZE),
            chunk_records: 0,
            chunk_data: 0,
            reserved: 0,
            totals: RoundTrailer::default(),
        })
    }

    /// Records the current chunk can still take.
    pub fn room(&self) -> usize {
        MAX_CHUNK_RECORDS - self.chunk_records
    }

    /// Space for `pages` pages (at most `room()`), to be filled and then `commit`ted.
    pub fn reserve(&mut self, pages: usize) -> &mut [u8] {
        assert!(pages <= self.room(), "reserve beyond the chunk");
        let start = self.data.len();
        self.data.resize(start + pages * PAGE_SIZE, 0);
        self.reserved = pages;
        &mut self.data[start..]
    }

    /// Adds the reserved pages as the pages at guest physical address `gpa` onwards. A zero
    /// page is left out when `skip_zero` (FIRST round), sent as a zero tag otherwise.
    pub fn commit(&mut self, gpa: u64, skip_zero: bool) -> Result<(), StreamError> {
        let pages = std::mem::take(&mut self.reserved);
        let start = self.data.len() - pages * PAGE_SIZE;
        let mut keep = start;
        for i in 0..pages {
            let from = start + i * PAGE_SIZE;
            let page_gpa = gpa + (i * PAGE_SIZE) as u64;
            if is_zero_page(&self.data[from..from + PAGE_SIZE]) {
                if skip_zero {
                    continue;
                }
                self.tags
                    .extend_from_slice(&(page_gpa | TAG_ZERO).to_le_bytes());
                self.totals.zero_pages += 1;
            } else {
                if keep != from {
                    self.data.copy_within(from..from + PAGE_SIZE, keep);
                }
                keep += PAGE_SIZE;
                self.tags.extend_from_slice(&page_gpa.to_le_bytes());
                self.chunk_data += 1;
                self.totals.data_pages += 1;
            }
            self.chunk_records += 1;
            self.totals.records += 1;
        }
        self.data.truncate(keep);
        if self.chunk_records == MAX_CHUNK_RECORDS {
            self.flush_chunk()?;
        }
        Ok(())
    }

    fn flush_chunk(&mut self) -> Result<(), StreamError> {
        if self.chunk_records == 0 {
            return Ok(());
        }
        let mut head = [0u8; CHUNK_HEADER_LEN];
        head[..4].copy_from_slice(&u32::try_from(self.chunk_records).unwrap().to_le_bytes());
        head[4..].copy_from_slice(&u32::try_from(self.chunk_data).unwrap().to_le_bytes());
        for part in [&head[..], &self.tags, &self.data] {
            self.hash.update(part);
            self.out.write_all(part)?;
        }
        self.tags.clear();
        self.data.clear();
        self.chunk_records = 0;
        self.chunk_data = 0;
        Ok(())
    }

    /// Writes the last chunk, the end mark and the trailer. Returns the round's counts.
    pub fn finish(
        mut self,
        state_digest: Option<[u8; DIGEST_LEN]>,
    ) -> Result<RoundTrailer, StreamError> {
        assert_eq!(self.reserved, 0, "reserved pages were not committed");
        self.flush_chunk()?;
        self.totals.state_digest = state_digest.unwrap_or_default();
        let mut tail = Vec::with_capacity(CHUNK_HEADER_LEN + TRAILER_LEN);
        tail.extend_from_slice(&[0u8; CHUNK_HEADER_LEN]);
        tail.extend_from_slice(&self.totals.records.to_le_bytes());
        tail.extend_from_slice(&self.totals.data_pages.to_le_bytes());
        tail.extend_from_slice(&self.totals.zero_pages.to_le_bytes());
        tail.extend_from_slice(&self.totals.state_digest);
        self.hash.update(&tail);
        tail.extend_from_slice(self.hash.finish().as_ref());
        tail.extend_from_slice(&END_MAGIC);
        self.out.write_all(&tail)?;
        Ok(self.totals)
    }
}

/// Where a reader puts the pages of a round.
pub trait PageSink {
    /// Whether the page at guest physical address `gpa` is in guest memory.
    fn contains(&self, gpa: u64) -> bool;
    /// Writes one page.
    fn write_page(&mut self, gpa: u64, data: &[u8]);
    /// Makes one page zero.
    fn zero_page(&mut self, gpa: u64);
    /// Called once after every page of a chunk was written or zeroed.
    fn chunk_done(&mut self) {}
}

fn read_or_cut<R: StreamRead>(input: &mut R, buf: &mut [u8]) -> Result<(), StreamError> {
    input.read_exact(buf).map_err(|err| match err.kind() {
        io::ErrorKind::UnexpectedEof => StreamError::Cut,
        _ => StreamError::Io(err),
    })
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}

fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

/// Reads one round: first `start` (the header, so the caller can check it before any page is
/// written), then `read_pages`.
pub struct RoundReader<'a, R: StreamRead> {
    input: &'a mut R,
    hash: Context,
}

impl<R: StreamRead> std::fmt::Debug for RoundReader<'_, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoundReader").finish_non_exhaustive()
    }
}

impl<'a, R: StreamRead> RoundReader<'a, R> {
    /// Reads and checks the header of a round.
    pub fn start(input: &'a mut R) -> Result<(Self, RoundHeader), StreamError> {
        let mut hash = Context::new(&SHA256);
        let mut head = [0u8; HEADER_LEN];
        read_or_cut(input, &mut head)?;
        hash.update(&head);
        if head[..8] != ROUND_MAGIC {
            return Err(StreamError::BadMagic);
        }
        let version = le_u32(&head[8..]);
        if version != STREAM_VERSION {
            return Err(StreamError::BadVersion(version));
        }
        let flags = le_u32(&head[12..]);
        if flags & !KNOWN_FLAGS != 0 {
            return Err(StreamError::BadFlags(flags));
        }
        let page_size = le_u32(&head[32..]);
        if page_size as usize != PAGE_SIZE {
            return Err(StreamError::BadPageSize(page_size));
        }
        if head[48..].iter().any(|&b| b != 0) {
            return Err(StreamError::BadReserved);
        }
        let region_count = le_u32(&head[36..]) as usize;
        if region_count > MAX_REGIONS {
            return Err(StreamError::BadLayout("too many regions".into()));
        }
        let mut regions = Vec::with_capacity(region_count);
        let mut desc = [0u8; REGION_LEN];
        for _ in 0..region_count {
            read_or_cut(input, &mut desc)?;
            hash.update(&desc);
            regions.push((le_u64(&desc), le_u64(&desc[8..])));
        }
        let header = RoundHeader {
            flags,
            session: le_u64(&head[16..]),
            seq: le_u64(&head[24..]),
            mem_bytes: le_u64(&head[40..]),
            regions,
        };
        header.check_layout()?;
        Ok((RoundReader { input, hash }, header))
    }

    /// Reads the chunks and the trailer, putting every page into `sink`, and checks the counts
    /// and the digest. The pages are written as they arrive: on an error the sink holds part of
    /// the round and the caller must treat the whole memory as unusable.
    pub fn read_pages<S: PageSink>(mut self, sink: &mut S) -> Result<RoundTrailer, StreamError> {
        let mut totals = RoundTrailer::default();
        let mut tags = vec![0u8; MAX_CHUNK_RECORDS * 8];
        let mut data = vec![0u8; MAX_CHUNK_RECORDS * PAGE_SIZE];
        loop {
            let mut head = [0u8; CHUNK_HEADER_LEN];
            read_or_cut(self.input, &mut head)?;
            self.hash.update(&head);
            let count = le_u32(&head);
            let data_pages = le_u32(&head[4..]);
            if count == 0 {
                if data_pages != 0 {
                    return Err(StreamError::BadChunkData(data_pages, 0));
                }
                break;
            }
            if count as usize > MAX_CHUNK_RECORDS {
                return Err(StreamError::BadChunk(count));
            }
            let tags = &mut tags[..count as usize * 8];
            read_or_cut(self.input, tags)?;
            self.hash.update(tags);
            let mut with_data = 0u32;
            for tag in tags.chunks_exact(8).map(le_u64) {
                if tag & TAG_RESERVED != 0 {
                    return Err(StreamError::BadTag(tag));
                }
                let gpa = tag & !(PAGE_SIZE as u64 - 1);
                if !sink.contains(gpa) {
                    return Err(StreamError::OutsideMemory(gpa));
                }
                if tag & TAG_ZERO == 0 {
                    with_data += 1;
                }
            }
            if with_data != data_pages {
                return Err(StreamError::BadChunkData(data_pages, with_data));
            }
            let data = &mut data[..data_pages as usize * PAGE_SIZE];
            read_or_cut(self.input, data)?;
            self.hash.update(data);
            let mut pages = data.chunks_exact(PAGE_SIZE);
            for tag in tags.chunks_exact(8).map(le_u64) {
                let gpa = tag & !(PAGE_SIZE as u64 - 1);
                if tag & TAG_ZERO != 0 {
                    sink.zero_page(gpa);
                    totals.zero_pages += 1;
                } else {
                    sink.write_page(gpa, pages.next().unwrap());
                    totals.data_pages += 1;
                }
            }
            sink.chunk_done();
            totals.records += u64::from(count);
        }
        let mut tail = [0u8; TRAILER_LEN];
        read_or_cut(self.input, &mut tail)?;
        self.hash.update(&tail[..56]);
        let digest = self.hash.finish();
        if tail[88..] != END_MAGIC {
            return Err(StreamError::BadEndMagic);
        }
        if tail[56..88] != *digest.as_ref() {
            return Err(StreamError::BadDigest);
        }
        if le_u64(&tail) != totals.records
            || le_u64(&tail[8..]) != totals.data_pages
            || le_u64(&tail[16..]) != totals.zero_pages
        {
            return Err(StreamError::BadTotals);
        }
        totals.state_digest.copy_from_slice(&tail[24..56]);
        Ok(totals)
    }
}

/// SHA-256 of `bytes`.
pub fn sha256(bytes: &[u8]) -> [u8; DIGEST_LEN] {
    let digest = aws_lc_rs::digest::digest(&SHA256, bytes);
    let mut out = [0u8; DIGEST_LEN];
    out.copy_from_slice(digest.as_ref());
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;
    use std::os::fd::FromRawFd;
    use std::thread;

    use super::*;

    /// A sparse guest memory for the tests: pages absent read as zero.
    #[derive(Debug, Default)]
    pub(crate) struct MapSink {
        pub(crate) mem_bytes: u64,
        pub(crate) pages: BTreeMap<u64, Vec<u8>>,
    }

    impl PageSink for MapSink {
        fn contains(&self, gpa: u64) -> bool {
            gpa < self.mem_bytes
        }
        fn write_page(&mut self, gpa: u64, data: &[u8]) {
            self.pages.insert(gpa, data.to_vec());
        }
        fn zero_page(&mut self, gpa: u64) {
            self.pages.remove(&gpa);
        }
    }

    fn page(fill: u8) -> Vec<u8> {
        let mut p = vec![0u8; PAGE_SIZE];
        p[17] = fill;
        p[PAGE_SIZE - 1] = fill;
        p
    }

    fn first_header(mem_bytes: u64) -> RoundHeader {
        RoundHeader {
            flags: FLAG_FIRST,
            session: 0x5e55_1011,
            seq: 0,
            mem_bytes,
            regions: vec![(0, mem_bytes)],
        }
    }

    /// Writes a round of the given `(gpa, page)` list, pages in runs as the source does.
    fn write_round(
        header: &RoundHeader,
        pages: &[(u64, Vec<u8>)],
        skip_zero: bool,
        state: Option<[u8; DIGEST_LEN]>,
    ) -> (Vec<u8>, RoundTrailer) {
        let mut out = Vec::new();
        let mut w = RoundWriter::start(&mut out, header).unwrap();
        for (gpa, data) in pages {
            if w.room() == 0 {
                unreachable!();
            }
            w.reserve(1).copy_from_slice(data);
            w.commit(*gpa, skip_zero).unwrap();
        }
        let totals = w.finish(state).unwrap();
        (out, totals)
    }

    fn read_round(
        bytes: &[u8],
        sink: &mut MapSink,
    ) -> Result<(RoundHeader, RoundTrailer), StreamError> {
        let mut input = bytes;
        let (reader, header) = RoundReader::start(&mut input)?;
        let trailer = reader.read_pages(sink)?;
        assert!(
            input.is_empty(),
            "the reader must consume exactly one round"
        );
        Ok((header, trailer))
    }

    #[test]
    fn test_round_roundtrip() {
        let mem = 1024 * PAGE_SIZE as u64;
        let pages: Vec<(u64, Vec<u8>)> = (0..600u64)
            .map(|i| {
                let fill = if i % 3 == 0 {
                    0
                } else {
                    u8::try_from(i % 251).unwrap() + 1
                };
                (i * PAGE_SIZE as u64, page(fill))
            })
            .collect();

        // FIRST: zero pages are left out.
        let (bytes, totals) = write_round(&first_header(mem), &pages, true, None);
        assert_eq!(totals.records, 400);
        assert_eq!(totals.zero_pages, 0);
        let mut sink = MapSink {
            mem_bytes: mem,
            ..Default::default()
        };
        let (header, trailer) = read_round(&bytes, &mut sink).unwrap();
        assert_eq!(header, first_header(mem));
        assert_eq!(trailer, totals);
        assert_eq!(sink.pages.len(), 400);
        for (gpa, data) in &pages {
            match sink.pages.get(gpa) {
                Some(got) => assert_eq!(got, data),
                None => assert!(is_zero_page(data)),
            }
        }

        // A later round turns a page to zero: sent as a zero tag; the state digest travels.
        let next = RoundHeader {
            flags: FLAG_FINAL,
            session: 0x5e55_1011,
            seq: 1,
            mem_bytes: mem,
            regions: vec![],
        };
        let state = sha256(b"vmstate");
        let (bytes, totals) = write_round(
            &next,
            &[(PAGE_SIZE as u64, page(0)), (0, page(9))],
            false,
            Some(state),
        );
        assert_eq!(
            (totals.records, totals.data_pages, totals.zero_pages),
            (2, 1, 1)
        );
        let (header, trailer) = read_round(&bytes, &mut sink).unwrap();
        assert!(header.is_final() && !header.is_first());
        assert_eq!(trailer.state_digest, state);
        assert!(!sink.pages.contains_key(&(PAGE_SIZE as u64)));
        assert_eq!(sink.pages[&0], page(9));
    }

    #[test]
    fn test_empty_round() {
        let header = RoundHeader {
            flags: 0,
            session: 1,
            seq: 3,
            mem_bytes: PAGE_SIZE as u64,
            regions: vec![],
        };
        let (bytes, totals) = write_round(&header, &[], false, None);
        assert_eq!(bytes.len(), HEADER_LEN + CHUNK_HEADER_LEN + TRAILER_LEN);
        assert_eq!(totals.records, 0);
        let mut sink = MapSink {
            mem_bytes: PAGE_SIZE as u64,
            ..Default::default()
        };
        assert_eq!(read_round(&bytes, &mut sink).unwrap().1.records, 0);
    }

    // A round cut anywhere on the wire is refused.
    #[test]
    fn test_cut_round_is_refused() {
        let mem = 8 * PAGE_SIZE as u64;
        let pages: Vec<_> = (0..4u64)
            .map(|i| (i * PAGE_SIZE as u64, page(u8::try_from(i).unwrap() + 1)))
            .collect();
        let (bytes, _) = write_round(&first_header(mem), &pages, true, None);
        for cut in 0..bytes.len() {
            let mut sink = MapSink {
                mem_bytes: mem,
                ..Default::default()
            };
            let mut input = &bytes[..cut];
            let res = RoundReader::start(&mut input).and_then(|(r, _)| r.read_pages(&mut sink));
            assert!(
                matches!(res, Err(StreamError::Cut)),
                "cut at {cut}: {res:?}"
            );
        }
    }

    // Any single altered byte of a round is refused (digest, magic or structure).
    #[test]
    fn test_altered_round_is_refused() {
        let mem = 8 * PAGE_SIZE as u64;
        let pages: Vec<_> = vec![
            (0, page(1)),
            (PAGE_SIZE as u64, page(0)),
            (2 * PAGE_SIZE as u64, page(2)),
        ];
        let header = RoundHeader {
            flags: 0,
            session: 7,
            seq: 2,
            mem_bytes: mem,
            regions: vec![],
        };
        let (bytes, _) = write_round(&header, &pages, false, Some([3; DIGEST_LEN]));
        for pos in 0..bytes.len() {
            let mut altered = bytes.clone();
            altered[pos] ^= 0x40;
            let mut sink = MapSink {
                mem_bytes: mem,
                ..Default::default()
            };
            let mut input = &altered[..];
            let res = RoundReader::start(&mut input).and_then(|(r, _)| r.read_pages(&mut sink));
            assert!(res.is_err(), "byte {pos} altered and accepted");
        }
    }

    #[test]
    fn test_bad_tags_are_refused() {
        let mem = 2 * PAGE_SIZE as u64;
        let header = RoundHeader {
            flags: 0,
            session: 1,
            seq: 1,
            mem_bytes: mem,
            regions: vec![],
        };
        // A page outside guest memory.
        let (bytes, _) = write_round(&header, &[(4 * PAGE_SIZE as u64, page(1))], false, None);
        let mut sink = MapSink {
            mem_bytes: mem,
            ..Default::default()
        };
        assert!(matches!(
            read_round(&bytes, &mut sink),
            Err(StreamError::OutsideMemory(_))
        ));
        assert!(sink.pages.is_empty());

        // Reserved tag bits set.
        let (mut bytes, _) = write_round(&header, &[(0, page(1))], false, None);
        let tag_at = HEADER_LEN + CHUNK_HEADER_LEN;
        bytes[tag_at] |= 0x2;
        assert!(matches!(
            read_round(&bytes, &mut sink),
            Err(StreamError::BadTag(_))
        ));
    }

    #[test]
    fn test_bad_layouts_are_refused() {
        let ps = PAGE_SIZE as u64;
        let bad = [
            RoundHeader {
                regions: vec![],
                ..first_header(ps)
            },
            RoundHeader {
                regions: vec![(0, ps), (0, ps)],
                ..first_header(2 * ps)
            },
            RoundHeader {
                regions: vec![(1, ps)],
                ..first_header(ps)
            },
            RoundHeader {
                regions: vec![(0, ps)],
                ..first_header(2 * ps)
            },
            RoundHeader {
                flags: 0,
                ..first_header(ps)
            },
        ];
        for header in bad {
            let mut out = Vec::new();
            assert!(matches!(
                RoundWriter::start(&mut out, &header),
                Err(StreamError::BadLayout(_))
            ));
        }
    }

    // Two rounds back to back on one non-blocking pipe: the writer waits for room, the reader
    // waits for data, and the reader stops exactly at the end of each round.
    #[test]
    fn test_fd_stream_nonblocking_pipe() {
        let mut fds = [0; 2];
        // SAFETY: valid array of two descriptors.
        let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
        assert_eq!(ret, 0);
        // SAFETY: both descriptors were just created and are owned here.
        let (rd, wr) = unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) };
        let mem = 2048 * PAGE_SIZE as u64;
        let pages: Vec<_> = (0..2048u64)
            .map(|i| {
                (
                    i * PAGE_SIZE as u64,
                    page(u8::try_from(i % 200).unwrap() + 1),
                )
            })
            .collect();

        let writer = {
            let pages = pages.clone();
            thread::spawn(move || {
                let mut out = FdStream::new(wr);
                for seq in 0..2u64 {
                    let header = RoundHeader {
                        seq,
                        flags: if seq == 0 { FLAG_FIRST } else { 0 },
                        regions: if seq == 0 { vec![(0, mem)] } else { vec![] },
                        ..first_header(mem)
                    };
                    let mut w = RoundWriter::start(&mut out, &header).unwrap();
                    for chunk in pages.chunks(100) {
                        let mut gpa = chunk[0].0;
                        let mut left = chunk;
                        while !left.is_empty() {
                            let n = left.len().min(w.room());
                            let buf = w.reserve(n);
                            for (i, (_, data)) in left[..n].iter().enumerate() {
                                buf[i * PAGE_SIZE..(i + 1) * PAGE_SIZE].copy_from_slice(data);
                            }
                            w.commit(gpa, seq == 0).unwrap();
                            gpa += (n * PAGE_SIZE) as u64;
                            left = &left[n..];
                        }
                    }
                    w.finish(None).unwrap();
                }
            })
        };

        let mut input = FdStream::new(rd);
        let mut sink = MapSink {
            mem_bytes: mem,
            ..Default::default()
        };
        for seq in 0..2u64 {
            let (reader, header) = RoundReader::start(&mut input).unwrap();
            assert_eq!(header.seq, seq);
            assert_eq!(reader.read_pages(&mut sink).unwrap().records, 2048);
        }
        writer.join().unwrap();
        assert_eq!(sink.pages.len(), 2048);
        for (gpa, data) in &pages {
            assert_eq!(&sink.pages[gpa], data);
        }
        // The writer closed its end: a third round reads as cut.
        assert!(matches!(
            RoundReader::start(&mut input),
            Err(StreamError::Cut)
        ));
    }

    // Writing into a pipe whose reader is gone fails (EPIPE) instead of killing the process.
    #[test]
    fn test_fd_stream_reader_gone() {
        let mut fds = [0; 2];
        // SAFETY: valid array of two descriptors.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: both descriptors were just created and are owned here.
        let (rd, wr) = unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) };
        drop(rd);
        // The Rust runtime ignores SIGPIPE in the test binary; the VMM handles it (metric only).
        let mut out = FdStream::new(wr);
        let err = RoundWriter::start(&mut out, &first_header(PAGE_SIZE as u64)).unwrap_err();
        assert!(matches!(err, StreamError::Io(e) if e.kind() == io::ErrorKind::BrokenPipe));
    }

    #[test]
    fn test_is_zero_page() {
        assert!(is_zero_page(&[0u8; PAGE_SIZE]));
        for pos in [0, 1, 15, 16, 255, 256, PAGE_SIZE - 1] {
            let mut p = vec![0u8; PAGE_SIZE];
            p[pos] = 1;
            assert!(!is_zero_page(&p), "{pos}");
        }
    }
}
