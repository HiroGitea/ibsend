# Contributing to ibsend

Thanks for helping improve ibsend. Bug reports, documentation fixes and focused
pull requests are welcome.

## Before opening an issue

Please include the Linux distribution, kernel, HCA model, `rdma-core` version,
link mode (InfiniBand or RoCE), IPoIB configuration and the complete command
output. For performance reports, also include PCIe generation/width, MTU, file
size, storage type and the pool/slab settings.

Security-sensitive reports should not be posted publicly. Use GitHub's
**Report a vulnerability** flow when it is available for the repository.

## Development setup

ibsend requires Linux, a recent Rust toolchain and the `rdma-core` development
headers (`libibverbs` and `librdmacm`). The GUI also needs the platform packages
required by `eframe`.

```sh
cargo build
cargo build --features gui
cargo test
```

Tests that exercise the real transfer path require two RDMA-capable hosts.
Unit tests cover the protocol, path handling, adaptive tuning and CRC code and
can run without establishing an RDMA connection.

## Pull requests

- Keep changes focused and explain the problem they solve.
- Add or update tests when behavior changes.
- Keep the English, Simplified Chinese and Japanese README files aligned when
  changing shared documentation.
- Run `cargo test` before submitting.

Unless stated otherwise, contributions are accepted under the repository's
MIT OR Apache-2.0 dual license.
