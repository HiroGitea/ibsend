//! ibsend CLI —— 参数解析和文字前端。所有逻辑都在 lib 里。
//!
//! 无头和有头是对等的：两边都跑同一个守护循环，都能被发现、都能收发。
//! GUI 只是这个守护的另一个前端（见 --features gui）。

use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use ibsend::daemon::{self, DaemonConfig, Event};
use ibsend::{discover, send_files, Progress, RecvConfig, Receiver, SendConfig};

fn human(b: f64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut i) = (b, 0);
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    format!("{v:.2} {}", U[i])
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mul) = match s.chars().last()?.to_ascii_uppercase() {
        'G' => (&s[..s.len() - 1], 1u64 << 30),
        'M' => (&s[..s.len() - 1], 1u64 << 20),
        'K' => (&s[..s.len() - 1], 1u64 << 10),
        _ => (s, 1),
    };
    num.trim().parse::<u64>().ok().map(|v| v * mul)
}

/// 没给 --bind 时自动挑一个 IPoIB 地址
fn auto_bind() -> Option<String> {
    discover::local_ipoib().ok()?.first().map(|(_, ip, _)| ip.to_string())
}

fn usage() -> ! {
    eprintln!(
"ibsend —— InfiniBand 文件传输

  ibsend daemon [选项]            常驻：可被发现、可接收（无头模式）
      --bind <IP>    监听地址，默认自动选 IPoIB 接口
      --out <DIR>    落盘目录，默认当前目录
      --pool <SIZE>  RAM 暂存池，默认按可用内存自适应
      --slab <SIZE>  单块大小，默认按传输大小自适应
      --name <名字>  对外显示的设备名，默认用主机名

  ibsend discover                 扫描 IPoIB 子网，列出在线的对端
      --timeout <MS> 单地址解析超时，默认 300

  ibsend send <对端|名字> <文件或目录>...  发送（目录会递归展开）
      --slabs <N>    本地流水线深度，默认 16

  ibsend recv [选项]              收一次就退出（同 daemon 的选项）

  ibsend authorize [--force]      申请内存锁定权限（RDMA 需要 pin 内存，系统
                                  默认只给 8MB）。额度已经够用时不会打扰你，
                                  --force 可以强制申请

无头和有头是对等的：两边都跑 daemon，都能被发现、都能互相收发。
带界面的版本见 ibsend-gui（cargo build --features gui）。

符号链接会被跳过而不是跟随。");
    std::process::exit(2);
}

/// 解析 daemon/recv 共用的参数
fn parse_recv_args(args: &[String]) -> RecvConfig {
    let mut cfg = RecvConfig::new("");
    let mut i = 0;
    while i < args.len() {
        let v = args.get(i + 1).cloned().unwrap_or_else(|| usage());
        match args[i].as_str() {
            "--bind" => cfg.bind = v,
            "--out" => cfg.out = PathBuf::from(v),
            "--pool" => cfg.pool_bytes = Some(parse_size(&v).unwrap_or_else(|| usage())),
            "--slab" => cfg.slab = Some(parse_size(&v).unwrap_or_else(|| usage())),
            "--name" => cfg.name = v,
            _ => usage(),
        }
        i += 2;
    }
    if cfg.bind.is_empty() {
        cfg.bind = auto_bind().unwrap_or_else(|| {
            eprintln!("错误：找不到 IPoIB 接口，请用 --bind 指定地址\n");
            usage()
        });
    }
    cfg
}

/// 内存池被 memlock 卡住时的提醒。措辞跟着实际可用的方式走——
/// 无头机器上让人去跑一个只会打印另一条命令的命令，是在兜圈子。
fn warn_memlock() {
    let why = ibsend::tune::pool_explain(None);
    if why.is_empty() {
        return;
    }
    eprintln!("⚠ {why}");
    if ibsend::authorize::can_prompt() {
        eprintln!("  跑一次 `ibsend authorize` 解除");
    } else {
        eprintln!("  执行：{}", ibsend::authorize::manual_command());
    }
}

fn cmd_daemon(args: &[String]) -> io::Result<()> {
    warn_memlock();
    let cfg = parse_recv_args(args);
    let mut last = std::time::Instant::now();
    daemon::run(DaemonConfig { recv: cfg }, |ev| {
        if let Event::Progress(p) = &ev {
            if last.elapsed() >= Duration::from_millis(400) {
                eprint!("\r  {} / {}   {}/s   RAM 暂存 {}      ",
                        human(p.bytes as f64), human(p.total as f64),
                        human(p.rate()), human(p.staged as f64));
                let _ = io::stderr().flush();
                last = std::time::Instant::now();
            }
            return;
        }
        if let Some(line) = daemon::describe(&ev) {
            eprintln!("\r{line}                              ");
        }
    })
}

fn cmd_discover(args: &[String]) -> io::Result<()> {
    let mut tmo = 300u64;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--timeout" {
            tmo = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or_else(|| usage());
            i += 2;
        } else {
            usage();
        }
    }
    match discover::local_ipoib() {
        Ok(v) if !v.is_empty() => {
            for (name, ip, mask) in &v {
                eprintln!("本机 {name}  {ip}/{mask}");
            }
        }
        _ => {
            eprintln!("找不到 IPoIB 接口");
            return Ok(());
        }
    }
    eprintln!("扫描中…");
    let peers = discover::scan(ibsend::PORT, tmo as i32, 64);
    if peers.is_empty() {
        eprintln!("没找到对端（对方需要在跑 ibsend daemon）");
    }
    for p in &peers {
        println!("  {:<16} {:<20} {:<22} → {}", p.addr, p.name, p.note, p.inbox);
    }
    Ok(())
}

