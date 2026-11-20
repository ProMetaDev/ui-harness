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
    ("bearer ", 16, "auth header"),
];

/// 名字本身声明「这是密钥」的键（环境变量名、JSON 字段名）。
///
/// `PATH` / `*_PATH` 单独放过：内核会把工作区、模型路径这类变量喂给模型，
/// 一律打码等于砍掉环境感知，而它们泄露的不是凭据。
pub fn key_is_secret(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    if up == "PATH" || up.ends_with("_PATH") || up.ends_with("_PATHS") {
        return false;
    }
    [
        "KEY",
        "SECRET",
        "TOKEN",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "AUTH",
    ]
    .iter()
    .any(|needle| up.contains(needle))
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
    if v.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-') {
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
        let mut ev = J::obj(vec![
            ("seq", J::n(7.0)),
            ("promptTokens", J::n(1234.0)),
            ("tool", J::s("shell")),
            ("output", J::s("export API_KEY=abcdef0123456789; make")),
            ("args", J::obj(vec![("apiKey", J::s("abc12345"))])),
            ("effects", J::Arr(vec![J::s("fs.read /ws/sk-proj-verylongvalue1234")])),
        ]);
        let n = redact_json(&mut ev);
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
            "key sk-proj-verylongvalue1234 end",
            "ghp_A1b2C3d4E5f6G7h8I9j0 done",
            "AKIA1234567890ABCDEF",
            "AIzaSyA1234567890abcdefghijklmnopqrstuvw",
            "glpat-AbCdEf123456",
            "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
        ] {
            assert!(!find(sample).is_empty(), "{} 应该被判成密钥", sample);
            let (clean, n) = redact(sample);
            assert!(n >= 1, "{}", sample);
            assert!(
                find(&clean).is_empty(),
                "脱敏后不该还剩东西: {}",
                clean
            );
        }
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
            assert!(find(sample).is_empty(), "{} 被误判: {:?}", sample, find(sample));
            assert_eq!(redact(sample).1, 0);
        }
    }

    #[test]
    fn overlapping_spans_are_merged_not_double_replaced() {
        // 两个键名 + 一个前缀同时命中同一段文本：下标必须还能对上原文
        let line = "AUTH_TOKEN=\"sk-proj-verylongvalue1234\"; echo ok";
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
