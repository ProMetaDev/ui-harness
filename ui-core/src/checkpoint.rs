//! 零依赖 workspace checkpoint / diff / restore。
//!
//! 目标不是替代 git，而是给 Agent 的“这一轮改了什么”提供一个可审阅、可回滚
//! 的本地边界：
//!   - checkpoint 把工作区文件内容做内容寻址存储；
//!   - diff 对比当前工作区与某个 checkpoint；
//!   - restore 从 checkpoint 写回，写回前自动再存一个 pre-restore。
//!
//! 不跟随符号链接、不处理 `.git/.ui/target/node_modules`；这些边界写在
//! `EXCLUDED` 里，避免把日志/缓存/大产物打进快照。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::json::Json;
use crate::sha256;
use crate::time;

const EXCLUDED: &[&str] = &[".git", ".ui", "target", "node_modules"];
const CTX: usize = 3;
const MAX_DIFF_LINES: usize = 4000;

fn rel_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, PathBuf>) -> Result<(), String> {
    let rd = fs::read_dir(dir).map_err(|e| format!("读目录 {} 失败: {}", dir.display(), e))?;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if EXCLUDED.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            // 不跟随：checkpoint 不能把工作区外的内容吸进来。
            continue;
        }
        if meta.is_dir() {
            walk(root, &path, out)?;
        } else if meta.is_file() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            out.insert(rel_key(rel), path);
        }
    }
    Ok(())
}

fn scan(root: &Path) -> Result<BTreeMap<String, PathBuf>, String> {
    let mut files = BTreeMap::new();
    walk(root, root, &mut files)?;
    Ok(files)
}

fn checkpoint_root(ws: &Path) -> PathBuf {
    ws.join(".ui").join("checkpoints")
}

fn blob_dir(ws: &Path) -> PathBuf {
    checkpoint_root(ws).join("blobs")
}

fn cp_dir(ws: &Path, id: &str) -> PathBuf {
    checkpoint_root(ws).join(id)
}

fn write_blob(ws: &Path, bytes: &[u8]) -> Result<String, String> {
    let hash = sha256::hex(bytes);
    let dir = blob_dir(ws);
    fs::create_dir_all(&dir).map_err(|e| format!("创建 blob 目录失败: {}", e))?;
    let path = dir.join(&hash);
    if path.exists() {
        return Ok(hash);
    }
    let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), time::now_ms()));
    fs::write(&tmp, bytes).map_err(|e| format!("写 blob 失败: {}", e))?;
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("落盘 blob 失败: {}", e)
    })?;
    Ok(hash)
}

fn read_blob(ws: &Path, hash: &str) -> Result<Vec<u8>, String> {
    let path = blob_dir(ws).join(hash);
    fs::read(&path).map_err(|e| format!("读 blob {} 失败: {}", hash, e))
}

/// 创建一个 checkpoint，返回元信息。
pub fn create(ws: &Path, label: &str) -> Result<Json, String> {
    fs::create_dir_all(checkpoint_root(ws)).map_err(|e| format!("创建 checkpoint 目录失败: {}", e))?;
    let files = scan(ws)?;
    let mut entries = Vec::new();
    for (rel, path) in files {
        let bytes = fs::read(&path).map_err(|e| format!("读 {} 失败: {}", rel, e))?;
        let hash = write_blob(ws, &bytes)?;
        entries.push(Json::obj(vec![
            ("path", Json::from(rel.as_str())),
            ("sha256", Json::from(hash.as_str())),
            ("bytes", Json::from(bytes.len() as u64)),
        ]));
    }
    let file_count = entries.len();
    let body = Json::obj(vec![
        ("label", Json::from(label)),
        ("createdAt", Json::from(time::now_ms())),
        ("workspace", Json::from(ws.to_string_lossy().to_string())),
        ("entries", Json::Arr(entries)),
    ]);
    let digest = sha256::hex(body.to_compact().as_bytes());
    let id = format!("cp-{}-{}", time::now_ms(), &digest[..8]);
    let dir = cp_dir(ws, &id);
    fs::create_dir_all(&dir).map_err(|e| format!("创建 checkpoint 失败: {}", e))?;
    let tmp = dir.join(".manifest.tmp");
    fs::write(&tmp, body.to_pretty()).map_err(|e| format!("写 manifest 失败: {}", e))?;
    fs::rename(&tmp, dir.join("manifest.json")).map_err(|e| format!("落盘 manifest 失败: {}", e))?;
    Ok(Json::obj(vec![
        ("id", Json::from(id.as_str())),
        ("label", Json::from(label)),
        ("files", Json::from(file_count as u64)),
    ]))
}

