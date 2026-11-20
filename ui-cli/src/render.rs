//! Pure rendering: takes state, returns lines. The terminal driver in `main`
//! only moves bytes around, so what the user sees is testable without a tty.

use ui_core::budget::Limits;
use ui_core::json::Json;
use ui_core::time::{human_duration, iso8601};

pub fn bar(used: f64, limit: f64, width: usize) -> String {
    if limit <= 0.0 {
        return "—".repeat(width);
    }
    let ratio = (used / limit).clamp(0.0, 1.0);
    let filled = (ratio * width as f64).round() as usize;
    let mut s = String::with_capacity(width);
    for i in 0..width {
        s.push(if i < filled { '█' } else { '·' });
    }
    s
}

/// One line that answers "am I about to run out, and of what".
pub fn status_line(model: &str, budget: &Json, limits: &Limits) -> String {
    let used = budget.get("used");
    let tokens = used.and_then(|u| u.u64_at("tokens")).unwrap_or(0);
    let calls = used.and_then(|u| u.u64_at("toolCalls")).unwrap_or(0);
    let wall = used.and_then(|u| u.u64_at("wallMs")).unwrap_or(0);
    let usd = used.and_then(|u| u.num_at("usd")).unwrap_or(0.0);
    format!(
        "{} │ tok {}/{} {} │ calls {}/{} │ {} │ ${:.3}",
        model,
        tokens,
        limits.tokens,
        bar(tokens as f64, limits.tokens as f64, 8),
        calls,
        limits.tool_calls,
        human_duration(wall),
        usd
    )
}

pub fn section_rows(snapshot: &Json) -> Vec<String> {
    let mut rows = Vec::new();
    for section in snapshot.arr_at("sections").into_iter().flatten() {
        rows.push(format!(
            "  {:<8} {:<8} {:>7} tok {:>8} B  {}",
            section.str_at("id").unwrap_or("?"),
            section.str_at("owner").unwrap_or("?"),
            section.u64_at("tokens").unwrap_or(0),
            section.u64_at("bytes").unwrap_or(0),
            section.str_at("volatility").unwrap_or("-")
        ));
    }
    rows
}

/// 事件里的字符串数组压成一行；超过 6 项就省略，别让一行日志撑满整个终端。
fn listed(v: Option<&Vec<Json>>) -> String {
    let items: Vec<&str> = match v {
        Some(a) => a.iter().filter_map(Json::as_str).collect(),
        None => return "—".to_string(),
    };
    if items.is_empty() {
        return "无".to_string();
    }
    if items.len() <= 6 {
        return items.join("、");
    }
    format!("{}…（共 {} 项）", items[..6].join("、"), items.len())
}

