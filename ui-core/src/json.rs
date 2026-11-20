//! Minimal JSON: enough for the journal, the policy file and model scripts.
//!
//! Zero dependencies on purpose. The journal is written and re-read by the same
//! kernel, so the encoder must be byte-stable: same value, same bytes, so that
//! the hash chain in `journal` means something.

use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn obj(items: Vec<(&str, Json)>) -> Json {
        Json::Obj(items.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    pub fn s(v: impl Into<String>) -> Json {
        Json::Str(v.into())
    }

    pub fn n(v: impl Into<f64>) -> Json {
        Json::Num(v.into())
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(items) => items.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn str_at(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Json::as_str)
    }

    pub fn num_at(&self, key: &str) -> Option<f64> {
        self.get(key).and_then(Json::as_f64)
    }

    pub fn u64_at(&self, key: &str) -> Option<u64> {
        self.num_at(key).map(|v| v as u64)
    }

    pub fn bool_at(&self, key: &str) -> Option<bool> {
        self.get(key).and_then(Json::as_bool)
    }

    pub fn arr_at(&self, key: &str) -> Option<&Vec<Json>> {
        self.get(key).and_then(Json::as_arr)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&Vec<Json>> {
        match self {
            Json::Arr(v) => Some(v),
            _ => None,
        }
    }

    /// Insert or replace a field, preserving order for new keys.
    pub fn set(&mut self, key: &str, value: Json) {
        if let Json::Obj(items) = self {
            if let Some(slot) = items.iter_mut().find(|(k, _)| k == key) {
                slot.1 = value;
                return;
            }
            items.push((key.to_string(), value));
        }
    }

    /// Drop a field, returning its value. Used when re-keying an event into a
    /// new journal (envelope fields belong to the writer, not the payload).
    pub fn remove(&mut self, key: &str) -> Option<Json> {
        match self {
            Json::Obj(items) => items
                .iter()
                .position(|(k, _)| k == key)
                .map(|at| items.remove(at).1),
            _ => None,
        }
    }

    pub fn to_compact(&self) -> String {
        let mut out = String::new();
        self.write_compact(&mut out);
        out
    }

    pub fn to_pretty(&self) -> String {
        let mut out = String::new();
        self.write_pretty(&mut out, 0);
        out
    }

    fn write_compact(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Num(v) => write_num(out, *v),
            Json::Str(s) => write_str(out, s),
            Json::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_compact(out);
                }
                out.push(']');
            }
            Json::Obj(items) => {
                out.push('{');
                for (i, (k, v)) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_str(out, k);
                    out.push(':');
                    v.write_compact(out);
                }
                out.push('}');
            }
        }
    }

    fn write_pretty(&self, out: &mut String, depth: usize) {
        let pad = |n: usize| "  ".repeat(n);
        match self {
            Json::Arr(items) if !items.is_empty() => {
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    out.push_str(&pad(depth + 1));
                    item.write_pretty(out, depth + 1);
                    if i + 1 < items.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str(&pad(depth));
                out.push(']');
            }
            Json::Obj(items) if !items.is_empty() => {
                out.push_str("{\n");
                for (i, (k, v)) in items.iter().enumerate() {
                    out.push_str(&pad(depth + 1));
                    write_str(out, k);
                    out.push_str(": ");
                    v.write_pretty(out, depth + 1);
                    if i + 1 < items.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str(&pad(depth));
                out.push('}');
            }
            other => other.write_compact(out),
        }
    }
}

impl From<&str> for Json {
    fn from(v: &str) -> Self {
        Json::Str(v.to_string())
    }
}
impl From<String> for Json {
    fn from(v: String) -> Self {
        Json::Str(v)
    }
}
impl From<bool> for Json {
    fn from(v: bool) -> Self {
        Json::Bool(v)
    }
}
impl From<f64> for Json {
    fn from(v: f64) -> Self {
        Json::Num(v)
    }
}
impl From<i64> for Json {
    fn from(v: i64) -> Self {
        Json::Num(v as f64)
    }
}
impl From<i32> for Json {
    fn from(v: i32) -> Self {
        Json::Num(v as f64)
    }
}
impl From<u64> for Json {
    fn from(v: u64) -> Self {
        Json::Num(v as f64)
    }
}
impl From<usize> for Json {
    fn from(v: usize) -> Self {
        Json::Num(v as f64)
    }
}

fn write_num(out: &mut String, v: f64) {
    if !v.is_finite() {
        out.push_str("null");
        return;
    }
    if v.fract() == 0.0 && v.abs() < 9.0e15 {
        let _ = write!(out, "{}", v as i64);
    } else {
        let _ = write!(out, "{}", v);
    }
}

fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// 递归深度上限。没有它，一条 `[[[[…` 的模型输出/日志行会直接打爆进程栈
/// （stack overflow 是 abort，catch_unwind 也救不了）。
const MAX_DEPTH: usize = 128;

