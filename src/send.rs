//! 发送端：建连 → 控制面协商 → 注册本地池 → 把缺的部分灌进 slab
//!
//! 顺序很重要：先建 QP，再在 QP 上协商切分方案，最后才注册本地池。原先控制面
//! 走 TCP 时必须反过来（连接前就得知道 slab 尺寸才能提前注册），只能把池子信息
//! 塞进 rdma_cm 的 private_data；现在这个先有鸡还是先有蛋的问题自然消失了。

use std::ffi::{c_int, CString};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::crc::Crc32c;
use crate::proto::{self, Entry, Manifest, Reply, Trailer};
use crate::walk::Item;
use crate::{ffi, tune, Progress, Stats, PORT};

pub struct SendConfig {
    pub peer: String,
    pub port: u16,
    /// 本地流水线深度。post_send 是异步的，读盘和网卡 DMA 天然重叠，
    /// 16 块就够把链路喂满。实际值还会被 memlock 预算压低。
    pub local_slabs: u32,
}

impl SendConfig {
    pub fn new(peer: impl Into<String>) -> Self {
        SendConfig { peer: peer.into(), port: PORT, local_slabs: 16 }
    }
}

pub fn send_files<F: FnMut(Progress)>(
    cfg: &SendConfig,
    items: &[Item],
    mut cb: F,
) -> io::Result<Stats> {
    let mut files = Vec::new();
    for it in items {
        let md = fs::metadata(&it.path)?;
        if !md.is_file() {
            return Err(io::Error::other(format!("{} 不是普通文件", it.path.display())));
        }
        files.push(Entry { name: it.name.clone(), size: md.len() });
    }
    // 把本机还能 pin 多少告诉对端，让它挑一个双方都放得下的 slab 尺寸。
    // 留 1/8 给 CQ/QP 和控制通道自己的缓冲，它们同样计入 memlock。
    let budget = tune::memlock_headroom().map(|h| h - h / 8).unwrap_or(0);
    let manifest = Manifest { files, sender_budget: budget };

    // 只重试「对端还没就位」这一类；本地资源问题立刻报错，否则一个必然失败的
    // 错误会被重试循环拖成几分钟的静默挂起。
    let mut err = ffi::ErrBuf::new();
    let cpeer = CString::new(cfg.peer.as_str())?;
    let mut raw = std::ptr::null_mut();
    for i in 0..20 {
        let mut again: c_int = 0;
        raw = unsafe {
            ffi::ibx_connect(cpeer.as_ptr(), cfg.port, 5000, &mut again, err.as_mut_ptr())
        };
        if !raw.is_null() {
            break;
        }
        if again == 0 || i == 19 {
            return Err(io::Error::other(format!("ibx_connect: {}", err.take())));
        }
        thread::sleep(Duration::from_millis(100));
    }
    let x = raw;
    let _guard = ffi::Owned(x);

    ffi::ctl_send(x, &proto::encode(proto::MSG_TRANSFER, |b| manifest.write_to(b))?)?;
    // 20 秒足够对端扫完目录并算出切分方案；等太久只会让「对端有问题」表现成卡死
    let msg = ffi::ctl_recv(x, 20_000)?;
    let (ty, body) = proto::decode(&msg)?;
    if ty == proto::MSG_ABORT {
        return Err(io::Error::other(format!("对端拒绝：{}", String::from_utf8_lossy(body))));
    }
    if ty != proto::MSG_REPLY {
        return Err(io::Error::other(format!("期待应答，收到 {ty}")));
    }
    let reply = Reply::read_from(&mut &body[..])?;
    let have = reply.have.clone();

    let skipped = have.iter().zip(&manifest.files).filter(|(h, e)| **h >= e.size).count();
    let resumed = have.iter().zip(&manifest.files).filter(|(h, e)| **h > 0 && **h < e.size).count();
    let total: u64 = manifest.files.iter().zip(&have).map(|(e, h)| e.size.saturating_sub(*h)).sum();

    // 池子会被向上对齐到 2MB（大页需要），所以按对齐后的大小收敛块数
    let slab = reply.slab_size.max(4096);
    let mut local_slabs = cfg.local_slabs.max(2);
    if budget > 0 {
        while local_slabs > 2
            && tune::pool_alloc_size(local_slabs as u64 * slab as u64) > budget
        {
            local_slabs -= 1;
        }
    }
    let need = tune::pool_alloc_size(local_slabs as u64 * slab as u64);

    let armed = if budget > 0 && need > budget {
        Err(format!(
            "memlock 剩余 {:.1} MB，装不下 2 个 {:.1} MB 的块（对齐后要 {:.1} MB）；\
             放开 memlock 见 contrib/99-rdma.conf",
            budget as f64 / 1048576.0, slab as f64 / 1048576.0, need as f64 / 1048576.0))
    } else if unsafe {
        ffi::ibx_send_arm(x, slab, local_slabs, reply.pool_addr, reply.pool_rkey,
                          reply.nslabs, err.as_mut_ptr())
    } < 0 {
        Err(format!("send_arm: {}", err.take()))
    } else {
        Ok(())
    };

    // 本地池没备好就明确告诉对端，别让它守着一个空会话等到超时
    if let Err(why) = armed {
        let _ = ffi::ctl_send(x, &proto::encode(proto::MSG_ABORT,
            |b| { b.extend_from_slice(why.as_bytes()); Ok(()) })?);
        return Err(io::Error::other(why));
    }
    ffi::ctl_send(x, &proto::encode_bare(proto::MSG_GO))?;

    let t0 = Instant::now();

    // 读盘交给单独的线程。post_send 是异步的，但 read() 是阻塞的——单线程时
    // 读盘那几毫秒网卡在干等。实测 ZFS 源只能跑到 2.29 GB/s，而同一份数据从
    // tmpfs 发能到 3.22 GB/s，差的就是这段没叠起来的读时间。
    //
    // 只用一个读线程：磁盘单线程就能读到 5.5 GB/s，本来就快过 3.4 GB/s 的网络，
    // 瓶颈是串行而不是磁盘能力，再加线程只会去抢一个已经够快的磁盘。
    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let (done_tx, done_rx) = mpsc::channel::<io::Result<(u64, usize)>>();
    let r_items = items.to_vec();
    let r_files = manifest.files.clone();
    let r_have = have.clone();
    let reader = thread::spawn(move || -> Vec<u32> {
        let mut rd = CatReader::new(&r_items, &r_files, &r_have);
        for job in job_rx {
            // SAFETY: 这块 slab 已被主线程 acquire 出来，在 commit 之前独属于本线程
            let buf = unsafe { std::slice::from_raw_parts_mut(job.ptr, job.cap) };
            let r = rd.fill(buf);
            let stop = !matches!(r, Ok(n) if n > 0);
            if done_tx.send(r.map(|n| (job.seq, n))).is_err() || stop {
                break;
            }
        }
        rd.crcs
    });

    // 预取深度必须同时受两个窗口约束：本地 slab 数，以及对端的信用窗口。
    // 只看本地的话，当对端窗口更小时会死锁——预取把信用窗口占满，而释放信用
    // 需要对端收到数据，收数据需要 commit，commit 却排在预取循环之后。
    let depth = (local_slabs.min(reply.nslabs) as usize).max(1);
    let res = (|| -> io::Result<()> {
        let mut inflight = 0usize;
        loop {
            // 先把读队列喂满，让读盘始终领先于发送
            while inflight < depth {
                let mut seq = 0u64;
                let p = unsafe { ffi::ibx_send_acquire(x, &mut seq, err.as_mut_ptr()) };
                if p.is_null() {
                    return Err(io::Error::other(format!("acquire: {}", err.take())));
                }
                if job_tx.send(Job { ptr: p as *mut u8, cap: slab as usize, seq }).is_err() {
                    break;
                }
                inflight += 1;
            }

            let (seq, n) = match done_rx.recv() {
                Ok(r) => r?,
                Err(_) => return Err(io::Error::other("读盘线程意外退出")),
            };
            inflight -= 1;
            // 必须按 seq 递增顺序提交：接收端的块序号靠到达顺序隐式推导。
            // 单读线程 + FIFO 通道天然保证了这个顺序。
            if unsafe { ffi::ibx_send_commit(x, seq, n, err.as_mut_ptr()) } < 0 {
                return Err(io::Error::other(format!("commit: {}", err.take())));
            }
            if n == 0 {
                break; // len == 0 就是流结束标记
            }
            cb(Progress { bytes: unsafe { ffi::ibx_bytes(x) }, total, staged: 0,
                          elapsed: t0.elapsed() });
        }
        if unsafe { ffi::ibx_send_drain(x, err.as_mut_ptr()) } < 0 {
            return Err(io::Error::other(format!("drain: {}", err.take())));
        }
        Ok(())
    })();

    drop(job_tx);
    let crcs = reader.join().map_err(|_| io::Error::other("读盘线程 panic"))?;

    // 计时到 drain 返回为止：连接拆除是固件操作，在 mlx4 上很慢，不算传输耗时
    let elapsed = t0.elapsed();
    let bytes = unsafe { ffi::ibx_bytes(x) };
    res?;

    // 数据面已排空，把每个文件本次传输范围的 CRC32C 发过去让对端核对
    ffi::ctl_send(x, &proto::encode(proto::MSG_TRAILER,
        |b| Trailer { crcs }.write_to(b))?)?;

    Ok(Stats { bytes, elapsed, skipped, resumed, verified: 0, bad: Vec::new(),
               peer_inbox: reply.inbox })
}

