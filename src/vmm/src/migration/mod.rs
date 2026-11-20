// Copyright 2026 VPSnet. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Pre-copy migration of guest memory between two Firecracker processes (protocol 1).
//!
//! The source copies guest memory in rounds while the vCPUs run. Each round first takes and
//! clears the dirty set (KVM's dirty log and Firecracker's own bitmap), then copies exactly those
//! pages: "reset before copy", so a page written after its copy is in the next round's set. The
//! pause carries only the last round, written together with the device state. The target writes
//! the rounds into fresh private anonymous memory before any device state exists, and loads the
//! state on top of that memory.

pub mod dirty;
#[cfg(test)]
mod e2e_tests;
pub mod source;
pub mod stream;
pub mod target;

/// The protocol version answered by `GET /migration`.
pub const PROTOCOL_VERSION: u32 = 1;

/// The page size of the protocol. Guest memory is tracked and sent in pages of this size.
pub const PAGE_SIZE: usize = 4096;

/// A descriptor passed with SCM_RIGHTS on the API socket: the move's connection (a pipe or a
/// connected socket) that rounds are written to or read from.
#[derive(Debug)]
pub struct MigrationFd(pub std::fs::File);

impl PartialEq for MigrationFd {
    fn eq(&self, other: &Self) -> bool {
        use std::os::fd::AsRawFd;
        self.0.as_raw_fd() == other.0.as_raw_fd()
    }
}

impl Eq for MigrationFd {}

/// `PUT /migration/send-round`.
#[derive(Debug, PartialEq, Eq)]
pub struct MigrationSendRoundParams {
    /// Start a new session: reset the dirty set and send every non-zero page.
    pub first: bool,
    /// The descriptor the round is written to.
    pub stream: Option<MigrationFd>,
}

/// `GET /migration/memory-digest`: the SHA-256 of all of guest memory, region after region in
/// guest physical order.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MemoryDigest {
    /// Lower-case hex SHA-256.
    pub sha256: String,
    /// Bytes hashed (the guest memory size).
    pub bytes: u64,
}

/// Hashes guest memory regions in order. Single-threaded: about 1 GiB/s with SHA extensions.
pub fn memory_digest<'a, R: vm_memory::GuestMemoryRegion + 'a>(
    regions: impl Iterator<Item = &'a R>,
) -> Result<MemoryDigest, MigrationError> {
    use aws_lc_rs::digest::{Context, SHA256};
    let mut hash = Context::new(&SHA256);
    let mut buf = vec![0u8; 1 << 20];
    let mut bytes = 0u64;
    for region in regions {
        let mut offset = 0u64;
        while offset < region.len() {
            let n = usize::try_from((region.len() - offset).min(buf.len() as u64)).unwrap();
            region
                .get_slice(vm_memory::MemoryRegionAddress(offset), n)
                .map_err(|err| MigrationError::Memory(err.to_string()))?
                .copy_to(&mut buf[..n]);
            hash.update(&buf[..n]);
            offset += n as u64;
        }
        bytes += region.len();
    }
    let sha256 = hash
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(MemoryDigest { sha256, bytes })
}

/// `GET /migration`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MigrationInfo {
    /// The migration protocol version.
    pub protocol: u32,
}

impl Default for MigrationInfo {
    fn default() -> Self {
        MigrationInfo {
            protocol: PROTOCOL_VERSION,
        }
    }
}

/// Errors of the migration rounds.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum MigrationError {
    /// Dirty page tracking is off for this microVM: start it with `track_dirty_pages`.
    DirtyTrackingOff,
    /// Cannot read KVM's dirty log of slot {0}: {1}
    DirtyLog(u32, kvm_ioctls::Error),
    /// Not supported by migration protocol 1: {0}
    Unsupported(String),
    /// Migration stream error: {0}
    Stream(#[from] stream::StreamError),
    /// No migration session: the first round must be sent with `first: true`
    NoSession,
    /// A migration round is still being sent
    RoundInProgress,
    /// The request must carry exactly one file descriptor (SCM_RIGHTS)
    MissingDescriptor,
    /// The migration receiver is closed: {0}
    ReceiverClosed(String),
    /// Round out of sequence: {0}
    Sequence(String),
    /// Guest memory error: {0}
    Memory(String),
    /// The FINAL round has not been received
    Incomplete,
    /// The state file is not the one written with the FINAL round (SHA-256 differs)
    StateDigest,
    /// The snapshot's memory layout differs from the received one: {0}
    LayoutMismatch(String),
    /// Boot resources are configured: a migration target only loads a snapshot
    BootConfigured,
}

/// `PUT /migration/receive-round`.
#[derive(Debug, PartialEq, Eq)]
pub struct MigrationReceiveRoundParams {
    /// The descriptor the round is read from.
    pub stream: Option<MigrationFd>,
}
