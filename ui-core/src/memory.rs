//! 跨会话记忆（P1）：显式写入 + 每轮确定性检索 + 可验证的哈希链。
//!
//! 设计依据 [docs/30-memory-design.md](../../docs/30-memory-design.md) 第 3/4/5/7/8 节。
//! 这一期只做三件事：
//!
//! 1. **存储**：`<ws>/.ui/memory.jsonl`，直接复用 [`crate::journal::Journal`]——
//!    seq/prev/hash 链、fsync、多进程锁、尾部撕裂修复全部免费继承；
//! 2. **写入**：只有 `ui remember` / `ui forget` 两条路，模型不能写；
//! 3. **读取**：每轮开始做一次零依赖 BM25 + 路径/关键词加成，top-k 进快照的
//!    `memory` 段；没命中时**不写事件、不改提示词**。
//!
//! 明确不做（P1 范围之外）：候选/自动提炼、embedding、云端、代码索引。
//! 也不把 `memory/used` 当成可变计数器：命中次数是从事件重放出来的派生值。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::budget::estimate_tokens;
use crate::json::Json;
use crate::journal::{self, Journal};
use crate::prompt::Message;
use crate::sha256;
use crate::time;

/// 相对 workspace 的记忆文件。
pub const REL_PATH: &str = ".ui/memory.jsonl";
/// P1 默认 top-k。任务书以这里为准：默认 3（设计稿早期写的是 5）。
pub const DEFAULT_TOP_K: usize = 3;
/// 单轮注入预算（tokens）。`UI_MEMORY_BUDGET` 可覆盖。
pub const DEFAULT_MAX_TOKENS: u64 = 300;
/// 正文上限，单位是字符（不是字节）。
pub const MAX_TEXT_CHARS: usize = 240;
/// 低于这个置信度不注入。
pub const MIN_CONFIDENCE: f64 = 0.30;

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// 记忆种类。字段名在事件里叫 `memKind`，不能叫 `kind`——
/// `Journal::append` 会把 payload 字段合并进事件，`kind` 已经被事件类型占用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemKind {
    Fact,
    Pref,
    Pitfall,
    Build,
}

impl MemKind {
    pub fn name(self) -> &'static str {
        match self {
            MemKind::Fact => "fact",
            MemKind::Pref => "pref",
            MemKind::Pitfall => "pitfall",
            MemKind::Build => "build",
        }
    }

    pub fn parse(value: &str) -> Option<MemKind> {
        match value.trim().to_ascii_lowercase().as_str() {
            "fact" => Some(MemKind::Fact),
            "pref" | "preference" => Some(MemKind::Pref),
            "pitfall" => Some(MemKind::Pitfall),
            "build" => Some(MemKind::Build),
            _ => None,
        }
    }

    /// 默认 TTL（毫秒增量）。0 = 永不过期。设计稿 §3.6。
    pub fn default_ttl_ms(self) -> u64 {
        match self {
            MemKind::Fact => 90 * DAY_MS,
            MemKind::Pref => 0,
            MemKind::Pitfall => 365 * DAY_MS,
            MemKind::Build => 180 * DAY_MS,
        }
    }

    /// 类型加成：纠错和偏好比普通事实更该被看见。
    pub fn score_bonus(self) -> f64 {
        match self {
            MemKind::Pitfall | MemKind::Pref => 0.2,
            MemKind::Build => 0.1,
            MemKind::Fact => 0.0,
        }
    }
}

/// 来源类型。P1 只有人写（`cli` 语义叫 `human`），其余留给第二期。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Human,
    Model,
    Repair,
    Delegation,
}

impl SourceKind {
    pub fn name(self) -> &'static str {
        match self {
            SourceKind::Human => "human",
            SourceKind::Model => "model",
            SourceKind::Repair => "repair",
            SourceKind::Delegation => "delegation",
        }
    }

    pub fn parse(value: &str) -> Option<SourceKind> {
        match value.trim().to_ascii_lowercase().as_str() {
            "human" | "cli" => Some(SourceKind::Human),
            "model" => Some(SourceKind::Model),
            "repair" => Some(SourceKind::Repair),
            "delegation" => Some(SourceKind::Delegation),
            _ => None,
        }
    }
}

/// 一条待写入的记忆。`Store::remember` 会先 normalize 再算内容 id。
#[derive(Debug, Clone)]
pub struct Draft {
    pub mem_kind: MemKind,
    /// `workspace` | `project`（P1 只允许这两个值）。
    pub scope: String,
    /// 空 = 不做硬键冲突检测。非空时同 `(scope,key)` 只允许一条 live。
    pub key: String,
    pub text: String,
    pub source_kind: SourceKind,
    pub source_seq: u64,
    /// 绝对毫秒时间戳；0 = 永不过期。**显式 `--expires 0` 不是 0**：
    /// CLI 会把它算成「现在」，表示立即过期（见 `ui remember` 的说明）。
    pub expires_at: u64,
    pub confidence: f64,
}

impl Draft {
    pub fn human(
        mem_kind: MemKind,
        scope: impl Into<String>,
        key: impl Into<String>,
        text: impl Into<String>,
        expires_at: u64,
    ) -> Draft {
        Draft {
            mem_kind,
            scope: scope.into(),
            key: key.into(),
            text: text.into(),
            source_kind: SourceKind::Human,
            source_seq: 0,
            expires_at,
            confidence: 1.0,
        }
    }
}

