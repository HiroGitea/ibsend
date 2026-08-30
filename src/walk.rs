//! 把用户拖进来的东西展开成待传文件列表。
//!
//! 文件直接用文件名；目录递归展开，名字带上相对路径（拖 `~/photos` 得到
//! `photos/a.jpg`、`photos/2024/b.jpg`），接收端据此重建目录结构。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 一个待传文件：本地路径 + 告诉对端的名字（可能含相对路径）
#[derive(Debug, Clone)]
pub struct Item {
    pub path: PathBuf,
    pub name: String,
}

/// 展开时跳过的东西，用来给用户一个交代
#[derive(Debug, Clone, Default)]
pub struct Skipped {
    pub symlinks: usize,
    pub specials: usize,
    pub unreadable: usize,
}

impl Skipped {
    pub fn is_empty(&self) -> bool {
        self.symlinks == 0 && self.specials == 0 && self.unreadable == 0
    }
    pub fn describe(&self) -> String {
        let mut v = Vec::new();
        if self.symlinks > 0 { v.push(format!("{} 个符号链接", self.symlinks)); }
        if self.specials > 0 { v.push(format!("{} 个特殊文件", self.specials)); }
        if self.unreadable > 0 { v.push(format!("{} 个读不了的条目", self.unreadable)); }
        v.join("，")
    }
}

const MAX_FILES: usize = 200_000;
const MAX_DEPTH: usize = 64;

/// 展开一批路径。目录会被递归展开。
///
/// 符号链接一律跳过而不是跟随：跟随会带来环、会把目录外的东西悄悄拉进来，
/// 也会让「传了什么」变得不可预测。
pub fn expand(inputs: &[PathBuf]) -> io::Result<(Vec<Item>, Skipped)> {
    let mut out = Vec::new();
    let mut skip = Skipped::default();
    for p in inputs {
        let md = fs::symlink_metadata(p)?;
        let ft = md.file_type();
        if ft.is_symlink() {
            skip.symlinks += 1;
        } else if ft.is_file() {
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unnamed".into());
            out.push(Item { path: p.clone(), name });
        } else if ft.is_dir() {
            let root = p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "dir".into());
            walk(p, &root, 0, &mut out, &mut skip)?;
        } else {
            skip.specials += 1;
        }
    }
    Ok((out, skip))
}

fn walk(
    dir: &Path,
    prefix: &str,
    depth: usize,
    out: &mut Vec<Item>,
    skip: &mut Skipped,
) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(io::Error::other(format!("目录层级超过 {MAX_DEPTH}，疑似有环")));
    }
    // 排序只为可重复：同一个目录两次展开得到同样的顺序，续传和排查都省心
    let mut names: Vec<_> = fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .collect();
    names.sort();

    for n in names {
        if out.len() >= MAX_FILES {
            return Err(io::Error::other(format!("文件数超过 {MAX_FILES}，先打包成 tar 吧")));
        }
        let child = dir.join(&n);
        let md = match fs::symlink_metadata(&child) {
            Ok(m) => m,
            Err(_) => {
                skip.unreadable += 1;
                continue;
            }
        };
        let rel = format!("{prefix}/{}", n.to_string_lossy());
        let ft = md.file_type();
        if ft.is_symlink() {
            skip.symlinks += 1;
        } else if ft.is_file() {
            out.push(Item { path: child, name: rel });
        } else if ft.is_dir() {
            walk(&child, &rel, depth + 1, out, skip)?;
        } else {
            skip.specials += 1;
        }
    }
    Ok(())
}

/// 把对端给的名字清洗成一条安全的相对路径。
///
/// 允许子目录（这样才能重建目录结构），但拒绝一切能跑到目标目录之外的形式：
/// 绝对路径、`..`、盘符前缀、空组件。名字来自网络，这里是唯一的关卡。
pub fn safe_relpath(name: &str) -> Option<PathBuf> {
    use std::path::Component;
    let p = Path::new(name);
    if p.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(s) => {
                if s.is_empty() {
                    return None;
                }
                out.push(s);
            }
            // ParentDir / RootDir / Prefix / CurDir 一律拒绝
            _ => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 路径穿越一律拒绝() {
        for bad in [
            "/etc/passwd",
            "../x",
            "a/../../x",
            "..",
            ".",
            "",
            "a/../..",
            "/",
        ] {
            assert!(safe_relpath(bad).is_none(), "{bad:?} 应当被拒绝");
        }
    }

    #[test]
    fn 正常的相对路径保留结构() {
        assert_eq!(safe_relpath("a.bin"), Some(PathBuf::from("a.bin")));
        assert_eq!(safe_relpath("photos/2024/a.jpg"), Some(PathBuf::from("photos/2024/a.jpg")));
        // 中间的 "./" 会被 Components 归一化掉，不算穿越
        assert_eq!(safe_relpath("photos/./a.jpg"), Some(PathBuf::from("photos/a.jpg")));
        assert_eq!(safe_relpath("中文 目录/文件.bin"), Some(PathBuf::from("中文 目录/文件.bin")));
    }

    #[test]
    fn 展开目录带上相对路径() {
        let dir = std::env::temp_dir().join(format!("ibsend-walk-{}", std::process::id()));
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(dir.join("a.bin"), b"a").unwrap();
        fs::write(sub.join("b.bin"), b"b").unwrap();

        let (items, skip) = expand(&[dir.clone()]).unwrap();
        let mut names: Vec<_> = items.iter().map(|i| i.name.clone()).collect();
        names.sort();
        let base = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(names, vec![format!("{base}/a.bin"), format!("{base}/sub/b.bin")]);
        assert!(skip.is_empty());
        fs::remove_dir_all(&dir).ok();
    }
}
