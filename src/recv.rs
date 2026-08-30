//! 接收端：后台注册内存池 → 在 QP 上等连接 → 控制面协商 → 收数据 → 异步落盘

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use crate::crc::Crc32c;
use crate::proto::{self, Entry, Manifest, PeerInfo, Reply, Trailer};
use crate::{ffi, tune, Progress, Stats, PORT};

pub struct RecvConfig {
    /// 必须是具体的 IPoIB 地址：只有绑定到具体地址，rdma_cm 才能提前确定
    /// 用哪块 HCA，内存池的注册才能在等待连接之前就开始。
    pub bind: String,
    pub port: u16,
    /// 池子大小，None = 按可用内存和 memlock 自适应
    pub pool_bytes: Option<u64>,
    /// slab 大小，None = 按传输大小自适应
    pub slab: Option<u64>,
    /// 被发现时告诉对方的一句话（链路速率之类）
    pub note: String,
    /// 对外显示的设备名。空则用主机名。
    pub name: String,
    /// 收到的文件放哪。会在发现应答里告诉对端，让人发之前就知道东西会去哪。
    pub out: PathBuf,
}

impl RecvConfig {
    pub fn new(bind: impl Into<String>) -> Self {
        RecvConfig {
            bind: bind.into(),
            port: PORT,
            pool_bytes: None,
            slab: None,
            note: link_note(),
            name: String::new(),
            out: PathBuf::from("."),
        }
    }
}

/// 展示用的绝对路径。相对路径（比如默认的 "."）对远端毫无意义。
fn display_path(p: &Path) -> String {
    std::fs::canonicalize(p)
        .unwrap_or_else(|_| p.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// 一次连接的性质
pub enum Incoming {
    /// 对端要传文件
    Transfer(Manifest),
    /// 探测已应答
    Probe,
    /// 这段时间没人连上来，调用方可以去干别的
    Idle,
    /// 连接被丢弃了，附带原因。必须带上原因：静默吞掉握手错误会让
    /// 「对端明明连上了却什么都没发生」变成完全不可诊断的问题。
    Dropped(String),
}

/// 读本机 IB 端口速率，用作被发现时的自我介绍
pub fn link_note() -> String {
    let Ok(dir) = std::fs::read_dir("/sys/class/infiniband") else { return String::new() };
    for e in dir.flatten() {
        if let Ok(s) = std::fs::read_to_string(e.path().join("ports/1/rate")) {
            return s.trim().to_string();
        }
    }
    String::new()
}

/// 把对端给的名字变成 (最终路径, .part 路径)。
///
/// 允许子目录以便重建目录结构，但名字来自网络，必须先过 `walk::safe_relpath`
/// 那一关；实在不安全就落到一个固定的占位名，绝不按对方给的路径写。
fn dest(dir: &Path, name: &str, idx: usize) -> (PathBuf, PathBuf) {
    let rel = crate::walk::safe_relpath(name)
        .unwrap_or_else(|| PathBuf::from(format!("unsafe-name-{idx}.bin")));
    let fin = dir.join(&rel);
    let file = fin.file_name().map(|s| s.to_os_string()).unwrap_or_default();
    let part = fin.with_file_name(format!("{}.part", file.to_string_lossy()));
    (fin, part)
}

/// 扫描目标目录，算出每个文件已经有多少字节。
/// 完整存在 → 整个跳过；有 .part → 从那里续；其余从头传。
fn scan_existing(dir: &Path, files: &[Entry]) -> Vec<u64> {
    files
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let (fin, part) = dest(dir, &e.name, i);
            if let Ok(md) = fs::metadata(&fin) {
                if md.is_file() && md.len() == e.size {
                    return e.size;
                }
            }
            if let Ok(md) = fs::metadata(&part) {
                // .part 不小于目标大小说明是脏的，丢弃重来
                if md.is_file() && md.len() < e.size {
                    return md.len();
                }
            }
            0
        })
        .collect()
}

