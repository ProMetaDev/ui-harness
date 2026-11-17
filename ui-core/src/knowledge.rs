//! 知识卡：产物化 + 来源绑定（K1），任务前预取 + 留痕（K2）。
//!
//! 依据 [docs/29-qoder-knowledge.md](../../docs/29-qoder-knowledge.md) 第 6.1 / 6.2 节：
//! Qoder 的公开材料里没有「某次回答用了哪张卡、那张卡当时对应哪个 commit、谁确认过」
//! 的证据链，而我们的 seq + 哈希链 + 血缘正好能补上。所以这里不加向量库，只加：
//!
//! 1. **卡片文件**（`docs/knowledge/*.md`，YAML front-matter，零依赖）：
//!    `docs/knowledge/example-module.md` 是格式示例；
//! 2. **来源绑定**：`source_sha` = 「`touches` 命中的文件集」的确定性哈希，
//!    `check` 重算一遍就能回答「这张卡还对应现在的工作区吗」；
//! 3. **确定性预取**：任务文本里的路径 / 词命中哪张卡、为什么命中（`reason`），
//!    命中与未命中都可复现——不做 embedding，不做自动改写。
//!
//! 明确不做的事（与 [docs/29-qoder-knowledge.md](../../docs/29-qoder-knowledge.md)
//! 第 7 节的 N1/N2/N4 对齐）：不把代码上传做向量化、不自动生成知识并当成规范、
//! 不用云端版本覆盖本地卡片。人确认位 `confirmed_by` 是必填语义。

use std::fs;
use std::path::{Path, PathBuf};

use crate::json::Json;
use crate::policy::glob_match;
use crate::sha256;

/// 知识卡目录，相对工作区。
pub const DIR: &str = "docs/knowledge";

/// 遍历工作区时跳过的目录：日志、产物、依赖、密钥。卡片 `touches` 不覆盖它们。
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".ui",
    ".cargo",
    "target",
    "node_modules",
    "out",
    "dist",
    "secrets",
];

/// 一张知识卡。字段与 front-matter 一一对应，不认识的键被忽略（不是错误）。
#[derive(Debug, Clone)]
pub struct Card {
    pub id: String,
    pub title: String,
    /// 绑定到哪一份源码：`touches` 命中文件集的哈希。空 = 未绑定。
    pub source_sha: String,
    pub generated_at: String,
    pub generated_by_seq: Option<u64>,
    /// `human` | `none`（人确认位：没有它就不该当规范用）
    pub confirmed_by: String,
    /// 这张卡描述哪些文件（glob，`**` 跨目录、`*` 不跨，大小写不敏感）
    pub touches: Vec<String>,
    pub body: String,
    pub path: PathBuf,
}

impl Card {
    /// 正文的 token 估算：字节数 / 4。
    /// `knowledge/use` 的 `budget_tokens` 用这个值——注入的是正文，不是 front-matter。
    pub fn budget_tokens(&self) -> u64 {
        (self.body.len() as u64) / 4
    }

    pub fn touches_json(&self) -> Json {
        Json::Arr(self.touches.iter().map(|t| Json::from(t.as_str())).collect())
    }

    /// 便于打印的文件名（`docs/knowledge/mod-pay.md`）。卡片之间按 id 区分，重复 id 只留第一张。
    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    }
}

/// 解析一张卡。**没有 `id` 的 .md 不是卡**（README 之类会被忽略，而不是变成无名卡）。
pub fn parse(text: &str, path: &Path) -> Option<Card> {
    let text = text.trim_start_matches('\u{feff}').trim_start();
    let rest = text.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let front = &rest[..end];
    let body = rest[end + 4..]
        .trim_start_matches(|c| c == '\r' || c == '\n')
        .to_string();

    let mut card = Card {
        id: String::new(),
        title: String::new(),
        source_sha: String::new(),
        generated_at: String::new(),
        generated_by_seq: None,
        confirmed_by: String::new(),
        touches: Vec::new(),
        body,
        path: path.to_path_buf(),
    };

    for line in front.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // 只支持 `key: value` 这一种形状：极简解析，不引 yaml。
        let (key, value) = match line.split_once(':') {
            Some(kv) => kv,
            None => continue,
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        match key.as_str() {
            "id" => card.id = unquote(value),
            "title" => card.title = unquote(value),
            "source_sha" => card.source_sha = unquote(value),
            "generated_at" => card.generated_at = unquote(value),
            "generated_by_seq" => card.generated_by_seq = unquote(value).parse::<u64>().ok(),
            "confirmed_by" => card.confirmed_by = unquote(value),
            "touches" => card.touches = parse_list(value),
            _ => {}
        }
    }

    if card.id.is_empty() {
        return None;
    }
    if card.title.is_empty() {
        card.title = card.id.clone();
    }
    if card.confirmed_by.is_empty() {
        // 缺省不是 human：没有明说「人确认过」就不算确认过。
        card.confirmed_by = "none".to_string();
    }
    Some(card)
}

