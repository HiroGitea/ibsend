fn main() {
    cc::Build::new()
        .file("csrc/ibx.c")
        .flag("-pthread")
        // 必须钉住 C11：glibc 2.38 起在 C23 模式下会把 atoi/strtol 重定向到
        // __isoc23_* 符号，而 GCC 15 默认就是 gnu23。在滚动发行版上编译会
        // 静默产出跑不了 Debian 12（glibc 2.36）的二进制。
        .flag("-std=gnu11")
        .opt_level(2)
        .warnings(true)
        .compile("ibx");
    println!("cargo:rustc-link-lib=ibverbs");
    println!("cargo:rustc-link-lib=rdmacm");
    println!("cargo:rerun-if-changed=csrc/ibx.c");
    println!("cargo:rerun-if-changed=csrc/ibx.h");
}
