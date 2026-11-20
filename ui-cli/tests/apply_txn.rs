//! `ui apply --yes` 是一个事务：要么全部落地，要么整批退回原样。
//!
//! 背景（Phase 0 第 7 项）：回写原来是 `for … std::fs::copy(src, dst)?`。
//! 第 3 个文件写失败时前 2 个已经改完、第 3 个可能只写了一半（`copy` 先
//! 截断再拷），`?` 抛出错误后工作区停在「一半新一半旧」的状态，日志里
//! 一个字没提，也没有任何手段退回原样。
//!
//! 这几条测试盯着三件事：
//! - 写回中途失败 → 已经写的那几个必须自动退回原样，且这笔在日志里查得到；
//! - 提交后 `--rollback` → 原样回来，且**不会**抹掉用户在 apply 之后的改动；
//! - 上一次事务死在半路 → 下一条 apply 必须先报警，不在残缺状态上叠写。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ui")
}

fn temp_ws(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ui-txn-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// 起一个隔离子代理，让它在副本里改 `a.txt` 和 `b.txt`。
///
/// 返回 delegate/end 里的子代理 ID——`ui apply --id` 认的就是它。
fn isolate_two_edits(ws: &Path) -> String {
    fs::write(ws.join("a.txt"), "old a").unwrap();
    fs::write(ws.join("b.txt"), "old b").unwrap();

    let scenario = ws.join("scenario.json");
    fs::write(
        &scenario,
        r#"{"model":"scripted","steps":[
            {"content":"改两个文件","toolCalls":[
                {"name":"fs_write","args":{"path":"a.txt","content":"新的 a"}},
                {"name":"fs_write","args":{"path":"b.txt","content":"新的 b"}}]},
            {"content":"完成"}
        ]}"#,
    )
    .unwrap();
    let wf = ws.join("wf.json");
    fs::write(
        &wf,
        r#"{"name":"txn","steps":[{"parallel":[{"prompt":"改两个文件"}]}]}"#,
    )
    .unwrap();

    let out = Command::new(bin())
        .args([
            "workflow",
            "--file",
            wf.to_str().unwrap(),
            "--mock",
            scenario.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--isolate",
            "--yes",
        ])
        .output()
        .expect("workflow");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        fs::read_to_string(ws.join("a.txt")).unwrap(),
        "old a",
        "隔离模式下父工作区不该被子代理动过"
    );

    let events = ui_core::journal::Journal::load(ws.join(".ui").join("journal.jsonl")).unwrap();
    events
        .iter()
        .rev()
        .find(|e| e.str_at("kind") == Some("delegate/end"))
        .expect("应有 delegate/end")
        .str_at("id")
        .expect("delegate/end 应有 id")
        .to_string()
}

