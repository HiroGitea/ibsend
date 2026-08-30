//! 守护循环：内存池注册一次，之后循环服务发现探测和文件传输。
//!
//! 无头模式直接跑它；GUI 模式在后台线程里跑同一个东西，只是把事件送进 channel
//! 而不是打印。也就是说有头和无头共用同一份接收实现，不是两套。

use std::io;

use crate::recv::{Incoming, RecvConfig, Receiver};
use crate::{Manifest, Progress, Stats};

pub struct DaemonConfig {
    pub recv: RecvConfig,
}

/// 守护进程向前端汇报的事件
#[derive(Debug, Clone)]
pub enum Event {
    /// 内存池后台注册进度
    PoolProgress { done: u64, total: u64 },
    /// 池子就绪，开始服务
    Ready { pool: u64, bind: String },
    /// 有人探测了我们（发现）
    Probed,
    /// 一次传输开始
    Started(Manifest),
    /// 传输进行中
    Progress(Progress),
    /// 一次传输完成
    Done(Stats, Manifest),
    /// 一次传输失败——守护进程会继续服务下一个
    Failed(String),
}

/// 跑守护循环。除非 accept 本身出错（端口被抢之类），否则不返回。
pub fn run<F: FnMut(Event)>(cfg: DaemonConfig, mut on: F) -> io::Result<()> {
    let bind = cfg.recv.bind.clone();
    let mut rx = Receiver::start(cfg.recv)?;

    // 注册在后台跑，这里只是把进度转出去
    loop {
        let (done, total, ready) = rx.pool_progress()?;
        if ready {
            on(Event::Ready { pool: total, bind: bind.clone() });
            break;
        }
        on(Event::PoolProgress { done, total });
        std::thread::sleep(std::time::Duration::from_millis(80));
    }

    loop {
        let m = match rx.accept_one(2000) {
            Ok(Incoming::Idle) => continue,
            Ok(Incoming::Probe) => {
                on(Event::Probed);
                continue;
            }
            Ok(Incoming::Dropped(why)) => {
                on(Event::Failed(format!("丢弃一条连接：{why}")));
                continue;
            }
            Ok(Incoming::Transfer(m)) => m,
            Err(e) => return Err(e), // 监听端本身坏了，没法继续
        };

        on(Event::Started(m.clone()));
        let r = rx.receive(&m, |p| on(Event::Progress(p)));
        match r {
            Ok(st) => on(Event::Done(st, m)),
            // 单次传输失败不该拖垮守护进程
            Err(e) => on(Event::Failed(e.to_string())),
        }
        if let Err(e) = rx.end_session() {
            on(Event::Failed(format!("会话收尾失败：{e}")));
        }
    }
}

/// 事件的单行文字表示，无头模式和 GUI 日志都用它
pub fn describe(ev: &Event) -> Option<String> {
    let human = |b: f64| {
        const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
        let (mut v, mut i) = (b, 0);
        while v >= 1000.0 && i < U.len() - 1 {
            v /= 1000.0;
            i += 1;
        }
        format!("{v:.2} {}", U[i])
    };
    Some(match ev {
        Event::Ready { pool, bind } => format!("就绪：{} 内存池，监听 {}", human(*pool as f64), bind),
        Event::Probed => return None, // 太吵，不打印
        Event::Started(m) => format!("接收 {} 个文件，共 {}", m.files.len(), human(m.total() as f64)),
        Event::Done(st, _) => {
            let mut s = format!("完成 {} / {:.2}s = {}/s",
                human(st.bytes as f64), st.elapsed.as_secs_f64(), human(st.rate()));
            if st.skipped > 0 { s += &format!("，跳过 {} 个", st.skipped); }
            if st.resumed > 0 { s += &format!("，续传 {} 个", st.resumed); }
            if !st.bad.is_empty() { s += &format!("，✗ {} 个校验失败", st.bad.len()); }
            s
        }
        Event::Failed(e) => format!("失败：{e}"),
        Event::PoolProgress { .. } | Event::Progress(_) => return None,
    })
}
