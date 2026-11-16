//! 观测式效果：shell / 插件 / MCP 这类"信任边界"通道，执行前后各扫一次工作区，
//! 把**实际变了什么**记进日志。
//!
//! 为什么需要它：效果集是"声明"的——shell 只声明 `proc.exec <命令>`，命令内部
//! 读了什么、写了什么，声明里没有。审计要回答"到底改了什么"，只能靠观测。
//!
//! 代价控制：
//!   - 只扫元数据（大小 + mtime），不读内容、不算哈希；
//!   - 跳过 .git / target / node_modules 等目录；
//!   - 文件数超过上限就停下并标记 truncated，绝不为了"完整"把任务拖死。

use crate::json::Json;
use std::collections::BTreeMap;
use std::path::Path;

/// 扫描时跳过的目录名（任何层级命中都跳过）。
pub const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".ui",
    "experiments",
    "dist",
    "out",
    ".venv",
    "__pycache__",
];

/// 单次扫描最多看多少个文件。超过就截断并如实标记。
pub const MAX_FILES: usize = 20_000;

#[derive(Debug, Clone, Default)]
pub struct FsState {
    /// 相对路径 → (字节数, 修改时间毫秒)
    pub entries: BTreeMap<String, (u64, u64)>,
    pub truncated: bool,
}

#[derive(Debug, Default, Clone)]
pub struct Changes {
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
    pub scanned: usize,
    pub truncated: bool,
}

impl Changes {
    pub fn total(&self) -> usize {
        self.added.len() + self.changed.len() + self.removed.len()
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    pub fn to_json(&self) -> Json {
        let arr = |v: &Vec<String>| Json::Arr(v.iter().map(|s| Json::from(s.as_str())).collect());
        Json::obj(vec![
            ("added", arr(&self.added)),
            ("changed", arr(&self.changed)),
            ("removed", arr(&self.removed)),
            ("scanned", Json::from(self.scanned)),
            ("truncated", Json::Bool(self.truncated)),
        ])
    }

    /// 给人看的一行摘要，例如 `+2 ~1 -0`。
    pub fn summary(&self) -> String {
        format!(
            "+{} ~{} -{}",
            self.added.len(),
            self.changed.len(),
            self.removed.len()
        )
    }
}

pub fn scan(root: &Path) -> FsState {
    let mut st = FsState::default();
    let mut count = 0usize;
    walk(root, root, &mut st, &mut count);
    st
}

fn walk(root: &Path, dir: &Path, st: &mut FsState, count: &mut usize) {
    if st.truncated {
        return;
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        if st.truncated {
            return;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(root, &path, st, count);
            continue;
        }
        if *count >= MAX_FILES {
            st.truncated = true;
            return;
        }
        *count += 1;
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        st.entries.insert(rel, (meta.len(), mtime));
    }
}

pub fn diff(before: &FsState, after: &FsState) -> Changes {
    let mut ch = Changes {
        scanned: after.entries.len(),
        truncated: before.truncated || after.truncated,
        ..Default::default()
    };
    for (path, (size, mtime)) in &after.entries {
        match before.entries.get(path) {
            None => ch.added.push(path.clone()),
            Some((bsize, bmtime)) => {
                if bsize != size || bmtime != mtime {
                    ch.changed.push(path.clone());
                }
            }
        }
    }
    for path in before.entries.keys() {
        if !after.entries.contains_key(path) {
            ch.removed.push(path.clone());
        }
    }
    ch.added.sort();
    ch.changed.sort();
    ch.removed.sort();
    // 截断时列表可能不完整，但"变了"这件事仍然成立——调用方会看到 truncated=true
    ch
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ui-observe-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn detects_add_change_remove_and_skips_heavy_dirs() {
        let d = tmp("basic");
        std::fs::write(d.join("a.txt"), "one").unwrap();
        std::fs::write(d.join("b.txt"), "two").unwrap();

        let before = scan(&d);
        assert_eq!(before.entries.len(), 2);

        std::fs::write(d.join("a.txt"), "one-longer").unwrap();
        std::fs::remove_file(d.join("b.txt")).unwrap();
        std::fs::write(d.join("c.txt"), "new").unwrap();
        // 这些目录必须被跳过，否则大仓库里扫一次要好几秒
        std::fs::create_dir_all(d.join("target").join("deep")).unwrap();
        std::fs::write(d.join("target").join("deep").join("x.o"), "junk").unwrap();

        let after = scan(&d);
        let ch = diff(&before, &after);
        assert_eq!(ch.added, vec!["c.txt".to_string()]);
        assert_eq!(ch.changed, vec!["a.txt".to_string()]);
        assert_eq!(ch.removed, vec!["b.txt".to_string()]);
        assert!(!ch.truncated);
        assert!(!after.entries.keys().any(|k| k.starts_with("target/")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn no_change_means_empty_diff() {
        let d = tmp("same");
        std::fs::write(d.join("a.txt"), "x").unwrap();
        let before = scan(&d);
        let after = scan(&d);
        assert!(diff(&before, &after).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
