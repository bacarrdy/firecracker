// Copyright 2026 VPSnet. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A whole move in one process, with real KVM VMs.

use std::fs::File;
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryRegion};
use vmm_sys_util::tempfile::TempFile;

use super::stream::FdStream;
use super::*;
use crate::persist::{VmInfo, create_state_only_snapshot};
use crate::resources::VmResources;
use crate::rpc_interface::{PrebootApiController, VmmAction, VmmData};
use crate::seccomp::get_empty_filters;
use crate::test_utils::create_vmm;
use crate::test_utils::mock_resources::NOISY_KERNEL_IMAGE;
use crate::vmm_config::instance_info::InstanceInfo;
use crate::vmm_config::snapshot::{
    CreateSnapshotParams, LoadSnapshotParams, MemBackendConfig, MemBackendType,
    SnapshotLoadHugePageConfig, SnapshotType,
};
use crate::vstate::memory::GuestMemoryMmap;
use crate::{EventManager, Vmm};

fn pipe() -> (File, File) {
    let mut fds = [0; 2];
    // SAFETY: valid array of two descriptors.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    // SAFETY: both descriptors were just created and are owned here.
    unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
}

fn guest_memory(vmm: &Vmm) -> GuestMemoryMmap {
    vmm.vm.as_kvm().unwrap().guest_memory().clone()
}

/// The pages that differ between two guest memories of the same layout, with the byte range
/// that differs in each.
fn differing_pages(a: &GuestMemoryMmap, b: &GuestMemoryMmap) -> Vec<(u64, usize, usize)> {
    let mut out = Vec::new();
    let mut pa = vec![0u8; PAGE_SIZE];
    let mut pb = vec![0u8; PAGE_SIZE];
    for region in a.iter() {
        let base = region.start_addr().0;
        for gpa in (base..base + region.len()).step_by(PAGE_SIZE) {
            a.read_slice(&mut pa, GuestAddress(gpa)).unwrap();
            b.read_slice(&mut pb, GuestAddress(gpa)).unwrap();
            if pa != pb {
                let first = pa.iter().zip(&pb).position(|(x, y)| x != y).unwrap();
                let last = pa.iter().zip(&pb).rposition(|(x, y)| x != y).unwrap();
                out.push((gpa, first, last));
            }
        }
    }
    out
}

enum ToTarget {
    Round(File),
    Digest,
    Load(PathBuf),
}

