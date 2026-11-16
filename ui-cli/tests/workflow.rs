//! 工作流 runner：步骤就是普通 turn，日志里要能看出走到哪一步。

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ui")
}

fn temp_ws(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ui-wf-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn runs_every_step_and_records_progress() {
    let ws = temp_ws("run");
    let scenario = ws.join("scenario.json");
    fs::write(
        &scenario,
        r#"{"model":"scripted","steps":[
            {"content":"第一步完成"},
            {"content":"第二步完成"}
        ]}"#,
    )
    .unwrap();
    let wf = ws.join("wf.json");
    fs::write(
        &wf,
        r#"{"name":"demo","steps":[
            {"prompt":"做第一步"},
            {"prompt":"做第二步","gate":true}
        ]}"#,
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
            "--yes",
        ])
        .output()
        .expect("workflow");

    assert!(
        out.status.success(),
        "workflow failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("第 1/2 步") && stdout.contains("第 2/2 步"), "{}", stdout);

    let events = ui_core::journal::Journal::load(ws.join(".ui").join("journal.jsonl")).unwrap();
    let kinds: Vec<&str> = events.iter().filter_map(|e| e.str_at("kind")).collect();
    assert_eq!(kinds.iter().filter(|k| **k == "workflow/step").count(), 2);
    assert!(kinds.contains(&"workflow/start"));
    assert!(kinds.contains(&"workflow/end"));
    assert_eq!(
        kinds.iter().filter(|k| **k == "turn/start").count(),
        2,
        "每一步都应是一次 turn"
    );
}

#[test]
fn step_skills_are_preloaded_and_recorded() {
    let ws = temp_ws("skills");
    let dir = ws.join("skills").join("security");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        "---\nname: security\ndescription: 安全评审清单\n---\n先看注入面。",
    )
    .unwrap();

    let scenario = ws.join("scenario.json");
    fs::write(
        &scenario,
        r#"{"model":"scripted","steps":[{"content":"按 skill 看过了"}]}"#,
    )
    .unwrap();
    let wf = ws.join("wf.json");
    fs::write(
        &wf,
        r#"{"name":"s","steps":[{"prompt":"做安全评审","skills":["security"]}]}"#,
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
            "--yes",
        ])
        .output()
        .expect("workflow");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("预加载 skill: security"));

    let events = ui_core::journal::Journal::load(ws.join(".ui").join("journal.jsonl")).unwrap();
    let load = events
        .iter()
        .find(|e| e.str_at("kind") == Some("skill/load"))
        .expect("应有 skill/load");
    assert_eq!(load.str_at("source"), Some("workflow"));
}

#[test]
fn parallel_group_spawns_children_with_their_own_journals() {
    let ws = temp_ws("parallel");
    let scenario = ws.join("scenario.json");
    fs::write(
        &scenario,
        r#"{"model":"scripted","steps":[{"content":"子代理完成"}]}"#,
    )
    .unwrap();
    let wf = ws.join("wf.json");
    fs::write(
        &wf,
        r#"{"name":"p","steps":[{"parallel":[
            {"prompt":"从安全角度评审"},
            {"prompt":"从性能角度评审"}
        ]}]}"#,
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
            "--yes",
        ])
        .output()
        .expect("workflow");
    assert!(
        out.status.success(),
        "workflow failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let events = ui_core::journal::Journal::load(ws.join(".ui").join("journal.jsonl")).unwrap();
    let delegates: Vec<_> = events
        .iter()
        .filter(|e| e.str_at("kind") == Some("delegate/end"))
        .collect();
    assert_eq!(delegates.len(), 2, "两个子代理都要记账");
    assert!(delegates.iter().all(|d| d.bool_at("ok") == Some(true)));
    assert!(
        delegates
            .iter()
            .all(|d| d.str_at("summary") == Some("子代理完成")),
        "摘要必须是子代理的答案，不是状态行: {:?}",
        delegates.iter().map(|d| d.str_at("summary")).collect::<Vec<_>>()
    );

    // 子代理各写各的日志（.ui/agents/<id>/journal.jsonl），父日志里只有摘要
    let agents = ws.join(".ui").join("agents");
    let files = fs::read_dir(&agents)
        .map(|d| {
            d.flatten()
                .filter(|e| e.path().join("journal.jsonl").is_file())
                .count()
        })
        .unwrap_or(0);
    assert_eq!(files, 2, "每个子代理应有独立 journal");
}

