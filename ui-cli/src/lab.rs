//! 实验包的读取与校验。Web 的「实验室」视图（`/api/lab/bundles`）用这一份；
//! CLI 的 `ui lab verify` 把**结论对账**（`verify_claims`）也委托给这里，
//! 两边共用同一套绑定语法与 `loose` 判定，别再各写一份。
//!
//! 结论绑定语法：`文件.json#/字段/数组下标`，例如
//!
//!   metrics.json#/accuracy
//!   metrics.json#/history/0/score
//!
//! 旧格式（只写名字、没有 `#`）仍然接受，但只能在**全部产物**里做全局任意匹配，
//! 有「别的字段数值接近 → 巧合通过」的风险，所以会进 `warnings` 并标 `loose: true`。
//! claim 也可以把绑定串放在 `path` 字段里、`name` 保留人类可读的标签；两者都给时
//! 以 `path` 为准，且 `path` 必须写成完整绑定（不允许悄悄退化成旧格式）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ui_core::journal::Journal;
use ui_core::json::Json;
use ui_core::sha256;

/// 一条结论的核验结果。
#[derive(Clone, Debug)]
pub struct ClaimOutcome {
    /// claim 里的 `name`（人写的标签；新格式通常就是绑定路径本身）。
    pub name: String,
    pub want: f64,
    pub ok: bool,
    /// 旧格式（没有 `#`）：全局任意匹配，存在巧合通过的风险。
    pub loose: bool,
    /// 绑定的产物位置，如 `metrics.json#/history/0/score`；旧格式为 `None`。
    pub bound: Option<String>,
    pub detail: String,
}

/// 结论对账的汇总：CLI 的 `--json` 与 Web 的实验室列表都从这里取数。
#[derive(Clone, Debug, Default)]
pub struct ClaimReport {
    pub total: usize,
    pub ok: usize,
    /// 只要有一条走了旧格式全局匹配，就是 `true`。
    pub loose: bool,
    /// 不导致失败、但必须让人看见的提示（每条以 `⚠` 开头）。
    pub warnings: Vec<String>,
    /// 导致校验失败的问题。
    pub issues: Vec<String>,
    pub claims: Vec<ClaimOutcome>,
}

impl ClaimReport {
    /// `claims` 数组（`--json` 里的 `claims` 字段）。
    pub fn claims_json(&self) -> Json {
        let mut out = Vec::with_capacity(self.claims.len());
        for c in &self.claims {
            out.push(Json::obj(vec![
                ("name", Json::from(c.name.as_str())),
                (
                    "value",
                    if c.want.is_finite() {
                        Json::Num(c.want)
                    } else {
                        Json::Null
                    },
                ),
                ("ok", Json::Bool(c.ok)),
                ("loose", Json::Bool(c.loose)),
                (
                    "bound",
                    c.bound.clone().map(Json::from).unwrap_or(Json::Null),
                ),
                ("detail", Json::from(c.detail.as_str())),
            ]));
        }
        Json::Arr(out)
    }
}

/// 一条 claim 的绑定解析结果。
#[derive(Clone, Debug, PartialEq)]
pub enum ClaimRef {
    /// 没有 `#`：旧格式，全局任意匹配。
    Loose,
    /// `文件.json#/字段/下标`。
    Scoped { file: String, path: Vec<String> },
    /// 有 `#` 但写坏了（例如只有 `#/accuracy`，没写文件名）——**拒绝**，不猜。
    Invalid(String),
}

