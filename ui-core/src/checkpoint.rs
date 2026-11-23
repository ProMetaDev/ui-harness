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
            if crate::protected::is_sensitive_file_name(&name)
                || crate::protected::is_private_path(root, &path)
            {
                continue;
            }
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

fn walk_limited(
    root: &Path,
    dir: &Path,
    out: &mut BTreeMap<String, PathBuf>,
    bytes: &mut u64,
    max_files: usize,
    max_bytes: u64,
) -> Result<(), String> {
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
            continue;
        }
        if meta.is_dir() {
            walk_limited(root, &path, out, bytes, max_files, max_bytes)?;
        } else if meta.is_file() {
            if crate::protected::is_sensitive_file_name(&name)
                || crate::protected::is_private_path(root, &path)
            {
                continue;
            }
            let size = meta.len();
            if out.len() + 1 > max_files {
                return Err(format!(
                    "checkpoint 文件数超过上限 {}；可用 UI_AUTO_CHECKPOINT=0 或提高上限",
                    max_files
                ));
            }
            if bytes.saturating_add(size) > max_bytes {
                return Err(format!(
                    "checkpoint 体积超过上限 {} MiB；可用 UI_AUTO_CHECKPOINT=0 或提高上限",
                    max_bytes / 1024 / 1024
                ));
            }
            *bytes = bytes.saturating_add(size);
            let rel = path.strip_prefix(root).unwrap_or(&path);
            out.insert(rel_key(rel), path);
        }
    }
    Ok(())
}

