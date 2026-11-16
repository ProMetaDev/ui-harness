//! 实验包的读取与校验：CLI 的 `lab verify` 和 Web 的「实验室」视图共用这一份。

use std::path::{Path, PathBuf};

use ui_core::journal::Journal;
use ui_core::json::Json;
use ui_core::sha256;

pub struct Verify {
    pub ok: bool,
    pub artifacts: usize,
    pub inputs: usize,
    pub claims_ok: usize,
    pub claims_total: usize,
    pub journal_ok: bool,
    pub issues: Vec<String>,
}

fn sha256_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(sha256::hex(&bytes))
}

pub fn numeric_leaves(dir: &Path) -> std::collections::BTreeMap<String, f64> {
    fn walk(value: &Json, prefix: &str, out: &mut std::collections::BTreeMap<String, f64>) {
        match value {
            Json::Num(n) => {
                out.insert(prefix.to_string(), *n);
            }
            Json::Obj(items) => {
                for (k, v) in items {
                    let key = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{}.{}", prefix, k)
                    };
                    walk(v, &key, out);
                }
            }
            _ => {}
        }
    }
    let mut out = std::collections::BTreeMap::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().map(|e| e == "json").unwrap_or(false) {
                if let Ok(text) = std::fs::read_to_string(&p) {
                    if let Ok(v) = ui_core::json::parse(&text) {
                        walk(&v, "", &mut out);
                    }
                }
            }
        }
    }
    out
}

/// 三项检查：产物哈希、输入一致性（给了工作区才查）、结论对账；外加日志链与未完成调用。
pub fn verify_bundle(dir: &Path, workspace: Option<&Path>) -> Verify {
    let mut out = Verify {
        ok: false,
        artifacts: 0,
        inputs: 0,
        claims_ok: 0,
        claims_total: 0,
        journal_ok: false,
        issues: Vec::new(),
    };

    if let Ok(text) = std::fs::read_to_string(dir.join("manifest.jsonl")) {
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let rec = match ui_core::json::parse(line) {
                Ok(v) => v,
                Err(_) => {
                    out.issues.push("manifest 行不是 JSON".to_string());
                    continue;
                }
            };
            let rel = rec.str_at("path").unwrap_or("");
            let want = rec.str_at("sha256").unwrap_or("");
            match sha256_file(&dir.join("artifacts").join(rel)) {
                Some(got) if got == want => out.artifacts += 1,
                Some(_) => out.issues.push(format!("{} 哈希不符", rel)),
                None => out.issues.push(format!("{} 缺失", rel)),
            }
        }
    } else {
        out.issues.push("没有 manifest.jsonl".to_string());
    }

    match Journal::read(dir.join("journal.jsonl")) {
        Ok((events, corrupt)) => {
            out.journal_ok = corrupt.is_none();
            if !out.journal_ok {
                out.issues.push("日志链不完整".to_string());
            } else {
                let inc = ui_core::journal::analyze_incomplete(&events);
                if inc.open_turn.is_some() || !inc.open_tools.is_empty() {
                    out.issues.push("日志里有未完成的调用".to_string());
                }
            }
        }
        Err(_) => out.issues.push("读不到 journal.jsonl".to_string()),
    }

    if let Some(ws) = workspace {
        if let Ok(text) = std::fs::read_to_string(dir.join("inputs.jsonl")) {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                let rec = match ui_core::json::parse(line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let rel = rec.str_at("path").unwrap_or("");
                let want = rec.str_at("sha256").unwrap_or("");
                match sha256_file(&ws.join(rel)) {
                    Some(got) if got == want => out.inputs += 1,
                    Some(_) => out.issues.push(format!("输入 {} 已改变", rel)),
                    None => out.issues.push(format!("输入 {} 找不到", rel)),
                }
            }
        }
    }

    if let Ok(text) = std::fs::read_to_string(dir.join("claims.json")) {
        if let Ok(doc) = ui_core::json::parse(&text) {
            let leaves = numeric_leaves(&dir.join("artifacts"));
            for c in doc.arr_at("claims").cloned().unwrap_or_default() {
                out.claims_total += 1;
                let name = c.str_at("name").unwrap_or("");
                let want = c.get("value").and_then(|v| v.as_f64()).unwrap_or(f64::NAN);
                let tol = (want.abs() * 1e-3).max(1e-9);
                let hit = want.is_finite() && leaves.values().any(|v| (v - want).abs() <= tol);
                if hit {
                    out.claims_ok += 1;
                } else {
                    out.issues.push(format!("结论 {} = {} 无产物背书", name, want));
                }
            }
        }
    }

    out.ok = out.issues.is_empty();
    out
}

/// 扫描一个目录下的所有实验包（有 meta.json 的子目录）。
pub fn list_bundles(root: &Path) -> Vec<Json> {
    let mut out: Vec<Json> = Vec::new();
    let rd = match std::fs::read_dir(root) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for entry in rd.flatten() {
        let dir: PathBuf = entry.path();
        if !dir.is_dir() || !dir.join("meta.json").is_file() {
            continue;
        }
        let meta = std::fs::read_to_string(dir.join("meta.json"))
            .ok()
            .and_then(|t| ui_core::json::parse(&t).ok())
            .unwrap_or(Json::Null);
        let ws = meta.str_at("workspace").map(PathBuf::from);
        let report = verify_bundle(&dir, ws.as_deref());
        out.push(Json::obj(vec![
            (
                "name",
                Json::from(
                    meta.str_at("name")
                        .map(str::to_string)
                        .unwrap_or_else(|| entry.file_name().to_string_lossy().to_string()),
                ),
            ),
            ("dir", Json::from(dir.to_string_lossy().to_string())),
            ("createdMs", Json::from(meta.u64_at("createdMs").unwrap_or(0))),
            ("artifacts", Json::from(report.artifacts)),
            ("claims", Json::from(report.claims_total)),
            ("ok", Json::Bool(report.ok)),
            ("issues", Json::from(report.issues.len())),
            (
                "issueText",
                Json::from(report.issues.first().cloned().unwrap_or_default()),
            ),
        ]));
    }
    out.sort_by(|a, b| b.u64_at("createdMs").cmp(&a.u64_at("createdMs")));
    out
}
