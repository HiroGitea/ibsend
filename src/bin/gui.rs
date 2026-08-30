//! ibsend 的图形前端。
//!
//! 关键点：它不是「另一套实现」。后台线程跑的是和无头模式完全相同的
//! `daemon::run`，GUI 只是把事件画出来而不是打印出来。所以有头节点和无头
//! 节点是对等的——都能被发现、都能收、都能发。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use ibsend::daemon::{self, DaemonConfig, Event};
use ibsend::authorize::{self, Outcome};
use ibsend::{discover, send_files, tune, Manifest, Peer, Progress, RecvConfig, SendConfig, Stats};

const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x4a, 0x9e, 0xff);
const OK: egui::Color32 = egui::Color32::from_rgb(0x4c, 0xc3, 0x8a);
const WARN: egui::Color32 = egui::Color32::from_rgb(0xe8, 0xa8, 0x3a);
const DIM: egui::Color32 = egui::Color32::from_gray(150);

fn human(b: f64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut i) = (b, 0);
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    format!("{v:.2} {}", U[i])
}

enum SendMsg {
    Progress(Progress),
    Done(Stats),
    Failed(String),
}

#[derive(Default)]
struct PoolState {
    done: u64,
    total: u64,
    ready: bool,
    bind: String,
    err: Option<String>,
}

struct App {
    // 发现
    peers: Vec<Peer>,
    scanning: bool,
    scan_rx: Option<Receiver<Vec<Peer>>>,
    selected: usize,

    // 接收（后台守护，与无头模式同一份实现）
    daemon_rx: Receiver<Event>,
    pool: PoolState,
    incoming: Option<(Manifest, Progress)>,
    out_dir: PathBuf,

    // 发送
    queue: Vec<PathBuf>,
    send_rx: Option<Receiver<SendMsg>>,
    outgoing: Option<Progress>,

    log: VecDeque<(Instant, String)>,
    path_input: String,
    auth_rx: Option<Receiver<Outcome>>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_font(&cc.egui_ctx);

        let out_dir = default_out_dir();
        let _ = std::fs::create_dir_all(&out_dir);

        // 后台守护：和 `ibsend daemon` 跑的是同一个函数
        let (tx, daemon_rx) = channel();
        let out = out_dir.clone();
        let ctx = cc.egui_ctx.clone();
        thread::spawn(move || {
            let bind = match discover::local_ipoib().ok().and_then(|v| v.first().cloned()) {
                Some((_, ip, _)) => ip.to_string(),
                None => {
                    let _ = tx.send(Event::Failed("找不到 IPoIB 接口".into()));
                    ctx.request_repaint();
                    return;
                }
            };
            let mut recv = RecvConfig::new(bind);
            recv.out = out;
            let cfg = DaemonConfig { recv };
            let r = daemon::run(cfg, |ev| {
                let _ = tx.send(ev);
                ctx.request_repaint();
            });
            if let Err(e) = r {
                let _ = tx.send(Event::Failed(e.to_string()));
                ctx.request_repaint();
            }
        });