fn unquote(value: &str) -> String {
    let v = value.trim();
    if v.len() >= 2 {
        let bytes = v.as_bytes();
        let (first, last) = (bytes[0], bytes[v.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return v[1..v.len() - 1].to_string();
        }
    }
    v.to_string()
}

/// `["a","b"]` / `[a, b]` / `a, b` → `["a", "b"]`。引号内允许逗号。
pub fn parse_list(value: &str) -> Vec<String> {
    let v = value.trim();
    let inner = v
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(v);
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in inner.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    current.push(c);
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                ',' => {
                    let item = current.trim();
                    if !item.is_empty() {
                        out.push(item.to_string());
                    }
                    current.clear();
                }
                _ => current.push(c),
            },
        }
    }
    let item = current.trim();
    if !item.is_empty() {
        out.push(item.to_string());
    }
    out
}

/// `<ws>/docs/knowledge/*.md`，按 id 排序（同 id 只留第一张，先到的文件胜出）。
pub fn discover(workspace: &Path) -> Vec<Card> {
    let dir = workspace.join(DIR);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut cards: Vec<Card> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if let Some(card) = parse(&text, &path) {
            if !cards.iter().any(|c| c.id == card.id) {
                cards.push(card);
            }
        }
    }
    cards.sort_by(|a, b| a.id.cmp(&b.id));
    cards
}

pub fn find(workspace: &Path, id: &str) -> Option<Card> {
    discover(workspace).into_iter().find(|c| c.id == id)
}

/// `touches` 命中的文件集 + 它们的确定性哈希。
#[derive(Debug, Clone, PartialEq)]
pub struct SourceState {
    /// 相对路径（`/` 分隔），已排序去重
    pub files: Vec<String>,
    /// 没有命中文件时为 None：`None` 与「哈希算出来是 0」是两件事
    pub digest: Option<String>,
}

/// 卡片来源的哈希规则（写在文档里也要能人工复现）：
///
/// ```text
/// feed = 对每个命中文件（按相对路径排序）:
///         相对路径 "\n" sha256(文件字节) hex "\n"
/// digest = sha256(feed) hex
/// ```
///
/// 为什么不是「把所有字节连起来」：连起来分不清文件边界，
/// 两个文件各加一个字节 vs 一个文件加两个字节会撞成同一个哈希。
pub fn source_state(workspace: &Path, touches: &[String]) -> SourceState {
    let mut files: Vec<String> = Vec::new();
    if !touches.is_empty() {
        walk(workspace, workspace, &mut |rel: &str| {
            if touches.iter().any(|t| glob_match(t, rel)) {
                files.push(rel.to_string());
            }
        });
    }
    files.sort();
    files.dedup();
    if files.is_empty() {
        return SourceState {
            files,
            digest: None,
        };
    }

    let mut feed = String::new();
    for rel in &files {
        feed.push_str(rel);
        feed.push('\n');
        match fs::read(workspace.join(rel)) {
            Ok(bytes) => feed.push_str(&sha256::hex(&bytes)),
            // 读不到就不假装没变：这一项参与哈希，任何恢复都会让哈希变回原值
            Err(_) => feed.push_str("unreadable"),
        }
        feed.push('\n');
    }
    SourceState {
        files,
        digest: Some(sha256::hex(feed.as_bytes())),
    }
}

/// `source_sha` 的唯一算法入口（CLI、测试、将来的 emit 都走这里）。
pub fn source_digest(workspace: &Path, touches: &[String]) -> Option<String> {
    source_state(workspace, touches).digest
}

/// 卡片与当前工作区的一致状态。
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// 卡上的 sha 与重算结果一致
    Ok { files: usize },
    /// 卡上写了 sha，但对不上（文件变了 / 文件没了 / 卡上写错了）
    Stale {
        expected: String,
        actual: Option<String>,
        files: Vec<String>,
    },
    /// 卡上没写 sha：不是「一致」，是「还没绑定」。默认不算失败，`--strict` 下算失败。
    Unbound { files: Vec<String> },
}

