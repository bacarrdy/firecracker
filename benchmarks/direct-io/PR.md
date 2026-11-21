## Changes

Add an optional `direct_io` setting for raw virtio-blk drives. Both reads and writes use the same `O_DIRECT` file description. Buffered I/O remains the default.

## Reason

#5910 proposed a second open for direct writes and kept reads buffered. The team declined that version. During review, direct I/O in both directions was suggested as an alternative. Would you consider this single-open option for Sync and Async? [Earlier discussion](https://github.com/firecracker-microvm/firecracker/pull/5910#issuecomment-4629799026) · [Closing decision](https://github.com/firecracker-microvm/firecracker/pull/5910#issuecomment-6954909364)

The option keeps guest disk data out of the host page cache. After a 128 MiB read from an ext4 backing file, the whole read range was resident with buffered I/O and none of it was resident with direct I/O. We saw the same result in five runs per setting for both engines.

On our consumer NVMe the proposed PR build wrote 4 GiB in 1 MiB requests at 1,602 MiB/s before the final guest fsync and 1,597 MiB/s including it. Stock reached 1,559 and 321 MiB/s in an earlier session on the same device. First 4 KiB random reads were slower with the PR build at 72,458 IOPS versus 77,696 on stock. Both reached the backing volume.

Our revised two-open build reached 2,077 MiB/s for the same 1 MiB write including fsync, with 10.9 ms write p99. The proposed build had 15.8 ms write p99. We have kept the two-open results and the downstream 32-segment results in the tables below.

The proposed main commit is `29c04bb6c`. We measured that exact executable with the option off and on. Our later two-open SG32 build already used direct I/O for reads and writes. Its other file description handled metadata, flush and discard. Our one-open SG32 builds preserve its scalar, vectored and alignment-repair paths, and use the same open for flush and discard. They include scatter/gather and guest-memory changes beyond this PR.

## Exact PR executable on an ext4 backing file

Five runs per configuration on a workstation, with 128 MiB per sequential phase. The mixed and write rates include the final guest fsync. Latency is measured from the start of each 16-request batch. These short tests have a wide spread. Bold marks the best displayed value for each engine.

| Guest workload | Unit | Async buffered | Async direct | Sync buffered | Sync direct |
| --- | --- | --- | --- | --- | --- |
| 1 MiB sequential read, first | MiB/s | 1,770.2 | **2,356.2** | **1,667.0** | 1,550.2 |
| 1 MiB sequential write, final fsync | MiB/s | 326.0 | **350.4** | 308.9 | **347.7** |
| 4 KiB mixed 50/50, final fsync | IOPS | **33,141** | 28,825 | **16,090** | 12,190 |
| 1 MiB first read, batch p99 | µs | 9,973 | **8,497** | **14,468** | 25,723 |
| 1 MiB write, batch p99 | µs | **6,628** | 51,087 | **10,475** | 49,499 |
| 4 KiB mixed read, batch p99 | µs | **809** | 2,528 | **2,144** | 3,978 |
| 4 KiB mixed write, batch p99 | µs | 642 | **542** | **2,144** | 3,978 |

<details>
<summary>Workstation repeated-read cache check</summary>

| Guest workload | Unit | Async buffered | Async direct | Sync buffered | Sync direct |
| --- | --- | --- | --- | --- | --- |
| 1 MiB sequential read, repeat | MiB/s | **4,165.4** | 2,471.1 | **4,305.3** | 1,415.8 |
| 1 MiB repeat read, batch p99 | µs | **5,999** | 9,092 | **6,277** | 27,186 |

</details>

## Ceph RBD

Guest fio on an additional raw RBD disk mapped with rbd-nbd. The first two columns use the same PR executable. The last two use the downstream 32-segment builds. All four use Async and Writeback, with direct I/O in the guest. We ran two seeds and reversed build order on the second pass. Each cell gives **MiB/s / IOPS / guest I/O p99 in ms**. Reads use active fio time. Writes and both directions of a mix include command overhead and the final fsync in their throughput clock. The p99 is for the individual data operation.

| Workload | Direction | PR buffered | PR direct | SG32 RC1 | SG32 b22 + fixes |
| --- | --- | --- | --- | --- | --- |
| 4 KiB random read, QD32 | read | 155.5 / 39,816 / **1.499** | **159.6** / **40,866** / 1.532 | 154.2 / 39,476 / 1.622 | 157.4 / 40,284 / 1.573 |
| 4 KiB random write, QD32 | write | 60.8 / 15,574 / **0.623** | 75.9 / 19,442 / 5.702 | 76.6 / 19,615 / 4.071 | **82.9** / **21,210** / 4.039 |
| 4 KiB random mix 50/50, QD32 | read | 40.6 / 10,399 / **2.068** | **50.1** / **12,832** / 5.898 | 47.6 / 12,185 / 5.538 | 49.9 / 12,767 / 5.865 |
| 4 KiB random mix 50/50, QD32 | write | 40.7 / 10,422 / **0.455** | **50.2** / **12,861** / 0.889 | 47.7 / 12,220 / 0.901 | 50.0 / 12,798 / 0.901 |
| 8 KiB random read, QD16 | read | 171.5 / 21,947 / 1.303 | 178.3 / 22,818 / 1.524 | **198.8** / **25,445** / **1.059** | 188.2 / 24,087 / 1.409 |
| 8 KiB random write, QD16 | write | 90.0 / 11,519 / **0.540** | 104.7 / 13,407 / 2.933 | **109.7** / **14,041** / 2.036 | **109.7** / 14,039 / 1.827 |
| 8 KiB random mix 70/30, QD16 | read | 63.3 / 8,104 / **1.393** | **76.7** / **9,814** / 2.556 | 76.0 / 9,727 / 2.294 | 76.6 / 9,809 / 3.224 |
| 8 KiB random mix 70/30, QD16 | write | 27.3 / 3,496 / **0.307** | **33.1** / **4,233** / 0.426 | 32.8 / 4,196 / 0.438 | **33.1** / 4,231 / 0.365 |
| 64 KiB sequential read, QD32 | read | **401.9** / **6,430** / **10.945** | 231.2 / 3,699 / 24.773 | 283.2 / 4,531 / 14.123 | 252.3 / 4,037 / 17.695 |
| 64 KiB sequential write, QD32 | write | 207.0 / 3,312 / **4.874** | 244.6 / 3,914 / 40.042 | 357.6 / 5,721 / 15.827 | **361.4** / **5,782** / 9.699 |
| 64 KiB random mix 50/50, QD32 | read | 194.6 / 3,114 / **5.562** | 112.0 / 1,792 / 14.025 | **247.9** / **3,967** / 9.699 | 217.7 / 3,483 / 8.159 |
| 64 KiB random mix 50/50, QD32 | write | 194.9 / 3,118 / **6.517** | 112.2 / 1,795 / 21.103 | **248.3** / **3,972** / 7.979 | 218.0 / 3,488 / 7.766 |
| 512 KiB sequential read, QD16 | read | 516.3 / 1,033 / 33.489 | 250.7 / 501 / 86.180 | 302.3 / 605 / 47.186 | **1,356.6** / **2,713** / **14.025** |
| 512 KiB sequential write, QD16 | write | 298.2 / 596 / **7.291** | 230.5 / 461 / 138.674 | 380.0 / 760 / 52.691 | **400.1** / **800** / 45.285 |
| 512 KiB random mix 50/50, QD32 | read | 239.6 / 479 / **15.827** | 224.3 / 449 / 156.500 | **247.6** / **495** / 117.441 | 185.1 / 370 / 407.372 |
| 512 KiB random mix 50/50, QD32 | write | 243.4 / 487 / **12.255** | 228.0 / 456 / 151.257 | **251.6** / **503** / 112.198 | 188.2 / 376 / 277.348 |
| 1 MiB sequential read, QD16 | read | 651.0 / 651 / 47.907 | 652.3 / 652 / 85.393 | 1,666.4 / 1,666 / 22.249 | **1,769.8** / **1,770** / **16.613** |
| 1 MiB sequential write, QD16 | write | 270.2 / 270 / **17.957** | 287.2 / 287 / 125.043 | 438.9 / **439** / 71.959 | **439.1** / **439** / 124.518 |
| 1 MiB random mix 50/50, QD32 | read | 219.4 / 219 / **37.290** | 298.9 / **299** / 109.314 | 270.7 / 271 / 94.896 | **299.2** / **299** / 83.362 |
| 1 MiB random mix 50/50, QD32 | write | 216.8 / 217 / **39.846** | 293.5 / **294** / 104.727 | 268.8 / 269 / 98.042 | **293.8** / **294** / 81.789 |