fn cmd_send(args: &[String]) -> io::Result<()> {
    if args.is_empty() {
        usage();
    }
    let target = args[0].clone();
    let mut paths = Vec::new();
    let mut slabs = 16u32;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--slabs" {
            slabs = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or_else(|| usage());
            i += 2;
        } else {
            paths.push(PathBuf::from(&args[i]));
            i += 1;
        }
    }
    if paths.is_empty() {
        usage();
    }

    // 不是 IP 就当成名字，扫一遍子网找它
    let peer = if target.parse::<std::net::Ipv4Addr>().is_ok() {
        target
    } else {
        eprintln!("按名字查找 {target}…");
        let peers = discover::scan(ibsend::PORT, 300, 64);
        match peers.iter().find(|p| p.name == target) {
            Some(p) => p.addr.to_string(),
            None => return Err(io::Error::other(format!("没找到名为 {target} 的对端"))),
        }
    };

    // 目录在这里展开成文件列表，名字带上相对路径
    let (items, skipped) = ibsend::walk::expand(&paths)?;
    if items.is_empty() {
        return Err(io::Error::other("没有可发送的文件"));
    }
    if !skipped.is_empty() {
        eprintln!("跳过 {}", skipped.describe());
    }
    let total: u64 = items.iter()
        .filter_map(|i| std::fs::metadata(&i.path).ok().map(|m| m.len()))
        .sum();
    eprintln!("{} 个文件，共 {}", items.len(), human(total as f64));

    let mut cfg = SendConfig::new(peer);
    cfg.local_slabs = slabs;
    let mut last = std::time::Instant::now();
    let st = send_files(&cfg, &items, |p: Progress| {
        if last.elapsed() >= Duration::from_millis(400) {
            eprint!("\r  {} / {}   {}/s      ",
                    human(p.bytes as f64), human(p.total as f64), human(p.rate()));
            let _ = io::stderr().flush();
            last = std::time::Instant::now();
        }
    })?;
    eprintln!("\r发送完成：{} / {:.2}s = {}/s                    ",
              human(st.bytes as f64), st.elapsed.as_secs_f64(), human(st.rate()));
    if !st.peer_inbox.is_empty() {
        eprintln!("  对端已存放于 {}", st.peer_inbox);
    }
    if st.skipped > 0 {
        eprintln!("  对端已有 {} 个文件，跳过", st.skipped);
    }
    if st.resumed > 0 {
        eprintln!("  {} 个文件从中断处续传", st.resumed);
    }
    Ok(())
}

