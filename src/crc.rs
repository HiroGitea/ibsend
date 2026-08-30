//! CRC32C（Castagnoli）。
//!
//! 这一层校验和**不是**为了防网络错误——IB 每个包带 ICRC(CRC-32) 和
//! VCRC(CRC-16)，由 HCA 硬件计算和校验，RC 传输还有硬件重传，数据在网线上
//! 被改坏这件事已经不可能发生了。
//!
//! 它防的是另一类问题：本程序自己的 bug（偏移算错、slab 复用竞态）、非 ECC
//! 内存翻转、磁盘写入错误。这一类无法卸载到 ConnectX-2（签名/T10-DIF offload
//! 是 CX-4+/mlx5 的特性，mlx4 没有），只能软件算。
//!
//! 但代价接近于零：接收端在落盘线程上算，那个线程本来就在等磁盘 I/O；
//! 而 SSE4.2 的 crc32 指令跑得比 3.2 GB/s 的链路快得多。

#[derive(Clone)]
pub struct Crc32c(u32);

impl Default for Crc32c {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32c {
    pub fn new() -> Self {
        Crc32c(!0)
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0 = update(self.0, data);
    }

    pub fn finish(&self) -> u32 {
        !self.0
    }
}

/// 一次性算一段数据的 CRC32C
pub fn crc32c(data: &[u8]) -> u32 {
    !update(!0, data)
}

// ---------------------------------------------------------------- 分发

#[cfg(target_arch = "x86_64")]
fn update(crc: u32, data: &[u8]) -> u32 {
    use std::sync::atomic::{AtomicU8, Ordering};
    static HW: AtomicU8 = AtomicU8::new(2); // 2 = 未探测
    let hw = match HW.load(Ordering::Relaxed) {
        2 => {
            let v = u8::from(is_x86_feature_detected!("sse4.2"));
            HW.store(v, Ordering::Relaxed);
            v
        }
        v => v,
    };
    if hw == 1 {
        // SAFETY: 上面刚探测过 sse4.2
        unsafe { update_sse42(crc, data) }
    } else {
        update_soft(crc, data)
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn update(crc: u32, data: &[u8]) -> u32 {
    update_soft(crc, data)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn update_sse42(mut crc: u32, data: &[u8]) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u64, _mm_crc32_u8};
    let mut i = 0;
    while i + 8 <= data.len() {
        let v = u64::from_le_bytes(data[i..i + 8].try_into().unwrap());
        crc = _mm_crc32_u64(crc as u64, v) as u32;
        i += 8;
    }
    while i < data.len() {
        crc = _mm_crc32_u8(crc, data[i]);
        i += 1;
    }
    crc
}

// ---------------------------------------------------------------- 软件回退

const POLY: u32 = 0x82F6_3B78; // Castagnoli，位反转形式

fn table() -> &'static [u32; 256] {
    use std::sync::OnceLock;
    static T: OnceLock<[u32; 256]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            }
            *e = c;
        }
        t
    })
}

fn update_soft(mut crc: u32, data: &[u8]) -> u32 {
    let t = table();
    for &b in data {
        crc = t[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CRC32C 的标准测试向量
    #[test]
    fn 标准向量() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
    }

    #[test]
    fn 软硬件实现一致() {
        let data: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        // 覆盖 8 字节主循环之外的各种尾巴长度
        for len in [0, 1, 7, 8, 9, 15, 16, 63, 100, 1023, 4096] {
            let soft = !update_soft(!0, &data[..len]);
            let disp = crc32c(&data[..len]);
            assert_eq!(soft, disp, "长度 {len} 上软硬件结果不一致");
        }
    }

    #[test]
    fn 分段更新等于一次更新() {
        let data: Vec<u8> = (0..10000u32).map(|i| i as u8).collect();
        let one = crc32c(&data);
        let mut c = Crc32c::new();
        for chunk in data.chunks(777) {
            c.update(chunk);
        }
        assert_eq!(c.finish(), one);
    }
}