On the PR executable, buffered / direct reached 39,816 / 40,866 IOPS on the first 4 KiB read. The 1 MiB write reached 270.2 / 287.2 MiB/s through fsync, with write p99 of 18.0 / 125.0 ms. In the 512 KiB mix the write direction reached 243.4 / 228.0 MiB/s with 12.3 / 151.3 ms p99.

On the downstream b22 + fixes build the two 1 MiB write p99s were 74 and 175 ms, with a final fsync under 1 ms. The upstream buffered runs had 18 ms write p99 and final fsyncs of 1,024 and 937 ms. All four wrote 512 MiB to the NBD device. In the 512 KiB mix, both SG32 direct builds issued almost identical backing reads and writes. At the same two seeds, write p99 was 126 and 98 ms on RC1 and 354 and 200 ms on b22 + fixes. The guest recorded completion latency. We did not trace individual requests through the host and the cluster.

<details>
<summary>Ceph writes and mixes before and after fsync</summary>

Each cell gives **MiB/s (IOPS) before fsync / MiB/s (IOPS) including fsync / final fsync in ms**. Both throughput clocks include fio startup and exit. Both directions in a mixed row use the same clock.

| Workload | Direction | PR buffered | PR direct | SG32 RC1 | SG32 b22 + fixes |
| --- | --- | --- | --- | --- | --- |
| 4 KiB random write, QD32 | write | **139.1** (**35,608**) / 60.8 (15,574) / 1,184.56 | 76.0 (19,450) / 75.9 (19,442) / 0.68 | 76.6 (19,621) / 76.6 (19,615) / 0.54 | 82.9 (21,216) / **82.9** (**21,210**) / **0.39** |
| 4 KiB random mix 50/50, QD32 | read | **69.3** (**17,734**) / 40.6 (10,399) / 650.99 | 50.1 (12,836) / **50.1** (**12,832**) / 0.44 | 47.6 (12,189) / 47.6 (12,185) / **0.40** | 49.9 (12,771) / 49.9 (12,767) / **0.40** |
| 4 KiB random mix 50/50, QD32 | write | **69.4** (**17,773**) / 40.7 (10,422) / 650.99 | 50.3 (12,866) / **50.2** (**12,861**) / 0.44 | 47.8 (12,224) / 47.7 (12,220) / **0.40** | 50.0 (12,802) / 50.0 (12,798) / **0.40** |
| 8 KiB random write, QD16 | write | **165.3** (**21,155**) / 90.0 (11,519) / 647.83 | 104.8 (13,413) / 104.7 (13,407) / **0.48** | 109.8 (14,049) / **109.7** (**14,041**) / 0.63 | 109.7 (14,047) / **109.7** (14,039) / 0.66 |
| 8 KiB random mix 70/30, QD16 | read | **76.7** (9,816) / 63.3 (8,104) / 246.39 | **76.7** (**9,818**) / **76.7** (**9,814**) / 0.49 | 76.0 (9,731) / 76.0 (9,727) / **0.48** | **76.7** (9,813) / 76.6 (9,809) / 0.50 |
| 8 KiB random mix 70/30, QD16 | write | **33.1** (4,234) / 27.3 (3,496) / 246.39 | **33.1** (**4,235**) / **33.1** (**4,233**) / 0.49 | 32.8 (4,197) / 32.8 (4,196) / **0.48** | **33.1** (4,233) / **33.1** (4,231) / 0.50 |
| 64 KiB sequential write, QD32 | write | **370.9** (**5,935**) / 207.0 (3,312) / 1,092.58 | 245.6 (3,930) / 244.6 (3,914) / 14.26 | 358.3 (5,734) / 357.6 (5,721) / 3.11 | 361.5 (5,784) / **361.4** (**5,782**) / **0.54** |
| 64 KiB random mix 50/50, QD32 | read | **291.9** (**4,670**) / 194.6 (3,114) / 415.37 | 112.0 (1,792) / 112.0 (1,792) / 0.48 | 248.0 (3,969) / **247.9** (**3,967**) / **0.42** | 217.8 (3,485) / 217.7 (3,483) / 0.46 |
| 64 KiB random mix 50/50, QD32 | write | **292.2** (**4,676**) / 194.9 (3,118) / 415.37 | 112.2 (1,795) / 112.2 (1,795) / 0.48 | 248.4 (3,974) / **248.3** (**3,972**) / **0.42** | 218.1 (3,490) / 218.0 (3,488) / 0.46 |
| 512 KiB sequential write, QD16 | write | **664.1** (**1,328**) / 298.2 (596) / 946.60 | 230.5 (461) / 230.5 (461) / **0.44** | 380.2 (760) / 380.0 (760) / 0.79 | 400.3 (801) / **400.1** (**800**) / 0.48 |
| 512 KiB random mix 50/50, QD32 | read | **381.5** (**763**) / 239.6 (479) / 394.15 | 224.4 (449) / 224.3 (449) / **0.49** | 247.7 (495) / **247.6** (**495**) / 0.55 | 185.2 (370) / 185.1 (370) / 0.58 |
| 512 KiB random mix 50/50, QD32 | write | **387.5** (**775**) / 243.4 (487) / 394.15 | 228.1 (456) / 228.0 (456) / **0.49** | 251.7 (503) / **251.6** (**503**) / 0.55 | 188.3 (377) / 188.2 (376) / 0.58 |
| 1 MiB sequential write, QD16 | write | **559.5** (**559**) / 270.2 (270) / 980.39 | 287.3 (287) / 287.2 (287) / 0.56 | 439.1 (439) / 438.9 (**439**) / 0.60 | 439.3 (439) / **439.1** (**439**) / **0.54** |
| 1 MiB random mix 50/50, QD32 | read | **333.7** (**334**) / 219.4 (219) / 391.21 | 299.1 (299) / 298.9 (**299**) / **0.48** | 270.8 (271) / 270.7 (271) / 0.55 | 299.4 (299) / **299.2** (**299**) / 0.50 |
| 1 MiB random mix 50/50, QD32 | write | **331.0** (**331**) / 216.8 (217) / 391.21 | 293.7 (294) / 293.5 (**294**) / **0.48** | 268.9 (269) / 268.8 (269) / 0.55 | 293.9 (294) / **293.8** (**294**) / 0.50 |

</details>

<details>
<summary>Ceph rates during active fio, before the final fsync</summary>

MiB/s / IOPS / p99 in ms on the fio clock.