fn load_manifest(ws: &Path, id: &str) -> Result<Json, String> {
    let path = cp_dir(ws, id).join("manifest.json");
    let text = fs::read_to_string(&path).map_err(|e| format!("读 checkpoint {} 失败: {}", id, e))?;
    crate::json::parse(&text).map_err(|e| format!("manifest 不是 JSON: {}", e))
}

pub fn list(ws: &Path) -> Result<Vec<Json>, String> {
    let dir = checkpoint_root(ws);
    let rd = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("cp-") {
            continue;
        }
        if let Ok(m) = load_manifest(ws, &name) {
            out.push(Json::obj(vec![
                ("id", Json::from(name.as_str())),
                ("label", Json::from(m.str_at("label").unwrap_or(""))),
                ("createdAt", Json::from(m.u64_at("createdAt").unwrap_or(0))),
                (
                    "files",
                    Json::from(m.get("entries").and_then(|e| e.as_arr()).map(|a| a.len()).unwrap_or(0) as u64),
                ),
            ]));
        }
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.u64_at("createdAt").unwrap_or(0)));
    Ok(out)
}

pub fn latest_id(ws: &Path) -> Result<String, String> {
    list(ws)?
        .first()
        .and_then(|e| e.str_at("id").map(str::to_string))
        .ok_or_else(|| "还没有 checkpoint；先运行 `ui checkpoint`".to_string())
}

pub struct Change {
    pub path: String,
    pub kind: String,
    pub old: Option<Vec<u8>>,
    pub new: Option<Vec<u8>>,
    pub patch: String,
}

pub struct DiffReport {
    pub checkpoint: String,
    pub changes: Vec<Change>,
}

impl DiffReport {
    pub fn to_json(&self) -> Json {
        Json::obj(vec![
            ("checkpoint", Json::from(self.checkpoint.as_str())),
            (
                "changes",
                Json::Arr(
                    self.changes
                        .iter()
                        .map(|c| {
                            Json::obj(vec![
                                ("path", Json::from(c.path.as_str())),
                                ("kind", Json::from(c.kind.as_str())),
                                ("patch", Json::from(c.patch.as_str())),
                                (
                                    "oldBytes",
                                    Json::from(c.old.as_ref().map(|b| b.len()).unwrap_or(0) as u64),
                                ),
                                (
                                    "newBytes",
                                    Json::from(c.new.as_ref().map(|b| b.len()).unwrap_or(0) as u64),
                                ),
                            ])
                        })
                        .collect(),
                ),
            ),
        ])
    }

    pub fn render(&self) -> String {
        if self.changes.is_empty() {
            return format!("checkpoint {}: 工作区无变化\n", self.checkpoint);
        }
        let mut out = String::new();
        out.push_str(&format!("checkpoint {}\n", self.checkpoint));
        for c in &self.changes {
            out.push_str(&format!("\n### {} [{}]\n", c.path, c.kind));
            out.push_str(&c.patch);
            if !c.patch.ends_with('\n') {
                out.push('\n');
            }
        }
        out
    }
}

fn is_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

fn lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .split_inclusive('\n')
        .map(str::to_string)
        .collect()
}

