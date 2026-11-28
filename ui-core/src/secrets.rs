//! 疑似密钥的定位与脱敏（单一来源）。
//!
//! 判据必须只有一份，否则「界面上打码、导出时原样」这类最尴尬的漏法一定会
//! 回来。现在的消费方：
//!
//! - `agent::scrub_env`：`env` 工具的输出（既进模型上下文，也进日志）
//! - `cli::cmd_export`：导出可转发轨迹时逐字段清洗（默认开）
//! - `cli::redact_journal_command`：把脱敏结果写回一份新日志
//! - 前端 `chat.html` / `viewer.html`：日志里已经写进去的历史值，只能在
//!   展示侧兜底——所以这里刻意保持字符串级、无状态的判据，方便照着实现
//!
//! 和 `memory::looks_secret` 的分工：那个函数问的是「这条记忆整体像不像
//! 密钥」，这里问的是「这段文本里哪一段是密钥」。日志行是一段 JSON，密钥
//! 藏在 `"PATH=... sk-abc ..."` 这种值中间，必须按子串定位，整串判断没用。

use crate::json::Json;

/// 文本里一处疑似密钥，`start`/`end` 是字节下标（左闭右开）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretSpan {
    pub start: usize,
    pub end: usize,
    pub label: &'static str,
}

/// 键名式判据：`token: xxx`、`api_key=xxx`、`"password": "xxx"`。
const KEY_WORDS: &[(&str, &'static str)] = &[
    ("api_key", "api key"),
    ("apikey", "api key"),
    ("access_key", "access key"),
    ("secret_key", "secret key"),
    ("secret", "secret"),
    ("token", "token"),
    ("password", "password"),
    ("passwd", "password"),
    ("credential", "credential"),
    ("private_key", "private key"),
    ("client_secret", "client secret"),
];

/// 前缀式判据：不需要键名，光看形状就知道是密钥。
/// `(前缀, 前缀之后至少多少个字符, 标签)`
const PREFIXES: &[(&str, usize, &'static str)] = &[
    ("sk-", 12, "api key"),
    ("ghp_", 16, "github token"),
    ("gho_", 16, "github token"),
    ("ghu_", 16, "github token"),
    ("ghs_", 16, "github token"),
    ("ghr_", 16, "github token"),
    ("github_pat_", 30, "github token"),
    ("xoxb-", 8, "slack token"),
    ("xoxa-", 8, "slack token"),
    ("xoxp-", 8, "slack token"),
    ("xoxr-", 8, "slack token"),
    ("xoxs-", 8, "slack token"),
    ("akia", 16, "aws key"),
    ("asia", 16, "aws key"),
    ("aiza", 30, "google api key"),
    ("ya29.", 16, "google oauth"),
    ("glpat-", 12, "gitlab token"),
];

/// 完整私钥块的头尾（小写；识别时统一转小写）。
///
/// 只认 PRIVATE KEY，不碰 `BEGIN CERTIFICATE`：公钥证书本来就设计成可公开的。
/// 没有结束标记时按到文本末尾处理——截断的私钥仍然不能因为「格式不完整」而漏出。
const PEM_PRIVATE_KEYS: &[(&str, &str)] = &[
    ("-----begin private key-----", "-----end private key-----"),
    (
        "-----begin rsa private key-----",
        "-----end rsa private key-----",
    ),
    (
        "-----begin ec private key-----",
        "-----end ec private key-----",
    ),
    (
        "-----begin dsa private key-----",
        "-----end dsa private key-----",
    ),
    (
        "-----begin openssh private key-----",
        "-----end openssh private key-----",
    ),
    (
        "-----begin encrypted private key-----",
        "-----end encrypted private key-----",
    ),
    (
        "---- begin ssh2 encrypted private key ----",
        "---- end ssh2 encrypted private key ----",
    ),
    (
        "-----begin pgp private key block-----",
        "-----end pgp private key block-----",
    ),
];

const AUTH_SCHEMES: &[&str] = &["bearer", "basic"];

/// 名字本身声明「这是密钥」的键（环境变量名、JSON 字段名）。
///
/// `PATH` / `*_PATH` 单独放过：内核会把工作区、模型路径这类变量喂给模型，
/// 一律打码等于砍掉环境感知，而它们泄露的不是凭据。
pub fn key_is_secret(name: &str) -> bool {
    let words = key_words(name);
    if words.is_empty() {
        return false;
    }
    // `PATH` / `*_PATH` 单独放过：内核会把工作区、模型路径这类变量喂给模型，
    // 一律打码等于砍掉环境感知，而它们泄露的不是凭据。
    if matches!(words.last().map(String::as_str), Some("path" | "paths")) {
        return false;
    }
    [
        "key",
        "secret",
        "token",
        "password",
        "passwd",
        "credential",
        "auth",
        "authorization",
        "authentication",
    ]
    .iter()
    .any(|needle| words.iter().any(|word| word == needle))
}

/// 键名按分隔符和驼峰分词，避免 `monkey` / `tokenizer` / `author` 这类普通词
/// 因为包含 KEY/TOKEN/AUTH 子串而被整体替换。
fn key_words(name: &str) -> Vec<String> {
    let chars: Vec<(usize, char)> = name.char_indices().collect();
    let mut words = Vec::new();
    let mut start = 0usize;
    for i in 0..chars.len() {
        let (at, ch) = chars[i];
        let boundary = !ch.is_alphanumeric()
            || (i > 0
                && chars[i - 1].0 >= start
                && ch.is_uppercase()
                && (chars[i - 1].1.is_lowercase()
                    || chars[i - 1].1.is_numeric()
                    || (chars[i - 1].1.is_uppercase()
                        && chars
                            .get(i + 1)
                            .is_some_and(|(_, next)| next.is_lowercase()))));
        if boundary {
            if start < at {
                words.push(name[start..at].to_ascii_lowercase());
            }
            start = if ch.is_alphanumeric() {
                at
            } else {
                at + ch.len_utf8()
            };
        }
    }
    if start < name.len() {
        words.push(name[start..].to_ascii_lowercase());
    }
    words
}

/// 结构元数据只做静态凭据检测，不参与跨事件前缀缓存。仅限内核生成的
/// ID、状态与配置字段；路径、文件名和产物名是模型可控输入，必须走流式门。
fn key_is_structural(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "id" | "ids"
            | "journal"
            | "workdir"
            | "model"
            | "provider"
            | "skill"
            | "card"
            | "status"
            | "state"
            | "kind"
            | "type"
            | "mode"
            | "role"
            | "actor"
            | "owner"
            | "user"
            | "who"
            | "session"
            | "agentid"
            | "taskid"
            | "callid"
            | "toolcallid"
            | "approvalid"
            | "step"
            | "turn"
            | "seq"
            | "ts"
            | "hash"
            | "prev"
            | "w"
            | "version"
            | "language"
            | "branch"
            | "commit"
            | "verdict"
            | "decision"
            | "checkpoint"
            | "withheld"
            | "redacted"
            | "exitcode"
    )
}

/// 值长得像凭据吗（用在键名式判据上）。
///
/// 三条排除项各自挡一类误伤：
/// - 纯数字/点分：`promptTokens: 123`、版本号、端口
/// - `$` / `<`：shell 变量引用与 `<your-key>` 这种占位符
/// - 太短：`token: none`、`KEY=`
fn value_is_credential(v: &str) -> bool {
    let n = v.chars().count();
    if n < 8 {
        return false;
    }
    if v.starts_with('$') || v.starts_with('<') || v.contains('<') {
        return false;
    }
    if v.chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
    {
        return false;
    }
    v.chars().any(|c| c.is_ascii_alphanumeric())
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

/// `lower` 里 `needle` 的所有出现位置（`lower` 与原文字节等长：
/// `to_ascii_lowercase` 只改 ASCII 字母，非 ASCII 字节原样保留）。
fn occurrences(lower: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut hits = Vec::new();
    if needle.is_empty() || needle.len() > lower.len() {
        return hits;
    }
    let mut start = 0;
    while let Some(rel) = lower[start..]
        .windows(needle.len())
        .position(|w| w == needle)
    {
        hits.push(start + rel);
        start += rel + 1;
    }
    hits
}

/// 找出文本里所有疑似密钥，按出现顺序、互不重叠。
pub fn find(text: &str) -> Vec<SecretSpan> {
    let bytes = text.as_bytes();
    let lower = bytes.to_ascii_lowercase();
    let mut raw: Vec<SecretSpan> = Vec::new();

    for (needle, label) in KEY_WORDS {
        for at in occurrences(&lower, needle.as_bytes()) {
            // 前一个字符是字母/数字：`promptTokens` 里的 `token` 不是凭据键名。
            // `_` 不算——`ACCESS_TOKEN` 是。
            if at > 0 && is_word_byte(bytes[at - 1]) {
                continue;
            }
            // 键名吃到 `[A-Za-z0-9_]` 结尾：`api_key_id=` 是同一个键。
            let mut i = at + needle.len();
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
                i += 1;
            }
            if i >= bytes.len() || (bytes[i] != b':' && bytes[i] != b'=') {
                continue;
            }
            i += 1;
            // 分隔符后的空白与引号不算值
            while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'"' | b'\'' | b'`') {
                i += 1;
            }
            let value_start = i;
            // 值吃到「不是可打印 ASCII」为止。非 ASCII 必须停：键值对后面
            // 跟的是中文正文（`api_key=abc123，请保存。`），按字节扫会把整句
            // 话一起当密钥吞掉；控制字符与分隔符同样结束这一段。
            while i < bytes.len()
                && bytes[i] >= 0x21
                && bytes[i] < 0x7f
                && !matches!(
                    bytes[i],
                    b'"' | b'\'' | b'`' | b',' | b'}' | b']' | b'\\' | b';'
                )
            {
                i += 1;
            }
            if value_is_credential(&text[value_start..i]) {
                raw.push(SecretSpan {
                    start: at,
                    end: i,
                    label,
                });
            }
        }
    }

    for (needle, min_tail, label) in PREFIXES {
        for at in occurrences(&lower, needle.as_bytes()) {
            if at > 0 && is_word_byte(bytes[at - 1]) {
                continue;
            }
            let mut i = at + needle.len();
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric()
                    || bytes[i] == b'_'
                    || bytes[i] == b'-'
                    || bytes[i] == b'.')
            {
                i += 1;
            }
            if i - (at + needle.len()) >= *min_tail {
                raw.push(SecretSpan {
                    start: at,
                    end: i,
                    label,
                });
            }
        }
    }

    // Authorization 头允许一个或多个 ASCII 空白（空格、tab、CRLF）。Bearer 与
    // Basic 都按方案边界扫描，不能把 `Bearer ` 当普通前缀而切断跨空白 token。
    for scheme in AUTH_SCHEMES {
        for at in occurrences(&lower, scheme.as_bytes()) {
            if at > 0 && is_word_byte(bytes[at - 1]) {
                continue;
            }
            let mut i = at + scheme.len();
            let whitespace_start = i;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i == whitespace_start {
                continue;
            }
            let value_start = i;
            // RFC 6750 b64token：字母数字之外还允许 `-._~+/`，末尾可有 `=`。
            // 少一个字符就会让合法凭据在长度判断前被截断，整条 Authorization 漏出。
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric()
                    || matches!(bytes[i], b'_' | b'-' | b'.' | b'~' | b'+' | b'/' | b'='))
            {
                i += 1;
            }
            if i - value_start >= 16 {
                raw.push(SecretSpan {
                    start: at,
                    end: i,
                    label: "auth header",
                });
            }
        }
    }

    // PEM 私钥必须整块替换：只抹 BEGIN 行会留下可重建密钥的 base64 正文。
    // 每个头只扫描一次：命中后直接跳到块尾；截断块覆盖到 EOF 后停止。
    // 不能对每个 BEGIN 都重新找 END，否则大量重复头会把单次脱敏变成 O(n²)。
    for (begin, end) in PEM_PRIVATE_KEYS {
        let mut cursor = 0usize;
        while let Some(rel) = lower[cursor..]
            .windows(begin.len())
            .position(|window| window == begin.as_bytes())
        {
            let at = cursor + rel;
            let body_start = at + begin.len();
            let block_end = lower[body_start..]
                .windows(end.len())
                .position(|window| window == end.as_bytes())
                .map(|rel| body_start + rel + end.len())
                .unwrap_or(bytes.len());
            raw.push(SecretSpan {
                start: at,
                end: block_end,
                label: "private key",
            });
            if block_end == bytes.len() {
                break;
            }
            cursor = block_end;
        }
    }

    // 重叠合并：`"secret": "sk-abcd..."` 两条规则都会命中，留先出现的那条，
    // 否则替换时后一段的下标已经落在被改过的文本里。
    raw.sort_by_key(|s| (s.start, std::cmp::Reverse(s.end)));
    let mut spans: Vec<SecretSpan> = Vec::new();
    for span in raw {
        match spans.last_mut() {
            Some(last) if span.start < last.end => {
                if span.end > last.end {
                    last.end = span.end;
                }
            }
            _ => spans.push(span),
        }
    }
    spans
}

