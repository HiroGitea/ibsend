//! ibsend CLI —— 参数解析和文字前端。所有逻辑都在 lib 里。
//!
//! 无头和有头是对等的：两边都跑同一个守护循环，都能被发现、都能收发。
//! GUI 只是这个守护的另一个前端（见 --features gui）。
//!
//! 输出有三种：终端里原地刷新进度；输出被重定向时（docker logs、kubectl logs）
//! 按行、放慢节奏；`--json` 时 stdout 每行一个事件，给脚本和编排系统解析。

use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ibsend::daemon::{self, DaemonConfig, Event};
use ibsend::json::Obj;
use ibsend::{discover, send_files, Manifest, Progress, RecvConfig, Receiver, SendConfig, Stats};

/// 退出码是给脚本用的契约，改了就是破坏兼容
const EXIT_FAIL: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_VERIFY: i32 = 3;
const EXIT_NOT_FOUND: i32 = 4;

/// 出错时要不要在 stdout 补一条 error 事件。由 `Out::new` 设置，
/// 这样它和各命令自己解析出来的 --json 永远一致。
static JSON: AtomicBool = AtomicBool::new(false);

/// 命令失败：退出码 + 人话
struct Failure {
    code: i32,
    msg: String,
}

impl Failure {
    fn new(code: i32, msg: impl Into<String>) -> Failure {
        Failure { code, msg: msg.into() }
    }
}

impl From<io::Error> for Failure {
    fn from(e: io::Error) -> Failure {
        Failure::new(EXIT_FAIL, e.to_string())
    }
}

type CmdResult = Result<(), Failure>;

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

fn stderr_is_tty() -> bool {
    unsafe { libc::isatty(2) == 1 }
}

/// 展示用的绝对路径。相对路径（比如默认的 "."）对别人毫无意义。
fn abs(p: &Path) -> String {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().into_owned()
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// 终端：原地刷新
    Tty,
    /// 被重定向：按行输出、放慢节奏，不用 \r
    Log,
    /// --json：stdout 每行一个事件，stderr 只留警告
    Json,
}

/// 行尾补的空格，盖掉原地刷新时上一行残留的字
const PAD: &str = "                              ";

struct Out {
    mode: Mode,
    last: Option<Instant>,
}

impl Out {
    fn new(json: bool) -> Out {
        JSON.store(json, Ordering::Relaxed);
        let mode = if json {
            Mode::Json
        } else if stderr_is_tty() {
            Mode::Tty
        } else {
            Mode::Log
        };
        Out { mode, last: None }
    }

    fn json(&self) -> bool {
        self.mode == Mode::Json
    }

    /// 一行给人看的话。json 模式下不说——那时 stderr 只留警告。
    fn say(&self, msg: impl AsRef<str>) {
        match self.mode {
            Mode::Tty => eprintln!("\r{}{PAD}", msg.as_ref()),
            Mode::Log => eprintln!("{}", msg.as_ref()),
            Mode::Json => {}
        }
    }

    /// 警告在任何模式下都打印
    fn warn(&self, msg: impl AsRef<str>) {
        eprintln!("{}", msg.as_ref());
    }

    /// 一条事件，只在 json 模式下输出
    fn event(&self, obj: Obj) {
        self.emit(&obj.finish());
    }

