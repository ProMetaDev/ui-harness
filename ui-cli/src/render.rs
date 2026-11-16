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
        "effect/unknown" => format!("{} · 状态未知", e.str_at("tool").unwrap_or("?")),
        "effect/rerun" => format!("{} · 重放", e.str_at("tool").unwrap_or("?")),
        "journal/torn" => "崩溃留下的半条记录已截断".to_string(),
        "compact" => format!(
            "折叠 {} 条：{} → {} tok",
            e.u64_at("dropped").unwrap_or(0),
            e.u64_at("beforeTokens").unwrap_or(0),
            e.u64_at("afterTokens").unwrap_or(0)
        ),
        "approval/answered" => format!(
            "{} · {}",
            e.str_at("tool").unwrap_or("?"),
            e.str_at("verdict").unwrap_or("?")
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
}