/// 把所有疑似密钥换成 `[REDACTED:<标签>]`，返回新文本与命中数。
///
/// 命中数为 0 时返回的字符串与原文相同——调用方（导出）靠这个数字决定
/// 「原样写出」还是「写重排后的 JSON」，避免把没必要的行重新序列化一遍。
pub fn redact(text: &str) -> (String, usize) {
    let spans = find(text);
    if spans.is_empty() {
        return (text.to_string(), 0);
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for span in &spans {
        if span.start < cursor {
            continue;
        }
        out.push_str(&text[cursor..span.start]);
        out.push_str(&format!("[REDACTED:{}]", span.label));
        cursor = span.end;
    }
    out.push_str(&text[cursor..]);
    (out, spans.len())
}

/// 跨 `assistant` 事件的模型正文脱敏器。
///
/// delta 已经不走浏览器，但模型也可能把一段秘密拆到两次 `on_assistant`。
/// 因此这里对事件尾部尚未闭合的密钥形状直接 fail closed；PEM 则保留少量头/尾状态，
/// 正文在遇到匹配 END 前一律不发布。状态不按任务重置，跨任务拆分同样受约束。
#[derive(Default, Clone)]
pub struct StreamRedactor {
    pending: String,
    /// 已发布的裸凭据头。中性事件不能把候选清掉：攻击者可在两段之间插入
    /// `turn/start`、工具名或 snapshot section。候选去重且有界；超限时封锁
    /// 当前威胁边界，不能静默驱逐安全候选。
    published_prefixes: Vec<String>,
    published_prefix_bytes: usize,
    pem_end: Option<&'static str>,
    suppress: bool,
    /// 候选历史上限被触发后，只有显式 reset/finish 才能开启新威胁边界。
    sealed: bool,
}

const MAX_STREAM_PENDING: usize = 4096;
const MAX_STREAM_PREFIXES: usize = 512;
const MAX_STREAM_PREFIX_BYTES: usize = 16_384;

fn credential_terminator(b: u8) -> bool {
    b.is_ascii_whitespace() || matches!(b, b'"' | b'\'' | b'`' | b',' | b'}' | b']' | b'\\' | b';')
}

fn token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'~' | b'+' | b'/' | b'=')
}

