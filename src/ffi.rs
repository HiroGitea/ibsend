//! csrc/ibx.c 的 FFI 绑定（crate 内部）。
//!
//! 重要约束：libibverbs 的 QP 操作不是线程安全的，所有 ibx_* 调用必须发生在
//! 同一个线程上。落盘线程只负责写盘，通过 channel 把完成的块号回传给主线程。

use std::ffi::{c_char, c_int, c_void, CStr};
use std::os::raw::c_uint;

pub const ERRLEN: usize = 256;

#[repr(C)]
pub struct Ibx {
    _opaque: [u8; 0],
}

extern "C" {
    // 接收端
    pub fn ibx_listen_start(bind_ip: *const c_char, port: u16,
                            pool_bytes: usize, err: *mut c_char) -> *mut Ibx;
    pub fn ibx_pool_progress(x: *mut Ibx, done: *mut u64, total: *mut u64,
                             err: *mut c_char) -> c_int;
    pub fn ibx_listen_accept(x: *mut Ibx, timeout_ms: c_int, err: *mut c_char) -> c_int;
    pub fn ibx_recv_arm(x: *mut Ibx, slab_size: u32, nslabs: u32, err: *mut c_char) -> c_int;
    pub fn ibx_pool_addr(x: *const Ibx) -> u64;
    pub fn ibx_pool_rkey(x: *const Ibx) -> u32;
    pub fn ibx_recv_next(x: *mut Ibx, buf: *mut *mut c_void, len: *mut usize,
                         seq: *mut u64, timeout_ms: c_int, err: *mut c_char) -> c_int;
    pub fn ibx_recv_release(x: *mut Ibx, seq: u64, err: *mut c_char) -> c_int;
    pub fn ibx_recv_wake(x: *mut Ibx);
    pub fn ibx_session_end(x: *mut Ibx, err: *mut c_char) -> c_int;

    // 发送端
    pub fn ibx_connect(peer_ip: *const c_char, port: u16, resolve_ms: c_int,
                       again: *mut c_int, err: *mut c_char) -> *mut Ibx;
    pub fn ibx_send_arm(x: *mut Ibx, slab_size: u32, local_slabs: u32, peer_addr: u64,
                        peer_rkey: u32, peer_nslabs: u32, err: *mut c_char) -> c_int;
    pub fn ibx_send_acquire(x: *mut Ibx, seq: *mut u64, err: *mut c_char) -> *mut c_void;
    pub fn ibx_send_commit(x: *mut Ibx, seq: u64, len: usize, err: *mut c_char) -> c_int;
    pub fn ibx_send_drain(x: *mut Ibx, err: *mut c_char) -> c_int;

    // 控制通道（QP 上的 SEND/RECV，取代了原先那条 TCP）
    pub fn ibx_ctl_send(x: *mut Ibx, buf: *const u8, len: u32, err: *mut c_char) -> c_int;
    pub fn ibx_ctl_recv(x: *mut Ibx, buf: *mut u8, cap: u32, out_len: *mut u32,
                        timeout_ms: c_int, err: *mut c_char) -> c_int;

    // 杂项
    pub fn ibx_close(x: *mut Ibx);
    pub fn ibx_bytes(x: *const Ibx) -> u64;
    #[allow(dead_code)]
    pub fn ibx_slab_size(x: *const Ibx) -> c_uint;
    pub fn ibx_inflight(x: *const Ibx) -> u64;
    pub fn ibx_list_ipoib(buf: *mut c_char, buflen: usize) -> c_int;
}

/// C 侧错误缓冲区，用完转成 Rust String
pub struct ErrBuf([c_char; ERRLEN]);

impl ErrBuf {
    pub fn new() -> Self {
        ErrBuf([0; ERRLEN])
    }
    pub fn as_mut_ptr(&mut self) -> *mut c_char {
        self.0.as_mut_ptr()
    }
    pub fn take(&self) -> String {
        // SAFETY: C 侧一律用 snprintf 写入，保证 NUL 结尾
        unsafe { CStr::from_ptr(self.0.as_ptr()) }.to_string_lossy().into_owned()
    }
}

/// 控制通道消息的最大长度。清单条目多时会用到，但也要挡住畸形长度。
pub const CTL_MAX: u32 = 16 << 20;

/// 在 QP 上发一条控制消息
pub fn ctl_send(x: *mut Ibx, msg: &[u8]) -> std::io::Result<()> {
    let mut err = ErrBuf::new();
    if msg.len() as u64 > CTL_MAX as u64 {
        return Err(std::io::Error::other("控制消息过长"));
    }
    if unsafe { ibx_ctl_send(x, msg.as_ptr(), msg.len() as u32, err.as_mut_ptr()) } < 0 {
        return Err(std::io::Error::other(format!("ctl_send: {}", err.take())));
    }
    Ok(())
}

/// 在 QP 上收一条控制消息。超时返回 ErrorKind::TimedOut。
pub fn ctl_recv(x: *mut Ibx, timeout_ms: i32) -> std::io::Result<Vec<u8>> {
    let mut err = ErrBuf::new();
    let mut buf = vec![0u8; CTL_MAX as usize];
    let mut n: u32 = 0;
    match unsafe {
        ibx_ctl_recv(x, buf.as_mut_ptr(), CTL_MAX, &mut n, timeout_ms, err.as_mut_ptr())
    } {
        0 => {
            buf.truncate(n as usize);
            Ok(buf)
        }
        2 => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "等控制消息超时")),
        _ => Err(std::io::Error::other(format!("ctl_recv: {}", err.take()))),
    }
}

/// `*mut Ibx` 的 RAII 包装。裸指针本身不是 Send，但连接在整个生命周期里
/// 只被一个线程使用，跨线程移交所有权是安全的。
pub struct Owned(pub *mut Ibx);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ibx_close(self.0) };
        }
    }
}

unsafe impl Send for Owned {}