| Workload | Direction | PR buffered | PR direct | SG32 RC1 | SG32 b22 + fixes |
| --- | --- | --- | --- | --- | --- |
| 4 KiB random read, QD32 | read | 155.5 / 39,816 / **1.499** | **159.6** / **40,866** / 1.532 | 154.2 / 39,476 / 1.622 | 157.4 / 40,284 / 1.573 |
| 4 KiB random write, QD32 | write | **238.5** / **61,049** / **0.623** | 107.5 / 27,515 / 5.702 | 111.2 / 28,460 / 4.071 | 111.3 / 28,503 / 4.039 |
| 4 KiB random mix 50/50, QD32 | read | **120.4** / **30,820** / **2.068** | 68.9 / 17,643 / 5.898 | 69.0 / 17,660 / 5.538 | 69.8 / 17,869 / 5.865 |
| 4 KiB random mix 50/50, QD32 | write | **120.7** / **30,891** / **0.455** | 69.1 / 17,680 / 0.889 | 69.2 / 17,711 / 0.901 | 70.0 / 17,914 / 0.901 |
| 8 KiB random read, QD16 | read | 171.5 / 21,947 / 1.303 | 178.3 / 22,818 / 1.524 | **198.8** / **25,445** / **1.059** | 188.2 / 24,087 / 1.409 |
| 8 KiB random write, QD16 | write | **319.8** / **40,937** / **0.540** | 167.1 / 21,385 / 2.933 | 196.0 / 25,091 / 2.036 | 197.8 / 25,324 / 1.827 |
| 8 KiB random mix 70/30, QD16 | read | **139.6** / **17,869** / **1.393** | 127.7 / 16,348 / 2.556 | 129.1 / 16,518 / 2.294 | 128.8 / 16,488 / 3.224 |
| 8 KiB random mix 70/30, QD16 | write | **60.2** / **7,709** / **0.307** | 55.1 / 7,050 / 0.426 | 55.7 / 7,125 / 0.438 | 55.6 / 7,116 / 0.365 |
| 64 KiB sequential read, QD32 | read | **401.9** / **6,430** / **10.945** | 231.2 / 3,699 / 24.773 | 283.2 / 4,531 / 14.123 | 252.3 / 4,037 / 17.695 |
| 64 KiB sequential write, QD32 | write | **522.9** / **8,367** / **4.874** | 320.7 / 5,131 / 40.042 | 491.7 / 7,866 / 15.827 | 513.1 / 8,210 / 9.699 |
| 64 KiB random mix 50/50, QD32 | read | **543.4** / **8,695** / **5.562** | 137.1 / 2,193 / 14.025 | 376.3 / 6,021 / 9.699 | 340.0 / 5,441 / 8.159 |
| 64 KiB random mix 50/50, QD32 | write | **544.0** / **8,703** / **6.517** | 137.3 / 2,196 / 21.103 | 376.8 / 6,028 / 7.979 | 340.5 / 5,449 / 7.766 |
| 512 KiB sequential read, QD16 | read | 516.3 / 1,033 / 33.489 | 250.7 / 501 / 86.180 | 302.3 / 605 / 47.186 | **1,356.6** / **2,713** / **14.025** |
| 512 KiB sequential write, QD16 | write | **1,505.3** / **3,011** / **7.291** | 278.6 / 557 / 138.674 | 523.4 / 1,047 / 52.691 | 574.1 / 1,148 / 45.285 |
| 512 KiB random mix 50/50, QD32 | read | **1,035.0** / **2,070** / **15.827** | 387.5 / 775 / 156.500 | 422.2 / 844 / 117.441 | 285.7 / 571 / 407.372 |
| 512 KiB random mix 50/50, QD32 | write | **1,051.6** / **2,103** / **12.255** | 393.9 / 788 / 151.257 | 429.0 / 858 / 112.198 | 290.5 / 581 / 277.348 |
| 1 MiB sequential read, QD16 | read | 651.0 / 651 / 47.907 | 652.3 / 652 / 85.393 | 1,666.4 / 1,666 / 22.249 | **1,769.8** / **1,770** / **16.613** |
| 1 MiB sequential write, QD16 | write | **983.1** / **983** / **17.957** | 386.4 / 386 / 125.043 | 683.4 / 683 / 71.959 | 652.2 / 652 / 124.518 |
| 1 MiB random mix 50/50, QD32 | read | **719.2** / **719** / **37.290** | 550.7 / 551 / 109.314 | 471.8 / 472 / 94.896 | 569.6 / 570 / 83.362 |
| 1 MiB random mix 50/50, QD32 | write | **718.1** / **718** / **39.846** | 538.5 / 538 / 104.727 | 469.9 / 470 / 98.042 | 558.4 / 558 / 81.789 |

</details>

<details>
<summary>Ceph repeated-read cache check</summary>

The guest read the same range again with the host cache retained. The cluster had its own caches throughout. MiB/s / IOPS / p99 in ms.

| Workload | Direction | PR buffered | PR direct | SG32 RC1 | SG32 b22 + fixes |
| --- | --- | --- | --- | --- | --- |
| 512 KiB repeat read, QD16 | read | **1,945.0** / **3,890** / **9.110** | 857.3 / 1,715 / 48.595 | 827.9 / 1,656 / 23.101 | 937.7 / 1,875 / 22.708 |

</details>

<details>
<summary>Both Ceph observations for every workload</summary>

Each cell shows the first seed followed by the second, in MiB/s with guest p99 in milliseconds in parentheses. Writes and mixes include final fsync in the throughput clock. Reads use active fio time.

