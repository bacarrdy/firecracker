# Pre-copy migration reference for Firecracker v1.17.0

This branch implements a prototype for copying guest RAM between two KVM
microVMs. The receiver places incoming pages in private anonymous memory
before it loads device or CPU state. Its base is the upstream v1.17.0 release.
Protocol 1 and the API extensions described here are specific to this branch.

## Running a move

Use matching VMMs, compatible hosts and guest CPU configurations, and a source
configured with `track_dirty_pages`. Prepare the disks and other external
resources the restored VM needs. The stream carries guest RAM. Transfer the
snapshot state file separately. The caller owns transport, timeouts, storage
consistency and exclusive ownership of the running guest. Use a trusted peer
and a protected connection for guest memory and state.

1. Start an empty target VMM. Check `GET /migration` on both VMMs for
   `{"protocol":1}`.
2. For each round, pass one connected file descriptor to each VMM using
   `SCM_RIGHTS` with its Unix API request. Run the receiver and sender
   concurrently, since either request can block on its peer. On the target,
   call `PUT /migration/receive-round` with `{}`. On the source, call
   `PUT /migration/send-round` with `{"first":true}`. This first round covers
   all memory, with zero pages implicit. Subsequent source calls with `{}`
   send pages dirtied since the previous take. The source vCPUs can run during
   these rounds. Wait for both sides to complete each round.
3. Pause the source. Issue the next receive request on the target. On the
   source, call `PUT /snapshot/create` with
   `{"snapshot_type":"StateOnly","snapshot_path":"vmstate"}`, passing the
   stream descriptor. This quiesces devices, writes the state file and sends
   the final memory round with the state file's SHA-256.
4. After the final round completes, compare `GET /migration/memory-digest`
   on the paused source and the pre-boot target if verifying all RAM. Transfer
   the state file. On the target, call `PUT /snapshot/load` with
   `{"snapshot_path":"vmstate","mem_backend":{"backend_type":"Migration"}}`.
   The loader checks the state digest and memory layout. Keep the source paused
   and resume the target after transferring ownership. Restore can itself write
   guest memory, so do the optional digest comparison before loading.

## Dirty tracking and retries

Diff snapshots and streamed rounds consume the same dirty logs. Taking a Diff
snapshot during a stream can omit pages from the following round. A stream
can also consume pages needed by an earlier Diff snapshot chain. Keep these
operations exclusive. After streaming, take a new full snapshot before starting
another Diff chain. This prototype leaves that coordination to the caller.

After a failed round, discard the partial target and its stream. Retry with a
fresh target, a new connection and a new full FIRST round from the source.
Protocol 1 requires 4 KiB pages and ordinary DRAM regions. It excludes
hugetlbfs-backed memory, virtio-mem regions and devices that Firecracker
cannot snapshot.

## Exercising the prototype

On an x86_64 Linux machine with KVM, use the Firecracker development container
to run `cargo test -p vmm --lib migration::`. Its
`migration::e2e_tests::test_move_between_two_vmms` test boots a guest, copies
three rounds, pauses it, compares both memory digests, loads its state, then
resumes the target and checks that guest memory changes again. The two VMMs
run in one test process. API parser tests live in the firecracker crate.
