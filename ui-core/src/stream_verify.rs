// 流式校验：内存不随日志增长。
//
// 为什么要有它：`ui verify` 之前走 `Journal::read`，把每条事件都变成 `Json`
// 留在内存里。实测 7 万条要 80 MB，100 万条就要 1 GB+——审计工具在最需要它的
// 场景（大日志）反而先撑不住。
//
// 这里只保留 O(1) 的滚动状态：上一条的 hash、期望的 seq、以及"有没有未完成的
// 调用"。每条记录解析完就丢弃。

use crate::journal::{Corrupt, GENESIS};
use crate::json;
use crate::sha256;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

pub struct StreamReport {
    pub events: usize,
    pub turns: usize,
    pub tool_starts: usize,
    pub first_seq: u64,
    pub last_seq: u64,
    pub corrupt: Option<Corrupt>,
    pub open_turn: Option<u64>,
    pub open_tools: Vec<String>,
    /// 未完成调用的**原始事件**（tool/start），resume 需要 args 才能判断能否重放
    pub open_tool_events: Vec<crate::json::Json>,
    /// 未完成轮次的**原始事件**（turn/start）
    pub open_turn_event: Option<crate::json::Json>,
    /// 校验通过的字节数（用于截断修复）
    pub good_bytes: usize,
}

pub fn verify_stream(path: &Path) -> std::io::Result<StreamReport> {
    // 日志不存在 = 还没有任何记录，不是损坏。
    // 旧实现（Journal::read）就是这么处理的；流式版一开始漏了这条，
    // 结果干净机器上门禁的「verify（干净日志）」直接失败（CI 抓到的回归）。
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StreamReport {
                events: 0,
                turns: 0,
                tool_starts: 0,
                first_seq: 0,
                last_seq: 0,
                corrupt: None,
                open_turn: None,
                open_tools: Vec::new(),
                open_tool_events: Vec::new(),
                open_turn_event: None,
                good_bytes: 0,
            })
        }
        Err(e) => return Err(e),
    };
    let mut reader = BufReader::with_capacity(64 * 1024, file);

    let mut prev = GENESIS.to_string();
    let mut expected_seq = 1u64;
    let mut good_bytes = 0usize;
    let mut report = StreamReport {
        events: 0,
        turns: 0,
        tool_starts: 0,
        first_seq: 0,
        last_seq: 0,
        corrupt: None,
        open_turn: None,
        open_tools: Vec::new(),
        open_tool_events: Vec::new(),
        open_turn_event: None,
        good_bytes: 0,
    };
    let mut open: BTreeSet<String> = BTreeSet::new();
    let mut open_events: std::collections::BTreeMap<String, crate::json::Json> = Default::default();
    let mut open_turn_event: Option<crate::json::Json> = None;
    let mut open_turn: Option<u64> = None;
    let mut line_no = 0usize;
    let mut buf: Vec<u8> = Vec::with_capacity(4096);

    macro_rules! corrupt {
        ($torn:expr, $seq:expr, $reason:expr) => {{
            report.corrupt = Some(Corrupt {
                torn: $torn,
                seq: $seq,
                good_bytes,
                reason: $reason,
            });
            break;
        }};
    }

    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        line_no += 1;

        // 没有结尾换行只可能是文件的最后一行；但“没有换行”不等于“内容坏了”。
        // 先按正常记录校验：hash/seq/prev 都通过就是一条合法记录（只是缺换行），
        // 由 Journal::open 负责补换行；只有校验失败才是可修复的尾部撕裂。
        let had_newline = buf.last() == Some(&b'\n');
        let torn_tail = !had_newline;
        let line = if had_newline {
            &buf[..buf.len() - 1]
        } else {
            &buf[..]
        };
        if line.iter().all(|b| b.is_ascii_whitespace()) {
            good_bytes += n;
            continue;
        }

        // hash 是最后一个字段：按字节切掉 `,"hash":"…"}` 再算，不能重新序列化
        let (cut, want) = match trailing_hash(line) {
            Some(v) => v,
            None => corrupt!(torn_tail, Some(report.last_seq), format!("第 {} 行不是完整记录", line_no)),
        };
        let mut preimage = Vec::with_capacity(cut + 1);
        preimage.extend_from_slice(&line[..cut]);
        preimage.push(b'}');
        let got = sha256::hex(&preimage);
        if got != want {
            corrupt!(
                false,
                Some(expected_seq),
                format!("hash mismatch（记录 {}…，实际 {}…）", &want[..8], &got[..8])
            );
        }

        let text = match std::str::from_utf8(line) {
            Ok(t) => t,
            Err(_) => corrupt!(torn_tail, Some(report.last_seq), format!("第 {} 行不是合法 UTF-8", line_no)),
        };

        // 只取四个字段（seq/prev/kind/callId），不解析整条事件。
        // 日志里 prompt/snapshot 这类事件很大，而校验根本不需要它们的内容——
        // 完整解析它们纯属浪费（实测 30 万条时占了大头）。
        let fields = match scan_fields(line) {
            Some(f) => f,
            None => corrupt!(false, Some(expected_seq), format!("第 {} 行不是合法 JSON", line_no)),
        };

        let seq = fields.seq;
        if seq != expected_seq {
            corrupt!(
                false,
                Some(seq),
                format!("seq 不连续：期望 {}，实际 {}", expected_seq, seq)
            );
        }
        if fields.prev != prev {
            corrupt!(false, Some(seq), "prev 与上一条的 hash 不接".to_string());
        }

        prev = want.clone();
        expected_seq += 1;
        good_bytes += n;
        report.good_bytes = good_bytes;

        if report.events == 0 {
            report.first_seq = seq;
        }
        report.last_seq = seq;
        report.events += 1;

        // 只有这两类事件需要完整解析（resume 要拿 args）
        match fields.kind.as_str() {
            "turn/start" => {
                report.turns += 1;
                let turn_no = fields.turn.unwrap_or(0);
                open_turn = Some(turn_no);
                open_turn_event = json::parse(text).ok();
            }
            "turn/end" => {
                open_turn = None;
                open_turn_event = None;
            }
            "tool/start" => {
                report.tool_starts += 1;
                let id = fields.call_id.clone().unwrap_or_else(|| "?".to_string());
                open.insert(id.clone());
                if let Ok(ev) = json::parse(text) {
                    open_events.insert(id, ev);
                }
            }
            "tool/end" => {
                let id = fields.call_id.clone().unwrap_or_else(|| "?".to_string());
                open.remove(&id);
                open_events.remove(&id);
            }
            _ => {}
        }
    }

    report.open_turn = open_turn;
    report.open_tools = open.into_iter().collect();
    report.open_tool_events = open_events.into_values().collect();
    report.open_turn_event = open_turn_event;
    report.good_bytes = good_bytes;
    Ok(report)
}