pub fn event_row(e: &Json) -> String {
    let kind = e.str_at("kind").unwrap_or("?");
    let detail = match kind {
        "turn/start" => format!(
            "turn {} · {}",
            e.u64_at("turn").unwrap_or(0),
            e.str_at("prompt").unwrap_or("")
        ),
        "turn/end" => format!(
            "turn {} · {} · {} 步 · {} 次工具",
            e.u64_at("turn").unwrap_or(0),
            e.str_at("reason").unwrap_or("?"),
            e.u64_at("steps").unwrap_or(0),
            e.u64_at("toolCalls").unwrap_or(0)
        ),
        "prompt/snapshot" => format!(
            "{} · {} tok · {} 段",
            e.str_at("snapshotId").unwrap_or("?"),
            e.u64_at("tokens").unwrap_or(0),
            e.arr_at("sections").map(|s| s.len()).unwrap_or(0)
        ),
        "model/response" => {
            let calls = e.arr_at("toolCalls").map(|c| c.len()).unwrap_or(0);
            format!("{} · {} 个工具调用", e.str_at("model").unwrap_or("?"), calls)
        }
        "model/repair" => format!(
            "第 {} 次 · {} · {}",
            e.u64_at("attempt").unwrap_or(0),
            e.str_at("tool").unwrap_or("?"),
            e.str_at("error").unwrap_or("?")
        ),
        "tool/request" => format!(
            "{} · {} · {}",
            e.str_at("tool").unwrap_or("?"),
            e.str_at("decision").unwrap_or("?"),
            e.str_at("reason").unwrap_or("")
        ),
        "tool/start" => format!(
            "{} · {}",
            e.str_at("tool").unwrap_or("?"),
            if e.bool_at("idempotent") == Some(true) {
                "可重放"
            } else {
                "有副作用"
            }
        ),
        "tool/end" => format!(
            "{} · {}",
            e.str_at("tool").unwrap_or("?"),
            if e.bool_at("ok") == Some(true) { "ok" } else { "失败" }
        ),
        // 模型实际看到的工具输出（已按预算截断、落账前脱敏）。不印出来的话，"这一步到底喂了
        // 多少字、有没有被落盘"在账本里就是一行空白——而那正是排查上下文税的第一问。
        "tool/output" => {
            let text = e.str_at("text").unwrap_or("");
            let chars = text.chars().count();
            // "落了盘"这件事的判据是内核自己写下的那句指针（`retain_tool_output`
            // 只在成功落盘后才拼得出"完整内容在"），不是"正文里提到了那个目录"：
            // 一句「原文已按保留策略清理，不在了: .ui/tool-output/…」也会提到它，
            // 而把失败说明标成"全量落盘"，看账的人就会去追一个已经不存在的指针。
            let spilled = text.contains("完整内容在");
            // 来源命中隐私清单的那一档：日志里根本没有正文，字数与指纹在字段上
            let body = match e.str_at("withheld") {
                Some(declared) => format!(
                    "模型看到 {} 字（来源命中隐私清单 {}，正文未落账；全文在 {}）",
                    e.u64_at("chars").unwrap_or(0),
                    declared,
                    e.str_at("file").unwrap_or("?")
                ),
                None => format!(
                    "模型看到 {} 字{}",
                    chars,
                    if spilled { "（已截断，全量落盘）" } else { "" }
                ),
            };
            let redacted = match e.u64_at("redacted") {
                Some(n) if n > 0 => format!(" · 落账前抹掉 {} 处", n),
                _ => String::new(),
            };
            format!("{} · {}{}", e.str_at("tool").unwrap_or("?"), body, redacted)
        }
        "effect/unknown" => format!("{} · 状态未知", e.str_at("tool").unwrap_or("?")),
        "effect/rerun" => format!("{} · 重放", e.str_at("tool").unwrap_or("?")),
        // 观测式效果：信任边界通道执行后「实际改了什么」。
        // 这是审计里最值钱的一行——声明式效果只说了"要执行这条命令"。
        "effect/observed" => {
            let ch = e.get("changes").cloned().unwrap_or(Json::Null);
            let added = ch.arr_at("added").map(|a| a.len()).unwrap_or(0);
            let changed = ch.arr_at("changed").map(|a| a.len()).unwrap_or(0);
            let removed = ch.arr_at("removed").map(|a| a.len()).unwrap_or(0);
            let sample = ch
                .arr_at("added")
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(|s| format!(" · 例 {}", s))
                .unwrap_or_default();
            format!(
                "{} · 实际改动 +{} ~{} -{}{}",
                e.str_at("tool").unwrap_or("?"),
                added,
                changed,
                removed,
                sample
            )
        }
        "journal/torn" => "崩溃留下的半条记录已截断".to_string(),
        "compact" => format!(
            "折叠 {} 条：{} → {} tok",
            e.u64_at("dropped").unwrap_or(0),
            e.u64_at("beforeTokens").unwrap_or(0),
            e.u64_at("afterTokens").unwrap_or(0)
        ),
        // 可审计的知识（K2）：这一行就是「这次为什么拿到这段上下文」的证据。
        "knowledge/use" => format!(
            "卡片 {} · {} · {} tok{}",
            e.str_at("card").unwrap_or("?"),
            e.str_at("reason").unwrap_or(""),
            e.u64_at("budget_tokens").unwrap_or(0),
            e.num_at("score")
                .map(|s| format!(" · score {:.2}", s))
                .unwrap_or_default()
        ),
        "knowledge/emit" => format!(
            "卡片 {} · sha {} · 生成于 seq {} · 确认位 {}",
            e.str_at("card").unwrap_or("?"),
            e.str_at("source_sha").unwrap_or("—"),
            e.u64_at("generated_by_seq").unwrap_or(0),
            e.str_at("confirmed_by").unwrap_or("none")
        ),
        // 记忆：这一行就是「模型为什么知道这条」的证据（照 knowledge/use 的写法）。
        "memory/used" => format!(
            "记忆 {} · {} · {} tok",
            e.str_at("id").unwrap_or("?"),
            e.str_at("reason").unwrap_or(""),
            e.u64_at("tokens").unwrap_or(0)
        ),
        // 批准：光有 verdict 说不清"放行的是哪一次调用、开了多大的口子"。
        // approvalId + scope 都在，才是可以对账的批准记录。
        "approval/requested" => format!(
            "{} · {} · 效果 {}",
            e.str_at("approvalId").unwrap_or("?"),
            e.str_at("tool").unwrap_or("?"),
            e.arr_at("effects")
                .map(|a| a
                    .iter()
                    .filter_map(|x| x.str_at("kind"))
                    .collect::<Vec<_>>()
                    .join("+"))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "—".to_string())
        ),
        "approval/answered" => format!(
            "{} · {} · {}{}",
            e.str_at("approvalId").unwrap_or("?"),
            e.str_at("tool").unwrap_or("?"),
            e.str_at("verdict").unwrap_or("?"),
            match e.str_at("scope") {
                Some("session") => " · 本会话一直放行",
                Some("once") => " · 只放这一次",
                _ => "",
            }
        ) + match e.str_at("via") {
            // 决定不是人做的，就得写明是谁替的——否则"超时"在读账本时和"否决"长得一样。
            Some("timeout") => "（300 秒没人答，按拒绝）",
            Some("cancelled") => "（被停止打断，按拒绝）",
            Some("stale") => "（审批卡已失效，按拒绝）",
            Some("no-channel") => "（没有审批通道，按拒绝）",
            Some("unknown-word") => "（批准词不认识，按拒绝）",
            Some("auto") => "（--yes 自动批准）",
            Some("no-input") => "（读不到输入，按拒绝）",
            _ => "",
        },
        // 排队期间被取消：这一轮没开始，但它得看得见——
        // 否则日志里只剩"没做"，分不清是取消了、丢了、还是压根没提交。
        "turn/skipped" => format!(
            "任务 {} · 排队时被取消，未开始{}",
            e.str_at("task").unwrap_or("?"),
            e.str_at("text")
                .map(|t| format!(" · {}", t))
                .unwrap_or_default()
        ),
        // 回写事务：人对工作区动的手，每一步都要能重放出来。
        "apply/begin" => format!(
            "事务 {} · 准备写回 {} 个：{}",
            e.str_at("txn").unwrap_or("?"),
            e.arr_at("files").map(|a| a.len()).unwrap_or(0),
            listed(e.arr_at("files"))
        ),
        "apply/committed" => format!(
            "事务 {} · 已写回 {} 个：{} · 留底 {}",
            e.str_at("txn").unwrap_or("?"),
            e.arr_at("written").map(|a| a.len()).unwrap_or(0),
            listed(e.arr_at("written")),
            e.str_at("backups").unwrap_or("—")
        ),
        "apply/rolled-back" => format!(
            "事务 {} · {} · 还原 {} 个{}{}",
            e.str_at("txn").unwrap_or("?"),
            if e.str_at("stage") == Some("rollback") {
                "人手动退回"
            } else {
                "写回失败自动退回"
            },
            e.arr_at("restored").map(|a| a.len()).unwrap_or(0),
            e.arr_at("skipped")
                .filter(|a| !a.is_empty())
                .map(|a| format!(" · 保留（之后又改过）{}", listed(Some(a))))
                .unwrap_or_default(),
            e.str_at("error")
                .map(|s| format!(" · 原因 {}", s))
                .unwrap_or_default()
        ),
        "apply/abandoned" => format!(
            "事务 {} · 被 --force 放弃（半途那笔不再回滚）",
            e.str_at("txn").unwrap_or("?")
        ),
        _ => String::new(),
    };
    let clock = iso8601(e.u64_at("ts").unwrap_or(0));
    format!(
        "{:>4} {:<8} {:<18} {}",
        e.u64_at("seq").unwrap_or(0),
        &clock[11..19],
        kind,
        detail
    )
}

