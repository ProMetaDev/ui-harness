//! 流式校验必须和「读进内存再校验」给出**同样的判定**。
//!
//! 换实现最容易出的问题是"新路径更严格/更宽松"：坏日志被判好（危险），
//! 或者好日志被判坏（烦人）。这里把三种情况都钉住。

use std::fs;
use std::io::Write;
use ui_core::journal::{Journal, GENESIS};
use ui_core::stream_verify::verify_stream;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("ui-stream-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn build(dir: &std::path::Path, events: usize) -> std::path::PathBuf {
    let path = dir.join("journal.jsonl");
    let mut j = Journal::open(&path).unwrap();
    for i in 0..events {
        if i % 10 == 0 {
            j.append("turn/start", ui_core::json::Json::obj(vec![("turn", ui_core::json::Json::from((i / 10) as u64))]))
                .unwrap();
        } else if i % 10 == 9 {
            // 成对闭合，否则"干净日志"里会留下未完成的轮次
            j.append("turn/end", ui_core::json::Json::obj(vec![("turn", ui_core::json::Json::from((i / 10) as u64))]))
                .unwrap();
        } else {
            j.append("note", ui_core::json::Json::obj(vec![("i", ui_core::json::Json::from(i as u64))]))
                .unwrap();
        }
    }
    path
}

#[test]
fn stream_matches_in_memory_on_a_clean_journal() {
    let dir = tmp("clean");
    let path = build(&dir, 120);

    let (events, corrupt) = Journal::read(&path).unwrap();
    let r = verify_stream(&path).unwrap();

    assert!(corrupt.is_none());
    assert!(r.corrupt.is_none(), "干净的日志不该被流式校验判坏");
    assert_eq!(r.events, events.len(), "事件数必须一致");
    assert_eq!(r.events, 120);
    assert_eq!(r.first_seq, 1);
    assert_eq!(r.last_seq, 120);
    let size = fs::metadata(&path).unwrap().len() as usize;
    assert_eq!(
        r.good_bytes, size,
        "干净日志的 good_bytes 应等于文件大小（events={} first={} last={}）",
        r.events, r.first_seq, r.last_seq
    );
    assert!(r.open_turn.is_none(), "turn 都闭合了");
    assert!(r.open_tools.is_empty());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn stream_detects_tampering_the_same_way() {
    let dir = tmp("tamper");
    let path = build(&dir, 50);

    // 改一个字符（正文），哈希必然对不上
    let text = fs::read_to_string(&path).unwrap();
    let tampered = text.replacen("\"i\":3", "\"i\":999", 1);
    fs::write(&path, tampered).unwrap();

    let (_, corrupt_mem) = Journal::read(&path).unwrap();
    let r = verify_stream(&path).unwrap();

    assert!(corrupt_mem.is_some(), "内存路径应判坏");
    let c = r.corrupt.expect("流式路径也必须判坏");
    assert!(!c.torn, "正文被改不是「尾部撕裂」，不能被当成可修复");
    assert!(c.reason.contains("hash"), "理由要说清是哈希不符: {}", c.reason);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn stream_treats_a_truncated_tail_as_repairable() {
    let dir = tmp("torn");
    let path = build(&dir, 40);
    let full = fs::read(&path).unwrap();

    // 砍掉最后 20 个字节：留下半条记录
    let cut = full.len() - 20;
    let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.set_len(cut as u64).unwrap();
    f.flush().unwrap();

    let r = verify_stream(&path).unwrap();
    let c = r.corrupt.expect("截断必须被发现");
    assert!(c.torn, "尾部残缺应当标记为可修复");

    // 可修复前缀 = 最后一个完整行的结束位置（不是"砍掉 20 字节"的位置：
    // 那条被砍的记录整条都不能要，否则会留下半个 JSON）
    let kept = fs::read(&path).unwrap();
    let expected = kept.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
    assert_eq!(c.good_bytes, expected, "可修复前缀必须落在行边界上");
    assert!(c.good_bytes <= cut, "前缀不能超过截断点");
    assert!(r.events >= 38, "前面的记录仍然有效，实际 {}", r.events);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn stream_handles_an_empty_journal() {
    let dir = tmp("empty");
    let path = dir.join("journal.jsonl");
    fs::write(&path, "").unwrap();

    let r = verify_stream(&path).unwrap();
    assert_eq!(r.events, 0);
    assert!(r.corrupt.is_none());
    assert_eq!(r.good_bytes, 0);

    // 只有 GENESIS 的链条也算完好
    let _ = GENESIS;
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_journal_is_an_empty_one_not_an_error() {
    // 回归（CI 抓到的）：流式校验一开始对「文件不存在」直接报错，
    // 于是干净机器上门禁的「verify（干净日志）」失败。
    // 没有日志 = 还没有任何记录，不是损坏。
    let dir = tmp("missing");
    let path = dir.join("nope.jsonl");
    let r = verify_stream(&path).expect("缺失日志不该是错误");
    assert_eq!(r.events, 0);
    assert!(r.corrupt.is_none());
    assert!(r.open_tools.is_empty());
    assert_eq!(r.good_bytes, 0);
    let _ = fs::remove_dir_all(&dir);
}