| Workload | Direction | PR buffered | PR direct | SG32 RC1 | SG32 b22 + fixes |
| --- | --- | --- | --- | --- | --- |
| 4 KiB random read, QD32 | read | 151.7 (1.516) / 159.4 (1.483) | 157.4 (1.696) / 161.8 (1.368) | 146.8 (1.745) / 161.6 (1.499) | 151.7 (1.581) / 163.1 (1.565) |
| 4 KiB random write, QD32 | write | 59.7 (0.627) / 62.0 (0.618) | 75.9 (4.751) / 76.0 (6.652) | 76.6 (3.785) / 76.6 (4.358) | 89.1 (3.719) / 76.6 (4.358) |
| 4 KiB random mix 50/50, QD32 | read | 40.0 (2.769) / 41.2 (1.368) | 49.8 (5.341) / 50.4 (6.455) | 49.8 (5.145) / 45.4 (5.931) | 49.8 (5.603) / 49.9 (6.128) |
| 4 KiB random mix 50/50, QD32 | write | 40.6 (0.432) / 40.8 (0.477) | 50.5 (0.922) / 50.0 (0.856) | 50.5 (0.848) / 45.0 (0.954) | 50.5 (0.872) / 49.5 (0.930) |
| 8 KiB random read, QD16 | read | 189.6 (1.155) / 153.3 (1.450) | 159.0 (1.974) / 197.5 (1.073) | 201.3 (1.044) / 196.3 (1.073) | 178.5 (1.647) / 197.8 (1.171) |
| 8 KiB random write, QD16 | write | 89.8 (0.569) / 90.2 (0.510) | 99.8 (3.326) / 109.6 (2.540) | 109.6 (1.761) / 109.8 (2.310) | 109.7 (1.778) / 109.6 (1.876) |
| 8 KiB random mix 70/30, QD16 | read | 63.1 (1.368) / 63.6 (1.417) | 76.9 (2.376) / 76.5 (2.736) | 76.7 (2.146) / 75.3 (2.441) | 76.8 (4.358) / 76.5 (2.089) |
| 8 KiB random mix 70/30, QD16 | write | 27.0 (0.330) / 27.6 (0.285) | 32.9 (0.403) / 33.2 (0.449) | 32.8 (0.444) / 32.7 (0.432) | 32.9 (0.354) / 33.2 (0.375) |
| 64 KiB sequential read, QD32 | read | 390.8 (11.076) / 412.9 (10.813) | 218.3 (21.103) / 244.0 (28.443) | 270.8 (17.957) / 295.6 (10.289) | 265.1 (17.957) / 239.5 (17.433) |
| 64 KiB sequential write, QD32 | write | 179.9 (6.062) / 234.1 (3.686) | 307.1 (28.443) / 182.2 (51.642) | 359.6 (14.483) / 355.6 (17.170) | 361.3 (10.158) / 361.5 (9.241) |
| 64 KiB random mix 50/50, QD32 | read | 149.9 (8.028) / 239.3 (3.097) | 118.1 (13.042) / 106.0 (15.008) | 219.0 (10.420) / 276.8 (8.978) | 215.9 (8.028) / 219.5 (8.290) |
| 64 KiB random mix 50/50, QD32 | write | 150.3 (11.600) / 239.4 (1.434) | 118.3 (19.530) / 106.0 (22.675) | 219.6 (8.454) / 277.0 (7.504) | 216.4 (7.766) / 219.6 (7.766) |
| 512 KiB sequential read, QD16 | read | 259.2 (46.924) / 773.4 (20.054) | 76.9 (143.655) / 424.5 (28.705) | 262.4 (57.934) / 342.2 (36.438) | 1,376.3 (13.435) / 1,336.8 (14.615) |
| 512 KiB repeat read, QD16 | read | 2,925.7 (8.978) / 964.2 (9.241) | 296.3 (83.362) / 1,418.3 (13.828) | 397.8 (31.064) / 1,258.0 (15.139) | 1,471.3 (15.925) / 404.1 (29.491) |
| 512 KiB sequential write, QD16 | write | 265.9 (9.634) / 330.4 (4.948) | 236.4 (160.432) / 224.6 (116.916) | 398.4 (42.729) / 361.5 (62.652) | 361.6 (61.604) / 438.6 (28.967) |
| 512 KiB random mix 50/50, QD32 | read | 238.5 (17.170) / 240.8 (14.483) | 172.2 (212.861) / 276.4 (100.139) | 218.2 (131.596) / 277.0 (103.285) | 152.8 (608.174) / 217.5 (206.569) |
| 512 KiB random mix 50/50, QD32 | write | 241.3 (13.566) / 245.5 (10.945) | 174.2 (204.472) / 281.8 (98.042) | 220.8 (126.353) / 282.5 (98.042) | 154.6 (354.419) / 221.8 (200.278) |
| 1 MiB sequential read, QD16 | read | 870.7 (33.161) / 431.3 (62.652) | 148.8 (143.655) / 1,155.8 (27.132) | 1,450.4 (29.753) / 1,882.4 (14.746) | 1,723.9 (17.695) / 1,815.6 (15.532) |
| 1 MiB sequential write, QD16 | write | 264.1 (17.957) / 276.4 (17.957) | 267.1 (158.335) / 307.3 (91.750) | 439.1 (87.556) / 438.7 (56.361) | 439.0 (73.925) / 439.2 (175.112) |
| 1 MiB random mix 50/50, QD32 | read | 231.8 (28.705) / 207.0 (45.875) | 275.4 (143.655) / 322.4 (74.973) | 315.0 (73.925) / 226.3 (115.868) | 275.1 (91.750) / 323.3 (74.973) |
| 1 MiB random mix 50/50, QD32 | write | 239.2 (36.438) / 194.4 (43.254) | 284.1 (143.655) / 302.9 (65.798) | 325.0 (67.633) / 212.6 (128.451) | 283.8 (92.799) / 303.7 (70.779) |

</details>

The raw disk had a 1 GiB working set prefilled with random data. Each phase transferred 128 or 512 MiB. We evicted the Linux cache for the test mapping before first reads and retained it for the repeat. The shared Ceph cluster kept its own caches. An automatic scrub interrupted the first window after 28 phases. We retained those observations and completed the missing profiles on a fresh test image. These are short guest measurements on a shared cluster.

## Local NVMe

The host used a consumer Kingston KC3000 2 TB (EIFK51.2) with Linux 7.0.0-43-generic. The guest ran Linux 6.1.190 and ext4 on an LVM-thin volume. We plan to repeat this on an enterprise SSD with hardware power-loss protection. We measured return from guest fsync. Power-loss behavior remains untested.

Each build completed 12 workload phases at four seeds. Stock, both two-open builds and b22 were rotated in the original run. RC1 ran in a later window. In the last window we alternated b22 + fixes with the exact PR binary on a third thin volume. Each ran first twice per workload. All three windows used the same host, guest workloads and seeds. We recorded other disk traffic and temperature and allowed the SSD to rest between runs. Cross-session differences include storage and VM placement effects.

**MiB/s.** First reads use active I/O time. Writes and both directions of a mix include the final guest fsync and command overhead. The repeated-read cache checks are listed separately.

| Workload | Direction | Stock | 2 opens, mixed† | 2 opens, direct | 1 open, b22 | 1 open, RC1 | 1 open, b22 + fixes | PR direct (SG1) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 4 KiB random read, QD32 (first) | read | **303.5** | 308.8 | 261.3 | 253.8 | 267.1 | 265.0 | 283.0 |
| 4 KiB random write, QD32 | write | 127.5 | 262.3 | 221.4 | 230.2 | 209.6 | 218.3 | **233.3** |
| 4 KiB random 50/50, QD32 | read | 89.9 | 144.9 | **124.1** | 116.7 | 120.3 | 116.6 | 113.8 |
| 4 KiB random 50/50, QD32 | write | 90.2 | 145.4 | **124.6** | 116.7 | 120.4 | 116.7 | 114.2 |
| 8 KiB random 70/30, QD16 | read | 200.5 | 276.4 | 260.6 | **286.6** | 260.0 | 272.4 | 272.0 |
| 8 KiB random 70/30, QD16 | write | 85.5 | 118.7 | 111.5 | **122.3** | 111.3 | 116.7 | 116.0 |
| 64 KiB random 50/50, QD32 | read | 238.9 | 730.9 | **741.8** | 738.4 | 675.5 | 678.0 | 646.3 |
| 64 KiB random 50/50, QD32 | write | 237.5 | 728.0 | **741.1** | 734.6 | 674.9 | 677.3 | 646.7 |
| 512 KiB random 50/50, QD32 | read | 270.6 | 906.5 | 841.0 | 931.2 | 726.5 | **964.9** | 882.4 |
| 512 KiB random 50/50, QD32 | write | 276.1 | 932.3 | 857.7 | 948.3 | 744.8 | **974.0** | 897.8 |
| 512 KiB sequential read, QD16 (first) | read | 2,738.0 | 2,742.6 | 4,113.0 | 4,501.4 | 3,895.6 | **4,565.1** | 4,323.0 |
| 1 MiB sequential write, QD16 | write | 321.3 | 1,920.5 | **2,076.5** | 1,602.4 | 1,352.7 | 1,708.1 | 1,596.6 |
| 4 KiB random write, QD1, final fsync | write | 43.1 | 46.8 | 45.1 | 46.2 | **46.6** | 46.1 | 45.7 |
| 4 KiB random write, QD1, fsync each | write | 1.067 | 1.102 | **1.125** | 1.122 | 1.016 | 1.075 | 1.051 |

† The first two-open, mixed build failed 65 of 480 integrity checks. Its results stay visible. Bold selects the best displayed value from the remaining builds. The revised two-open direct build, RC1 and b22 + fixes each passed their 480-case suites.

**Guest I/O p99 in ms.** The QD1 fsync-each profile times each write together with its fsync. The fio percentiles measure individual data operations before the final fsync.