pub fn approval_prompt(tool: &str, effects: &[String], args: &Json) -> String {
    let mut out = String::new();
    out.push_str(&format!("\n┌─ 需要批准 ─ {} ─────────────\n", tool));
    out.push_str("│ 效果:\n");
    for e in effects {
        out.push_str(&format!("│   {}\n", e));
    }
    out.push_str("│ 参数:\n");
    let mut arg_lines = args
        .to_pretty()
        .lines()
        .take(12)
        .collect::<Vec<_>>()
        .join("\n│   ");
    if arg_lines.is_empty() {
        arg_lines = "（无）".to_string();
    }
    out.push_str(&format!("│   {}\n", arg_lines));
    out.push_str("│ [y] 本次允许   [a] 本会话总是允许   [n] 拒绝\n└─ ");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_line_shows_progress_against_limits() {
        let budget = Json::obj(vec![(
            "used",
            Json::obj(vec![
                ("tokens", Json::from(1000u64)),
                ("toolCalls", Json::from(3u64)),
                ("wallMs", Json::from(65_000u64)),
                ("usd", Json::n(0.25)),
            ]),
        )]);
        let line = status_line("scripted", &budget, &Limits::default());
        assert!(line.contains("1000/200000"));
        assert!(line.contains("3/60"));
        assert!(line.contains("1m05s"));
    }

    #[test]
    fn bar_fills_proportionally() {
        assert_eq!(bar(0.0, 100.0, 4), "····");
        assert_eq!(bar(50.0, 100.0, 4), "██··");
        assert_eq!(bar(100.0, 100.0, 4), "████");
        assert_eq!(bar(500.0, 100.0, 4), "████");
    }

    #[test]
    fn event_row_marks_side_effecting_starts() {
        let write = Json::obj(vec![
            ("seq", Json::from(5u64)),
            ("ts", Json::from(0u64)),
            ("kind", Json::from("tool/start")),
            ("tool", Json::from("fs_write")),
            ("idempotent", Json::Bool(false)),
        ]);
        let read = Json::obj(vec![
            ("seq", Json::from(6u64)),
            ("ts", Json::from(0u64)),
            ("kind", Json::from("tool/start")),
            ("tool", Json::from("fs_read")),
            ("idempotent", Json::Bool(true)),
        ]);
        assert!(event_row(&write).contains("有副作用"));
        assert!(event_row(&read).contains("可重放"));
    }

    #[test]
    fn event_row_names_the_used_card_and_why() {
        let used = Json::obj(vec![
            ("seq", Json::from(7u64)),
            ("ts", Json::from(0u64)),
            ("kind", Json::from("knowledge/use")),
            ("card", Json::from("mod-payment")),
            ("reason", Json::from("path-match:src/pay/x.rs")),
            ("budget_tokens", Json::from(1200u64)),
        ]);
        let row = event_row(&used);
        assert!(row.contains("mod-payment"), "{}", row);
        assert!(row.contains("path-match:src/pay/x.rs"), "{}", row);
        assert!(row.contains("1200 tok"), "{}", row);
    }

    /// 只有 verdict 的批准记录不能对账：不知道放的是哪一次调用、口子开多大。
    #[test]
    fn approval_rows_show_the_identity_and_the_scope() {
        let asked = Json::obj(vec![
            ("seq", Json::from(9u64)),
            ("ts", Json::from(0u64)),
            ("kind", Json::from("approval/requested")),
            ("approvalId", Json::from("ap-1")),
            ("callId", Json::from("c-7-f4082d39-1")),
            ("tool", Json::from("shell")),
            (
                "effects",
                Json::Arr(vec![Json::obj(vec![("kind", Json::from("net.out"))])]),
            ),
        ]);
        let row = event_row(&asked);
        assert!(row.contains("ap-1"), "{}", row);
        assert!(row.contains("shell"), "{}", row);
        assert!(row.contains("net.out"), "{}", row);

        let once = Json::obj(vec![
            ("seq", Json::from(10u64)),
            ("ts", Json::from(0u64)),
            ("kind", Json::from("approval/answered")),
            ("approvalId", Json::from("ap-1")),
            ("tool", Json::from("shell")),
            ("verdict", Json::from("allow")),
            ("scope", Json::from("once")),
        ]);
        let mut session = once.clone();
        session.set("scope", Json::from("session"));
        assert!(event_row(&once).contains("只放这一次"), "{}", event_row(&once));
        let always = event_row(&session);
        assert!(always.contains("本会话一直放行"), "{}", always);
        assert!(always.contains("ap-1"), "{}", always);

        // 代答的几种来源：都按拒绝落账，但每一行要说清"是谁替人做的决定"。
        // 否则超时、被停止打断、没有通道，在读账本时和"人说了不"一模一样。
        for via in [
            "timeout",
            "cancelled",
            "stale",
            "no-channel",
            "unknown-word",
            "auto",
            "no-input",
        ] {
            let mut row_src = once.clone();
            row_src.set("verdict", Json::from("deny"));
            row_src.set("scope", Json::from("once"));
            row_src.set("via", Json::from(via));
            let row = event_row(&row_src);
            assert!(
                row.contains("按拒绝") || row.contains("自动批准"),
                "{via} 要写明来源: {row}"
            );
            assert!(
                !row.contains("ap-1 · shell · allow"),
                "{via} 不该被读成人批准的: {row}"
            );
        }
        let human = event_row(&once);
        assert!(
            !human.contains("按拒绝") && !human.contains("自动批准"),
            "人明确答的「只放这一次」不该带代答标记: {human}"
        );
    }

    /// 排队时被撤掉的任务、以及回写事务的每个状态，都要在 `ui replay` 里看得见。
    #[test]
    fn control_plane_events_are_not_silent_rows() {
        let skipped = Json::obj(vec![
            ("seq", Json::from(11u64)),
            ("ts", Json::from(0u64)),
            ("kind", Json::from("turn/skipped")),
            ("task", Json::from("t2")),
            ("reason", Json::from("cancelled-while-queued")),
            ("text", Json::from("把日志翻一遍")),
        ]);
        let row = event_row(&skipped);
        assert!(row.contains("t2") && row.contains("把日志翻一遍"), "{}", row);

        let files = Json::Arr(vec![Json::from("a.txt"), Json::from("b.txt")]);
        let begin = Json::obj(vec![
            ("seq", Json::from(1u64)),
            ("ts", Json::from(0u64)),
            ("kind", Json::from("apply/begin")),
            ("txn", Json::from("00000004-f4082d39")),
            ("files", files.clone()),
        ]);
        assert!(event_row(&begin).contains("a.txt、b.txt"), "{}", event_row(&begin));

        let failed = Json::obj(vec![
            ("seq", Json::from(2u64)),
            ("ts", Json::from(0u64)),
            ("kind", Json::from("apply/rolled-back")),
            ("txn", Json::from("00000004-f4082d39")),
            ("stage", Json::from("write")),
            ("error", Json::from("写回 b.txt 失败：是目录")),
            ("restored", files.clone()),
            ("removed", Json::Arr(vec![])),
        ]);
        let row = event_row(&failed);
        assert!(row.contains("写回失败自动退回"), "{}", row);
        assert!(row.contains("写回 b.txt 失败：是目录"), "{}", row);

        let mut by_hand = begin.clone();
        by_hand.set("kind", Json::from("apply/rolled-back"));
        by_hand.set("stage", Json::from("rollback"));
        by_hand.set("restored", Json::Arr(vec![Json::from("a.txt")]));
        by_hand.set(
            "skipped",
            Json::Arr(vec![Json::from("b.txt · apply 之后又改过")]),
        );
        let row = event_row(&by_hand);
        assert!(row.contains("人手动退回"), "{}", row);
        assert!(row.contains("保留（之后又改过）"), "{}", row);

        // 工具输出那一行不能是空白：它就是"这一步喂给模型多少字"的答案。
        let out = Json::obj(vec![
            ("seq", Json::from(3u64)),
            ("ts", Json::from(0u64)),
            ("kind", Json::from("tool/output")),
            ("callId", Json::from("w1-2-1")),
            ("tool", Json::from("fs_read")),
            ("ok", Json::Bool(true)),
            ("text", Json::from("第一行内容\n第二行内容")),
        ]);
        let row = event_row(&out);
        assert!(row.contains("fs_read"), "{}", row);
        assert!(row.contains("11 字"), "{}", row);
        assert!(!row.contains("落盘"), "没截断就不该说截断: {}", row);
        let big = Json::obj(vec![
            ("seq", Json::from(4u64)),
            ("ts", Json::from(0u64)),
            ("kind", Json::from("tool/output")),
            ("tool", Json::from("shell")),
            ("ok", Json::Bool(true)),
            (
                "text",
                Json::from("开头…（中间 40 万字符已移出上下文）…结尾\n完整内容在 .ui/tool-output/1-x.txt"),
            ),
        ]);
        assert!(
            event_row(&big).contains("已截断，全量落盘"),
            "{}",
            event_row(&big)
        );
        // 反面：一条**提到**那个目录的失败说明，不是"按需展开成功"
        let gone = Json::obj(vec![
            ("seq", Json::from(5u64)),
            ("ts", Json::from(0u64)),
            ("kind", Json::from("tool/output")),
            ("tool", Json::from("fs_read")),
            ("ok", Json::Bool(false)),
            (
                "text",
                Json::from(
                    "落盘原文已按保留策略清理，不在了: .ui/tool-output/0001-deadbeef-8-shell.txt"
                ),
            ),
        ]);
        assert!(
            !event_row(&gone).contains("全量落盘"),
            "提到路径不等于落了盘：{}",
            event_row(&gone)
        );
        assert!(event_row(&gone).contains("模型看到"), "{}", event_row(&gone));
    }

    /// 上千个文件的回写不能把一行撑成整屏。
    #[test]
    fn long_file_lists_get_folded() {
        let many = Json::Arr((0..40).map(|i| Json::from(format!("f{}.rs", i))).collect());
        let e = Json::obj(vec![
            ("seq", Json::from(1u64)),
            ("ts", Json::from(0u64)),
            ("kind", Json::from("apply/committed")),
            ("txn", Json::from("t")),
            ("written", many),
        ]);
        let row = event_row(&e);
        assert!(row.contains("共 40 项"), "{}", row);
        assert!(row.matches('、').count() <= 6, "{}", row);
    }
}
