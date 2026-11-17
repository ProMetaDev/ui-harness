//! 记忆 P1 的跨进程测试：写入 → 检索 → 注入 → 审计 → 遗忘/过期/篡改。
//!
//! 这组测试的牙齿（每条都能在实现错了时失败）：
//!
//! 1. 命中的正文确实进了**快照**（用 `echoSnapshot` 让模型把看到的快照原样回显），
//!    且 `memory/used` 的 id / tokens 对得上；
//! 2. 未命中时零事件，且快照与「没有记忆文件」时逐字节相同；
//! 3. forget 之后第二轮不再注入、也不再写 used；
//! 4. 过期（`--expires 0`）的记忆不被检索；
//! 5. 改一个字节 → `ui memory check` 退 2；
//! 6. 正常写入后 check 退 0 且 seq 连续。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use ui_core::json::Json;
use ui_core::journal::Journal;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ui")
}

fn temp_ws(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ui-memory-cli-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn scenario(ws: &Path) -> PathBuf {
    let f = ws.join("scenario.json");
    fs::write(
        &f,
        r#"{"model":"echo","echoSnapshot":true,"steps":[{"content":"好了"}]}"#,
    )
    .unwrap();
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
    Journal::load(ws.join(".ui").join("journal.jsonl")).expect("读主日志")
}

fn memory_journal(ws: &Path) -> Vec<Json> {
    Journal::load(ws.join(".ui").join("memory.jsonl")).expect("读记忆文件")
}

fn kinds(events: &[Json], kind: &str) -> Vec<Json> {
    events
        .iter()
        .filter(|e| e.str_at("kind") == Some(kind))
        .cloned()
        .collect()
}

/// 从 `ui remember` 的输出里取 id（`m-` + 12 hex）。同时证明它确实打印了 id。
fn parse_id(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i + 14 <= bytes.len() {
        if bytes[i] == b'm' && bytes[i + 1] == b'-' {
            let candidate = &text[i..i + 14];
            let tail = &candidate[2..];
            if tail.len() == 12 && tail.chars().all(|c| c.is_ascii_hexdigit()) {
                return candidate.to_string();
            }
        }
        i += 1;
    }
    panic!("remember 输出里没有 id：{}", text);
}

fn remember(ws: &Path, kind: &str, key: &str, text: &str) -> String {
    let out = ui(
        ws,
        &[
            "remember",
            text,
            "--kind",
            kind,
            "--key",
            key,
            "--scope",
            "workspace",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    parse_id(&stdout(&out))
}

fn run(ws: &Path, task: &str) -> Output {
    let sc = scenario(ws);
    ui(
        ws,
        &[
            "run",
            "--task",
            task,
            "--mock",
            sc.to_str().unwrap(),
            "--yes",
        ],
    )
}

fn last_response(ws: &Path) -> String {
    kinds(&journal(ws), "model/response")
        .last()
        .and_then(|e| e.str_at("content").map(str::to_string))
        .expect("必须有 model/response")
}

fn memory_section(ws: &Path) -> Option<Json> {
    let snapshot = kinds(&journal(ws), "prompt/snapshot").pop()?;
    snapshot
        .arr_at("sections")?
        .iter()
        .find(|s| s.str_at("id") == Some("memory"))
        .cloned()
}

#[test]
fn remember_then_run_injects_the_memory() {
    let ws = temp_ws("inject");
    let text = "门禁 bash tools/ci.sh；单测 cargo test --all";
    let id = remember(&ws, "build", "build.verify", text);

    let out = run(&ws, "跑一下门禁，先看构建命令");
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));

    // 1) 模型真的看见了正文（echoSnapshot 把快照原样回显）。
    let seen = last_response(&ws);
    assert!(seen.contains(text), "快照里必须有记忆正文：{}", seen);
    assert!(seen.contains("<memory>"), "必须出现 memory 段：{}", seen);

    // 2) 快照的 memory 段被计价。
    let section = memory_section(&ws).expect("必须有 memory 段");
    assert!(
        section.u64_at("tokens").unwrap_or(0) > 0,
        "memory 段的 tokens 必须 > 0"
    );

    // 2b) 直接渲染快照也能看到正文（不用 echoSnapshot 的第二条证据）。
    let prompt = ui(
        &ws,
        &["prompt", "--memory-query", "跑一下门禁", "--full"],
    );
    assert_eq!(prompt.status.code(), Some(0), "{}", stdout(&prompt));
    assert!(
        stdout(&prompt).contains(text),
        "prompt --memory-query 必须渲染出正文"
    );
    assert!(stdout(&prompt).contains("<memory>"));

    // 3) 主日志里有 memory/used，id 与 remember 打印的一致。
    let used = kinds(&journal(&ws), "memory/used");
    assert_eq!(used.len(), 1, "一条命中：一条 memory/used");
    assert_eq!(used[0].str_at("id"), Some(id.as_str()));
    assert!(
        !used[0].str_at("reason").unwrap_or("").is_empty(),
        "reason 必须写命中原因"
    );
    assert_eq!(
        used[0].u64_at("tokens"),
        Some((text.len() as u64) / 4),
        "tokens 口径 = 正文长度 / 4"
    );

    // 4) 记忆链里也有一条使用记录（hits 由它重放）。
    let used_mem = kinds(&memory_journal(&ws), "memory/used");
    assert_eq!(used_mem.len(), 1);
    assert_eq!(used_mem[0].str_at("id"), Some(id.as_str()));

    // 5) 主日志仍然可验证；replay 能看见这一行。
    let verify = ui(&ws, &["verify"]);
    assert_eq!(verify.status.code(), Some(0), "{}", stdout(&verify));
    assert!(stdout(&verify).contains("哈希链完整"));
    let replay = stdout(&ui(&ws, &["replay"]));
    assert!(replay.contains("memory/used"), "replay 必须显示 memory/used：{}", replay);
    assert!(replay.contains(&id), "replay 必须显示命中的 id：{}", replay);
}

#[test]
fn run_without_a_match_writes_no_memory_event_and_keeps_the_task_verbatim() {
    let ws = temp_ws("nomatch");
    // 这条记忆与任务没有任何 token 重叠（含 CJK 单字）。
    remember(&ws, "fact", "", "数据库迁移前必须先备份");
    let task = "把欢迎页的文案改短一点";

    let first = run(&ws, task);
    assert_eq!(first.status.code(), Some(0), "{}", stdout(&first));

    assert!(
        kinds(&journal(&ws), "memory/used").is_empty(),
        "未命中不该写主日志事件"
    );
    assert!(
        kinds(&memory_journal(&ws), "memory/used").is_empty(),
        "未命中不该写记忆链事件"
    );
    let start = kinds(&journal(&ws), "turn/start")
        .pop()
        .expect("必须有 turn/start");
    assert_eq!(
        start.str_at("prompt"),
        Some(task),
        "未命中时提示词必须逐字等于任务文本"
    );
    let without_section = last_response(&ws);
    assert!(!without_section.contains("<memory>"), "不能有空 memory 段");

    // 更强的负样本：删掉记忆文件再跑一次，快照必须逐字节相同。
    fs::remove_file(ws.join(".ui").join("memory.jsonl")).unwrap();
    let second = run(&ws, task);
    assert_eq!(second.status.code(), Some(0), "{}", stdout(&second));
    let without_file = last_response(&ws);
    assert_eq!(
        without_section, without_file,
        "未命中时快照必须与「没有记忆」完全一致（提示词逐字不变）"
    );
}

#[test]
fn forget_makes_it_stop_being_retrieved() {
    let ws = temp_ws("forget");
    let text = "退款只有一条路径：pay::refund，先锁单后打款";
    let id = remember(&ws, "pitfall", "", text);

    let first = run(&ws, "把退款流程改一下");
    assert_eq!(first.status.code(), Some(0), "{}", stdout(&first));
    assert!(
        last_response(&ws).contains(text),
        "第一次必须注入（否则 forget 测试没有意义）"
    );
    assert_eq!(kinds(&journal(&ws), "memory/used").len(), 1);

    let forget = ui(&ws, &["forget", &id, "--reason", "用户要求删除"]);
    assert_eq!(forget.status.code(), Some(0), "{}", stdout(&forget));
    assert!(stdout(&forget).contains("下轮起不再注入"));

    let second = run(&ws, "把退款流程改一下");
    assert_eq!(second.status.code(), Some(0), "{}", stdout(&second));
    let seen = last_response(&ws);
    assert!(!seen.contains(text), "forget 后不能再注入：{}", seen);
    assert!(!seen.contains("<memory>"), "forget 后不能有 memory 段");
    assert_eq!(
        kinds(&journal(&ws), "memory/used").len(),
        1,
        "forget 后不能再写 memory/used"
    );
    assert_eq!(kinds(&memory_journal(&ws), "memory/used").len(), 1);

    // list 默认不显示；--all 显示墓碑。
    let list = stdout(&ui(&ws, &["memory", "list"]));
    assert!(!list.contains(text), "默认 list 不能显示已忘记忆：{}", list);
    let all = stdout(&ui(&ws, &["memory", "list", "--all"]));
    assert!(all.contains("FORGOTTEN"), "--all 必须标出墓碑：{}", all);
    assert!(all.contains(text), "--all 仍可看到正文（append-only）");

    // 未知 id 退 1，且不写事件。
    let unknown = ui(&ws, &["forget", "m-000000000000", "--reason", "x"]);
    assert_eq!(unknown.status.code(), Some(1), "{}", stdout(&unknown));
    assert_eq!(kinds(&memory_journal(&ws), "memory/forget").len(), 1);
}

#[test]
fn expired_memory_is_not_retrieved() {
    let ws = temp_ws("expired");
    let text = "门禁 bash tools/ci.sh；单测 cargo test --all";
    let out = ui(
        &ws,
        &[
            "remember",
            text,
            "--kind",
            "build",
            "--key",
            "build.verify",
            "--expires",
            "0",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let id = parse_id(&stdout(&out));

    // --expires 0 必须写成「现在」这个绝对时间戳，而不是 never。
    let write = kinds(&memory_journal(&ws), "memory/write")
        .pop()
        .expect("必须有 memory/write");
    let expires = write
        .num_at("expiresAt")
        .expect("--expires 0 不能写成 null/never");
    assert!(expires > 0.0, "必须是绝对时间戳");
    assert!(
        expires <= ui_core::time::now_ms() as f64,
        "必须已经过期（expiresAt={}）",
        expires
    );

    let run_out = run(&ws, "跑一下门禁");
    assert_eq!(run_out.status.code(), Some(0), "{}", stdout(&run_out));
    let seen = last_response(&ws);
    assert!(!seen.contains(text), "过期记忆不能注入：{}", seen);
    assert!(!seen.contains("<memory>"));
    assert!(kinds(&journal(&ws), "memory/used").is_empty());
    assert!(kinds(&memory_journal(&ws), "memory/used").is_empty());

    let list = stdout(&ui(&ws, &["memory", "list"]));
    assert!(!list.contains(text), "默认 list 不能列过期记忆：{}", list);
    let all = stdout(&ui(&ws, &["memory", "list", "--all"]));
    assert!(all.contains("EXPIRED"), "--all 必须标 EXPIRED：{}", all);
    assert!(all.contains(text));
    assert_eq!(id.len(), 14);
}

#[test]
fn memory_check_exits_2_after_tampering() {
    let ws = temp_ws("tamper");
    remember(&ws, "build", "build.verify", "门禁命令 bash tools/ci.sh");

    let ok = ui(&ws, &["memory", "check"]);
    assert_eq!(ok.status.code(), Some(0), "{}", stdout(&ok));
    assert!(stdout(&ok).contains("哈希链完整"), "{}", stdout(&ok));

    // 改正文里的一个字符（UTF-8 长度相同的汉字），文件仍是合法 JSON，但链断了。
    let path = ws.join(".ui").join("memory.jsonl");
    let before = fs::read_to_string(&path).unwrap();
    let after = before.replacen("门禁", "闸禁", 1);
    assert_ne!(before, after, "测试前提：确实改了内容");
    fs::write(&path, after).unwrap();

    let bad = ui(&ws, &["memory", "check"]);
    assert_eq!(
        bad.status.code(),
        Some(2),
        "被篡改必须退 2，实际输出：{}",
        stdout(&bad)
    );
    assert!(stdout(&bad).contains("FAIL"), "{}", stdout(&bad));
}

#[test]
fn memory_write_lands_in_the_hash_chain() {
    let ws = temp_ws("chain");
    let a = remember(&ws, "build", "build.verify", "门禁 bash tools/ci.sh；单测 cargo test --all");
    let b = remember(&ws, "fact", "db.backup", "数据库迁移前必须先备份");

    let check = ui(&ws, &["memory", "check"]);
    assert_eq!(check.status.code(), Some(0), "{}", stdout(&check));
    assert!(stdout(&check).contains("哈希链完整"), "{}", stdout(&check));
    assert!(stdout(&check).contains("生效 2"), "{}", stdout(&check));

    let events = memory_journal(&ws);
    assert_eq!(events.len(), 2);
    for (index, event) in events.iter().enumerate() {
        assert_eq!(
            event.u64_at("seq"),
            Some(index as u64 + 1),
            "seq 必须从 1 连续"
        );
        assert_eq!(event.str_at("kind"), Some("memory/write"));
        let kind = ui_core::memory::MemKind::parse(event.str_at("memKind").unwrap()).unwrap();
        let expected = ui_core::memory::id_for(
            kind,
            event.str_at("scope").unwrap(),
            event.str_at("key").unwrap(),
            event.str_at("text").unwrap(),
        );
        assert_eq!(
            event.str_at("id"),
            Some(expected.as_str()),
            "id 必须与 memKind/scope/key/text 一致"
        );
    }
    assert_eq!(events[0].str_at("id"), Some(a.as_str()));
    assert_eq!(events[1].str_at("id"), Some(b.as_str()));

    // 跑一轮产生 memory/used 之后，链仍然连续且 check 仍然 0。
    run(&ws, "跑一下门禁");
    let events = memory_journal(&ws);
    assert_eq!(events.len(), 3, "write + write + used");
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.u64_at("seq"), Some(index as u64 + 1));
    }
    let again = ui(&ws, &["memory", "check"]);
    assert_eq!(again.status.code(), Some(0), "{}", stdout(&again));

    // 帮助文本必须带上入口（否则用户找不到命令）。
    let help = stdout(&ui(&ws, &["--help"]));
    assert!(help.contains("ui remember"), "{}", help);
    assert!(help.contains("ui memory check"), "{}", help);
}
