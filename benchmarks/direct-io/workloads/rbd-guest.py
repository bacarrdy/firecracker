"""Run one uncapped fio profile on a QA block device with a known identity marker."""
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import time

MIB = 1 << 20
WORKING_SET = 1024 * MIB
MARKER = WORKING_SET + 16 * MIB
SIZE = 1152 * MIB


def profiles():
    result = {}
    for kib in (4, 8, 64, 512, 1024):
        for kind in ("read", "write", "mix"):
            mode = ("rand" + kind if kib < 64 else kind) if kind != "mix" else "randrw"
            qd = 16 if kib == 8 or (kib >= 512 and kind != "mix") else 32
            result[f"{kind}{kib}"] = (mode, kib, qd, 128 if kib <= 8 else 512)
    result["repeat512"] = result["read512"]
    return result


def run():
    device, marker, case, seed = sys.argv[1:]
    args = profiles()[case]
    rw, kib, qd, amount = args
    if not stat.S_ISBLK(os.stat(device).st_mode):
        raise RuntimeError("the fio target must be a block device")
    fd = os.open(device, os.O_RDWR | os.O_CLOEXEC)
    try:
        if not stat.S_ISBLK(os.fstat(fd).st_mode):
            raise RuntimeError("the fio target changed before it could be opened")
        target = f"/proc/self/fd/{fd}"
        inherited = (fd,)
        if int(subprocess.check_output(["blockdev", "--getsize64", target], pass_fds=inherited)) != SIZE:
            raise RuntimeError("unexpected test disk size")
        if hashlib.sha256(os.pread(fd, 4096, MARKER)).hexdigest() != marker:
            raise RuntimeError("test disk identity marker does not match")
        fio = next(str(p) for p in (Path("/usr/bin/fio"), Path("/qa/bin/fio"), Path("/usr/local/bin/fio")) if p.is_file())
        flags = [fio, "--name=guest", "--filename="+target, "--size=1024M", f"--io_size={amount}M",
                 f"--rw={rw}", f"--bs={kib}k", f"--iodepth={qd}", "--numjobs=1", "--direct=1",
                 "--ioengine=libaio", "--mem=mmap", "--invalidate=0", "--norandommap=1",
                 "--refill_buffers=1", "--randrepeat=1", "--randseed="+seed,
                 "--end_fsync=0", "--fsync_on_close=0", "--lat_percentiles=1",
                 "--clat_percentiles=1", "--output-format=json", "--eta=never"]
        if rw == "randrw":
            flags += ["--rwmixread=70" if kib == 8 else "--rwmixread=50"]
        start = time.monotonic_ns()
        completed = subprocess.run(flags, check=True, capture_output=True, pass_fds=inherited)
        written = time.monotonic_ns()
        if rw in ("write", "randwrite", "randrw"):
            os.fsync(fd)
        synced = time.monotonic_ns()
        report = json.loads(completed.stdout)
        job, = report["jobs"]
        if job["error"] != 0:
            raise RuntimeError(f"fio reported error {job['error']}")
        ops = {k: {f: job[k][f] for f in ("io_bytes","total_ios","bw_bytes","iops","runtime","lat_ns","clat_ns")} for k in ("read","write") if job[k]["total_ios"]}
        if sum(d["io_bytes"] for d in ops.values()) != amount * MIB:
            raise RuntimeError("fio did not transfer the requested amount of data")
        print(json.dumps(dict(profile=case, seed=int(seed), options=job["job options"],
                  version=report["fio version"], error=job["error"], directions=ops,
                  elapsed_before_flush_ns=written-start, elapsed_after_flush_ns=synced-start,
                  fsync_ns=synced-written if rw in ("write","randwrite","randrw") else None,
                  depth=job["iodepth_level"])))
    finally:
        os.close(fd)


if __name__ == "__main__":
    run()