    fn emit(&self, line: &str) {
        if self.json() {
            let mut out = io::stdout().lock();
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
    }

    /// 进度，按模式节流。`recv` 为真时带上 RAM 暂存量。
    fn progress(&mut self, p: &Progress, recv: bool) {
        let every = match self.mode {
            Mode::Tty => Duration::from_millis(400),
            Mode::Log => Duration::from_secs(5),
            Mode::Json => Duration::from_secs(1),
        };
        if self.last.is_some_and(|t| t.elapsed() < every) {
            return;
        }
        self.last = Some(Instant::now());

        if self.json() {
            let mut o = Obj::event("progress")
                .uint("bytes", p.bytes)
                .uint("total", p.total)
                .float("rate", p.rate())
                .float("elapsed", p.elapsed.as_secs_f64());
            if recv {
                o = o.uint("staged", p.staged);
            }
            return self.event(o);
        }
        let mut line = format!("  {} / {}   {}/s",
                               human(p.bytes as f64), human(p.total as f64), human(p.rate()));
        if recv {
            line += &format!("   RAM 暂存 {}", human(p.staged as f64));
        }
        if self.mode == Mode::Tty {
            eprint!("\r{line}      ");
            let _ = io::stderr().flush();
        } else {
            eprintln!("{line}");
        }
    }

    /// 新的一次传输开始：让它的第一条进度立即出现
    fn restart(&mut self) {
        self.last = None;
    }
}

fn started_event(m: &Manifest) -> Obj {
    Obj::event("started").uint("files", m.files.len() as u64).uint("bytes", m.total())
}

fn done_event(st: &Stats, recv: bool) -> Obj {
    let o = Obj::event("done")
        .uint("bytes", st.bytes)
        .float("elapsed", st.elapsed.as_secs_f64())
        .float("rate", st.rate())
        .uint("skipped", st.skipped as u64)
        .uint("resumed", st.resumed as u64);
    if recv {
        o.uint("verified", st.verified as u64).strs("bad", &st.bad)
    } else {
        o.str("inbox", &st.peer_inbox)
    }
}

/// 就绪信号：内存池注册完之后写一个文件，给容器探针和编排脚本用。
///
/// 启动时先删掉旧的——它可能留在持久卷上，来自上一次运行，
/// 那时的就绪和这一次毫无关系。
struct ReadyFile(Option<PathBuf>);

impl ReadyFile {
    fn new(path: Option<PathBuf>) -> io::Result<ReadyFile> {
        if let Some(p) = &path {
            match std::fs::remove_file(p) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        Ok(ReadyFile(path))
    }

    /// 先写临时文件再改名，探针不会读到写了一半的内容
    fn mark(&self, content: &str) -> io::Result<()> {
        let Some(p) = &self.0 else { return Ok(()) };
        if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, format!("{content}\n"))?;
        std::fs::rename(&tmp, p)
    }
}

/// 就绪事件。它同时是就绪文件的内容，编排脚本从里面取监听地址。
fn ready_line(bind: &str, port: u16, pool: u64, cfg_name: &str, inbox: &str) -> String {
    let name = if cfg_name.is_empty() { ibsend::proto::hostname() } else { cfg_name.to_string() };
    Obj::event("ready")
        .str("bind", bind)
        .uint("port", port as u64)
        .uint("pool", pool)
        .str("name", &name)
        .str("inbox", inbox)
        .finish()
}

/// rdma_cm 的字符设备。没有它什么都做不了，而底层只会报一句 "No such device"。
fn require_rdma_dev() -> CmdResult {
    if Path::new("/dev/infiniband/rdma_cm").exists() {
        return Ok(());
    }
    let how = if ibsend::authorize::in_container() {
        "容器需要 `--device /dev/infiniband`；Kubernetes 里要申请 RDMA 设备资源\
         （例如 rdma/hca_shared_devices_a）"
    } else {
        "确认 rdma_ucm 内核模块已加载（modprobe rdma_ucm）"
    };
    Err(Failure::new(EXIT_FAIL, format!("找不到 /dev/infiniband/rdma_cm：{how}")))
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
      --port <N>     rdma_cm 端口，默认 18515
      --out <DIR>    落盘目录，默认当前目录
      --pool <SIZE>  RAM 暂存池，默认按可用内存（含容器内存限额）自适应
      --slab <SIZE>  单块大小，默认按传输大小自适应
      --name <名字>  对外显示的设备名，默认用主机名
      --ready-file <PATH>
                     内存池就绪后创建这个文件，内容是 ready 事件（给容器探针用）
      --json         stdout 每行输出一个 JSON 事件

  ibsend discover [选项]          扫描 IPoIB 子网，列出在线的对端（含本机）
      --timeout <MS> 单地址解析超时，默认 300
      --port <N>     对端端口，默认 18515
      --json

  ibsend send <对端|名字> <文件或目录>...  发送（目录会递归展开）
      --slabs <N>    本地流水线深度，默认 16
      --port <N>     对端端口，默认 18515
      --prefix <DIR> 放进对端落盘目录下的这个子目录
      --json

  ibsend recv [选项]              收一次就退出（同 daemon 的选项）

  ibsend authorize [--force]      申请内存锁定权限（RDMA 需要 pin 内存，系统
                                  默认只给 8MB）。额度已经够用时不会打扰你，
                                  --force 可以强制申请

退出码：0 成功，1 失败，2 参数错误，3 校验失败，4 按名字找不到对端。

无头和有头是对等的：两边都跑 daemon，都能被发现、都能互相收发。
带界面的版本见 ibsend-gui（cargo build --features gui）。

符号链接会被跳过而不是跟随。");
    std::process::exit(EXIT_USAGE);
}

fn parse_port(v: Option<&String>) -> u16 {
    v.and_then(|s| s.parse().ok()).filter(|&p| p > 0).unwrap_or_else(|| usage())
}

/// daemon/recv 共用的参数
struct RecvArgs {
    cfg: RecvConfig,
    json: bool,
    ready_file: Option<PathBuf>,
}

fn parse_recv_args(args: &[String]) -> RecvArgs {
    let mut cfg = RecvConfig::new("");
    let (mut json, mut ready_file) = (false, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--json" {
            json = true;
            continue;
        }
        if a == "--port" {
            cfg.port = parse_port(it.next());
            continue;
        }
        let v = it.next().cloned().unwrap_or_else(|| usage());
        match a.as_str() {
            "--bind" => cfg.bind = v,
            "--out" => cfg.out = PathBuf::from(v),
            "--pool" => cfg.pool_bytes = Some(parse_size(&v).unwrap_or_else(|| usage())),
            "--slab" => cfg.slab = Some(parse_size(&v).unwrap_or_else(|| usage())),
            "--name" => cfg.name = v,
            "--ready-file" => ready_file = Some(PathBuf::from(v)),
            _ => usage(),
        }
    }
    if cfg.bind.is_empty() {
        cfg.bind = auto_bind().unwrap_or_else(|| {
            eprintln!("错误：找不到 IPoIB 接口，请用 --bind 指定地址\n");
            usage()
        });
    }
    RecvArgs { cfg, json, ready_file }
}

