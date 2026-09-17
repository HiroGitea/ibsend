<div align="center">
  <img src="assets/ibsend-mark.svg" width="104" alt="ibsend 标志">
  <h1>ibsend</h1>
  <p><strong>面向 InfiniBand 网络的高速文件传输工具。</strong></p>
  <p>
    <a href="https://github.com/HiroGitea/ibsend/actions/workflows/build.yml"><img alt="构建状态" src="https://github.com/HiroGitea/ibsend/actions/workflows/build.yml/badge.svg?branch=master"></a>
    <img alt="平台：Linux" src="https://img.shields.io/badge/platform-Linux-FCC624?logo=linux&logoColor=black">
    <img alt="RDMA：InfiniBand 与 RoCE" src="https://img.shields.io/badge/RDMA-InfiniBand%20%7C%20RoCE-7C3AED">
    <a href="#许可证"><img alt="许可证：MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue"></a>
  </p>
  <p><a href="../README.md">English</a> · <strong>简体中文</strong> · <a href="README.ja.md">日本語</a></p>
</div>

ibsend 用于在 InfiniBand 或 RoCE 网络中的 Linux 主机之间传输文件。它通过 RDMA
直接传输文件和目录，支持设备发现和断点续传，也提供可选的图形界面。

接收端将收到的数据暂存在内存中，并同时写入磁盘。磁盘写入暂时跟不上网络速度时，
只要缓冲池还有空间，就能继续接收数据。QDR 测试中的传输速率达到 **3.47 GB/s**，
测试环境和耗时的测量方式见[性能](#性能)。

ibsend 适用于**受信任的私有 RDMA 网络**，不提供传输加密或对端身份验证。
命令行和图形界面目前使用中文，文档提供英文、简体中文和日文版本。

[安装](#安装) · [快速上手](#快速上手) · [命令](#命令) ·
[性能](#性能) · [使用限制](#使用限制)

## 主要功能

- **RDMA 直接传输**：文件数据和控制消息共用一条 RDMA 连接，无需独立的 TCP 通道。
- **自适应内存缓冲**：根据可用内存和内存锁定上限调整缓冲池大小，并异步写入磁盘。
- **断点续传与校验**：从中断留下的 `.part` 文件继续传输，并用 CRC32C 校验传输的数据。
- **批量传输与设备发现**：一条命令发送多个路径，通过 IPoIB 子网发现接收端，
  也可使用图形界面拖拽发送。

## 安装

两台机器均需运行 Linux，配备支持 RDMA 的网卡，并已配置好 RDMA 网络连接。
使用 InfiniBand 时，请先配置 IPoIB 地址。从源码构建需要较新的稳定版 Rust
工具链、C 编译器，以及 `rdma-core` 中 `libibverbs` 和 `librdmacm` 的开发头文件。

Debian / Ubuntu 可通过以下命令安装构建依赖：

```sh
sudo apt-get install build-essential libibverbs-dev librdmacm-dev
```

在两台机器上分别构建并安装命令行工具：

```sh
git clone https://github.com/HiroGitea/ibsend.git
cd ibsend
cargo install --path . --locked
```

安装后，请确认 `$HOME/.cargo/bin` 已加入 `PATH`。

如需使用图形界面，请安装所需的依赖，并在构建时启用 `gui` 功能。Debian / Ubuntu 的安装命令如下：

```sh
sudo apt-get install libwayland-dev libxkbcommon-dev
cargo install --path . --locked --features gui
```

这会同时安装 `ibsend` 和 `ibsend-gui`。默认的命令行构建不需要图形界面依赖。

### 内存锁定

RDMA 要求注册的内存常驻 RAM。如果 ibsend 提示内存锁定上限限制了缓冲池大小，执行：

```sh
ibsend authorize
```

该命令通过 polkit 或 `sudo` 为已安装的程序申请 `CAP_IPC_LOCK` 权限；无法交互
授权时，会显示手动配置命令。当前内存锁定上限足够时，不会修改权限。授权后需重启正在运行的
ibsend；通过图形界面授权时，界面会自动重启。

该权限对所有运行此程序的用户生效。重新编译或安装可能清除该权限，更新后
可能需要再次授权。

## 快速上手

在**接收端**启动接收服务，并将其命名为 `nas`：

```sh
ibsend daemon --name nas --out ./received
```

保持该进程运行。接收的文件会保存到启动目录下的 `./received` 中。

在**发送端**查找接收设备，然后发送文件或目录：

```sh
ibsend discover
ibsend send nas ./big.iso
ibsend send nas ./photos
```

上述命令会在接收端生成 `received/big.iso` 和 `received/photos/`，保留目录内的
相对路径。也可以用 `ibsend send nas ./big.iso ./photos` 一次发送多个路径。

如需按地址连接，将 `nas` 替换为接收端的 RDMA IPv4 地址：

```sh
ibsend send 10.0.0.1 ./big.iso
```

接口自动选择和设备发现都依赖 IPoIB。使用 RoCE 时，请在接收端通过
`--bind <RDMA-IPv4-address>` 显式指定监听地址，并在发送端直接使用该地址。
示例中的 `nas` 是接收服务在设备发现时使用的名称。

使用图形界面时，启动 `ibsend-gui`，选择接收设备，将文件拖入窗口，然后按 **Ctrl+S** 发送。

## 命令

| 命令 | 说明 |
|---|---|
| `ibsend daemon` | 持续运行接收服务，并响应设备发现。 |
| `ibsend discover` | 列出本地 IPoIB 子网中的接收设备。 |
| `ibsend send <peer> <paths…>` | 通过设备名称或 IPv4 地址指定接收端，发送文件或目录。 |
| `ibsend recv` | 接收一次传输后退出。 |
| `ibsend authorize` | 按需配置内存锁定权限。 |
| `ibsend-gui [files…]` | 启动可选的图形界面。 |

`daemon` 和 `recv` 支持以下选项：

| 选项 | 默认值 | 说明 |
|---|---|---|
| `--bind <IP>` | 第一个 IPoIB 地址 | 本机用于监听的 RDMA 地址。 |
| `--out <DIR>` | 当前目录 | 接收文件的保存目录。 |
| `--name <NAME>` | 主机名 | 设备发现时显示的名称。 |
| `--pool <SIZE>` | 自动调整 | 接收端内存缓冲池大小。 |
| `--slab <SIZE>` | 自动调整 | 传输块大小。 |

大小参数支持 `K`、`M`、`G` 后缀，按 1024 的幂计算，例如 `--pool 2G` 表示申请
2 GiB 缓冲池。发送端的 `--slabs <N>` 控制流水线深度，默认为 16；
`discover --timeout <MS>` 设置单个地址的解析超时，默认为 300 毫秒。
不带参数运行 `ibsend` 可查看用法。

## 续传与校验

传输中断后，保持源文件和目标目录一致，重新执行同一条发送命令即可。
接收端会逐个检查目标路径：

| 目标状态 | 处理方式 |
|---|---|
| 已存在大小符合预期的完整文件 | 跳过该文件。 |
| 存在尚未完成的 `.part` 文件 | 从当前字节位置继续传输。 |
| 两者均不存在 | 从头传输。 |

请保留 `.part` 文件，以便下次续传。完整文件的跳过判断**只比较大小**，不比较内容。

CRC32C 用于检查发送端读取的数据与接收端写入线程处理的数据是否一致。续传时只校验本次
发送的部分，已有部分不会重新校验，也不会重新读取磁盘上的目标文件进行校验。
校验结果请查看接收端输出。

## 性能

以下数据来自两台配备双端口 40 Gb QDR 网卡、PCIe 2.0 x8 接口的主机。
IPoIB 使用 connected 模式，MTU 为 65520；发送端运行滚动发行版 Linux，
接收端运行 Debian 12。数据反映该环境下的测试结果，实际速率随硬件和负载变化。

传输的数据量小于接收端缓冲池容量时：

| 发送端流水线配置 | 传输速率 |
|---|---|
| 6 × 1 MB 块 | **3.47 GB/s** |
| 3 × 2 MB 块 | **3.47 GB/s** |
| 1 × 4 MB 块 | 1.94 GB/s |

另一组测试使用 1.5 GB 缓冲池，将约 2 GB 数据写入 ZFS 存储：

| 测量项 | 耗时 | 速率 |
|---|---|---|
| 发送端数据传输 | **0.62 s** | **3.48 GB/s** |
| 接收端（包含文件写入） | 2.58 s | 831 MB/s |

**发送端耗时只统计数据发送，接收端耗时还包括文件写入。** 内存缓冲允许发送端先完成数据发送，
接收端随后继续写入文件。缓冲池满后，发送端会等待可用空间，持续吞吐量受接收端
写入速度限制。实际性能还取决于源文件读取速度、网卡与 PCIe 带宽，以及可用内存。

## 工作原理

ibsend 通过一条可靠的 RDMA 连接交换文件清单、控制消息和文件数据。网卡将接收的
数据直接写入已注册的内存池，独立的写入线程负责写入文件，并释放缓冲空间。
接收服务在不同传输会话之间复用同一内存池。

<p align="center">
  <img src="assets/architecture.svg" width="900" alt="文件从发送端内存经 RDMA 进入接收端内存，再由写入线程写入磁盘">
</p>

协议、内存管理、线程模型和 Rust 库使用示例见[设计文档（英文）](design.md)。

## 使用限制

- **仅用于可信网络**：不提供加密和身份验证，应在 RDMA 网络层限制访问范围。
- **仅保留文件内容与相对路径**：跳过符号链接和特殊文件；不保留空目录、文件权限、
  所有者和时间戳。
- **按大小跳过文件**：目标文件大小符合预期时即视为完整，即使内容不同也会跳过；
  续传时不校验已有部分。
- **IPoIB 设备发现**：自动扫描仅覆盖地址数不超过 4096 的 IPv4 IPoIB 子网。
  无法发现设备时，可直接使用对端 IPv4 地址。

## 参与贡献

欢迎提交问题、改进文档或发起 Pull Request。开发环境、检查命令，以及性能报告
所需的硬件信息见[贡献指南](../CONTRIBUTING.md)。

## 许可证

本项目采用 [MIT](../LICENSE-MIT) 或 [Apache-2.0](../LICENSE-APACHE) 许可证，
使用者可任选其一。

除非另行声明，有意提交并纳入本项目的任何贡献（按 Apache-2.0 的定义），
都将以上述双许可发布，不附加任何额外条款。
