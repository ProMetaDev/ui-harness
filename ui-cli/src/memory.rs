//! `ui remember` / `ui forget` / `ui memory`：记忆系统的命令行出口（P1）。
//!
//! 这一层只做 I/O 与打印，算法全在 `ui_core::memory`：
//! 内容 id、校验、哈希链物化、BM25 检索、`memory/used` 的 payload。
//!
//! 关键取舍：记忆是 **append-only 事件**（`<ws>/.ui/memory.jsonl`），不是可变表。
//! `forget` 只写墓碑；被忘的正文仍留在文件里（与日志的诚实边界一致）。

use std::path::Path;

use ui_core::memory::{self, Draft, MemKind, SourceKind};
use ui_core::time;

use crate::{workspace, Args};

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

pub fn cmd_remember(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let text = args
        .get("text")
        .or_else(|| args.sub(0))
        .ok_or("用法：ui remember \"<text>\" [--kind fact|pref|pitfall|build] [--key k] [--scope workspace|project] [--expires DAYS]")?
        .to_string();
    let kind = MemKind::parse(args.get("kind").unwrap_or("fact")).ok_or_else(|| {
        format!(
            "--kind 只支持 fact | pref | pitfall | build，收到 `{}`",
            args.get("kind").unwrap_or("")
        )
    })?;
    let scope = args.get("scope").unwrap_or("workspace").to_string();
    let key = args.get("key").unwrap_or("").to_string();
    let now = time::now_ms();
    let expires_at = match args.get("expires") {
        None => memory::default_expires_at(kind, now),
        Some(value) if value.eq_ignore_ascii_case("never") => 0,
        Some(value) => {
            let days = value
                .trim()
                .parse::<u64>()
                .map_err(|_| format!("--expires 要整数天数或 never，收到 `{}`", value))?;
            // 显式 0 天 = 现在就过期（测试与“临时记一下”都用得上）。
            // 这与设计稿里「0 = 永不过期」的存储表示不冲突：存储里 0/ null 仍表示 never，
            // 这里只是把 days=0 换算成「当前时间」这个绝对时间戳。
            now.saturating_add(days.saturating_mul(DAY_MS))
        }
    };

    let draft = Draft {
        mem_kind: kind,
        scope,
        key,
        text,
        source_kind: SourceKind::Human,
        source_seq: 0,
        expires_at,
        confidence: 1.0,
    };
    let mut store = memory::Store::open(&ws);
    let outcome = store.remember(&draft)?;
    if outcome.existed {
        println!(
            "已存在 {}（seq {}，同内容不重复写；{}）",
            outcome.id,
            outcome.seq,
            memory::describe_expiry(outcome.expires_at, now)
        );
    } else {
        let normalized = draft.normalized()?;
        println!(
            "已写入 {}（seq {}，{}/{}，{}）",
            outcome.id,
            outcome.seq,
            normalized.mem_kind.name(),
            normalized.scope,
            memory::describe_expiry(outcome.expires_at, now)
        );
    }
    println!("  文件 {}", store.path().display());
    Ok(0)
}

pub fn cmd_forget(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let id = args
        .sub(0)
        .ok_or("用法：ui forget <id> --reason \"...\"")?;
    let reason = args.get("reason").unwrap_or("用户要求删除");
    let mut store = memory::Store::open(&ws);
    match store.forget(id, reason)? {
        Some(seq) => {
            println!("✓ memory/forget seq={} id={} reason={}", seq, id, reason);
            println!("  下轮起不再注入；墓碑保留，正文仍在记忆文件里（append-only 不物理擦除）");
            Ok(0)
        }
        None => {
            println!("没有 id 为 `{}` 的记忆（未写事件）", id);
            Ok(1)
        }
    }
}

pub fn cmd(args: &Args) -> Result<i32, String> {
    if args.flag("help") || args.sub(0).is_none() {
        println!("{}", help());
        return Ok(0);
    }
    let ws = workspace(args);
    match args.sub(0) {
        Some("list") => list(&ws, args),
        Some("check") => check(&ws),
        Some(other) => Err(format!(
            "未知子命令 `memory {}`（list | check）",
            other
        )),
        None => Ok(0),
    }
}