        let mut app = App {
            peers: Vec::new(),
            scanning: false,
            scan_rx: None,
            selected: 0,
            daemon_rx,
            pool: PoolState::default(),
            incoming: None,
            out_dir,
            queue: Vec::new(),
            send_rx: None,
            outgoing: None,
            log: VecDeque::new(),
            path_input: String::new(),
            auth_rx: None,
        };
        // 命令行带来的文件直接进队列。这样既能被文件管理器当「打开方式」调用，
        // 也让脚本化测试不必去戳界面坐标。
        for a in std::env::args().skip(1) {
            let p = PathBuf::from(a);
            if p.is_file() {
                app.queue.push(p);
            }
        }
        app.start_scan(&cc.egui_ctx);
        app
    }

    /// 弹 polkit 授权框。对话框会阻塞，所以放到后台线程去等。
    fn request_auth(&mut self) {
        if self.auth_rx.is_some() {
            return;
        }
        let (tx, rx) = channel();
        self.auth_rx = Some(rx);
        thread::spawn(move || {
            let _ = tx.send(authorize::request());
        });
        self.note("正在请求内存锁定权限…");
    }

    fn note(&mut self, s: impl Into<String>) {
        self.log.push_front((Instant::now(), s.into()));
        self.log.truncate(80);
    }

    fn start_scan(&mut self, ctx: &egui::Context) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        let (tx, rx) = channel();
        self.scan_rx = Some(rx);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let peers = discover::scan(ibsend::PORT, 300, 64);
            let _ = tx.send(peers);
            ctx.request_repaint();
        });
    }

    fn start_send(&mut self, ctx: &egui::Context) {
        let Some(peer) = self.peers.get(self.selected).cloned() else { return };
        if self.queue.is_empty() || self.send_rx.is_some() {
            return;
        }
        let paths = std::mem::take(&mut self.queue);
        let (files, skipped) = match ibsend::walk::expand(&paths) {
            Ok(v) => v,
            Err(e) => {
                self.note(format!("展开失败：{e}"));
                return;
            }
        };
        if files.is_empty() {
            self.note("没有可发送的文件");
            return;
        }
        if !skipped.is_empty() {
            self.note(format!("跳过 {}", skipped.describe()));
        }
        self.note(format!("发送 {} 个文件到 {}", files.len(), peer.name));

        let (tx, rx) = channel();
        self.send_rx = Some(rx);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let cfg = SendConfig::new(peer.addr.to_string());
            let t2 = tx.clone();
            let c2 = ctx.clone();
            let r = send_files(&cfg, &files, move |p| {
                let _ = t2.send(SendMsg::Progress(p));
                c2.request_repaint();
            });
            let _ = tx.send(match r {
                Ok(st) => SendMsg::Done(st),
                Err(e) => SendMsg::Failed(e.to_string()),
            });
            ctx.request_repaint();
        });
    }

    /// 收干净所有后台线程送来的消息
    fn drain(&mut self) {
        if let Some(rx) = &self.scan_rx {
            if let Ok(peers) = rx.try_recv() {
                let n = peers.len();
                self.peers = peers;
                self.selected = self.selected.min(self.peers.len().saturating_sub(1));
                self.scanning = false;
                self.scan_rx = None;
                self.note(format!("扫描完成，找到 {n} 个对端"));
            }
        }

        while let Ok(ev) = self.daemon_rx.try_recv() {
            match ev {
                Event::PoolProgress { done, total } => {
                    self.pool.done = done;
                    self.pool.total = total;
                }
                Event::Ready { pool, bind } => {
                    self.pool = PoolState { done: pool, total: pool, ready: true, bind, err: None };
                    self.note(format!("接收就绪，内存池 {}", human(pool as f64)));
                }
                Event::Started(m) => {
                    let p = Progress { bytes: 0, total: m.total(), staged: 0, elapsed: Duration::ZERO };
                    self.note(format!("接收 {} 个文件，共 {}", m.files.len(), human(m.total() as f64)));
                    self.incoming = Some((m, p));
                }
                Event::Progress(p) => {
                    if let Some((_, cur)) = &mut self.incoming {
                        *cur = p;
                    }
                }
                Event::Done(st, _) => {
                    self.incoming = None;
                    let mut s = format!("接收完成 {} @ {}/s", human(st.bytes as f64), human(st.rate()));
                    if st.skipped > 0 { s += &format!("，跳过 {}", st.skipped); }
                    if st.resumed > 0 { s += &format!("，续传 {}", st.resumed); }
                    if !st.bad.is_empty() { s += &format!("，✗ {} 个校验失败", st.bad.len()); }
                    self.note(s);
                }
                Event::Failed(e) => {
                    self.incoming = None;
                    if !self.pool.ready {
                        self.pool.err = Some(e.clone());
                    }
                    self.note(format!("接收失败：{e}"));
                }
                Event::Probed => {}
            }
        }

        // 先把消息全收出来，再逐条处理——否则 &self.send_rx 的借用会挡住 self.note
        let msgs: Vec<SendMsg> = match &self.send_rx {
            Some(rx) => rx.try_iter().collect(),
            None => Vec::new(),
        };
        // 授权结果
        if let Some(rx) = &self.auth_rx {
            if let Ok(outcome) = rx.try_recv() {
                self.auth_rx = None;
                match outcome {
                    Outcome::Granted => {
                        self.note("授权成功，正在重启以生效…");
                        // 能力在 execve 时才装载，当前进程拿不到，只能重启
                        let e = authorize::restart();
                        self.note(format!("重启失败：{e}"));
                    }
                    Outcome::AlreadyHave => self.note("已经有权限了"),
                    Outcome::Declined => self.note("授权被取消"),
                    Outcome::Unavailable(why) => self.note(format!("无法弹出授权框：{why}")),
                }
            }
        }

        let mut finished = false;
        for m in msgs {
            match m {
                SendMsg::Progress(p) => self.outgoing = Some(p),
                SendMsg::Done(st) => {
                    let mut s = format!("发送完成 {} / {:.2}s = {}/s",
                        human(st.bytes as f64), st.elapsed.as_secs_f64(), human(st.rate()));
                    if st.skipped > 0 { s += &format!("，对端已有 {} 个", st.skipped); }
                    if st.resumed > 0 { s += &format!("，续传 {}", st.resumed); }
                    if !st.peer_inbox.is_empty() {
                        s += &format!("  → 已存放于 {}", st.peer_inbox);
                    }
                    self.note(s);
                    finished = true;
                }
                SendMsg::Failed(e) => {
                    self.note(format!("发送失败：{e}"));
                    finished = true;
                }
            }
        }
        if finished {
            self.send_rx = None;
            self.outgoing = None;
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _f: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain();

        // 拖进来的文件进队列
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw.dropped_files.iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        for p in dropped {
            if !self.queue.contains(&p) {
                self.queue.push(p);
            }
        }

        // 快捷键
        let (want_send, want_scan) = ctx.input(|i| (
            i.modifiers.ctrl && i.key_pressed(egui::Key::S),
            i.modifiers.ctrl && i.key_pressed(egui::Key::R),
        ));
        if want_scan {
            self.start_scan(&ctx);
        }
        if want_send {
            self.start_send(&ctx);
        }

        self.header(ui);
        ui.add_space(6.0);
        self.devices(ui, &ctx);
        ui.add_space(6.0);
        self.dropzone(ui, &ctx);
        ui.add_space(6.0);
        self.transfers(ui);
        ui.add_space(6.0);
        self.logview(ui);

        // 有活儿的时候持续刷新
        if self.scanning || self.incoming.is_some() || self.send_rx.is_some()
            || !self.pool.ready || self.auth_rx.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

impl App {
    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("ibsend");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.pool.ready {
                    ui.colored_label(DIM, format!("本机 {}", self.pool.bind));
                }
            });
        });

        // 接收侧状态：这台机器自己也是个可被发现的接收端
        if let Some(e) = &self.pool.err {
            ui.colored_label(WARN, format!("⚠ 接收未就绪：{e}"));
        } else if self.pool.ready {
            ui.horizontal(|ui| {
                ui.colored_label(OK, "●");
                ui.label(format!("接收就绪 · 内存池 {} · 收到 {}",
                    human(self.pool.total as f64), self.out_dir.display()));
            });
            let why = tune::pool_explain(None);
            if !why.is_empty() {
                ui.horizontal(|ui| {
                    ui.colored_label(WARN, format!("⚠ {why}"));
                    let busy = self.auth_rx.is_some();
                    if authorize::can_prompt() {
                        let b = ui.add_enabled(!busy, egui::Button::new("授权"));
                        if b.on_hover_text(
                            "RDMA 需要把内存 pin 住不被换出，系统默认每进程只给 8 MB。\n\
                             会弹出系统授权框，给这个程序加上 CAP_IPC_LOCK 能力。\n\
                             注意：能力写在程序文件上，授权一次后一直有效；\n\
                             重新编译会替换文件，需要再授权一次。"
                        ).clicked() {
                            self.request_auth();
                        }
                        if busy {
                            ui.spinner();
                        }
                    } else {
                        ui.colored_label(DIM, "（无图形会话，跑 ibsend authorize）");
                    }
                });
            }
        } else {
            let f = if self.pool.total > 0 {
                self.pool.done as f32 / self.pool.total as f32
            } else {
                0.0
            };
            ui.horizontal(|ui| {
                ui.colored_label(WARN, "◌");
                ui.label("内存池注册中");
                ui.colored_label(DIM, "（预取页面，之后 ibv_reg_mr 会快很多）");
            });
            // 高度要够放下文字：6px 的条会把文字裁掉，看起来就是一条空杠
            ui.add(
                egui::ProgressBar::new(f)
                    .desired_height(20.0)
                    .fill(ACCENT)
                    .text(format!(
                        "{} / {}   {:.0}%",
                        human(self.pool.done as f64),
                        human(self.pool.total as f64),
                        f * 100.0
                    )),
            );
        }
        ui.separator();
    }

    fn devices(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.strong("设备");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(!self.scanning, egui::Button::new("重新扫描")).clicked() {
                    self.start_scan(ctx);
                }
                if self.scanning {
                    ui.spinner();
                }
            });
        });

        if self.peers.is_empty() {
            ui.colored_label(DIM, if self.scanning {
                "扫描 IPoIB 子网中…"
            } else {
                "没找到对端。对方需要在跑 ibsend daemon 或这个界面。"
            });
            return;
        }
        // radio 而不是高亮：只有一个设备时也能看出这是可选的
        for (i, p) in self.peers.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui.radio(i == self.selected, "").clicked() {
                    self.selected = i;
                }
                ui.strong(&p.name);
                ui.colored_label(DIM, format!("{}", p.addr));
                ui.colored_label(DIM, &p.note);
                if !p.inbox.is_empty() {
                    ui.colored_label(ACCENT, format!("→ {}", p.inbox))
                      .on_hover_text("发过去的文件会落在对端的这个目录");
                }
            });
        }
    }

    fn dropzone(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.separator();
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 56.0), egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_stroke(
            rect, 6.0,
            egui::Stroke::new(if hovering { 2.0 } else { 1.0 },
                              if hovering { ACCENT } else { egui::Color32::from_gray(90) }),
            egui::StrokeKind::Inside,
        );
        painter.text(
            rect.center(), egui::Align2::CENTER_CENTER,
            if hovering { "松手即可加入队列" } else { "把文件拖到这里" },
            egui::FontId::proportional(15.0),
            if hovering { ACCENT } else { DIM },
        );

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.colored_label(DIM, "或输入路径");
            let te = ui.add(egui::TextEdit::singleline(&mut self.path_input)
                .desired_width(ui.available_width() - 70.0)
                .hint_text("/path/to/file"));
            let enter = te.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button("加入").clicked() || enter) && !self.path_input.trim().is_empty() {
                let p = PathBuf::from(self.path_input.trim());
                if p.is_file() {
                    if !self.queue.contains(&p) {
                        self.queue.push(p);
                    }
                    self.path_input.clear();
                } else {
                    self.note(format!("{} 不是普通文件", p.display()));
                }
            }
        });

        if !self.queue.is_empty() {
            ui.add_space(4.0);
            let (items, _) = ibsend::walk::expand(&self.queue).unwrap_or_default();
            let total: u64 = items.iter()
                .filter_map(|i| std::fs::metadata(&i.path).ok().map(|m| m.len())).sum();
            ui.horizontal(|ui| {
                ui.label(format!("待发送 {} 个文件，共 {}", items.len(), human(total as f64)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("清空").clicked() {
                        self.queue.clear();
                    }
                    let can = !self.peers.is_empty() && self.send_rx.is_none();
                    let to = self.peers.get(self.selected).map(|p| p.name.clone());
                    let label = match &to {
                        Some(n) => format!("发送到 {n}"),
                        None => "发送".into(),
                    };
                    if ui.add_enabled(can, egui::Button::new(label)).clicked() {
                        self.start_send(ctx);
                    }
                });
            });
            egui::ScrollArea::vertical().max_height(70.0).id_salt("q").show(ui, |ui| {
                for it in items.iter().take(200) {
                    ui.colored_label(DIM, format!("  {}", it.name));
                }
                if items.len() > 200 {
                    ui.colored_label(DIM, format!("  …还有 {} 个", items.len() - 200));
                }
            });
        }
    }

    fn transfers(&mut self, ui: &mut egui::Ui) {
        if self.outgoing.is_none() && self.incoming.is_none() {
            return;
        }
        ui.separator();
        if let Some(p) = &self.outgoing {
            ui.strong("发送中");
            let f = if p.total > 0 { p.bytes as f32 / p.total as f32 } else { 0.0 };
            ui.add(egui::ProgressBar::new(f).text(format!(
                "{} / {}    {}/s", human(p.bytes as f64), human(p.total as f64), human(p.rate()))));
        }
        if let Some((m, p)) = &self.incoming {
            ui.strong(format!("接收中 · {} 个文件", m.files.len()));
            let f = if p.total > 0 { p.bytes as f32 / p.total as f32 } else { 0.0 };
            ui.add(egui::ProgressBar::new(f).text(format!(
                "{} / {}    {}/s", human(p.bytes as f64), human(p.total as f64), human(p.rate()))));

            // RAM 暂存深度——这个仪表是这套设计独有的：它显示网络比磁盘快多少
            let pool = self.pool.total.max(1);
            let sf = (p.staged as f32 / pool as f32).clamp(0.0, 1.0);
            ui.add(egui::ProgressBar::new(sf).desired_height(10.0)
                .fill(if sf > 0.85 { WARN } else { ACCENT })
                .text(format!("RAM 暂存 {} / {}", human(p.staged as f64), human(pool as f64))));
            if sf > 0.85 {
                ui.colored_label(WARN, "暂存快满了，发送端会被流控压到磁盘速度");
            }
        }
    }

    fn logview(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.horizontal(|ui| {
            ui.strong("日志");
            ui.colored_label(DIM, "  Ctrl+S 发送 · Ctrl+R 重新扫描");
        });
        egui::ScrollArea::vertical().max_height(150.0).id_salt("log").show(ui, |ui| {
            for (_, line) in &self.log {
                ui.colored_label(DIM, line);
            }
        });
    }
}

/// egui 自带字体不含 CJK，不装的话中文全是豆腐块
fn install_cjk_font(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto/NotoSansCJKsc-Regular.otf",
        "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc",
    ];
    let Some(bytes) = CANDIDATES.iter().find_map(|p| std::fs::read(p).ok()) else {
        eprintln!("警告：找不到 CJK 字体，中文会显示为方块");
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("cjk".into(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(fam).or_default().push("cjk".into());
    }
    ctx.set_fonts(fonts);
}

fn default_out_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dl = PathBuf::from(&home).join("Downloads");
    if dl.is_dir() { dl.join("ibsend") } else { PathBuf::from(home).join("ibsend-inbox") }
}

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 560.0])
            .with_min_inner_size([520.0, 440.0])
            .with_title("ibsend"),
        ..Default::default()
    };
    eframe::run_native("ibsend", opts, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
