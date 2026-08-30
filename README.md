# ibsend

**English** · [简体中文](README.zh-CN.md) · [日本語](README.ja.md)

Peer-to-peer file transfer over InfiniBand. Both the control plane and the data
plane ride a single RC queue pair — there is no TCP anywhere in the path.

- **The receiver's CPU never touches the payload.** Data arrives by
  `RDMA_WRITE_WITH_IMM` straight into registered memory.
- **RAM staging.** The receiver lands data in a large registered pool and a
  writer thread drains it to disk at whatever pace the disk manages. The sender
  finishes at line rate even when the disk cannot keep up.
- **Firewalls cannot get in the way.** IPoIB is an ordinary NIC as far as the
  kernel is concerned, so TCP would traverse the full netfilter path — but RDMA
  bypasses the kernel network stack entirely.

Every node is symmetric: each runs the same daemon, is discoverable, and can
both send and receive. A GUI is available but is only another front end over
the same daemon.

> **Note:** the CLI and GUI messages are currently Chinese only. The
> documentation is available in English, Chinese and Japanese.

## Requirements

- An InfiniBand or RoCE HCA with IPoIB configured. IPoIB is used only for
  address resolution (`rdma_resolve_addr` → GID) and subnet scanning.
- `rdma-core` (libibverbs, librdmacm) plus its headers to build.
- Linux and a recent Rust toolchain.
- Permission to lock memory — see [Memory locking](#memory-locking).

## Build

```sh
cargo build --release                    # CLI only
cargo build --release --features gui     # CLI + GUI (pulls in eframe)
```

The GUI is behind a feature flag on purpose: headless nodes should not have to
carry the OpenGL dependency chain.

## Quick start

```sh
# On every node — discoverable, ready to receive
ibsend daemon --out /data

# See who is on the fabric
ibsend discover

# Send a file or a whole directory, by address or by device name
ibsend send 10.0.0.1 ./big.iso
ibsend send nas ./photos
```

Directories are expanded recursively with relative paths preserved; the
receiver rebuilds the tree. Symlinks are skipped rather than followed —
following them invites cycles, quietly pulls in data from outside the tree, and
makes "what did I actually send" unpredictable.

## Memory locking

RDMA registration pins physical pages, and the default `RLIMIT_MEMLOCK` is 8 MB
on most distributions. That is far too small for the staging pool, which is
where most of this design's value lives.

The narrow fix is to grant this one binary `CAP_IPC_LOCK` — the kernel's check
in `ib_umem_get` is "over the limit **and** lacking CAP_IPC_LOCK", so the
capability sidesteps the limit without relaxing it for every other process the
user runs.

`ibsend authorize` picks whichever route the environment supports:

| Environment | How |
|---|---|
| Desktop (`DISPLAY`/`WAYLAND_DISPLAY`) | polkit dialog, or the **Authorize** button in the GUI |
| SSH or headless, with a terminal | invokes `sudo`, which asks for the password right there |
| Scripts, no terminal | prints `sudo setcap …` to run by hand |

It stays quiet when the available pool is already large enough; there is no
reason to ask for a password just to make a comfortable pool larger.

Two things worth knowing:

- **The capability lives in the binary's extended attributes**, not in a
  per-run elevation. Once granted, anyone running that file carries it.
- **File capabilities load at `execve`**, so the granting process cannot use
  them; a restart is required. The GUI restarts itself.
- Rebuilding replaces the file and drops the capability.

Packages should do this in a post-install hook instead:

```sh
setcap cap_ipc_lock+ep /usr/bin/ibsend
```

## Commands

| Command | Purpose |
|---|---|
| `ibsend daemon` | Stay resident: discoverable and receiving |
| `ibsend discover` | Scan the IPoIB subnet for peers |
| `ibsend send <peer> <paths…>` | Send files or directories |
| `ibsend recv` | Receive once, then exit |
| `ibsend authorize` | Request memory-locking permission |
| `ibsend-gui [files…]` | Graphical front end (drag and drop, Ctrl+S to send) |

`--bind` defaults to the first IPoIB interface. Pool size and slab carving
adapt automatically; `--pool`, `--slab` and `--name` override them.

## Library

```rust
use ibsend::{Incoming, Receiver, RecvConfig};

let mut rx = Receiver::start(RecvConfig::new("10.0.0.2"))?; // returns immediately
loop {
    match rx.accept_one(2000)? {          // registration overlaps with waiting
        Incoming::Transfer(m) => {
            rx.receive(&m, |p| println!("{} B/s, staged {} B", p.rate(), p.staged))?;
            rx.end_session()?;            // keeps the pool, serves the next peer
        }
        Incoming::Dropped(why) => eprintln!("dropped: {why}"),
        _ => {}
    }
}
```

Modules: `proto` (wire format), `tune` (adaptive sizing), `walk` (directory
expansion and path sanitising), `discover`, `recv`, `send`, `daemon`,
`authorize`, `crc`. `ffi` is private. The CLI only parses arguments and renders
progress.

## How it works

**Chunk numbers never go on the wire.** An RC queue pair preserves order, so the
receiver simply counts arrivals; the full 32-bit immediate carries the chunk's
byte count instead. That is byte-exact and supports streaming without knowing
the total length in advance. `imm == 0` ends the stream.

**Connect, then negotiate, then register.** With the control plane on the queue
pair the ordering works out naturally: establish the QP, agree on the carving
over it, then each side registers its pool. When the control plane was TCP this
had to run backwards, which forced pool details into `rdma_cm`'s
`private_data`.

**The credit window is the whole pool.** The receiver returns "slabs released"
as it drains; the sender writes only within that window. Data therefore piles up
in RAM rather than throttling the sender, until the pool genuinely fills.

**Register once, serve many.** Ending a session tears down the queue pair but
keeps the registered pool, so a daemon pays the registration cost only at
startup. How the pool is carved into slabs is pure arithmetic, so it can be
re-decided per transfer for free.

**Registration is asynchronous.** Allocation, page prefault and `ibv_reg_mr` run
on a background thread that overlaps with waiting for a peer, so startup has no
perceptible stall.

**2 MB huge pages.** The pool is 2 MB aligned and marked `MADV_HUGEPAGE`. The
address-translation cache on older HCAs is small; 16 GB with 4 KB pages needs
four million entries, and huge pages divide that by 512.

**One I/O thread on each side.** The sender has a reader thread, the receiver a
writer thread; the main thread does nothing but queue-pair work, because
libibverbs is not thread safe. `ibx_recv_wake` is the sole exception — it only
writes eight bytes to an eventfd and touches no verbs object.

## Design notes

Things that were not obvious, recorded so they are not rediscovered the hard
way:

**Receive work requests are consumed in posting order, regardless of message
type.** Mixing buffer sizes on one queue means a large message can land in a
small slot and fail with `local length error`. All receive slots are therefore
the same size, and the message kind is carried in a frame header flag.

**Never overload a return-value space.** `poll_cq` style helpers return "how
many completions were processed"; using a small positive number to also mean
"timed out" produced an intermittent handshake hang, because the run where
exactly that many completions were batched together was misread as a timeout.

**Do not use a timeout to notice that a background thread finished.** The writer
thread's completion is invisible to a main loop blocked in the receive path, so
credits are only returned when the timeout expires. With a tight credit window
that degenerates into one timeout period per chunk. An eventfd wake removes it.

**Prefetch depth must respect both windows.** Prefetching against the local slab
count alone deadlocks when the peer's credit window is smaller: prefetch fills
the window, credits need data to arrive, data needs a commit, and the commit sits
after the prefetch loop.

**Distinguish retryable from fatal.** A connect failure caused by the peer not
being ready is worth retrying; one caused by a local resource limit never is.
Retrying the latter turns an instant, clear error into a silent multi-minute
hang.

**Every timeout earns its place.** One guards against mutual waiting, one stops
a half-dead sender from wedging the daemon forever, and an idle timeout catches
a peer that connects and then says nothing.

**Dropped connections must carry a reason.** Silently swallowing a handshake
error turns "the peer connected and then nothing happened" into something nobody
can diagnose.

**Keep the magic constant and version separately.** Mixed versions are
inevitable once something is distributed. Both sides now report
"protocol version mismatch: peer vN, local vM" within milliseconds instead of
one side disconnecting quietly while the other waits for a timeout.

**Watch out for glibc symbol creep.** Building on a rolling distribution can
silently produce binaries that will not start on a stable one — `atoi` pulls in
`__isoc23_strtol` (2.38) and `std::process::Command` pulls in `pidfd_spawnp`
(2.39). The CLI avoids both and needs nothing newer than glibc 2.34.

## Measured

Two hosts, dual-port 40 Gb QDR HCAs, IPoIB in connected mode with a 65520 byte
MTU, PCIe 2.0 x8. Sender on a rolling-release Linux, receiver on Debian 12.

**Link ceiling** (transfer smaller than the pool, so flow control never engages):

| Sender slabs in flight | Rate |
|---|---|
| 6 × 1 MB | **3.47 GB/s** |
| 3 × 2 MB | **3.47 GB/s** |
| 1 × 4 MB | 1.94 GB/s |

3.47 GB/s is 27.8 Gb/s — 87 % of QDR's 32 Gb/s data rate after 8b/10b encoding,
and simultaneously right at the practical ceiling of PCIe 2.0 x8. Both limits
happen to land on the same number.

The last row shows pipelining is not optional: with a single slab the sender
must wait for each write to complete before refilling, so reading and sending
serialise and throughput drops to 56 % of peak.

**What RAM staging buys** (2 GB onto a ZFS pool, 1.5 GB staging pool):

| | Elapsed | Rate |
|---|---|---|
| Sender | **0.62 s** | 3.48 GB/s |
| Receiver, including drain | 2.58 s | 831 MB/s |
| Peak staged in RAM | | 481 MB |

The disk only absorbs 831 MB/s, yet the sender was done in 0.62 s — the pool
swallowed the 4.2× difference. Without staging the sender would have been
throttled to disk speed and taken the full 2.58 s.

Flow control engaging is directly observable: as staged bytes climbed from
839 MB to 975 MB against a 1.07 GB pool, the rate fell from 3.49 GB/s to
2.60 GB/s.

Every transfer's SHA-256 matched and every CRC32C check passed.

## Resume

Re-run the same command. The receiver scans the destination: a file that already
exists at the right size is skipped entirely, a `.part` file is resumed from its
current length, and everything else is sent in full. So `.part` files are not
litter to clean up — they are the resume point.

Skipping is decided per file (matching size counts as complete); resuming is
byte-granular.

## Checksums

After the data drains, the sender sends a CRC32C per file over the control
channel and the receiver compares. The receiver computes it on the writer
thread, which is waiting on disk anyway; SSE4.2's `crc32` instruction runs at
roughly 8 GB/s (`cargo run --release --example crcbench`), well past the link.

Be clear about what this catches. **Link integrity is already offloaded**: every
IB packet carries ICRC and VCRC checked by the HCA, and RC retransmits in
hardware, so corruption on the wire is not a thing a software checksum could
find. What it catches is this program's own bugs, non-ECC memory flips and disk
write errors — a class that cannot be offloaded to older HCAs, since signature
and T10-DIF offload arrived with later generations.

**A resumed file is only checked over the bytes sent this session.** Re-reading
the existing prefix would cost about as much as retransmitting it, which would
defeat the point of resuming. The output says so explicitly.

## Limitations

- Empty directories are not transferred (only files are).
- No encryption and no authentication; the IB subnet is assumed to be a trusted
  private network. Note that RDMA traffic cannot be filtered with `iptables`
  either — its access control lives in the rkey, the partition key and the
  subnet manager.
- Skip detection compares file size only, not content.