pub fn parse(input: &str) -> Result<Json, String> {
    // Windows 上到处是带 BOM 的 JSON（PowerShell 的 Set-Content、记事本都会加）：
    // 不剥掉 BOM，Windows 用户的第一步就会失败。
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let mut p = Parser {
        b: input.as_bytes(),
        i: 0,
        depth: 0,
    };
    p.skip_ws();
    let value = p.value()?;
    p.skip_ws();
    if p.i != p.b.len() {
        return Err(format!("trailing data at byte {}", p.i));
    }
    Ok(value)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek();
        if c.is_some() {
            self.i += 1;
        }
        c
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')) {
            self.i += 1;
        }
    }

    fn enter(&mut self) -> Result<(), String> {
        if self.depth >= MAX_DEPTH {
            return Err(format!(
                "nesting deeper than {} at byte {}",
                MAX_DEPTH, self.i
            ));
        }
        self.depth += 1;
        Ok(())
    }

    fn value(&mut self) -> Result<Json, String> {
        match self.peek() {
            Some(b'{') => {
                self.enter()?;
                let v = self.object();
                self.depth -= 1;
                v
            }
            Some(b'[') => {
                self.enter()?;
                let v = self.array();
                self.depth -= 1;
                v
            }
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            Some(c) => Err(format!("unexpected '{}' at byte {}", c as char, self.i)),
            None => Err("unexpected end of input".to_string()),
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> Result<Json, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(value)
        } else {
            Err(format!("invalid literal at byte {}", self.i))
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.bump(); // {
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.bump();
            return Ok(Json::Obj(items));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            if self.bump() != Some(b':') {
                return Err(format!("expected ':' at byte {}", self.i));
            }
            self.skip_ws();
            let value = self.value()?;
            items.push((key, value));
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b'}') => return Ok(Json::Obj(items)),
                _ => return Err(format!("expected ',' or '}}' at byte {}", self.i)),
            }
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.bump(); // [
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.bump();
            return Ok(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b']') => return Ok(Json::Arr(items)),
                _ => return Err(format!("expected ',' or ']' at byte {}", self.i)),
            }
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.bump();
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.bump();
        }
        if self.peek() == Some(b'.') {
            self.bump();
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.bump();
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.bump();
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.bump();
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.bump();
            }
        }
        let raw = std::str::from_utf8(&self.b[start..self.i]).map_err(|e| e.to_string())?;
        let value = raw
            .parse::<f64>()
            .map_err(|_| format!("bad number '{}'", raw))?;
        // JSON 没有 Infinity/NaN。接受 1e400 会得到 inf，而编码器把它写成
        // `null`，往返立刻变形——日志/快照的字节稳定性就没了。
        if !value.is_finite() {
            return Err(format!("number out of range: {}", raw));
        }
        Ok(Json::Num(value))
    }

    fn string(&mut self) -> Result<String, String> {
        if self.bump() != Some(b'"') {
            return Err(format!("expected string at byte {}", self.i));
        }
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = self.bump().ok_or_else(|| "unterminated string".to_string())?;
            match c {
                b'"' => break,
                b'\\' => {
                    let esc = self.bump().ok_or_else(|| "unterminated escape".to_string())?;
                    match esc {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xD800..=0xDBFF).contains(&hi) {
                                if self.bump() != Some(b'\\') || self.bump() != Some(b'u') {
                                    return Err("lone high surrogate".to_string());
                                }
                                let lo = self.hex4()?;
                                if !(0xDC00..=0xDFFF).contains(&lo) {
                                    return Err("invalid low surrogate".to_string());
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else {
                                hi
                            };
                            let ch = char::from_u32(cp)
                                .ok_or_else(|| format!("invalid code point U+{:04X}", cp))?;
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        other => return Err(format!("bad escape '\\{}'", other as char)),
                    }
                }
                c if c < 0x20 => return Err("raw control character in string".to_string()),
                c => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|e| format!("invalid utf-8 in string: {}", e))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let mut v: u32 = 0;
        for _ in 0..4 {
            let c = self.bump().ok_or_else(|| "truncated \\u escape".to_string())?;
            let d = (c as char)
                .to_digit(16)
                .ok_or_else(|| format!("bad hex digit '{}'", c as char))?;
            v = v * 16 + d;
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_nested_values() {
        let src = r#"{"a":[1,2.5,"x",true,null],"b":{"c":"\u4f60\u597d"}}"#;
        let v = parse(src).unwrap();
        let again = parse(&v.to_compact()).unwrap();
        assert_eq!(v, again);
        assert_eq!(v.get("b").unwrap().str_at("c"), Some("你好"));
    }

    #[test]
    fn rejects_truncated_input() {
        assert!(parse(r#"{"a": "unterminated"#).is_err());
        assert!(parse("[1,2").is_err());
        assert!(parse("{\"a\":1} trailing").is_err());
    }

    #[test]
    fn preserves_key_order_for_stable_hashing() {
        let v = Json::obj(vec![("z", Json::n(1)), ("a", Json::n(2))]);
        assert_eq!(v.to_compact(), r#"{"z":1,"a":2}"#);
    }

    #[test]
    fn parses_negative_and_exponent_numbers() {
        assert_eq!(parse("-1.5e3").unwrap(), Json::Num(-1500.0));
    }
}
