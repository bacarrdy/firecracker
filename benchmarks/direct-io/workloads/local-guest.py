"""Measure the time before and after an explicit guest fsync on a fixed data file."""
import json
import math
import mmap
import os
import random
import stat
import subprocess
import sys
import time

FILE = os.environ["TEST_FILE"]
MIB = 1024**2
# fio direction, block size, queue depth, total bytes (MiB), read percentage
CASES = {
    "read4": ("randread", "4k", 32, 256, None),
    "write4": ("randwrite", "4k", 32, 1024, None),
    "mix4": ("randrw", "4k", 32, 1024, 50),
    "mix8": ("randrw", "8k", 16, 1024, 70),
    "mix64": ("randrw", "64k", 32, 2048, 50),
    "mix512": ("randrw", "512k", 32, 2048, 50),
    "read512": ("read", "512k", 16, 4096, None),
    "write1m": ("write", "1m", 16, 4096, None),
}
TXN = ("txn4_batch", "txn4_sync")
now = time.monotonic_ns

def percentile99(samples):
    return sorted(samples)[math.ceil(len(samples) * .99) - 1]

def transaction(fd, each, seed):
    rng = random.Random(seed)
    offsets = [4096 * rng.randrange(1024**2) for _ in range(8192)]
    buffer = mmap.mmap(-1, 4096 * 256)
    buffer[:] = rng.randbytes(len(buffer))
    view = memoryview(buffer)
    blocks = [view[n:n+4096] for n in range(0, len(buffer), 4096)]
    latency = []
    barrier = []
    try:
        start = now()
        for i, offset in enumerate(offsets):
            data = blocks[i % len(blocks)]
            flight = now()
            assert os.pwrite(fd, data, offset) == len(data)
            if each:
                barrier_start = now()
                os.fsync(fd)
                barrier.append(now() - barrier_start)
            latency.append(now() - flight)
        written = now()
        os.fsync(fd)
        synced = now()
    finally:
        for block in blocks:
            block.release()
        view.release()
        buffer.close()
    n = len(offsets)
    return dict(pre_ns=written-start, post_ns=synced-start, final_sync_ns=synced-written,
                per_write_fsync=each, fsync_calls=n*int(each)+1, fsync_p99_ns=percentile99(barrier) if barrier else None,
                directions={"write":dict(size=n*4096, ios=n, runtime_ns=written-start,
                                         lat_p99_ns=percentile99(latency))})

def measure(fd, case, seed):
    direction, bs, qd, size, read_mix = CASES[case]
    flags = [os.environ.get("FIO", "fio"), "--name=measured", "--filename="+FILE, "--size=4G",
             f"--io_size={size}M", f"--bs={bs}", "--rw="+direction, f"--iodepth={qd}",
             "--direct=1", "--ioengine=libaio", "--mem=mmap", "--invalidate=0",
             "--randrepeat=1", f"--randseed={seed}", "--norandommap=1", "--refill_buffers=1",
             "--end_fsync=0", "--fsync_on_close=0", "--fsync=0", "--fdatasync=0",
             "--group_reporting=1", "--lat_percentiles=1", "--output-format=json", "--eta=never"]
    if read_mix is not None:
        flags.append(f"--rwmixread={read_mix}")
    write = direction in {"write", "randwrite", "randrw"}
    start = now()
    completed = subprocess.run(flags, stdout=subprocess.PIPE, check=True)
    before_barrier = now()
    if write:
        os.fsync(fd)
    after_barrier = now() if write else before_barrier
    job, = json.loads(completed.stdout)["jobs"]
    assert job["error"] == 0
    directions = {key:dict(size=x["io_bytes"], ios=x["total_ios"], runtime_ns=x["runtime"]*10**6,
                           fio_bw_bytes=x["bw_bytes"], fio_iops=x["iops"],
                           lat_p99_ns=x["lat_ns"]["percentile"]["99.000000"])
                  for key in ("read","write") for x in [job[key]] if x["total_ios"]}
    assert sum(x["size"] for x in directions.values()) == size*MIB
    return dict(pre_ns=before_barrier-start, post_ns=after_barrier-start,
                final_sync_ns=after_barrier-before_barrier, per_write_fsync=False, fsync_calls=int(write),
                directions=directions)

if __name__ == "__main__":
    case, seed = sys.argv[1], int(sys.argv[2])
    assert case in CASES or case in TXN
    flags = os.O_NOFOLLOW | (os.O_RDWR if case not in ("read4","read512") else os.O_RDONLY)
    if case in TXN:
        flags |= os.O_DIRECT
    fd = os.open(FILE, flags)
    try:
        info = os.fstat(fd)
        assert stat.S_ISREG(info.st_mode) and info.st_size == 4*1024**3
        result = transaction(fd, case=="txn4_sync", seed) if case in TXN else measure(fd,case,seed)
        print(json.dumps(result))
    finally:
        os.close(fd)
