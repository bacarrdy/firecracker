# Direct I/O for Firecracker

[Proposed PR text and benchmark tables](PR.md) · [Source code](code/README.md) · [Per-run data](results/README.md) · [Workloads](workloads/README.md)

The upstream executable used for the performance tables was built from
`29c04bb6c63b765b52f39cd9be4a0461957c8500`. The draft now also labels its
snapshots as format 13.1. Local KVM checks restored a 13.0 buffered snapshot and
13.1 direct snapshots with both IO engines. The companion v1.17.0 reference
source is `2c49d3210741cd5ca2a9cdc97d7285b15453577d`. The source index includes
the patch for the second candidate. [Draft #6459](https://github.com/firecracker-microvm/firecracker/pull/6459)

The exact upstream executable was tested on an ext4 backing file, a local LVM-thin volume and Ceph RBD. The downstream SG32 builds have additional changes. The report identifies all binaries and records workload, units, integrity checks and limitations for each series.