#[test]
fn isolated_children_write_in_copies_and_apply_brings_changes_back() {
    let ws = temp_ws("isolate");
    fs::write(ws.join("greet.js"), "export function greet(){}\n").unwrap();

    let scenario = ws.join("scenario.json");
    fs::write(
        &scenario,
        r#"{"model":"scripted","steps":[
            {"content":"写一个文件","toolCalls":[
                {"name":"fs_write","args":{"path":"note.txt","content":"来自子代理"}}]},
            {"content":"写完了"}
        ]}"#,
    )
    .unwrap();
    let wf = ws.join("wf.json");
    fs::write(
        &wf,
        r#"{"name":"iso","steps":[{"parallel":[{"prompt":"写个文件"}]}]}"#,
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

    // 隔离：父工作区不该出现 note.txt
    assert!(
        !ws.join("note.txt").exists(),
        "隔离模式下父工作区不能被直接改"
    );

    let events = ui_core::journal::Journal::load(ws.join(".ui").join("journal.jsonl")).unwrap();
    let rec = events
        .iter()
        .find(|e| e.str_at("kind") == Some("delegate/end"))
        .expect("应有 delegate/end");
    let changed: Vec<&str> = rec
        .arr_at("changed")
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert!(changed.contains(&"note.txt"), "改动清单: {:?}", changed);
    let id = rec.str_at("id").unwrap().to_string();
    let workdir = rec.str_at("workdir").expect("应记录 workdir").to_string();
    assert!(std::path::Path::new(&workdir).join("note.txt").is_file());

    // 计划模式：只打印，不写
    let plan = Command::new(bin())
        .args(["apply", "--id", &id, "--workspace", ws.to_str().unwrap()])
        .output()
        .expect("apply");
    assert!(plan.status.success());
    assert!(String::from_utf8_lossy(&plan.stdout).contains("计划模式"));
    assert!(!ws.join("note.txt").exists());

    // --yes 才写回
    let apply = Command::new(bin())
        .args(["apply", "--id", &id, "--workspace", ws.to_str().unwrap(), "--yes"])
        .output()
        .expect("apply");
    assert!(apply.status.success(), "{}", String::from_utf8_lossy(&apply.stderr));
    assert_eq!(
        fs::read_to_string(ws.join("note.txt")).unwrap().trim(),
        "来自子代理"
    );
}