fn scan_limited(
    root: &Path,
    max_files: usize,
    max_bytes: u64,
) -> Result<BTreeMap<String, PathBuf>, String> {
    let mut files = BTreeMap::new();
    let mut bytes = 0u64;
    walk_limited(root, root, &mut files, &mut bytes, max_files, max_bytes)?;
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

/// 自动快照的默认上限：防止第一次写操作把大仓库扫死。
pub const AUTO_MAX_FILES: usize = 20_000;
pub const AUTO_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// 自动 checkpoint 的上限；环境变量可覆盖。
pub fn auto_limits() -> (usize, u64) {
    let files = std::env::var("UI_CHECKPOINT_MAX_FILES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(AUTO_MAX_FILES);
    let bytes = std::env::var("UI_CHECKPOINT_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(AUTO_MAX_BYTES);
    (files, bytes)
}

/// 创建一个 checkpoint，返回元信息。
pub fn create(ws: &Path, label: &str) -> Result<Json, String> {
    create_with(ws, label, scan(ws)?)
}

/// 自动 checkpoint 专用：有文件数/体积上限，超限时不写半份 manifest。
pub fn create_limited(ws: &Path, label: &str) -> Result<Json, String> {
    let (files, bytes) = auto_limits();
    create_limited_with(ws, label, files, bytes)
}

pub fn create_limited_with(
    ws: &Path,
    label: &str,
    max_files: usize,
    max_bytes: u64,
) -> Result<Json, String> {
    create_with(ws, label, scan_limited(ws, max_files, max_bytes)?)
}

fn create_with(
    ws: &Path,
    label: &str,
    files: BTreeMap<String, PathBuf>,
) -> Result<Json, String> {
    fs::create_dir_all(checkpoint_root(ws)).map_err(|e| format!("创建 checkpoint 目录失败: {}", e))?;
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

#[derive(Clone)]
pub struct Hunk {
    pub id: String,
    pub path: String,
    pub index: usize,
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<String>,
}

pub struct Change {
    pub path: String,
    /// rename 的旧路径；其他 kind 为 None。
    pub from: Option<String>,
    pub kind: String,
    pub old: Option<Vec<u8>>,
    pub new: Option<Vec<u8>>,
    pub patch: String,
    pub hunks: Vec<Hunk>,
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
                                ("from", c.from.as_deref().map(Json::from).unwrap_or(Json::Null)),
                                ("kind", Json::from(c.kind.as_str())),
                                ("patch", Json::from(c.patch.as_str())),
                                (
                                    "hunks",
                                    Json::Arr(
                                        c.hunks
                                            .iter()
                                            .map(|h| {
                                                Json::obj(vec![
                                                    ("id", Json::from(h.id.as_str())),
                                                    ("path", Json::from(h.path.as_str())),
                                                    ("index", Json::from(h.index as u64)),
                                                    ("oldStart", Json::from(h.old_start as u64)),
                                                    ("oldCount", Json::from(h.old_count as u64)),
                                                    ("newStart", Json::from(h.new_start as u64)),
                                                    ("newCount", Json::from(h.new_count as u64)),
                                                    (
                                                        "lines",
                                                        Json::Arr(
                                                            h.lines
                                                                .iter()
                                                                .map(|l| Json::from(l.as_str()))
                                                                .collect(),
                                                        ),
                                                    ),
                                                ])
                                            })
                                            .collect(),
                                    ),
                                ),
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
            let from = c
                .from
                .as_deref()
                .map(|p| format!(" from {}", p))
                .unwrap_or_default();
            out.push_str(&format!("\n### {}{} [{}]\n", c.path, from, c.kind));
            out.push_str(&c.patch);
            if !c.patch.ends_with('\n') {
                out.push('\n');
            }
            if !c.hunks.is_empty() {
                let ids: Vec<&str> = c.hunks.iter().map(|h| h.id.as_str()).collect();
                out.push_str(&format!("hunks: {}\n", ids.join(", ")));
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

fn make_hunks(path: &str, old: &[u8], new: &[u8]) -> Vec<Hunk> {
    if !is_text(old) || !is_text(new) {
        return Vec::new();
    }
    let old_lines = lines(old);
    let new_lines = lines(new);
    let ops = lcs_ops(&old_lines, &new_lines);
    if ops.is_empty() {
        return Vec::new();
    }
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, (tag, _, _))| *tag != ' ')
        .map(|(i, _)| i)
        .collect();
    if changed.is_empty() {
        return Vec::new();
    }
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = changed[0].saturating_sub(CTX);
    let mut end = (changed[0] + CTX + 1).min(ops.len());
    for &i in &changed[1..] {
        let s = i.saturating_sub(CTX);
        let e = (i + CTX + 1).min(ops.len());
        if s <= end {
            end = end.max(e);
        } else {
            ranges.push((start, end));
            start = s;
            end = e;
        }
    }
    ranges.push((start, end));

    let mut out = Vec::new();
    for (n, (hs, he)) in ranges.into_iter().enumerate() {
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
        let mut hlines = Vec::new();
        for (tag, o, n) in &ops[hs..he] {
            let line = if *tag == '-' { o } else { n };
            hlines.push(format!("{}{}", tag, line));
        }
        out.push(Hunk {
            id: format!("{}#{}", path, n + 1),
            path: path.to_string(),
            index: n + 1,
            old_start: old_start + 1,
            old_count,
            new_start: new_start + 1,
            new_count,
            lines: hlines,
        });
    }
    out
}

fn render_hunks(hunks: &[Hunk]) -> String {
    if hunks.is_empty() {
        return String::new();
    }
    let path = &hunks[0].path;
    let mut out = format!("--- a/{}\n+++ b/{}\n", path, path);
    for h in hunks {
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            h.old_start, h.old_count, h.new_start, h.new_count
        ));
        for line in &h.lines {
            out.push_str(line);
            if !line.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    out
}

fn patch_text(path: &str, old: &[u8], new: &[u8]) -> String {
    if !is_text(old) || !is_text(new) {
        return format!("Binary files a/{} and b/{} differ\n", path, path);
    }
    let hunks = make_hunks(path, old, new);
    if hunks.is_empty() {
        return format!(
            "（文件较大或无可逐行 diff；a/{} {} bytes → b/{} {} bytes）\n",
            path,
            old.len(),
            path,
            new.len()
        );
    }
    render_hunks(&hunks)
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
        let old_bytes = old.as_deref().unwrap_or(b"");
        let new_bytes = new.as_deref().unwrap_or(b"");
        let hunks = make_hunks(&key, old_bytes, new_bytes);
        let patch = if hunks.is_empty() {
            patch_text(&key, old_bytes, new_bytes)
        } else {
            render_hunks(&hunks)
        };
        match (&old, &new) {
            (Some(o), Some(n)) if o == n => {}
            (None, Some(n)) => changes.push(Change {
                path: key.clone(),
                from: None,
                kind: "added".into(),
                old: None,
                new: Some(n.clone()),
                patch: patch.clone(),
                hunks: hunks.clone(),
            }),
            (Some(o), None) => changes.push(Change {
                path: key.clone(),
                from: None,
                kind: "deleted".into(),
                old: Some(o.clone()),
                new: None,
                patch: patch.clone(),
                hunks: hunks.clone(),
            }),
            (Some(o), Some(n)) => changes.push(Change {
                path: key.clone(),
                from: None,
                kind: "modified".into(),
                old: Some(o.clone()),
                new: Some(n.clone()),
                patch: patch.clone(),
                hunks: hunks.clone(),
            }),
            (None, None) => {}
        }
    }
    // 内容完全相同的 delete + add 视为 rename。只做精确内容配对；
    // “改名又改内容”仍按 delete+add 呈现，避免猜测导致 review 写错。
    let mut deleted_by_hash: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, c) in changes.iter().enumerate() {
        if c.kind == "deleted" {
            if let Some(old) = &c.old {
                deleted_by_hash
                    .entry(sha256::hex(old))
                    .or_default()
                    .push(i);
            }
        }
    }
    let mut consumed_added = std::collections::BTreeSet::new();
    let mut renamed: Vec<(usize, usize)> = Vec::new();
    for (i, c) in changes.iter().enumerate() {
        if c.kind != "added" {
            continue;
        }
        let Some(new) = &c.new else { continue };
        let hash = sha256::hex(new);
        if let Some(bucket) = deleted_by_hash.get_mut(&hash) {
            if let Some(di) = bucket.pop() {
                renamed.push((di, i));
                consumed_added.insert(i);
            }
        }
    }
    for (di, ai) in &renamed {
        let add_path = changes[*ai].path.clone();
        let add_new = changes[*ai].new.clone();
        let old_path = changes[*di].path.clone();
        changes[*di].kind = "renamed".into();
        changes[*di].from = Some(old_path);
        changes[*di].path = add_path;
        changes[*di].new = add_new;
        changes[*di].patch = String::new();
        changes[*di].hunks.clear();
    }
    let mut filtered = Vec::new();
    for (i, c) in changes.into_iter().enumerate() {
        if consumed_added.contains(&i) {
            continue;
        }
        filtered.push(c);
    }
    Ok(DiffReport {
        checkpoint,
        changes: filtered,
    })
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

fn apply_hunks(old: &[u8], hunks: &[Hunk]) -> Result<Vec<u8>, String> {
    let old_lines = lines(old);
    let mut ordered: Vec<&Hunk> = hunks.iter().collect();
    ordered.sort_by_key(|h| h.old_start);
    let mut out = String::new();
    let mut cursor = 0usize;
    for h in ordered {
        let start = h.old_start.saturating_sub(1);
        if start < cursor {
            return Err(format!("hunk {} 重叠", h.id));
        }
        if start > old_lines.len() {
            return Err(format!("hunk {} 起点越界", h.id));
        }
        for line in &old_lines[cursor..start] {
            out.push_str(line);
        }
        let old_block: Vec<&str> = h
            .lines
            .iter()
            .filter(|l| !l.starts_with('+'))
            .map(|l| &l[1..])
            .collect();
        if old_block.len() != h.old_count {
            return Err(format!("hunk {} oldCount 不一致", h.id));
        }
        if start + old_block.len() > old_lines.len() {
            return Err(format!("hunk {} 超出旧文件", h.id));
        }
        for (i, want) in old_block.iter().enumerate() {
            if old_lines[start + i] != *want {
                return Err(format!("hunk {} 上下文不匹配", h.id));
            }
        }
        for line in &h.lines {
            if !line.starts_with('-') {
                out.push_str(&line[1..]);
            }
        }
        cursor = start + old_block.len();
    }
    for line in &old_lines[cursor..] {
        out.push_str(line);
    }
    Ok(out.into_bytes())
}

fn write_review_target(ws: &Path, target: &Path, bytes: &[u8]) -> Result<(), String> {
    ensure_restore_target(ws, target)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {}", e))?;
    }
    let tmp = target.with_extension(format!("ui-review-{}", std::process::id()));
    fs::write(&tmp, bytes).map_err(|e| format!("写 {} 失败: {}", target.display(), e))?;
    fs::rename(&tmp, target).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("落盘 {} 失败: {}", target.display(), e)
    })
}

