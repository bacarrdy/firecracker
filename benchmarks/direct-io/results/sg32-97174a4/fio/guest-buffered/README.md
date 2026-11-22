## Guest page cache enabled

Commit `97174a4`, Async and Writeback. Both host modes negotiated 32 segments. Guest fio used `direct=0` against the same disposable block device backed by a 1 GiB file on ext4, on a consumer Kingston KC2500 NVMe. Three rounds per mode, with the mode order reversed in round two. The main phases transfer 256 MiB each.

Cells are **MiB/s / IOPS / p99 ms**. Write and mixed rates use elapsed time from launching fio through the final guest fsync, including close-time writeback. The p99 is for the individual data operation. Both directions in a mixed phase use the same throughput clock.

Queue depths are fio settings. The Linux libaio buffered path can behave synchronously. The raw results include the observed fio I/O depth distributions.

### Random reads and writes · QD32

| Block size | Buffered read | Direct read | Buffered write | Direct write |
| --- | ---: | ---: | ---: | ---: |
| 4 KiB | 28.8 / 7,374 / 5.079 | **30.2** / **7,722** / **4.817** | 139.6 / 35,743 / 1.942 | **223.5** / **57,216** / **0.799** |
| 8 KiB | 53.5 / 6,851 / 5.538 | **57.3** / **7,331** / **5.145** | 157.4 / 20,152 / **1.761** | **222.6** / **28,496** / 6.914 |
| 16 KiB | 91.4 / 5,849 / 6.455 | **101.4** / **6,491** / **5.800** | 158.5 / 10,143 / **3.031** | **203.8** / **13,043** / 9.765 |
| 32 KiB | 145.4 / 4,652 / 8.028 | **150.4** / **4,813** / **7.569** | 188.2 / 6,023 / **3.752** | **222.9** / **7,134** / 9.503 |
| 64 KiB | 210.7 / 3,371 / 11.076 | **231.3** / **3,700** / **10.027** | **188.3** / **3,013** / **3.195** | 134.7 / 2,155 / 29.229 |
| 128 KiB | 299.1 / 2,393 / 15.139 | **327.4** / **2,619** / **13.959** | 188.5 / 1,508 / **5.865** | **223.4** / **1,787** / 17.957 |
| 256 KiB | 384.4 / 1,538 / 26.608 | **480.3** / **1,921** / **22.675** | 188.4 / 754 / **9.765** | **223.2** / **893** / 31.064 |
| 512 KiB | 532.2 / 1,064 / 43.778 | **664.9** / **1,330** / **37.487** | 189.1 / 378 / **14.483** | **223.1** / **446** / 55.312 |
| 1 MiB | 572.7 / 573 / 77.070 | **744.2** / **744** / **59.507** | 188.2 / 188 / **36.438** | **223.0** / **223** / 101.188 |

### Mixed random I/O · QD32

| Block size | Buffered read | Direct read | Buffered write | Direct write |
| --- | ---: | ---: | ---: | ---: |
| 4 KiB (50/50) | 24.3 / 6,222 / 3.391 | **26.0** / **6,659** / **3.195** | 24.3 / 6,227 / 3.260 | **26.3** / **6,729** / **3.031** |
| 8 KiB (70/30) | 45.6 / 5,836 / 4.489 | **47.7** / **6,105** / **4.293** | 19.1 / 2,448 / 4.358 | **20.3** / **2,602** / **4.145** |
| 16 KiB (50/50) | 56.9 / 3,642 / **5.210** | **59.7** / **3,820** / 25.559 | 56.7 / 3,630 / **5.145** | **59.5** / **3,807** / 25.559 |
| 32 KiB (50/50) | 71.4 / 2,285 / **6.324** | **76.9** / **2,461** / 27.656 | 72.9 / 2,332 / **6.324** | **77.3** / **2,474** / 27.656 |
| 64 KiB (50/50) | **84.2** / **1,348** / **9.110** | 67.1 / 1,074 / 62.128 | **85.2** / **1,363** / **8.978** | 68.1 / 1,090 / 70.779 |
| 128 KiB (50/50) | 100.4 / 804 / **11.993** | **109.6** / **877** / 30.540 | 104.2 / 834 / **11.731** | **113.7** / **910** / 30.278 |
| 256 KiB (50/50) | **112.8** / **451** / **16.450** | 110.5 / 442 / 34.865 | **116.2** / **465** / **16.187** | 113.1 / 452 / 34.865 |
| 512 KiB (50/50) | 125.1 / 250 / **23.724** | **140.2** / **280** / 58.458 | 130.1 / 260 / **23.462** | **145.8** / **292** / 57.934 |
| 1 MiB (50/50) | 127.8 / 128 / **39.059** | **142.9** / **143** / 93.848 | 127.8 / 128 / **40.108** | **142.9** / **143** / 93.848 |

### Sequential reads and writes · QD16

| Block size | Buffered read | Direct read | Buffered write | Direct write |
| --- | ---: | ---: | ---: | ---: |
| 512 KiB | 988.4 / 1,977 / 9.765 | **1,292.9** / **2,586** / **8.847** | 189.5 / 379 / **12.124** | **223.5** / **447** / 31.326 |
| 1 MiB | 1,414.4 / 1,414 / 13.304 | **2,226.1** / **2,226** / **9.372** | 189.1 / 189 / **15.139** | **223.4** / **223** / 55.312 |

### 4 KiB writes, QD1

| fsync | Buffered write | Direct write | Final fsync · buffered / direct, ms |
| --- | ---: | ---: | ---: |
| One final fsync | 19.1 / 4,893 / 0.015 | **20.2** / **5,171** / **0.014** | 27.212 / **5.556** |
| fsync after each write | 0.6 / 153 / 0.042 | 0.6 / **165** / **0.038** | 5.895 / **5.703** |

After the 256 MiB sequential read, the host backing-file cache held 258.0 MiB with buffered I/O and 0.0 MiB with direct I/O. In a 64 MiB read with the guest block device kept open, guest Buffers + Cached grew by 65.0 and 65.0 MiB, respectively.
