//! 申请 CAP_IPC_LOCK 授权。
//!
//! 为什么需要：RDMA 注册内存要 pin 住物理页，而系统默认只允许每个进程 pin 8 MB。
//! 拿到 CAP_IPC_LOCK 就能绕开这个限制，接收端的内存暂存池才能真正开起来。
//!
//! 有图形会话时走 polkit（`pkexec`）弹桌面授权框；无头但有终端时走 `sudo`，
//! 让它在终端里问密码——那就是无头环境下的对等物。两条路都走不通（比如脚本
//! 里没有 tty）才退回到打印命令让人手动执行。
//!
//! 要注意它和 Windows 的 UAC
//! 不是一回事：`setcap` 把能力写进**二进制文件的扩展属性**，是对磁盘上那个文件
//! 的持久修改，不是"本次运行临时提权"。所以：
//!   - 授权一次之后，谁运行这个文件都带着这个能力
//!   - 能力在 execve 时才装载，所以当前进程拿不到，必须重启
//!   - 重新编译会替换文件，能力随之消失
//!
//! 分发时更好的做法是在安装包的 post-install 里做 setcap（见 README），
//! 这个运行时流程是给「直接跑构建产物」的人兜底的。
//!
//! 容器里这一套都不适用：能力由运行时的边界集决定，要在 Docker / Kubernetes
//! 那一侧配置，所以只给出配置提示（见 [`container_hint`]）。

use std::ffi::CString;
use std::io;
use std::path::{Path, PathBuf};

/// 用什么方式去要授权
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Method {
    /// 有图形会话，弹桌面授权框
    Polkit,
    /// 无头但有终端，让 sudo 在终端里问密码
    Sudo,
    /// 两条都不通（或者在容器里），只能让人手动处理
    Manual,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// 本来就有，什么都没做
    AlreadyHave,
    /// 授权成功，重启后生效
    Granted,
    /// 用户取消或密码错误
    Declined,
    /// 没法弹框（无桌面会话、缺 pkexec/setcap 等），附带手动命令
    Unavailable(String),
}

/// 本程序自己的真实路径。
///
/// 不能直接把 "/proc/self/exe" 传给 setcap —— 那个符号链接会在 **pkexec 自己的**
/// 进程上下文里解析，结果指向 pkexec。必须先解出真实路径。
pub fn self_path() -> io::Result<PathBuf> {
    std::fs::read_link("/proc/self/exe")
}

/// 是不是跑在容器里。
///
/// 容器里给二进制 setcap 没有意义：改动留在容器的可写层，重建就没了；能力能不能
/// 生效也取决于运行时给的边界集。这些都得在编排侧配置。
pub fn in_container() -> bool {
    use std::path::Path;
    Path::new("/.dockerenv").exists()
        || Path::new("/run/.containerenv").exists() // podman
        || std::env::var_os("KUBERNETES_SERVICE_HOST").is_some()
        || std::env::var_os("container").is_some() // systemd 的约定，podman/nspawn 会设
        || std::fs::read_to_string("/proc/1/cgroup").is_ok_and(|s| {
            ["docker", "kubepods", "containerd", "libpod"].iter().any(|k| s.contains(k))
        })
}

/// 本进程是否带着 no_new_privs。带着它时 execve 不会授予文件能力，
/// 非 root 用户靠镜像里的 setcap 拿 CAP_IPC_LOCK 这条路就断了。
fn no_new_privs() -> bool {
    std::fs::read_to_string("/proc/self/status").is_ok_and(|s| {
        s.lines().any(|l| l.split_whitespace().eq(["NoNewPrivs:", "1"]))
    })
}

/// 容器里怎么解除 memlock 限制
pub fn container_hint() -> String {
    let mut s = String::from(
        "容器里要由运行时授予 IPC_LOCK：Docker 加 `--cap-add IPC_LOCK`，\
         Kubernetes 在 securityContext.capabilities.add 里加 IPC_LOCK",
    );
    if no_new_privs() {
        s += "；当前进程带着 no_new_privs，非 root 时文件能力不会生效——\
              去掉 `--security-opt no-new-privileges`，或把 allowPrivilegeEscalation 设为 true";
    }
    s
}

/// 需要用户手动执行的命令，无法弹框时显示给他。容器里是一段配置提示。
pub fn manual_command() -> String {
    if in_container() {
        return container_hint();
    }
    match self_path() {
        Ok(p) => format!("sudo setcap cap_ipc_lock+ep {}", p.display()),
        Err(_) => "sudo setcap cap_ipc_lock+ep <ibsend 的路径>".into(),
    }
}

/// 在 PATH 里找出命令的完整路径
fn which(cmd: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(cmd))
        .find(|p| p.is_file())
}