/// 按 hunk 接受/拒绝一次 diff。
///
/// 语义：`accepted` 里的 hunk 保留新内容，其余 hunk 回到 checkpoint 的旧内容。
/// 写盘前自动创建 `pre-review-<id>`，所以 review 本身也可回滚。
pub fn review(ws: &Path, id: &str, accepted: &[String]) -> Result<Json, String> {
    let report = diff(ws, Some(id))?;
    let mut all_ids = Vec::new();
    for c in &report.changes {
        for h in &c.hunks {
            all_ids.push(h.id.clone());
        }
    }
    let file_level: std::collections::BTreeSet<String> = report
        .changes
        .iter()
        .filter(|c| c.hunks.is_empty())
        .map(|c| c.path.clone())
        .collect();
    for a in accepted {
        if a != "all" && !all_ids.contains(a) && !file_level.contains(a) {
            return Err(format!("未知 hunk: {}", a));
        }
    }
    let accept_all = accepted.iter().any(|s| s == "all");
    let accepted_set: std::collections::BTreeSet<String> = if accept_all {
        all_ids.iter().cloned().collect()
    } else {
        accepted.iter().cloned().collect()
    };

    // 先算完所有目标内容，再写盘：某个 hunk 上下文不匹配时不能留下半套修改。
    let mut plan: Vec<(std::path::PathBuf, Option<Vec<u8>>, String)> = Vec::new();
    for change in &report.changes {
        let target = ws.join(Path::new(&change.path));
        ensure_restore_target(ws, &target)?;
        if change.kind == "renamed" {
            let accepted = accept_all || accepted_set.contains(&change.path);
            let from = change.from.as_deref().unwrap_or("");
            let old_target = ws.join(Path::new(from));
            ensure_restore_target(ws, &old_target)?;
            if accepted {
                plan.push((target, change.new.clone(), change.path.clone()));
                plan.push((old_target, None, from.to_string()));
            } else {
                plan.push((target, None, change.path.clone()));
                plan.push((old_target, change.old.clone(), from.to_string()));
            }
            continue;
        }
        let selected: Vec<Hunk> = change
            .hunks
            .iter()
            .filter(|h| accepted_set.contains(&h.id))
            .cloned()
            .collect();
        let result: Option<Vec<u8>> = if change.hunks.is_empty() {
            // 二进制/过大文件没有 hunk：文件名可作为文件级接受标记。
            if accept_all || accepted_set.contains(&change.path) {
                change.new.clone()
            } else {
                change.old.clone()
            }
        } else if selected.len() == change.hunks.len() {
            change.new.clone()
        } else if selected.is_empty() {
            change.old.clone()
        } else {
            let old = change.old.clone().unwrap_or_default();
            Some(apply_hunks(&old, &selected)?)
        };
        plan.push((target, result, change.path.clone()));
    }

    let backup = create(ws, &format!("pre-review-{}", id))?;
    let mut written = 0u64;
    let mut removed = 0u64;
    for (target, result, rel) in plan {
        match result {
            Some(bytes) => {
                write_review_target(ws, &target, &bytes)?;
                written += 1;
            }
            None => {
                if target.exists() {
                    ensure_restore_target(ws, &target)?;
                    fs::remove_file(&target).map_err(|e| format!("删除 {} 失败: {}", rel, e))?;
                    removed += 1;
                }
            }
        }
    }
    Ok(Json::obj(vec![
        ("checkpoint", Json::from(id)),
        ("accepted", Json::from(accepted_set.len() as u64)),
        ("written", Json::from(written)),
        ("removed", Json::from(removed)),
        ("backup", backup.get("id").cloned().unwrap_or(Json::Null)),
    ]))
}

