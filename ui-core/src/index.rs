//! 零依赖代码库索引：文件/行数/符号/内容哈希，以及按 query 的相关文件地图。
//!
//! 不引入 tree-sitter、embedding、向量库或数据库。索引只是 `.ui/index/manifest.json`
//! 一份 JSON，构建是一次文件扫描；查询是字符串匹配 + 简单打分。它不会替代
//! `grep/fs_read`，只负责把“大仓库里先看哪几个文件”变得可复现。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::json::Json;
use crate::sha256;
use crate::time;

const EXCLUDED: &[&str] = &[".git", ".ui", "target", "node_modules", "dist", "build"];
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_SYMBOLS: usize = 40;

pub const MANIFEST: &str = ".ui/index/manifest.json";

fn rel_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn walk(ws: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
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
            walk(ws, &path, out)?;
        } else if meta.is_file() {
            out.push(path);
        }
    }
    let _ = ws;
    Ok(())
}

fn is_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

fn symbol_from_line(line: &str) -> Option<String> {
    let t = line.trim_start();
    for (kw, prefix) in [
        ("fn ", "fn "),
        ("pub fn ", "fn "),
        ("async fn ", "fn "),
        ("struct ", "struct "),
        ("pub struct ", "struct "),
        ("enum ", "enum "),
        ("pub enum ", "enum "),
        ("trait ", "trait "),
        ("pub trait ", "trait "),
        ("class ", "class "),
        ("def ", "def "),
        ("function ", "fn "),
        ("const ", "const "),
    ] {
        if let Some(rest) = t.strip_prefix(kw) {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':')
                .collect();
            if !name.is_empty() {
                return Some(format!("{}{}", prefix, name));
            }
        }
    }
    if t.starts_with('#') && !t.starts_with("##") {
        let h = t.trim_start_matches('#').trim();
        if !h.is_empty() {
            return Some(format!("h {}", h.chars().take(60).collect::<String>()));
        }
    }
    None
}

fn symbols(path: &str, text: &str) -> Vec<String> {
    // 只对文本里出现过的语言做符号提取；不靠扩展名硬猜。
    let mut out = Vec::new();
    for line in text.lines() {
        if out.len() >= MAX_SYMBOLS {
            break;
        }
        if let Some(sym) = symbol_from_line(line) {
            out.push(sym);
        }
    }
    let _ = path;
    out
}