pub fn check(workspace: &Path, card: &Card) -> Status {
    let state = source_state(workspace, &card.touches);
    let expected = card.source_sha.trim();
    if expected.is_empty() {
        return Status::Unbound {
            files: state.files,
        };
    }
    match &state.digest {
        Some(actual) if hash_matches(expected, actual) => Status::Ok {
            files: state.files.len(),
        },
        _ => Status::Stale {
            expected: expected.to_string(),
            actual: state.digest,
            files: state.files,
        },
    }
}

/// 卡上可以写完整 64 位 sha256，也可以写它的前缀（例如 40 位，见 doc 里的示例格式）。
/// 少于 16 位（64 bit）不算数——前缀太短就没有区分度了。
fn hash_matches(claimed: &str, actual: &str) -> bool {
    let claimed = claimed.trim().to_ascii_lowercase();
    if claimed.len() < 16 || !claimed.chars().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }
    actual.to_ascii_lowercase().starts_with(&claimed)
}

/// 一次命中的结果：哪张卡 + 为什么命中（原样写进 `knowledge/use.reason`）。
#[derive(Debug, Clone)]
pub struct Hit {
    pub card: Card,
    pub reason: String,
}

/// 任务前的**确定性**预取：按 id 排序扫一遍，命中就收，最多 `max` 张。
///
/// 「确定性」是刻意的：同一段任务文本 + 同一批卡片 → 永远同样的命中集合与顺序。
/// 未命中就是空 vec，调用方据此**不写事件、不改提示词**。
pub fn prefetch(workspace: &Path, task: &str, max: usize) -> Vec<Hit> {
    let mut hits: Vec<Hit> = Vec::new();
    for card in discover(workspace) {
        if hits.len() >= max {
            break;
        }
        if let Some(reason) = match_reason(&card, task) {
            hits.push(Hit { card, reason });
        }
    }
    hits
}

/// 命中规则（三步，任意一步命中即返回）：
///
/// 1. 任务文本里出现的**具体路径**落在卡的某个 `touches` glob 上 → `path-match:<路径>`；
/// 2. 卡的某个 `touches` 去掉通配符后的字面前缀（≥3 字符）出现在任务文本里
///    → `path-match:<前缀>`；
/// 3. 卡的 `id` 或 `title` 出现在任务文本里 → `keyword-match:<id|title>`。
///
/// 只做子串 / glob 匹配：没有相似度、没有 embedding，所以「为什么命中」写得出来。
pub fn match_reason(card: &Card, task: &str) -> Option<String> {
    let hay = task.to_lowercase();

    for token in path_tokens(task) {
        if card.touches.iter().any(|t| glob_match(t, token)) {
            return Some(format!("path-match:{}", token));
        }
    }

    for touch in &card.touches {
        let needle = literal_prefix(touch);
        // 卡片里的 glob 可能带大写（Windows 上很常见），任务文本按小写比：
        // 与 `glob_match` 的大小写不敏感语义保持一致。
        if needle.chars().count() >= 3 && hay.contains(&needle.to_lowercase()) {
            return Some(format!("path-match:{}", needle));
        }
    }

    if !card.id.is_empty() && hay.contains(&card.id.to_lowercase()) {
        return Some(format!("keyword-match:{}", card.id));
    }
    let title = card.title.trim();
    if !title.is_empty() && hay.contains(&title.to_lowercase()) {
        return Some(format!("keyword-match:{}", title));
    }
    None
}

/// 从任务文本里挑出「像路径」的词（含 `/`，去掉首尾的标点）。
fn path_tokens(task: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    let mut start: Option<usize> = None;
    for (index, c) in task.char_indices() {
        let keep = c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-');
        if keep {
            if start.is_none() {
                start = Some(index);
            }
        } else if let Some(s) = start.take() {
            push_token(&mut out, &task[s..index]);
        }
    }
    if let Some(s) = start {
        push_token(&mut out, &task[s..]);
    }
    out
}

fn push_token<'a>(out: &mut Vec<&'a str>, token: &'a str) {
    let token = token
        .trim_start_matches(|c| c == '.' || c == '/')
        .trim_end_matches(|c| c == '.' || c == ',' || c == ')' || c == ':');
    if token.len() >= 3 && token.contains('/') {
        out.push(token);
    }
}