fn apply(ws: &Path, extra: &[&str]) -> (i32, String) {
    let mut cmd = Command::new(bin());
    cmd.args(["apply", "--workspace", ws.to_str().unwrap()]);
    cmd.args(extra);
    let out = cmd.output().expect("apply");
    (
        out.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

fn journal_text(ws: &Path) -> String {
    fs::read_to_string(ws.join(".ui").join("journal.jsonl")).unwrap_or_default()
}

/// `.ui/apply/` 下还留着哪些事务（目录名）。
fn txn_dirs(ws: &Path) -> Vec<String> {
    let root = ws.join(".ui").join("apply");
    let mut names: Vec<String> = match fs::read_dir(&root) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect(),
        Err(_) => return Vec::new(),
    };
    names.sort();
    names
}

/// 写回中途失败：第 1 个文件已经落地，第 2 个写不进去——整批必须退回原样。
///
/// 用「父工作区里 `b.txt` 变成了目录」来制造失败：`copy` 打开目标就报
/// EISDIR，跟权限、磁盘满同一类后果，但不需要 root 也不需要特殊环境。
#[test]
fn a_half_written_batch_is_rolled_back_to_the_originals() {
    let ws = temp_ws("rollback");
    let id = isolate_two_edits(&ws);

    // 子代理跑完之后，把父工作区的 b.txt 换成目录 → 这个目标写不进去。
    // 同时它也构成「漂移」，所以要 --force 才能进到写回阶段。
    fs::remove_file(ws.join("b.txt")).unwrap();
    fs::create_dir_all(ws.join("b.txt")).unwrap();
    fs::write(ws.join("b.txt").join("inner"), "别人的东西").unwrap();

    let (code, text) = apply(&ws, &["--id", &id, "--yes", "--force"]);
    assert_eq!(code, 5, "回写失败要退出 5：{}", text);
    assert!(text.contains("回写失败"), "{}", text);

    // a.txt 一度被写成「新的 a」，事务必须把它退回「old a」
    assert_eq!(
        fs::read_to_string(ws.join("a.txt")).unwrap(),
        "old a",
        "中途失败后不能留下半批新内容"
    );
    // 那个目录是本来就在的，不是事务产物，不能被动过
    assert_eq!(
        fs::read_to_string(ws.join("b.txt").join("inner")).unwrap(),
        "别人的东西"
    );

    // 这笔操作在日志里查得到：开始过、也回滚了，原因写清楚
    let journal = journal_text(&ws);
    assert!(journal.contains("\"apply/begin\""), "{}", journal);
    assert!(
        journal.contains("\"apply/rolled-back\"") && journal.contains("写回 b.txt 失败"),
        "回滚必须记进日志：{}",
        journal
    );

    // b.txt 那个目标没能「退干净」（它本来就不是文件），所以留底目录要留着
    let dirs = txn_dirs(&ws);
    assert_eq!(dirs.len(), 1, "没退干净时留底必须留着：{:?}", dirs);
    let manifest =
        fs::read_to_string(ws.join(".ui").join("apply").join(&dirs[0]).join("manifest.json"))
            .unwrap();
    assert!(manifest.contains("rolled-back"), "{}", manifest);
    assert!(
        manifest.contains("\"backup\"") && manifest.contains("0000-a.txt"),
        "a.txt 的原样应当有留底：{}",
        manifest
    );
    let backup = ws.join(".ui").join("apply").join(&dirs[0]).join("before");
    let kept: Vec<String> = fs::read_dir(&backup)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(kept.iter().any(|n| n.ends_with("a.txt")), "{:?}", kept);
    assert_eq!(
        fs::read_to_string(backup.join(&kept[0])).unwrap(),
        "old a",
        "留底必须是写回之前的内容"
    );

    let _ = fs::remove_dir_all(&ws);
}

/// 提交后能整批退回原样；计划模式（不带 `--yes`）一个字节都不动。
#[test]
fn a_committed_apply_can_be_rolled_back_to_the_originals() {
    let ws = temp_ws("commit");
    let id = isolate_two_edits(&ws);

    let (code, text) = apply(&ws, &["--id", &id, "--yes"]);
    assert_eq!(code, 0, "{}", text);
    assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "新的 a");
    assert_eq!(fs::read_to_string(ws.join("b.txt")).unwrap(), "新的 b");
    assert!(text.contains("ui apply --rollback"), "要告诉用户怎么退：{}", text);

    let dirs = txn_dirs(&ws);
    assert_eq!(dirs.len(), 1, "{:?}", dirs);
    let manifest = fs::read_to_string(
        ws.join(".ui").join("apply").join(&dirs[0]).join("manifest.json"),
    )
    .unwrap();
    assert!(manifest.contains("committed"), "{}", manifest);
    assert!(journal_text(&ws).contains("\"apply/committed\""));

    // 先计划：只说打算做什么
    let (code, text) = apply(&ws, &["--rollback"]);
    assert_eq!(code, 0, "{}", text);
    assert!(text.contains("计划模式"), "{}", text);
    assert_eq!(
        fs::read_to_string(ws.join("a.txt")).unwrap(),
        "新的 a",
        "计划模式不能动盘"
    );

    let (code, text) = apply(&ws, &["--rollback", "--yes"]);
    assert_eq!(code, 0, "{}", text);
    assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "old a");
    assert_eq!(fs::read_to_string(ws.join("b.txt")).unwrap(), "old b");
    assert!(journal_text(&ws).contains("\"stage\":\"rollback\""));
    assert!(
        txn_dirs(&ws).is_empty(),
        "退干净了就该把留底扔掉：{:?}",
        txn_dirs(&ws)
    );

    // 再退一次：已经没有可退的事务，要说清楚而不是假装成功
    let (code, text) = apply(&ws, &["--rollback", "--yes"]);
    assert_ne!(code, 0, "{}", text);
    assert!(text.contains("没有") || text.contains("退回原样"), "{}", text);

    let _ = fs::remove_dir_all(&ws);
}

