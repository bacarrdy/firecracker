# Workloads

## Local block-device runs

The guest used an ext4 file on a dedicated thin volume. The local table contains four trials, using seeds 61441 through 61444. The older builds ran in earlier windows on the same host. The exact upstream PR executable and the b22 build with the write fixes were interleaved on the same volume. The CSVs identify the executable for every observation. Different working sets, queue depths, hardware or cache states can change the result. The raw table records temperature, other I/O, volume bytes, rates, and exclusion reasons for each trial.

In a test guest, set `TEST_FILE` to a prefilled, disposable 4 GiB regular file and set `FIO` to your fio executable. Run, for example, `python3 local-guest.py mix8 61441`. These workloads overwrite the test file. The workload parameters and polling boundaries match the runner used for the results. First reads started with host-cache eviction scoped to the backing volume. Repeat reads kept the previous read's cache state. The guest used direct I/O in both cases. The physical volume counters report how many bytes still reached the backing device.

The QD32/QD16 fio rows include reads, writes and random mixes. Pre-fsync write rates divide transferred bytes by elapsed wall time through fio exit. Post-fsync rates include the caller's final fsync on the same file. Read-only rates use fio's own active I/O time. The QD1 workload uses 8,192 positioned 4 KiB writes, either syncing each write or syncing at the end. The accompanying [small-file KVM study](ext4-pr/README.md) uses a separate guest program and timer.

## Ceph RBD

The measurement VM has four vCPUs, 2 GiB of RAM and an additional raw RBD disk mapped through rbd-nbd on the host. The guest used Linux 6.1.190 and fio 3.41 with libaio, direct I/O and one job. The VMM ran Async with Writeback. We verified the executing binaries, the open mode for the actual data device, guest segment limits and that fio, Firecracker, the host cgroup and RBD imposed no configured I/O rate ceiling for these profiles.

Use a new disposable 1,152 MiB image. Fill its first 1,024 MiB with random data. At byte offset `1040 * 1024 * 1024`, write a 4,096-byte random marker, flush it and retain its SHA-256. Attach the image as the QA guest's data disk. Invoke `python3 rbd-guest.py DEVICE MARKER_SHA256 PROFILE SEED`. The [guest workload](rbd-guest.py) verifies the device size and marker before each job. A write or mixed profile overwrites data in the test area. Its working set is a random-prefilled 1 GiB. Each small-block phase transfers 128 MiB and each larger phase 512 MiB. The random mixes are 50/50 except for the 8 KiB 70/30 mix. The runner records the two directions independently, total I/O latency, completion latency, the fio I/O time, the time through fio exit and the time through an explicit final fsync. The latter two include command overhead. NBD counters describe requests that reached the host mapping. Ceph client, OSD and hardware caches remain part of the measured path.

We used seeds 251121 and 251122. The build order reversed on the second pass. There were at least 15 seconds between profiles, including before repeat reads. Mapping-scoped host-cache eviction preceded every profile except the repeat read. After the first 28 phases normal cluster scrubbing interrupted the session. The remaining phases ran on a freshly prepared image. See the window identifier on each [fio record](../results/ceph-uncapped-jobs.jsonl) and the [boot and storage configuration](../results/ceph-configuration.json).

## Code identity

The local table uses SHA-256 prefixes of the executables:

| Build | Executable SHA-256 prefix |
| --- | --- |
| Stock, buffered on upstream main | `23893681` |
| Two opens, buffered reads and direct writes | `93d47ab4` |
| Two opens, direct reads and writes, SG32 | `4d1253ec` |
| One open, direct reads and writes, earlier SG32 | `b22ed8ad` |
| One open, direct reads and writes, SG32 RC1 | `d7f1e4e8` |
| One open, SG32 b22 queue with write fixes | `d3a5a44b` |
| Proposed upstream executable, SG1 | `056dd409` |

The earlier Ceph SG1 bidirectional build was `7d2fd6c2`. The [source notes](../code/README.md) give Git revisions for the upstream proposal and the portable v1.17.0 comparison source.