fn mtime_ms(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn build_with(ws: &Path, incremental: bool) -> Result<Json, String> {
    let mut paths = Vec::new();
    walk(ws, ws, &mut paths)?;
    paths.sort();

    let previous: BTreeMap<String, Json> = if incremental {
        load(ws)
            .ok()
            .and_then(|m| m.arr_at("files").cloned())
            .map(|rows| {
                rows.into_iter()
                    .filter_map(|e| {
                        let path = e.str_at("path")?.to_string();
                        Some((path, e))
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        BTreeMap::new()
    };
    let mut seen = BTreeMap::new();
    let mut entries = Vec::new();
    let mut symbols_count = 0u64;
    let mut reused = 0u64;
    let mut updated = 0u64;

    for path in paths {
        let rel = rel_key(path.strip_prefix(ws).unwrap_or(&path));
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if let Some(prev) = previous.get(&rel) {
            if prev.u64_at("bytes") == Some(meta.len()) {
                let prev_mtime = prev.u64_at("mtimeMs").unwrap_or(0);
                if prev_mtime != 0 && prev_mtime == mtime_ms(&meta) {
                    symbols_count += prev
                        .arr_at("symbols")
                        .map(|a| a.len() as u64)
                        .unwrap_or(0);
                    entries.push(prev.clone());
                    seen.insert(rel.clone(), ());
                    reused += 1;
                    continue;
                }
            }
        }
        let bytes = fs::read(&path).map_err(|e| format!("读 {} 失败: {}", rel, e))?;
        if bytes.len() as u64 > MAX_FILE_BYTES || !is_text(&bytes) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let syms = symbols(&rel, &text);
        symbols_count += syms.len() as u64;
        entries.push(Json::obj(vec![
            ("path", Json::from(rel.as_str())),
            ("bytes", Json::from(bytes.len() as u64)),
            ("lines", Json::from(text.lines().count() as u64)),
            ("mtimeMs", Json::from(mtime_ms(&meta))),
            ("sha256", Json::from(sha256::hex(&bytes))),
            (
                "symbols",
                Json::Arr(syms.iter().map(|s| Json::from(s.as_str())).collect()),
            ),
        ]));
        seen.insert(rel.clone(), ());
        updated += 1;
    }

    let removed = previous.keys().filter(|k| !seen.contains_key(*k)).count() as u64;
    let body = Json::obj(vec![
        ("version", Json::from(2u64)),
        ("builtAt", Json::from(time::now_ms())),
        ("workspace", Json::from(ws.to_string_lossy().to_string())),
        ("incremental", Json::Bool(incremental)),
        ("reused", Json::from(reused)),
        ("updated", Json::from(updated)),
        ("removed", Json::from(removed)),
        ("files", Json::Arr(entries)),
    ]);
    let path = ws.join(MANIFEST);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建索引目录失败: {}", e))?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, body.to_pretty()).map_err(|e| format!("写索引失败: {}", e))?;
    fs::rename(&tmp, &path).map_err(|e| format!("落盘索引失败: {}", e))?;
    Ok(Json::obj(vec![
        (
            "files",
            Json::from(body.arr_at("files").map(|a| a.len()).unwrap_or(0) as u64),
        ),
        ("symbols", Json::from(symbols_count)),
        ("reused", Json::from(reused)),
        ("updated", Json::from(updated)),
        ("removed", Json::from(removed)),
        ("manifest", Json::from(path.to_string_lossy().to_string())),
    ]))
}

/// 构建索引，返回文件数与符号数。
pub fn build(ws: &Path) -> Result<Json, String> {
    build_with(ws, false)
}

/// 增量：大小与 mtime 都未变的文件复用旧符号；变了就重读。
pub fn build_incremental(ws: &Path) -> Result<Json, String> {
    build_with(ws, true)
}

pub fn load(ws: &Path) -> Result<Json, String> {
    let path = ws.join(MANIFEST);
    let text = fs::read_to_string(&path)
        .map_err(|_| "还没有索引；先运行 `ui index`".to_string())?;
    crate::json::parse(&text).map_err(|e| format!("索引不是合法 JSON: {}", e))
}

fn terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .map(|s| s.to_lowercase())
        .filter(|s| s.len() >= 2)
        .take(12)
        .collect()
}

/// 返回最多 `limit` 个相关文件（path/symbol 命中打分）。
pub fn search(ws: &Path, query: &str, limit: usize) -> Result<Vec<Json>, String> {
    let manifest = load(ws)?;
    let needles = terms(query);
    if needles.is_empty() {
        return Ok(Vec::new());
    }
    let mut scored: Vec<(u32, Json)> = Vec::new();
    for e in manifest
        .get("files")
        .and_then(|v| v.as_arr())
        .cloned()
        .unwrap_or_default()
    {
        let path = e.str_at("path").unwrap_or("").to_lowercase();
        let syms: Vec<String> = e
            .arr_at("symbols")
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|s| s.as_str().map(|s| s.to_lowercase()))
            .collect();
        let mut score = 0u32;
        for n in &needles {
            if path.contains(n) {
                score += 4;
            }
            for s in &syms {
                if s.contains(n) {
                    score += 3;
                }
            }
        }
        if score > 0 {
            let mut e = e;
            e.set("score", Json::from(score as u64));
            scored.push((score, e));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.str_at("path").cmp(&a.1.str_at("path"))));
    Ok(scored.into_iter().take(limit).map(|(_, e)| e).collect())
}

/// 给 prompt 的相关文件地图；索引不存在时返回空串，不阻塞 agent。
pub fn context(ws: &Path, query: &str, limit: usize) -> String {
    let Ok(hits) = search(ws, query, limit) else {
        return String::new();
    };
    if hits.is_empty() {
        return String::new();
    }
    let mut out = String::from("## 相关文件（轻量索引；不是全文）\n");
    for e in hits {
        let path = e.str_at("path").unwrap_or("?");
        let lines = e.u64_at("lines").unwrap_or(0);
        let syms: Vec<&str> = e
            .arr_at("symbols")
            .map(|a| a.iter().filter_map(|s| s.as_str()).take(5).collect())
            .unwrap_or_default();
        if syms.is_empty() {
            out.push_str(&format!("- {} ({} 行)\n", path, lines));
        } else {
            out.push_str(&format!("- {} ({} 行) — {}\n", path, lines, syms.join(", ")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ui-index-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn index_finds_symbols_and_ignores_ui_internal() {
        let ws = scratch("symbols");
        fs::create_dir_all(ws.join("src")).unwrap();
        fs::write(
            ws.join("src/agent.rs"),
            "pub fn run_turn() {}\nstruct Agent {}\n",
        )
        .unwrap();
        fs::write(ws.join("README.md"), "# Hello\n").unwrap();
        fs::create_dir_all(ws.join(".ui")).unwrap();
        fs::write(ws.join(".ui/secret.txt"), "should not index").unwrap();

        let out = build(&ws).unwrap();
        assert_eq!(out.u64_at("files"), Some(2));
        let hits = search(&ws, "run_turn", 5).unwrap();
        assert_eq!(hits[0].str_at("path"), Some("src/agent.rs"));
        assert!(hits[0]
            .arr_at("symbols")
            .map(|a| a.iter().any(|s| s.as_str() == Some("fn run_turn")))
            .unwrap_or(false));
        let text = context(&ws, "agent", 5);
        assert!(text.contains("src/agent.rs"));
        assert!(!text.contains(".ui/secret"));
        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn incremental_reuses_unchanged_and_updates_changed() {
        let ws = scratch("incremental");
        fs::write(ws.join("a.rs"), "fn a() {}\n").unwrap();
        fs::write(ws.join("b.rs"), "fn b() {}\n").unwrap();
        build(&ws).unwrap();
        let again = build_incremental(&ws).unwrap();
        assert_eq!(again.u64_at("reused"), Some(2));
        assert_eq!(again.u64_at("updated"), Some(0));

        fs::write(ws.join("a.rs"), "fn a() {}\nfn a2() {}\n").unwrap();
        let changed = build_incremental(&ws).unwrap();
        assert_eq!(changed.u64_at("updated"), Some(1));
        assert_eq!(changed.u64_at("reused"), Some(1));

        fs::remove_file(ws.join("b.rs")).unwrap();
        let removed = build_incremental(&ws).unwrap();
        assert_eq!(removed.u64_at("removed"), Some(1));
        let _ = fs::remove_dir_all(&ws);
    }

    #[test]
    fn missing_index_is_a_clear_error() {
        let ws = scratch("missing");
        assert!(load(&ws).unwrap_err().contains("ui index"));
        let _ = fs::remove_dir_all(&ws);
    }
}
