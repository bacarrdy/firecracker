# Direct I/O measurements

- [Full fio matrix and samples](fio.md) with the guest page cache bypassed
  and enabled on the same SG32 build.
- [Historical Ceph RBD observations](ceph.md) with both samples at each
  workload and the executable hashes.
- [Historical local NVMe tables and samples](local-nvme.md). These runs used
  a consumer Kingston KC3000 without hardware power-loss protection.
  Builds were measured across multiple windows and volumes. Power-loss
  durability was outside the scope of the test.

Short ext4 raw records cover `97174a4` and `d542b92`. The binary hash, kernel and run order are in the corresponding metadata. The setup
uses one guest vCPU, 384 MiB of RAM, PCI and a 4096-byte guest block size.
Each sequential range is 128 MiB, with batches of 16 requests. The write
rate includes the final guest fsync. Latency is measured from batch
submission to each completion.