| Workload | Direction | Stock | 2 opens, mixed† | 2 opens, direct | 1 open, b22 | 1 open, RC1 | 1 open, b22 + fixes | PR direct (SG1) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 4 KiB random read, QD32 (first) | read | 0.672 | 0.577 | 0.705 | 0.700 | **0.664** | **0.664** | 0.705 |
| 4 KiB random write, QD32 | write | 0.664 | 0.545 | 0.606 | **0.582** | 0.635 | 0.586 | 0.586 |
| 4 KiB random 50/50, QD32 | read | **0.594** | 0.578 | 0.664 | 0.627 | 0.705 | 0.664 | 0.659 |
| 4 KiB random 50/50, QD32 | write | **0.502** | 0.507 | 0.590 | 0.557 | 0.561 | 0.569 | 0.594 |
| 8 KiB random 70/30, QD16 | read | 0.420 | 0.406 | 0.406 | **0.383** | 1.162 | 0.420 | 0.420 |
| 8 KiB random 70/30, QD16 | write | 0.283 | 0.289 | 0.279 | **0.257** | 0.269 | 0.281 | 0.291 |
| 64 KiB random 50/50, QD32 | read | 1.556 | 1.384 | **1.237** | 1.303 | 2.331 | 2.437 | 2.200 |
| 64 KiB random 50/50, QD32 | write | 1.130 | 0.889 | **0.815** | 0.909 | 0.909 | 1.010 | 0.889 |
| 512 KiB random 50/50, QD32 | read | **5.177** | 8.569 | 8.028 | 6.586 | 12.452 | 8.126 | 8.339 |
| 512 KiB random 50/50, QD32 | write | **3.670** | 8.667 | 8.143 | 6.029 | 13.304 | 6.554 | 10.764 |
| 512 KiB sequential read, QD16 (first) | read | 3.949 | 3.899 | 2.753 | **2.277** | 2.654 | 2.654 | 2.392 |
| 1 MiB sequential write, QD16 | write | 14.631 | 11.338 | **10.928** | 13.435 | 21.398 | 13.959 | 15.761 |
| 4 KiB random write, QD1, final fsync | write | **0.097** | 0.112 | 0.111 | 0.117 | 0.113 | 0.110 | 0.113 |
| 4 KiB random write, QD1, fsync each | write | 13.197 | 13.035 | **12.939** | 12.952 | 13.858 | 13.469 | 13.857 |

<details>
<summary>IOPS for every local disk workload</summary>

| Workload | Direction | Stock | 2 opens, mixed† | 2 opens, direct | 1 open, b22 | 1 open, RC1 | 1 open, b22 + fixes | PR direct (SG1) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 4 KiB random read, QD32 (first) | read | **77,695.6** | 79,057.2 | 66,888.6 | 64,964.6 | 68,389.9 | 67,849.1 | 72,458.3 |
| 4 KiB random write, QD32 | write | 32,639.6 | 67,137.7 | 56,666.5 | 58,940.7 | 53,662.1 | 55,884.5 | **59,723.3** |
| 4 KiB random 50/50, QD32 | read | 23,002.8 | 37,095.1 | **31,772.7** | 29,873.0 | 30,788.6 | 29,851.2 | 29,132.1 |
| 4 KiB random 50/50, QD32 | write | 23,095.3 | 37,226.8 | **31,885.3** | 29,877.6 | 30,813.5 | 29,874.3 | 29,232.0 |
| 8 KiB random 70/30, QD16 | read | 25,659.5 | 35,378.6 | 33,362.3 | **36,684.1** | 33,280.0 | 34,864.9 | 34,818.8 |
| 8 KiB random 70/30, QD16 | write | 10,946.7 | 15,192.6 | 14,272.8 | **15,654.3** | 14,247.3 | 14,933.9 | 14,852.0 |
| 64 KiB random 50/50, QD32 | read | 3,823.0 | 11,694.5 | **11,868.0** | 11,814.8 | 10,808.3 | 10,847.6 | 10,340.9 |
| 64 KiB random 50/50, QD32 | write | 3,800.2 | 11,647.7 | **11,857.9** | 11,753.3 | 10,797.7 | 10,836.7 | 10,346.4 |
| 512 KiB random 50/50, QD32 | read | 541.2 | 1,813.0 | 1,681.9 | 1,862.5 | 1,453.1 | **1,929.9** | 1,764.7 |
| 512 KiB random 50/50, QD32 | write | 552.2 | 1,864.6 | 1,715.4 | 1,896.5 | 1,489.5 | **1,947.9** | 1,795.7 |
| 512 KiB sequential read, QD16 (first) | read | 5,476.1 | 5,485.2 | 8,225.9 | 9,002.8 | 7,791.3 | **9,130.1** | 8,645.9 |
| 1 MiB sequential write, QD16 | write | 321.3 | 1,920.5 | **2,076.5** | 1,602.4 | 1,352.7 | 1,708.1 | 1,596.6 |
| 4 KiB random write, QD1, final fsync | write | 11,040.0 | 11,985.2 | 11,540.1 | 11,834.2 | **11,931.7** | 11,801.8 | 11,698.9 |
| 4 KiB random write, QD1, fsync each | write | 273.2 | 282.1 | **288.0** | 287.3 | 260.0 | 275.2 | 269.0 |

</details>

<details>
<summary>Local writes before the final fsync and time spent in fsync</summary>

| Workload | Direction | Stock | 2 opens, mixed† | 2 opens, direct | 1 open, b22 | 1 open, RC1 | 1 open, b22 + fixes | PR direct (SG1) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 4 KiB random write, QD32 | write | 226.2 | 262.7 | 221.5 | 230.4 | 209.8 | 218.5 | **233.9** |
| 4 KiB random 50/50, QD32 | read | **135.8** | 145.0 | 124.2 | 116.8 | 120.4 | 116.7 | 113.9 |
| 4 KiB random 50/50, QD32 | write | **136.2** | 145.5 | 124.7 | 116.8 | 120.5 | 116.8 | 114.3 |
| 8 KiB random 70/30, QD16 | read | 278.0 | 276.8 | 261.0 | **286.9** | 260.6 | 272.7 | 272.4 |
| 8 KiB random 70/30, QD16 | write | 119.0 | 118.8 | 111.7 | **122.4** | 111.6 | 116.8 | 116.2 |
| 64 KiB random 50/50, QD32 | read | 669.6 | 732.5 | **743.5** | 740.2 | 679.2 | 679.4 | 650.1 |
| 64 KiB random 50/50, QD32 | write | 667.1 | 729.9 | **742.9** | 736.4 | 678.6 | 679.8 | 649.8 |
| 512 KiB random 50/50, QD32 | read | 947.3 | 909.2 | 843.3 | 934.0 | 731.7 | **968.2** | 889.2 |
| 512 KiB random 50/50, QD32 | write | 962.2 | 935.5 | 860.1 | 950.9 | 750.0 | **976.9** | 905.0 |
| 1 MiB sequential write, QD16 | write | 1,558.5 | 1,926.5 | **2,083.3** | 1,607.1 | 1,360.6 | 1,713.4 | 1,601.9 |
| 4 KiB random write, QD1, final fsync | write | **53.1** | 47.0 | 45.3 | 46.4 | 46.8 | 46.3 | 46.0 |
| 4 KiB random write, QD1, fsync each | write | 1.067 | 1.102 | **1.125** | 1.122 | 1.016 | 1.075 | 1.051 |

**IOPS before final fsync**