/// 启动时关于内存池的提醒。措辞跟着实际可用的方式走——
/// 无头机器上让人去跑一个只会打印另一条命令的命令，是在兜圈子；
/// 容器里则只能在运行时那一侧解决。
fn warn_memlock(out: &Out, pool: Option<u64>) {
    use ibsend::authorize;
    if let Some(lim) = ibsend::tune::cgroup_mem_limit() {
        out.say(format!("内存限额 {}（cgroup），内存池按它计算", human(lim as f64)));
    }
    let why = ibsend::tune::pool_explain(pool);
    if why.is_empty() {
        return;
    }
    out.warn(format!("⚠ {why}"));
    if authorize::in_container() {
        out.warn(format!("  {}", authorize::container_hint()));
    } else if authorize::can_prompt() {
        out.warn("  跑一次 `ibsend authorize` 解除");
    } else {
        out.warn(format!("  执行：{}", authorize::manual_command()));
    }
}

fn cmd_daemon(args: &[String]) -> CmdResult {
    let RecvArgs { cfg, json, ready_file } = parse_recv_args(args);
    let mut out = Out::new(json);
    require_rdma_dev()?;
    warn_memlock(&out, cfg.pool_bytes);
    let ready = ReadyFile::new(ready_file)?;
    let (port, name, inbox) = (cfg.port, cfg.name.clone(), abs(&cfg.out));

    daemon::run(DaemonConfig { recv: cfg }, |ev| {
        match &ev {
            Event::Progress(p) => return out.progress(p, true),
            Event::Ready { pool, bind } => {
                let line = ready_line(bind, port, *pool, &name, &inbox);
                if let Err(e) = ready.mark(&line) {
                    out.warn(format!("⚠ 写不了就绪文件：{e}"));
                }
                out.emit(&line);
            }
            Event::Started(m) => {
                out.restart();
                out.event(started_event(m));
            }
            Event::Done(st, _) => out.event(done_event(st, true)),
            Event::Failed(e) => out.event(Obj::event("failed").str("message", e)),
            Event::Probed | Event::PoolProgress { .. } => {}
        }
        if let Some(line) = daemon::describe(&ev) {
            out.say(line);
        }
    })?;
    Ok(())
}