/// 物化后的一条记忆。派生字段（hits/lastHitAt/forgottenAt）不写回事件。
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    pub mem_kind: MemKind,
    pub scope: String,
    pub key: String,
    pub text: String,
    pub source_kind: SourceKind,
    pub source_seq: u64,
    pub expires_at: u64,
    pub confidence: f64,
    pub created_at: u64,
    pub write_seq: u64,
    pub hits: u64,
    pub last_hit_at: u64,
    pub forgotten_at: Option<u64>,
    pub forgotten_reason: Option<String>,
}

impl Entry {
    pub fn is_expired(&self, now: u64) -> bool {
        self.forgotten_at.is_none() && self.expires_at != 0 && self.expires_at <= now
    }

    pub fn is_live(&self, now: u64) -> bool {
        self.forgotten_at.is_none() && !self.is_expired(now)
    }

    pub fn status(&self, now: u64) -> &'static str {
        if self.forgotten_at.is_some() {
            "FORGOTTEN"
        } else if self.is_expired(now) {
            "EXPIRED"
        } else {
            "LIVE"
        }
    }
}

/// 一次检索命中。`reason` 会原样写进 `memory/used`。
#[derive(Debug, Clone)]
pub struct Hit {
    pub entry: Entry,
    pub score: f64,
    pub reason: String,
}

/// 检索配置。`now_ms = 0` 表示取当前时间（测试可传固定值）。
#[derive(Debug, Clone)]
pub struct Config {
    pub top_k: usize,
    pub max_tokens: u64,
    pub now_ms: u64,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            top_k: DEFAULT_TOP_K,
            max_tokens: DEFAULT_MAX_TOKENS,
            now_ms: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RememberOutcome {
    pub id: String,
    pub seq: u64,
    /// 同内容同 id 已经 live：幂等成功，没有追加事件。
    pub existed: bool,
    pub expires_at: u64,
}

/// 记忆库。文件不存在 = 空库，不是错误。
pub struct Store {
    path: PathBuf,
    entries: Vec<Entry>,
}

impl Store {
    pub fn open(workspace: &Path) -> Store {
        Store::open_path(workspace.join(REL_PATH))
    }

    fn open_path(path: PathBuf) -> Store {
        Store {
            path,
            entries: Vec::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// 从磁盘重放。哈希链坏了就返回错误（run_turn 会把它变成 `AgentError::Memory`，
    /// 在写 `turn/start` 之前停下——坏的记忆不能被静默当成空记忆）。
    pub fn refresh(&mut self) -> Result<(), String> {
        let events = Journal::load(&self.path)
            .map_err(|e| format!("记忆文件损坏（{}）：{}", self.path.display(), e))?;
        self.entries = materialize(&events)?;
        Ok(())
    }

    /// 显式写入。返回 `existed=true` 表示同 id 已 live，没有追加事件。
    ///
    /// 临界区顺序：`memory.guard` → 重新加载 → 冲突检查 → `memory.jsonl`。
    /// 锁序与设计稿 §4.5 一致，没有反向路径。
    pub fn remember(&mut self, draft: &Draft) -> Result<RememberOutcome, String> {
        let draft = draft.normalized()?;
        let now = time::now_ms();
        let id = id_for(draft.mem_kind, &draft.scope, &draft.key, &draft.text);
        let path = self.path.clone();
        let guard = guard_path(&path);
        let (outcome, latest) = journal::with_file_lock(&guard, || {
            let mut latest = Store::open_path(path.clone());
            latest.refresh().map_err(io_data)?;

            if let Some(existing) = latest
                .entries
                .iter()
                .find(|e| e.id == id && e.forgotten_at.is_none())
            {
                if !existing.is_expired(now) {
                    return Ok((
                        RememberOutcome {
                            id: id.clone(),
                            seq: existing.write_seq,
                            existed: true,
                            expires_at: existing.expires_at,
                        },
                        latest,
                    ));
                }
            }

            if !draft.key.is_empty() {
                if let Some(other) = latest.entries.iter().find(|e| {
                    e.is_live(now)
                        && e.scope == draft.scope
                        && e.key == draft.key
                        && e.id != id
                }) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "记忆冲突：{} 下已有同 key `{}` 的 live 记忆 {}；先 ui forget {}，或换一个 --key",
                            draft.scope, draft.key, other.id, other.id
                        ),
                    ));
                }
            }

            let mut journal = Journal::open(&path)?;
            // 这里走 `append_raw`，是**故意**绕过落账那道门的：
            // `.ui/memory.jsonl` 不是"记录"，它就是记忆库本体——这一条正文
            // 会在之后每一步被召回进上下文，抹掉等于让模型记错东西。
            // 补这一刀的不是门，是两头各一次：`ui remember` 在写之前就把
            // 凭据形状拒掉（cmd_remember），`ui secrets` 在扫的时候把这个
            // 文件一起算进来（来源标 `memory`）——手改过的、老版本写下的，
            // 都还要能被指出来，不能"扫不到就等于不存在"。
            let event = journal.append_raw("memory/write", write_payload(&draft))?;
            let seq = event.u64_at("seq").unwrap_or(0);
            drop(journal);
            latest.refresh().map_err(io_data)?;
            Ok((
                RememberOutcome {
                    id: id.clone(),
                    seq,
                    existed: false,
                    expires_at: draft.expires_at,
                },
                latest,
            ))
        })
        .map_err(|e| e.to_string())?;