/// A guest that keeps writing memory (the noisy test kernel); three rounds while it runs; the
/// pause (StateOnly + FINAL round); a second VMM that receives the rounds before it loads the
/// state with the Migration backend. Before its load, the target's memory equals the paused
/// source's byte for byte; after the load only the restore writes differ; then the target runs.
#[test]
fn test_move_between_two_vmms() {
    let (source, _source_em) = create_vmm(Some(NOISY_KERNEL_IMAGE), true, true, false, false);
    thread::sleep(Duration::from_millis(500));

    // The target VMM: a pre-boot controller in its own thread, as in a second process.
    let (to_target, target_rx) = mpsc::channel::<ToTarget>();
    let (from_target, results) = mpsc::channel::<Result<VmmData, String>>();
    let target = thread::spawn(move || {
        let filters = get_empty_filters();
        let mut resources = VmResources::default();
        let mut em = EventManager::new().unwrap();
        let mut controller =
            PrebootApiController::new(&filters, InstanceInfo::default(), &mut resources, &mut em);
        for msg in target_rx {
            let action = match msg {
                ToTarget::Round(file) => {
                    VmmAction::MigrationReceiveRound(MigrationReceiveRoundParams {
                        stream: Some(MigrationFd(file)),
                    })
                }
                ToTarget::Digest => VmmAction::GetMigrationMemoryDigest,
                ToTarget::Load(path) => VmmAction::LoadSnapshot(LoadSnapshotParams {
                    snapshot_path: path,
                    mem_backend: MemBackendConfig {
                        backend_path: PathBuf::new(),
                        backend_type: MemBackendType::Migration,
                    },
                    track_dirty_pages: true,
                    resume_vm: false,
                    network_overrides: vec![],
                    vsock_override: None,
                    clock_realtime: false,
                    huge_pages: SnapshotLoadHugePageConfig::Snapshot,
                }),
            };
            let is_load = matches!(action, VmmAction::LoadSnapshot(_));
            let result = controller
                .handle_preboot_request(action)
                .map_err(|err| err.to_string());
            from_target.send(result).unwrap();
            if is_load {
                break;
            }
        }
        controller.built_vmm.take()
    });

    let (rd, wr) = pipe();
    let send = |vmm: &mut Vmm, first: bool| {
        to_target
            .send(ToTarget::Round(rd.try_clone().unwrap()))
            .unwrap();
        let job = vmm
            .migration_prepare_round(first, false, FdStream::new(wr.try_clone().unwrap()))
            .unwrap();
        let stats = job.run(None).unwrap();
        let received = results.recv().unwrap().unwrap();
        assert!(matches!(received, VmmData::MigrationReceived(ref r) if r.seq == stats.seq));
        stats
    };

    // Rounds while the guest runs.
    let mut rounds = Vec::new();
    for i in 0..3 {
        rounds.push(send(&mut source.lock().unwrap(), i == 0));
        thread::sleep(Duration::from_millis(100));
    }
    assert!(rounds[0].first && rounds[0].data_pages > 0);
    // The guest kept writing: the later rounds carried pages it wrote while running.
    assert!(rounds[1].dirty_pages > 0 && rounds[2].dirty_pages > 0);

    // The pause: StateOnly + FINAL round, in one call.
    let state_file = TempFile::new().unwrap();
    let mut src = source.lock().unwrap();
    let paused_at = Instant::now();
    src.pause_vm().unwrap();
    to_target
        .send(ToTarget::Round(rd.try_clone().unwrap()))
        .unwrap();
    let params = CreateSnapshotParams {
        snapshot_type: SnapshotType::StateOnly,
        snapshot_path: state_file.as_path().to_path_buf(),
        mem_file_path: PathBuf::new(),
        sync_snapshot_files: false,
        migration_stream: None,
    };
    let vm_info = VmInfo::from(&*src);
    let last = create_state_only_snapshot(
        &mut src,
        &vm_info,
        &params,
        FdStream::new(wr.try_clone().unwrap()),
    )
    .unwrap();
    let final_done = paused_at.elapsed();
    assert!(matches!(
        results.recv().unwrap().unwrap(),
        VmmData::MigrationReceived(ref r) if r.r#final
    ));

    // The proof: the paused source's memory equals the target's received memory.
    let source_digest = src.migration_memory_digest().unwrap();
    to_target.send(ToTarget::Digest).unwrap();
    let VmmData::MigrationMemoryDigest(target_digest) = results.recv().unwrap().unwrap() else {
        panic!("no digest");
    };
    assert_eq!(source_digest, target_digest, "target memory differs");

    to_target
        .send(ToTarget::Load(state_file.as_path().to_path_buf()))
        .unwrap();
    results.recv().unwrap().unwrap();
    let loaded = paused_at.elapsed();
    let target_vmm = target.join().unwrap().expect("the target loaded the state");
    let mut dst = target_vmm.lock().unwrap();

    // After the load only the restore's own writes differ: the VMClock page, the VMGenID, KVM's
    // pvclock wall clock; a few bytes each.
    let differ = differing_pages(&guest_memory(&src), &guest_memory(&dst));
    assert!(differ.len() <= 3, "{differ:x?}");
    assert!(differ.iter().all(|&(_, a, b)| b - a < 128), "{differ:x?}");

    eprintln!(
        "move: memory {} MiB | rounds (seq, dirty, data pages, bytes, us) {:?} | final round {} \
         pages ({} data, {} zero), {} B, {} us | pause to final round done {:?}, to target \
         loaded {:?} | after load {} restore-written pages",
        source_digest.bytes >> 20,
        rounds
            .iter()
            .map(|r| (r.seq, r.dirty_pages, r.data_pages, r.bytes, r.duration_us))
            .collect::<Vec<_>>(),
        last.dirty_pages,
        last.data_pages,
        last.zero_pages,
        last.bytes,
        last.duration_us,
        final_done,
        loaded,
        differ.len()
    );

    // The target runs on: its guest keeps writing memory after the resume.
    let before = dst.migration_memory_digest().unwrap();
    dst.resume_vm().unwrap();
    thread::sleep(Duration::from_millis(300));
    dst.pause_vm().unwrap();
    assert_ne!(dst.migration_memory_digest().unwrap(), before);
}
