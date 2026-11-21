# Disk I/O source

## Firecracker main

The proposed branch for #6459 is `29c04bb6c63b765b52f39cd9be4a0461957c8500` on `58cf1b2cdca407f2323d1b0b25ebb95dfe5b4bcb`. This branch fast-forwards the existing PR head. It adds the optional `direct_io` drive setting, one backing open, alignment discovery and bounded staging in both engines. Snapshot restore and backing replacement retain the mode. The follow-ups cover completion errors, guest memory checks, publishing the used index, and completing a write if a Writeback guest declines FLUSH.

- `46c5f22cf` virtio-blk: support direct I/O for reads and writes
- `f3b02ba92` tests: wait for the direct I/O completion event
- `c4597fe4e` fix(io_uring): decode negative completion errors as positive errno
- `23bf7f0eb` memory: check the plugged bitmap for the requested slots
- `8cdc43edb` virtio-blk: skip the used index store when no request completed
- `29c04bb6c` virtio-blk: honor writethrough when a direct drive's FLUSH is declined

Apply [the signed series](firecracker-main.mbox) with `git am /path/to/firecracker-main.mbox` on the base above. [Changed files](firecracker-main-files.txt).

## One open with scatter/gather

The [v1.17.0 reference patch](firecracker-v1.17-sg32.patch) applies to `95f868c8e345b1cc8faccd1a3c910b4989dc3f58`. The [Git bundle](firecracker-v1.17-sg32.bundle) preserves the original commits, merge and sign-offs. In a Firecracker clone containing v1.17.0, run:

```sh
git fetch /path/to/firecracker-v1.17-sg32.bundle refs/heads/reference/sg32-one-open
git switch --detach FETCH_HEAD
```

The resulting commit is `2c49d3210741cd5ca2a9cdc97d7285b15453577d`. It has the combined local and RBD benchmark build's virtio-blk and io_uring implementations, 32 data segments, a 256-entry virtqueue and its vm-memory dependency. The downstream configuration key is `direct_write`. In the one-open build this enables direct reads and writes. Checked-in tests cover alignment, partial requests, read-modify-write, FLUSH, rate-limiter admission, snapshots and replacing the backing file.

The portable reference excludes unrelated downstream features. Its disk source is compared with the integrated RC1 binary's source. The benchmarks labeled RC1 used the integrated executable with SHA-256 `d7f1e4e8318243a64f3eb0bb58d01126979554776a292016135d22886959de2f`. [All changed files](firecracker-v1.17-sg32-files.txt).

The vendored vm-memory is the 0.18.0 version used to build RC1. The [small library patch](vm-memory-0.18.0.patch) applies to `b6404d240d639a231abcd2b2db5a4b79ca43059c`. [The port for rust-vmm](rust-vmm-region-map.patch) applies to `fc7640584c188dc91b52d57a4ed3c586dc40cb97`. Both reuse an immutable table of guest memory regions across outstanding requests.

The main mbox and the combined diff were reapplied to clean checkouts. Their Git tree IDs match the source checkouts.

## Reproducing the b22 queue with the write fixes

Apply [the signed queue change](sg32-b22-write-fixes.mbox) with `git am` on the portable SG32 reference at `2c49d3210`. The [plain diff](sg32-b22-write-fixes.patch) is also included. The portable result is `df2f9f453a43b6076b99ec4f1fdd80707a39a65c`.

The downstream executable `d3a5a44bf167682e1c1fee5a03ac2c49885c7177c2381217558230247c7e3ebf` records queue depth after popping a chain. It retains the bounds check after rearming notifications, the completion errno fix and the direct Writeback behavior when the guest declines FLUSH.

[Experimental source diffs and their individual scopes](experiments/README.md) · [Measurements from those experiments](../results/README.md).