        *self = latest;
        Ok(outcome)
    }

    /// 写墓碑（逻辑删除，不物理擦除）。未知 id 返回 `Ok(None)`。
    pub fn forget(&mut self, id: &str, reason: &str) -> Result<Option<u64>, String> {
        let id = id.trim();
        if id.is_empty() {
            return Err("用法：ui forget <id> --reason \"...\"".to_string());
        }
        let reason = {
            let r = reason.trim();
            if r.is_empty() {
                "用户要求删除"
            } else {
                r
            }
        };
        let path = self.path.clone();
        let guard = guard_path(&path);
        let (seq, latest) = journal::with_file_lock(&guard, || {
            let mut latest = Store::open_path(path.clone());
            latest.refresh().map_err(io_data)?;
            if !latest.entries.iter().any(|e| e.id == id) {
                return Ok((None, latest));
            }
            let mut journal = Journal::open(&path)?;
            let event = journal.append(
                "memory/forget",
                Json::obj(vec![
                    ("id", Json::from(id)),
                    ("reason", Json::from(reason)),
                ]),
            )?;
            let seq = event.u64_at("seq").unwrap_or(0);
            drop(journal);
            latest.refresh().map_err(io_data)?;
            Ok((Some(seq), latest))
        })
        .map_err(|e| e.to_string())?;

        *self = latest;
        Ok(seq)
    }

    /// 记录一次使用。P1 的 `memory/used` 同时写两边：
    ///
    /// - 主日志：`ui replay` 能看见「这一轮用了哪条」，和 `knowledge/use` 同风格；
    /// - 记忆链：命中次数 `hits` 由它重放出来（设计稿 §3.3 的派生字段）。
    ///
    /// 两边的 payload 完全相同，字段只有 `id` / `reason` / `tokens`。
    pub fn record_used(&mut self, id: &str, reason: &str, tokens: u64) -> Result<(), String> {
        let payload = used_payload(id, reason, tokens);
        let mut journal = Journal::open(&self.path).map_err(|e| e.to_string())?;
        journal
            .append("memory/used", payload)
            .map_err(|e| e.to_string())?;
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            entry.hits += 1;
            entry.last_hit_at = time::now_ms();
        }
        Ok(())
    }

    /// 每轮一次检索：BM25 + 路径/关键词加成，top-k 后按 token 预算截断。
    /// 返回的命中集合就是「这一轮真正注入的条目」——预算外的不写 `memory/used`。
    pub fn retrieve(&self, query: &str, cfg: &Config) -> Vec<Hit> {
        let now = if cfg.now_ms == 0 {
            time::now_ms()
        } else {
            cfg.now_ms
        };
        let query_tokens = unique_tokens(tokenize(query));
        if query_tokens.is_empty() || cfg.top_k == 0 || cfg.max_tokens == 0 {
            return Vec::new();
        }

        let entries: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|e| e.is_live(now) && e.confidence >= MIN_CONFIDENCE)
            .collect();
        if entries.is_empty() {
            return Vec::new();
        }

        let docs: Vec<Vec<String>> = entries
            .iter()
            .map(|e| tokenize(&format!("{} {} {}", e.text, e.key, e.scope)))
            .collect();
        let n = docs.len() as f64;
        let avgdl = (docs.iter().map(|d| d.len()).sum::<usize>() as f64 / n).max(1.0);

        let mut doc_counts: Vec<HashMap<String, usize>> = Vec::with_capacity(docs.len());
        let mut df: HashMap<String, usize> = HashMap::new();
        for doc in &docs {
            let mut counts: HashMap<String, usize> = HashMap::new();
            for token in doc {
                *counts.entry(token.clone()).or_insert(0) += 1;
            }
            for token in counts.keys() {
                *df.entry(token.clone()).or_insert(0) += 1;
            }
            doc_counts.push(counts);
        }

        let (k1, b) = (1.2f64, 0.75f64);
        let query_lower = query.to_lowercase();
        let query_paths = path_tokens(query);
        let mut hits: Vec<Hit> = Vec::new();

        for (entry, counts) in entries.iter().zip(doc_counts.iter()) {
            let dl = counts.values().sum::<usize>().max(1) as f64;
            let mut score = 0.0f64;
            let mut matched: Vec<String> = Vec::new();
            for token in &query_tokens {
                let tf = *counts.get(token).unwrap_or(&0);
                if tf == 0 {
                    continue;
                }
                let doc_freq = *df.get(token).unwrap_or(&0) as f64;
                let idf = (1.0 + (n - doc_freq + 0.5) / (doc_freq + 0.5)).ln();
                score += idf * tf as f64 * (k1 + 1.0)
                    / (tf as f64 + k1 * (1.0 - b + b * dl / avgdl));
                if matched.len() < 4 {
                    matched.push(token.clone());
                }
            }
            if score <= 0.0 {
                continue;
            }

            let mut path_reason: Option<String> = None;
            let text_lower = entry.text.to_lowercase();
            let key_lower = entry.key.to_lowercase();
            for path in &query_paths {
                if text_lower.contains(path) || key_lower.contains(path) {
                    score += 2.0;
                    path_reason = Some(path.clone());
                    break;
                }
            }

            let mut key_reason: Option<String> = None;
            if !entry.key.is_empty() && query_lower.contains(&key_lower) {
                score += 1.0;
                key_reason = Some(entry.key.clone());
            }

            score += entry.mem_kind.score_bonus();
            score *= 0.7 + 0.3 * entry.confidence.clamp(0.0, 1.0);

            let mut reason = if matched.is_empty() {
                "bm25".to_string()
            } else {
                format!("bm25:{}", matched.join(","))
            };
            if let Some(k) = key_reason {
                reason = format!("key:{};{}", k, reason);
            }
            if let Some(p) = path_reason {
                reason = format!("path:{};{}", p, reason);
            }

            hits.push(Hit {
                entry: (*entry).clone(),
                score,
                reason,
            });
        }

        // 同分按 id 升序：同一库 + 同一 query 的结果必须逐字节可复现。
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.entry.id.cmp(&b.entry.id))
        });

        let mut out: Vec<Hit> = Vec::new();
        let mut rendered = HEADER.to_string();
        for hit in hits {
            if out.len() >= cfg.top_k {
                break;
            }
            let mut candidate = rendered.clone();
            candidate.push('\n');
            candidate.push_str(&render_line(&hit));
            // 不截断条目：被截断的记忆可能语义相反；加入下一条会超预算就停。
            if estimate_tokens(&candidate) > cfg.max_tokens {
                break;
            }
            rendered = candidate;
            out.push(hit);
        }
        out
    }
}