fn suffix_str(text: &str, max_bytes: usize) -> &str {
    let mut start = text.len().saturating_sub(max_bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// 找到第一处没有匹配 END 的 PEM 私钥头。每个头只扫描一次，最坏 O(n)。
fn open_pem(text: &str) -> Option<(usize, &'static str)> {
    let lower = text.as_bytes().to_ascii_lowercase();
    let mut best: Option<(usize, &'static str)> = None;
    for (begin, end) in PEM_PRIVATE_KEYS {
        let mut cursor = 0usize;
        while let Some(rel) = lower[cursor..]
            .windows(begin.len())
            .position(|window| window == begin.as_bytes())
        {
            let at = cursor + rel;
            let body_start = at + begin.len();
            if let Some(end_rel) = lower[body_start..]
                .windows(end.len())
                .position(|window| window == end.as_bytes())
            {
                cursor = body_start + end_rel + end.len();
                continue;
            }
            if best.map(|(old, _)| at < old).unwrap_or(true) {
                best = Some((at, end));
            }
            break;
        }
    }
    best
}

/// 事件尾部可能是某个密钥形状的前半段：返回必须替换掉的起始字节。
fn assistant_pending_start(text: &str) -> usize {
    let bytes = text.as_bytes();
    let lower = bytes.to_ascii_lowercase();
    let mut candidate = bytes.len();

    for (needle, min_tail, _label) in PREFIXES {
        for at in occurrences(&lower, needle.as_bytes()) {
            if at > 0 && is_word_byte(bytes[at - 1]) {
                continue;
            }
            let mut i = at + needle.len();
            while i < bytes.len() && token_byte(bytes[i]) {
                i += 1;
            }
            // 已达到静态判据的最小长度就立即整段替换；只有还没闭合的短前缀
            // 才缓存到下一事件，避免完整凭据在行尾退化成 partial。
            if i == bytes.len() && i - (at + needle.len()) < *min_tail {
                candidate = candidate.min(at);
            }
        }
    }

    for scheme in AUTH_SCHEMES {
        for at in occurrences(&lower, scheme.as_bytes()) {
            if at > 0 && is_word_byte(bytes[at - 1]) {
                continue;
            }
            let mut i = at + scheme.len();
            if i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                continue;
            }
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let value_start = i;
            while i < bytes.len() && token_byte(bytes[i]) {
                i += 1;
            }
            if i == bytes.len() && i - value_start < 16 {
                candidate = candidate.min(at);
            }
        }
    }

    for (needle, _label) in KEY_WORDS {
        for at in occurrences(&lower, needle.as_bytes()) {
            if at > 0 && is_word_byte(bytes[at - 1]) {
                continue;
            }
            let mut i = at + needle.len();
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            if i == bytes.len()
                && (i == at + needle.len() || lower[at + needle.len()..i].contains(&b'_'))
            {
                // 完整键名或 `api_key_id` 这类下划线扩展可能下一段才出现
                // 分隔符；普通复数/单词（`secrets`、`tokenize`）不是凭据键。
                candidate = candidate.min(at);
                continue;
            }
            while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
                i += 1;
            }
            if i >= bytes.len() || (bytes[i] != b':' && bytes[i] != b'=') {
                continue;
            }
            i += 1;
            while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'"' | b'\'' | b'`') {
                i += 1;
            }
            let value_start = i;
            while i < bytes.len()
                && bytes[i] >= 0x21
                && bytes[i] < 0x7f
                && !matches!(
                    bytes[i],
                    b'"' | b'\'' | b'`' | b',' | b'}' | b']' | b'\\' | b';'
                )
            {
                i += 1;
            }
            if i == bytes.len() && !value_is_credential(&text[value_start..i]) {
                candidate = candidate.min(at);
            }
        }
    }

    // 前缀本身被拆开时缓存事件尾部，下一次输入再验证。只从词首开始缓存，
    // 避免把正常单词末尾的 a/e/pro 等片段误当成密钥前缀。
    let mut check_suffix = |needle: &[u8]| {
        for n in (1..needle.len()).rev() {
            if n > bytes.len() || !lower.ends_with(&needle[..n]) {
                continue;
            }
            let at = bytes.len() - n;
            if at == 0 || !is_word_byte(bytes[at - 1]) {
                candidate = candidate.min(at);
                break;
            }
        }
    };
    for (needle, _min_tail, _label) in PREFIXES {
        check_suffix(needle.as_bytes());
    }
    for (needle, _label) in KEY_WORDS {
        check_suffix(needle.as_bytes());
    }
    for scheme in AUTH_SCHEMES {
        check_suffix(scheme.as_bytes());
    }
    candidate
}

/// 找到一个已经进入 pending、但本次遇到终止符时仍低于静态最小长度的凭据头。
/// 这种头不能原样发布：调用方可能把后续另一个字段的值继续拼到同一个外发流里。
/// 返回 `(起点, 值终点)`，终点之后的中性正文仍按普通文本处理。
fn assistant_terminated_candidate(text: &str) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let lower = bytes.to_ascii_lowercase();
    let mut candidate: Option<(usize, usize)> = None;
    let mut consider = |at: usize, end: usize| {
        if candidate
            .map(|(old_at, old_end)| at < old_at || (at == old_at && end > old_end))
            .unwrap_or(true)
        {
            candidate = Some((at, end));
        }
    };

    for (needle, min_tail, _label) in PREFIXES {
        for at in occurrences(&lower, needle.as_bytes()) {
            if at > 0 && is_word_byte(bytes[at - 1]) {
                continue;
            }
            let mut i = at + needle.len();
            while i < bytes.len() && token_byte(bytes[i]) {
                i += 1;
            }
            if i < bytes.len()
                && i - (at + needle.len()) < *min_tail
                && credential_terminator(bytes[i])
            {
                consider(at, i);
            }
        }
    }

    for scheme in AUTH_SCHEMES {
        for at in occurrences(&lower, scheme.as_bytes()) {
            if at > 0 && is_word_byte(bytes[at - 1]) {
                continue;
            }
            let mut i = at + scheme.len();
            if i >= bytes.len() || !bytes[i].is_ascii_whitespace() {
                continue;
            }
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let value_start = i;
            while i < bytes.len() && token_byte(bytes[i]) {
                i += 1;
            }
            if i < bytes.len() && i - value_start < 16 && credential_terminator(bytes[i]) {
                consider(at, i);
            }
        }
    }

    for (needle, _label) in KEY_WORDS {
        for at in occurrences(&lower, needle.as_bytes()) {
            if at > 0 && is_word_byte(bytes[at - 1]) {
                continue;
            }
            let mut i = at + needle.len();
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
                i += 1;
            }
            if i >= bytes.len() || (bytes[i] != b':' && bytes[i] != b'=') {
                continue;
            }
            i += 1;
            while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'"' | b'\'' | b'`') {
                i += 1;
            }
            let value_start = i;
            while i < bytes.len()
                && bytes[i] >= 0x21
                && bytes[i] < 0x7f
                && !matches!(
                    bytes[i],
                    b'"' | b'\'' | b'`' | b',' | b'}' | b']' | b'\\' | b';'
                )
            {
                i += 1;
            }
            if i < bytes.len()
                && credential_terminator(bytes[i])
                && !value_is_credential(&text[value_start..i])
            {
                consider(at, i);
            }
        }
    }

    candidate
}

/// 返回 PEM 头部的部分后缀起点。多个头共享前缀时必须选择最早、最长者，
/// 否则先命中的短头会吞掉后续字节，跨事件拼接时反而漏出正文。
fn partial_pem_start(text: &str) -> usize {
    let bytes = text.as_bytes();
    let lower = bytes.to_ascii_lowercase();
    let mut candidate = bytes.len();
    let mut matched = 0usize;
    for (begin, _) in PEM_PRIVATE_KEYS {
        for n in (1..begin.len()).rev() {
            if n > bytes.len() || !lower.ends_with(&begin.as_bytes()[..n]) {
                continue;
            }
            let at = bytes.len() - n;
            // PEM 头以破折号开头，完整头匹配也不要求词首边界。部分头若
            // 额外要求前一个字节不是单词，`x-----BEGIN ...` 就能拆开绕过。
            if at < candidate || (at == candidate && n > matched) {
                candidate = at;
                matched = n;
            }
            break;
        }
    }
    candidate
}

/// `pending` 是否只是某个凭据前缀的前半段（还没有进入 token 正文）。
fn is_bare_partial_prefix(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    PEM_PRIVATE_KEYS
        .iter()
        .any(|(begin, _)| begin.starts_with(&lower) && lower.len() < begin.len())
        || PREFIXES
            .iter()
            .any(|(needle, _, _)| needle.starts_with(&lower) && lower.len() < needle.len())
        || KEY_WORDS
            .iter()
            .any(|(needle, _)| needle.starts_with(&lower) && lower.len() < needle.len())
        || AUTH_SCHEMES
            .iter()
            .any(|needle| needle.starts_with(&lower) && lower.len() < needle.len())
}