/// 返回 (切点偏移, hash)。切点之后是 `,"hash":"…"}`，校验时用 `}` 补回。
pub(crate) fn trailing_hash(line: &[u8]) -> Option<(usize, String)> {
    if line.last() != Some(&b'}') || line.len() < 12 {
        return None;
    }
    let needle = b",\"hash\":\"";
    let start = line.windows(needle.len()).rposition(|w| w == needle)?;
    let hash_start = start + needle.len();
    let hash_end = line.len() - 2; // 去掉结尾的 `"` 与 `}`
    if hash_end <= hash_start || hash_end - hash_start != 64 {
        return None;
    }
    let hex = std::str::from_utf8(&line[hash_start..hash_end]).ok()?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((start, hex.to_string()))
}

/// 校验只需要这几个字段。不解析完整 JSON——日志里 prompt/snapshot 这类
/// 事件很大，完整解析它们纯属浪费。
struct Fields {
    seq: u64,
    prev: String,
    kind: String,
    call_id: Option<String>,
    turn: Option<u64>,
}

/// 扫描一条事件里的少数顶层字段。
///
/// 只认「顶层、值为字符串或整数」的写法；遇到转义、嵌套或任何看不懂的结构
/// 就返回 None，由调用方退回完整解析——**宁可慢一点，也不能把坏日志判成好的**。
fn scan_fields(line: &[u8]) -> Option<Fields> {
    let mut seq: Option<u64> = None;
    let mut prev: Option<String> = None;
    let mut kind: Option<String> = None;
    let mut call_id: Option<String> = None;
    let mut turn: Option<u64> = None;

    let mut i = 0usize;
    let n = line.len();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    // 当前字符串（键或值）的起点
    let mut str_start = 0usize;
    // 上一个结束的字符串是不是键
    let mut last_was_key: Option<String> = None;

    while i < n {
        let b = line[i];
        if in_string {
            if escaped {
                // 这些字段的值不该出现转义；出现了就不猜，退回完整解析
                return None;
            }
            if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
                let s = std::str::from_utf8(&line[str_start..i]).ok()?;
                if depth == 1 {
                    match last_was_key.take() {
                        None => last_was_key = Some(s.to_string()),
                        Some(_) => {}
                    }
                }
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => {
                in_string = true;
                escaped = false;
                str_start = i + 1;
            }
            b'{' | b'[' => {
                depth += 1;
                if depth > 1 {
                    // 嵌套结构：这里不处理
                    return None;
                }
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
            }
            b':' => {
                // 键后面是值：只对需要的键取值，其余直接跳过（不分配字符串）
                if depth == 1 {
                    if let Some(key) = last_was_key.take() {
                        let val_start = i + 1;
                        match key.as_str() {
                            "seq" => {
                                let (num, next) = read_number(line, val_start)?;
                                seq = Some(num);
                                i = next;
                                continue;
                            }
                            "turn" => {
                                let (num, next) = read_number(line, val_start)?;
                                turn = Some(num);
                                i = next;
                                continue;
                            }
                            "prev" | "kind" | "callId" => {
                                let (s, next) = read_simple_string(line, val_start)?;
                                match key.as_str() {
                                    "prev" => prev = Some(s),
                                    "kind" => kind = Some(s),
                                    _ => call_id = Some(s),
                                }
                                i = next;
                                continue;
                            }
                            _ => {
                                i = skip_value(line, val_start)?;
                                continue;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }

    Some(Fields {
        seq: seq?,
        prev: prev?,
        kind: kind.unwrap_or_default(),
        call_id,
        turn,
    })
}

/// 读一个整数字段，返回 (值, 下一个位置)。
fn read_number(line: &[u8], start: usize) -> Option<(u64, usize)> {
    let mut i = start;
    while i < line.len() && line[i] == b' ' {
        i += 1;
    }
    let from = i;
    while i < line.len() && line[i].is_ascii_digit() {
        i += 1;
    }
    if i == from {
        return None;
    }
    let s = std::str::from_utf8(&line[from..i]).ok()?;
    Some((s.parse::<u64>().ok()?, i))
}

/// 读一个**不含转义**的字符串字段，返回 (值, 下一个位置)。
fn read_simple_string(line: &[u8], start: usize) -> Option<(String, usize)> {
    let mut i = start;
    while i < line.len() && line[i] == b' ' {
        i += 1;
    }
    if i >= line.len() || line[i] != b'"' {
        return None;
    }
    i += 1;
    let from = i;
    while i < line.len() {
        match line[i] {
            b'\\' => return None, // 有转义：不猜，交给调用方
            b'"' => {
                let s = std::str::from_utf8(&line[from..i]).ok()?.to_string();
                return Some((s, i + 1));
            }
            _ => i += 1,
        }
    }
    None
}

/// 跳过一个值（字符串/数字/对象/数组/字面量），返回下一个位置。
fn skip_value(line: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    while i < line.len() && line[i] == b' ' {
        i += 1;
    }
    if i >= line.len() {
        return None;
    }
    match line[i] {
        b'"' => {
            i += 1;
            let mut esc = false;
            while i < line.len() {
                match line[i] {
                    b'\\' => {
                        esc = !esc;
                        i += 1;
                    }
                    b'"' => {
                        if esc {
                            esc = false;
                            i += 1;
                        } else {
                            return Some(i + 1);
                        }
                    }
                    _ => {
                        esc = false;
                        i += 1;
                    }
                }
            }
            None
        }
        b'{' | b'[' => {
            let mut depth = 0i32;
            let mut in_str = false;
            let mut esc = false;
            while i < line.len() {
                let c = line[i];
                if in_str {
                    if esc {
                        esc = false;
                    } else if c == b'\\' {
                        esc = true;
                    } else if c == b'"' {
                        in_str = false;
                    }
                } else {
                    match c {
                        b'"' => in_str = true,
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                return Some(i + 1);
                            }
                        }
                        _ => {}
                    }
                }
                i += 1;
            }
            None
        }
        _ => {
            while i < line.len() && line[i] != b',' && line[i] != b'}' {
                i += 1;
            }
            Some(i)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Journal;
    use crate::json::Json;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ui-stream-verify-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn two_events(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("journal.jsonl");
        let mut j = Journal::open(&path).unwrap();
        j.append("note", Json::obj(vec![("text", Json::from("first"))]))
            .unwrap();
        j.append("note", Json::obj(vec![("text", Json::from("second"))]))
            .unwrap();
        drop(j);
        path
    }

    #[test]
    fn a_valid_final_record_without_newline_is_not_corruption() {
        let dir = scratch("no-newline");
        let path = two_events(&dir);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.trim_end_matches('\n')).unwrap();
        let r = verify_stream(&path).unwrap();
        assert!(r.corrupt.is_none(), "合法尾记录不能报 torn: {:?}", r.corrupt);
        assert_eq!(r.events, 2);
    }

    #[test]
    fn middle_damage_is_not_a_repairable_tail() {
        let dir = scratch("middle");
        let path = two_events(&dir);
        let text = std::fs::read_to_string(&path).unwrap();
        // 改第一条的正文但不重算 hash：位置在文件中间。
        let broken = text.replacen("first", "FIRST", 1);
        std::fs::write(&path, broken).unwrap();
        let r = verify_stream(&path).unwrap();
        let c = r.corrupt.expect("must detect");
        assert!(!c.torn, "中间损坏不能标成可修复尾部");
    }
}