fn guard_path(path: &Path) -> PathBuf {
    path.parent()
        .map(|p| p.join("memory.guard"))
        .unwrap_or_else(|| PathBuf::from("memory.guard"))
}

fn io_data(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// 内容寻址 id：`sha256(memKind \x1f scope \x1f key \x1f text)` 前 12 位。
/// 同内容同 id，天然去重；改正文而不改 id 会被 `ui memory check` 抓住。
pub fn id_for(mem_kind: MemKind, scope: &str, key: &str, text: &str) -> String {
    let feed = format!(
        "{}\x1f{}\x1f{}\x1f{}",
        mem_kind.name(),
        scope,
        key,
        text
    );
    format!("m-{}", &sha256::hex(feed.as_bytes())[..12])
}

/// 正文的计价口径：字节数 / 4（与 `knowledge::Card::budget_tokens` 一致）。
/// `memory/used.tokens` 用这个值。
pub fn budget_tokens(text: &str) -> u64 {
    // 与注入预算、snapshot 计价共用同一套脚本感知估算；按字节/4 会把
    // 中文记忆少算 1.3–1.7 倍。
    estimate_tokens(text)
}

pub fn write_payload(draft: &Draft) -> Json {
    Json::obj(vec![
        ("id", Json::from(id_for(
            draft.mem_kind,
            &draft.scope,
            &draft.key,
            &draft.text,
        ))),
        ("memKind", Json::from(draft.mem_kind.name())),
        ("scope", Json::from(draft.scope.as_str())),
        ("key", Json::from(draft.key.as_str())),
        ("text", Json::from(draft.text.as_str())),
        ("sourceKind", Json::from(draft.source_kind.name())),
        ("sourceSeq", Json::from(draft.source_seq)),
        (
            "expiresAt",
            if draft.expires_at == 0 {
                Json::Null
            } else {
                Json::from(draft.expires_at)
            },
        ),
        ("confidence", Json::Num(draft.confidence)),
    ])
}

pub fn used_payload(id: &str, reason: &str, tokens: u64) -> Json {
    Json::obj(vec![
        ("id", Json::from(id)),
        ("reason", Json::from(reason)),
        ("tokens", Json::from(tokens)),
    ])
}

impl Draft {
    /// 规范化 + 校验。任何一项失败都不写事件。
    pub fn normalized(&self) -> Result<Draft, String> {
        let scope = self.scope.trim().to_ascii_lowercase();
        if scope != "workspace" && scope != "project" {
            return Err(format!(
                "--scope 只支持 workspace | project，收到 `{}`",
                self.scope
            ));
        }
        let key = self.key.trim().to_ascii_lowercase();
        if !key.is_empty() && !valid_key(&key) {
            return Err(format!(
                "--key 只能是小写字母/数字与 _ . : / -（以字母或数字开头，最长 64），收到 `{}`",
                key
            ));
        }
        let text = normalize_text(&self.text)?;
        if !(0.0..=1.0).contains(&self.confidence) {
            return Err(format!("confidence 必须在 0..1，收到 {}", self.confidence));
        }
        Ok(Draft {
            mem_kind: self.mem_kind,
            scope,
            key,
            text,
            source_kind: self.source_kind,
            source_seq: self.source_seq,
            expires_at: self.expires_at,
            confidence: self.confidence,
        })
    }
}

fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    key.chars().count() <= 64
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | ':' | '/' | '-'))
}

/// 单行化：折叠换行/制表/连续空白，拒绝其他控制字符。
pub fn normalize_text(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut last_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
            continue;
        }
        if c.is_control() {
            return Err(format!("正文不能包含控制字符 U+{:04X}", c as u32));
        }
        out.push(c);
        last_space = false;
    }
    let text = out.trim().to_string();
    if text.is_empty() {
        return Err("正文不能为空".to_string());
    }
    let chars = text.chars().count();
    if chars > MAX_TEXT_CHARS {
        return Err(format!(
            "正文最多 {} 字符，收到 {} 字符（记忆是稀疏的，长文本请拆成多条）",
            MAX_TEXT_CHARS, chars
        ));
    }
    if looks_secret(&text) {
        return Err(
            "正文看起来包含密钥/凭据（api_key= / token: / sk- / ghp_ / AKIA / BEGIN / 长 hex / secrets/）；拒绝写入"
                .to_string(),
        );
    }
    Ok(text)
}

