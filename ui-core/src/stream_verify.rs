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
    /// 校验通过的字节数（用于截断修复）
    pub good_bytes: usize,
}

pub fn verify_stream(path: &Path) -> std::io::Result<StreamReport> {
    let file = File::open(path)?;
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
        good_bytes: 0,
    };
    let mut open: BTreeSet<String> = BTreeSet::new();
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

        // 没有结尾换行 = 尾部撕裂（崩溃残留），不是篡改
        if buf.last() != Some(&b'\n') {
            corrupt!(true, Some(report.last_seq), format!("第 {} 行没有结尾换行（崩溃残留）", line_no));
        }
        let line = &buf[..buf.len() - 1];
        if line.iter().all(|b| b.is_ascii_whitespace()) {
            good_bytes += n;
            continue;
        }

        // hash 是最后一个字段：按字节切掉 `,"hash":"…"}` 再算，不能重新序列化
        let (cut, want) = match trailing_hash(line) {
            Some(v) => v,
            None => corrupt!(true, Some(report.last_seq), format!("第 {} 行不是完整记录", line_no)),
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
            Err(_) => corrupt!(true, Some(report.last_seq), format!("第 {} 行不是合法 UTF-8（撕裂）", line_no)),
        };
        let event = match json::parse(text) {
            Ok(v) => v,
            Err(e) => corrupt!(false, Some(expected_seq), format!("第 {} 行不是 JSON: {}", line_no, e)),
        };

        let seq = event.u64_at("seq").unwrap_or(0);
        if seq != expected_seq {
            corrupt!(
                false,
                Some(seq),
                format!("seq 不连续：期望 {}，实际 {}", expected_seq, seq)
            );
        }
        if event.str_at("prev").unwrap_or("") != prev {
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

        match event.str_at("kind").unwrap_or("") {
            "turn/start" => {
                report.turns += 1;
                open_turn = Some(event.u64_at("turn").unwrap_or(0));
            }
            "turn/end" => open_turn = None,
            "tool/start" => {
                report.tool_starts += 1;
                open.insert(event.str_at("callId").unwrap_or("?").to_string());
            }
            "tool/end" => {
                open.remove(event.str_at("callId").unwrap_or("?"));
            }
            _ => {}
        }
    }

    report.open_turn = open_turn;
    report.open_tools = open.into_iter().collect();
    report.good_bytes = good_bytes;
    Ok(report)
}

/// 返回 (切点偏移, hash)。切点之后是 `,"hash":"…"}`，校验时用 `}` 补回。
fn trailing_hash(line: &[u8]) -> Option<(usize, String)> {
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
