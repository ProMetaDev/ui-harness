//! 导出的查看器必须是「一个文件、自带数据、离线可用」。
//! 这里验证它真的把日志嵌进去了，而且没有留下占位符。

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ui")
}

fn temp_ws(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ui-export-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn exports_a_self_contained_html_trace() {
    let ws = temp_ws("viewer");
    let scenario = ws.join("scenario.json");
    fs::write(
        &scenario,
        r#"{
          "model": "scripted",
          "steps": [
            { "content": "先看看文件", "toolCalls": [
              { "name": "fs_read", "args": { "path": "nope.txt" } } ] },
            { "content": "没有这个文件，先说结论。" }
          ]
        }"#,
    )
    .unwrap();

    let run = Command::new(bin())
        .args([
            "run",
            "--task",
            "probe",
            "--mock",
            scenario.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--yes",
        ])
        .output()
        .expect("run");
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));

    let out = ws.join("trace.html");
    let export = Command::new(bin())
        .args([
            "export",
            "--workspace",
            ws.to_str().unwrap(),
            "--html",
            out.to_str().unwrap(),
            "--max-tokens",
            "12345",
        ])
        .output()
        .expect("export");
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );

    let html = fs::read_to_string(&out).expect("html written");
    assert!(html.contains("UI · 执行轨迹"), "viewer shell missing");
    assert!(
        !html.contains("__UI_DATA__"),
        "placeholder must be replaced"
    );
    assert!(
        html.contains(r#""kind":"prompt/snapshot""#),
        "journal events must be embedded"
    );
    assert!(
        html.contains(r#""maxTokens":12345"#),
        "exported limits must reflect the flags"
    );
    // 自带样式与脚本，不依赖网络
    assert!(html.contains("<style>") && html.contains("<script>"));
    assert!(!html.contains("http://") && !html.contains("https://"));
}

/// 不属于任何一步的操作（合并、回滚、排队时被撤掉的任务）过去在查看器里
/// 根本不存在：分组循环只认 `prompt/snapshot` 锚点，其它一律丢掉。
/// 这个测试钉住两件事——旁路账要出现，且这些事件要有"人话"而不是掉回裸 JSON。
#[test]
fn the_viewer_shows_events_that_belong_to_no_turn() {
    let ws = temp_ws("side");
    let scenario = ws.join("scenario.json");
    fs::write(
        &scenario,
        r#"{"model":"scripted","steps":[{"content":"看一眼"},{"content":"再看一眼"}]}"#,
    )
    .unwrap();
    let run = Command::new(bin())
        .args([
            "run",
            "--task",
            "probe",
            "--mock",
            scenario.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--yes",
        ])
        .output()
        .expect("run");
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));

    // 这些是真实内核会写的事件；用 Journal 追加，链仍然是完整的（export 会校验）
    let mut journal = ui_core::Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
    let arr = |v: &[&str]| ui_core::json::Json::Arr(v.iter().map(|s| ui_core::json::Json::from(*s)).collect());
    journal
        .append(
            "turn/skipped",
            ui_core::json::Json::obj(vec![
                ("task", ui_core::json::Json::from("t2")),
                ("reason", ui_core::json::Json::from("cancelled-while-queued")),
                ("text", ui_core::json::Json::from("这条排队，然后被我撤掉")),
            ]),
        )
        .unwrap();
    journal
        .append(
            "apply/begin",
            ui_core::json::Json::obj(vec![
                ("txn", ui_core::json::Json::from("00000009-abcd1234")),
                ("files", arr(&["a.txt", "b.txt"])),
            ]),
        )
        .unwrap();
    journal
        .append(
            "apply/rolled-back",
            ui_core::json::Json::obj(vec![
                ("txn", ui_core::json::Json::from("00000009-abcd1234")),
                ("stage", ui_core::json::Json::from("rollback")),
                ("restored", arr(&["a.txt"])),
            ]),
        )
        .unwrap();

    let out = ws.join("side.html");
    let export = Command::new(bin())
        .args([
            "export",
            "--workspace",
            ws.to_str().unwrap(),
            "--html",
            out.to_str().unwrap(),
        ])
        .output()
        .expect("export");
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    let html = fs::read_to_string(&out).unwrap();
    assert!(
        html.contains(r#""kind":"apply/rolled-back""#),
        "旁路事件必须进了导出数据"
    );
    // 渲染侧：有这张卡、按"是否已在某一步里"分组、这些 kind 各有自己的说法
    assert!(html.contains("旁路事件"), "查看器没有旁路账这张卡");
    assert!(
        html.contains("inStep.has(e)"),
        "分组判据必须是 inStep 集合（曾引用过早已不写的标记，于是整张卡退化成全部事件）"
    );
    assert!(
        !html.contains("__inStep"),
        "不许留下没人写入的旧标记名"
    );
    assert!(
        html.contains("if (e.kind === \"turn/end\") cur = null;"),
        "一轮结束必须关掉当前步骤，否则这一步之后的所有事件都会被算进步骤里"
    );
    for kind in ["apply/begin", "apply/committed", "apply/rolled-back", "turn/skipped", "delegate/end"] {
        assert!(
            html.contains(&format!("case \"{}\":", kind)),
            "{} 没有自己的说明文案，会掉回裸 JSON",
            kind
        );
    }
}
