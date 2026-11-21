# Direct I/O for Firecracker

[Proposed PR text and benchmark tables](PR.md) · [Source code](code/README.md) · [Per-run data](results/README.md) · [Workloads](workloads/README.md)

This is the local preview for reopening [#6459](https://github.com/firecracker-microvm/firecracker/pull/6459) as a draft. Its proposed head `29c04bb6c63b765b52f39cd9be4a0461957c8500` fast-forwards the publicly visible head `f3b02ba92`. The companion v1.17.0 reference source is `2c49d3210741cd5ca2a9cdc97d7285b15453577d`; the source index includes the patch for the second candidate.

The exact upstream executable was tested on an ext4 backing file, a local LVM-thin volume and Ceph RBD. The downstream SG32 builds have additional changes. The report identifies all binaries and records workload, units, integrity checks and limitations for each series.