fn lcs_ops(old: &[String], new: &[String]) -> Vec<(char, String, String)> {
    let n = old.len();
    let m = new.len();
    if n + m > MAX_DIFF_LINES {
        return Vec::new();
    }
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if old[i] == new[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < n && j < m {
        if old[i] == new[j] {
            out.push((' ', old[i].clone(), new[j].clone()));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push(('-', old[i].clone(), String::new()));
            i += 1;
        } else {
            out.push(('+', String::new(), new[j].clone()));
            j += 1;
        }
    }
    while i < n {
        out.push(('-', old[i].clone(), String::new()));
        i += 1;
    }
    while j < m {
        out.push(('+', String::new(), new[j].clone()));
        j += 1;
    }
    out
}

fn patch_text(path: &str, old: &[u8], new: &[u8]) -> String {
    if !is_text(old) || !is_text(new) {
        return format!("Binary files a/{} and b/{} differ\n", path, path);
    }
    let old_lines = lines(old);
    let new_lines = lines(new);
    let ops = lcs_ops(&old_lines, &new_lines);
    if ops.is_empty() {
        return format!(
            "（文件较大，未生成逐行 diff；a/{} {} bytes → b/{} {} bytes）\n",
            path,
            old.len(),
            path,
            new.len()
        );
    }
    // 找到变更区间，按 CTX 分组为 hunk。
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, (tag, _, _))| *tag != ' ')
        .map(|(i, _)| i)
        .collect();
    if changed.is_empty() {
        return String::new();
    }
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    let mut start = changed[0].saturating_sub(CTX);
    let mut end = (changed[0] + CTX + 1).min(ops.len());
    for &i in &changed[1..] {
        let s = i.saturating_sub(CTX);
        let e = (i + CTX + 1).min(ops.len());
        if s <= end {
            end = end.max(e);
        } else {
            hunks.push((start, end));
            start = s;
            end = e;
        }
    }
    hunks.push((start, end));

    let mut out = format!("--- a/{}\n+++ b/{}\n", path, path);
    for (hs, he) in hunks {
        let mut old_start = 0usize;
        let mut new_start = 0usize;
        for (tag, _, _) in &ops[..hs] {
            match tag {
                ' ' => {
                    old_start += 1;
                    new_start += 1;
                }
                '-' => old_start += 1,
                '+' => new_start += 1,
                _ => {}
            }
        }
        let old_count = ops[hs..he].iter().filter(|(t, _, _)| *t != '+').count();
        let new_count = ops[hs..he].iter().filter(|(t, _, _)| *t != '-').count();
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            old_start + 1,
            old_count,
            new_start + 1,
            new_count
        ));
        for (tag, o, n) in &ops[hs..he] {
            let line = if *tag == '-' { o } else { n };
            out.push(*tag);
            out.push_str(line);
            if !line.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    out
}

pub fn diff(ws: &Path, id: Option<&str>) -> Result<DiffReport, String> {
    let checkpoint = match id {
        Some(id) => id.to_string(),
        None => latest_id(ws)?,
    };
    let manifest = load_manifest(ws, &checkpoint)?;
    let mut baseline: BTreeMap<String, (String, u64)> = BTreeMap::new();
    for e in manifest
        .get("entries")
        .and_then(|v| v.as_arr())
        .cloned()
        .unwrap_or_default()
    {
        if let (Some(path), Some(hash)) = (e.str_at("path"), e.str_at("sha256")) {
            baseline.insert(path.to_string(), (hash.to_string(), e.u64_at("bytes").unwrap_or(0)));
        }
    }
    let current = scan(ws)?;
    let mut keys: Vec<String> = baseline.keys().cloned().collect();
    for k in current.keys() {
        if !baseline.contains_key(k) {
            keys.push(k.clone());
        }
    }
    keys.sort();

    let mut changes = Vec::new();
    for key in keys {
        let old = baseline.get(&key).map(|(h, _)| read_blob(ws, h)).transpose()?;
        let new = current
            .get(&key)
            .map(|p| fs::read(p).map_err(|e| format!("读 {} 失败: {}", key, e)))
            .transpose()?;
        match (&old, &new) {
            (Some(o), Some(n)) if o == n => {}
            (None, Some(n)) => changes.push(Change {
                path: key.clone(),
                kind: "added".into(),
                old: None,
                new: Some(n.clone()),
                patch: patch_text(&key, b"", n),
            }),
            (Some(o), None) => changes.push(Change {
                path: key.clone(),
                kind: "deleted".into(),
                old: Some(o.clone()),
                new: None,
                patch: patch_text(&key, o, b""),
            }),
            (Some(o), Some(n)) => changes.push(Change {
                path: key.clone(),
                kind: "modified".into(),
                old: Some(o.clone()),
                new: Some(n.clone()),
                patch: patch_text(&key, o, n),
            }),
            (None, None) => {}
        }
    }
    Ok(DiffReport { checkpoint, changes })
}

fn ensure_restore_target(ws: &Path, target: &Path) -> Result<(), String> {
    let ws_real = fs::canonicalize(ws).unwrap_or_else(|_| ws.to_path_buf());
    let mut probe = target.parent();
    while let Some(parent) = probe {
        if parent.as_os_str().is_empty() {
            break;
        }
        if let Ok(real) = fs::canonicalize(parent) {
            if !real.starts_with(&ws_real) {
                return Err(format!(
                    "restore 目标父目录越出 workspace: {}",
                    target.display()
                ));
            }
            break;
        }
        probe = parent.parent();
    }
    if let Ok(meta) = fs::symlink_metadata(target) {
        if meta.file_type().is_symlink() {
            return Err(format!("restore 拒绝写符号链接: {}", target.display()));
        }
    }
    Ok(())
}