/// 下一段是否仍可能把这个“已发布的裸前缀”补成完整凭据头。
///
/// 候选跨中性事件保留；只有当前段确实延续形状时才消费。普通 `y` + `.` 不会
/// 丢字，`AKI` + 中性事件 + `AWEB...` 也不能靠事件边界拆开 AWS 密钥。
fn partial_prefix_continues(pending: &str, next: &str) -> bool {
    if next.is_empty() {
        return true;
    }
    let pending = pending.to_ascii_lowercase();
    let next = next.to_ascii_lowercase();
    let mut combined = String::with_capacity(pending.len() + next.len());
    combined.push_str(&pending);
    combined.push_str(&next);

    let starts_prefix = |needle: &str| {
        if needle.starts_with(&combined) {
            return combined.len() <= needle.len();
        }
        combined.starts_with(needle)
    };
    let token_tail_is_open = |needle: &str| {
        if combined.len() <= needle.len() {
            return true;
        }
        combined
            .as_bytes()
            .get(needle.len()..)
            .is_some_and(|tail| !tail.iter().any(|b| credential_terminator(*b)))
    };

    PEM_PRIVATE_KEYS
        .iter()
        .any(|(begin, _)| starts_prefix(begin))
        || PREFIXES
            .iter()
            .any(|(needle, _, _)| starts_prefix(needle) && token_tail_is_open(needle))
        || KEY_WORDS.iter().any(|(needle, _)| {
            if !starts_prefix(needle) {
                return false;
            }
            let Some(tail) = combined.as_bytes().get(needle.len()..) else {
                return true;
            };
            if tail.is_empty() {
                return true;
            }
            if tail.first() == Some(&b'_') {
                return true;
            }
            let trimmed = tail
                .iter()
                .copied()
                .skip_while(|b| b.is_ascii_whitespace())
                .collect::<Vec<_>>();
            trimmed.first().is_some_and(|b| matches!(b, b':' | b'='))
        })
        || AUTH_SCHEMES.iter().any(|needle| {
            if !starts_prefix(needle) {
                return false;
            }
            let Some(tail) = combined.as_bytes().get(needle.len()..) else {
                return true;
            };
            if tail.is_empty() {
                return true;
            }
            tail.first().is_some_and(u8::is_ascii_whitespace)
                && !tail
                    .iter()
                    .skip_while(|b| b.is_ascii_whitespace())
                    .any(|b| credential_terminator(*b))
        })
}

impl StreamRedactor {
    /// journal / Hub 的字符串出口：正文类字段跨事件 fail closed，防止不同事件、
    /// 字段各自不命中、按顺序拼接后却还原秘密。只有事件根层的既有结构字段
    /// 只做静态检测；嵌套的 `args` / `effects` 全是模型可控正文，不能按字段名豁免。
    /// PEM 的未闭合块也在整个会话内保持状态。
    pub fn redact_json(&mut self, value: &mut Json) -> usize {
        self.redact_json_inner(value, true, true)
    }

    /// 在根对象的 `text` 字段原位置推进一段原始正文。工具输出需要先看见截断的
    /// PEM 头，不能由调用方静态替换后再进门；来源扣留时只推进状态、不发布正文。
    pub fn redact_json_with_text(&mut self, value: &mut Json, text: &str, publish: bool) -> usize {
        let Json::Obj(items) = value else {
            return self.redact_json(value);
        };
        let mut hits = 0usize;
        for (key, value) in items.iter_mut() {
            if key == "text" {
                let (safe, changed) = self.redact_stream_text(text);
                hits += changed;
                if publish {
                    *value = Json::from(safe);
                }
                continue;
            }
            if let Json::Str(text) = value {
                if key_is_secret(key) && text.chars().count() >= 6 {
                    *text = format!("[REDACTED:field:{}]", key);
                    hits += 1;
                    continue;
                }
            }
            let structural = key_is_structural(key);
            hits += self.redact_json_inner(value, !structural, false);
        }
        hits
    }

    /// 单个正文出口；返回值中的计数沿用 JSON 门的“变化字符串字段数”口径。
    pub fn redact_stream_text(&mut self, text: &str) -> (String, usize) {
        let safe = self.push(text);
        let changed = usize::from(safe != text);
        (safe, changed)
    }

    fn redact_json_inner(&mut self, value: &mut Json, stream_text: bool, root: bool) -> usize {
        match value {
            Json::Str(text) => {
                let safe = if stream_text {
                    // 必须先进入跨事件门，再静态替换完整形状。反过来先 redact
                    // 会把未闭合 PEM 头吃掉，下一片的正文就不知道要 suppress。
                    self.push(text)
                } else {
                    redact(text).0
                };
                let changed = usize::from(safe != *text);
                if changed > 0 {
                    *text = safe;
                }
                changed
            }
            Json::Arr(items) => items
                .iter_mut()
                .map(|item| self.redact_json_inner(item, stream_text, false))
                .sum(),
            Json::Obj(items) => {
                let mut hits = 0usize;
                for (key, value) in items.iter_mut() {
                    if let Json::Str(text) = value {
                        if key_is_secret(key) && text.chars().count() >= 6 {
                            *text = format!("[REDACTED:field:{}]", key);
                            hits += 1;
                            continue;
                        }
                    }
                    let structural = root && key_is_structural(key);
                    let child_stream = stream_text && !structural;
                    hits += self.redact_json_inner(value, child_stream, false);
                }
                hits
            }
            _ => 0,
        }
    }

    /// 会话/日志威胁边界切换时重置；同一会话的跨任务分片仍受保护。
    pub fn reset(&mut self) {
        self.pending.clear();
        self.published_prefixes.clear();
        self.published_prefix_bytes = 0;
        self.pem_end = None;
        self.suppress = false;
        self.sealed = false;
    }

    /// 可安全落盘的最小跨进程状态。pending 可能含凭据正文，绝不序列化；
    /// 未闭合 pending 统一降级为 suppress。published_prefixes 则由
    /// `is_bare_partial_prefix` 保证只含公开凭据头的前半段，可精确保存，避免
    /// 普通事件末尾的 `a` / `x` 在重开后吞掉下一段正常文字。PEM 只保存公开
    /// 结束标记编号。状态有界，超限仍 fail closed。
    pub(crate) fn durable_state(&self) -> String {
        if self.sealed {
            return "sealed".to_string();
        }
        let mode = if let Some((index, _)) = PEM_PRIVATE_KEYS
            .iter()
            .enumerate()
            .find(|(_, (_, end))| self.pem_end == Some(*end))
        {
            format!("pem:{}", index)
        } else if self.suppress || self.pem_end.is_some() || !self.pending.is_empty() {
            "suppress".to_string()
        } else if self.published_prefixes.is_empty() {
            return "clear".to_string();
        } else {
            "prefixes".to_string()
        };
        if self.published_prefixes.is_empty() {
            mode
        } else {
            format!("{}:{}", mode, self.published_prefixes.join(","))
        }
    }

