<div align="center">
  <img src="docs/assets/ibsend-mark.svg" width="104" alt="ibsend logo">
  <h1>ibsend</h1>
  <p><strong>Fast file transfers over InfiniBand.</strong></p>
  <p>
    <a href="https://github.com/HiroGitea/ibsend/actions/workflows/build.yml"><img alt="Build status" src="https://github.com/HiroGitea/ibsend/actions/workflows/build.yml/badge.svg?branch=master"></a>
    <img alt="Platform: Linux" src="https://img.shields.io/badge/platform-Linux-FCC624?logo=linux&logoColor=black">
    <img alt="RDMA: InfiniBand and RoCE" src="https://img.shields.io/badge/RDMA-InfiniBand%20%7C%20RoCE-7C3AED">
    <a href="#license"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue"></a>
  </p>
  <p><strong>English</strong> · <a href="docs/README.zh-CN.md">简体中文</a> · <a href="docs/README.ja.md">日本語</a></p>
</div>

ibsend is a file transfer tool for Linux hosts on an InfiniBand or RoCE network.
It sends files and directory trees directly between machines using RDMA, with
peer discovery, resumable transfers and an optional desktop interface.

The receiver buffers incoming data in memory while writing it to disk. This
allows the receiver to keep accepting data when disk writes temporarily fall
behind the network. QDR tests reached **3.47 GB/s**; see
[Performance](#performance) for the test setup and how transfer times were measured.

ibsend is intended for **trusted, private RDMA networks**. It does not provide
encryption or peer authentication. CLI and GUI messages are currently in Chinese;
documentation is available in English, Simplified Chinese and Japanese.

[Installation](#installation) · [Quick start](#quick-start) ·
[Commands](#commands) · [Containers](#containers-and-kubernetes) ·
[Performance](#performance) · [Limitations](#limitations)

## Features

- **Direct RDMA transfers.** File data and control messages use the same RDMA
  connection, without a separate TCP channel.
- **Adaptive memory buffering.** Pool sizing adjusts to available memory and
  memory-locking limits; disk writes run asynchronously.
- **Resume and verify.** Resume interrupted transfers from `.part` files and
  verify transferred data with CRC32C.
- **Multiple files and receiver discovery.** Send multiple paths in one command, discover
  receivers on the IPoIB subnet, or use the optional drag-and-drop GUI.
- **Containers and automation.** Run ibsend with Docker or Kubernetes, and
  integrate it into scripts with JSON output and fixed exit codes.

## Installation

Both machines need Linux, an RDMA-capable adapter and working RDMA connectivity.
For InfiniBand, configure IPoIB addresses before starting ibsend. Building from
source requires a recent stable Rust toolchain, a C compiler and the development
headers for `libibverbs` and `librdmacm` from `rdma-core`.

On Debian or Ubuntu, install the build dependencies:

```sh
sudo apt-get install build-essential libibverbs-dev librdmacm-dev
```

Build and install the CLI on both machines:

```sh
git clone https://github.com/HiroGitea/ibsend.git
cd ibsend
cargo install --path . --locked
```

Make sure `$HOME/.cargo/bin` is on `PATH`.

For the optional GUI, install the desktop dependencies and enable the `gui`
feature. On Debian or Ubuntu:

```sh
sudo apt-get install libwayland-dev libxkbcommon-dev
cargo install --path . --locked --features gui
```

This installs both `ibsend` and `ibsend-gui`. The default CLI build does not
require the GUI dependencies.

### Memory locking

RDMA requires registered memory to remain resident in RAM. If ibsend reports
that the memory-locking limit restricts its buffer pool, run:

```sh
ibsend authorize
```

The command requests `CAP_IPC_LOCK` for the installed binary through polkit or
`sudo`, or prints a command for manual setup. It makes no change when the
memory-locking limit is already sufficient. Restart running ibsend processes after
authorization; the GUI restarts automatically when authorized from its interface.

The permission applies to anyone running that binary. Rebuilding or reinstalling
can remove it, so authorization may be needed again after an update.

## Quick start

On the **receiving machine**, start a receiver named `nas`:

```sh
ibsend daemon --name nas --out ./received
```

Leave it running. Incoming files are saved under `./received`, relative to the
directory where the daemon was started.

On the **sending machine**, find the receiver and send a file or directory:

```sh
ibsend discover
ibsend send nas ./big.iso
ibsend send nas ./photos
```

These commands create `received/big.iso` and `received/photos/` on the receiving
machine, preserving the directory's relative paths. Multiple paths can be sent
together with `ibsend send nas ./big.iso ./photos`.

To connect by address, replace `nas` with the receiver's RDMA IPv4 address:

```sh
ibsend send 10.0.0.1 ./big.iso
```

Automatic interface selection and discovery use IPoIB. For RoCE, bind the
receiver explicitly with `--bind <RDMA-IPv4-address>` and send to that address
directly. The `nas` name above is an ibsend discovery name.

For desktop use, launch `ibsend-gui`, select a peer and add files by dragging them
into the window. Press **Ctrl+S** to send.

## Commands

| Command | Description |
|---|---|
| `ibsend daemon` | Keep a receiver running and available for discovery. |
| `ibsend discover` | List receivers on local IPoIB subnets. |
| `ibsend send <peer> <paths…>` | Send files or directories to a peer name or IPv4 address. |
| `ibsend recv` | Receive one transfer, then exit. |
| `ibsend authorize` | Set up memory-locking permission when needed. |
| `ibsend-gui [files…]` | Open the optional desktop interface. |

Options for `daemon` and `recv`:

| Option | Default | Description |
|---|---|---|
| `--bind <IP>` | First IPoIB address | Local RDMA address to listen on. |
| `--out <DIR>` | Current directory | Destination for received files. |
| `--name <NAME>` | Hostname | Name advertised during discovery. |
| `--pool <SIZE>` | Automatic | Size of the receiver's memory buffer pool. |
| `--slab <SIZE>` | Automatic | Transfer block size. |
| `--port <N>` | `18515` | `rdma_cm` port. |
| `--ready-file <PATH>` | None | File to create once the receiver is ready. |
| `--json` | Off | Write JSON events to standard output. |

Sizes accept `K`, `M` and `G` suffixes, using powers of 1024. For example,
`--pool 2G` requests a 2 GiB pool. The sender's `--slabs <N>` option controls
pipeline depth and defaults to 16, and `--prefix <DIR>` places files under a
subdirectory of the receiver's output directory. `discover --timeout <MS>` sets
the per-address resolution timeout and defaults to 300 ms. `send` and `discover`
also accept `--port` and `--json`; see
[Scripting interface](docs/containers.md#scripting-interface) for the JSON events
and exit codes. Run `ibsend` without arguments for usage information.

## Resume and verification

To resume an interrupted transfer, run the same send command again with the
same source files and destination. The receiver checks each destination path:

| Destination state | Behavior |
|---|---|
| A completed file with the expected size exists | Skip the file. |
| A partial `.part` file exists | Continue from its current byte offset. |
| Neither exists | Transfer the file from the beginning. |

Keep `.part` files to preserve resume progress. Completed files are skipped based
on **size only**; matching contents are not checked before skipping.

CRC32C checks compare the bytes read by the sender with those handled by the
receiver's writer thread. For resumed files, only bytes sent in the current
session are checked. The existing prefix is not revalidated, and destination
files are not read back from disk for verification. Check the receiver's output
for checksum results.

## Containers and Kubernetes

Build the container image from the repository root:

```sh
docker build -t ibsend .
```

Containers need the host's IPoIB network, the RDMA devices and the `IPC_LOCK`
capability. The `inbox` directory must be writable by UID 10001:

```sh
docker run -d --network host --device /dev/infiniband \
  --cap-drop ALL --cap-add IPC_LOCK --memory 8g \
  -v "$PWD/inbox:/data" ibsend daemon --out /data --name nas
```

The receive pool is sized from the container's memory limit. For Kubernetes, the
repository provides a Helm chart, plain manifests, example workloads and a
`kubectl ibsend` plugin. See [Containers and Kubernetes](docs/containers.md).

## Performance

The following results were recorded on two hosts with dual-port 40 Gb QDR
adapters and PCIe 2.0 x8 connections. IPoIB used connected mode with an MTU of
65520; the sender ran a rolling-release Linux distribution and the receiver ran
Debian 12. These results describe that setup; throughput varies with hardware
and workload.

For transfers smaller than the receiver's buffer pool:

| Sender pipeline | Transfer rate |
|---|---|
| 6 × 1 MB blocks | **3.47 GB/s** |
| 3 × 2 MB blocks | **3.47 GB/s** |
| 1 × 4 MB block | 1.94 GB/s |

In a separate transfer of approximately 2 GB to a ZFS target with a 1.5 GB
buffer pool:

| Measurement | Elapsed time | Rate |
|---|---|---|
| Sender data transfer | **0.62 s** | **3.48 GB/s** |
| Receiver, including file writes | 2.58 s | 831 MB/s |

**Sender time and receiver completion time measure different work.** Memory
buffering lets the sender finish while the receiver continues writing. Once
the pool fills, the sender waits for space and sustained throughput is limited
by the receiver's write rate. Actual performance also depends on source reads,
adapter and PCIe bandwidth, and available memory.

## How it works

ibsend uses a single reliable RDMA connection for file metadata, transfer control
and file data. The adapter writes incoming data directly into a registered
memory pool. A separate writer thread saves the data to disk and releases buffer
space for more incoming data. The daemon reuses the pool across sessions.

<p align="center">
  <img src="docs/assets/architecture.svg" width="900" alt="Files move from sender memory over RDMA into receiver memory, then to disk through a writer thread">
</p>

See the [design documentation](docs/design.md) for the protocol, buffering,
threading model and Rust library example.

## Limitations

- **Trusted networks only.** Transfers are neither encrypted nor authenticated.
  Restrict access at the RDMA fabric level.
- **File contents and relative paths.** Symlinks and special files are skipped;
  empty directories, file permissions, ownership and timestamps are not preserved.
- **Size-based skipping.** An existing file with the expected size is treated as
  complete, even if its contents differ. Resumed prefixes are not verified.
- **IPoIB discovery.** Automatic discovery scans IPv4 IPoIB subnets with at most
  4096 addresses. Use a peer's IPv4 address when discovery is unavailable.

## Contributing

Bug reports, documentation improvements and pull requests are welcome. See
[CONTRIBUTING.md](CONTRIBUTING.md) for the development setup, checks and the
hardware details to include in performance reports.

## License

Licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this work, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.
