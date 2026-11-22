# SG32 fio comparison

Both host modes use build `97174a4` and the same 1 GiB allocated backing file. Async and Writeback are used for all runs. Guest fio 3.43 (libaio) uses the raw /dev/vda device. There are three pairs in each guest mode and 33 phases per run. The random and mixed phases request QD32. The sequential phases request QD16. The QD1 write phases use 8 MiB, and the other phases use 256 MiB. Both host modes use the same seed within each round.

Rates for reads use fio's data-operation duration. Rates for writes and mixed jobs use the guest's monotonic clock from starting fio through its exit and the final fsync. This includes any flush on closing the guest raw device. Per-operation p99 comes from fio's total latency histogram. The CSV preserves the fio-only rates, p95, p99.9, fsync time and the share fio reported at the requested queue depth. Each run uses a file-local POSIX_FADV_DONTNEED before each phase. The guest-buffered runs also drop clean caches inside the disposable guest between phases.

A kernel status line interrupted the first guest-buffered direct run's result transport. Its seven completed records were retained, and the remaining cases used the same seed and backing from a fresh VM. The backing-file cache residency field for the recovered seventh record is empty. The paired 64 MiB cache check kept the guest block device open and sampled Buffers + Cached before and after. Those samples are in guest-buffered/cache-proof.json.

- [Guest direct I/O](guest-direct)
- [Guest page cache enabled](guest-buffered)