fn cmd_recv_once(args: &[String]) -> io::Result<()> {
    warn_memlock();
    let cfg = parse_recv_args(args);
    let bind = cfg.bind.clone();
    let port = cfg.port;
    let mut rx = Receiver::start(cfg)?;
    eprintln!("内存池 {}（后台注册中，与等待连接重叠）", human(rx.pool_bytes() as f64));
    loop {
        let (done, total, ready) = rx.pool_progress()?;
        if ready {
            eprintln!("\r内存池就绪 {}                       ", human(total as f64));
            break;
        }
        eprint!("\r  注册中 {:.0}%  ", done as f64 / total.max(1) as f64 * 100.0);
        let _ = io::stderr().flush();
        std::thread::sleep(Duration::from_millis(80));
    }
    eprintln!("等待发送端  {bind}:{port}（rdma_cm 端口，不是 TCP）");

    let m = loop {
        match rx.accept_one(2000)? {
            ibsend::Incoming::Transfer(m) => break m,
            ibsend::Incoming::Dropped(why) => eprintln!("丢弃一条连接：{why}"),
            _ => {}
        }
    };
    eprintln!("收到清单：{} 个文件，共 {}", m.files.len(), human(m.total() as f64));

    let mut last = std::time::Instant::now();
    let st = rx.receive(&m, |p: Progress| {
        if last.elapsed() >= Duration::from_millis(400) {
            eprint!("\r  {} / {}   {}/s   RAM 暂存 {}      ",
                    human(p.bytes as f64), human(p.total as f64),
                    human(p.rate()), human(p.staged as f64));
            let _ = io::stderr().flush();
            last = std::time::Instant::now();
        }
    })?;
    let _ = rx.end_session();
    eprintln!("\r完成：{} / {:.2}s = {}/s（含落盘）              ",
              human(st.bytes as f64), st.elapsed.as_secs_f64(), human(st.rate()));
    if st.skipped > 0 { eprintln!("  跳过 {} 个已完整存在的文件", st.skipped); }
    if st.resumed > 0 { eprintln!("  {} 个文件从中断处续传", st.resumed); }
    if st.verified > 0 { eprintln!("  CRC32C 校验通过 {} 个文件", st.verified); }
    for n in &st.bad { eprintln!("  ✗ 校验失败：{n}"); }
    if !st.bad.is_empty() {
        return Err(io::Error::other(format!("{} 个文件校验失败", st.bad.len())));
    }
    Ok(())
}

fn cmd_authorize(args: &[String]) -> io::Result<()> {
    use ibsend::authorize::{self, Method, Outcome};
    let force = args.iter().any(|a| a == "--force");

    if ibsend::tune::has_ipc_lock() {
        eprintln!("已经有内存锁定权限，接收池不受 memlock 限制。");
        return Ok(());
    }
    let pool = ibsend::tune::pool_ceiling(None);
    // 够用就别问用户要密码。授权本身是有代价的（能力会一直留在程序文件上），
    // 为了"还能更大"去要一次 root，不划算。
    if !force && ibsend::tune::pool_is_adequate(None) {
        eprintln!("不需要授权：当前 memlock 已能开到 {:.1} GB 的接收池，够用。",
                  pool as f64 / 1e9);
        eprintln!("（确实想提高上限就加 --force）");
        return Ok(());
    }
    eprintln!("当前 memlock 上限 {:.0} MB，接收池只能开到 {:.1} MB。",
        ibsend::tune::memlock_limit().unwrap_or(0) as f64 / 1048576.0,
        pool as f64 / 1048576.0);
    match authorize::request() {
        Outcome::Granted => {
            eprintln!("授权成功。权限在下次启动时生效——它写在程序文件的扩展属性里，");
            eprintln!("execve 时才装载，所以当前进程拿不到。重新运行即可。");
            eprintln!("注意：重新编译会替换文件，需要再授权一次。");
            Ok(())
        }
        Outcome::AlreadyHave => Ok(()),
        Outcome::Declined => Err(io::Error::other("授权被取消")),
        Outcome::Unavailable(why) => {
            let hint = match authorize::method() {
                Method::Manual if !authorize::can_prompt() =>
                    "（没有图形会话，也没有可用的 sudo/终端）",
                _ => "",
            };
            eprintln!("没能就地授权{hint}。执行：\n  {why}");
            Ok(())
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
    }
    let r = match args[1].as_str() {
        "authorize" => cmd_authorize(&args[2..]),
        "daemon" => cmd_daemon(&args[2..]),
        "discover" => cmd_discover(&args[2..]),
        "send" => cmd_send(&args[2..]),
        "recv" => cmd_recv_once(&args[2..]),
        _ => usage(),
    };
    if let Err(e) = r {
        eprintln!("\n错误：{e}");
        std::process::exit(1);
    }
}