#[test]
fn killing_the_parent_cancels_the_children() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let ws = temp_ws("cancel");
    let scenario = ws.join("scenario.json");
    // 子代理会卡在一条慢命令里，正好用来验证「父死子停」
    fs::write(
        &scenario,
        r#"{"model":"scripted","steps":[
            {"content":"跑个慢命令","toolCalls":[{"name":"shell","args":{"command":"sleep 20"}}]},
            {"content":"不该到这里"}]}"#,
    )
    .unwrap();
    let wf = ws.join("wf.json");
    fs::write(
        &wf,
        r#"{"name":"c","steps":[{"parallel":[{"prompt":"慢任务"}]}]}"#,
    )
    .unwrap();

    let mut parent = Command::new(bin())
        .args([
            "workflow",
            "--file",
            wf.to_str().unwrap(),
            "--mock",
            scenario.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--yes",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn parent");

    let child_journal = ws
        .join(".ui")
        .join("agents")
        .join("c-s1-1")
        .join("journal.jsonl");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !child_journal.is_file() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(child_journal.is_file(), "子代理应该已经起来");

    // 等到子代理真的进了工具调用再杀父进程：这样测的是"跑着的时候被取消"，
    // 而不是"启动竞态"。
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(text) = fs::read_to_string(&child_journal) {
            if text.contains(r#""kind":"tool/start""#) {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    parent.kill().expect("kill parent");
    let _ = parent.wait();

    // 父进程死了，子进程应当在 EOF 后自我取消，并写下 turn/end cancelled
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut cancelled = false;
    while Instant::now() < deadline {
        if let Ok(text) = fs::read_to_string(&child_journal) {
            if text.contains(r#""reason":"cancelled""#) {
                cancelled = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(cancelled, "父进程被 kill 后，子进程必须自己停下来");
}

#[test]
fn apply_refuses_to_overwrite_when_two_delegates_disagree() {
    use ui_core::json::Json;

    let ws = temp_ws("conflict");
    let a = ws.join("child-a");
    let b = ws.join("child-b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("same.txt"), "来自 A\n").unwrap();
    fs::write(b.join("same.txt"), "来自 B\n").unwrap();

    let mut journal =
        ui_core::Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
    for (id, dir) in [("c1", &a), ("c2", &b)] {
        journal
            .append(
                "delegate/end",
                Json::obj(vec![
                    ("id", Json::from(id)),
                    ("ok", Json::Bool(true)),
                    ("summary", Json::from("x")),
                    ("journal", Json::from("x")),
                    (
                        "workdir",
                        Json::from(dir.to_string_lossy().to_string()),
                    ),
                    ("changed", Json::Arr(vec![Json::from("same.txt")])),
                ]),
            )
            .unwrap();
    }

    let out = Command::new(bin())
        .args([
            "apply",
            "--all",
            "--workspace",
            ws.to_str().unwrap(),
            "--yes",
        ])
        .output()
        .expect("apply");
    assert_eq!(out.status.code(), Some(3), "有冲突时必须返回非零");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("冲突"), "{}", stdout);
    assert!(
        !ws.join("same.txt").exists(),
        "冲突文件不能被自动写入（宁可不合，也不能猜）"
    );
}

#[test]
fn apply_refuses_to_overwrite_a_file_that_drifted() {
    let ws = temp_ws("drift");
    fs::write(ws.join("a.txt"), "v1\n").unwrap();

    let scenario = ws.join("scenario.json");
    fs::write(
        &scenario,
        r#"{"model":"scripted","steps":[
            {"content":"改成 v2","toolCalls":[
                {"name":"fs_write","args":{"path":"a.txt","content":"v2\n"}}]},
            {"content":"done"}]}"#,
    )
    .unwrap();
    let wf = ws.join("wf.json");
    fs::write(
        &wf,
        r#"{"name":"d","steps":[{"parallel":[{"prompt":"改文件"}]}]}"#,
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
    assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "v1\n");

    // 子代理跑完之后，父工作区里这个文件被人改了 → 漂移
    fs::write(ws.join("a.txt"), "v3\n").unwrap();

    let refused = Command::new(bin())
        .args(["apply", "--all", "--workspace", ws.to_str().unwrap(), "--yes"])
        .output()
        .expect("apply");
    assert_eq!(
        refused.status.code(),
        Some(4),
        "漂移用独立的退出码 4（冲突是 3）"
    );
    assert!(String::from_utf8_lossy(&refused.stdout).contains("漂移"));
    assert_eq!(
        fs::read_to_string(ws.join("a.txt")).unwrap(),
        "v3\n",
        "默认不能覆盖漂移后的文件"
    );

    // --json：机器可读，stdout 只有一行 JSON
    let machine = Command::new(bin())
        .args([
            "apply",
            "--all",
            "--workspace",
            ws.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("apply json");
    assert_eq!(machine.status.code(), Some(4));
    let stdout = String::from_utf8_lossy(&machine.stdout);
    let parsed = ui_core::json::parse(stdout.trim()).expect("stdout 必须是合法 JSON");
    assert_eq!(parsed.bool_at("ok"), Some(false));
    assert_eq!(parsed.u64_at("exit"), Some(4));
    let drift: Vec<&str> = parsed
        .arr_at("drift")
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert_eq!(drift, vec!["a.txt"]);

    // --force 才覆盖
    let forced = Command::new(bin())
        .args([
            "apply",
            "--all",
            "--workspace",
            ws.to_str().unwrap(),
            "--yes",
            "--force",
        ])
        .output()
        .expect("apply force");
    assert!(forced.status.success());
    assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "v2\n");
}

#[test]
fn workflow_json_reports_the_outcome_on_stdout_only() {
    let ws = temp_ws("json");
    let scenario = ws.join("scenario.json");
    fs::write(
        &scenario,
        r#"{"model":"scripted","steps":[
            {"content":"一"},{"content":"二"}]}"#,
    )
    .unwrap();
    let wf = ws.join("wf.json");
    fs::write(
        &wf,
        r#"{"name":"j","steps":[{"prompt":"第一步"},{"prompt":"第二步","gate":true}]}"#,
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
            "--yes",
            "--json",
        ])
        .output()
        .expect("workflow --json");

    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed = ui_core::json::parse(stdout.trim()).expect("stdout 必须是单行 JSON");
    assert_eq!(parsed.bool_at("ok"), Some(true));
    assert_eq!(parsed.u64_at("exit"), Some(0));
    assert_eq!(parsed.str_at("workflow"), Some("j"));
    assert_eq!(parsed.str_at("reason"), Some("done"));
    // 进度信息应该在 stderr，而不是混进 JSON
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("第 1/2 步"), "进度应走 stderr: {}", stderr);
}

#[test]
fn gate_stops_the_workflow_when_not_approved() {
    let ws = temp_ws("gate");
    let scenario = ws.join("scenario.json");
    fs::write(
        &scenario,
        r#"{"model":"scripted","steps":[{"content":"不该跑到这里"}]}"#,
    )
    .unwrap();
    let wf = ws.join("wf.json");
    fs::write(
        &wf,
        r#"{"name":"gated","steps":[{"prompt":"危险动作","gate":true}]}"#,
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
        ])
        .output()
        .expect("workflow");

    assert_eq!(out.status.code(), Some(1), "闸门未批准应返回 1");
    let events = ui_core::journal::Journal::load(ws.join(".ui").join("journal.jsonl")).unwrap();
    let end = events
        .iter()
        .find(|e| e.str_at("kind") == Some("workflow/end"))
        .expect("应有 workflow/end");
    assert_eq!(end.str_at("reason"), Some("gated"));
    assert_eq!(
        events.iter().filter(|e| e.str_at("kind") == Some("turn/start")).count(),
        0,
        "未批准的步骤不应产生 turn"
    );
}
