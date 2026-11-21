import hashlib
import os
from pathlib import Path
import subprocess

from framework.artifacts import GUEST_KERNEL_DEFAULT, pin_guest_kernel
from framework.vm_backend import VM_BACKEND_KVM, pin_vm_backend


def read_digest(vm, block_params="bs=4096 count=2"):
    _, digest, _ = vm.ssh.check_output(
        f"set -o pipefail; dd if=/dev/vdb {block_params} iflag=direct status=none | sha256sum"
    )
    return digest.split()[0]


def assert_direct(vm, path, enabled, drive_id="scratch"):
    raw = vm.api.vm_config.get().json()
    drive = next(item for item in raw["drives"] if item["drive_id"] == drive_id)
    # The API can represent default false by omitting the field.
    assert drive.get("direct_io", False) is enabled, drive
    state = Path(f"/proc/{vm.firecracker_pid}")
    target = Path(vm.chroot()) / Path(path).name
    target_stat = target.stat()
    candidates = []
    for fd in (state / "fd").iterdir():
        try:
            fd_stat = fd.stat()
            if (fd_stat.st_dev, fd_stat.st_ino) == (target_stat.st_dev, target_stat.st_ino):
                info = (state / "fdinfo" / fd.name).read_text()
                flags = int(next(line.split()[1] for line in info.splitlines()
                                 if line.startswith("flags:")), 8)
                candidates.append(flags)
        except (FileNotFoundError, PermissionError):
            continue
    assert candidates, (target, vm.firecracker_pid)
    assert all(bool(flags & os.O_DIRECT) is enabled for flags in candidates), candidates
    print(f"API direct_io={enabled}, open flags={[oct(x) for x in candidates]}, {path}")



def assert_default_seccomp(vm):
    def proc_status(path):
        return dict(tuple(part.strip() for part in line.split(":", 1))
                    for line in path.read_text().splitlines() if ":" in line)

    state = Path(f"/proc/{vm.firecracker_pid}")
    main = proc_status(state / "status")
    workers = [status for task in (state / "task").iterdir()
               if (status := proc_status(task / "status"))["Name"] == "fc_q0_scratch"]
    assert main["Seccomp"] == "2"
    assert len(workers) == 1 and workers[0]["Seccomp"] == "2", workers
    print("VMM and scratch block worker are running with seccomp filtering.")


def write_sector(vm):
    vm.ssh.check_output(
        "dd if=/dev/zero of=/dev/vdb bs=512 seek=7 count=2 "
        "oflag=direct conv=notrunc,fsync status=none"
    )


def attach_loop(image):
    device = subprocess.check_output(
        ["losetup", "--find", "--show", "--sector-size", "4096", str(image)],
        text=True,
    ).strip()
    assert subprocess.check_output(["blockdev", "--getss", device], text=True).strip() == "4096"
    return Path(device)


@pin_vm_backend(VM_BACKEND_KVM)
@pin_guest_kernel(GUEST_KERNEL_DEFAULT)
def test_snapshot_direct_io(uvm, microvm_factory, io_engine, tmp_path):
    pattern = bytes(range(256)) * 32
    initial = tmp_path / "initial.raw"
    initial.write_bytes(pattern)
    with initial.open("ab") as backing:
        backing.truncate(2 * 1024 * 1024)
    loop = attach_loop(initial)
    replacement = tmp_path / "replacement.raw"
    replacement.write_bytes(pattern[::-1])
    with replacement.open("ab") as backing:
        backing.truncate(2 * 1024 * 1024)
    alternate = tmp_path / "alternate.raw"
    alternate.write_bytes(bytes(8192))
    with alternate.open("ab") as backing:
        backing.truncate(2 * 1024 * 1024)
    restored = None
    new_loop = None
    try:
        uvm.spawn()
        uvm.basic_config(vcpu_count=1, mem_size_mib=256)
        uvm.add_net_iface()
        uvm.add_drive("scratch", loop, io_engine=io_engine, direct_io=True,
                      cache_type="Writeback", blk_size=512, num_queues=1)
        assert_direct(uvm, loop, True)
        uvm.start()
        assert_default_seccomp(uvm)
        assert uvm.ssh.check_output("blockdev --getss /dev/vdb")[1].strip() == "512"
        write_sector(uvm)
        expected = pattern[:3584] + bytes(1024) + pattern[4608:]
        assert read_digest(uvm) == hashlib.sha256(expected).hexdigest()
        assert read_digest(uvm, "bs=512 skip=7 count=2") == hashlib.sha256(bytes(1024)).hexdigest()
        assert initial.read_bytes()[:8192] == expected
        print("Before snapshot, two 512 B writes and two 512 B reads crossed a 4 KiB boundary; guest and host bytes match.")
        snapshot = uvm.snapshot_full()
        print("Created full snapshot; stopping the source VMM.")
        uvm.kill()
        restored = microvm_factory.build()
        restored.spawn()
        restored.restore_from_snapshot(snapshot, resume=True)
        assert_default_seccomp(restored)
        assert_direct(restored, loop, True)
        assert read_digest(restored) == hashlib.sha256(expected).hexdigest()
        print("Fresh VMM restored the snapshot and guest readback matches the pre-snapshot bytes.")
        root_id = next(drive["drive_id"] for drive in restored.api.vm_config.get().json()["drives"]
                       if drive["is_root_device"])
        assert_direct(restored, restored.disks[root_id], False, root_id)
        # A threaded raw block PATCH exercises geometry probing in the worker.
        new_loop = attach_loop(alternate)
        restored.api.drive.patch(
            drive_id="scratch", path_on_host=restored.create_jailed_resource(new_loop)
        )
        assert_direct(restored, new_loop, True)
        assert read_digest(restored) == hashlib.sha256(bytes(8192)).hexdigest()
        print("Threaded PATCH to a second 4Kn block device retained O_DIRECT and read correctly.")
        restored.api.drive.patch(
            drive_id="scratch", path_on_host=restored.create_jailed_resource(replacement)
        )
        assert_direct(restored, replacement, True)
        assert read_digest(restored) == hashlib.sha256(pattern[::-1]).hexdigest()
        write_sector(restored)
        flipped = pattern[::-1]
        expected_replacement = flipped[:3584] + bytes(1024) + flipped[4608:]
        assert read_digest(restored) == hashlib.sha256(expected_replacement).hexdigest()
        assert replacement.read_bytes()[:8192] == expected_replacement
        assert initial.read_bytes()[:8192] == expected
        print("Threaded PATCH retained O_DIRECT; guest and new host backing matched, with the old backing unchanged.")
    finally:
        try:
            if restored is not None:
                restored.kill(might_be_dead=True)
        finally:
            try:
                uvm.kill(might_be_dead=True)
            finally:
                for device in filter(None, [new_loop, loop]):
                    subprocess.run(["losetup", "--detach", str(device)], check=True)