/// 解析 claim 的绑定串。
///
/// 约定（fail closed）：出现 `#` 就必须写成 `文件.json#/路径`；只写
/// `#/accuracy` 视为**错误**而不是降级成全局匹配——不写文件名的绑定本就不唯一，
/// 猜错比不查更糟。没有 `#` 的整串才是旧格式。
pub fn parse_claim_ref(raw: &str) -> ClaimRef {
    let raw = raw.trim();
    let (file, ptr) = match raw.split_once('#') {
        None => return ClaimRef::Loose,
        Some((f, p)) => (f.trim(), p),
    };
    if file.is_empty() {
        return ClaimRef::Invalid(format!(
            "`{}` 有 `#` 却没写产物文件名（应写成 metrics.json#/accuracy）",
            raw
        ));
    }
    if !ptr.starts_with('/') {
        return ClaimRef::Invalid(format!(
            "`{}` 的路径必须以 `/` 开头（应写成 {}#/accuracy）",
            raw, file
        ));
    }
    let path: Vec<String> = ptr[1..].split('/').map(unescape).collect();
    if path.iter().any(|s| s.is_empty()) {
        return ClaimRef::Invalid(format!("`{}` 的路径里有空段", raw));
    }
    ClaimRef::Scoped {
        file: file.to_string(),
        path,
    }
}

/// JSON Pointer 的两个转义：`~1` → `/`，`~0` → `~`（先 `~1` 再 `~0`）。
fn unescape(seg: &str) -> String {
    seg.replace("~1", "/").replace("~0", "~")
}

fn tolerance(want: f64) -> f64 {
    (want.abs() * 1e-3).max(1e-9)
}

fn path_str(path: &[String]) -> String {
    path.join("/")
}

/// 在**指定的一个产物文件**里按路径取唯一一个数值。
///
/// 与旧实现的区别：不去别处找、不降级。文件缺失、路径缺失、下标越界、
/// 取到的不是数值，都返回错误。
fn resolve_scoped(artifacts: &Path, file: &str, path: &[String]) -> Result<f64, String> {
    if file.contains('/') || file.contains('\\') || file == "." || file == ".." {
        return Err(format!(
            "产物文件名 `{}` 不能带目录（只支持 artifacts/ 下的顶层 .json）",
            file
        ));
    }
    if !file.ends_with(".json") {
        return Err(format!("产物 `{}` 不是 .json", file));
    }
    let p = artifacts.join(file);
    let text = std::fs::read_to_string(&p).map_err(|_| format!("产物 {} 不在包里", file))?;
    let doc = ui_core::json::parse(&text).map_err(|e| format!("{} 不是合法 JSON：{}", file, e))?;

    let mut cur = &doc;
    let mut walked: Vec<String> = Vec::new();
    for seg in path {
        match cur {
            Json::Obj(items) => {
                cur = items
                    .iter()
                    .find(|(k, _)| k == seg)
                    .map(|(_, v)| v)
                    .ok_or_else(|| {
                        format!(
                            "路径 {}/{} 不存在（在 {} 处缺字段 `{}`）",
                            file,
                            path_str(path),
                            if walked.is_empty() {
                                "根".to_string()
                            } else {
                                path_str(&walked)
                            },
                            seg
                        )
                    })?;
            }
            Json::Arr(items) => {
                let idx: usize = seg.parse().map_err(|_| {
                    format!("{}/{} 里 `{}` 不是数组下标", file, path_str(path), seg)
                })?;
                cur = items.get(idx).ok_or_else(|| {
                    format!(
                        "{}/{} 下标越界（长度 {}）",
                        file,
                        path_str(path),
                        items.len()
                    )
                })?;
            }
            _ => {
                return Err(format!(
                    "{}/{} 在 `{}` 处断了：它不是对象或数组",
                    file,
                    path_str(path),
                    path_str(&walked)
                ))
            }
        }
        walked.push(seg.clone());
    }
    cur.as_f64()
        .ok_or_else(|| format!("{}/{} 不是数值", file, path_str(path)))
}

