//! 截断的工具参数必须在端到端路径上失败并触发 `model/repair`，
//! 绝不能被补全成“合法 JSON”后执行。
//!
//! 回归目标：模型输出被截断（长度上限 / 网络中断）时，旧的
//! `parse_arguments` 会补引号、补 `}`、甚至从尾部逐字删到能解析，
//! 于是 `fs_write` 把半截内容写进工作区。这里用 scripted mock 复现
//! 一次真实的内核循环，只断言可观察行为（文件内容 + 日志事件顺序）。

use std::fs;
use std::path::PathBuf;

use ui_core::budget::Limits;
use ui_core::journal::Journal;
use ui_core::json::Json;
use ui_core::model::{Model, ScriptedModel};
use ui_core::{Agent, NullHooks};

/// 模型在流被切断时实际给出的参数：content 字符串没闭合，对象也没闭合。
const TRUNCATED: &str = r#"{"path":"truncated.txt","content":"fn main() {"#;
/// 只有修复重写后才会拿到的完整参数。
const COMPLETE: &str = "fn main() {\n}\n";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ui-strict-args-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn scripted(scenario: &Json) -> Box<dyn Model + Send> {
    Box::new(ScriptedModel::from_json(scenario).unwrap())
}

#[test]
fn truncated_fs_write_arguments_are_repaired_not_written() {
    let ws = scratch("write");

    // 第 1 步：截断的 fs_write。第 2 步只在旧行为（不触发 repair）下才会被消费，
    // 用来让旧代码走到 done，从而使失败落在“文件内容”断言上而不是场景耗尽上。
    let scenario = Json::obj(vec![
        ("model", Json::from("scripted")),
        (
            "steps",
            Json::Arr(vec![
                Json::obj(vec![
                    ("content", Json::from("先写文件")),
                    (
                        "toolCalls",
                        Json::Arr(vec![Json::obj(vec![
                            ("name", Json::from("fs_write")),
                            ("argsRaw", Json::from(TRUNCATED)),
                        ])]),
                    ),
                ]),
                Json::obj(vec![("content", Json::from("完成"))]),
            ]),
        ),
        // 内核要求重写时，模型才给出完整参数。
        (
            "repairs",
            Json::Arr(vec![Json::obj(vec![
                ("content", Json::from("重新给出完整参数")),
                (
                    "toolCalls",
                    Json::Arr(vec![Json::obj(vec![
                        ("name", Json::from("fs_write")),
                        (
                            "args",
                            Json::obj(vec![
                                ("path", Json::from("truncated.txt")),
                                ("content", Json::from(COMPLETE)),
                            ]),
                        ),
                    ])]),
                ),
            ])]),
        ),
    ]);

    let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
    let mut agent = Agent::new(journal, scripted(&scenario), ws.clone(), Limits::default());
    let report = agent.run_turn("写个文件", &mut NullHooks).unwrap();
    assert_eq!(report.reason, "done");

    // ① 目标文件里只能是 repair 之后重写的完整内容——截断的前缀绝不能落盘。
    let written = fs::read_to_string(ws.join("truncated.txt")).expect("repair 后应写入完整内容");
    assert_eq!(written, COMPLETE, "文件里只能是完整内容，实际：{:?}", written);

    // ② 日志必须留下 repair 事件，并且发生在任何一次真正的工具执行之前。
    let events = &agent.journal.events;
    let repair_idx = events
        .iter()
        .position(|e| e.str_at("kind") == Some("model/repair"))
        .expect("截断参数必须产生 model/repair 事件");
    let repair = &events[repair_idx];
    assert_eq!(repair.str_at("tool"), Some("fs_write"));
    assert_eq!(
        repair.str_at("raw"),
        Some(TRUNCATED),
        "repair 事件必须保留模型原始（截断）输出"
    );
    assert!(
        repair
            .str_at("error")
            .map(|e| !e.is_empty())
            .unwrap_or(false),
        "repair 事件必须写明解析失败原因"
    );

    let first_start = events
        .iter()
        .position(|e| e.str_at("kind") == Some("tool/start"))
        .expect("repair 之后应有且仅有一次真正的写入");
    assert!(
        repair_idx < first_start,
        "repair 必须发生在第一次真正执行之前（repair 在 {}，tool/start 在 {}）",
        repair_idx,
        first_start
    );
    let starts = events
        .iter()
        .filter(|e| e.str_at("kind") == Some("tool/start"))
        .count();
    assert_eq!(starts, 1, "截断那次不能被执行，只有 repair 后的那次能写");
    let requests = events
        .iter()
        .filter(|e| e.str_at("kind") == Some("tool/request"))
        .count();
    assert_eq!(requests, 1, "坏参数应在进入策略/执行之前就被拦下");
}
