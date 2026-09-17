//! ibsend —— InfiniBand 点对点文件传输库。
//!
//! 控制面和数据面都在同一条 RC QP 上，一个 TCP 都不用：
//!   - 控制面（清单、续传点、校验和）：QP 上的 SEND/RECV
//!   - 数据面：`RDMA_WRITE_WITH_IMM`，接收端 CPU 不参与数据搬运
//!
//! 全程不走 TCP 的好处是防火墙管不着：IPoIB 在内核眼里是普通网卡，TCP 要走完整
//! 的 netfilter；而 RDMA 完全绕开内核网络栈，iptables/ufw 对它无能为力。
//!
//! 接收端把数据先接进一大块注册内存（RAM 暂存），落盘线程按磁盘自己的节奏
//! 排空。只有整个池子被写满，发送端才会因为没有信用而阻塞——这样消费级
//! NVMe 打穿 SLC 缓存导致写入掉速时，链路仍然能跑满。
//!
//! ```no_run
//! use ibsend::{Incoming, Receiver, RecvConfig};
//! let mut rx = Receiver::start(RecvConfig::new("10.0.0.2"))?; // 立即返回，后台注册
//! loop {
//!     match rx.accept_one(2000)? {                              // 等对端，注册与之重叠
//!         Incoming::Transfer(m) => {
//!             rx.receive(&m, |_| {})?;
//!             rx.end_session()?;                                // 保留内存池，接着服务下一个
//!         }
//!         Incoming::Dropped(why) => eprintln!("丢弃：{why}"),
//!         _ => {}
//!     }
//! }
//! # Ok::<(), std::io::Error>(())
//! ```

pub mod authorize;
pub mod crc;
pub mod daemon;
pub mod discover;
mod ffi;
pub mod json;
pub mod proto;
pub mod recv;
pub mod send;
pub mod tune;
pub mod walk;

pub use proto::{Entry, Manifest, Reply, Trailer};
pub use daemon::{DaemonConfig, Event};
pub use discover::{scan, Peer};
pub use recv::{Incoming, RecvConfig, Receiver};
pub use walk::{expand, Item};
pub use send::{send_files, SendConfig};

/// 唯一的端口。控制面和数据面都在这条 QP 上，所以只需要一个 rdma_cm 端口号。
/// 注意它不是 TCP 端口——rdma_cm 有自己的端口空间，`ss -ltn` 里看不到它。
pub const PORT: u16 = 18515;

/// 一次传输的进度快照
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    /// 已经过网的字节数
    pub bytes: u64,
    /// 总字节数（来自清单）
    pub total: u64,
    /// 接收端：已收进 RAM 但还没落盘的字节数。这个值持续上涨说明磁盘跟不上，
    /// 涨到池子大小时发送端就会被流控卡住。
    pub staged: u64,
    pub elapsed: std::time::Duration,
}

impl Progress {
    pub fn rate(&self) -> f64 {
        let s = self.elapsed.as_secs_f64();
        if s > 0.0 { self.bytes as f64 / s } else { 0.0 }
    }
}

/// 一次传输的最终结果
#[derive(Debug, Clone, Default)]
pub struct Stats {
    /// 本次实际过网的字节数（不含续传时跳过的部分）
    pub bytes: u64,
    pub elapsed: std::time::Duration,
    /// 已完整存在、整个跳过的文件数
    pub skipped: usize,
    /// 从中途续传的文件数
    pub resumed: usize,
    /// CRC32C 校验通过的文件数（仅接收端）
    pub verified: usize,
    /// CRC32C 不匹配的文件名（仅接收端）。非空即表示数据有问题。
    pub bad: Vec<String>,
    /// 对端把文件放在了哪（仅发送端）
    pub peer_inbox: String,
}

impl Stats {
    pub fn rate(&self) -> f64 {
        let s = self.elapsed.as_secs_f64();
        if s > 0.0 { self.bytes as f64 / s } else { 0.0 }
    }
}