/// 只回退一个文件：manifest 里有就写回旧内容，没有（checkpoint 后新增）就删除。
pub fn revert_file(ws: &Path, id: &str, rel: &str) -> Result<Json, String> {
    let rel_path = Path::new(rel);
    let reserved = rel
        .split('/')
        .next()
        .map(|s| matches!(s, ".ui" | ".git" | "target" | "node_modules"))
        .unwrap_or(false);
    if rel_path.is_absolute() || rel.split('/').any(|s| s == "..") || reserved {
        return Err(format!("拒绝回退受保护路径: {}", rel));
    }
    let manifest = load_manifest(ws, id)?;
    let entry = manifest
        .get("entries")
        .and_then(|v| v.as_arr())
        .and_then(|a| a.iter().find(|e| e.str_at("path") == Some(rel)))
        .cloned();
    let target = ws.join(rel_path);
    ensure_restore_target(ws, &target)?;
    if entry.is_none() && !target.exists() {
        return Err(format!("{} 不在 checkpoint，也不在当前工作区", rel));
    }
    let backup = create(ws, &format!("pre-revert-{}", id))?;
    let action;
    if let Some(e) = entry {
        let hash = e.str_at("sha256").unwrap_or("");
        let bytes = read_blob(ws, hash)?;
        write_review_target(ws, &target, &bytes)?;
        action = "restored";
    } else {
        fs::remove_file(&target).map_err(|e| format!("删除 {} 失败: {}", rel, e))?;
        action = "removed";
    }
    Ok(Json::obj(vec![
        ("checkpoint", Json::from(id)),
        ("path", Json::from(rel)),
        ("action", Json::from(action)),
        ("backup", backup.get("id").cloned().unwrap_or(Json::Null)),
    ]))
}