/// 密钥扫描是硬闸：命中就不写任何事件，避免把疑似凭据留在 append-only 日志里。
pub fn looks_secret(text: &str) -> bool {
    let lower = text.to_lowercase();
    for pattern in [
        "api_key=",
        "apikey=",
        "token:",
        "sk-",
        "ghp_",
        "akia",
        "-----begin",
        "secrets/",
    ] {
        if lower.contains(pattern) {
            return true;
        }
    }
    let mut hex_run = 0usize;
    for c in text.chars() {
        if c.is_ascii_hexdigit() {
            hex_run += 1;
            if hex_run >= 32 {
                return true;
            }
        } else {
            hex_run = 0;
        }
    }
    false
}

/// 默认 TTL 的绝对时间戳。`pref` 永不过期（0），其余按设计稿 §3.6。
pub fn default_expires_at(kind: MemKind, now: u64) -> u64 {
    let ttl = kind.default_ttl_ms();
    if ttl == 0 {
        0
    } else {
        now.saturating_add(ttl)
    }
}

/// `--expires` 的人类可读说明。
pub fn describe_expiry(expires_at: u64, now: u64) -> String {
    if expires_at == 0 {
        return "永不过期".to_string();
    }
    if expires_at <= now {
        return "已过期".to_string();
    }
    let left = expires_at - now;
    if left >= DAY_MS {
        format!("{} 天后过期", left / DAY_MS)
    } else if left >= 3_600_000 {
        format!("{} 小时后过期", left / 3_600_000)
    } else {
        format!("{} 分钟后过期", (left / 60_000).max(1))
    }
}

const HEADER: &str = "记忆（从日志派生、可能已过时；与当前文件或工具结果冲突时，以工具结果为准，不要据此直接改文件）：";

fn render_line(hit: &Hit) -> String {
    let entry = &hit.entry;
    let key = if entry.key.is_empty() {
        String::new()
    } else {
        format!(" key={}", entry.key)
    };
    format!(
        "- [{}] {} {} conf={:.2} hits={}{} :: {}",
        entry.id,
        entry.mem_kind.name(),
        entry.scope,
        entry.confidence,
        entry.hits,
        key,
        entry.text
    )
}

/// 把命中渲染成快照 `memory` 段的正文。空命中 → 空串（调用方据此不加段落）。
pub fn render(hits: &[Hit]) -> String {
    if hits.is_empty() {
        return String::new();
    }
    let mut out = HEADER.to_string();
    for hit in hits {
        out.push('\n');
        out.push_str(&render_line(hit));
    }
    out
}

/// 本轮检索的 query：本轮输入 + goal + history 里最近 2 条 user 消息。
/// 「继续」这类短输入需要上下文才检索得到。截断到 2000 字符。
pub fn build_query(input: &str, goal: Option<&str>, history: &[Message]) -> String {
    let mut parts: Vec<String> = vec![input.to_string()];
    if let Some(goal) = goal {
        if !goal.trim().is_empty() {
            parts.push(goal.to_string());
        }
    }
    let mut recent = 0usize;
    for message in history.iter().rev() {
        if message.role != "user" {
            continue;
        }
        parts.push(message.content.clone());
        recent += 1;
        if recent >= 2 {
            break;
        }
    }
    truncate_chars(&parts.join(" "), 2000)
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect()
}

fn is_cjk(c: char) -> bool {
    let cp = c as u32;
    matches!(
        cp,
        0x2E80..=0x2FDF
            | 0x3040..=0x30FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xAC00..=0xD7AF
            | 0xF900..=0xFAFF
    )
}

fn is_stopword(token: &str) -> bool {
    const ASCII: &[&str] = &["the", "a", "an", "is", "are", "to", "of", "in", "on", "for", "with"];
    const CJK: &[&str] = &["的", "了", "是", "在", "和", "与", "把", "被", "就", "也", "不", "要", "请", "我", "你"];
    if ASCII.iter().any(|w| *w == token) {
        return true;
    }
    CJK.iter().any(|w| *w == token)
}

fn flush_ascii(buffer: &mut String, out: &mut Vec<String>) {
    if buffer.chars().count() >= 2 {
        let token = buffer.to_ascii_lowercase();
        if !is_stopword(&token) {
            out.push(token);
        }
    }
    buffer.clear();
}

fn flush_cjk(buffer: &mut Vec<char>, out: &mut Vec<String>) {
    for c in buffer.iter() {
        let token = c.to_string();
        if !is_stopword(&token) {
            out.push(token);
        }
    }
    for pair in buffer.windows(2) {
        let token: String = pair.iter().collect();
        if !is_stopword(&token) {
            out.push(token);
        }
    }
    buffer.clear();
}

/// 零依赖分词：ASCII 连续段（≥2）+ CJK 单字与相邻双字。
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut ascii = String::new();
    let mut cjk: Vec<char> = Vec::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            flush_cjk(&mut cjk, &mut out);
            ascii.push(c);
        } else if is_cjk(c) {
            flush_ascii(&mut ascii, &mut out);
            cjk.push(c);
        } else {
            flush_ascii(&mut ascii, &mut out);
            flush_cjk(&mut cjk, &mut out);
        }
    }
    flush_ascii(&mut ascii, &mut out);
    flush_cjk(&mut cjk, &mut out);
    out
}

fn unique_tokens(tokens: Vec<String>) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for token in tokens {
        if seen.insert(token.clone()) {
            out.push(token);
        }
    }
    out
}

