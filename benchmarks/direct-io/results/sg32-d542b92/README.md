# SG32 verification on d542b92

The source was `d542b92ce37ca4542e1b27407ea47dd803254db8`. The binaries are identified by SHA-256. The existing ext4 harness ran guest readback and full backing file SHA-256 checks. All four Async/Sync and buffered/direct smoke cases passed. The host was compiling during the first smoke set, which includes a slow Async direct write.

The six runs in `ext4-async` were collected after the builds finished. They alternate buffered and direct Async on the same binary. The 128 MiB write rate includes the final guest fsync. Each run passed the same readback and file digest checks.

The CRC run used a 4096-byte-sector loop backing device, Async and Writeback. The guest saw 32 data segments and a virtual logical block size of 512 bytes. All three CRC32C profiles passed.