/// `src/pay/**` → `src/pay`；`src/billing/*.rs` → `src/billing`。
fn literal_prefix(glob: &str) -> String {
    let cut = glob.find(|c| c == '*' || c == '?').unwrap_or(glob.len());
    glob[..cut].trim_end_matches('/').to_string()
}

/// 拼进首个提示词前的正文块。命中原因与来源都写进去——
/// 模型看到的是「哪张卡、凭什么命中、对应哪份源码」，而不是一堆无从追溯的上下文。
pub fn prefetch_block(hits: &[Hit]) -> String {
    let mut out = String::new();
    out.push_str("## 预取的知识卡（docs/knowledge，确定性匹配）\n");
    out.push_str(
        "下面每张卡都已在 journal 里留下 knowledge/use（含来源 sha 与命中原因）。\
         卡片是项目知识，不是不可质疑的规范：与代码冲突时以代码为准，并在回答里说明。\n",
    );
    for hit in hits {
        let card = &hit.card;
        out.push_str(&format!("\n### [{}] {}\n", card.id, card.title));
        out.push_str(&format!("- 命中原因: {}\n", hit.reason));
        out.push_str(&format!(
            "- source_sha: {}\n",
            if card.source_sha.is_empty() {
                "（未绑定）".to_string()
            } else {
                card.source_sha.clone()
            }
        ));
        out.push_str(&format!("- confirmed_by: {}\n", card.confirmed_by));
        out.push_str(&format!(
            "- touches: {}\n\n",
            if card.touches.is_empty() {
                "（无）".to_string()
            } else {
                card.touches.join(", ")
            }
        ));
        out.push_str(card.body.trim());
        out.push('\n');
    }
    out
}

/// `knowledge/use` 的 payload（契约：只有这五个字段，不加契约大类）。
pub fn use_payload(card: &Card, reason: &str, score: Option<f64>) -> Json {
    Json::obj(vec![
        ("card", Json::from(card.id.as_str())),
        ("source_sha", Json::from(card.source_sha.as_str())),
        ("reason", Json::from(reason)),
        ("budget_tokens", Json::from(card.budget_tokens())),
        ("score", score.map(Json::n).unwrap_or(Json::Null)),
    ])
}

/// `knowledge/emit` 的 payload（契约：card / source_sha / generated_by_seq / confirmed_by / touches）。
pub fn emit_payload(card: &Card, generated_by_seq: u64, confirmed_by: &str) -> Json {
    Json::obj(vec![
        ("card", Json::from(card.id.as_str())),
        ("source_sha", Json::from(card.source_sha.as_str())),
        ("generated_by_seq", Json::from(generated_by_seq)),
        ("confirmed_by", Json::from(confirmed_by)),
        ("touches", card.touches_json()),
    ])
}