/// query 里的路径片段（含 `/` 或 `\`）。用于路径加成。
pub fn path_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split_whitespace() {
        let trimmed = raw.trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\'' | '`' | ',' | '.' | ';' | ':' | '!' | '?' | '(' | ')' | '[' | ']'
                    | '{' | '}' | '<' | '>' | '，' | '。' | '；' | '：' | '、' | '（' | '）'
            )
        });
        if trimmed.chars().count() < 3 {
            continue;
        }
        if trimmed.contains('/') || trimmed.contains('\\') {
            out.push(trimmed.replace('\\', "/").to_ascii_lowercase());
        }
    }
    out
}

/// 重放事件得到物化状态。任何字段非法都 fail closed（返回 Err），不静默跳过。
fn materialize(events: &[Json]) -> Result<Vec<Entry>, String> {
    let mut entries: Vec<Entry> = Vec::new();
    for event in events {
        match event.str_at("kind").unwrap_or("") {
            "memory/write" => {
                let seq = event.u64_at("seq").unwrap_or(0);
                let id = event
                    .str_at("id")
                    .ok_or_else(|| format!("memory/write seq={} 缺少 id", seq))?
                    .to_string();
                let mem_kind = MemKind::parse(
                    event
                        .str_at("memKind")
                        .ok_or_else(|| format!("memory/write seq={} 缺少 memKind", seq))?,
                )
                .ok_or_else(|| format!("memory/write seq={} 的 memKind 未知", seq))?;
                let scope = event
                    .str_at("scope")
                    .ok_or_else(|| format!("memory/write seq={} 缺少 scope", seq))?
                    .to_string();
                if scope != "workspace" && scope != "project" {
                    return Err(format!(
                        "memory/write seq={} 的 scope `{}` 非法（只允许 workspace|project）",
                        seq, scope
                    ));
                }
                let key = event.str_at("key").unwrap_or("").to_string();
                let text = event
                    .str_at("text")
                    .ok_or_else(|| format!("memory/write seq={} 缺少 text", seq))?
                    .to_string();
                let expected = id_for(mem_kind, &scope, &key, &text);
                if id != expected {
                    return Err(format!(
                        "记忆 {} 的 id 与内容不一致（seq={}，期望 {}）；正文可能被改过",
                        id, seq, expected
                    ));
                }
                let source_kind = match event.str_at("sourceKind") {
                    None => SourceKind::Human,
                    Some(value) => SourceKind::parse(value).ok_or_else(|| {
                        format!("memory/write seq={} 的 sourceKind `{}` 未知", seq, value)
                    })?,
                };
                let expires_at = match event.get("expiresAt") {
                    None | Some(Json::Null) => 0,
                    Some(Json::Num(value)) if *value >= 0.0 => *value as u64,
                    Some(_) => {
                        return Err(format!("memory/write seq={} 的 expiresAt 非法", seq));
                    }
                };
                let confidence = match event.get("confidence") {
                    None => 1.0,
                    Some(Json::Num(value)) if (0.0..=1.0).contains(value) => *value,
                    Some(_) => {
                        return Err(format!("memory/write seq={} 的 confidence 非法", seq));
                    }
                };
                let source_seq = event.u64_at("sourceSeq").unwrap_or(0);
                let created_at = event.u64_at("ts").unwrap_or(0);
                let mut entry = Entry {
                    id: id.clone(),
                    mem_kind,
                    scope,
                    key,
                    text,
                    source_kind,
                    source_seq,
                    expires_at,
                    confidence,
                    created_at,
                    write_seq: seq,
                    hits: 0,
                    last_hit_at: 0,
                    forgotten_at: None,
                    forgotten_reason: None,
                };
                if let Some(slot) = entries.iter_mut().find(|e| e.id == id) {
                    // 同 id 覆盖：刷新有效期/置信度，保留派生命中数。
                    entry.hits = slot.hits;
                    entry.last_hit_at = slot.last_hit_at;
                    *slot = entry;
                } else {
                    entries.push(entry);
                }
            }
            "memory/forget" => {
                let seq = event.u64_at("seq").unwrap_or(0);
                let id = event
                    .str_at("id")
                    .ok_or_else(|| format!("memory/forget seq={} 缺少 id", seq))?;
                let reason = event.str_at("reason").unwrap_or("").to_string();
                if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                    entry.forgotten_at = Some(event.u64_at("ts").unwrap_or(0));
                    entry.forgotten_reason = Some(reason);
                }
            }
            "memory/used" => {
                let id = event.str_at("id").unwrap_or("");
                if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                    // 只统计「当时还是 live」的命中；墓碑后的 used 不再抬高 hits。
                    if entry.forgotten_at.is_none() {
                        entry.hits += 1;
                        entry.last_hit_at = event.u64_at("ts").unwrap_or(0);
                    }
                }
            }
            "journal/torn" => {}
            other => {
                return Err(format!(
                    "记忆文件里有未知事件类型 `{}`（seq={}）",
                    other,
                    event.u64_at("seq").unwrap_or(0)
                ));
            }
        }
    }

    // 不变量：key 非空时同 (scope,key) 最多一条 live。违反 = fail closed，
    // 不静默挑一条用（设计稿 §3.5 / V5）。
    let now = time::now_ms();
    let mut live_keys: HashMap<(String, String), String> = HashMap::new();
    for entry in entries.iter().filter(|e| e.is_live(now)) {
        if entry.key.is_empty() {
            continue;
        }
        let slot = (entry.scope.clone(), entry.key.clone());
        if let Some(other) = live_keys.get(&slot) {
            return Err(format!(
                "记忆冲突：{} 下有两条 live 记忆共用 key `{}`（{} 与 {}）；先 ui forget 其中一条",
                entry.scope, entry.key, other, entry.id
            ));
        }
        live_keys.insert(slot, entry.id.clone());
    }

    Ok(entries)
}