fn cmd_discover(args: &[String]) -> CmdResult {
    let (mut tmo, mut port, mut json) = (300i32, ibsend::PORT, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--timeout" => {
                tmo = it.next().and_then(|s| s.parse().ok()).filter(|&t| t > 0)
                    .unwrap_or_else(|| usage());
            }
            "--port" => port = parse_port(it.next()),
            "--json" => json = true,
            _ => usage(),
        }
    }
    let out = Out::new(json);
    require_rdma_dev()?;

    let locals = match discover::local_ipoib() {
        Ok(v) if !v.is_empty() => v,
        _ => return Err(Failure::new(EXIT_FAIL, "找不到 IPoIB 接口")),
    };
    for (name, ip, mask) in &locals {
        out.say(format!("本机 {name}  {ip}/{mask}"));
        out.event(Obj::event("local")
            .str("iface", name)
            .str("addr", &ip.to_string())
            .str("mask", &mask.to_string()));
    }
    out.say("扫描中…");
    let peers = discover::scan_including_local(port, tmo, 64);
    if peers.is_empty() {
        out.say("没找到对端（对方需要在跑 ibsend daemon）");
    }
    for p in &peers {
        let local = locals.iter().any(|(_, ip, _)| *ip == p.addr);
        if out.json() {
            out.event(Obj::event("peer")
                .str("name", &p.name)
                .str("addr", &p.addr.to_string())
                .str("note", &p.note)
                .str("inbox", &p.inbox)
                .bool("local", local));
        } else {
            println!("  {:<16} {:<20} {:<22} → {}{}", p.addr, p.name, p.note, p.inbox,
                     if local { "  （本机）" } else { "" });
        }
    }
    Ok(())
}

fn cmd_send(args: &[String]) -> CmdResult {
    let (mut slabs, mut port, mut json, mut prefix) = (16u32, ibsend::PORT, false, None);
    let mut pos = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--slabs" => slabs = it.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| usage()),
            "--port" => port = parse_port(it.next()),
            "--prefix" => {
                let v = it.next().unwrap_or_else(|| usage());
                // 名字最终要过接收端的 safe_relpath，这里先挡一道，免得传完才发现被改名
                let p = ibsend::walk::safe_relpath(v).unwrap_or_else(|| {
                    eprintln!("错误：--prefix 必须是不含 .. 的相对路径\n");
                    usage()
                });
                prefix = Some(p.to_string_lossy().into_owned());
            }
            "--json" => json = true,
            "--" => {
                pos.extend(it.by_ref().cloned());
                break;
            }
            _ => pos.push(a.clone()),
        }
    }
    if pos.len() < 2 {
        usage();
    }
    let mut out = Out::new(json);
    require_rdma_dev()?;
    let target = pos.remove(0);
    let paths: Vec<PathBuf> = pos.into_iter().map(PathBuf::from).collect();

    // 不是 IP 就当成名字，扫一遍子网找它
    let peer = if target.parse::<Ipv4Addr>().is_ok() {
        target
    } else {
        out.say(format!("按名字查找 {target}…"));
        let peers = discover::scan_including_local(port, 300, 64);
        match peers.iter().find(|p| p.name == target) {
            Some(p) => p.addr.to_string(),
            None => return Err(Failure::new(EXIT_NOT_FOUND, format!("没找到名为 {target} 的对端"))),
        }
    };

    // 目录在这里展开成文件列表，名字带上相对路径
    let (mut items, skipped) = ibsend::walk::expand(&paths)?;
    if items.is_empty() {
        return Err(Failure::new(EXIT_FAIL, "没有可发送的文件"));
    }
    if let Some(pre) = &prefix {
        for i in &mut items {
            i.name = format!("{pre}/{}", i.name);
        }
    }
    if !skipped.is_empty() {
        out.say(format!("跳过 {}", skipped.describe()));
    }
    let total: u64 = items.iter()
        .filter_map(|i| std::fs::metadata(&i.path).ok().map(|m| m.len()))
        .sum();
    out.say(format!("{} 个文件，共 {}", items.len(), human(total as f64)));
    out.event(Obj::event("started")
        .str("peer", &peer)
        .uint("files", items.len() as u64)
        .uint("bytes", total));

    let mut cfg = SendConfig::new(peer);
    cfg.port = port;
    cfg.local_slabs = slabs;
    let st = send_files(&cfg, &items, |p: Progress| out.progress(&p, false))?;
    out.event(done_event(&st, false));
    out.say(format!("发送完成：{} / {:.2}s = {}/s",
                    human(st.bytes as f64), st.elapsed.as_secs_f64(), human(st.rate())));
    if !st.peer_inbox.is_empty() {
        out.say(format!("  对端已存放于 {}", st.peer_inbox));
    }
    if st.skipped > 0 {
        out.say(format!("  对端已有 {} 个文件，跳过", st.skipped));
    }
    if st.resumed > 0 {
        out.say(format!("  {} 个文件从中断处续传", st.resumed));
    }
    Ok(())
}

