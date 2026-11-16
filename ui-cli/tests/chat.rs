//! The interactive surface, tested through a pipe: same loop, same journal,
//! several turns.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ui")
}

fn temp_ws(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ui-chat-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn chat_runs_two_turns_on_one_journal() {
    let ws = temp_ws("two");
    let scenario = ws.join("scenario.json");
    fs::write(
        &scenario,
        r#"{
          "model": "scripted",
          "steps": [
            { "content": "第一轮收到" },
            { "content": "第二轮收到" }
          ]
        }"#,
    )
    .unwrap();

    let mut child = Command::new(bin())
        .args([
            "chat",
            "--mock",
            scenario.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ui chat");

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all("第一件事\n/status\n第二件事\n/quit\n".as_bytes())
        .unwrap();

    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "chat failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("第一轮收到"), "got: {}", stdout);
    assert!(stdout.contains("第二轮收到"), "got: {}", stdout);
    assert!(stdout.contains("tok"), "/status must show the ledger");

    // Both turns share one journal, and it verifies.
    let journal = ws.join(".ui").join("journal.jsonl");
    let events = ui_core::journal::Journal::load(&journal).expect("journal verifies");
    let turns = events
        .iter()
        .filter(|e| e.str_at("kind") == Some("turn/start"))
        .count();
    assert_eq!(turns, 2, "one journal, two turns");
}