/// 交给读线程去填的一块 slab。指针指向已注册的内存池；主线程 acquire 之后、
/// commit 之前，这块只属于读线程，跨线程移交所有权是安全的。
struct Job {
    ptr: *mut u8,
    cap: usize,
    seq: u64,
}
unsafe impl Send for Job {}

/// 把需要传的文件段当成一条连续字节流读出来填满每个 slab，顺带算 CRC32C。
/// 跳过的文件完全不出现在流里。
struct CatReader {
    /// (路径, 已有字节数, 文件索引)，已完整的文件不在列表里
    items: Vec<(PathBuf, u64, usize)>,
    idx: usize,
    cur: Option<File>,
    cur_crc: Crc32c,
    cur_idx: usize,
    open: bool,
    crcs: Vec<u32>,
}

impl CatReader {
    fn new(src: &[Item], files: &[Entry], have: &[u64]) -> Self {
        let items = src
            .iter()
            .enumerate()
            .filter(|(i, _)| have[*i] < files[*i].size)
            .map(|(i, it)| (it.path.clone(), have[i], i))
            .collect();
        CatReader {
            items, idx: 0, cur: None,
            cur_crc: Crc32c::new(), cur_idx: 0, open: false,
            crcs: vec![0; files.len()],
        }
    }

    fn fill(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut off = 0;
        while off < buf.len() {
            if !self.open {
                if self.idx >= self.items.len() {
                    break;
                }
                let (path, have, fi) = self.items[self.idx].clone();
                self.idx += 1;
                let mut f = File::open(&path)?;
                if have > 0 {
                    f.seek(SeekFrom::Start(have))?; // 续传：跳过对端已有的部分
                }
                self.cur = Some(f);
                self.cur_crc = Crc32c::new();
                self.cur_idx = fi;
                self.open = true;
            }
            let n = self.cur.as_mut().unwrap().read(&mut buf[off..])?;
            if n == 0 {
                self.crcs[self.cur_idx] = self.cur_crc.finish(); // 本文件读完，落定 CRC
                self.cur = None;
                self.open = false;
                continue;
            }
            self.cur_crc.update(&buf[off..off + n]);
            off += n;
        }
        Ok(off)
    }
}
