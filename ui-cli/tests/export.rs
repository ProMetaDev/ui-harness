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