/// `ui memory check` 的报告。
#[derive(Debug, Clone, Default)]
pub struct CheckReport {
    pub events: usize,
    pub writes: usize,
    pub forgets: usize,
    pub used: usize,
    pub live: usize,
    pub forgotten: usize,
    pub expired: usize,
}

/// 校验记忆文件：哈希链、seq 连续性、id/内容一致、悬空引用、live 冲突。
///
/// `Ok(None)` = 文件不存在（未启用），不是错误。
pub fn check(workspace: &Path) -> Result<Option<CheckReport>, String> {
    let path = workspace.join(REL_PATH);
    if !path.exists() {
        return Ok(None);
    }
    let (events, corrupt) = Journal::read(&path)
        .map_err(|e| format!("读记忆文件失败（{}）：{}", path.display(), e))?;
    if let Some(c) = corrupt {
        return Err(format!(
            "记忆链损坏 seq={:?}: {}（{}）",
            c.seq,
            c.reason,
            path.display()
        ));
    }

    let mut report = CheckReport {
        events: events.len(),
        ..Default::default()
    };
    let mut write_ids: HashSet<String> = HashSet::new();
    for (index, event) in events.iter().enumerate() {
        let seq = event.u64_at("seq").unwrap_or(0);
        if seq != (index as u64 + 1) {
            return Err(format!(
                "记忆 seq 不连续：第 {} 条事件的 seq={}（期望 {}）",
                index + 1,
                seq,
                index + 1
            ));
        }
        match event.str_at("kind").unwrap_or("") {
            "memory/write" => {
                report.writes += 1;
                let id = event
                    .str_at("id")
                    .ok_or_else(|| format!("memory/write seq={} 缺少 id", seq))?;
                let mem_kind = MemKind::parse(event.str_at("memKind").unwrap_or(""))
                    .ok_or_else(|| format!("memory/write seq={} 的 memKind 未知", seq))?;
                let scope = event.str_at("scope").unwrap_or("");
                if scope != "workspace" && scope != "project" {
                    return Err(format!("memory/write seq={} 的 scope 非法", seq));
                }
                let key = event.str_at("key").unwrap_or("");
                let text = event
                    .str_at("text")
                    .ok_or_else(|| format!("memory/write seq={} 缺少 text", seq))?;
                let expected = id_for(mem_kind, scope, key, text);
                if id != expected {
                    return Err(format!(
                        "记忆 {} 的 id 与内容不一致（seq={}，期望 {}）",
                        id, seq, expected
                    ));
                }
                write_ids.insert(id.to_string());
            }
            "memory/forget" => {
                report.forgets += 1;
                let id = event
                    .str_at("id")
                    .ok_or_else(|| format!("memory/forget seq={} 缺少 id", seq))?;
                if !write_ids.contains(id) {
                    return Err(format!(
                        "memory/forget seq={} 引用了不存在的记忆 {}",
                        seq, id
                    ));
                }
                if event.str_at("reason").is_none() {
                    return Err(format!("memory/forget seq={} 缺少 reason", seq));
                }
            }
            "memory/used" => {
                report.used += 1;
                let id = event
                    .str_at("id")
                    .ok_or_else(|| format!("memory/used seq={} 缺少 id", seq))?;
                if !write_ids.contains(id) {
                    return Err(format!(
                        "memory/used seq={} 引用了不存在的记忆 {}",
                        seq, id
                    ));
                }
                if event.u64_at("tokens").is_none() {
                    return Err(format!("memory/used seq={} 缺少 tokens", seq));
                }
            }
            "journal/torn" => {}
            other => {
                return Err(format!(
                    "记忆文件里有未知事件类型 `{}`（seq={}）",
                    other, seq
                ));
            }
        }
    }

    // 复用物化逻辑：字段非法、id 不一致、live 冲突都会在这里再失败一次。
    let entries = materialize(&events)?;
    let now = time::now_ms();
    for entry in &entries {
        if entry.forgotten_at.is_some() {
            report.forgotten += 1;
        } else if entry.is_expired(now) {
            report.expired += 1;
        } else {
            report.live += 1;
        }
    }
    Ok(Some(report))
}

/// 读记忆文件的全部事件（CLI 的 list/测试用）。文件不存在 = 空 vec。
pub fn load_events(workspace: &Path) -> Result<Vec<Json>, String> {
    Journal::load(workspace.join(REL_PATH)).map_err(|e| e.to_string())
}