pub struct Receiver {
    x: *mut ffi::Ibx,
    cfg: RecvConfig,
    pool_bytes: u64,
}

impl Drop for Receiver {
    fn drop(&mut self) {
        if !self.x.is_null() {
            unsafe { ffi::ibx_close(self.x) };
        }
    }
}

impl Receiver {
    /// 立即返回。内存池的分配、预取和 `ibv_reg_mr` 都在后台线程进行，
    /// 与等待连接的时间重叠——这就是启动没有可感知停顿的原因。
    pub fn start(cfg: RecvConfig) -> io::Result<Receiver> {
        let pool_bytes = tune::pool_ceiling(cfg.pool_bytes);
        let mut err = ffi::ErrBuf::new();
        let cbind = CString::new(cfg.bind.as_str())?;
        let x = unsafe {
            ffi::ibx_listen_start(cbind.as_ptr(), cfg.port, pool_bytes as usize, err.as_mut_ptr())
        };
        if x.is_null() {
            return Err(io::Error::other(format!("ibx_listen_start: {}", err.take())));
        }
        Ok(Receiver { x, cfg, pool_bytes })
    }

    pub fn pool_bytes(&self) -> u64 {
        self.pool_bytes
    }

    /// 后台注册进度：(已预取字节, 总字节, 是否就绪)
    pub fn pool_progress(&self) -> io::Result<(u64, u64, bool)> {
        let mut err = ffi::ErrBuf::new();
        let (mut done, mut total) = (0u64, 0u64);
        let st = unsafe { ffi::ibx_pool_progress(self.x, &mut done, &mut total, err.as_mut_ptr()) };
        if st < 0 {
            return Err(io::Error::other(err.take()));
        }
        Ok((done, total, st == 1))
    }

    /// 等一个连接并判断它想干什么。探测就地应答，只有真要传文件才返回 Transfer。
    /// `wait_ms` 内没人来就返回 Idle。
    pub fn accept_one(&mut self, wait_ms: i32) -> io::Result<Incoming> {
        let mut err = ffi::ErrBuf::new();
        match unsafe { ffi::ibx_listen_accept(self.x, wait_ms, err.as_mut_ptr()) } {
            0 => {}
            2 => return Ok(Incoming::Idle),
            _ => return Err(io::Error::other(format!("ibx_listen_accept: {}", err.take()))),
        }

        let r = (|| -> io::Result<Incoming> {
            let msg = ffi::ctl_recv(self.x, 10_000)?;
            let (ty, body) = match proto::decode_checked(&msg) {
                Ok(v) => v,
                Err(Some(vm)) => {
                    // 断开之前先把原因发回去。这条 ABORT 的报头里带着我们的版本号，
                    // 对端解它时同样会得到「版本不一致」，于是两边都看得到人话，
                    // 而不是一边默默断开、另一边傻等超时。
                    let why = vm.to_string();
                    let _ = proto::encode(proto::MSG_ABORT, |b| {
                        b.extend_from_slice(why.as_bytes());
                        Ok(())
                    })
                    .and_then(|m| ffi::ctl_send(self.x, &m));
                    return Ok(Incoming::Dropped(why));
                }
                Err(None) => return Ok(Incoming::Dropped("不是 ibsend 的连接".into())),
            };
            match ty {
                proto::MSG_PROBE => {
                    let name = if self.cfg.name.is_empty() {
                        proto::hostname()
                    } else {
                        self.cfg.name.clone()
                    };
                    let info = PeerInfo {
                        name,
                        note: self.cfg.note.clone(),
                        inbox: display_path(&self.cfg.out),
                    };
                    let out = proto::encode(proto::MSG_PEERINFO, |b| info.write_to(b))?;
                    ffi::ctl_send(self.x, &out)?;
                    Ok(Incoming::Probe)
                }
                proto::MSG_TRANSFER => Ok(Incoming::Transfer(Manifest::read_from(&mut &body[..])?)),
                t => Ok(Incoming::Dropped(format!("未知消息类型 {t}"))),
            }
        })();

        match r {
            Ok(Incoming::Transfer(m)) => Ok(Incoming::Transfer(m)),
            Ok(other) => {
                self.end_session()?; // 探测和废连接用完即拆
                Ok(other)
            }
            Err(e) => {
                let _ = self.end_session();
                Ok(Incoming::Dropped(e.to_string()))
            }
        }
    }