    /// 从日志的同一条记录恢复安全状态。缺失/未知状态代表旧版或篡改，不能
    /// 猜测成 clear；调用方必须拒绝续写。恢复只重建保守门，不恢复 pending。
    pub fn restore_durable_state(&mut self, state: &str) -> Result<(), &'static str> {
        self.reset();
        if state == "sealed" {
            self.sealed = true;
            return Ok(());
        }
        if state == "clear" {
            return Ok(());
        }
        let mut parts = state.split(':');
        let mode = parts.next().unwrap_or("");
        let index = if mode == "pem" {
            let raw = parts.next().ok_or("PEM 状态缺少编号")?;
            Some(raw.parse::<usize>().map_err(|_| "PEM 状态编号无效")?)
        } else {
            None
        };
        let encoded = parts.next().unwrap_or("");
        if parts.next().is_some() {
            return Err("脱敏流状态格式无效");
        }
        if let Some(index) = index {
            self.pem_end = Some(
                PEM_PRIVATE_KEYS
                    .get(index)
                    .map(|(_, end)| *end)
                    .ok_or("PEM 状态编号越界")?,
            );
        }
        match mode {
            "suppress" => self.suppress = true,
            "pem" => {}
            "prefixes" if encoded.is_empty() => return Err("公开前缀状态不能为空"),
            "prefixes" => {}
            _ => return Err("日志缺少或包含未知的脱敏流状态"),
        }
        if !encoded.is_empty() {
            let mut bytes = 0usize;
            for prefix in encoded.split(',') {
                if prefix.is_empty()
                    || !is_bare_partial_prefix(prefix)
                    || self.published_prefixes.contains(&prefix.to_string())
                {
                    return Err("脱敏流状态包含无效公开前缀");
                }
                bytes += prefix.len();
                if self.published_prefixes.len() >= MAX_STREAM_PREFIXES
                    || bytes > MAX_STREAM_PREFIX_BYTES
                {
                    return Err("脱敏流状态超过前缀上限");
                }
                self.published_prefixes.push(prefix.to_string());
            }
            self.published_prefix_bytes = bytes;
        } else if mode == "suppress" && index.is_none() {
            // 无后缀的 suppress 是合法的旧状态格式。
        } else if mode == "pem" {
            // 无公开前缀的 PEM 状态合法。
        }
        Ok(())
    }

    fn remember_prefix(&mut self, prefix: String) {
        if self.sealed || self.published_prefixes.contains(&prefix) {
            return;
        }
        if self.published_prefixes.len() >= MAX_STREAM_PREFIXES
            || self.published_prefix_bytes.saturating_add(prefix.len()) > MAX_STREAM_PREFIX_BYTES
        {
            self.sealed = true;
            self.suppress = true;
            self.pending.clear();
            self.published_prefixes.clear();
            self.published_prefix_bytes = 0;
            return;
        }
        self.published_prefix_bytes += prefix.len();
        self.published_prefixes.push(prefix);
    }

    fn remove_prefix(&mut self, index: usize) -> String {
        let prefix = self.published_prefixes.remove(index);
        self.published_prefix_bytes = self.published_prefix_bytes.saturating_sub(prefix.len());
        prefix
    }

    /// 输入一段模型 assistant 正文，返回可安全发布的新正文。
    pub fn push(&mut self, text: &str) -> String {
        if self.sealed {
            return String::new();
        }
        let mut text = text;
        let mut prefix = String::new();
        if self.suppress {
            let Some(at) = text.bytes().position(credential_terminator) else {
                return prefix;
            };
            prefix.push_str("[REDACTED:partial]");
            self.suppress = false;
            text = &text[at..];
        }

        // 最长的候选优先，避免通用首字节抢先消费更具体的 `api_` / `Bearer`。
        let continuation = self
            .published_prefixes
            .iter()
            .enumerate()
            .filter(|(_, candidate)| partial_prefix_continues(candidate, text))
            .max_by_key(|(_, candidate)| candidate.len())
            .map(|(index, _)| index);

        if let Some(index) = continuation {
            let published = self.remove_prefix(index);
            let combined = format!("{published}{text}");
            if is_bare_partial_prefix(&combined) {
                // 新字节仍是裸头的一部分：本次必须发布，不能像旧实现那样吞字。
                self.remember_prefix(combined);
                let mut out = text.to_string();
                out.insert_str(0, &prefix);
                return out;
            }

            // 候选之间的中性事件可能留下另一条 pending。先处理跨候选拼接，
            // 再按原顺序处理后续 pending，避免把中性正文误粘到凭据里。
            let later_pending = std::mem::take(&mut self.pending);
            self.pending = combined;
            let mut out = self.push_part("");
            if out.starts_with(&published) {
                out.drain(..published.len());
            }
            if !later_pending.is_empty() {
                out.push_str(&self.push(&later_pending));
            }
            out.insert_str(0, &prefix);
            return out;
        }

        let mut out = self.push_part(text);
        out.insert_str(0, &prefix);
        out
    }

    fn push_part(&mut self, text: &str) -> String {
        let had_pending = !self.pending.is_empty();
        let mut input = std::mem::take(&mut self.pending);
        input.push_str(text);

        if let Some(end) = self.pem_end {
            let lower = input.as_bytes().to_ascii_lowercase();
            if let Some(rel) = lower
                .windows(end.len())
                .position(|window| window == end.as_bytes())
            {
                input = input[rel + end.len()..].to_string();
                self.pem_end = None;
            } else {
                self.pending = suffix_str(&input, end.len().saturating_sub(1)).to_string();
                return String::new();
            }
        }

        if let Some((at, end)) = open_pem(&input) {
            let mut out = redact(&input[..at]).0;
            out.push_str("[REDACTED:private key]");
            self.pem_end = Some(end);
            self.pending.clear();
            return out;
        }

        if had_pending {
            if let Some((at, end)) = assistant_terminated_candidate(&input) {
                // 未闭合头已经被本次短值加终止符断开。不能把 `Bearer ` 原样
                // 发布后清状态，否则下一字段可继续携带 token 尾部。前缀按
                // partial 扣留，终止符后的普通正文仍递归走完整流门。
                let mut out = redact(&input[..at]).0;
                out.push_str("[REDACTED:partial]");
                out.push_str(&self.push_part(&input[end..]));
                return out;
            }
        }

        let pending = assistant_pending_start(&input).min(partial_pem_start(&input));
        if pending < input.len() {
            let mut out = redact(&input[..pending]).0;
            let tail = &input[pending..];
            if is_bare_partial_prefix(tail) {
                // 只泄露一个公开的凭据头，不泄露 token 正文。先发布以保持普通
                // 正文原样，再用影子状态处理下一段；不能把 `a` / `x` / `y`
                // 缓存在 pending 里，否则正常字段会被吞掉或粘到下一个字段。
                out.push_str(tail);
                self.pending.clear();
                self.remember_prefix(tail.to_string());
            } else if input.len() - pending > MAX_STREAM_PENDING {
                self.pending.clear();
                self.suppress = true;
            } else {
                self.pending = tail.to_string();
            }
            return out;
        }

        redact(&input).0
    }

    /// 显式威胁边界结束：未闭合的可疑前缀只能以占位符收尾。
    ///
    /// 普通 journal 事件、assistant 回调和 JSON 字符串结束**不能**调用它；
    /// 那样会把跨事件前缀清空。只有会话/日志切换才调用 `reset`，需要主动
    /// 关闭一段独立文本流时才调用本方法。
    pub fn finish(&mut self) -> String {
        let pending = !self.pending.is_empty();
        let partial_prefix = !self.published_prefixes.is_empty();
        let open_private_key = self.pem_end.is_some();
        let sealed = self.sealed;
        self.pending.clear();
        self.published_prefixes.clear();
        self.published_prefix_bytes = 0;
        self.pem_end = None;
        self.sealed = false;
        if self.suppress || sealed {
            self.suppress = false;
            "[REDACTED:partial]".to_string()
        } else if pending || partial_prefix || open_private_key {
            "[REDACTED:partial]".to_string()
        } else {
            String::new()
        }
    }
}