pub fn restore(ws: &Path, id: &str) -> Result<Json, String> {
    let manifest = load_manifest(ws, id)?;
    // 先给当前工作区留一个可回滚点；restore 本身也可能写错。
    let backup = create(ws, &format!("pre-restore-{}", id))?;
    let entries = manifest
        .get("entries")
        .and_then(|v| v.as_arr())
        .cloned()
        .unwrap_or_default();
    let mut restored = 0u64;
    for e in entries {
        let rel = match e.str_at("path") {
            Some(p) => p,
            None => continue,
        };
        let rel_path = Path::new(rel);
        if rel_path.is_absolute() || rel.split('/').any(|s| s == "..") {
            return Err(format!("manifest 路径越界: {}", rel));
        }
        let hash = e.str_at("sha256").unwrap_or("");
        let bytes = read_blob(ws, hash)?;
        let target = ws.join(rel_path);
        ensure_restore_target(ws, &target)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {}", e))?;
        }
        let tmp = target.with_extension(format!("ui-restore-{}", std::process::id()));
        fs::write(&tmp, &bytes).map_err(|e| format!("写回 {} 失败: {}", rel, e))?;
        fs::rename(&tmp, &target).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("落盘 {} 失败: {}", rel, e)
        })?;
        restored += 1;
    }
    Ok(Json::obj(vec![
        ("restored", Json::from(restored)),
        ("checkpoint", Json::from(id)),
        ("backup", backup.get("id").cloned().unwrap_or(Json::Null)),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ui-checkpoint-{}-{}",
            std::process::id(),
            name
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn checkpoint_diff_restore_round_trip() {
        let ws = scratch("roundtrip");
        fs::write(ws.join("a.txt"), "one\ntwo\n").unwrap();
        fs::create_dir_all(ws.join("src")).unwrap();
        fs::write(ws.join("src/b.txt"), "hello\n").unwrap();
        let cp = create(&ws, "before").unwrap();
        let id = cp.str_at("id").unwrap().to_string();

        fs::write(ws.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        fs::write(ws.join("src/new.txt"), "new\n").unwrap();
        fs::remove_file(ws.join("src/b.txt")).unwrap();

        let report = diff(&ws, Some(&id)).unwrap();
        let kinds: Vec<&str> = report.changes.iter().map(|c| c.kind.as_str()).collect();
        assert!(kinds.contains(&"modified"));
        assert!(kinds.contains(&"added"));
        assert!(kinds.contains(&"deleted"));
        assert!(report.render().contains("@@"));
        assert!(report.to_json().str_at("checkpoint") == Some(id.as_str()));

        let out = restore(&ws, &id).unwrap();
        assert_eq!(out.u64_at("restored"), Some(2));
        assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "one\ntwo\n");
        assert_eq!(fs::read_to_string(ws.join("src/b.txt")).unwrap(), "hello\n");
        // restore 不应删除 checkpoint 后新增的文件（非 prune 语义）。
        assert!(ws.join("src/new.txt").exists());
        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn restore_refuses_to_write_through_a_symlink() {
        #[cfg(unix)]
        {
            let ws = scratch("restore-symlink");
            fs::write(ws.join("a.txt"), "inside\n").unwrap();
            let cp = create(&ws, "before").unwrap();
            let id = cp.str_at("id").unwrap();
            fs::remove_file(ws.join("a.txt")).unwrap();
            let outside = std::env::temp_dir().join(format!(
                "ui-checkpoint-outside-{}",
                std::process::id()
            ));
            fs::write(&outside, "outside\n").unwrap();
            std::os::unix::fs::symlink(&outside, ws.join("a.txt")).unwrap();
            let err = restore(&ws, id).unwrap_err();
            assert!(err.contains("符号链接") || err.contains("越出"), "{}", err);
            let _ = fs::remove_file(&outside);
            let _ = fs::remove_dir_all(&ws);
        }
    }

    #[test]
    fn symlinks_are_not_followed() {
        #[cfg(unix)]
        {
            let ws = scratch("symlink");
            fs::write(ws.join("real.txt"), "inside\n").unwrap();
            std::os::unix::fs::symlink("real.txt", ws.join("link.txt")).unwrap();
            let cp = create(&ws, "link").unwrap();
            let id = cp.str_at("id").unwrap();
            let report = diff(&ws, Some(id)).unwrap();
            assert!(report.changes.iter().all(|c| c.path != "link.txt"));
            let _ = fs::remove_dir_all(&ws);
        }
    }
}