/// 产物里的所有数值叶子。
///
/// 键 = `文件名#/路径/段/下标`，例如 `metrics.json#/history/0/score`。
/// 和旧实现的两点不同：**数组按下标展开**；**键里带文件名**，多个文件同名
/// 字段不再互相覆盖（旧实现只留最后一个，`BTreeMap::insert` 覆盖）。
pub fn numeric_leaves(dir: &Path) -> BTreeMap<String, f64> {
    fn walk(value: &Json, path: &mut Vec<String>, out: &mut BTreeMap<String, f64>) {
        match value {
            Json::Num(n) => {
                out.insert(path.join("/"), *n);
            }
            Json::Obj(items) => {
                for (k, v) in items {
                    path.push(k.clone());
                    walk(v, path, out);
                    path.pop();
                }
            }
            Json::Arr(items) => {
                for (i, v) in items.iter().enumerate() {
                    path.push(i.to_string());
                    walk(v, path, out);
                    path.pop();
                }
            }
            _ => {}
        }
    }
    let mut out = BTreeMap::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().map(|e| e == "json").unwrap_or(false) {
                if let Ok(text) = std::fs::read_to_string(&p) {
                    if let Ok(v) = ui_core::json::parse(&text) {
                        let name = entry.file_name().to_string_lossy().to_string();
                        // 第一段就是文件名，后面用 `/` 接字段；键形如 `a.json#/accuracy`。
                        walk(&v, &mut vec![format!("{}#", name)], &mut out);
                    }
                }
            }
        }
    }
    out
}

/// 结论对账：每条 claim 要么绑定到唯一路径，要么显式降级为旧格式全局匹配。
///
/// claim 的绑定串优先取 `path` 字段；没有 `path` 时把 `name` 当绑定串
/// （所以 `--name a.json#/accuracy` 直接可用）。给了 `path` 却写成旧格式
/// 视为错误——显式路径字段写错不该悄悄变宽松。
pub fn verify_claims(artifacts: &Path, doc: &Json) -> ClaimReport {
    let mut rep = ClaimReport::default();
    let leaves = numeric_leaves(artifacts);

    for c in doc.arr_at("claims").cloned().unwrap_or_default() {
        rep.total += 1;
        let name = c.str_at("name").unwrap_or("").to_string();
        let label = if name.is_empty() {
            "<未命名>".to_string()
        } else {
            name.clone()
        };
        let want = c
            .get("value")
            .and_then(|v| v.as_f64())
            .unwrap_or(f64::NAN);

        if !want.is_finite() {
            let detail = "value 不是数值".to_string();
            rep.issues.push(format!("结论 {} 的 value 不是数值", label));
            rep.claims.push(ClaimOutcome {
                name: label,
                want,
                ok: false,
                loose: false,
                bound: None,
                detail,
            });
            continue;
        }

        let explicit = c
            .str_at("path")
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let binding: String = explicit
            .map(str::to_string)
            .unwrap_or_else(|| label.clone());
        let parsed = parse_claim_ref(&binding);
        // `path` 字段一旦写了就必须是完整绑定，不允许写成旧格式博取全局匹配。
        let parsed = match (explicit, parsed) {
            (Some(_), ClaimRef::Loose) => ClaimRef::Invalid(format!(
                "`path` 字段必须是 文件.json#/路径，不能只写 `{}`",
                binding
            )),
            (_, p) => p,
        };

        match parsed {
            ClaimRef::Loose => {
                rep.loose = true;
                let tol = tolerance(want);
                let hit = leaves.values().any(|v| (v - want).abs() <= tol);
                rep.warnings.push(format!(
                    "⚠ 未绑定路径，按全局匹配：{} = {}（建议写成 metrics.json#/accuracy）",
                    label, want
                ));
                if hit {
                    rep.ok += 1;
                    rep.claims.push(ClaimOutcome {
                        name: label,
                        want,
                        ok: true,
                        loose: true,
                        bound: None,
                        detail: format!("全局匹配命中（容差 ±{}，任意产物均可）", tol),
                    });
                } else {
                    rep.issues.push(format!(
                        "结论 {} = {} 在产物里找不到对应数值（未绑定路径，全局匹配）",
                        label, want
                    ));
                    rep.claims.push(ClaimOutcome {
                        name: label,
                        want,
                        ok: false,
                        loose: true,
                        bound: None,
                        detail: format!("全局匹配未命中（容差 ±{}）", tol),
                    });
                }
            }
            ClaimRef::Invalid(why) => {
                rep.issues
                    .push(format!("结论 {} = {} 的绑定无效：{}", label, want, why));
                rep.claims.push(ClaimOutcome {
                    name: label,
                    want,
                    ok: false,
                    loose: false,
                    bound: Some(binding),
                    detail: why,
                });
            }
            ClaimRef::Scoped { file, path } => {
                let bound = format!("{}#/{}", file, path_str(&path));
                match resolve_scoped(artifacts, &file, &path) {
                    Ok(got) => {
                        let tol = tolerance(want);
                        if (got - want).abs() <= tol {
                            rep.ok += 1;
                            rep.claims.push(ClaimOutcome {
                                name: label,
                                want,
                                ok: true,
                                loose: false,
                                bound: Some(bound),
                                detail: format!("命中 {} = {}", file, got),
                            });
                        } else {
                            rep.issues.push(format!(
                                "结论 {} = {} 未绑定到 {}：实际是 {}（容差 ±{}）",
                                label, want, bound, got, tol
                            ));
                            rep.claims.push(ClaimOutcome {
                                name: label,
                                want,
                                ok: false,
                                loose: false,
                                bound: Some(bound),
                                detail: format!("实际是 {}（容差 ±{}）", got, tol),
                            });
                        }
                    }
                    Err(why) => {
                        rep.issues.push(format!(
                            "结论 {} = {} 未绑定到 {}：{}",
                            label, want, bound, why
                        ));
                        rep.claims.push(ClaimOutcome {
                            name: label,
                            want,
                            ok: false,
                            loose: false,
                            bound: Some(bound),
                            detail: why,
                        });
                    }
                }
            }
        }
    }
    rep
}