/// 递归清洗一个 JSON 值（日志事件、导出、快照共用），返回替换处数。
///
/// 两条判据缺一不可：
/// - 值本身有形状（`sk-…`、`API_KEY=…`）→ 就地替换，周围文字留着，
///   看轨迹的人知道被抹掉的是什么类型的东西；
/// - 字段名自己声明是密钥（`"apiKey": "abc12345"`）→ 整值换掉。这种值
///   单独看没有任何形状特征，模式判不出来，只能靠名字。
///
/// 只碰字符串：`promptTokens: 123` 这类计数是数字，动一下就把预算条改坏了。
pub fn redact_json(value: &mut Json) -> usize {
    match value {
        Json::Str(s) => {
            let (clean, n) = redact(s);
            if n > 0 {
                *s = clean;
            }
            n
        }
        Json::Arr(items) => items.iter_mut().map(redact_json).sum(),
        Json::Obj(items) => {
            let mut hits = 0usize;
            for (key, val) in items.iter_mut() {
                // 6 个字符起才换：`"token": ""`、`"auth": "none"` 换了没意义，
                // 反而把「这里本来就没填」这条信息弄丢。
                if let Json::Str(s) = val {
                    if key_is_secret(key) && s.chars().count() >= 6 {
                        *s = format!("[REDACTED:field:{}]", key);
                        hits += 1;
                        continue;
                    }
                }
                hits += redact_json(val);
            }
            hits
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::Json as J;

    #[test]
    fn json_redaction_follows_the_shape_and_the_field_name() {
        const SHAPE: &str = "fs.read /ws/sk-proj-verylongvalue1234"; // allowlist secret（测试夹具，非真凭据）
        let mut ev = J::obj(vec![
            ("seq", J::n(7.0)),
            ("promptTokens", J::n(1234.0)),
            ("tool", J::s("shell")),
            ("output", J::s("export API_KEY=abcdef0123456789; make")),
            ("args", J::obj(vec![("apiKey", J::s("abc12345"))])),
            ("effects", J::Arr(vec![J::s(SHAPE)])),
        ]);
        let n = redact_json(&mut ev);
        assert!(key_is_secret("apiKey"), "{:?}", key_words("apiKey"));
        assert_eq!(n, 3, "值形状 1 + 字段名 1 + 嵌套值里的形状 1");
        let text = ev.to_compact();
        assert!(!text.contains("abcdef0123456789"), "{}", text);
        assert!(!text.contains("abc12345\""), "{}", text);
        assert!(!text.contains("verylongvalue"), "{}", text);
        // 数字字段原样保留：预算条、审计都靠它们
        assert!(text.contains("\"promptTokens\":1234"), "{}", text);
        assert!(text.contains("\"seq\":7"), "{}", text);
    }

    #[test]
    fn journal_stream_redaction_preserves_structural_metadata() {
        let mut stream = StreamRedactor::default();
        let mut ev = J::obj(vec![
            ("workdir", J::s("/ws/child-a")),
            ("id", J::s("a")),
            ("who", J::s("a")),
            ("withheld", J::s("secrets")),
            ("changed", J::Arr(vec![J::s("a")])),
        ]);
        assert_eq!(stream.redact_json(&mut ev), 0);
        assert_eq!(
            ev.to_compact(),
            r#"{"workdir":"/ws/child-a","id":"a","who":"a","withheld":"secrets","changed":["a"]}"#
        );
    }

    #[test]
    fn journal_stream_preserves_auth_prefix_across_json_events() {
        let mut stream = StreamRedactor::default();
        let mut first = J::obj(vec![("content", J::s("Bearer "))]);
        let mut second = J::obj(vec![(
            "content",
            J::s("abcd+efghijklmnopqrstuvwxyz"), // allowlist secret（测试夹具，非真凭据）
        )]);
        assert!(stream.redact_json(&mut first) > 0);
        assert_eq!(first.str_at("content"), Some(""));
        assert!(stream.redact_json(&mut second) > 0);
        assert!(!second.to_compact().contains("abcd+efghijklmnopqrstuvwxyz"));
        assert!(second.to_compact().contains("[REDACTED:auth header]"));
    }

    #[test]
    fn journal_stream_keeps_pem_state_when_header_is_its_own_event() {
        let mut stream = StreamRedactor::default();
        let mut first = J::obj(vec![(
            "content",
            J::s("-----BEGIN RSA PRIVATE KEY-----"), // allowlist secret（假私钥头）
        )]);
        let body = "WEBJSONPEMCANARY"; // allowlist secret（假私钥正文，非真凭据）
        let mut second = J::obj(vec![(
            "content",
            J::s(&format!("{}\n-----END RSA PRIVATE KEY-----", body)), // allowlist secret（假私钥尾）
        )]);
        assert!(stream.redact_json(&mut first) > 0);
        assert!(first.to_compact().contains("[REDACTED:private key]"));
        assert!(stream.redact_json(&mut second) > 0);
        assert_eq!(second.str_at("content"), Some(""), "PEM 正文必须整段扣留");
    }

    #[test]
    fn nested_model_controlled_fields_do_not_bypass_the_stream_gate() {
        let mut stream = StreamRedactor::default();
        let mut first = J::obj(vec![("args", J::obj(vec![("path", J::s("Bearer "))]))]);
        let mut second = J::obj(vec![(
            "args",
            J::obj(vec![(
                "url",
                J::s("abcd+efghijklmnopqrstuvwxyz"), // allowlist secret（测试夹具，非真凭据）
            )]),
        )]);
        assert!(stream.redact_json(&mut first) > 0);
        assert!(stream.redact_json(&mut second) > 0);
        assert!(!second.to_compact().contains("abcd+efghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn bare_prefix_bytes_do_not_corrupt_unrelated_metadata() {
        let mut stream = StreamRedactor::default();
        assert_eq!(stream.push("a"), "a");
        assert_eq!(stream.push("K"), "K");
        assert_eq!(stream.push("."), ".");
        assert_eq!(stream.push(".txt"), ".txt");
        assert_eq!(stream.push("x"), "x");
        assert_eq!(stream.push(" next"), " next");

        let mut stream = StreamRedactor::default();
        assert_eq!(stream.push("A"), "A");
        assert!(stream
            .push("KIAWEBASSISTANTTEST12")
            .contains("[REDACTED:aws key]")); // allowlist secret（测试夹具，非真凭据）
    }

    #[test]
    fn published_prefixes_survive_neutral_events() {
        for (secret, split) in [
            ("AKIAWEBASSISTANTTEST12", 3), // allowlist secret（测试夹具，非真凭据）
            ("Bearer abcd+efghijklmnopqrstuvwxyz", 4), // allowlist secret（测试夹具，非真凭据）
            ("Basic dGVzdDp0ZXN0LXNlY3JldC12YWx1ZQ==", 3), // allowlist secret（假凭据）
            ("api_key=abcdefgh1234", 6),   // allowlist secret（测试夹具，非真凭据）
            ("ya29.webassistantoauthvalue", 3), // allowlist secret（测试夹具，非真凭据）
            ("xoxb-webassistantslackvalue", 3), // allowlist secret（测试夹具，非真凭据）
        ] {
            let mut stream = StreamRedactor::default();
            let first = stream.push(&secret[..split]);
            let neutral = stream.push("neutral metadata");
            assert_eq!(neutral, "neutral metadata", "{:?}", secret);
            let second = stream.push(&secret[split..]);
            let rebuilt = format!("{}{}", first, second);
            assert!(!rebuilt.contains(secret), "{:?} -> {:?}", secret, rebuilt);
            assert!(
                rebuilt.contains("[REDACTED:"),
                "{:?} -> {:?}",
                secret,
                rebuilt
            );
        }
    }

    #[test]
    fn pending_auth_headers_are_not_released_by_short_terminated_values() {
        for head in ["Bearer ", "Basic ", "api_key="] {
            let mut stream = StreamRedactor::default();
            assert_eq!(stream.push(head), "", "{:?}", head);
            let neutral = stream.push("x;");
            assert!(!neutral.contains(head), "{:?} -> {:?}", head, neutral);
            assert!(neutral.contains("[REDACTED:partial]"), "{:?}", neutral);
            let tail = stream.push("abcd+efghijklmnopqrstuvwxyz"); // allowlist secret（测试夹具，非真凭据）
            let rebuilt = format!("{}{}", neutral, tail);
            assert!(
                !rebuilt.contains(&format!("{}abcd+efghijklmnopqrstuvwxyz", head)),
                "{:?} -> {:?}",
                head,
                rebuilt
            );
            assert_eq!(stream.push(" ordinary text"), " ordinary text");
        }
    }

    #[test]
    fn root_tool_names_do_not_bypass_the_stream_gate() {
        let mut stream = StreamRedactor::default();
        let mut first = J::obj(vec![("tool", J::s("AKI"))]);
        let mut neutral = J::obj(vec![("type", J::s("tool/end"))]);
        let mut second = J::obj(vec![
            ("tool", J::s("AWEBASSISTANTTEST12")), // allowlist secret（测试夹具，非真凭据）
        ]);
        stream.redact_json(&mut first);
        stream.redact_json(&mut neutral);
        stream.redact_json(&mut second);
        let rebuilt = format!(
            "{}{}",
            first.str_at("tool").unwrap_or(""),
            second.str_at("tool").unwrap_or("")
        );
        assert!(!rebuilt.contains("AKIAWEBASSISTANTTEST12"), "{}", rebuilt); // allowlist secret（测试夹具，非真凭据）
        assert!(rebuilt.contains("[REDACTED:"), "{}", rebuilt);
    }

    #[test]
    fn root_path_and_artifact_fields_do_not_bypass_the_stream_gate() {
        const AWS: &str = "AKIAWEBASSISTANTTEST12"; // allowlist secret（测试夹具，非真凭据）
        for key in ["path", "paths", "file", "filename", "added", "artifact"] {
            let mut stream = StreamRedactor::default();
            let mut first = J::obj(vec![(key, J::s("AKI"))]);
            let mut neutral = J::obj(vec![("type", J::s("tool/end"))]);
            let mut second = J::obj(vec![
                (key, J::s("AWEBASSISTANTTEST12")), // allowlist secret（测试夹具，非真凭据）
            ]);
            stream.redact_json(&mut first);
            stream.redact_json(&mut neutral);
            stream.redact_json(&mut second);
            let rebuilt = format!(
                "{}{}",
                first.str_at(key).unwrap_or(""),
                second.str_at(key).unwrap_or("")
            );
            assert!(!rebuilt.contains(AWS), "{}: {}", key, rebuilt);
            assert!(rebuilt.contains("[REDACTED:"), "{}: {}", key, rebuilt);
        }
    }

    #[test]
    fn pem_partial_header_does_not_require_a_word_boundary() {
        let mut stream = StreamRedactor::default();
        let first = stream.push("x-----BEGIN RSA PRIVATE K");
        let second = stream.push("EY-----BEGIN RSA PRIVATE KEY-----"); // allowlist secret（测试夹具，非真凭据）
        let third = stream.push("c2VjcmV0LWJvZHktdGVzdA==");
        let fourth = stream.push("-----END RSA PRIVATE KEY-----");
        let rebuilt = format!("{}{}{}{}", first, second, third, fourth);
        assert!(!rebuilt.contains("c2VjcmV0LWJvZHktdGVzdA=="), "{}", rebuilt);
        assert!(rebuilt.contains("[REDACTED:private key]"), "{}", rebuilt);
    }

    #[test]
    fn finish_closes_an_unterminated_pem_threat_boundary() {
        const PEM: &str = "-----BEGIN PRIVATE KEY-----"; // allowlist secret（测试夹具，非真凭据）
        let mut stream = StreamRedactor::default();
        assert_eq!(stream.push(PEM), "[REDACTED:private key]");
        assert_eq!(stream.push("c2VjcmV0"), "");
        assert_eq!(stream.finish(), "[REDACTED:partial]");
        assert_eq!(stream.push("ordinary text"), "ordinary text");
    }

    #[test]
    fn secret_key_names_match_words_not_arbitrary_substrings() {
        for name in ["monkey", "tokenizer", "author", "secretary", "KEYBOARD"] {
            assert!(!key_is_secret(name), "普通键名被误伤: {}", name);
        }
        for name in [
            "apiKey",
            "API_KEY",
            "clientSecret",
            "Authorization",
            "AUTH_TOKEN",
        ] {
            assert!(key_is_secret(name), "密钥键名未识别: {}", name);
        }
        for name in ["PATH", "API_KEY_PATH", "credentialPath", "TOKEN_PATHS"] {
            assert!(!key_is_secret(name), "路径键名被误伤: {}", name);
        }
    }

    #[test]
    fn ordinary_key_like_words_stay_visible_but_extended_key_names_stay_protected() {
        let mut stream = StreamRedactor::default();
        assert_eq!(stream.push("secrets"), "secrets");
        assert_eq!(stream.push(" next"), " next");

        let mut stream = StreamRedactor::default();
        assert_eq!(stream.push("api_key_"), "");
        let shown = stream.push("id=abcdefgh1234"); // allowlist secret（测试夹具，非真凭据）
        assert!(!shown.contains("abcdefgh1234"), "{}", shown);
        assert!(shown.contains("[REDACTED:"), "{}", shown);
    }

    #[test]
    fn observation_filename_arrays_remain_structural() {
        let mut stream = StreamRedactor::default();
        let mut ev = J::obj(vec![
            ("added", J::Arr(vec![J::s("a")])),
            ("removed", J::Arr(vec![J::s("b")])),
        ]);
        assert_eq!(stream.redact_json(&mut ev), 0);
        assert_eq!(ev.to_compact(), r#"{"added":["a"],"removed":["b"]}"#);
    }

    #[test]
    fn short_values_under_a_secret_field_stay_visible() {
        // `"token": ""` 换了等于把「这里没填」这条信息弄丢
        let mut ev = J::obj(vec![("token", J::s("none"))]);
        assert_eq!(redact_json(&mut ev), 0);
        assert_eq!(ev.to_compact(), r#"{"token":"none"}"#);
    }

    fn labels(text: &str) -> Vec<&'static str> {
        find(text).iter().map(|s| s.label).collect()
    }

    #[test]
    fn named_secrets_are_located_in_the_middle_of_a_line() {
        // 日志行就是一段塞满噪音的文本，判据必须能在里面挖出值
        let line = r#"{"content":"run: export API_KEY=abcdef0123456789 && make"}"#;
        let (clean, n) = redact(line);
        assert_eq!(n, 1, "{:?}", find(line));
        assert!(!clean.contains("abcdef0123456789"), "{}", clean);
        assert!(clean.contains("[REDACTED:api key]"), "{}", clean);
        assert!(clean.contains("make"), "上下文不能一起被抹掉: {}", clean);
    }

    #[test]
    fn key_prefix_must_be_a_real_key_name() {
        assert_eq!(labels(r#""promptTokens":123"#), Vec::<&str>::new());
        assert_eq!(labels(r#""completion_tokens":4567"#), Vec::<&str>::new());
        assert_eq!(labels("TOKEN=<your-token>"), Vec::<&str>::new());
        assert_eq!(labels("token: ${TOKEN}"), Vec::<&str>::new());
        assert_eq!(labels("password: hunter2"), Vec::<&str>::new(), "6 位太短");
        // 下划线是键名的一部分，不是「前一个词」
        assert!(!labels("ACCESS_TOKEN=abcdefgh1234").is_empty());
        assert!(!labels("client-secret: abcdefgh1234").is_empty());
    }

    #[test]
    fn well_known_token_shapes_need_no_key_name() {
        for sample in [
            "key sk-proj-verylongvalue1234 end", // 测试夹具：密钥形状，非真凭据（allowlist secret）
            "ghp_A1b2C3d4E5f6G7h8I9j0 done",
            "AKIA1234567890ABCDEF", // 测试夹具：密钥形状，非真凭据（allowlist secret）
            "AIzaSyA1234567890abcdefghijklmnopqrstuvw",
            "glpat-AbCdEf123456",
            "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
        ] {
            assert!(!find(sample).is_empty(), "{} 应该被判成密钥", sample);
            let (clean, n) = redact(sample);
            assert!(n >= 1, "{}", sample);
            assert!(find(&clean).is_empty(), "脱敏后不该还剩东西: {}", clean);
        }
    }

    #[test]
    fn pem_private_keys_are_redacted_as_whole_blocks() {
        for (begin, end) in [
            ("PRIVATE KEY", "PRIVATE KEY"),
            ("RSA PRIVATE KEY", "RSA PRIVATE KEY"),
            ("EC PRIVATE KEY", "EC PRIVATE KEY"),
            ("DSA PRIVATE KEY", "DSA PRIVATE KEY"),
            ("OPENSSH PRIVATE KEY", "OPENSSH PRIVATE KEY"),
            ("ENCRYPTED PRIVATE KEY", "ENCRYPTED PRIVATE KEY"),
        ] {
            let block = format!(
                "-----BEGIN {}-----\nQWJDREVGR0hJSktMTU5PUA==\n-----END {}-----",
                begin, end
            ); // allowlist secret（测试夹具：假私钥块，非真凭据）
            let text = format!("before\n{}\nafter", block);
            let (clean, n) = redact(&text);
            assert_eq!(n, 1, "{}", text);
            assert!(clean.starts_with("before\n"), "{}", clean);
            assert!(
                clean.ends_with("\nafter"),
                "前面的正文不能一起被抹掉: {}",
                clean
            );
            assert!(!clean.contains("QWJDREVGR0hJSktMTU5PUA=="), "{}", clean);
            assert!(clean.contains("[REDACTED:private key]"), "{}", clean);
            assert!(
                find(&clean).is_empty(),
                "脱敏后不该剩私钥形状: {:?}",
                find(&clean)
            );
        }
        for block in [
            "---- BEGIN SSH2 ENCRYPTED PRIVATE KEY ----\nQWJDREVGR0hJSktMTU5PUA==\n---- END SSH2 ENCRYPTED PRIVATE KEY ----", // allowlist secret（假私钥块）
            "-----BEGIN PGP PRIVATE KEY BLOCK-----\nQWJDREVGR0hJSktMTU5PUA==\n-----END PGP PRIVATE KEY BLOCK-----", // allowlist secret（假私钥块）
        ] {
            let (clean, n) = redact(block);
            assert_eq!(n, 1, "{}", block);
            assert!(!clean.contains("QWJDREVGR0hJSktMTU5PUA=="), "{}", clean);
            assert!(clean.contains("[REDACTED:private key]"), "{}", clean);
        }
    }

    #[test]
    fn truncated_private_key_fails_closed_but_public_certificates_stay_visible() {
        let truncated = "-----BEGIN RSA PRIVATE KEY-----\nQWJDREVGR0hJSktMTU5PUA==\n尾部也要保密"; // allowlist secret（测试夹具，非真凭据）
        let (clean, n) = redact(truncated);
        assert_eq!(n, 1);
        assert!(!clean.contains("QWJDREVGR0hJSktMTU5PUA=="), "{}", clean);
        assert!(
            !clean.contains("尾部也要保密"),
            "缺 END 的私钥正文不能留下: {}",
            clean
        );

        let certificate =
            "-----BEGIN CERTIFICATE-----\nQWJDREVGR0hJSktMTU5PUA==\n-----END CERTIFICATE-----";
        assert!(
            find(certificate).is_empty(),
            "公钥证书不是私钥: {:?}",
            find(certificate)
        );
    }

    #[test]
    fn repeated_pem_headers_are_scanned_once_and_fail_closed() {
        let header = "-----BEGIN RSA PRIVATE KEY-----"; // allowlist secret（测试夹具，非真凭据）
        let text = header.repeat(4096);
        let spans = find(&text);
        assert_eq!(spans.len(), 1, "重复头必须合并为单个截断私钥块");
        assert_eq!(spans[0].start, 0);
        assert_eq!(spans[0].end, text.len());
        assert_eq!(redact(&text).1, 1);
    }

    #[test]
    fn bearer_accepts_ascii_whitespace_but_not_placeholders() {
        for sample in [
            "Bearer   eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            "Bearer\teyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            "Bearer\r\neyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            "bEaReR\neyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            "Bearer abcd+efghijklmnopqrstuvwxyz", // allowlist secret（测试夹具，非真凭据）
            "Bearer abcd/efghijklmnopqrstuvwxyz", // allowlist secret（测试夹具，非真凭据）
            "Bearer abcd~efghijklmnopqrstuvwxyz", // allowlist secret（测试夹具，非真凭据）
            "Bearer abcdefghijklmnopqrstuvwxyz=", // allowlist secret（测试夹具，非真凭据）
            "Authorization: Basic dGVzdDp0ZXN0LXNlY3JldC12YWx1ZQ==", // allowlist secret（假凭据）
        ] {
            let (clean, n) = redact(sample);
            assert_eq!(n, 1, "{:?}", sample);
            assert!(clean.contains("[REDACTED:auth header]"), "{}", clean);
            assert!(find(&clean).is_empty(), "{}", clean);
        }
        for sample in [
            "Bearer <token>",
            "Bearer ${TOKEN}",
            "Bearer short",
            "promptBearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
        ] {
            assert!(find(sample).is_empty(), "不该误伤: {:?}", sample);
        }
    }

    #[test]
    fn assistant_stream_blocks_every_two_way_split_of_prefixed_secrets() {
        for secret in [
            "AKIAWEBASSISTANTTEST12", // allowlist secret（测试夹具，非真凭据）
            "Bearer abcd+efghijklmnopqrstuvwxyz", // allowlist secret（测试夹具，非真凭据）
            "Basic dGVzdDp0ZXN0LXNlY3JldC12YWx1ZQ==", // allowlist secret（假凭据）
            "api_key=abcdefgh1234",   // allowlist secret（测试夹具，非真凭据）
        ] {
            for split in 0..=secret.len() {
                let mut stream = StreamRedactor::default();
                let mut shown = stream.push(&secret[..split]);
                shown.push_str(&stream.push(&secret[split..]));
                assert!(
                    !shown.contains(secret),
                    "split={} 仍可拼接还原: {}",
                    split,
                    shown
                );
            }
            let mut stream = StreamRedactor::default();
            let mut shown = String::new();
            for byte in secret.bytes() {
                shown.push_str(&stream.push(""));
                shown.push_str(&stream.push(&String::from_utf8_lossy(&[byte])));
            }
            assert!(
                !shown.contains(secret),
                "secret={:?} 逐字节事件仍可拼接还原: {}",
                secret,
                shown
            );
        }
        for prose in ["normal reply", "bearerish prose"] {
            for split in 0..=prose.len() {
                let mut stream = StreamRedactor::default();
                let mut shown = stream.push(&prose[..split]);
                shown.push_str(&stream.push(&prose[split..]));
                // 一个后续中性事件用于确认尾部不是密钥前缀。
                shown.push_str(&stream.push("."));
                assert_eq!(
                    shown,
                    format!("{}.", prose),
                    "正常正文被误脱敏: split={}",
                    split
                );
            }
        }
        let mut stream = StreamRedactor::default();
        // 裸头本身不是秘密，先原样发布；显式结束一条独立流时仍补占位符，
        // 表示这个未闭合候选没有作为可复用凭据继续下去。
        assert_eq!(stream.push("api_"), "api_");
        assert_eq!(stream.finish(), "[REDACTED:partial]");
        assert_eq!(stream.finish(), "");

        let mut stream = StreamRedactor::default();
        assert_eq!(stream.push("api_key="), "");
        assert_eq!(stream.push(&"$".repeat(MAX_STREAM_PENDING + 100)), "");
        assert!(stream.pending.len() <= MAX_STREAM_PENDING);
        assert_eq!(stream.push(" next"), "[REDACTED:partial] next");

        let mut stream = StreamRedactor::default();
        const PEM: &str = "-----BEGIN RSA PRIVATE KEY-----"; // allowlist secret（假私钥头）
        assert!(stream.push(PEM).contains("[REDACTED:"));
        stream.reset();
        assert_eq!(stream.push("new session"), "new session");
    }

    #[test]
    fn assistant_stream_suppresses_pem_body_across_every_split() {
        let body = "WEBSTREAMPEMTESTCANARY"; // allowlist secret（假私钥正文，非真凭据）
        let pem = format!(
            "-----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----", // allowlist secret（假私钥块）
            body
        ); // allowlist secret（测试夹具，非真凭据）
        for split in 0..=pem.len() {
            let mut stream = StreamRedactor::default();
            let first = stream.push(&pem[..split]);
            let second = stream.push(&pem[split..]);
            let mut shown = first.clone();
            shown.push_str(&second);
            assert!(
                !shown.contains(body),
                "split={} first={:?} second={:?} 泄漏 PEM 正文: {}",
                split,
                first,
                second,
                shown
            );
            assert!(
                !shown.contains(&pem),
                "split={} 泄漏完整 PEM: {}",
                split,
                shown
            );
        }
        let mut stream = StreamRedactor::default();
        let mut shown = String::new();
        for byte in pem.bytes() {
            shown.push_str(&stream.push(&String::from_utf8_lossy(&[byte])));
        }
        assert!(
            !shown.contains(body),
            "逐字节 PEM 事件仍泄漏正文: {}",
            shown
        );
    }

    #[test]
    fn shapes_that_are_not_secrets_stay_put() {
        for sample in [
            "PATH=/usr/local/bin:/usr/bin",
            "MODEL_PATH=/ws/models/x.gguf",
            "steps=12 timeout=900",
            "v1.2.3",
            "2026-11-19T07:00:00Z",
            "sk-short",
            "github_pat_",
            "版本号 12345678",
        ] {
            assert!(
                find(sample).is_empty(),
                "{} 被误判: {:?}",
                sample,
                find(sample)
            );
            assert_eq!(redact(sample).1, 0);
        }
    }

    #[test]
    fn overlapping_spans_are_merged_not_double_replaced() {
        // 两个键名 + 一个前缀同时命中同一段文本：下标必须还能对上原文
        let line = "AUTH_TOKEN=\"sk-proj-verylongvalue1234\"; echo ok"; // 测试夹具：密钥形状，非真凭据（allowlist secret）
        let (clean, n) = redact(line);
        assert!(clean.contains("echo ok"), "{}", clean);
        assert!(!clean.contains("verylongvalue"), "{}", clean);
        assert!(n >= 1);
        // 替换后的文本不该留残余
        assert!(find(&clean).is_empty(), "{:?}", find(&clean));
    }

    #[test]
    fn byte_offsets_survive_multibyte_text() {
        let line = "模型说：api_key=abcdef0123456789，请保存。";
        let (clean, n) = redact(line);
        assert_eq!(n, 1);
        assert!(clean.starts_with("模型说："), "{}", clean);
        assert!(clean.contains("，请保存。"), "{}", clean);
        assert!(!clean.contains("abcdef0123456789"));
    }

    #[test]
    fn key_name_heuristic_keeps_paths() {
        assert!(key_is_secret("OPENAI_API_KEY"));
        assert!(key_is_secret("aws_secret_access_key"));
        assert!(key_is_secret("Authorization"));
        assert!(!key_is_secret("PATH"));
        assert!(!key_is_secret("HOME"));
        assert!(!key_is_secret("UI_MODEL_PATH"));
        assert!(!key_is_secret("RUST_LOG"));
    }
}