pub fn restore(ws: &Path, id: &str) -> Result<Json, String> {
    restore_inner(ws, id, false)
}

/// `--prune`：写回 manifest 后，删除 checkpoint 之后新增的未跟踪文件。
pub fn restore_prune(ws: &Path, id: &str) -> Result<Json, String> {
    restore_inner(ws, id, true)
}

fn restore_inner(ws: &Path, id: &str, prune: bool) -> Result<Json, String> {
    let manifest = load_manifest(ws, id)?;
    // 先给当前工作区留一个可回滚点；restore 本身也可能写错。
    let backup = create(ws, &format!("pre-restore-{}", id))?;
    let entries = manifest
        .get("entries")
        .and_then(|v| v.as_arr())
        .cloned()
        .unwrap_or_default();
    let tracked: std::collections::BTreeSet<String> = entries
        .iter()
        .filter_map(|e| e.str_at("path").map(str::to_string))
        .collect();
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
    let mut pruned = 0u64;
    if prune {
        let current = scan(ws)?;
        for (rel, path) in current {
            if tracked.contains(&rel) {
                continue;
            }
            ensure_restore_target(ws, &path)?;
            fs::remove_file(&path).map_err(|e| format!("prune {} 失败: {}", rel, e))?;
            pruned += 1;
        }
    }
    Ok(Json::obj(vec![
        ("restored", Json::from(restored)),
        ("pruned", Json::from(pruned)),
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
    fn restore_prune_removes_untracked_files_only_when_asked() {
        let ws = scratch("prune");
        fs::write(ws.join("a.txt"), "a\n").unwrap();
        let cp = create(&ws, "before").unwrap();
        let id = cp.str_at("id").unwrap().to_string();
        fs::write(ws.join("extra.txt"), "extra\n").unwrap();

        restore(&ws, &id).unwrap();
        assert!(ws.join("extra.txt").exists(), "默认 restore 不删新文件");

        fs::write(ws.join("extra.txt"), "extra\n").unwrap();
        let out = restore_prune(&ws, &id).unwrap();
        assert_eq!(out.u64_at("pruned"), Some(1));
        assert!(!ws.join("extra.txt").exists());
        assert!(ws.join("a.txt").exists());
        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn checkpoint_skips_secret_files_and_private_dirs() {
        let ws = scratch("private");
        fs::write(ws.join(".env"), "TOKEN=secret\n").unwrap();
        fs::create_dir_all(ws.join("secrets")).unwrap();
        fs::write(ws.join("secrets/db.txt"), "pw\n").unwrap();
        fs::write(ws.join("id_rsa"), "key\n").unwrap();
        fs::write(ws.join("a.txt"), "ok\n").unwrap();
        let cp = create(&ws, "before").unwrap();
        let id = cp.str_at("id").unwrap();
        let manifest = load_manifest(&ws, id).unwrap();
        let paths: Vec<&str> = manifest
            .arr_at("entries")
            .map(|a| a.iter().filter_map(|e| e.str_at("path")).collect())
            .unwrap_or_default();
        assert_eq!(paths, vec!["a.txt"], "checkpoint 不能复制私有文件: {:?}", paths);
        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn automatic_limit_fails_before_writing_a_manifest() {
        let ws = scratch("limited");
        fs::write(ws.join("a.txt"), "a").unwrap();
        fs::write(ws.join("b.txt"), "bb").unwrap();
        let err = create_limited_with(&ws, "auto", 1, 1024).unwrap_err();
        assert!(err.contains("文件数超过上限"), "{}", err);
        assert!(list(&ws).unwrap().is_empty(), "超限不能留下半份 checkpoint");
        let _ = fs::remove_dir_all(&ws);
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
    fn review_keeps_only_accepted_hunks() {
        let ws = scratch("review");
        let old: String = (0..20).map(|i| format!("line {}\n", i)).collect();
        fs::write(ws.join("a.txt"), &old).unwrap();
        let cp = create(&ws, "before").unwrap();
        let id = cp.str_at("id").unwrap().to_string();

        let mut new_lines: Vec<String> = (0..20).map(|i| format!("line {}\n", i)).collect();
        new_lines[1] = "CHANGED-1\n".into();
        new_lines[18] = "CHANGED-18\n".into();
        fs::write(ws.join("a.txt"), new_lines.concat()).unwrap();

        let report = diff(&ws, Some(&id)).unwrap();
        let hunks = &report.changes[0].hunks;
        assert_eq!(hunks.len(), 2, "two separated changes should be two hunks");
        let first = hunks[0].id.clone();
        let out = review(&ws, &id, &[first]).unwrap();
        assert_eq!(out.u64_at("written"), Some(1));
        let got = fs::read_to_string(ws.join("a.txt")).unwrap();
        assert!(got.contains("CHANGED-1"));
        assert!(!got.contains("CHANGED-18"), "rejected hunk must revert");
        assert!(got.contains("line 18"));
        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn revert_file_restores_or_deletes_one_path_only() {
        let ws = scratch("revert-file");
        fs::write(ws.join("a.txt"), "old\n").unwrap();
        fs::write(ws.join("b.txt"), "keep\n").unwrap();
        let cp = create(&ws, "before").unwrap();
        let id = cp.str_at("id").unwrap().to_string();
        fs::write(ws.join("a.txt"), "new\n").unwrap();
        fs::write(ws.join("added.txt"), "added\n").unwrap();

        let out = revert_file(&ws, &id, "a.txt").unwrap();
        assert_eq!(out.str_at("action"), Some("restored"));
        assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "old\n");
        assert_eq!(fs::read_to_string(ws.join("b.txt")).unwrap(), "keep\n");
        assert!(ws.join("added.txt").exists(), "只回退指定路径");

        let out = revert_file(&ws, &id, "added.txt").unwrap();
        assert_eq!(out.str_at("action"), Some("removed"));
        assert!(!ws.join("added.txt").exists());
        assert!(revert_file(&ws, &id, ".ui/journal.jsonl").is_err());
        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn review_accepts_binary_by_file_path() {
        let ws = scratch("review-binary");
        fs::write(ws.join("a.bin"), [0u8, 1, 2]).unwrap();
        let cp = create(&ws, "before").unwrap();
        let id = cp.str_at("id").unwrap().to_string();
        fs::write(ws.join("a.bin"), [0u8, 9, 2]).unwrap();
        let report = diff(&ws, Some(&id)).unwrap();
        assert!(report.changes[0].hunks.is_empty());
        let out = review(&ws, &id, &["a.bin".to_string()]).unwrap();
        assert_eq!(out.u64_at("written"), Some(1));
        assert_eq!(fs::read(ws.join("a.bin")).unwrap(), vec![0u8, 9, 2]);
        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn exact_content_rename_is_detected_and_reviewed() {
        let ws = scratch("rename");
        fs::write(ws.join("a.txt"), "same\n").unwrap();
        let cp = create(&ws, "before").unwrap();
        let id = cp.str_at("id").unwrap().to_string();
        fs::rename(ws.join("a.txt"), ws.join("b.txt")).unwrap();

        let report = diff(&ws, Some(&id)).unwrap();
        assert_eq!(report.changes.len(), 1);
        let c = &report.changes[0];
        assert_eq!(c.kind, "renamed");
        assert_eq!(c.path, "b.txt");
        assert_eq!(c.from.as_deref(), Some("a.txt"));

        review(&ws, &id, &["b.txt".to_string()]).unwrap();
        assert!(!ws.join("a.txt").exists());
        assert!(ws.join("b.txt").exists());

        review(&ws, &id, &[]).unwrap();
        assert!(ws.join("a.txt").exists());
        assert!(!ws.join("b.txt").exists());
        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn review_rejects_unknown_hunk_without_writing() {
        let ws = scratch("review-unknown");
        fs::write(ws.join("a.txt"), "one\n").unwrap();
        let cp = create(&ws, "before").unwrap();
        let id = cp.str_at("id").unwrap().to_string();
        fs::write(ws.join("a.txt"), "two\n").unwrap();
        let err = review(&ws, &id, &["a.txt#999".to_string()]).unwrap_err();
        assert!(err.contains("未知 hunk"));
        assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "two\n");
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