    /// 结束本次会话，拆掉 QP 但保留已注册的内存池，可以接着服务下一个。
    pub fn end_session(&mut self) -> io::Result<()> {
        let mut err = ffi::ErrBuf::new();
        if unsafe { ffi::ibx_session_end(self.x, err.as_mut_ptr()) } < 0 {
            return Err(io::Error::other(format!("session_end: {}", err.take())));
        }
        Ok(())
    }

    /// 协商切分、收完整个传输并校验。调用方随后应 end_session。
    pub fn receive<F: FnMut(Progress)>(&mut self, m: &Manifest, mut cb: F) -> io::Result<Stats> {
        let out = self.cfg.out.clone();
        let out = out.as_path();
        fs::create_dir_all(out)?;
        let have = scan_existing(out, &m.files);
        let skipped = have.iter().zip(&m.files).filter(|(h, e)| **h >= e.size).count();
        let resumed = have.iter().zip(&m.files).filter(|(h, e)| **h > 0 && **h < e.size).count();
        let total: u64 = m.files.iter().zip(&have).map(|(e, h)| e.size.saturating_sub(*h)).sum();

        // 池子早就注册好了，切成多少块纯粹是算术；同时迁就发送端的 memlock 预算
        let (slab, nslabs) = tune::carve(self.pool_bytes, total, self.cfg.slab, m.sender_budget);
        let mut err = ffi::ErrBuf::new();
        if unsafe { ffi::ibx_recv_arm(self.x, slab as u32, nslabs, err.as_mut_ptr()) } < 0 {
            return Err(io::Error::other(format!("recv_arm: {}", err.take())));
        }

        let reply = Reply {
            have: have.clone(),
            slab_size: slab as u32,
            nslabs,
            pool_addr: unsafe { ffi::ibx_pool_addr(self.x) },
            pool_rkey: unsafe { ffi::ibx_pool_rkey(self.x) },
            inbox: display_path(out),
        };
        ffi::ctl_send(self.x, &proto::encode(proto::MSG_REPLY, |b| reply.write_to(b))?)?;

        // 等发送端确认它本地池注册成功。没有这一步的话，发送端注册失败时我们
        // 会守着一个永远等不到数据的会话，只能靠空闲超时慢慢发现。
        let msg = ffi::ctl_recv(self.x, 30_000)?;
        let (ty, body) = proto::decode(&msg)?;
        match ty {
            proto::MSG_GO => {}
            proto::MSG_ABORT => {
                return Err(io::Error::other(format!(
                    "发送端放弃：{}", String::from_utf8_lossy(body))));
            }
            t => return Err(io::Error::other(format!("期待 GO，收到 {t}"))),
        }

        let (work_tx, work_rx) = mpsc::channel::<Chunk>();
        let (done_tx, done_rx) = mpsc::channel::<u64>();

        // 落盘线程只碰磁盘，不碰任何 ibx_* 接口。CRC 也在这里算——这个线程
        // 本来就在等磁盘 I/O，CPU 是闲的，等于免费。
        let mut sp = Splitter::new(out.to_path_buf(), m.files.clone(), have.clone());
        let waker = Waker(self.x);
        let writer = thread::spawn(move || -> io::Result<Vec<u32>> {
            for c in work_rx {
                let data = unsafe { std::slice::from_raw_parts(c.ptr, c.len) };
                sp.write(data)?;
                if done_tx.send(c.seq).is_err() {
                    break;
                }
                // 立刻叫醒主循环去归还这块并补发信用，别让它等满超时
                waker.wake();
            }
            sp.finish()?;
            Ok(sp.crcs)
        });

        let t0 = Instant::now();
        let res = self.pump(&work_tx, &done_rx, slab, total, t0, &mut cb);

        drop(work_tx);
        let ours = writer.join().map_err(|_| io::Error::other("落盘线程 panic"))?;
        // 计时到这里为止：收完 + 落盘完。后面的尾包往返不算传输耗时。
        let elapsed = t0.elapsed();
        let bytes = unsafe { ffi::ibx_bytes(self.x) };
        res?;
        let ours = ours?;

        // 校验：对比双方对同一段字节算出的 CRC32C
        let msg = ffi::ctl_recv(self.x, 30_000)?;
        let (ty, body) = proto::decode(&msg)?;
        if ty != proto::MSG_TRAILER {
            return Err(io::Error::other(format!("期待尾包，收到 {ty}")));
        }
        let theirs = Trailer::read_from(&mut &body[..])?;

        let mut bad = Vec::new();
        let mut verified = 0usize;
        for (i, e) in m.files.iter().enumerate() {
            if have[i] >= e.size {
                continue; // 整个跳过的文件本次没传，无从校验
            }
            match theirs.crcs.get(i) {
                Some(&c) if c == ours[i] => verified += 1,
                _ => bad.push(e.name.clone()),
            }
        }
        Ok(Stats { bytes, elapsed, skipped, resumed, verified, bad,
                   peer_inbox: String::new() })
    }

