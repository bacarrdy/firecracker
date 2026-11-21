#!/usr/bin/env python3
import argparse
import ctypes
import hashlib
import json
import mmap
import os
from pathlib import Path
import random
import selectors
import shutil
import struct
import subprocess
import tempfile
import time
import uuid

HERE = Path(__file__).resolve().parent
IMAGE = 'public.ecr.aws/firecracker/fcuvm:v94'
KERNEL = Path(os.environ['GUEST_KERNEL']).resolve()
RUNNER = Path(os.environ['PROBE_DIR']).resolve()
PAGE = 4096
MIB = 1024 * 1024
SEQ = 128 * MIB
MIXED = 64 * MIB
STAGES = ('ready', 'cold-read-MiBs', 'warm-read-MiBs', 'write-with-flush-MiBs',
          'mixed-with-flush-IOPS')
U32_MAX = (1 << 32) - 1


def mixed_write_pages():
    order = list(range(MIXED // PAGE))
    state = 0x849fe7ab
    for i in range(len(order) - 1, 0, -1):
        state = (state ^ (state << 13)) & U32_MAX
        state ^= state >> 17
        state = (state ^ (state << 5)) & U32_MAX
        j = state % (i + 1)
        order[i], order[j] = order[j], order[i]
    return set(order[1::2])


def prepare_data(path):
    expected = hashlib.sha256()
    writes = mixed_write_pages()
    original_page = bytearray([0x31]) * PAGE
    sequential_page = bytearray([0x32]) * PAGE
    mixed_page = bytearray([0x33]) * PAGE
    with path.open('wb') as backing:
        for page in range((2 * SEQ + MIXED) // PAGE):
            struct.pack_into('<Q', original_page, 0, page)
            backing.write(original_page)
            if SEQ // PAGE <= page < 2 * SEQ // PAGE:
                struct.pack_into('<Q', sequential_page, 0, (1 << 32) | page)
                expected.update(sequential_page)
            elif page - 2 * SEQ // PAGE in writes:
                struct.pack_into('<Q', mixed_page, 0, (2 << 32) | page)
                expected.update(mixed_page)
            else:
                expected.update(original_page)
        backing.flush()
        os.fsync(backing.fileno())
        stat = os.fstat(backing.fileno())
        if stat.st_blocks * 512 < stat.st_size:
            raise RuntimeError('Backing file is unexpectedly sparse')
        os.posix_fadvise(backing.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
    return expected.hexdigest()


def residency(file):
    libc = ctypes.CDLL(None, use_errno=True)
    mincore = libc.mincore
    mincore.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p]
    mincore.restype = ctypes.c_int
    size = 2 * SEQ + MIXED
    with mmap.mmap(file.fileno(), size, access=mmap.ACCESS_COPY) as region:
        resident = (ctypes.c_ubyte * (size // PAGE))()
        buf = ctypes.c_char.from_buffer(region)
        result = mincore(ctypes.addressof(buf), size, resident)
        del buf
        if result:
            error = ctypes.get_errno()
            raise OSError(error, os.strerror(error))
        return {
            'sequential_read': sum(value & 1 for value in resident[:SEQ // PAGE]) * PAGE,
            'sequential_write': sum(value & 1 for value in resident[SEQ // PAGE:2 * SEQ // PAGE]) * PAGE,
            'mixed': sum(value & 1 for value in resident[2 * SEQ // PAGE:]) * PAGE,
        }


def docker_exec(name, *command):
    return subprocess.run(['docker', 'exec', name, *command], check=True, capture_output=True,
                          text=True, timeout=8).stdout


def api(name, method, path, payload=None):
    command = ['curl', '--fail-with-body', '-sS', '--unix-socket', '/lab/api.sock',
               '-X', method, 'http://localhost' + path]
    if payload is not None:
        command += ['-H', 'Content-Type: application/json', '-d', json.dumps(payload)]
    try:
        return docker_exec(name, *command)
    except subprocess.CalledProcessError as error:
        raise RuntimeError(f'{method} {path} returned {error.stdout or error.stderr}') from error


def configure(name, engine, field, direct):
    api(name, 'PUT', '/boot-source', {'kernel_image_path': '/kernel', 'initrd_path': '/ramdisk',
                                    'boot_args': 'console=ttyS0 reboot=k panic=1 pci=conf1 rdinit=/init'})
    api(name, 'PUT', '/machine-config', {'vcpu_count': 1, 'mem_size_mib': 384})
    drive = {'drive_id': 'data', 'path_on_host': '/lab/data', 'is_root_device': False,
             'is_read_only': False, 'io_engine': engine, 'blk_size': 4096,
             'cache_type': 'Writeback', field: direct}
    api(name, 'PUT', '/drives/data', drive)
    api(name, 'PUT', '/metrics', {'metrics_path': '/lab/metrics'})
    cfg = json.loads(api(name, 'GET', '/vm/config'))['drives'][0]
    for key in (field, 'io_engine', 'blk_size', 'cache_type'):
        if key == field and field != 'direct_io':
            continue
        if cfg.get(key, False) != drive[key]:
            raise RuntimeError(f'Unexpected GET /vm/config value for {key}: {cfg}')
    api(name, 'PUT', '/actions', {'action_type': 'InstanceStart'})
    return cfg


def file_descriptors(name):
    flags = docker_exec(
        name, 'sh', '-c',
        """for fd in /proc/1/fd/*; do
          if [ "$(readlink "$fd")" = /lab/data ]; then
            sed -n 's/^flags:[[:space:]]*//p' /proc/1/fdinfo/"${fd##*/}"
          fi
        done""")
    return flags.splitlines()


def storage_counters(name):
    contents = docker_exec(name, 'cat', '/proc/1/io')
    return {key: int(value) for key, value in
            (line.split(':', 1) for line in contents.splitlines())
            if key in ('read_bytes', 'write_bytes')}


def run_guest(vm, name, data, console):
    stages = {}
    rates = {}
    latencies = {}
    counters = {}
    verification = False
    deadline = time.monotonic() + 60
    partial = b''
    with selectors.DefaultSelector() as monitor, data.open('rb') as backing:
        monitor.register(vm.stdout, selectors.EVENT_READ)
        while time.monotonic() < deadline:
            if not monitor.select(timeout=max(0, deadline - time.monotonic())):
                break
            chunk = os.read(vm.stdout.fileno(), 65536)
            if not chunk:
                raise RuntimeError(f'Firecracker exited with code {vm.poll()}')
            console.write(chunk)
            console.flush()
            fragments = (partial + chunk).split(b'\n')
            partial = fragments.pop()
            for fragment in fragments:
                line = fragment.decode(errors='replace').strip()
                if line == 'QA-verified':
                    verification = True
                if not line.startswith('QA '):
                    continue
                fields = line.split()
                stage = fields[1]
                if stage == 'logical-block-size':
                    if fields[2] != '4096':
                        raise RuntimeError(line)
                    continue
                if stage == 'exit':
                    if fields[2] == '0' and verification and list(stages) == list(STAGES):
                        return stages, rates, latencies, counters
                    raise RuntimeError(f'Guest verification failed: {line}')
                if stage not in STAGES:
                    raise RuntimeError(line)
                if stage == 'ready':
                    os.posix_fadvise(backing.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
                else:
                    rates[stage] = float(fields[2])
                    latencies[stage] = {direction: float(value) for direction, value in
                                        zip(('read', 'write'), fields[3:5], strict=True) if float(value)}
                stages[stage] = residency(backing)
                counters[stage] = storage_counters(name)
                vm.stdin.write(b'\n')
                vm.stdin.flush()
    raise RuntimeError(f'Timed out after guest stages {list(stages)}')


def run_once(binary, work, output, name, engine, field, direct):
    data = work / f'{name}.raw'
    expected_hash = prepare_data(data)
    dioalign = subprocess.check_output([RUNNER / 'dioalign', data], text=True).split()
    if len(dioalign) != 3 or dioalign[0] != '1' or min(map(int, dioalign[1:])) <= 0:
        raise RuntimeError(f'Ext4 did not return STATX_DIOALIGN: {dioalign}')
    work.joinpath('api.sock').unlink(missing_ok=True)
    work.joinpath('metrics').write_text('')
    container = f'fc-dio-pr-kvm-{uuid.uuid4().hex[:12]}'
    load_before = os.getloadavg()
    command = ['docker', 'run', '--rm', '--pull=never', '--interactive', '--name', container, '--network', 'none',
               '--device', '/dev/kvm', '--security-opt', 'seccomp=unconfined',
               '--mount', f'type=bind,src={work},dst=/lab',
               '--mount', f'type=bind,src={data},dst=/lab/data',
               '--mount', f'type=bind,src={KERNEL},dst=/kernel,readonly',
               '--mount', f'type=bind,src={RUNNER / "probe.gz"},dst=/ramdisk,readonly',
               '--mount', f'type=bind,src={binary},dst=/firecracker,readonly',
               '--entrypoint', '/firecracker', IMAGE, '--enable-pci', '--api-sock', '/lab/api.sock']
    with subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                          stderr=subprocess.STDOUT) as vm, (output / f'{name}.log').open('wb') as console:
        try:
            deadline = time.monotonic() + 10
            while not (work / 'api.sock').is_socket():
                if vm.poll() is not None:
                    raise RuntimeError(f'Firecracker exited before creating API socket: {vm.poll()}')
                if time.monotonic() > deadline:
                    raise RuntimeError('Timed out waiting for API socket')
                time.sleep(.02)
            cfg = configure(container, engine, field, direct)
            fd_flags = file_descriptors(container)
            if field == 'direct_io' and (not fd_flags or
                                         any(bool(int(flag, 8) & os.O_DIRECT) != direct
                                             for flag in fd_flags)):
                raise RuntimeError(f'{field}={direct}: unexpected fd flags {fd_flags}')
            stages, rates, latencies, counters = run_guest(vm, container, data, console)
            api(container, 'PUT', '/actions', {'action_type': 'FlushMetrics'})
        finally:
            subprocess.run(['docker', 'rm', '--force', container], capture_output=True, timeout=10)
            console.write(vm.stdout.read())
    load_after = os.getloadavg()
    current_metrics = (work / 'metrics').read_text()
    (output / f'{name}-metrics.jsonl').write_text(current_metrics)
    block_metrics = [json.loads(line)['block'] for line in current_metrics.splitlines()]
    metric_keys = ('read_bytes', 'write_bytes', 'read_count', 'write_count', 'flush_count',
                   'execute_fails', 'invalid_reqs_count')
    metrics = {key: sum(m[key] for m in block_metrics) for key in metric_keys}
    if metrics['read_bytes'] < 2 * SEQ + MIXED // 2 or metrics['write_bytes'] != SEQ + MIXED // 2:
        raise RuntimeError(f'Unexpected byte counts in block metrics: {metrics}')
    if metrics['flush_count'] != 2 or metrics['execute_fails'] or metrics['invalid_reqs_count']:
        raise RuntimeError(f'Unexpected errors or flush counts in block metrics: {metrics}')
    with data.open('rb') as backing:
        os.posix_fadvise(backing.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
        actual_hash = hashlib.file_digest(backing, 'sha256').hexdigest()
    data.unlink()
    if actual_hash != expected_hash:
        raise RuntimeError(f'{name}: raw file hash {actual_hash} != {expected_hash}')
    return {'run': name, 'io_engine': engine, field: direct, 'fd_flags_octal': fd_flags,
            'statx_dio_mem_align': int(dioalign[1]), 'statx_dio_offset_align': int(dioalign[2]),
            'vm_config_drive': cfg, 'page_cache_resident_bytes': stages, 'guest_rates': rates, 'guest_p99_us': latencies,
            'host_process_storage_counters': counters, 'vmm_block_metrics': metrics,
            'full_file_sha256': actual_hash, 'guest_readback_ok': True,
            'host_load_before': load_before, 'host_load_after': load_after}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('binary', type=Path)
    parser.add_argument('output', type=Path)
    parser.add_argument('--engines', nargs='+', choices=('Async', 'Sync'), default=['Async', 'Sync'])
    parser.add_argument('--modes', nargs='+', choices=('buffered', 'direct'), default=['buffered', 'direct'])
    parser.add_argument('--repeat', type=int, default=5)
    parser.add_argument('--field', default='direct_io', choices=('direct_io', 'direct_write'))
    parser.add_argument('--revision', default='smoke')
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    if os.sysconf('SC_PAGE_SIZE') != PAGE:
        raise RuntimeError(f'This host must have a {PAGE}-byte base page size')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    randomizer = random.Random(1120)
    orders = []
    for _ in range(args.repeat):
        pairings = [(engine, mode) for engine in args.engines for mode in args.modes]
        randomizer.shuffle(pairings)
        orders.append(pairings)
    hashes = {}
    for label, path in (('binary', binary), ('guest_kernel', KERNEL), ('initramfs', RUNNER / 'probe.gz')):
        with path.open('rb') as artifact:
            hashes[f'{label}_sha256'] = hashlib.file_digest(artifact, 'sha256').hexdigest()
    metadata = {
        **hashes,
        'binary': str(binary),
        'binary_version': subprocess.check_output([binary, '--version'], text=True).splitlines()[0],
        'revision': args.revision,
        'host_kernel': os.uname().release,
        'guest_kernel': str(KERNEL),
        'docker_image': IMAGE,
        'docker_image_id': subprocess.check_output(['docker', 'image', 'inspect', IMAGE, '--format', '{{.Id}}'],
                                                   text=True).strip(),
        'backing_mount': subprocess.check_output(['findmnt', '-T', output, '-n', '-o', 'SOURCE,FSTYPE,OPTIONS'],
                                                   text=True).strip(),
        'random_seed': 1120,
        'orders': orders,
    }
    (output / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    with tempfile.TemporaryDirectory(prefix='scratch-', dir=HERE) as scratch, (output / 'runs.jsonl').open('w') as results:
        scratch = Path(scratch)
        frozen_binary = scratch / 'firecracker-under-test'
        shutil.copy2(binary, frozen_binary)
        with frozen_binary.open('rb') as artifact:
            if hashlib.file_digest(artifact, 'sha256').hexdigest() != hashes['binary_sha256']:
                raise RuntimeError('Binary changed while setting up the benchmark')
        for run, pairings in enumerate(orders, 1):
            for engine, mode in pairings:
                tag = f'{run:02d}-{engine.lower()}-{mode}'
                result = run_once(frozen_binary, scratch, output, tag, engine, args.field, mode == 'direct')
                line = json.dumps(result)
                results.write(line + '\n')
                results.flush()
                print(tag, result['guest_rates'], result['page_cache_resident_bytes'], flush=True)


if __name__ == '__main__':
    main()
