//! `BOOL_FLAGS` 防遗漏测试。
//!
//! 为什么要有：布尔开关表（`Args::parse` 里的 `BOOL_FLAGS`）和散落各处的
//! `args.flag("...")` 是两份手工维护的名单。0.3.0 加了 7 个新开关，表里漏了，
//! 于是 `ui --yes run --task hi` 会把子命令 `run` 吃成 `yes` 的值——子命令整个丢。
//!
//! 这里直接扫 `main.rs` 源码：任何被 `args.flag("...")` 读取的开关，要么在
//! `BOOL_FLAGS` 里，要么在下面的显式例外表里写清为什么不能列。漏一个就红，
//! 报错信息列出全部缺失项。

use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// 被测源码本身（相对本测试文件）。扫源码而不是维护第二份名单。
const MAIN_RS: &str = include_str!("../src/main.rs");

const BOOL_FLAGS_MARKER: &str = "const BOOL_FLAGS: &[&str] = &[";

/// 显式例外：会出现在 `args.flag("...")` 里，但**故意**不列进 `BOOL_FLAGS`。
///
/// 第二列是原因；`exclusion_table_stays_honest` 要求它非空可读。
const NOT_BOOL_FLAGS: &[(&str, &str)] = &[
    (
        "checkpoint",
        "--checkpoint 是双语义：`ui run --checkpoint` 是开关，\
         `ui diff/review/revert/restore --checkpoint <id>` 是键值选项；\
         列进 BOOL_FLAGS 会让 <id> 被当成位置参数静默忽略，diff 到错的基线",
    ),
    (
        "trusted-proxy",
        "--trusted-proxy 是键值选项；flag() 只用来识别「给了名字却没给值」，\
         必须把后面的值留给解析器",
    ),
];

fn bool_flags() -> Vec<String> {
    let start = MAIN_RS
        .find(BOOL_FLAGS_MARKER)
        .unwrap_or_else(|| panic!("main.rs 里找不到 `{BOOL_FLAGS_MARKER}`"));
    let body_start = start + BOOL_FLAGS_MARKER.len();
    let body_end = body_start
        + MAIN_RS[body_start..]
            .find("];")
            .unwrap_or_else(|| panic!("main.rs 的 BOOL_FLAGS 数组没有 `];` 结尾"));
    MAIN_RS[body_start..body_end]
        .split('"')
        .skip(1)
        .step_by(2)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn referenced_flags() -> Vec<String> {
    const CALL: &str = "args.flag(\"";
    let mut found = Vec::new();
    for line in MAIN_RS.lines() {
        // 注释里的示例不算调用。
        if line.trim_start().starts_with("//") {
            continue;
        }
        let mut rest = line;
        while let Some(pos) = rest.find(CALL) {
            let after = &rest[pos + CALL.len()..];
            match after.find('"') {
                Some(end) => {
                    found.push(after[..end].to_string());
                    rest = &after[end..];
                }
                None => break,
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

#[test]
fn every_flag_read_is_in_bool_flags() {
    let declared = bool_flags();
    let referenced = referenced_flags();
    let excluded: Vec<&str> = NOT_BOOL_FLAGS.iter().map(|(name, _)| *name).collect();
    let missing: Vec<&str> = referenced
        .iter()
        .map(String::as_str)
        .filter(|f| !declared.iter().any(|d| d.as_str() == *f) && !excluded.contains(f))
        .collect();
    assert!(
        missing.is_empty(),
        "BOOL_FLAGS 漏了这些被 args.flag() 读取的开关: {:?}\n\
         要么加进 ui-cli/src/main.rs 的 BOOL_FLAGS，要么在 tests/bool_flags.rs 的 \
         NOT_BOOL_FLAGS 里写明为什么它是键值选项",
        missing
    );
}

#[test]
fn exclusion_table_stays_honest() {
    let declared = bool_flags();
    let referenced = referenced_flags();
    for (name, why) in NOT_BOOL_FLAGS {
        assert!(why.len() >= 10, "例外 {name} 要写清原因");
        assert!(
            referenced.iter().any(|f| f.as_str() == *name),
            "例外 {name} 已经没人 args.flag() 读了，删掉这行"
        );
        assert!(
            !declared.iter().any(|f| f.as_str() == *name),
            "{name} 在例外表里，却又列进了 BOOL_FLAGS：二选一"
        );
    }
}

#[test]
fn p2_review_flags_are_covered() {
    // 二轮评审点名的 0.3.0 新开关：6 个纯布尔 + 1 个双语义（checkpoint 走例外表）。
    let declared = bool_flags();
    for name in [
        "gc",
        "incremental",
        "no-auto-checkpoint",
        "pending",
        "prune",
        "reconcile",
    ] {
        assert!(
            declared.iter().any(|f| f == name),
            "BOOL_FLAGS 漏了 --{name}"
        );
    }
    assert!(
        NOT_BOOL_FLAGS.iter().any(|(name, _)| *name == "checkpoint"),
        "checkpoint 要么进 BOOL_FLAGS，要么留在例外表里并说明双语义"
    );
}

fn workspace_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ui-bool-flags-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn ui(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ui"))
        .args(args)
        .output()
        .expect("执行 ui 二进制")
}

fn ui_ok(args: &[&str]) -> String {
    let out = ui(args);
    assert!(
        out.status.success(),
        "ui {:?} 失败: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// `--checkpoint` 的双语义回归：`ui diff --checkpoint <旧基线>` 必须真的拿旧基线，
/// 而不是被布尔开关解析吞掉后默默 diff 最新基线。
///
/// 这条用例是 `checkpoint` 留在 NOT_BOOL_FLAGS 的证据：把它列进 BOOL_FLAGS 会红。
#[test]
fn checkpoint_value_option_still_selects_older_baseline() {
    let ws = workspace_dir("checkpoint-value");
    let ws_s = ws.to_str().unwrap();
    fs::write(ws.join("a.txt"), "one\n").unwrap();
    ui_ok(&["checkpoint", "--workspace", ws_s, "--label", "first"]);

    fs::write(ws.join("a.txt"), "two\n").unwrap();
    ui_ok(&["checkpoint", "--workspace", ws_s, "--label", "second"]);

    let listed = ui_ok(&["checkpoints", "--workspace", ws_s, "--json"]);
    let rows = ui_core::json::parse(listed.trim()).expect("checkpoints --json");
    let first_id = rows
        .as_arr()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row.str_at("label") == Some("first"))
        })
        .and_then(|row| row.str_at("id"))
        .expect("找不到 label=first 的 checkpoint")
        .to_string();

    let diff = ui_ok(&[
        "diff",
        "--workspace",
        ws_s,
        "--checkpoint",
        &first_id,
        "--json",
    ]);
    let report = ui_core::json::parse(diff.trim()).expect("diff --json");
    let changes = report.arr_at("changes").cloned().unwrap_or_default();
    assert!(
        changes.iter().any(|c| c.str_at("path") == Some("a.txt")),
        "`diff --checkpoint {first_id}` 没拿旧基线：--checkpoint 的值被当成位置参数吃掉了"
    );

    let _ = fs::remove_dir_all(&ws);
}
