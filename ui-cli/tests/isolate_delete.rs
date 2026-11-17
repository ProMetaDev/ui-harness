//! 隔离子代理的**删除**行为：必须被报告，但绝不自动合并。
//!
//! 背景：删除是不可逆操作，自动删是危险的；但完全不提也是错的——
//! 父工作区会留下一个本该消失的文件，而日志假装无事发生。

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ui")
}

fn temp_ws(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ui-del-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_child_deleting_a_file_is_reported_but_not_applied() {
    let ws = temp_ws("report");
    fs::write(ws.join("extra.txt"), "should stay").unwrap();

    let wf = ws.join("wf.json");
    fs::write(
        &wf,
        r#"{"name":"w","steps":[{"parallel":[{"prompt":"删掉 extra.txt"}]}]}"#,
    )
    .unwrap();
    let mock = ws.join("mock.json");
    fs::write(
        &mock,
        r#"{"model":"scripted","steps":[
            {"content":"删","toolCalls":[{"name":"shell","args":{"command":"rm extra.txt"}}]},
            {"content":"done"}]}"#,
    )
    .unwrap();

    let out = Command::new(bin())
        .args([
            "workflow", "--file", wf.to_str().unwrap(), "--mock", mock.to_str().unwrap(),
            "--workspace", ws.to_str().unwrap(), "--isolate", "--yes",
        ])
        .output()
        .expect("workflow");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    // 隔离：父工作区的文件不能被删
    assert!(ws.join("extra.txt").exists(), "子代理的删除不该影响父工作区");

    // 日志里必须记下"删了什么"
    let journal = fs::read_to_string(ws.join(".ui").join("journal.jsonl")).unwrap();
    assert!(
        journal.contains("\"deleted\"") && journal.contains("extra.txt"),
        "delegate/end 里应当有 deleted 字段：{}",
        journal.lines().filter(|l| l.contains("delegate/end")).collect::<Vec<_>>().join("\n")
    );

    // apply 必须**说出来**，且仍然不删
    let out = Command::new(bin())
        .args(["apply", "--all", "--workspace", ws.to_str().unwrap(), "--yes"])
        .output()
        .expect("apply");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("删除"), "apply 应当报告删除：{}", text);
    assert!(ws.join("extra.txt").exists(), "apply 不能自动删文件");

    let _ = fs::remove_dir_all(&ws);
}