/// fork + execv 跑一个命令并等它结束，返回退出码。
///
/// 不用 `std::process::Command`：它在新 glibc 上会走 `pidfd_spawnp`（2.39 才有），
/// 于是在滚动发行版上编出的二进制跑不了 Debian 12 这类稳定系统。
/// 用完整路径 + `execv` 而不是 `execvp`，避免在 fork 之后的子进程里做 PATH 查找
/// ——多线程程序 fork 之后只能调用异步信号安全的函数。
fn run(prog: &Path, args: &[&str]) -> io::Result<i32> {
    let mut owned = vec![CString::new(prog.as_os_str().as_encoded_bytes())?];
    for a in args {
        owned.push(CString::new(*a)?);
    }
    let mut argv: Vec<*const libc::c_char> = owned.iter().map(|c| c.as_ptr()).collect();
    argv.push(std::ptr::null());

    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            libc::execv(argv[0], argv.as_ptr());
            libc::_exit(127); // execv 失败才会走到这里
        }
        let mut status: libc::c_int = 0;
        if libc::waitpid(pid, &mut status, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { -1 })
    }
}

/// 标准输入是不是终端——决定 sudo 能不能问密码
fn has_tty() -> bool {
    unsafe { libc::isatty(0) == 1 }
}

fn has_display() -> bool {
    std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// 这台机器上该用哪种方式要授权
pub fn method() -> Method {
    if in_container() || which("setcap").is_none() {
        return Method::Manual; // 连 setcap 都没有，给命令让人自己想办法
    }
    if has_display() && which("pkexec").is_some() {
        Method::Polkit
    } else if has_tty() && which("sudo").is_some() {
        Method::Sudo
    } else {
        Method::Manual
    }
}

/// 能不能就地要到授权（不管是弹框还是终端里问密码）
pub fn can_prompt() -> bool {
    method() != Method::Manual
}

/// 弹框申请授权。阻塞直到用户处理完对话框。
pub fn request() -> Outcome {
    if crate::tune::has_ipc_lock() {
        return Outcome::AlreadyHave;
    }
    let m = method();
    if m == Method::Manual {
        return Outcome::Unavailable(manual_command());
    }
    let path = match self_path() {
        Ok(p) => p,
        Err(e) => return Outcome::Unavailable(format!("找不到自身路径：{e}")),
    };
    let target = path.to_string_lossy().into_owned();

    let (prog, decline) = match m {
        // pkexec 用 126 表示用户取消或认证失败
        Method::Polkit => (which("pkexec"), 126),
        // sudo 认证失败退 1；fork+execv 会继承终端，所以它能就地问密码
        Method::Sudo => (which("sudo"), 1),
        Method::Manual => unreachable!(),
    };
    let prog = match prog {
        Some(p) => p,
        None => return Outcome::Unavailable(manual_command()),
    };

    if m == Method::Sudo {
        eprintln!("需要 root 权限给程序加上 CAP_IPC_LOCK，下面由 sudo 询问密码：");
    }
    match run(&prog, &["setcap", "cap_ipc_lock+ep", &target]) {
        Ok(0) => Outcome::Granted,
        Ok(c) if c == decline => Outcome::Declined,
        Ok(c) => Outcome::Unavailable(format!(
            "setcap 失败（退出码 {c}）。手动执行：{}", manual_command())),
        Err(e) => Outcome::Unavailable(format!("{e}。手动执行：{}", manual_command())),
    }
}

/// 重启自己让新能力生效。文件能力在 execve 时才装载，当前进程拿不到。
///
/// 用「起新进程 + 自己退出」而不是 exec 替换镜像：exec 会保留没设 CLOEXEC 的
/// 文件描述符，而 rdma_cm 的监听端口是否 CLOEXEC 并无保证，留着会让新进程绑不上。
/// 新进程那边的绑定有重试，足以覆盖两个进程短暂并存的窗口。
pub fn restart() -> io::Error {
    let exe = match self_path() {
        Ok(p) => p,
        Err(e) => return e,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut owned = match CString::new(exe.as_os_str().as_encoded_bytes()) {
        Ok(c) => vec![c],
        Err(e) => return io::Error::other(e),
    };
    for a in &args {
        match CString::new(a.as_str()) {
            Ok(c) => owned.push(c),
            Err(e) => return io::Error::other(e),
        }
    }
    let mut argv: Vec<*const libc::c_char> = owned.iter().map(|c| c.as_ptr()).collect();
    argv.push(std::ptr::null());

    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            return io::Error::last_os_error();
        }
        if pid == 0 {
            libc::setsid(); // 脱离当前会话，免得父进程退出时被一起收走
            libc::execv(argv[0], argv.as_ptr());
            libc::_exit(127);
        }
    }
    std::process::exit(0);
}
