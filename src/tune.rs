//! 自适应参数。
//!
//! 核心观察：MR 注册的是一整块连续区域，把它切成多少个多大的 slab 纯粹是
//! 算术。所以池子大小按内存容量在启动时定死（要提前注册才能藏掉停顿），
//! 而 slab 的切法可以等清单到达、知道本次传输多大之后再决定，零成本。

/// 从 /proc/meminfo 读 MemAvailable（字节）
pub fn mem_available() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("MemAvailable:") {
            let kb: u64 = v.trim().trim_end_matches("kB").trim().parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

/// 本进程是否持有 CAP_IPC_LOCK。
///
/// 内核在 `ib_umem_get` 里的检查是 `超限 && !capable(CAP_IPC_LOCK)`，所以有这个
/// 能力就等于没有上限。我们的自我钳制也必须跟着跳过——否则用户授权完了，程序
/// 还在读 /proc/self/limits 看到 8MB 然后自觉把池子压小，白授权。
pub fn has_ipc_lock() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("CapEff:")).map(parse_capeff))
        .unwrap_or(false)
}

/// 从 /proc/.../status 的 CapEff 行判断有没有 CAP_IPC_LOCK（第 14 位）
fn parse_capeff(line: &str) -> bool {
    const CAP_IPC_LOCK: u32 = 14;
    line.split_whitespace()
        .nth(1)
        .and_then(|h| u64::from_str_radix(h, 16).ok())
        .is_some_and(|c| (c >> CAP_IPC_LOCK) & 1 == 1)
}

/// 读 /proc/self/limits 里的 memlock 软限制（字节）。
/// 无限制、或持有 CAP_IPC_LOCK（等价于无限制）时返回 None。
///
/// 有这个才能在申请之前就知道能 pin 多少，而不是等 ibv_reg_mr 甩一句
/// "Cannot allocate memory" 才发现。
pub fn memlock_limit() -> Option<u64> {
    if has_ipc_lock() {
        return None;
    }
    let s = std::fs::read_to_string("/proc/self/limits").ok()?;
    let line = s.lines().find(|l| l.starts_with("Max locked memory"))?;
    let soft = line["Max locked memory".len()..].split_whitespace().next()?;
    if soft.eq_ignore_ascii_case("unlimited") {
        return None;
    }
    soft.parse().ok()
}

/// 本进程当前已经占用的 memlock 额度（字节）。
///
/// 必须把 VmLck 和 VmPin 都算上，而且主要是后者：
///   VmLck = mlock() 的 mm->locked_vm
///   VmPin = pin_user_pages 的 mm->pinned_vm  ← ibv_reg_mr 记在这里
/// 两者都计入 RLIMIT_MEMLOCK，但是两个独立计数器。只看 VmLck 会永远读到 0，
/// 于是发送端以为额度全空，照样撞墙。
///
/// 这个数很关键：同一个进程既当接收端又当发送端时（GUI 就是这样），接收池
/// 已经吃掉的额度必须从发送端的预算里扣掉。
pub fn locked_now() -> u64 {
    let Ok(s) = std::fs::read_to_string("/proc/self/status") else { return 0 };
    let mut total = 0u64;
    for key in ["VmLck:", "VmPin:"] {
        if let Some(l) = s.lines().find(|l| l.starts_with(key)) {
            if let Some(kb) = l.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()) {
                total += kb * 1024;
            }
        }
    }
    total
}

/// 还能再 pin 多少。unlimited 返回 None。
pub fn memlock_headroom() -> Option<u64> {
    memlock_limit().map(|lim| lim.saturating_sub(locked_now()))
}

/// 池子上限。这块内存会被 pin 住，所以只吃可用内存的一部分，
/// 并且绝不超过 memlock 还允许 pin 的量。
///
/// RAM 暂存真正的价值是「装得下的传输能全程跑线速，之后再慢慢排空」。
/// 对于远大于内存的传输，池子只能吸收 (网速-盘速)×几秒 的突发，
/// 所以上限没必要开得过分大。
/// 不考虑 memlock 时想要多大
fn pool_want(user: Option<u64>) -> u64 {
    match user {
        Some(u) => u.max(4 << 20),
        None => {
            let avail = mem_available().unwrap_or(2 << 30);
            (avail / 3).clamp(256 << 20, 16u64 << 30)
        }
    }
}

