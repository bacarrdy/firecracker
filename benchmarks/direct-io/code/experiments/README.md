# Experimental patches

These are the original diffs from the I/O experiments. They target different revisions of the downstream block code. The 64-segment, larger-ring and indirect revisions had their own snapshot requirements. Both SG32 release candidates use 32 segments, 256 ring entries and direct descriptor chains. The proposed series is in the [source index](../README.md).

The SG64 and SG64/512 experiments require a fresh boot. Format 12.1 stores the
SEG_MAX feature bit, with the reader supplying the limit of 32. The experiment
rejects snapshot creation, restore and migration. We verified the rejection on
the measured SG64/512 binary both before boot and after pausing a running guest.

| Patch | Experiment |
| --- | --- |
| [4k-alignment.patch](4k-alignment.patch) | Single-segment alignment checks. |
| [4k-scalar.patch](4k-scalar.patch) | Reuse a validated scalar buffer. |
| [iov-window-in-place.patch](iov-window-in-place.patch) | Mutate the remaining vector window in place. |
| [bounded-iov-windows.mbox](bounded-iov-windows.mbox) | Bound vectored transfers and continue on completion. |
| [cached-sq-head.mbox](cached-sq-head.mbox) | Reuse the observed kernel submission-queue head. |
| [available-index.patch](available-index.patch) | Reuse the observed available index for queue depth. |
| [sq-tail.patch](sq-tail.patch) | Publish the submission tail at submit time. |
| [async-dispatch.patch](async-dispatch.patch) | Allow one read to use forced asynchronous submission. |
| [two-async-reads.patch](two-async-reads.patch) | Allow two eligible small reads to use that policy. |
| [upstream-queue-mask.mbox](upstream-queue-mask.mbox) | Backport the upstream ring mask. The mbox retains its original authors. |
| [conditional-queue-mask.patch](conditional-queue-mask.patch) | Earlier conditional ring mask. |
| [sg64.mbox](sg64.mbox) | Advertise up to 64 data segments on a fresh VM. |
| [sg64-ring512.mbox](sg64-ring512.mbox) | Give the 64-segment experiment 512 entries in the virtqueue. |
| [worker-affinity.patch](worker-affinity.patch) | Set the io-wq worker CPU mask for the trial. |
| [indirect-descriptors.mbox](indirect-descriptors.mbox) | Negotiate and parse indirect descriptor tables. |
| [indirect-region-reuse.mbox](indirect-region-reuse.mbox) | Reuse the mapped region while reading an indirect table. |
| [no-sqarray.patch](no-sqarray.patch) | Use the direct SQE layout on supporting kernels. |
| [sqarray-lab-control.patch](sqarray-lab-control.patch) | Control the SQ layout when running the same-binary comparison. |
| [sqarray-wrap.patch](sqarray-wrap.patch) | Exercise wraparound on single-entry submissions. |
| [adjacent-4k.patch](adjacent-4k.patch) | Merge eligible adjacent requests. |
| [completion-epoll.patch](completion-epoll.patch) | Compare completion event sources. |
| [indirect-snapshot.patch](indirect-snapshot.patch) | Version the experimental indirect-descriptor snapshot state. |
| [indirect-restore.patch](indirect-restore.patch) | Restore the saved indirect negotiation. |
