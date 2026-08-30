//! 控制面线格式。
//!
//! 走的是 QP 上的 SEND/RECV（见 csrc/ibx.c 的控制通道），不是 TCP。每条消息
//! 是 `[magic u32][类型 u8][载荷]`，由下层保证可靠有序，这里只管编解码。
//!
//! 一次传输会话：
//!   发送端 → TRANSFER + 清单（文件名、大小、发送端 memlock 预算）
//!   接收端 → REPLY   + 逐文件已有字节数、切分方案、内存池的 addr/rkey
//!   发送端 → GO（本地池注册成功）或 ABORT（注册失败，别让对端空等）
//!   ...数据面 RDMA_WRITE_WITH_IMM...
//!   发送端 → TRAILER + 逐文件本次传输范围的 CRC32C
//!
//! 一次发现探测：
//!   探测方 → PROBE
//!   对端   → PEERINFO + 名字和链路速率

use std::io::{self, Read, Write};

/// 永不改变——它只用来回答「这是不是 ibsend」
const MAGIC: u32 = 0x4942_5358; // "IBSX"

/// 线格式版本，每次改动线格式就 +1。
///
/// 和 magic 分开是有意的：分发出去之后必然会出现两边版本不一致的情况，
/// 那时候要给出「版本不匹配」这条明确的错误，而不是让双方各自懵着——
/// 接收端解不出 magic 就默默断开，发送端傻等到超时，用户看到的是"卡住"。
pub const VERSION: u32 = 2;

pub const MSG_TRANSFER: u8 = 1;
pub const MSG_PROBE: u8 = 2;
pub const MSG_REPLY: u8 = 3;
pub const MSG_GO: u8 = 4;
pub const MSG_ABORT: u8 = 5;
pub const MSG_TRAILER: u8 = 6;
pub const MSG_PEERINFO: u8 = 7;

/// 打包一条消息
pub fn encode<F>(ty: u8, body: F) -> io::Result<Vec<u8>>
where
    F: FnOnce(&mut Vec<u8>) -> io::Result<()>,
{
    let mut b = MAGIC.to_be_bytes().to_vec();
    b.extend_from_slice(&VERSION.to_be_bytes());
    b.push(ty);
    body(&mut b)?;
    Ok(b)
}

/// 无载荷的消息
pub fn encode_bare(ty: u8) -> Vec<u8> {
    encode(ty, |_| Ok(())).expect("无载荷编码不会失败")
}

/// 版本不一致时的专门错误，好让调用方给出人话
#[derive(Debug)]
pub struct VersionMismatch {
    pub theirs: u32,
    pub ours: u32,
}

impl std::fmt::Display for VersionMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "协议版本不一致：对端 v{}，本机 v{}。两边请升到同一版本。",
               self.theirs, self.ours)
    }
}

/// 拆出类型和载荷
pub fn decode(buf: &[u8]) -> io::Result<(u8, &[u8])> {
    match decode_checked(buf) {
        Ok(v) => Ok(v),
        Err(Some(vm)) => Err(io::Error::new(io::ErrorKind::InvalidData, vm.to_string())),
        Err(None) => Err(io::Error::new(io::ErrorKind::InvalidData, "不是 ibsend 的控制消息")),
    }
}

/// 和 decode 一样，但把「版本不一致」单独拎出来，让接收端能在断开之前
/// 先把原因告诉对方
pub fn decode_checked(buf: &[u8]) -> Result<(u8, &[u8]), Option<VersionMismatch>> {
    if buf.len() < 9 {
        return Err(None);
    }
    if u32::from_be_bytes(buf[..4].try_into().unwrap()) != MAGIC {
        return Err(None);
    }
    let theirs = u32::from_be_bytes(buf[4..8].try_into().unwrap());
    if theirs != VERSION {
        return Err(Some(VersionMismatch { theirs, ours: VERSION }));
    }
    Ok((buf[8], &buf[9..]))
}

// ---------------------------------------------------------------- 结构

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Manifest {
    pub files: Vec<Entry>,
    /// 发送端还能 pin 多少内存。接收端挑 slab 尺寸时要保证发送端至少放得下
    /// 2 块，否则流水线转不起来（甚至根本注册不了）。0 = 不限。
    pub sender_budget: u64,
}

