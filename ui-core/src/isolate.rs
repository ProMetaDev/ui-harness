//! 隔离子代理的副本比对。
//!
//! 子代理在副本里干活，结束时我们要知道**它改了哪些文件**；`ui apply` 再把这些文件
//! 合回父工作区。判断"改没改"用的是内容哈希，而不是逐字节比较：
//!
//!   - 逐字节比较要把副本和父工作区各读一遍（两份数据）；
//!   - 早期的实现为省时间对 > 2 MB 的文件直接当成"没改"——那是个**盲区**：
//!     子代理改大数据集或模型权重会被静默丢弃。
//!
//! 这里用 xxHash64（见 `fasthash`）：只读副本一遍，且没有大小限制。

use crate::fasthash::Xxh64;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

/// 与 manifest 里同样的算法与格式（`xxh:<16 hex>`）。
pub fn hash_file(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut hasher = Xxh64::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(format!("xxh:{:016x}", hasher.finish()))
}

/// 遍历副本，返回相对路径列表：内容与 manifest 不一致、或 manifest 里没有的。
///
/// `manifest` 的值必须是 `hash_file` 的输出格式。旧格式（纯 SHA-256）会被当成
/// "不一致"——宁可多问一句，也不漏掉一次改动。
pub fn changed_files(root: &Path, manifest: &HashMap<String, String>) -> Vec<String> {
    let mut out = Vec::new();
    walk(root, root, manifest, &mut out);
    out.sort();
    out.dedup();
    out
}

fn walk(root: &Path, dir: &Path, manifest: &HashMap<String, String>, out: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if crate::observe::SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, manifest, out);
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        let key = rel.to_string_lossy().replace('\\', "/");
        let unchanged = match (manifest.get(&key), hash_file(&path)) {
            (Some(expected), Some(now)) => expected == &now,
            _ => false,
        };
        if !unchanged {
            out.push(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ui-isolate-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn manifest_of(root: &Path, rel: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert(rel.to_string(), hash_file(&root.join(rel)).unwrap());
        m
    }

    #[test]
    fn unchanged_files_are_not_reported() {
        let d = tmp("same");
        fs::write(d.join("a.txt"), "hello").unwrap();
        let m = manifest_of(&d, "a.txt");
        assert!(changed_files(&d, &m).is_empty());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_small_change_is_detected() {
        let d = tmp("small");
        fs::write(d.join("a.txt"), "hello").unwrap();
        let m = manifest_of(&d, "a.txt");
        fs::write(d.join("a.txt"), "hellp").unwrap();
        assert_eq!(changed_files(&d, &m), vec!["a.txt".to_string()]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_change_in_a_big_file_is_detected() {
        // 回归：旧实现跳过 > 2 MB 的文件，子代理改了也会被当成"没改"
        let d = tmp("big");
        let big = d.join("big.bin");
        fs::write(&big, vec![0u8; 3 * 1024 * 1024]).unwrap();
        let m = manifest_of(&d, "big.bin");
        assert!(changed_files(&d, &m).is_empty());

        let mut data = fs::read(&big).unwrap();
        data.extend_from_slice(b"appended");
        fs::write(&big, &data).unwrap();

        assert_eq!(changed_files(&d, &m), vec!["big.bin".to_string()]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn new_files_and_unknown_manifest_entries_count_as_changed() {
        let d = tmp("new");
        fs::write(d.join("a.txt"), "a").unwrap();
        let m = manifest_of(&d, "a.txt");
        fs::write(d.join("b.txt"), "b").unwrap(); // 子代理新建的
        let changed = changed_files(&d, &m);
        assert!(changed.contains(&"b.txt".to_string()), "{:?}", changed);

        // 旧格式（SHA-256）的 manifest 值不会被误判为"相同"
        let mut legacy = HashMap::new();
        legacy.insert("a.txt".to_string(), "0".repeat(64));
        assert!(changed_files(&d, &legacy).contains(&"a.txt".to_string()));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn skipped_directories_are_ignored() {
        let d = tmp("skip");
        fs::create_dir_all(d.join("target")).unwrap();
        fs::write(d.join("target/x.o"), "junk").unwrap();
        assert!(changed_files(&d, &HashMap::new()).is_empty());
        let _ = fs::remove_dir_all(&d);
    }
}
