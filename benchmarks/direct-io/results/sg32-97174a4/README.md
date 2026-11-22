Commit `97174a436`, five runs per mode and engine on local ext4 with the same guest and raw workload as the historical ext4 table. Each cell is **rate / p99 in ms**. The mixed row reports both read and write p99. The sequential phases used 128 MiB and QD16. The write and mixed rates include the final guest fsync.

| Engine | Workload | Buffered, 32 segments | O_DIRECT, 32 segments |
| --- | --- | ---: | ---: |
| Async | Cold 1 MiB sequential read, MiB/s | 2,443.1 / 7.571 | **2,752.1** / **6.853** |
| Async | Repeat 1 MiB sequential read, MiB/s | **5,522.0** / **3.133** | 2,730.5 / 6.674 |
| Async | 1 MiB sequential write with fsync, MiB/s | 322.5 / **4.119** | **364.8** / 43.898 |
| Async | 4 KiB mixed 50/50 with fsync, IOPS | **40,999.6** / **0.409** / 0.218 | 40,756.9 / 2.267 / **0.186** |
| Sync | Cold 1 MiB sequential read, MiB/s | **2,142.9** / **8.057** | 2,094.6 / 9.729 |
| Sync | Repeat 1 MiB sequential read, MiB/s | **6,284.2** / **2.804** | 2,065.2 / 9.861 |
| Sync | 1 MiB sequential write with fsync, MiB/s | 321.7 / **4.902** | **351.3** / 52.825 |
| Sync | 4 KiB mixed 50/50 with fsync, IOPS | **19,622.9** / **0.944** / **0.944** | 15,165.9 / 2.973 / 2.973 |

The buffered runs left the full 128 MiB read range resident in the host page cache. The direct runs left none. All 20 runs passed guest readback and whole-file SHA-256. These short ext4 runs capture the cache and flush trade-off for the current code. The Ceph and fc1 results below are from the named historical builds.
