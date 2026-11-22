# Full fio matrix on SG32

Commit `97174a4` on Linux 7.0.0-31 with ext4 and a consumer Kingston KC2500.
Guest Linux 6.1.191 with one vCPU. Firecracker used Async and Writeback in all runs.
The table covers 396 phases from 12 VM boots. Queue depths are fio settings and workload suffixes give the block size in KiB.
Linux libaio can complete buffered guest operations at submission, so the
effective queue depth can differ from the requested depth.

Each data cell gives **active fio MiB/s / IOPS / data-operation p99 in ms**.
These are per-case medians of three trials. Buffered writes can complete
while data is still in the host page cache. The separately timed final guest
fsync is shown for writes and mixes. The QD1 cases use either one final flush or a sync per write.
Per-operation sync is part of the fio job time when configured inside fio.

The outer command clock reported the guest command's exit with coarse
polling. The tables use fio's own operation time and the separate final
flush measurement, so benchmark harness polling does not set the I/O rate.

[All samples, including the fio parameters](fio-samples.csv).

## Guest page cache bypass

| Workload | Direction | Host buffered | Host direct | Final fsync, buffered / direct (ms) |
| --- | --- | ---: | ---: | ---: |
| randread-4 | read | 301.2 / 77,101 / 0.750 | 270.0 / 69,131 / 1.974 | — |
| randwrite-4 | write | 317.2 / 81,209 / 0.528 | 360.6 / 92,304 / 0.840 | 759.78 / 6.80 |
| randrw-4 | read | 152.9 / 39,131 / 0.684 | 118.1 / 30,231 / 2.769 | 907.39 / 6.14 |
| randrw-4 | write | 154.5 / 39,544 / 0.578 | 119.6 / 30,619 / 0.412 | 907.39 / 6.14 |
| randread-8 | read | 416.3 / 53,281 / 1.384 | 424.5 / 54,342 / 1.188 | — |
| randwrite-8 | write | 508.9 / 65,145 / 0.823 | 377.6 / 48,330 / 1.417 | 724.43 / 7.00 |
| randrw-8 | read | 401.7 / 51,416 / 0.954 | 252.5 / 32,314 / 2.703 | 218.46 / 6.64 |
| randrw-8 | write | 168.5 / 21,563 / 0.864 | 107.6 / 13,774 / 0.766 | 218.46 / 6.64 |
| randread-16 | read | 635.2 / 40,655 / 1.516 | 684.5 / 43,807 / 1.319 | — |
| randwrite-16 | write | 917.6 / 58,724 / 0.881 | 361.1 / 23,109 / 2.056 | 726.10 / 5.49 |
| randrw-16 | read | 560.9 / 35,895 / 0.840 | 235.7 / 15,083 / 3.031 | 368.64 / 5.64 |
| randrw-16 | write | 557.0 / 35,651 / 0.494 | 234.1 / 14,980 / 0.872 | 368.64 / 5.64 |
| randread-32 | read | 831.2 / 26,597 / 2.056 | 882.8 / 28,248 / 1.729 | — |
| randwrite-32 | write | 1,286.4 / 41,166 / 1.106 | 361.6 / 11,571 / 4.080 | 720.94 / 5.60 |
| randrw-32 | read | 864.3 / 27,658 / 1.073 | 272.8 / 8,731 / 5.407 | 366.16 / 5.77 |
| randrw-32 | write | 853.8 / 27,322 / 0.766 | 269.5 / 8,625 / 0.864 | 366.16 / 5.77 |
| randread-64 | read | 1,103.4 / 17,655 / 2.900 | 1,163.6 / 18,618 / 2.376 | — |
| randwrite-64 | write | 1,939.4 / 31,030 / 1.368 | 362.1 / 5,793 / 9.896 | 718.86 / 5.61 |
| randrw-64 | read | 1,377.0 / 22,032 / 1.597 | 269.3 / 4,309 / 11.731 | 371.63 / 5.67 |
| randrw-64 | write | 1,375.7 / 22,011 / 0.897 | 274.7 / 4,395 / 2.376 | 371.63 / 5.67 |
| randread-128 | read | 1,242.7 / 9,942 / 5.407 | 1,242.7 / 9,942 / 4.686 | — |
| randwrite-128 | write | 2,415.1 / 19,321 / 1.958 | 365.2 / 2,922 / 19.530 | 713.43 / 5.64 |
| randrw-128 | read | 1,371.0 / 10,968 / 2.867 | 259.0 / 2,072 / 20.316 | 364.69 / 5.78 |
| randrw-128 | write | 1,381.7 / 11,054 / 2.179 | 267.2 / 2,137 / 9.634 | 364.69 / 5.78 |
| randread-256 | read | 1,062.2 / 4,249 / 11.469 | 1,446.3 / 5,785 / 8.978 | — |
| randwrite-256 | write | 3,555.6 / 14,222 / 2.441 | 365.2 / 1,461 / 42.729 | 713.62 / 5.58 |
| randrw-256 | read | 1,360.2 / 5,441 / 8.159 | 263.5 / 1,054 / 31.326 | 359.17 / 5.64 |
| randrw-256 | write | 1,392.5 / 5,570 / 3.785 | 276.9 / 1,108 / 26.083 | 359.17 / 5.64 |
| randread-512 | read | 1,446.3 / 2,893 / 15.794 | 1,662.3 / 3,325 / 17.957 | — |
| randwrite-512 | write | 3,506.8 / 7,014 / 5.407 | 365.7 / 731 / 80.216 | 713.08 / 5.67 |
| randrw-512 | read | 1,767.6 / 3,535 / 6.980 | 281.4 / 563 / 55.837 | 371.64 / 5.57 |
| randrw-512 | write | 1,838.0 / 3,676 / 5.800 | 292.6 / 585 / 49.021 | 371.64 / 5.57 |
| randread-1024 | read | 1,383.8 / 1,384 / 32.899 | 1,630.6 / 1,631 / 36.438 | — |
| randwrite-1024 | write | 3,878.8 / 3,879 / 11.862 | 363.1 / 363 / 198.181 | 711.75 / 7.20 |
| randrw-1024 | read | 1,969.2 / 1,969 / 15.008 | 262.1 / 262 / 127.402 | 362.50 / 5.64 |
| randrw-1024 | write | 1,969.2 / 1,969 / 12.386 | 294.0 / 294 / 143.655 | 362.50 / 5.64 |
| read-512 | read | 1,340.3 / 2,681 / 8.716 | 1,398.9 / 2,798 / 8.847 | — |
| write-512 | write | 3,657.1 / 7,314 / 2.736 | 363.6 / 727 / 39.584 | 714.50 / 5.74 |
| read-1024 | read | 2,206.9 / 2,207 / 16.908 | 2,612.2 / 2,612 / 9.765 | — |
| write-1024 | write | 4,129.0 / 4,129 / 6.062 | 195.1 / 195 / 208.667 | 715.21 / 8.38 |
| qd1-4-last | write | 94.1 / 24,094 / 0.066 | 58.8 / 15,059 / 0.113 | 37.56 / 8.19 |
| qd1-4-each | write | 0.6 / 160 / 0.461 | 0.7 / 168 / 6.259 | 7.32 / 7.15 |