pub fn pool_ceiling(user: Option<u64>) -> u64 {
    let want = pool_want(user);
    match memlock_headroom() {
        // 只吃一半：另一半留给本进程的发送端和 verbs 自己的内部结构
        // （CQ、QP 的环形缓冲同样算在 memlock 里）。GUI 里收发共处一个
        // 进程，不留这一半的话发送必然失败。
        Some(head) => want.min(head / 2).max(1 << 20),
        None => want,
    }
}

/// 池子大到这个数就不再唠叨要权限了。
///
/// 依据是实测：4 MB 的池子只能切出 4 个块，信用窗口窄到每块都要和落盘线程
/// 打一个来回，吞吐被压到几百 MB/s；到了几百 MB 这一档，窗口有几十上百块，
/// 流水线转得开，再大只是让 RAM 暂存能吸收更长的磁盘滞后——那是锦上添花，
/// 不值得为它去问用户要密码。
pub const ADEQUATE_POOL: u64 = 256 << 20;

/// 当前能开的池子够不够用
pub fn pool_is_adequate(user: Option<u64>) -> bool {
    pool_ceiling(user) >= ADEQUATE_POOL
}

/// 一句话说明池子为什么是这么大，给界面显示用。
///
/// 只有在池子小到会拖累性能时才返回内容——够用了就闭嘴，不去为了"还能更大"
/// 而向用户要权限。
pub fn pool_explain(user: Option<u64>) -> String {
    let (want, got) = (pool_want(user), pool_ceiling(user));
    if got >= want || got >= ADEQUATE_POOL {
        return String::new(); // 没被限制，或者虽然被限制但已经够用
    }
    match memlock_limit() {
        // 只陈述事实，怎么解决交给调用方——GUI 有按钮，CLI 只能给命令
        Some(lim) => format!(
            "受 memlock 限制（{:.0} MB）；解除后可用约 {:.1} GB",
            lim as f64 / 1048576.0,
            want as f64 / 1e9
        ),
        None => String::new(),
    }
}

/// 按本次传输总量把池子切成 (slab_size, nslabs)。
///
/// 目标：slab 要够大让每个 WR 的开销可以忽略（>=1MB），又要够多让流水线
/// 转得起来（>=8 块）。传输本身比池子小的话就没必要用满整个池子。
///
/// 注：这个启发式尚未用可信数据验证过。曾经试过改成「固定 1MB，靠块数吃满
/// 池子」，依据是一组测量显示 ZFS 上 1MB 的 read 比 16MB 快 40%；但那批数据
/// 是在接收端内存池被 memlock 压到 4MB 的情况下测的（编译把 CAP_IPC_LOCK
/// 从二进制上抹掉了），不成立，已回退。要重做这个判断，必须先确认两端的池子
/// 都是正常尺寸，并且源文件稳定命中缓存（否则量到的是 ARC 命中率不是 slab
/// 尺寸）。
/// 向下取到 2 的幂
pub fn floor_pow2(x: u64) -> u64 {
    if x == 0 { 0 } else { 1u64 << (63 - x.leading_zeros()) }
}

/// 内存池实际会占用的字节数：alloc_pool 把它向上对齐到 2MB（大页需要）
pub fn pool_alloc_size(bytes: u64) -> u64 {
    const HUGE: u64 = 2 << 20;
    bytes.div_ceil(HUGE) * HUGE
}