    fn pump<F: FnMut(Progress)>(
        &mut self,
        work_tx: &mpsc::Sender<Chunk>,
        done_rx: &mpsc::Receiver<u64>,
        slab: u64,
        total: u64,
        t0: Instant,
        cb: &mut F,
    ) -> io::Result<()> {
        const IDLE_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);
        let mut idle_since = Instant::now();
        let mut err = ffi::ErrBuf::new();
        // 已交给落盘线程但还没归还的块数
        loop {
            // 归还已落盘的 slab。必须在主线程做：QP 操作不是线程安全的。
            while let Ok(seq) = done_rx.try_recv() {
                if unsafe { ffi::ibx_recv_release(self.x, seq, err.as_mut_ptr()) } < 0 {
                    return Err(io::Error::other(format!("release: {}", err.take())));
                }
            }

            let (mut buf, mut len, mut seq) = (std::ptr::null_mut(), 0usize, 0u64);
            // 落盘线程会用 eventfd 主动叫醒这里，所以超时只是兜底（防止对端
            // 静默消失时永远阻塞），不再承担"发现落盘完成"的职责。
            let r = unsafe {
                ffi::ibx_recv_next(self.x, &mut buf, &mut len, &mut seq, 200, err.as_mut_ptr())
            };
            match r {
                1 => {
                    idle_since = Instant::now();
                    let _ = work_tx.send(Chunk { ptr: buf as *mut u8, len, seq });
                }
                2 => {
                    // 长时间一个字节都没来，说明对端已经不在了。守护进程不能
                    // 守着一个死会话空转到天荒地老。
                    if idle_since.elapsed() > IDLE_LIMIT {
                        return Err(io::Error::new(io::ErrorKind::TimedOut,
                            format!("{} 秒没收到任何数据，放弃本次会话", IDLE_LIMIT.as_secs())));
                    }
                }
                0 => return Ok(()),
                _ => return Err(io::Error::other(format!("recv: {}", err.take()))),
            }

            cb(Progress {
                bytes: unsafe { ffi::ibx_bytes(self.x) },
                total,
                staged: unsafe { ffi::ibx_inflight(self.x) } * slab,
                elapsed: t0.elapsed(),
            });
        }
    }
}

