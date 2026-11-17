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

/// 副本里**不复制、也不扫描**的目录名。
///
/// 必须是唯一来源：复制侧（`copy_tree`）和比对侧（`changed_files`）用同一份，
/// 否则会出现"复制进去了但永远扫不到"的静默丢失。
/// 特别注意 `secrets`：它不进副本，子代理在副本里新建的 `secrets/*` 也就不该被合并回去。
pub const SKIP: &[&str] = &[".ui", "out", "target", "node_modules", ".git", "secrets"];

/// manifest 的键格式：统一用 `/` 作分隔符。
///
/// 只在 Windows 上转换——Linux 的合法文件名可以包含字面 `\`，
/// 无条件替换会把 `a\b` 错当成目录 `a/b`。
#[cfg(windows)]
pub fn rel_key(rel: &str) -> String {
    rel.replace('\\', "/")
}

#[cfg(not(windows))]
pub fn rel_key(rel: &str) -> String {
    rel.to_string()
}

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

/// 副本与 manifest 的差异。
///
/// `deleted` 不能省：子代理删掉一个文件，如果只报"改动"，调用方就永远不知道
/// 需要处理删除——父工作区会留下一个本该消失的文件，而日志里毫无痕迹。
pub struct Diff {
    /// 内容变了，或 manifest 里没有的新文件
    pub changed: Vec<String>,
    /// manifest 里有、副本里没有的（被删掉了）
    pub deleted: Vec<String>,
}

/// 遍历副本，算出改动与删除。
///
/// `manifest` 的值必须是 `hash_file` 的输出格式。旧格式（纯 SHA-256）会被当成
/// "不一致"——宁可多问一句，也不漏掉一次改动。
pub fn diff(root: &Path, manifest: &HashMap<String, String>) -> Diff {
    let mut changed = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // 只遍历一遍树：同时收集"改了的"和"存在的所有键"
    walk(root, root, manifest, &mut changed, &mut seen);
    changed.sort();
    changed.dedup();

    let mut deleted: Vec<String> = manifest
        .keys()
        .filter(|k| !seen.contains(*k))
        .cloned()
        .collect();
    deleted.sort();

    Diff { changed, deleted }
}

/// 只取改动列表（大多数调用方只关心这个）。
pub fn changed_files(root: &Path, manifest: &HashMap<String, String>) -> Vec<String> {
    diff(root, manifest).changed
}

fn walk(
    root: &Path,
    dir: &Path,
    manifest: &HashMap<String, String>,
    changed: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if SKIP.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, manifest, changed, seen);
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        let key = rel_key(&rel.to_string_lossy());
        let unchanged = match (manifest.get(&key), hash_file(&path)) {
            (Some(expected), Some(now)) => expected == &now,
            _ => false,
        };
        seen.insert(key.clone());
        if !unchanged {
            changed.push(key);
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

    #[test]
    fn experiments_changes_are_detected() {
        // 回归（对抗复查抓到）：比对侧曾用 observe::SKIP_DIRS，
        // 它把 experiments/ 排除在外，但复制侧会把它复制进去——
        // 于是子代理在默认实验包目录里的改动被静默丢弃。
        let d = tmp("experiments");
        fs::create_dir_all(d.join("experiments")).unwrap();
        let f = d.join("experiments/e.json");
        fs::write(&f, "{\"v\":1}").unwrap();
        let m = manifest_of(&d, "experiments/e.json");
        assert!(changed_files(&d, &m).is_empty(), "没改不该报");

        fs::write(&f, "{\"v\":2}").unwrap();
        assert_eq!(changed_files(&d, &m), vec!["experiments/e.json".to_string()]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn secrets_are_never_tracked() {
        // 反向：secrets 不在副本里，子代理在副本里新建的 secrets/* 也不该被合并回去
        let d = tmp("secrets");
        fs::create_dir_all(d.join("secrets")).unwrap();
        fs::write(d.join("secrets/token.txt"), "leak").unwrap();
        assert!(changed_files(&d, &HashMap::new()).is_empty());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn nested_paths_round_trip_through_the_manifest() {
        // 回归（Windows）：写入侧曾用原始路径、读取侧把 `\` 换成 `/`，
        // 子目录文件全部 miss → apply 判漂移 → 改动不回写。
        let d = tmp("nested");
        fs::create_dir_all(d.join("src/pay")).unwrap();
        let f = d.join("src/pay/a.py");
        fs::write(&f, "one").unwrap();

        let key = rel_key("src/pay/a.py");
        let mut m = HashMap::new();
        m.insert(key.clone(), hash_file(&f).unwrap());
        assert!(changed_files(&d, &m).is_empty(), "键格式不一致会误判为改动");

        fs::write(&f, "two").unwrap();
        assert_eq!(changed_files(&d, &m), vec![key]);

        // Linux 上合法文件名可以包含字面反斜杠：rel_key 不能把它当目录分隔符改写，
        // 否则这个文件永远 manifest miss（只有 Windows 才需要归一化）。
        #[cfg(not(windows))]
        assert_eq!(rel_key(r"a\b"), r"a\b");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn deleted_files_are_reported_as_deleted_not_changed() {
        // 删除如果被忽略，父工作区会永远留下一个本该消失的文件，而日志毫无痕迹。
        let d = tmp("deleted");
        fs::write(d.join("a.txt"), "a").unwrap();
        fs::write(d.join("b.txt"), "b").unwrap();
        let mut m = HashMap::new();
        m.insert("a.txt".to_string(), hash_file(&d.join("a.txt")).unwrap());
        m.insert("b.txt".to_string(), hash_file(&d.join("b.txt")).unwrap());

        fs::remove_file(d.join("b.txt")).unwrap();

        let diff = diff(&d, &m);
        assert_eq!(diff.changed, Vec::<String>::new(), "删除不该算作改动");
        assert_eq!(diff.deleted, vec!["b.txt".to_string()], "删除必须被列出来");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn unchanged_files_are_in_neither_bucket() {
        let d = tmp("neither");
        fs::write(d.join("a.txt"), "a").unwrap();
        let m = manifest_of(&d, "a.txt");
        let diff = diff(&d, &m);
        assert!(diff.changed.is_empty());
        assert!(diff.deleted.is_empty());
        let _ = fs::remove_dir_all(&d);
    }
}