/// 事务新建的文件（写回之前不存在）：回滚是删掉它，连带事务建的空目录。
#[test]
fn a_file_the_apply_created_is_removed_by_the_rollback() {
    let ws = temp_ws("newfile");
    fs::write(ws.join("a.txt"), "old a").unwrap();

    let scenario = ws.join("scenario.json");
    fs::write(
        &scenario,
        r#"{"model":"scripted","steps":[
            {"content":"新建一个文件","toolCalls":[
                {"name":"fs_write","args":{"path":"notes/brand-new.txt","content":"子代理造的"}}]},
            {"content":"完成"}
        ]}"#,
    )
    .unwrap();
    let wf = ws.join("wf.json");
    fs::write(
        &wf,
        r#"{"name":"neo","steps":[{"parallel":[{"prompt":"新建文件"}]}]}"#,
    )
    .unwrap();
    let out = Command::new(bin())
        .args([
            "workflow",
            "--file",
            wf.to_str().unwrap(),
            "--mock",
            scenario.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--isolate",
            "--yes",
        ])
        .output()
        .expect("workflow");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let events = ui_core::journal::Journal::load(ws.join(".ui").join("journal.jsonl")).unwrap();
    let id = events
        .iter()
        .rev()
        .find(|e| e.str_at("kind") == Some("delegate/end"))
        .expect("delegate/end")
        .str_at("id")
        .unwrap()
        .to_string();

    let (code, text) = apply(&ws, &["--id", &id, "--yes"]);
    assert_eq!(code, 0, "{}", text);
    assert!(
        ws.join("notes").join("brand-new.txt").is_file(),
        "新文件连同它的目录都应当落地"
    );

    let (code, text) = apply(&ws, &["--rollback", "--yes"]);
    assert_eq!(code, 0, "{}", text);
    assert!(
        !ws.join("notes").join("brand-new.txt").exists(),
        "事务造出来的文件必须被删掉"
    );
    assert!(
        !ws.join("notes").exists(),
        "事务顺手建的空目录也要收掉，否则没退回「之前」"
    );
    assert_eq!(
        fs::read_to_string(ws.join("a.txt")).unwrap(),
        "old a",
        "没动过的文件不该被动"
    );

    let _ = fs::remove_dir_all(&ws);
}

/// `--rollback` 不能覆盖用户在 apply 之后的改动。
///
/// 事务的边界是「我写的那一份」：apply 之后人自己又改了，退回去就是拿
/// 两个月前的留底盖掉今天的劳动——那不是回滚，是数据丢失。
#[test]
fn rollback_refuses_to_overwrite_an_edit_made_after_the_apply() {
    let ws = temp_ws("guard");
    let id = isolate_two_edits(&ws);

    let (code, _) = apply(&ws, &["--id", &id, "--yes"]);
    assert_eq!(code, 0);

    // 用户在 apply 之后改了 a.txt
    fs::write(ws.join("a.txt"), "我自己改的").unwrap();

    let (code, text) = apply(&ws, &["--rollback", "--yes"]);
    assert_eq!(code, 4, "有文件没退成要退出 4（漂移）：{}", text);
    assert_eq!(
        fs::read_to_string(ws.join("a.txt")).unwrap(),
        "我自己改的",
        "用户的内容不能被留底盖掉"
    );
    assert_eq!(
        fs::read_to_string(ws.join("b.txt")).unwrap(),
        "old b",
        "其余文件照常退回原样"
    );
    assert!(text.contains("保留"), "{}", text);

    // 留底必须还在：a.txt 那一份还需要人工对照
    let dirs = txn_dirs(&ws);
    assert_eq!(dirs.len(), 1, "{:?}", dirs);
    assert!(ws.join(".ui").join("apply").join(&dirs[0]).join("before").is_dir());

    let _ = fs::remove_dir_all(&ws);
}

/// 上一次事务死在写回途中 → 下一条 apply 必须先报警，不能叠上去。
#[test]
fn a_transaction_left_mid_write_blocks_the_next_apply() {
    let ws = temp_ws("dangling");
    let id = isolate_two_edits(&ws);

    // 伪造一个「停在 applying」的事务档案：等价于上次进程死在写回途中
    let dir = ws.join(".ui").join("apply").join("00000001-deadbeef");
    fs::create_dir_all(dir.join("before")).unwrap();
    fs::write(dir.join("before/0000-a.txt"), "old a").unwrap();
    fs::write(
        dir.join("manifest.json"),
        r#"{
  "txn": "00000001-deadbeef",
  "status": "applying",
  "entries": [
    {"rel":"a.txt","src":"/nope","backup":"0000-a.txt","before":"x","after":"y"}
  ]
}"#,
    )
    .unwrap();

    let (code, text) = apply(&ws, &["--id", &id, "--yes"]);
    assert_eq!(code, 5, "残缺状态上要停下来：{}", text);
    assert!(text.contains("停在写回途中"), "{}", text);
    assert_eq!(
        fs::read_to_string(ws.join("a.txt")).unwrap(),
        "old a",
        "报警时一个文件都不能写"
    );

    // 人工确认「就在残缺状态上继续」才放行，而且这个决定要被记下来——
    // 否则那条闸会一辈子拦住后续 apply，每次都得 --force。
    let (code, text) = apply(&ws, &["--id", &id, "--yes", "--force"]);
    assert_eq!(code, 0, "{}", text);
    assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "新的 a");
    let manifest = fs::read_to_string(dir.join("manifest.json")).unwrap();
    assert!(manifest.contains("abandoned"), "{}", manifest);
    assert!(journal_text(&ws).contains("\"apply/abandoned\""));

    // 再合另一个目标：不该再被那笔旧事务拦住（此时 a.txt 已漂移，退 4）
    let (code, text) = apply(&ws, &["--id", &id, "--yes"]);
    assert_eq!(code, 4, "旧事务不该继续拦路，应当只剩漂移闸：{}", text);
    assert!(!text.contains("停在写回途中"), "{}", text);

    let _ = fs::remove_dir_all(&ws);
}