/// 接收端的应答：续传信息 + 切分方案 + 内存池位置。
///
/// 切分方案和池子地址都在这里给，是因为发送端必须先注册好本地池才能开始写。
/// 控制面在 QP 上之后，这一步发生在连接建立**之后**，所以顺序上不再打架。
#[derive(Debug, Clone, Default)]
pub struct Reply {
    /// `have[i]` = 第 i 个文件已落盘的字节数。0 = 全传，等于文件大小 = 跳过。
    pub have: Vec<u64>,
    pub slab_size: u32,
    pub nslabs: u32,
    pub pool_addr: u64,
    pub pool_rkey: u32,
    /// 本次传输的落盘目录。发现时拿到的可能已经过期，这个才是权威的。
    pub inbox: String,
}

/// 传输结束后的尾包：`crcs[i]` 是第 i 个文件本次传输那段范围的 CRC32C。
/// 完全跳过的文件其值无意义，接收端会忽略。
#[derive(Debug, Clone, Default)]
pub struct Trailer {
    pub crcs: Vec<u32>,
}

/// 探测应答
#[derive(Debug, Clone, Default)]
pub struct PeerInfo {
    pub name: String,
    pub note: String,
    /// 对端会把收到的文件放在哪。发之前就该知道东西会落到哪儿。
    pub inbox: String,
}

// ---------------------------------------------------------------- 编解码

fn put_str(b: &mut Vec<u8>, s: &str) {
    let n = s.as_bytes();
    b.extend_from_slice(&(n.len() as u16).to_be_bytes());
    b.extend_from_slice(n);
}

fn get_str<R: Read>(r: &mut R) -> io::Result<String> {
    let mut b2 = [0u8; 2];
    r.read_exact(&mut b2)?;
    let n = u16::from_be_bytes(b2) as usize;
    let mut v = vec![0u8; n];
    r.read_exact(&mut v)?;
    Ok(String::from_utf8_lossy(&v).into_owned())
}

impl Manifest {
    pub fn total(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut b = Vec::new();
        b.extend_from_slice(&(self.files.len() as u32).to_be_bytes());
        b.extend_from_slice(&self.sender_budget.to_be_bytes());
        for f in &self.files {
            put_str(&mut b, &f.name);
            b.extend_from_slice(&f.size.to_be_bytes());
        }
        w.write_all(&b)
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Manifest> {
        let mut b4 = [0u8; 4];
        r.read_exact(&mut b4)?;
        let count = u32::from_be_bytes(b4);
        let mut b8 = [0u8; 8];
        r.read_exact(&mut b8)?;
        let sender_budget = u64::from_be_bytes(b8);
        if count > 1 << 20 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "清单条目过多"));
        }
        let mut files = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let name = get_str(r)?;
            r.read_exact(&mut b8)?;
            files.push(Entry { name, size: u64::from_be_bytes(b8) });
        }
        Ok(Manifest { files, sender_budget })
    }
}

impl Reply {
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut b = Vec::new();
        b.extend_from_slice(&self.slab_size.to_be_bytes());
        b.extend_from_slice(&self.nslabs.to_be_bytes());
        b.extend_from_slice(&self.pool_addr.to_be_bytes());
        b.extend_from_slice(&self.pool_rkey.to_be_bytes());
        put_str(&mut b, &self.inbox);
        b.extend_from_slice(&(self.have.len() as u32).to_be_bytes());
        for h in &self.have {
            b.extend_from_slice(&h.to_be_bytes());
        }
        w.write_all(&b)
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Reply> {
        let (mut b4, mut b8) = ([0u8; 4], [0u8; 8]);
        r.read_exact(&mut b4)?; let slab_size = u32::from_be_bytes(b4);
        r.read_exact(&mut b4)?; let nslabs = u32::from_be_bytes(b4);
        r.read_exact(&mut b8)?; let pool_addr = u64::from_be_bytes(b8);
        r.read_exact(&mut b4)?; let pool_rkey = u32::from_be_bytes(b4);
        let inbox = get_str(r)?;
        r.read_exact(&mut b4)?; let n = u32::from_be_bytes(b4);
        if n > 1 << 20 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "应答条目过多"));
        }
        let mut have = Vec::with_capacity(n as usize);
        for _ in 0..n {
            r.read_exact(&mut b8)?;
            have.push(u64::from_be_bytes(b8));
        }
        Ok(Reply { have, slab_size, nslabs, pool_addr, pool_rkey, inbox })
    }
}

