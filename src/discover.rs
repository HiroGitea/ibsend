//! 对端发现。
//!
//! 扫描 IPoIB 子网，但**不走 TCP**——每个地址都用 rdma_cm 试着建一条 QP，然后
//! 在 QP 上问一句「你是谁」。这样整条链路一个 TCP 都没有，防火墙无从下手：
//! IPoIB 虽然在内核眼里是普通网卡（netfilter 照常生效），但 RDMA 完全绕开内核
//! 网络栈，iptables/ufw 碰不到；唯一沾 IP 的 `rdma_resolve_addr` 走的是 ARP／
//! 邻居解析，也不在 netfilter 的过滤范围里。
//!
//! 没有用组播：IPoIB 上发组播必须正确设置 `IP_MULTICAST_IF`，否则包会从默认
//! 路由的以太网口出去，找错网了还不报错。IB 子网通常就几台机器，扫描更稳。

use std::ffi::{c_char, CString};
use std::io;
use std::net::Ipv4Addr;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use crate::ffi;
use crate::proto::{self, PeerInfo};

#[derive(Debug, Clone)]
pub struct Peer {
    pub name: String,
    pub note: String,
    pub addr: Ipv4Addr,
    /// 对端的落盘目录——发之前就知道东西会去哪
    pub inbox: String,
}

/// 本机的 IPoIB 接口：(接口名, 地址, 掩码)
pub fn local_ipoib() -> io::Result<Vec<(String, Ipv4Addr, Ipv4Addr)>> {
    let mut buf = vec![0 as c_char; 4096];
    let n = unsafe { ffi::ibx_list_ipoib(buf.as_mut_ptr(), buf.len()) };
    if n < 0 {
        return Err(io::Error::other("getifaddrs 失败"));
    }
    let bytes: Vec<u8> = buf.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            Some((it.next()?.to_string(), it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect())
}

/// 探测单个地址：建一条 QP 问「你是谁」，问完就拆。
/// 不是 ibsend、或者地址上根本没有主机，都返回 None。
fn probe(addr: Ipv4Addr, port: u16, resolve_ms: i32) -> Option<Peer> {
    let cs = CString::new(addr.to_string()).ok()?;
    let mut err = ffi::ErrBuf::new();
    let mut again = 0;
    let x = unsafe { ffi::ibx_connect(cs.as_ptr(), port, resolve_ms, &mut again, err.as_mut_ptr()) };
    if x.is_null() {
        return None;
    }
    let _guard = ffi::Owned(x);

    ffi::ctl_send(x, &proto::encode_bare(proto::MSG_PROBE)).ok()?;
    let msg = ffi::ctl_recv(x, 3000).ok()?;
    let (ty, body) = proto::decode(&msg).ok()?;
    if ty != proto::MSG_PEERINFO {
        return None;
    }
    let info = PeerInfo::read_from(&mut &body[..]).ok()?;
    Some(Peer { name: info.name, note: info.note, inbox: info.inbox, addr })
}

/// 扫描所有 IPoIB 接口所在的子网。
/// `resolve_ms` 是单个地址的地址解析超时——空地址要等满这个时间（ARP 失败），
/// 所以它和并发度共同决定总耗时。
pub fn scan(port: u16, resolve_ms: i32, threads: usize) -> Vec<Peer> {
    let ifaces = match local_ipoib() {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let mut targets = Vec::new();
    for (_name, ip, mask) in &ifaces {
        let (ipn, maskn) = (u32::from(*ip), u32::from(*mask));
        let hosts = (!maskn).wrapping_add(1);
        if hosts > 4096 {
            continue; // 子网太大，扫描不现实
        }
        let net = ipn & maskn;
        for h in 1..hosts.saturating_sub(1) {
            let a = Ipv4Addr::from(net + h);
            if a != *ip {
                targets.push(a);
            }
        }
    }
    if targets.is_empty() {
        return Vec::new();
    }

    let queue = Arc::new(Mutex::new(targets.into_iter()));
    let (tx, rx) = mpsc::channel();
    let mut handles = Vec::new();
    for _ in 0..threads.max(1) {
        let q = Arc::clone(&queue);
        let tx = tx.clone();
        handles.push(thread::spawn(move || loop {
            let next = { q.lock().unwrap().next() };
            match next {
                Some(a) => {
                    if let Some(p) = probe(a, port, resolve_ms) {
                        let _ = tx.send(p);
                    }
                }
                None => break,
            }
        }));
    }
    drop(tx);

    let mut peers: Vec<Peer> = rx.iter().collect();
    for h in handles {
        let _ = h.join();
    }
    peers.sort_by_key(|p| u32::from(p.addr));
    peers
}