fn walk(root: &Path, dir: &Path, visit: &mut impl FnMut(&str)) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        // 不跟符号链接：卡片哈希必须只由工作区里的真实文件决定（否则可以指到 /etc）
        let kind = match entry.file_type() {
            Ok(k) => k,
            Err(_) => continue,
        };
        if kind.is_symlink() {
            continue;
        }
        let path = entry.path();
        if kind.is_dir() {
            walk(root, &path, visit);
        } else if kind.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                visit(&rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ui-knowledge-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_card(ws: &Path, file: &str, front: &str, body: &str) -> Card {
        let dir = ws.join(DIR);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file);
        fs::write(&path, format!("---\n{}\n---\n{}\n", front, body)).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        parse(&text, &path).expect("front-matter 里必须有 id")
    }

    #[test]
    fn parses_frontmatter_with_quoted_lists() {
        let card = parse(
            "---\nid: mod-payment\ntitle: 支付模块\nsource_sha: 9f2c\ngenerated_at: 2026-11-17T20:00:00Z\ngenerated_by_seq: 12\nconfirmed_by: human\ntouches: [\"src/pay/**\", \"src/billing/*.rs\"]\n---\n正文若干行\n",
            Path::new("/x/docs/knowledge/mod-payment.md"),
        )
        .unwrap();
        assert_eq!(card.id, "mod-payment");
        assert_eq!(card.title, "支付模块");
        assert_eq!(card.source_sha, "9f2c");
        assert_eq!(card.generated_by_seq, Some(12));
        assert_eq!(card.confirmed_by, "human");
        assert_eq!(card.touches, vec!["src/pay/**", "src/billing/*.rs"]);
        assert_eq!(card.body.trim(), "正文若干行");
    }

    #[test]
    fn markdown_without_an_id_is_not_a_card() {
        assert!(parse("# 说明\n没有 front-matter", Path::new("/x/README.md")).is_none());
        assert!(parse("---\ntitle: 没有 id\n---\n正文", Path::new("/x/a.md")).is_none());
    }

    #[test]
    fn missing_confirmed_by_is_not_human() {
        let card = parse("---\nid: a\n---\n正文\n", Path::new("/x/a.md")).unwrap();
        assert_eq!(card.confirmed_by, "none");
        assert_eq!(card.title, "a");
    }

    #[test]
    fn digest_follows_the_touched_files() {
        let ws = tmp("digest");
        fs::create_dir_all(ws.join("src/pay")).unwrap();
        fs::write(ws.join("src/pay/x.rs"), "fn a() {}\n").unwrap();
        fs::write(ws.join("src/pay/y.rs"), "fn b() {}\n").unwrap();

        let touches = vec!["src/pay/**".to_string()];
        let first = source_state(&ws, &touches);
        assert_eq!(first.files, vec!["src/pay/x.rs", "src/pay/y.rs"]);
        assert_eq!(first.digest.as_ref().map(|d| d.len()), Some(64));

        // 内容变了 → 哈希必须变
        fs::write(ws.join("src/pay/x.rs"), "fn a() { /* 改 */ }\n").unwrap();
        assert_ne!(source_state(&ws, &touches).digest, first.digest);

        // 加一个文件 → 哈希也必须变（文件集参与哈希）
        fs::write(ws.join("src/pay/z.rs"), "fn c() {}\n").unwrap();
        assert_ne!(source_state(&ws, &touches).digest, first.digest);
    }

    #[test]
    fn no_matching_file_means_no_digest() {
        let ws = tmp("nomatch");
        let state = source_state(&ws, &["src/pay/**".to_string()]);
        assert!(state.digest.is_none());
        assert!(state.files.is_empty());
    }

    #[test]
    fn check_separates_ok_stale_and_unbound() {
        let ws = tmp("status");
        fs::create_dir_all(ws.join("src/pay")).unwrap();
        fs::write(ws.join("src/pay/x.rs"), "fn a() {}\n").unwrap();
        let touches = vec!["src/pay/**".to_string()];
        let sha = source_digest(&ws, &touches).unwrap();

        let bound = write_card(
            &ws,
            "bound.md",
            &format!("id: bound\nsource_sha: {}\ntouches: [\"src/pay/**\"]", sha),
            "正文",
        );
        assert_eq!(check(&ws, &bound), Status::Ok { files: 1 });

        // 40 位前缀也应该通过（示例格式里写的就是 40 位 hex）
        let prefix = write_card(
            &ws,
            "prefix.md",
            &format!("id: prefix\nsource_sha: {}\ntouches: [\"src/pay/**\"]", &sha[..40]),
            "正文",
        );
        assert_eq!(check(&ws, &prefix), Status::Ok { files: 1 });

        let unbound = write_card(&ws, "unbound.md", "id: unbound\ntouches: [\"src/pay/**\"]", "正文");
        assert_eq!(check(&ws, &unbound), Status::Unbound { files: vec!["src/pay/x.rs".to_string()] });

        // 真的改文件内容
        fs::write(ws.join("src/pay/x.rs"), "fn a() { /* 改 */ }\n").unwrap();
        match check(&ws, &bound) {
            Status::Stale { files, actual, .. } => {
                assert_eq!(files, vec!["src/pay/x.rs".to_string()]);
                assert!(actual.is_some());
            }
            other => panic!("源变了必须 STALE，实际 {:?}", other),
        }
    }

    #[test]
    fn matches_by_path_then_by_word() {
        let card = parse(
            "---\nid: mod-payment\ntitle: 支付模块\ntouches: [\"src/pay/**\", \"src/billing/*.rs\"]\n---\n正文\n",
            Path::new("/x/mod-payment.md"),
        )
        .unwrap();

        let reason = match_reason(&card, "把 src/pay/x.rs 的退款改成两步").unwrap();
        assert_eq!(reason, "path-match:src/pay/x.rs");

        let reason = match_reason(&card, "src/billing/invoice.rs 里少了一张票").unwrap();
        assert_eq!(reason, "path-match:src/billing/invoice.rs");

        // 没有具体路径时退回字面前缀
        let reason = match_reason(&card, "src/pay 下要不要加个锁？").unwrap();
        assert_eq!(reason, "path-match:src/pay");

        // 再退回 id / 标题关键词
        assert_eq!(match_reason(&card, "支付模块为什么这么慢").unwrap(), "keyword-match:支付模块");
        assert_eq!(match_reason(&card, "问问 mod-payment 的约束").unwrap(), "keyword-match:mod-payment");

        // 不相关就是 None：未命中不产生任何事件
        assert!(match_reason(&card, "把欢迎页的文案改短一点").is_none());
        assert!(match_reason(&card, "docs/README.md 的措辞").is_none());
    }

    #[test]
    fn matching_is_case_insensitive_like_glob_match() {
        let card = parse(
            "---\nid: mod-win\ntitle: Windows 路径\ntouches: [\"Src/Pay/**\"]\n---\n正文\n",
            Path::new("/x/mod-win.md"),
        )
        .unwrap();
        assert_eq!(
            match_reason(&card, "看下 src/pay/x.rs").unwrap(),
            "path-match:src/pay/x.rs"
        );
        assert!(match_reason(&card, "看看 SRC/PAY 目录").is_some());
    }

    #[test]
    fn prefetch_is_sorted_and_capped() {
        let ws = tmp("prefetch");
        for i in 0..5 {
            write_card(
                &ws,
                &format!("c{}.md", i),
                &format!("id: mod-{}\ntitle: 模块 {}\ntouches: [\"src/pay/**\"]", i, i),
                "正文",
            );
        }
        let hits = prefetch(&ws, "src/pay/x.rs 有 bug", 3);
        assert_eq!(hits.len(), 3, "最多 3 张");
        let ids: Vec<&str> = hits.iter().map(|h| h.card.id.as_str()).collect();
        assert_eq!(ids, vec!["mod-0", "mod-1", "mod-2"], "按 id 排序，确定性");
        assert!(prefetch(&ws, "无关任务", 3).is_empty());
    }

    #[test]
    fn payloads_carry_the_evidence_fields() {
        let card = parse(
            "---\nid: mod-payment\ntitle: 支付模块\nsource_sha: abc\ntouches: [\"src/pay/**\"]\n---\n12345678",
            Path::new("/x/mod-payment.md"),
        )
        .unwrap();

        let use_json = use_payload(&card, "path-match:src/pay/x.rs", None);
        assert_eq!(use_json.str_at("card"), Some("mod-payment"));
        assert_eq!(use_json.str_at("source_sha"), Some("abc"));
        assert_eq!(use_json.str_at("reason"), Some("path-match:src/pay/x.rs"));
        assert_eq!(use_json.u64_at("budget_tokens"), Some(2), "8 字节 / 4");
        assert_eq!(use_json.get("score"), Some(&Json::Null), "确定性命中没有相似度分数");

        assert_eq!(use_payload(&card, "manual", Some(0.82)).num_at("score"), Some(0.82));

        let emit_json = emit_payload(&card, 9, "human");
        assert_eq!(emit_json.str_at("card"), Some("mod-payment"));
        assert_eq!(emit_json.str_at("confirmed_by"), Some("human"));
        assert_eq!(emit_json.u64_at("generated_by_seq"), Some(9));
        assert_eq!(
            emit_json.arr_at("touches").map(|a| a.len()),
            Some(1),
            "touches 必须进事件，否则吊销/失效找不到关系"
        );
    }

    #[test]
    fn prefetch_block_includes_body_and_reason() {
        let card = parse(
            "---\nid: mod-payment\ntitle: 支付模块\nconfirmed_by: human\ntouches: [\"src/pay/**\"]\n---\n退款只有一条路径：pay::refund",
            Path::new("/x/mod-payment.md"),
        )
        .unwrap();
        let block = prefetch_block(&[Hit {
            card,
            reason: "path-match:src/pay/x.rs".to_string(),
        }]);
        assert!(block.contains("支付模块"));
        assert!(block.contains("退款只有一条路径：pay::refund"));
        assert!(block.contains("path-match:src/pay/x.rs"));
        assert!(block.contains("confirmed_by: human"));
    }
}