impl Trailer {
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut b = (self.crcs.len() as u32).to_be_bytes().to_vec();
        for c in &self.crcs {
            b.extend_from_slice(&c.to_be_bytes());
        }
        w.write_all(&b)
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Trailer> {
        let mut b4 = [0u8; 4];
        r.read_exact(&mut b4)?;
        let n = u32::from_be_bytes(b4);
        if n > 1 << 20 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "尾包条目过多"));
        }
        let mut crcs = Vec::with_capacity(n as usize);
        for _ in 0..n {
            r.read_exact(&mut b4)?;
            crcs.push(u32::from_be_bytes(b4));
        }
        Ok(Trailer { crcs })
    }
}

impl PeerInfo {
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut b = Vec::new();
        put_str(&mut b, &self.name);
        put_str(&mut b, &self.note);
        put_str(&mut b, &self.inbox);
        w.write_all(&b)
    }
    pub fn read_from<R: Read>(r: &mut R) -> io::Result<PeerInfo> {
        Ok(PeerInfo { name: get_str(r)?, note: get_str(r)?, inbox: get_str(r)? })
    }
}

/// 本机名字，给探测应答用
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 清单往返() {
        let m = Manifest {
            sender_budget: 1 << 20,
            files: vec![
                Entry { name: "a.iso".into(), size: 1 << 30 },
                Entry { name: "中文 名字.bin".into(), size: 0 },
            ],
        };
        let msg = encode(MSG_TRANSFER, |b| m.write_to(b)).unwrap();
        let (ty, body) = decode(&msg).unwrap();
        assert_eq!(ty, MSG_TRANSFER);
        let got = Manifest::read_from(&mut &body[..]).unwrap();
        assert_eq!(got.files[1].name, "中文 名字.bin");
        assert_eq!(got.total(), 1 << 30);
        assert_eq!(got.sender_budget, 1 << 20);
    }

    #[test]
    fn 应答尾包探测往返() {
        let r = Reply { have: vec![0, 4096], slab_size: 1 << 20, nslabs: 64,
                        pool_addr: 0xdead_beef_0000, pool_rkey: 0x1234,
                        inbox: "/mnt/data".into() };
        let msg = encode(MSG_REPLY, |b| r.write_to(b)).unwrap();
        let (_, body) = decode(&msg).unwrap();
        let g = Reply::read_from(&mut &body[..]).unwrap();
        assert_eq!((g.slab_size, g.nslabs, g.pool_addr, g.pool_rkey), (1 << 20, 64, 0xdead_beef_0000, 0x1234));
        assert_eq!(g.have, vec![0, 4096]);
        assert_eq!(g.inbox, "/mnt/data");

        let msg = encode(MSG_TRAILER, |b| Trailer { crcs: vec![7, 8] }.write_to(b)).unwrap();
        let (_, body) = decode(&msg).unwrap();
        assert_eq!(Trailer::read_from(&mut &body[..]).unwrap().crcs, vec![7, 8]);

        let pi = PeerInfo { name: "nas".into(), note: "QDR".into(), inbox: "/收件箱".into() };
        let msg = encode(MSG_PEERINFO, |b| pi.write_to(b)).unwrap();
        let (_, body) = decode(&msg).unwrap();
        let g = PeerInfo::read_from(&mut &body[..]).unwrap();
        assert_eq!((g.name.as_str(), g.inbox.as_str()), ("nas", "/收件箱"));
    }

    #[test]
    fn 版本不一致给出的是明确错误而不是乱码() {
        // 伪造一条「未来版本」的消息
        let mut msg = encode_bare(MSG_PROBE);
        msg[7] = 99;
        match decode_checked(&msg) {
            Err(Some(vm)) => {
                assert_eq!((vm.theirs, vm.ours), (99, VERSION));
                assert!(vm.to_string().contains("版本不一致"));
            }
            other => panic!("应当报版本不一致，实际 {other:?}", other = other.map(|(t, _)| t)),
        }
        // magic 不对则是「根本不是 ibsend」，两者要分得开
        let mut junk = encode_bare(MSG_PROBE);
        junk[0] ^= 0xff;
        assert!(matches!(decode_checked(&junk), Err(None)));
    }
}
