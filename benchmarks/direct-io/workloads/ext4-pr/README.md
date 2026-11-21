# Firecracker direct I/O measurement method

Each trial boots a new KVM guest with one vCPU, 384 MiB RAM and one PCI virtio-block queue. Each run uses a fresh, fully allocated 320 MiB RAW file on the local NVMe ext4 filesystem. The virtual logical block is 4 KiB, and the drive uses Writeback with no rate limiter. All guest requests use Linux AIO and a block-device FD opened with `O_DIRECT`. The same release binary runs all four combinations of the Firecracker setting and I/O engine. The runner randomizes the four combinations within each repetition using a fixed seed.

1. A 128 MiB read uses 1 MiB requests submitted in groups of 16. The host issues `POSIX_FADV_DONTNEED` on the disposable backing file before this phase and records actual residency using `mincore`.
2. An immediate repeat reads the same data. The host samples residency and `/proc/<pid>/io` between the reads while the guest waits on its serial console.
3. A 128 MiB sequential write uses the same request size and queue depth on a different region. The timed phase ends after guest `fsync` returns.
4. A 64 MiB mixed phase visits 16,384 unique, shuffled 4 KiB pages in 16-request batches split evenly between reads and writes. The total IOPS includes the final guest `fsync`.

The sizes in the tables refer to guest Linux AIO requests. The guest kernel may split a request into multiple virtio operations. The guest measures latency from the beginning of each submission batch to the userspace collection of each completion. Per-operation p99 excludes the final flush. The aggregate write and mixed rates include it.

Each page has a deterministic generation and position marker. The guest compares all read data byte for byte after timing each phase. After the VM stops, the host evicts its cached pages and compares the SHA-256 of the entire file with the expected image calculated independently in Python. The runner captures the API drive configuration, host backing FD flags, per-file direct I/O alignment, guest serial output and Firecracker's block metrics. It validates the expected write byte count, two target-device flushes and zero device execution errors.

The host residency numbers describe Linux's page cache for this file. Process storage counters describe I/O charged to the VMM process by Linux. SSD firmware cache, physical extent placement and other workstation activity can affect timings. The small working set intentionally fits the host page cache and has few outstanding requests. These runs describe this setup and workload.

Set `PROBE_DIR` to an empty working directory and `GUEST_KERNEL` to a compatible Firecracker Linux kernel image. Run `./make-guest.sh "$PROBE_DIR"`, then `python3 run.py FIRECRACKER OUTPUT_DIRECTORY --revision COMMIT`. The host needs Docker and KVM. Use `python3 summarize.py OUTPUT_DIRECTORY` to summarize the results.