pub fn help() -> String {
    format!(
        "ui memory — 跨会话记忆（{}，append-only JSONL + SHA-256 链）\n\n\
         ui remember \"<text>\" [--kind fact|pref|pitfall|build] [--key k] [--scope workspace|project] [--expires DAYS]\n\
         写一条记忆并打印 id。--expires 默认按类型（fact 90 天 / pref 永不过期 / pitfall 365 天 / build 180 天）；\n\
         显式 --expires 0 = 立即过期，--expires never = 永不过期。\n\
         ui forget <id> --reason \"...\"   写墓碑：下轮起不再注入（未知 id 退 1）\n\
         ui memory list [--all]            列出记忆：默认只列有效（未忘、未过期）；--all 含墓碑/过期\n\
         ui memory check                   校验记忆文件哈希链 + id/内容一致；损坏退 2\n\n\
         检索：ui run 每轮开始一次 BM25（top-3 / 300 tokens，UI_MEMORY_BUDGET 可覆盖）；\n\
         命中的正文进快照的 memory 段，并为每条命中写 memory/used（id/reason/tokens）。\n\
         未命中 = 零事件、提示词逐字不变。",
        memory::REL_PATH
    )
}

fn list(ws: &Path, args: &Args) -> Result<i32, String> {
    let all = args.flag("all");
    let mut store = memory::Store::open(ws);
    store.refresh()?;
    let now = time::now_ms();
    let events = memory::load_events(ws)?.len();
    let rows: Vec<&memory::Entry> = store
        .entries()
        .iter()
        .filter(|entry| all || entry.is_live(now))
        .collect();

    println!(
        "记忆 {}（{} 条事件 · {} 字节 · {}{}）",
        memory::REL_PATH,
        events,
        memory::file_len(ws),
        store.entries().len(),
        if all { " 条（含墓碑/过期）" } else { " 条有效" }
    );
    if rows.is_empty() {
        if all {
            println!("  没有任何记忆。用 ui remember \"...\" 写一条。");
        } else {
            println!("  没有有效记忆。用 ui remember \"...\" 写一条；--all 可看墓碑/过期。");
        }
        return Ok(0);
    }
    println!(
        "  {:<16} {:<9} {:<10} {:<9} {:<22} {}",
        "id", "状态", "类型/范围", "conf", "key", "正文"
    );
    for entry in rows {
        println!(
            "  {:<16} {:<9} {:<10} conf={:.2} hits={:<4} {:<22} {}",
            entry.id,
            entry.status(now),
            format!("{}/{}", entry.mem_kind.name(), entry.scope),
            entry.confidence,
            entry.hits,
            if entry.key.is_empty() {
                "—".to_string()
            } else {
                entry.key.clone()
            },
            entry.text
        );
        if let Some(reason) = &entry.forgotten_reason {
            println!("      FORGOTTEN reason={}", reason);
        }
    }
    Ok(0)
}

fn check(ws: &Path) -> Result<i32, String> {
    match memory::check(ws) {
        Ok(None) => {
            println!(
                "记忆 未启用（{} 不存在；ui remember 会自动创建）",
                memory::REL_PATH
            );
            Ok(0)
        }
        Ok(Some(report)) => {
            println!("记忆 {}", memory::REL_PATH);
            println!(
                "  事件 {} · 生效 {} · 已忘 {} · 过期 {} · 使用 {} · 写入 {} · 墓碑 {}",
                report.events,
                report.live,
                report.forgotten,
                report.expired,
                report.used,
                report.writes,
                report.forgets
            );
            println!("  哈希链完整 · seq 连续 · id/内容一致");
            Ok(0)
        }
        Err(message) => {
            println!("FAIL  记忆校验失败：{}", message);
            println!(
                "      文件 {}",
                ws.join(memory::REL_PATH).display()
            );
            Ok(2)
        }
    }
}
