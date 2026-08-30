use ibsend::crc::crc32c;
use std::time::Instant;
fn main() {
    let data: Vec<u8> = (0..64u32 << 20).map(|i| i as u8).collect();
    for _ in 0..3 {
        let t = Instant::now();
        let c = crc32c(&data);
        let s = t.elapsed().as_secs_f64();
        println!("  CRC32C {:.2} GB/s  (校验值 {:#010x})", data.len() as f64 / s / 1e9, c);
    }
    println!("  sse4.2 探测结果: {}", is_x86_feature_detected!("sse4.2"));
}