| Workload | Direction | Stock | 2 opens, mixed† | 2 opens, direct | 1 open, b22 | 1 open, RC1 | 1 open, b22 + fixes | PR direct (SG1) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 4 KiB random write, QD32 | write | 57,895.7 | 67,247.6 | 56,704.4 | 58,979.2 | 53,721.0 | 55,923.6 | **59,868.7** |
| 4 KiB random 50/50, QD32 | read | **34,754.2** | 37,126.7 | 31,797.1 | 29,896.5 | 30,811.0 | 29,875.9 | 29,153.9 |
| 4 KiB random 50/50, QD32 | write | **34,857.2** | 37,258.5 | 31,910.5 | 29,901.1 | 30,836.0 | 29,898.9 | 29,253.8 |
| 8 KiB random 70/30, QD16 | read | 35,589.4 | 35,424.1 | 33,408.9 | **36,728.0** | 33,362.3 | 34,907.2 | 34,862.5 |
| 8 KiB random 70/30, QD16 | write | 15,229.8 | 15,212.1 | 14,291.4 | **15,673.0** | 14,282.5 | 14,952.0 | 14,870.7 |
| 64 KiB random 50/50, QD32 | read | 10,713.0 | 11,719.4 | **11,896.2** | 11,843.6 | 10,867.6 | 10,870.3 | 10,400.9 |
| 64 KiB random 50/50, QD32 | write | 10,672.8 | 11,678.7 | **11,886.1** | 11,782.2 | 10,857.0 | 10,877.3 | 10,397.2 |
| 512 KiB random 50/50, QD32 | read | 1,894.6 | 1,818.3 | 1,686.7 | 1,868.1 | 1,463.3 | **1,936.3** | 1,778.4 |
| 512 KiB random 50/50, QD32 | write | 1,924.5 | 1,871.0 | 1,720.2 | 1,901.7 | 1,500.1 | **1,953.7** | 1,810.0 |
| 1 MiB sequential write, QD16 | write | 1,558.5 | 1,926.5 | **2,083.3** | 1,607.1 | 1,360.6 | 1,713.4 | 1,601.9 |
| 4 KiB random write, QD1, final fsync | write | **13,588.6** | 12,042.9 | 11,592.2 | 11,891.2 | 11,990.0 | 11,859.4 | 11,778.0 |
| 4 KiB random write, QD1, fsync each | write | 273.2 | 282.1 | **288.0** | 287.3 | 260.0 | 275.2 | 269.0 |

**Final fsync in ms**

| Workload | Direction | Stock | 2 opens, mixed† | 2 opens, direct | 1 open, b22 | 1 open, RC1 | 1 open, b22 + fixes | PR direct (SG1) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 4 KiB random write, QD32 | write | 3,270.492 | 3.305 | **3.068** | 3.084 | 5.356 | 3.282 | 5.060 |
| 4 KiB random 50/50, QD32 | write | 1,916.511 | 3.002 | **3.092** | 3.454 | 3.097 | 3.405 | 3.375 |
| 8 KiB random 70/30, QD16 | write | 1,024.761 | 3.225 | 3.417 | 3.240 | 4.870 | **3.190** | 3.282 |
| 64 KiB random 50/50, QD32 | write | 2,764.804 | 3.496 | **3.087** | 3.094 | 7.771 | 3.952 | 8.097 |
| 512 KiB random 50/50, QD32 | write | 2,670.740 | 3.462 | 3.286 | **3.003** | 5.373 | 3.199 | 9.194 |
| 1 MiB sequential write, QD16 | write | 8,745.112 | 3.622 | **4.031** | 5.786 | 12.482 | 10.173 | 8.498 |
| 4 KiB random write, QD1, final fsync | write | 139.164 | 3.188 | **3.101** | 3.276 | 3.339 | 3.363 | 3.316 |
| 4 KiB random write, QD1, fsync each | write | 0.696 | 0.847 | 0.693 | **0.634** | **0.634** | 0.644 | 0.683 |

</details>

<details>
<summary>Local repeated-read cache checks</summary>

Reading the same addresses twice exercises the host cache. On the second pass stock read 0 MiB from the test volume at both sizes. The downstream b22 + fixes build read 256 MiB in the 4 KiB profile and 4,096 MiB in the 512 KiB profile.

**MiB/s**

| Workload | Direction | Stock | 2 opens, mixed† | 2 opens, direct | 1 open, b22 | 1 open, RC1 | 1 open, b22 + fixes | PR direct (SG1) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 4 KiB random read, QD32 (repeat) | read | **323.4** | 294.1 | 273.5 | 246.6 | 268.2 | 247.1 | 227.0 |
| 512 KiB sequential read, QD16 (repeat) | read | **5,055.5** | 3,314.3 | 3,871.5 | 3,331.6 | 3,908.5 | 3,943.1 | 4,195.2 |

**IOPS**

| Workload | Direction | Stock | 2 opens, mixed† | 2 opens, direct | 1 open, b22 | 1 open, RC1 | 1 open, b22 + fixes | PR direct (SG1) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 4 KiB random read, QD32 (repeat) | read | **82,780.6** | 75,290.3 | 70,028.5 | 63,118.7 | 68,662.8 | 63,258.7 | 58,118.4 |
| 512 KiB sequential read, QD16 (repeat) | read | **10,111.0** | 6,628.5 | 7,742.9 | 6,663.2 | 7,817.0 | 7,886.2 | 8,390.3 |

**p99 in ms**

| Workload | Direction | Stock | 2 opens, mixed† | 2 opens, direct | 1 open, b22 | 1 open, RC1 | 1 open, b22 + fixes | PR direct (SG1) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 4 KiB random read, QD32 (repeat) | read | **0.498** | 0.508 | 0.680 | 0.721 | 0.647 | 0.664 | 0.807 |
| 512 KiB sequential read, QD16 (repeat) | read | **1.860** | 3.568 | 2.507 | 3.244 | 2.703 | 2.802 | 2.540 |

</details>

On the integrated SG32 build, 512 KiB mixed reads and writes reached 965 and 974 MiB/s. The two-open SG32 build reached 841 and 858 MiB/s. Read p99 was 8.126 and 8.028 ms respectively, and write p99 was 6.554 and 8.143 ms. At 64 KiB the two-open build was faster in both directions and read p99 was 1.237 ms versus 2.437 ms on the integrated build.

<details>
<summary>Local tests with fragmented guest memory</summary>

We interleaved b22 with each comparator in separate three-round series. We fragmented guest memory before each run. Each cell gives **MiB/s / fio IOPS / p99 in ms**. For the fixed 4 GiB write, MiB/s includes the final sync and the command overhead. The fio IOPS in this table use active I/O time. The first series contained 36 guest runs and the stock series 54.

