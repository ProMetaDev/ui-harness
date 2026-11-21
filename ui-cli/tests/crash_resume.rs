//! The headline contract, tested with a real process kill.
//!
//! Spawn the real binary, let it reach a long-running side effect, SIGKILL it,
//! then check that (a) the journal still verifies, (b) the started-but-unfinished
//! call is visible, and (c) `ui resume` reconciles it instead of pretending it
//! never happened.

use ui_core::journal::{analyze_incomplete, Journal};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ui")
}

fn temp_ws(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ui-crash-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_scenario(ws: &Path) -> PathBuf {
    let path = ws.join("scenario.json");
    let scenario = r#"{
      "model": "scripted",
      "steps": [
        {
          "content": "先跑一个慢命令",
          "toolCalls": [
            { "name": "shell", "args": { "command": "sleep 5" } }
          ]
        },
        { "content": "不该走到这里" }
      ]
    }"#;
    fs::write(&path, scenario).unwrap();
    path
}

fn journal_of(ws: &Path) -> PathBuf {
    ws.join(".ui").join("journal.jsonl")
}

fn wait_for(path: &Path, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(text) = fs::read_to_string(path) {
            if text.contains(needle) {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    false
}

#[test]
fn a_killed_side_effect_is_reconciled_not_lost() {
    let ws = temp_ws("kill");
    let scenario = write_scenario(&ws);
    let jp = journal_of(&ws);

    let mut child = Command::new(bin())
        .args([
            "run",
            "--task",
            "跑个慢命令",
            "--mock",
            scenario.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--yes",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ui run");

    assert!(
        wait_for(&jp, "tool/start", Duration::from_secs(10)),
        "the write-ahead entry must appear before the effect runs"
    );
    child.kill().expect("kill mid-effect");
    let _ = child.wait();

    // The log must still verify after the kill: no half-written line, no bad hash.
    let events = Journal::load(&jp).expect("journal must verify after a crash");
    let incomplete = analyze_incomplete(&events);
    assert_eq!(
        incomplete.open_tools.len(),
        1,
        "one call was started and never finished"
    );
    assert_eq!(incomplete.open_tools[0].str_at("tool"), Some("shell"));
    assert!(
        incomplete.open_turn.is_some(),
        "the turn itself was left open"
    );

    // Resume: the human may replay, and then the call is marked, not hidden.
    let out = Command::new(bin())
        .args([
            "resume",
            "--workspace",
            ws.to_str().unwrap(),
            "--yes",
        ])
        .output()
        .expect("spawn ui resume");
    assert!(
        out.status.success(),
        "resume failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let events = Journal::load(&jp).expect("journal must verify after resume");
    let kinds: Vec<&str> = events.iter().filter_map(|e| e.str_at("kind")).collect();
    assert!(
        kinds.contains(&"effect/rerun") || kinds.contains(&"effect/unknown"),
        "resume must record a verdict about the interrupted effect, got {:?}",
        kinds
    );
    assert!(kinds.contains(&"turn/end"));

    let after = analyze_incomplete(&events);
    assert!(
        after.open_tools.is_empty(),
        "nothing may remain open after resume"
    );
    assert!(
        after.open_turn.is_none(),
        "resume must close the interrupted turn even though it writes a new writer id"
    );
}

#[test]
fn resume_is_a_noop_on_a_clean_journal() {
    let ws = temp_ws("clean");
    fs::create_dir_all(ws.join(".ui")).unwrap();
    fs::write(journal_of(&ws), "").unwrap();

    let out = Command::new(bin())
        .args(["resume", "--workspace", ws.to_str().unwrap()])
        .output()
        .expect("spawn ui resume");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("没有需要恢复的工作"),
        "got: {}",
        stdout
    );
}