/// `peer_budget` 是发送端还能 pin 多少内存（0 = 不限）。slab 必须小到让发送端
/// 至少放得下 2 块，否则它连本地池都注册不出来，流水线更无从谈起。
pub fn carve(pool_bytes: u64, total: u64, slab_override: Option<u64>, peer_budget: u64) -> (u64, u32) {
    const MIN_SLAB: u64 = 1 << 20;
    const MAX_SLAB: u64 = 16 << 20;
    const MIN_SLABS: u64 = 8;
    const MAX_SLABS: u64 = 8192; // 留在 max_qp_wr 之下

    let useful = total.max(MIN_SLAB * MIN_SLABS).min(pool_bytes);

    let slab = match slab_override {
        Some(s) => s.clamp(4096, MAX_SLAB),
        // 目标约 64 块；向下取到 2 的幂，落在 [1M, 16M]
        None => {
            let want = (useful / 64).clamp(MIN_SLAB, MAX_SLAB);
            MIN_SLAB << (63 - (want / MIN_SLAB).max(1).leading_zeros() as u64).min(4)
        }
    };

    // 发送端放不下 2 块就把 slab 压小。必须向下取到 2 的幂：内存池会被
    // 向上对齐到 2MB 边界（大页需要），非 2 的幂的 slab 乘出来的池子一取整
    // 就可能反超预算——正是这个对齐差点让 7.0 MB 变成 8.0 MB 撞上 memlock。
    let slab = if peer_budget > 0 {
        slab.min(floor_pow2((peer_budget / 2).max(4096))).max(4096)
    } else {
        slab
    };

    let by_need = useful.div_ceil(slab);
    let by_pool = pool_bytes / slab;
    let nslabs = by_need.clamp(MIN_SLABS, MAX_SLABS).min(by_pool).max(1);
    (slab, nslabs as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    const G: u64 = 1 << 30;
    const M: u64 = 1 << 20;

    #[test]
    fn capeff_位解析() {
        // 这段错了的话，用户授权完会毫无反应、也没有任何提示
        assert!(!parse_capeff("CapEff:\t0000000000000000"), "无能力");
        assert!(parse_capeff("CapEff:\t0000000000004000"), "只有 CAP_IPC_LOCK（第 14 位）");
        assert!(parse_capeff("CapEff:\t000001ffffffffff"), "root 的全量能力");
        assert!(!parse_capeff("CapEff:\t0000000000002000"), "第 13 位不是它");
        assert!(!parse_capeff("CapEff:\t0000000000008000"), "第 15 位也不是");
        assert!(!parse_capeff("CapEff:\t垃圾"), "解析失败要当作没有");
    }

    #[test]
    fn 已占额度读得到_pin_的部分() {
        // 至少要能解析出来不 panic；具体值取决于当前进程有没有注册过 MR
        let _ = locked_now();
        let st = std::fs::read_to_string("/proc/self/status").unwrap();
        assert!(st.contains("VmPin:"), "内核不报 VmPin 的话额度核算会失效");
    }

    #[test]
    fn 够用了就不再提示要权限() {
        // 注意 pool_ceiling 总会被 memlock 钳制，所以「要多大」不等于「能多大」，
        // 断言只能建立在实际能开出来的池子上。
        let got = pool_ceiling(None);
        assert_eq!(pool_is_adequate(None), got >= ADEQUATE_POOL);
        if got >= ADEQUATE_POOL {
            assert_eq!(pool_explain(None), "", "池子够用时不该再提示要权限");
        }
        // 持有 CAP_IPC_LOCK 等于没有上限，任何情况下都不该提示
        if has_ipc_lock() {
            assert_eq!(pool_explain(None), "");
        }
    }

    #[test]
    fn 池子不超过_memlock() {
        if let Some(lim) = memlock_limit() {
            assert!(pool_ceiling(None) <= lim);
            assert!(pool_ceiling(Some(1 << 40)) <= lim, "显式指定超大也应被压回 memlock 之内");
        }
    }

    #[test]
    fn 切分总是落在池子里() {
        for &pool in &[16 * M, 256 * M, 4 * G, 16 * G] {
            for &total in &[1u64, 5 * M, 100 * M, 8 * G, 500 * G] {
                let (slab, n) = carve(pool, total, None, 0);
                assert!(slab * n as u64 <= pool, "pool={pool} total={total} → {slab}×{n}");
                assert!(n >= 1 && slab >= 4096);
            }
        }
    }

    #[test]
    fn 小文件用小块保证流水线转得起来() {
        let (slab, n) = carve(16 * G, 100 * M, None, 0);
        assert!(slab <= 4 * M, "100MB 的传输不该用 {slab} 的大块");
        assert!(n >= 8, "块数 {n} 太少，流水线转不起来");
    }

    #[test]
    fn 大文件用大块压低每块开销() {
        let (slab, _) = carve(16 * G, 500 * G, None, 0);
        assert_eq!(slab, 16 * M);
    }

    #[test]
    fn 迁就发送端的预算() {
        // 关键：2 块的池子经过 2MB 对齐之后仍然不能超预算
        for budget in [4 * M, 6 * M, 7 * M, 7340032, 64 * M, G] {
            let (slab, n) = carve(16 * G, 500 * G, None, budget);
            let two = pool_alloc_size(2 * slab);
            assert!(two <= budget, "预算 {budget}：2 个 {slab} 的块对齐后要 {two}，超了");
            assert!(slab * n as u64 <= 16 * G);
        }
    }


}