| Compared with | Workload | Direction | Comparator result | 1 open, b22 |
| --- | --- | --- | --- | --- |
| 2 opens, direct | 4 KiB random read, QD32 | read | 280.1 / 71,688 / 0.659 | **313.2** / **80,161** / **0.618** |
| 2 opens, direct | 4 KiB random write, QD32 | write | 306.1 / 78,351 / **0.594** | **318.8** / **81,612** / 0.610 |
| 2 opens, direct | 8 KiB random mix 70/30, QD16 | read | 349.1 / 44,686 / 0.643 | **407.8** / **52,191** / **0.436** |
| 2 opens, direct | 8 KiB random mix 70/30, QD16 | write | 150.0 / 19,197 / 0.285 | **175.3** / **22,431** / **0.257** |
| 2 opens, direct | 64 KiB random mix 50/50, QD32 | read | **1,066.0** / **17,054** / 3.162 | 1,060.1 / 16,958 / **2.966** |
| 2 opens, direct | 64 KiB random mix 50/50, QD32 | write | **1,063.8** / **17,017** / 2.736 | 1,057.7 / 16,920 / **2.540** |
| 2 opens, direct | 512 KiB read over 4 GiB, QD16 | read | 1,736.3 / 3,473 / 6.586 | **1,866.9** / **3,734** / **6.324** |
| 2 opens, direct | 1 MiB write, 4 GiB and fsync, QD16 | write | 1,443.7 / 1,922 / 15.925 | **2,074.5** / **2,957** / **9.765** |
| Stock | 4 KiB random read, QD1 | read | 26.8 / 6,865 / 0.191 | **29.5** / **7,558** / **0.171** |
| Stock | 4 KiB random read, QD32 | read | **434.9** / **111,327** / **0.651** | 325.5 / 83,318 / 0.717 |
| Stock | 4 KiB random write, QD32 | write | 246.4 / 63,076 / **0.449** | **283.7** / **72,626** / 0.578 |
| Stock | 8 KiB random mix 70/30, QD16 | read | 319.7 / 40,925 / **0.643** | **374.7** / **47,955** / 0.979 |
| Stock | 8 KiB random mix 70/30, QD16 | write | 137.2 / 17,564 / 0.342 | **161.0** / **20,614** / **0.237** |
| Stock | 64 KiB random mix 50/50, QD32 | read | 233.1 / 3,727 / 3.949 | **1,085.8** / **17,371** / **3.588** |
| Stock | 64 KiB random mix 50/50, QD32 | write | 233.1 / 3,726 / 5.800 | **1,083.8** / **17,336** / **2.966** |
| Stock | 1 MiB sequential read, QD16 | read | 845.7 / 842 / 23.200 | **1,773.5** / **1,770** / **12.648** |
| Stock | 1 MiB sequential write, QD16 | write | 265.0 / 263 / 106.430 | **2,090.4** / **2,087** / **17.957** |
| Stock | 1 MiB write, 4 GiB and fsync, QD16 | write | 258.3 / 270 / 147.849 | **2,080.5** / **2,882** / **10.682** |
| Stock | 512 KiB read over 4 GiB, QD16 | read | 800.5 / 1,601 / 11.993 | **1,481.4** / **2,963** / **6.849** |

Here b22 reached 2,075 MiB/s on the 1 MiB write with fsync, with 9.765 ms p99. The revised two-open build reached 1,444 MiB/s with 15.925 ms p99. That ordering reversed in the ordinary-memory series above.

</details>

<details>
<summary>Earlier results from the local disk, ext4 files and Ceph</summary>

The first matched local three-build series reported one complete trio for each workload. Data rates are MiB/s. The bundle includes the other retained observations and the reasons some attempts were excluded.

| Block (KiB) | Workload | Direction | Stock | 2 opens, mixed† | First 1-open SG32 |
| --- | --- | --- | --- | --- | --- |
| 4 | read | read | **143.9** | 422.4 | 111.4 |
| 4 | write | write | **166.5** | 312.2 | 69.9 |
| 4 | mixed | read | 110.2 | 156.2 | **123.5** |
| 4 | mixed | write | 110.0 | 156.0 | **123.3** |
| 64 | read | read | 664.3 | 999.0 | **2,308.9** |
| 64 | write | write | 333.0 | 1,077.9 | **600.6** |
| 64 | mixed | read | 298.8 | 365.4 | **883.3** |
| 64 | mixed | write | 303.6 | 371.3 | **897.6** |
| 512 | read | read | 2,089.8 | 2,497.6 | **3,374.0** |
| 512 | write | write | 333.0 | 1,780.9 | **1,780.9** |
| 512 | mixed | read | 304.4 | 627.3 | **778.2** |
| 512 | mixed | write | 297.9 | 613.9 | **761.7** |
| 1024 | read | read | 1,609.4 | 3,483.0 | **3,552.5** |
| 1024 | write | write | 308.0 | 2,497.6 | **1,780.9** |
| 1024 | mixed | read | 273.1 | 926.1 | **777.4** |
| 1024 | mixed | write | 252.1 | 854.8 | **717.5** |

Four earlier tests went directly to the host storage path. Each row is a separate observation. These had different working sets and timing boundaries from the guest medians.

| Native workload | Direction | Block | QD | Active MiB/s | IOPS | Completion p99 ms | Command with drain MiB/s |
| --- | --- | --- | --- | --- | --- | --- | --- |
| peak-write-q16-4g | write | 1m | 16 | 1,669.8 | 1,670 | 32.637 | — |
| fixed-2g-flushed | write | 1m | 16 | 1,226.3 | 1,226 | 19.792 | 897.5 |
| peak-read-cold-q16 | read | 1m | 16 | 1,954.2 | 1,954 | 17.695 | — |
| peak-write-q16-4g | write | 1m | 16 | 2,278.1 | 2,278 | 13.435 | — |

The following workstation runs used earlier revisions of the PR. Units and timing boundaries match the exact-head ext4 table.

| Build | Guest workload | Unit | Async buffered | Async direct | Sync buffered | Sync direct |
| --- | --- | --- | --- | --- | --- | --- |
| Original PR (f3b02ba92) | 1 MiB sequential read, first | MiB/s | 1,828.9 | **1,905.5** | 1,065.3 | **1,101.0** |
| Original PR (f3b02ba92) | 1 MiB sequential read, repeat | MiB/s | **5,136.2** | 1,888.5 | **5,829.7** | 1,084.8 |
| Original PR (f3b02ba92) | 1 MiB sequential write, final fsync | MiB/s | 318.1 | **360.8** | 313.0 | **344.8** |
| Original PR (f3b02ba92) | 4 KiB mixed 50/50, final fsync | IOPS | **37,288** | 37,002 | **18,657** | 15,014 |
| Original PR (f3b02ba92) | 1 MiB first read, batch p99 | µs | 10,285 | **9,744** | **18,207** | 23,375 |
| Original PR (f3b02ba92) | 1 MiB repeat read, batch p99 | µs | **4,086** | 9,887 | **3,603** | 23,981 |
| Original PR (f3b02ba92) | 1 MiB write, batch p99 | µs | **5,439** | 44,336 | **7,072** | 54,254 |
| Original PR (f3b02ba92) | 4 KiB mixed read, batch p99 | µs | **450** | 2,299 | **1,034** | 3,020 |
| Original PR (f3b02ba92) | 4 KiB mixed write, batch p99 | µs | 255 | **233** | **1,034** | 3,020 |
| Before FLUSH change (9eff14d22) | 1 MiB sequential read, first | MiB/s | 2,121.5 | **2,633.7** | **1,988.4** | 1,710.6 |
| Before FLUSH change (9eff14d22) | 1 MiB sequential read, repeat | MiB/s | **4,975.9** | 2,696.8 | **5,338.3** | 1,725.3 |
| Before FLUSH change (9eff14d22) | 1 MiB sequential write, final fsync | MiB/s | 325.8 | **349.7** | 327.7 | **344.0** |
| Before FLUSH change (9eff14d22) | 4 KiB mixed 50/50, final fsync | IOPS | **37,895** | 37,076 | **18,692** | 14,827 |
| Before FLUSH change (9eff14d22) | 1 MiB first read, batch p99 | µs | 8,558 | **8,358** | **9,770** | 17,912 |
| Before FLUSH change (9eff14d22) | 1 MiB repeat read, batch p99 | µs | **4,437** | 7,374 | **4,442** | 18,120 |
| Before FLUSH change (9eff14d22) | 1 MiB write, batch p99 | µs | **5,660** | 51,791 | **7,041** | 52,647 |
| Before FLUSH change (9eff14d22) | 4 KiB mixed read, batch p99 | µs | **457** | 2,271 | **1,050** | 3,032 |
| Before FLUSH change (9eff14d22) | 4 KiB mixed write, batch p99 | µs | **230** | 244 | **1,050** | 3,032 |

This separate 50/50 mixed workload ran twice per mode on the original PR binary. Figures are decimal MB/s and fio IOPS during I/O. Its working set fit in host memory.