/// 指向注册内存池里某个 slab 的裸指针。池子在整个传输期间地址固定，
/// 且同一时刻只有一个线程持有某个 slab，跨线程传递是安全的。
struct Chunk {
    ptr: *mut u8,
    len: usize,
    seq: u64,
}
unsafe impl Send for Chunk {}

/// 让落盘线程能叫醒主循环。
///
/// 这是唯一允许跨线程碰 ibx 的地方：`ibx_recv_wake` 只往一个 eventfd 写
/// 8 字节，不触碰任何 verbs 对象。
struct Waker(*mut ffi::Ibx);
unsafe impl Send for Waker {}
impl Waker {
    fn wake(&self) {
        unsafe { ffi::ibx_recv_wake(self.0) };
    }
}

/// 把连续的字节流按清单切回一个个文件，顺带算每个文件的 CRC32C。
/// 一个 slab 可以跨越文件边界，所以大量小文件不会浪费 slab 空间。
struct Splitter {
    dir: PathBuf,
    /// 规范化后的目标目录，用来验证每个文件确实落在它下面
    root: PathBuf,
    files: Vec<Entry>,
    have: Vec<u64>,
    idx: usize,
    left: u64,
    cur: Option<(File, PathBuf, PathBuf)>,
    cur_idx: usize,
    cur_crc: Crc32c,
    crcs: Vec<u32>,
}

impl Splitter {
    fn new(dir: PathBuf, files: Vec<Entry>, have: Vec<u64>) -> Self {
        let n = files.len();
        let root = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        Splitter {
            dir, root, files, have,
            idx: 0, left: 0, cur: None, cur_idx: 0,
            cur_crc: Crc32c::new(),
            crcs: vec![0; n],
        }
    }

    fn open_next(&mut self) -> io::Result<bool> {
        while self.idx < self.files.len() {
            let i = self.idx;
            self.idx += 1;
            let (size, have) = (self.files[i].size, self.have[i]);
            if have >= size {
                continue; // 已完整，本次流里没有它的字节
            }
            let (fin, part) = dest(&self.dir, &self.files[i].name, i);
            if let Some(parent) = part.parent() {
                fs::create_dir_all(parent)?;
                // 防软链逃逸：目标目录里若已存在一个指向别处的符号链接目录，
                // 光靠路径清洗是拦不住的，必须实地验证解析后仍在根目录之下。
                let canon = fs::canonicalize(parent)?;
                if !canon.starts_with(&self.root) {
                    return Err(io::Error::other(format!(
                        "{} 解析后跑到了目标目录之外，拒绝写入", parent.display())));
                }
            }
            let f = if have > 0 {
                OpenOptions::new().append(true).open(&part)? // 续传：接在已有字节后面
            } else {
                File::create(&part)? // 从头来：截断任何残留
            };
            self.left = size - have;
            self.cur_idx = i;
            self.cur_crc = Crc32c::new();
            self.cur = Some((f, part, fin));
            if self.left > 0 {
                return Ok(true);
            }
            self.finish()?; // 空文件直接收尾
        }
        Ok(false)
    }

    fn finish(&mut self) -> io::Result<()> {
        if let Some((f, part, fin)) = self.cur.take() {
            drop(f);
            // 只有完整写完才改成最终名字，中断留下的 .part 正是下次的续传点
            fs::rename(part, fin)?;
            self.crcs[self.cur_idx] = self.cur_crc.finish();
        }
        Ok(())
    }

    fn write(&mut self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            if self.cur.is_none() && !self.open_next()? {
                return Ok(()); // 清单已写完，多出的字节丢弃
            }
            let take = std::cmp::min(self.left as usize, data.len());
            self.cur.as_mut().unwrap().0.write_all(&data[..take])?;
            self.cur_crc.update(&data[..take]);
            self.left -= take as u64;
            data = &data[take..];
            if self.left == 0 {
                self.finish()?;
            }
        }
        Ok(())
    }
}