## Guest page cache enabled

| Workload | Direction | Host buffered | Host direct | Final fsync, buffered / direct (ms) |
| --- | --- | ---: | ---: | ---: |
| randread-4 | read | 28.8 / 7,374 / 5.079 | 30.2 / 7,722 / 4.817 | — |
| randwrite-4 | write | 321.2 / 82,228 / 1.942 | 387.9 / 99,297 / 0.799 | 690.82 / 5.58 |
| randrw-4 | read | 28.2 / 7,208 / 3.391 | 28.9 / 7,395 / 3.195 | 345.80 / 5.65 |
| randrw-4 | write | 28.4 / 7,259 / 3.260 | 28.8 / 7,381 / 3.031 | 345.80 / 5.65 |
| randread-8 | read | 53.5 / 6,851 / 5.538 | 57.3 / 7,331 / 5.145 | — |
| randwrite-8 | write | 493.3 / 63,137 / 1.761 | 434.6 / 55,633 / 6.914 | 722.45 / 6.65 |
| randrw-8 | read | 53.7 / 6,879 / 4.489 | 54.1 / 6,929 / 4.293 | 206.20 / 10.11 |
| randrw-8 | write | 22.5 / 2,885 / 4.358 | 23.1 / 2,953 / 4.145 | 206.20 / 10.11 |
| randread-16 | read | 91.4 / 5,849 / 6.455 | 101.4 / 6,491 / 5.800 | — |
| randwrite-16 | write | 709.1 / 45,385 / 3.031 | 353.6 / 22,630 / 9.765 | 731.83 / 5.82 |
| randrw-16 | read | 86.1 / 5,507 / 5.210 | 76.2 / 4,876 / 25.559 | 361.45 / 5.72 |
| randrw-16 | write | 85.8 / 5,489 / 5.145 | 75.9 / 4,859 / 25.559 | 361.45 / 5.72 |
| randread-32 | read | 145.4 / 4,652 / 8.028 | 150.4 / 4,813 / 7.569 | — |
| randwrite-32 | write | 800.0 / 25,600 / 3.752 | 417.6 / 13,364 / 9.503 | 719.41 / 5.84 |
| randrw-32 | read | 128.0 / 4,096 / 6.324 | 111.6 / 3,573 / 27.656 | 384.81 / 6.14 |
| randrw-32 | write | 128.5 / 4,112 / 6.324 | 111.2 / 3,559 / 27.656 | 384.81 / 6.14 |
| randread-64 | read | 210.7 / 3,371 / 11.076 | 231.3 / 3,700 / 10.027 | — |
| randwrite-64 | write | 1,117.9 / 17,886 / 3.195 | 225.4 / 3,606 / 29.229 | 718.66 / 9.06 |
| randrw-64 | read | 197.3 / 3,156 / 9.110 | 96.0 / 1,537 / 62.128 | 368.69 / 8.74 |
| randrw-64 | write | 199.4 / 3,191 / 8.978 | 97.2 / 1,556 / 70.779 | 368.69 / 8.74 |
| randread-128 | read | 299.1 / 2,393 / 15.139 | 327.4 / 2,619 / 13.959 | — |
| randwrite-128 | write | 1,224.9 / 9,799 / 5.865 | 430.3 / 3,442 / 17.957 | 717.24 / 5.82 |
| randrw-128 | read | 276.1 / 2,209 / 11.993 | 192.7 / 1,541 / 30.540 | 360.23 / 5.77 |
| randrw-128 | write | 286.5 / 2,292 / 11.731 | 200.0 / 1,600 / 30.278 | 360.23 / 5.77 |
| randread-256 | read | 384.4 / 1,538 / 26.608 | 480.3 / 1,921 / 22.675 | — |
| randwrite-256 | write | 1,219.0 / 4,876 / 9.765 | 431.7 / 1,727 / 31.064 | 717.32 / 5.63 |
| randrw-256 | read | 322.7 / 1,291 / 16.450 | 226.3 / 905 / 34.865 | 362.25 / 5.70 |
| randrw-256 | write | 333.8 / 1,335 / 16.187 | 233.7 / 935 / 34.865 | 362.25 / 5.70 |
| randread-512 | read | 532.2 / 1,064 / 43.778 | 664.9 / 1,330 / 37.487 | — |
| randwrite-512 | write | 1,333.3 / 2,667 / 14.483 | 428.1 / 856 / 55.312 | 713.22 / 5.74 |
| randrw-512 | read | 403.8 / 808 / 23.724 | 242.7 / 485 / 58.458 | 361.14 / 5.57 |
| randrw-512 | write | 422.3 / 845 / 23.462 | 252.4 / 505 / 57.934 | 361.14 / 5.57 |
| randread-1024 | read | 572.7 / 573 / 77.070 | 744.2 / 744 / 59.507 | — |
| randwrite-1024 | write | 1,127.8 / 1,128 / 36.438 | 420.4 / 420 / 101.188 | 717.78 / 5.72 |
| randrw-1024 | read | 455.9 / 456 / 39.059 | 263.9 / 264 / 93.848 | 361.65 / 5.60 |
| randrw-1024 | write | 468.9 / 469 / 40.108 | 269.2 / 269 / 93.848 | 361.65 / 5.60 |
| read-512 | read | 988.4 / 1,977 / 9.765 | 1,292.9 / 2,586 / 8.847 | — |
| write-512 | write | 1,080.2 / 2,160 / 12.124 | 431.0 / 862 / 31.326 | 710.70 / 5.60 |
| read-1024 | read | 1,414.4 / 1,414 / 13.304 | 2,226.1 / 2,226 / 9.372 | — |
| write-1024 | write | 1,230.8 / 1,231 / 15.139 | 428.8 / 429 / 55.312 | 711.54 / 5.57 |
| qd1-4-last | write | 615.4 / 157,538 / 0.015 | 666.7 / 170,667 / 0.014 | 27.21 / 5.56 |
| qd1-4-each | write | 0.6 / 156 / 0.042 | 0.7 / 169 / 0.038 | 5.89 / 5.70 |
