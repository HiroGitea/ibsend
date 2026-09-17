//! 机器可读输出：每行一个 JSON 对象（NDJSON）。
//!
//! 命令行的 `--json` 靠它把事件交给脚本和编排系统。只有扁平对象这一种形状，
//! 为这点东西引入 serde 不划算。字段名和事件名是对外的契约，改动要当成破坏
//! 兼容处理。

use std::fmt::Write;

/// 一个正在拼的 JSON 对象，`event` 字段总是第一个
pub struct Obj(String);

impl Obj {
    pub fn event(name: &str) -> Obj {
        Obj(String::from("{")).str("event", name)
    }

    fn key(&mut self, k: &str) {
        if self.0.len() > 1 {
            self.0.push(',');
        }
        quote(&mut self.0, k);
        self.0.push(':');
    }

    pub fn str(mut self, k: &str, v: &str) -> Obj {
        self.key(k);
        quote(&mut self.0, v);
        self
    }

    pub fn uint(mut self, k: &str, v: u64) -> Obj {
        self.key(k);
        let _ = write!(self.0, "{v}");
        self
    }

    /// 非有限值（NaN、无穷）JSON 表示不了，记成 0
    pub fn float(mut self, k: &str, v: f64) -> Obj {
        self.key(k);
        let v = if v.is_finite() { v } else { 0.0 };
        let _ = write!(self.0, "{v:.3}");
        self
    }

    pub fn bool(mut self, k: &str, v: bool) -> Obj {
        self.key(k);
        self.0.push_str(if v { "true" } else { "false" });
        self
    }

    pub fn strs(mut self, k: &str, v: &[String]) -> Obj {
        self.key(k);
        self.0.push('[');
        for (i, s) in v.iter().enumerate() {
            if i > 0 {
                self.0.push(',');
            }
            quote(&mut self.0, s);
        }
        self.0.push(']');
        self
    }

    /// 收尾，得到一行（不含换行符）
    pub fn finish(mut self) -> String {
        self.0.push('}');
        self.0
    }
}

fn quote(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 拼出扁平对象() {
        let s = Obj::event("done")
            .uint("bytes", 42)
            .float("rate", 1.5)
            .bool("local", true)
            .strs("bad", &["a".into(), "b".into()])
            .finish();
        assert_eq!(s, r#"{"event":"done","bytes":42,"rate":1.500,"local":true,"bad":["a","b"]}"#);
        assert_eq!(Obj::event("x").strs("v", &[]).finish(), r#"{"event":"x","v":[]}"#);
    }

    #[test]
    fn 转义() {
        let s = Obj::event("e").str("m", "引号\" 反斜杠\\ 换行\n 控制\u{1}").finish();
        assert_eq!(s, r#"{"event":"e","m":"引号\" 反斜杠\\ 换行\n 控制\u0001"}"#);
    }

    #[test]
    fn 非有限浮点记成零() {
        assert_eq!(Obj::event("p").float("rate", f64::NAN).finish(), r#"{"event":"p","rate":0.000}"#);
        assert_eq!(Obj::event("p").float("r", f64::INFINITY).finish(), r#"{"event":"p","r":0.000}"#);
    }
}
