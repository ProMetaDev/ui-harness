//! 可审计的知识（K1 + K2）：卡片来源绑定、两条事件子类型、任务前确定性预取。
//!
//! 这组测试的牙齿：
//!
//! 1. `check` 在源一致时退 0、**真的改了文件内容**之后退 2；
//! 2. `knowledge/use` 写进日志后 `ui verify` 仍然说链完整；
//! 3. `ui run` 命中卡片时，提示词里真的有卡正文（不是「事件写了但没注入」）；
//! 4. 未命中时**一条 knowledge 事件都不写**，且提示词与任务文本逐字相同。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use ui_core::json::Json;
use ui_core::journal::Journal;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ui")
}

fn temp_ws(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ui-knowledge-cli-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_card(ws: &Path, file: &str, front: &str, body: &str) -> PathBuf {
    let dir = ws.join("docs").join("knowledge");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(file);
    fs::write(&path, format!("---\n{}\n---\n{}\n", front, body)).unwrap();
    path
}

fn scenario(ws: &Path) -> PathBuf {
    let f = ws.join("scenario.json");
    fs::write(&f, r#"{"model":"scripted","steps":[{"content":"好了"}]}"#).unwrap();
    f
}

fn ui(ws: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .args(["--workspace", ws.to_str().unwrap()])
        .output()
        .expect("跑 ui 子进程")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn journal(ws: &Path) -> Vec<Json> {
    Journal::load(ws.join(".ui").join("journal.jsonl")).expect("读日志")
}

fn kinds(ws: &Path, kind: &str) -> Vec<Json> {
    journal(ws)
        .into_iter()
        .filter(|e| e.str_at("kind") == Some(kind))
        .collect()
}

#[test]
fn check_exits_0_when_bound_and_2_after_the_source_really_changes() {
    let ws = temp_ws("check");
    fs::create_dir_all(ws.join("src/pay")).unwrap();
    fs::write(ws.join("src/pay/x.rs"), "fn refund() { /* 一步 */ }\n").unwrap();

    let touches = vec!["src/pay/**".to_string()];
    let sha = ui_core::knowledge::source_digest(&ws, &touches).expect("有命中文件才有哈希");
    write_card(
        &ws,
        "mod-pay.md",
        &format!(
            "id: mod-pay\ntitle: 支付\ngenerated_at: 2026-01-01T00:00:00Z\ngenerated_by_seq: 3\nconfirmed_by: human\nsource_sha: {}\ntouches: [\"src/pay/**\"]",
            sha
        ),
        "退款只有一条路径。",
    );

    let ok = ui(&ws, &["knowledge", "check"]);
    assert_eq!(ok.status.code(), Some(0), "源一致必须退 0：{}", stdout(&ok));
    assert!(stdout(&ok).contains("ok"), "{}", stdout(&ok));
    assert!(stdout(&ok).contains("结论  PASS"), "{}", stdout(&ok));

    // 真的改文件内容（不是改卡片）：卡上的 source_sha 立刻过期
    fs::write(ws.join("src/pay/x.rs"), "fn refund() { /* 两步 */ }\n").unwrap();

    let stale = ui(&ws, &["knowledge", "check"]);
    assert_eq!(
        stale.status.code(),
        Some(2),
        "源变了必须退 2：{}",
        stdout(&stale)
    );
    assert!(stdout(&stale).contains("STALE"), "{}", stdout(&stale));
    // 变的是文件，不是卡片：卡片文件本身没动过
    let written = fs::read_to_string(ws.join("docs/knowledge/mod-pay.md")).unwrap();
    assert!(written.contains(&sha));
}

#[test]
fn knowledge_use_lands_in_the_hash_chain_and_verify_still_passes() {
    let ws = temp_ws("use");
    let body = "退款只有一条路径：pay::refund，先锁单后打款。\n";
    write_card(
        &ws,
        "mod-pay.md",
        "id: mod-pay\ntitle: 支付模块\ngenerated_at: 2026-01-01T00:00:00Z\ngenerated_by_seq: 1\nconfirmed_by: human\nsource_sha: \ntouches: [\"src/pay/**\"]",
        body,
    );

    let out = ui(
        &ws,
        &["knowledge", "use", "mod-pay", "--reason", "path-match:src/pay/x.rs"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));

    let used = kinds(&ws, "knowledge/use");
    assert_eq!(used.len(), 1, "必须正好写一条 knowledge/use");
    let event = &used[0];
    assert_eq!(event.str_at("card"), Some("mod-pay"));
    assert_eq!(event.str_at("reason"), Some("path-match:src/pay/x.rs"));
    // budget_tokens = 卡正文的字节数 / 4
    let expected = ui_core::knowledge::find(&ws, "mod-pay")
        .unwrap()
        .budget_tokens();
    assert_eq!(event.u64_at("budget_tokens"), Some(expected));
    assert!(expected > 0, "正文不能是空的，否则这条断言没有意义");
    assert_eq!(event.get("score"), Some(&Json::Null), "确定性命中不编分数");

    // 链必须仍然完好（事件走的是同一条 append 路径）
    let verify = ui(&ws, &["verify"]);
    assert_eq!(verify.status.code(), Some(0), "{}", stdout(&verify));
    assert!(stdout(&verify).contains("哈希链完整"), "{}", stdout(&verify));
}

#[test]
fn knowledge_emit_records_the_source_binding_at_the_current_seq() {
    let ws = temp_ws("emit");
    let sc = scenario(&ws);
    write_card(
        &ws,
        "mod-pay.md",
        "id: mod-pay\ntitle: 支付模块\ngenerated_at: 2026-01-01T00:00:00Z\ngenerated_by_seq: 1\nconfirmed_by: none\nsource_sha: \ntouches: [\"src/pay/**\", \"src/billing/*.rs\"]",
        "退款只有一条路径。",
    );

    // 先有真实的一轮，emit 的 generated_by_seq 才有意义
    let run = ui(
        &ws,
        &["run", "--task", "随便看看", "--mock", sc.to_str().unwrap(), "--yes"],
    );
    assert_eq!(run.status.code(), Some(0), "{}", stdout(&run));
    let seq_before = journal(&ws).last().unwrap().u64_at("seq").unwrap();

    let out = ui(&ws, &["knowledge", "emit", "mod-pay", "--confirmed-by", "human"]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));

    let emitted = kinds(&ws, "knowledge/emit");
    assert_eq!(emitted.len(), 1);
    let event = &emitted[0];
    assert_eq!(event.str_at("card"), Some("mod-pay"));
    assert_eq!(event.str_at("confirmed_by"), Some("human"));
    assert_eq!(event.u64_at("generated_by_seq"), Some(seq_before));
    assert_eq!(event.u64_at("seq"), Some(seq_before + 1));
    let touches = event.arr_at("touches").expect("touches 必须进事件");
    assert_eq!(touches.len(), 2);
    assert_eq!(touches[0].as_str(), Some("src/pay/**"));

    let verify = ui(&ws, &["verify"]);
    assert_eq!(verify.status.code(), Some(0), "{}", stdout(&verify));
    assert!(stdout(&verify).contains("哈希链完整"));

    // 非法确认位必须被拒绝（confirmed_by 只有 human / none 两档）
    let bad = ui(&ws, &["knowledge", "emit", "mod-pay", "--confirmed-by", "robot"]);
    assert_eq!(bad.status.code(), Some(1), "{}", stdout(&bad));
    assert!(kinds(&ws, "knowledge/emit").len() == 1, "被拒绝就不能写事件");
}

#[test]
fn run_prefetches_the_card_body_into_the_first_prompt() {
    let ws = temp_ws("prefetch");
    fs::create_dir_all(ws.join("src/pay")).unwrap();
    fs::write(ws.join("src/pay/x.rs"), "fn refund() {}\n").unwrap();
    // 正文里放一句任务文本里没有的话：能断言「正文进了提示词」而不是「任务复述了」
    write_card(
        &ws,
        "mod-pay.md",
        "id: mod-pay\ntitle: 支付模块\ngenerated_at: 2026-01-01T00:00:00Z\ngenerated_by_seq: 1\nconfirmed_by: human\nsource_sha: \ntouches: [\"src/pay/**\"]",
        "退款只有一条路径：pay::refund，先锁单后打款。",
    );
    let sc = scenario(&ws);

    let out = ui(
        &ws,
        &[
            "run",
            "--task",
            "把 src/pay/x.rs 的退款改成两步确认",
            "--mock",
            sc.to_str().unwrap(),
            "--yes",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));

    let used = kinds(&ws, "knowledge/use");
    assert_eq!(used.len(), 1, "命中一张卡：一条事件：{}", stdout(&out));
    assert_eq!(used[0].str_at("card"), Some("mod-pay"));
    let reason = used[0].str_at("reason").unwrap();
    assert_eq!(reason, "path-match:src/pay/x.rs", "命中原因要写清楚");

    // 提示词里必须有卡正文（K2 的证据：不是只写了事件）
    let events = journal(&ws);
    let start = events
        .iter()
        .find(|e| e.str_at("kind") == Some("turn/start"))
        .expect("必须有 turn/start");
    let prompt = start.str_at("prompt").unwrap();
    assert!(
        prompt.contains("先锁单后打款"),
        "首个提示词里必须有卡正文，实际：{}",
        prompt
    );
    assert!(prompt.contains("[mod-pay] 支付模块"), "{}", prompt);
    // 顺序：knowledge/use 在 turn/start 之前（任务开始时预取）
    let use_seq = events
        .iter()
        .find(|e| e.str_at("kind") == Some("knowledge/use"))
        .and_then(|e| e.u64_at("seq"))
        .unwrap();
    assert!(use_seq < start.u64_at("seq").unwrap());

    // 回放里也看得见（人读的证据链）
    let replay = ui(&ws, &["replay"]);
    let text = stdout(&replay);
    assert!(text.contains("knowledge/use"), "{}", text);
    assert!(text.contains("支付模块"), "{}", text);
}

#[test]
fn run_without_a_match_writes_no_knowledge_event_and_keeps_the_task_verbatim() {
    let ws = temp_ws("nomatch");
    fs::create_dir_all(ws.join("src/pay")).unwrap();
    fs::write(ws.join("src/pay/x.rs"), "fn refund() {}\n").unwrap();
    write_card(
        &ws,
        "mod-pay.md",
        "id: mod-pay\ntitle: 支付模块\ngenerated_at: 2026-01-01T00:00:00Z\ngenerated_by_seq: 1\nconfirmed_by: human\nsource_sha: \ntouches: [\"src/pay/**\"]",
        "退款只有一条路径：pay::refund。",
    );
    let sc = scenario(&ws);
    let task = "把欢迎页的文案改短一点";

    let out = ui(
        &ws,
        &["run", "--task", task, "--mock", sc.to_str().unwrap(), "--yes"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));

    // 负样本：卡片确实存在，但任务没命中 → 一条 knowledge 事件都不写
    assert_eq!(ui_core::knowledge::discover(&ws).len(), 1, "卡片是存在的");
    assert!(kinds(&ws, "knowledge/use").is_empty(), "未命中不该写事件");
    assert!(kinds(&ws, "knowledge/emit").is_empty());

    let start = journal(&ws)
        .into_iter()
        .find(|e| e.str_at("kind") == Some("turn/start"))
        .unwrap();
    assert_eq!(
        start.str_at("prompt"),
        Some(task),
        "未命中时提示词必须逐字等于任务文本（行为不变）"
    );
}

#[test]
fn list_shows_the_card_and_missing_cards_are_not_an_error() {
    let ws = temp_ws("list");
    let empty = ui(&ws, &["knowledge", "list"]);
    assert_eq!(empty.status.code(), Some(0), "没有卡片不是错误");
    assert!(stdout(&empty).contains("知识卡 0 张"), "{}", stdout(&empty));

    write_card(
        &ws,
        "mod-pay.md",
        "id: mod-pay\ntitle: 支付模块\nconfirmed_by: human\nsource_sha: \ntouches: [\"src/pay/**\"]",
        "正文",
    );
    let list = ui(&ws, &["knowledge", "list"]);
    assert_eq!(list.status.code(), Some(0));
    let text = stdout(&list);
    assert!(text.contains("mod-pay"), "{}", text);
    assert!(text.contains("支付模块"), "{}", text);
    assert!(text.contains("confirmed_by=human"), "{}", text);
    assert!(text.contains("src/pay/**"), "{}", text);
}