/// 文件大小（不存在 = 0）。给 `ui memory list` 的头部用。
pub fn file_len(workspace: &Path) -> u64 {
    fs::metadata(workspace.join(REL_PATH))
        .map(|m| m.len())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ui-memory-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn remember(store: &mut Store, kind: MemKind, text: &str) -> RememberOutcome {
        store
            .remember(&Draft::human(kind, "workspace", "", text, 0))
            .unwrap()
    }

    #[test]
    fn content_id_is_stable_and_depends_on_every_field() {
        let a = id_for(MemKind::Build, "workspace", "build.verify", "cargo test --all");
        let b = id_for(MemKind::Build, "workspace", "build.verify", "cargo test --all");
        assert_eq!(a, b);
        assert_eq!(a.len(), 14, "m- + 12 hex");
        assert_ne!(a, id_for(MemKind::Fact, "workspace", "build.verify", "cargo test --all"));
        assert_ne!(a, id_for(MemKind::Build, "project", "build.verify", "cargo test --all"));
        assert_ne!(a, id_for(MemKind::Build, "workspace", "build.test", "cargo test --all"));
        assert_ne!(a, id_for(MemKind::Build, "workspace", "build.verify", "cargo test -p ui-core"));
    }

    #[test]
    fn tokenize_keeps_cjk_bigrams_and_ascii_words() {
        let tokens = tokenize("门禁 bash tools/ci.sh；单测 cargo test --all");
        assert!(tokens.iter().any(|t| t == "门禁"));
        assert!(tokens.iter().any(|t| t == "门"));
        assert!(tokens.iter().any(|t| t == "bash"));
        assert!(tokens.iter().any(|t| t == "tools"));
        assert!(tokens.iter().any(|t| t == "cargo"));
        assert!(!tokens.iter().any(|t| t == "a"), "单字母 ASCII 不入 token");
    }

    #[test]
    fn retrieval_is_deterministic_and_forget_stops_it() {
        let ws = tmp("retrieve");
        let mut store = Store::open(&ws);
        let build = remember(
            &mut store,
            MemKind::Build,
            "门禁 bash tools/ci.sh；单测 cargo test --all",
        );
        remember(&mut store, MemKind::Fact, "数据库迁移前必须先备份");

        let cfg = Config {
            now_ms: 1_700_000_000_000,
            ..Default::default()
        };
        let first = store.retrieve("跑一下门禁", &cfg);
        let second = store.retrieve("跑一下门禁", &cfg);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].entry.id, build.id);
        assert_eq!(
            first.iter().map(|h| h.entry.id.clone()).collect::<Vec<_>>(),
            second.iter().map(|h| h.entry.id.clone()).collect::<Vec<_>>(),
            "同库同 query 必须逐条可复现"
        );

        assert!(store.forget(&build.id, "测试").unwrap().is_some());
        assert!(store.retrieve("跑一下门禁", &cfg).is_empty(), "墓碑后不再检索");
    }

    #[test]
    fn expired_entries_are_not_retrieved() {
        let ws = tmp("expired");
        let mut store = Store::open(&ws);
        let outcome = store
            .remember(&Draft::human(
                MemKind::Build,
                "workspace",
                "",
                "门禁 bash tools/ci.sh",
                1_600_000_000_000,
            ))
            .unwrap();
        let hits = store.retrieve(
            "门禁",
            &Config {
                now_ms: 1_700_000_000_000,
                ..Default::default()
            },
        );
        assert!(hits.is_empty());
        assert!(store
            .entries()
            .iter()
            .any(|e| e.id == outcome.id && e.is_expired(1_700_000_000_000)));
    }

    #[test]
    fn budget_caps_the_injected_tokens_without_truncating_entries() {
        let ws = tmp("budget");
        let mut store = Store::open(&ws);
        for i in 0..6 {
            remember(
                &mut store,
                MemKind::Fact,
                &format!("构建命令 cargo test --all 的第 {} 条说明，注意顺序", i),
            );
        }
        let cfg = Config {
            top_k: 3,
            max_tokens: 80,
            now_ms: 1_700_000_000_000,
        };
        let hits = store.retrieve("构建命令 cargo test", &cfg);
        assert!(hits.len() <= 3);
        let text = render(&hits);
        assert!(estimate_tokens(&text) <= 80, "预算必须真的生效：{}", text);
        for hit in &hits {
            assert!(text.contains(&hit.entry.text), "条目不能被截断");
        }
    }

    #[test]
    fn check_catches_content_id_mismatch_even_with_a_rehashed_chain() {
        let ws = tmp("rehash");
        let mut store = Store::open(&ws);
        remember(&mut store, MemKind::Fact, "数据库迁移前必须先备份");
        let path = ws.join(REL_PATH);

        // 模拟一个会写链的攻击者：改正文，然后重算该行 hash（prev 不变）。
        let text = fs::read_to_string(&path).unwrap();
        let mut event = crate::json::parse(text.trim()).unwrap();
        event.set("text", Json::from("数据库迁移前不需要备份"));
        event.set("hash", Json::Null);
        if let Json::Obj(items) = &mut event {
            items.retain(|(k, _)| k != "hash");
        }
        let hash = sha256::hex(event.to_compact().as_bytes());
        event.set("hash", Json::from(hash));
        fs::write(&path, format!("{}\n", event.to_compact())).unwrap();

        // 链本身是自洽的（Journal::read 通过），但内容 id 对不上。
        let (events, corrupt) = Journal::read(&path).unwrap();
        assert!(corrupt.is_none(), "测试前提：重算后的链必须通过哈希校验");
        assert_eq!(events.len(), 1);
        let err = check(&ws).err().expect("id/内容不一致必须失败");
        assert!(err.contains("id 与内容不一致"), "{}", err);
    }

    #[test]
    fn check_reports_counts_and_missing_file_is_not_an_error() {
        let ws = tmp("check");
        assert!(check(&ws).unwrap().is_none(), "没有文件 = 未启用");
        let mut store = Store::open(&ws);
        let a = remember(&mut store, MemKind::Build, "门禁 bash tools/ci.sh");
        remember(&mut store, MemKind::Fact, "数据库迁移前必须先备份");
        store.forget(&a.id, "测试").unwrap();
        store.record_used(&a.id, "bm25:门禁", 8).unwrap();

        let report = check(&ws).unwrap().unwrap();
        assert_eq!(report.writes, 2);
        assert_eq!(report.forgets, 1);
        assert_eq!(report.used, 1);
        assert_eq!(report.live, 1);
        assert_eq!(report.forgotten, 1);
    }
}
