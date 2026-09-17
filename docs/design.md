# ibsend design

This document describes the transport and implementation for contributors and
applications using the Rust library. For installation and usage, see the
[README](../README.md).

## Transport

Each connection uses one reliable connected (RC) queue pair. Control messages
use `SEND`/`RECV`; file data uses `RDMA_WRITE_WITH_IMM`. There is no separate TCP
control channel. `rdma_cm` resolves addresses and establishes connections using
the port defined by `ibsend::PORT` (18515).

The receiver starts registering its memory pool in the background while waiting
for connections. After connecting, the sender provides a file manifest and its
memory budget. The receiver reports existing file lengths, negotiates the slab
layout and provides the remote memory information. The sender then registers its
local pool and confirms that it is ready to transfer.

The immediate value carries the payload length in bytes; zero marks the end of
the stream. Chunk sequence numbers remain local because RC preserves arrival
order. A slab can span file boundaries, allowing small files to share a block.

## Buffering and flow control

The receiver's registered pool is divided into slabs. Its credit window covers
the whole pool: the sender may write only to slabs for which it has credit, and
the receiver returns credits as its writer thread releases them.

This decouples network transfer from file writes while free buffer space remains.
A full pool applies backpressure to the sender. Buffering therefore helps absorb
temporary storage slowdowns, but cannot raise the sustained write rate of the
destination storage.

Ending a session releases its queue pair and retains the registered receiver
pool. Subsequent sessions can choose a different slab layout without registering
the pool again. Allocation, page prefaulting and registration run on a background
thread. The pool is aligned to 2 MiB and marked `MADV_HUGEPAGE` to request
transparent huge pages; whether they are used depends on the host configuration.

## Threads and buffer ownership

The sender has a file reader thread and the receiver has a file writer thread.
Each process's main transfer thread owns the queue-pair operations. A slab is
handed to an I/O thread only while it is available for that thread's use.

After consuming a slab, the writer signals an `eventfd` through `ibx_recv_wake`.
This wakes the receive loop so it can return credits promptly. The wake function
does not access verbs objects and is the wrapper's supported cross-thread
notification path.

Prefetch depth is bounded by both local slab availability and the remote credit
window. Considering only the local pool can leave the sender waiting for credits
before it has committed the data needed to release them.

## Control messages and errors

- **Receive slots have a uniform size.** Receive work requests are consumed in
  posting order. A shared slot size accommodates either a control frame or an
  immediate notification; frame flags identify control-message kinds.
- **Completion counts and timeout results are distinct.** Polling helpers must
  not use a positive completion count to also represent a timeout.
- **Retries depend on the failure.** Connection attempts can retry when a peer
  is not ready. Local resource failures are reported immediately.
- **Timeouts bound waiting.** Handshake and idle timeouts prevent an unresponsive
  peer from holding the receiver indefinitely. Writer completion uses an explicit
  wakeup instead of waiting for a polling timeout.
- **Connection failures include a reason.** Dropped handshakes are surfaced to
  the caller. The protocol carries a fixed magic value and a separate version,
  allowing peers to report incompatible versions explicitly.

## Resume and checksums

The receiver compares destination file sizes with the manifest. Completed files
of the expected size are skipped; partial `.part` files supply resume offsets.
The writer renames each partial file to its final name when its bytes have been
written. CRC32C comparison follows at the end of the session.

The sender computes CRC32C while reading and sends per-file checksums in a
control trailer. The receiver computes matching checksums on its writer thread.
Resumed files are checked only over bytes sent in that session; skipped files
are not checked. Verification compares the transfer buffers, without reading
destination files back from storage. The writer does not explicitly call
`fsync`, so receiver completion does not guarantee persistence across power loss.

The sender's reported elapsed time ends after its data work requests complete,
before the checksum trailer and connection teardown. The receiver's elapsed time
includes the writer finishing, but excludes the final checksum comparison. The
sender does not wait for an acknowledgment of the receiver's checksum result.

On x86-64, CRC32C uses SSE4.2 when available and otherwise falls back to software.
Other architectures use the software implementation. To measure checksum
throughput locally:

```sh
cargo run --release --locked --example crcbench
```

## Rust library

The CLI and GUI share the same library. `daemon::run` provides the persistent
receiver loop; `Receiver` exposes individual sessions:

```rust
use ibsend::{Incoming, Receiver, RecvConfig};

fn main() -> std::io::Result<()> {
    let mut config = RecvConfig::new("10.0.0.2");
    config.out = "./received".into();
    let mut receiver = Receiver::start(config)?;

    loop {
        match receiver.accept_one(2000)? {
            Incoming::Transfer(manifest) => {
                let result = receiver.receive(&manifest, |progress| {
                    println!("{} B/s, buffered {} B", progress.rate(), progress.staged);
                });
                receiver.end_session()?;
                let stats = result?;
                if !stats.bad.is_empty() {
                    eprintln!("CRC32C mismatch: {:?}", stats.bad);
                }
            }
            Incoming::Dropped(reason) => eprintln!("Connection dropped: {reason}"),
            Incoming::Probe | Incoming::Idle => {}
        }
    }
}
```

Generate the API documentation with `cargo doc --no-deps --open`.

| Module | Responsibility |
|---|---|
| `send`, `recv`, `daemon` | Transfers, receiver sessions and the daemon loop. |
| `proto` | Wire format and protocol versioning. |
| `tune` | Memory budgets from memlock and cgroup limits, and slab sizing. |
| `walk` | Directory traversal and relative-path validation. |
| `discover` | IPoIB subnet scanning and peer information. |
| `authorize` | Memory-locking permission setup. |
| `crc` | CRC32C implementations. |
| `json` | JSON event encoding for the CLI's `--json` output. |
| `ffi` (private) | Bindings to the C transport in `csrc/ibx.c`. |

## Packaging

Packages can grant memory-locking permission to the installed binary with a
post-install step, using the actual installation path:

```sh
setcap cap_ipc_lock+ep /usr/bin/ibsend
```

File capabilities apply on the next execution and need to be reapplied when the
binary is replaced. The GUI binary needs its own capability if packaged.

The container image cannot rely on `ibsend authorize`. It ships a second copy
of the binary with the capability set, and its entrypoint selects that copy only
when `IPC_LOCK` is in the capability bounding set; otherwise `execve` would fail
with `EPERM`. See [Containers and Kubernetes](containers.md#memory-locking).

The C build explicitly selects GNU C11 to avoid introducing C23-specific glibc
symbols through functions such as `atoi`. Process launching uses `fork` and
`execv` to avoid a newer `pidfd_spawnp` dependency. When distributing binaries,
check their actual glibc requirements against the target distribution.