fn cmd_recv_once(args: &[String]) -> CmdResult {
    let RecvArgs { cfg, json, ready_file } = parse_recv_args(args);
    let mut out = Out::new(json);
    require_rdma_dev()?;
    warn_memlock(&out, cfg.pool_bytes);
    let ready = ReadyFile::new(ready_file)?;
    let (bind, port, name, inbox) = (cfg.bind.clone(), cfg.port, cfg.name.clone(), abs(&cfg.out));

    let mut rx = Receiver::start(cfg)?;
    out.say(format!("内存池 {}（后台注册中，与等待连接重叠）", human(rx.pool_bytes() as f64)));
    loop {
        let (done, total, registered) = rx.pool_progress()?;
        if registered {
            break;
        }
        if out.mode == Mode::Tty {
            eprint!("\r  注册中 {:.0}%  ", done as f64 / total.max(1) as f64 * 100.0);
            let _ = io::stderr().flush();
        }
        std::thread::sleep(Duration::from_millis(80));
    }
    let line = ready_line(&bind, port, rx.pool_bytes(), &name, &inbox);
    ready.mark(&line)?;
    out.emit(&line);
    out.say(format!("内存池就绪 {}", human(rx.pool_bytes() as f64)));
    out.say(format!("等待发送端  {bind}:{port}（rdma_cm 端口，不是 TCP）"));

    let m = loop {
        match rx.accept_one(2000)? {
            ibsend::Incoming::Transfer(m) => break m,
            ibsend::Incoming::Dropped(why) => {
                out.say(format!("丢弃一条连接：{why}"));
                out.event(Obj::event("failed").str("message", &format!("丢弃一条连接：{why}")));
            }
            _ => {}
        }
    };
    out.say(format!("收到清单：{} 个文件，共 {}", m.files.len(), human(m.total() as f64)));
    out.event(started_event(&m));

    let st = rx.receive(&m, |p: Progress| out.progress(&p, true))?;
    let _ = rx.end_session();
    out.event(done_event(&st, true));
    out.say(format!("完成：{} / {:.2}s = {}/s（含落盘）",
                    human(st.bytes as f64), st.elapsed.as_secs_f64(), human(st.rate())));
    if st.skipped > 0 { out.say(format!("  跳过 {} 个已完整存在的文件", st.skipped)); }
    if st.resumed > 0 { out.say(format!("  {} 个文件从中断处续传", st.resumed)); }
    if st.verified > 0 { out.say(format!("  CRC32C 校验通过 {} 个文件", st.verified)); }
    for n in &st.bad { out.say(format!("  ✗ 校验失败：{n}")); }
    if !st.bad.is_empty() {
        return Err(Failure::new(EXIT_VERIFY, format!("{} 个文件校验失败", st.bad.len())));
    }
    Ok(())
}

fn cmd_authorize(args: &[String]) -> CmdResult {
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
    if authorize::in_container() {
        eprintln!("容器里不能给程序文件授权。\n  {}", authorize::container_hint());
        return Ok(());
    }
    match authorize::request() {
        Outcome::Granted => {
            eprintln!("授权成功。权限在下次启动时生效——它写在程序文件的扩展属性里，");
            eprintln!("execve 时才装载，所以当前进程拿不到。重新运行即可。");
            eprintln!("注意：重新编译会替换文件，需要再授权一次。");
            Ok(())
        }
        Outcome::AlreadyHave => Ok(()),
        Outcome::Declined => Err(Failure::new(EXIT_FAIL, "授权被取消")),
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
    if let Err(f) = r {
        if JSON.load(Ordering::Relaxed) {
            println!("{}", Obj::event("error").uint("code", f.code as u64).str("message", &f.msg).finish());
        }
        // 终端里进度可能还停在当前行，先换行
        let nl = if stderr_is_tty() { "\n" } else { "" };
        eprintln!("{nl}错误：{}", f.msg);
        std::process::exit(f.code);
    }
}