| Block | Direction | Buffered MB/s | Direct MB/s | Buffered IOPS | Direct IOPS |
| --- | --- | --- | --- | --- | --- |
| 4 KiB | read | **242.5** | 160.6 | **59,195** | 39,212 |
| 4 KiB | write | **243.1** | 161.0 | **59,352** | 39,316 |
| 64 KiB | read | **2,161.4** | 137.7 | **32,980** | 2,100 |
| 64 KiB | write | **2,172.8** | 138.4 | **33,154** | 2,112 |
| 512 KiB | read | **3,610.1** | 179.2 | **6,886** | 341 |
| 512 KiB | write | **3,802.0** | 188.8 | **7,251** | 360 |
| 1 MiB | read | **3,678.8** | 216.7 | **3,508** | 206 |
| 1 MiB | write | **3,923.8** | 231.1 | **3,742** | 220 |

An earlier RBD run used the revised two-open design with one data segment. Five fio jobs ran once each. Latency here is completion p99.

| Older SG1 workload | Block | QD | MiB/s | IOPS | Completion p99 ms |
| --- | --- | --- | --- | --- | --- |
| randread-qd1 | 4k | 1 | 5.66 | 1,449.7 | 0.987 |
| randread-qd32 | 4k | 32 | 119.12 | 30,493.5 | 1.434 |
| randwrite-qd1 | 4k | 1 | 2.49 | 638.7 | 3.260 |
| seqread-qd32 | 1M | 32 | 989.27 | 989.3 | 120.062 |
| seqwrite-qd32 | 1M | 32 | 212.59 | 212.6 | 274.727 |

An earlier RBD segment-count experiment used short dd runs and zero-filled data. Rates below are decimal MB/s. The 300 MiB fragmentation setup failed its readiness check and the 400 MiB run did not record confirmation. Treat those labels as requested setups.

| Guest setup | Engine | Workload | 1 segment | 32 segments | 64 segments |
| --- | --- | --- | --- | --- | --- |
| 300 MiB fragmentation setup | Sync | 1 MiB write | 39.43 | 368.61 | **434.68** |
| 300 MiB fragmentation setup | Sync | 4 KiB sync write | **2.88** | 2.77 | 2.77 |
| 300 MiB fragmentation setup | Sync | 4 KiB sync write, 4 jobs | 2.84 | 2.79 | **3.58** |
| 300 MiB fragmentation setup | Sync | 4 KiB read | **114.44** | 112.66 | 109.74 |
| 300 MiB fragmentation setup | Sync | 4 KiB read, 4 jobs | **174.52** | 173.10 | 166.77 |
| 300 MiB fragmentation setup | Async | 1 MiB write | 98.67 | 432.86 | **501.47** |
| 300 MiB fragmentation setup | Async | 4 KiB sync write | 2.67 | **2.73** | 2.68 |
| 300 MiB fragmentation setup | Async | 4 KiB sync write, 4 jobs | **5.27** | 4.84 | 5.04 |
| 300 MiB fragmentation setup | Async | 4 KiB read | **105.19** | 99.75 | 98.12 |
| 300 MiB fragmentation setup | Async | 4 KiB read, 4 jobs | 181.41 | 174.55 | **182.19** |
| 400 MiB fragmentation setup | Sync | 1 MiB write | 467.23 | — | **513.65** |
| 400 MiB fragmentation setup | Sync | 4 KiB sync write | 2.74 | — | **2.85** |
| 400 MiB fragmentation setup | Sync | 4 KiB sync write, 4 jobs | **2.97** | — | 2.92 |
| 400 MiB fragmentation setup | Sync | 4 KiB read | **114.95** | — | 110.45 |
| 400 MiB fragmentation setup | Sync | 4 KiB read, 4 jobs | **175.42** | — | 166.02 |
| 400 MiB fragmentation setup | Async | 1 MiB write | 483.04 | — | **516.81** |
| 400 MiB fragmentation setup | Async | 4 KiB sync write | 2.63 | — | **2.69** |
| 400 MiB fragmentation setup | Async | 4 KiB sync write, 4 jobs | **6.29** | — | 5.51 |
| 400 MiB fragmentation setup | Async | 4 KiB read | **104.01** | — | 97.46 |
| 400 MiB fragmentation setup | Async | 4 KiB read, 4 jobs | **182.96** | — | 168.59 |
| Fresh guest | Sync | 1 MiB write | 472.14 | — | **516.38** |
| Fresh guest | Sync | 4 KiB sync write | **2.83** | — | 2.73 |
| Fresh guest | Sync | 4 KiB sync write, 4 jobs | **3.51** | — | 2.85 |
| Fresh guest | Sync | 4 KiB read | **119.59** | — | 112.06 |
| Fresh guest | Sync | 4 KiB read, 4 jobs | **176.08** | — | 158.88 |
| Fresh guest | Async | 1 MiB write | **500.07** | — | 481.08 |
| Fresh guest | Async | 4 KiB sync write | **2.62** | — | 2.57 |
| Fresh guest | Async | 4 KiB sync write, 4 jobs | **5.25** | — | 4.79 |
| Fresh guest | Async | 4 KiB read | **104.72** | — | 96.93 |
| Fresh guest | Async | 4 KiB read, 4 jobs | **217.62** | — | 176.02 |

</details>

## Implementation and checks

Both engines use one open file for the lifetime of the backing. Aligned I/O goes straight to it. A partial host block uses an aligned buffer and can block the Async worker while it preserves adjacent sectors. A writable backing must have a single writer so overlapping read-modify-write operations have a consistent view. The implementation accepts one virtqueue. Regular backing files must report direct-I/O alignment through statx. It preserves the setting when restoring a snapshot or replacing the backing file.

The proposed main branch keeps the upstream one-segment geometry. The companion v1.17.0 series has 32 segments, a 256-entry virtqueue and the guest-memory change. The additional SG32 executable `d3a5a44b` keeps both write-path fixes and the recheck after rearming virtqueue notifications. Its queue-depth metric follows the earlier b22 behavior. [SG32 discussion](https://github.com/firecracker-microvm/firecracker/issues/6441#issuecomment-7041158627) · [Patches, workloads and every recorded result](https://github.com/bacarrdy/firecracker/tree/reference/direct-io-results-20261121/benchmarks/direct-io)

We tried segment and ring sizes, submission and completion changes, native AIO, worker affinity and indirect descriptors. That took a lot of runs and a lot of tokens. Some helped one workload and hurt another. The experiment tables include those regressions.

Both x86_64 and aarch64 passed the build check. All 20 x86_64 KVM runs of the proposed executable passed guest readback and whole-file SHA-256. Eight more KVM cases covered partial sectors and snapshot restore with backing replacement across Sync, Async, PCI and MMIO. The rate limiter worked in a separate functional test.

## License Acceptance

By submitting this pull request, I confirm that my contribution is made under the terms of the Apache 2.0 license. For more information on the Developer Certificate of Origin and signing off commits, see [CONTRIBUTING.md](https://github.com/firecracker-microvm/firecracker/blob/main/CONTRIBUTING.md).

## PR Checklist

- [x] I have read [CONTRIBUTING.md](https://github.com/firecracker-microvm/firecracker/blob/main/CONTRIBUTING.md).
- [x] Run `tools/devtool checkbuild --all` for x86_64 and aarch64.
- [x] Run `tools/devtool checkstyle --no-clippy`.
- [x] Describe the change, its purpose and the measurements.
- [x] Update the drive and block I/O documentation.
- [x] Add the user-facing drive option to CHANGELOG.md.
- [x] Issue linkage reviewed. This follows #5910 and does not close #6441.
- [x] Follow the [API change runbook](https://github.com/firecracker-microvm/firecracker/blob/main/docs/api-change-runbook.md) for the optional drive field.
- [x] Cover both engines, backing replacement, config replay, negotiated FLUSH and snapshot restore.
- [x] No new TODOs.
- [x] The drive API and virtio-blk engine changes belong in Firecracker. The vm-memory change is supplied separately.