pub struct Verify {
    pub ok: bool,
    pub artifacts: usize,
    pub inputs: usize,
    pub claims_ok: usize,
    pub claims_total: usize,
    pub journal_ok: bool,
    /// 有 claim 走了旧格式全局匹配（`--json` 的 `loose` 字段）。
    pub loose: bool,
    /// 不导致失败、但必须让人看见的提示。
    pub warnings: Vec<String>,
    pub issues: Vec<String>,
    pub claims: Vec<ClaimOutcome>,
}

fn sha256_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(sha256::hex(&bytes))
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
        loose: false,
        warnings: Vec::new(),
        issues: Vec::new(),
        claims: Vec::new(),
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
            let rep = verify_claims(&dir.join("artifacts"), &doc);
            out.claims_ok = rep.ok;
            out.claims_total = rep.total;
            out.loose = rep.loose;
            out.warnings.extend(rep.warnings);
            out.issues.extend(rep.issues);
            out.claims = rep.claims;
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
            ("claimsOk", Json::from(report.claims_ok)),
            ("loose", Json::Bool(report.loose)),
            (
                "warning",
                Json::from(report.warnings.first().cloned().unwrap_or_default()),
            ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ui-lab-unit-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("artifacts")).unwrap();
        d
    }

    /// 造一个自洽的包：manifest 哈希对得上、没有 journal（= 空日志）、claims 原样写入。
    fn bundle(name: &str, artifacts: &[(&str, &str)], claims: &str) -> PathBuf {
        let d = scratch(name);
        let mut manifest = String::new();
        for (file, body) in artifacts {
            fs::write(d.join("artifacts").join(file), body).unwrap();
            manifest.push_str(&format!(
                "{{\"path\":\"{}\",\"sha256\":\"{}\",\"bytes\":{}}}\n",
                file,
                ui_core::sha256::hex(body.as_bytes()),
                body.len()
            ));
        }
        fs::write(d.join("manifest.jsonl"), manifest).unwrap();
        fs::write(d.join("claims.json"), claims).unwrap();
        d
    }

    fn json_of(d: &Path) -> Json {
        let text = fs::read_to_string(d.join("claims.json")).unwrap();
        ui_core::json::parse(&text).unwrap()
    }

    fn details(rep: &ClaimReport) -> String {
        rep.claims
            .iter()
            .map(|c| c.detail.clone())
            .collect::<Vec<_>>()
            .join(" | ")
    }

    #[test]
    fn numeric_leaves_expands_arrays_and_keeps_files_apart() {
        let d = bundle(
            "leaves",
            &[
                ("a.json", "{\"accuracy\":0.7}"),
                (
                    "metrics.json",
                    "{\"history\":[{\"score\":0.42},{\"score\":0.91}]}",
                ),
            ],
            "{\"claims\":[]}",
        );
        let leaves = numeric_leaves(&d.join("artifacts"));
        // 旧实现忽略数组：这两个键根本不存在。
        assert_eq!(leaves.get("metrics.json#/history/0/score"), Some(&0.42));
        assert_eq!(leaves.get("metrics.json#/history/1/score"), Some(&0.91));
        // 旧实现只留一个 "accuracy"；现在两个文件各有一份，不互相覆盖。
        assert_eq!(leaves.get("a.json#/accuracy"), Some(&0.7));
        assert_eq!(leaves.len(), 3, "键必须带文件名且数组按下标展开");
    }

    #[test]
    fn scoped_claim_only_accepts_the_named_file() {
        let d = bundle(
            "scoped",
            &[
                ("a.json", "{\"accuracy\":0.901}"),
                // b.json 里有一个 0.9 的巧合值：旧实现正是靠它“通过”的。
                ("b.json", "{\"accuracy\":0.5,\"seed\":0.9}"),
            ],
            "{\"claims\":[
                {\"name\":\"a.json#/accuracy\",\"value\":0.9},
                {\"name\":\"a.json#/accuracy\",\"value\":0.901},
                {\"name\":\"b.json#/accuracy\",\"value\":0.5}
            ]}",
        );
        let rep = verify_claims(&d.join("artifacts"), &json_of(&d));
        assert_eq!(rep.total, 3);
        assert_eq!(
            rep.ok,
            2,
            "只认绑定的文件；旧实现会把 b.json 的 seed=0.9 当成 a.json 的 accuracy：{}",
            details(&rep)
        );
        assert_eq!(rep.issues.len(), 1, "{:?}", rep.issues);
        assert!(
            rep.issues[0].contains("a.json#/accuracy"),
            "失败信息必须点名绑定的路径：{}",
            rep.issues[0]
        );
        assert!(
            rep.issues[0].contains("实际是 0.901"),
            "必须报出绑定文件里的真实值：{}",
            rep.issues[0]
        );
        assert!(!rep.loose, "带 # 的新格式不该是 loose");
        assert!(rep.warnings.is_empty());
        let claims = rep.claims_json();
        assert_eq!(claims.as_arr().map(Vec::len), Some(3));
        assert_eq!(
            rep.claims[0].bound.as_deref(),
            Some("a.json#/accuracy"),
            "outcome 必须记住绑定"
        );
    }

    #[test]
    fn array_path_claims_are_resolved() {
        let d = bundle(
            "array-claim",
            &[(
                "metrics.json",
                "{\"history\":[{\"score\":0.42},{\"score\":0.91}]}",
            )],
            "{\"claims\":[
                {\"name\":\"metrics.json#/history/0/score\",\"value\":0.42},
                {\"name\":\"metrics.json#/history/1/score\",\"value\":0.42}
            ]}",
        );
        let rep = verify_claims(&d.join("artifacts"), &json_of(&d));
        // 旧实现完全不看数组：第一条会失败（这里必须通过）。
        assert_eq!(rep.ok, 1, "{}", details(&rep));
        assert_eq!(rep.issues.len(), 1);
        assert!(
            rep.issues[0].contains("metrics.json#/history/1/score"),
            "{}",
            rep.issues[0]
        );
        assert!(rep.claims[0].ok);
        assert_eq!(
            rep.claims[0].bound.as_deref(),
            Some("metrics.json#/history/0/score")
        );
    }

    #[test]
    fn missing_file_or_field_never_falls_back_to_global_match() {
        let d = bundle(
            "no-fallback",
            &[("a.json", "{\"accuracy\":0.9}")],
            "{\"claims\":[
                {\"name\":\"a.json#/nope\",\"value\":0.9},
                {\"name\":\"gone.json#/accuracy\",\"value\":0.9}
            ]}",
        );
        let rep = verify_claims(&d.join("artifacts"), &json_of(&d));
        // 旧实现对这两条都会在全局 leaves 里找到 0.9 而放行。
        assert_eq!(rep.ok, 0, "{}", details(&rep));
        assert_eq!(rep.issues.len(), 2, "{:?}", rep.issues);
        assert!(rep.issues[0].contains("不存在"), "{}", rep.issues[0]);
        assert!(rep.issues[1].contains("不在包里"), "{}", rep.issues[1]);
        assert!(!rep.loose);
    }

    #[test]
    fn path_without_a_file_name_is_rejected_not_downgraded() {
        let d = bundle(
            "no-file",
            &[("a.json", "{\"accuracy\":0.9}")],
            "{\"claims\":[
                {\"name\":\"#/accuracy\",\"value\":0.9},
                {\"name\":\"accuracy\",\"value\":0.9,\"path\":\"#/accuracy\"},
                {\"name\":\"accuracy2\",\"value\":0.9,\"path\":\"accuracy2\"}
            ]}",
        );
        let rep = verify_claims(&d.join("artifacts"), &json_of(&d));
        // 旧实现根本不解析 name/path，这三条都会因为全局有 0.9 而通过。
        assert_eq!(rep.ok, 0, "{}", details(&rep));
        assert_eq!(rep.issues.len(), 3, "{:?}", rep.issues);
        for issue in &rep.issues {
            assert!(
                issue.contains("没写产物文件名") || issue.contains("必须是 文件.json#/路径"),
                "{}",
                issue
            );
        }
        assert!(!rep.loose, "写坏的显式路径不能算 loose（那是另一种失败）");
    }

    #[test]
    fn legacy_claim_passes_but_is_explicitly_marked_loose() {
        let d = bundle(
            "legacy",
            &[("metrics.json", "{\"accuracy\":0.72}")],
            "{\"claims\":[{\"name\":\"accuracy\",\"value\":0.72}]}",
        );
        let rep = verify_claims(&d.join("artifacts"), &json_of(&d));
        assert_eq!(rep.ok, 1);
        assert!(rep.loose, "旧格式必须标成 loose");
        assert_eq!(rep.warnings.len(), 1);
        assert!(
            rep.warnings[0].contains("⚠ 未绑定路径，按全局匹配"),
            "{}",
            rep.warnings[0]
        );
        assert!(rep.claims[0].loose);
        assert_eq!(rep.claims[0].bound, None);
        assert_eq!(rep.claims_json().as_arr().map(Vec::len), Some(1));

        // 整包校验：警告不能让包失败，但 loose 要透出来。
        let v = verify_bundle(&d, None);
        assert!(v.ok, "{:?}", v.issues);
        assert_eq!(v.claims_ok, 1);
        assert!(v.loose);
        assert_eq!(v.warnings.len(), 1);
    }

    #[test]
    fn legacy_claim_still_fails_when_no_value_matches() {
        let d = bundle(
            "legacy-bad",
            &[("metrics.json", "{\"accuracy\":0.72}")],
            "{\"claims\":[{\"name\":\"accuracy\",\"value\":0.99}]}",
        );
        let v = verify_bundle(&d, None);
        assert!(!v.ok);
        assert!(v.loose, "旧格式即使失败也要标 loose");
        assert!(
            v.issues.iter().any(|i| i.contains("找不到对应数值")),
            "{:?}",
            v.issues
        );
    }
}
