# Measurement files

| Data | Rows |
| --- | ---: |
| [Earlier SG1 two-open RBD jobs](ceph-earlier-uncapped.csv) | 5 |
| [Earlier RBD segment-count experiment](ceph-scatter-gather-all.csv) | 210 |
| [Ceph fio and full-command data per direction and seed](ceph-uncapped-directions.csv) | 168 |
| [Ceph medians by mode, direction and workload](ceph-uncapped-medians.csv) | 84 |
| [Local NVMe rates, p99, temperatures and backing bytes for every trial](local-all-seeds.csv) | 448 |
| [local both baselines by seed](local-both-baselines-by-seed.csv) | 192 |
| [local both baselines](local-both-baselines.csv) | 48 |
| [Earlier matched three-build NVMe comparison](local-earlier-all-sizes.csv) | 48 |
| [Earlier engine trials with no I/O ceiling](local-earlier-engine-matrix.csv) | 3,423 |
| [Recorded exclusions in the earlier local trials](local-earlier-exclusions.csv) | 347 |
| [Earlier guest and native-host observations](local-earlier-observations.csv) | 456 |
| [local fragmented stock pairs](local-fragmented-stock-pairs.csv) | 33 |
| [local fragmented stock per seed](local-fragmented-stock-per-seed.csv) | 66 |
| [local fragmented two open pairs](local-fragmented-two-open-pairs.csv) | 24 |
| [local fragmented two open per seed](local-fragmented-two-open-per-seed.csv) | 48 |
| [Throughput and p99 together for each local phase](local-joint-metrics.csv) | 204 |
| [Four-seed medians by workload and direction](local-medians.csv) | 112 |
| [Whole-workload throughput and latency differences at each seed](local-paired-outcomes.csv) | 1,440 |
| [Earlier RC1 comparison at identical seeds across sessions](local-same-seed-differences.csv) | 256 |
| [Intermediate PR executable on ext4](pr-interim-ext4-runs.csv) | 80 |
| [Original PR binary on ext4](pr-original-ext4-runs.csv) | 80 |
| [Four-size mixed I/O series on the original PR build](pr-original-mixed-sizes.csv) | 16 |
| [Proposed PR executable on ext4](pr-preview-ext4-runs.csv) | 80 |

The [local trial timings](local-trial-timings.jsonl) contain guest clocks, block counters, host cache and CPU. The [Ceph jobs](ceph-uncapped-jobs.jsonl) retain both latency distributions, NBD counters and before/after fsync clocks. The [Ceph configuration](ceph-configuration.json) records the checked settings and binary for each boot. The [five older RBD jobs](ceph-earlier-uncapped.jsonl) are separate. [Build and verification records](verification.json).

## Other approaches tried

These experiments have different units and denominators. Some alternate two settings on the same executable. Others compare earlier recordings at the same seed. The input files preserve the comparison type and exclusion flags where recorded.

| Experiment | Paired observations or metric rows |
| --- | ---: |
| [cached-sq-head](experiments/cached-sq-head.csv) | 6 |
| [candidate-matrix-followup](experiments/candidate-matrix-followup.csv) | 492 |
| [candidate-matrix](experiments/candidate-matrix.csv) | 8,940 |
| [indirect-descriptors](experiments/indirect-descriptors.csv) | 112 |
| [indirect-parser-kvm](experiments/indirect-parser-kvm.csv) | 6 |
| [indirect-parser](experiments/indirect-parser.csv) | 192 |
| [native-aio-rolling](experiments/native-aio-rolling.csv) | 4 |
| [native-aio](experiments/native-aio.csv) | 8 |
| [queue-mask-upstream](experiments/queue-mask-upstream.csv) | 500 |
| [queue-mask](experiments/queue-mask.csv) | 500 |
| [sg64-queue256-queue512](experiments/sg64-queue256-queue512.csv) | 20 |
| [sq-array-write](experiments/sq-array-write.csv) | 20 |
| [sq-array](experiments/sq-array.csv) | 8 |
| [worker-affinity-sizes](experiments/worker-affinity-sizes.csv) | 16 |
| [worker-affinity](experiments/worker-affinity.csv) | 8 |
