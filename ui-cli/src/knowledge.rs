//! `ui knowledge`：知识卡的列出 / 校验 / 留痕（K1 + K2 的命令行出口）。
//!
//! 这一层只做 I/O 与打印，算法全在 `ui_core::knowledge`：
//! 卡片解析、来源哈希、命中原因、两条事件子类型的 payload。
//!
//! 关键取舍：卡片是**文件**（`docs/knowledge/*.md`），不是数据库。
//! 所以 `git diff` 能看、PR 能评审、`sha256` 就是索引——没有第二份真相。

use std::path::Path;

use ui_core::agent::Agent;
use ui_core::journal::Journal;
use ui_core::knowledge;

use crate::{append_log, resolve_journal, workspace, Args};

/// 一次任务最多预取几张卡（多了会挤掉任务本身，而且命中集合本身要可读）。
pub const MAX_PREFETCH: usize = 3;

pub fn cmd(args: &Args) -> Result<i32, String> {
    if args.flag("help") || args.sub(0).is_none() {
        println!("{}", help());
        return Ok(0);
    }
    let ws = workspace(args);
    match args.sub(0) {
        Some("list") => list(&ws),
        Some("check") => check(&ws, args),
        Some("use") => use_card(&ws, args),
        Some("emit") => emit(&ws, args),
        Some(other) => Err(format!(
            "未知子命令 `knowledge {}`（list | check | use | emit）",
            other
        )),
        None => Ok(0),
    }
}

fn help() -> String {
    "ui knowledge — 可审计的知识（docs/knowledge/*.md）\n\n\
     ui knowledge list                         列出卡片（id / 标题 / 确认位 / 来源 sha / touches）\n\
     ui knowledge check                        校验 source_sha 与 touches 命中文件的哈希，不一致打印 STALE\n\
     ui knowledge check --strict               UNBOUND（source_sha 为空）也算失败\n\
     ui knowledge use <id> --reason \"...\"      写一条 knowledge/use（手工验证证据链）\n\
     ui knowledge use <id> --reason \"...\" --score 0.8   带相似度分数（默认空：确定性命中）\n\
     ui knowledge emit <id> [--confirmed-by human|none]  写一条 knowledge/emit（来源绑定）\n\n\
     退出码：0 = 一致；2 = 有 STALE（知识过期）。\n\
     来源 sha 的算法：对 touches 命中的每个文件（按路径排序）算 sha256，\n\
     再把「相对路径 + 文件哈希」逐行喂进 sha256 —— ui_core::knowledge 里有唯一实现。"
        .to_string()
}

fn list(ws: &Path) -> Result<i32, String> {
    let cards = knowledge::discover(ws);
    if cards.is_empty() {
        println!(
            "知识卡 0 张（{} 下没有带 front-matter 的 .md）",
            knowledge::DIR
        );
        return Ok(0);
    }
    println!("知识卡 {}（来源绑定：K1）", knowledge::DIR);
    for card in &cards {
        println!(
            "  {:<18} {:<22} confirmed_by={:<5} sha={:<10} touches={}",
            card.id,
            card.title,
            card.confirmed_by,
            short(&card.source_sha),
            if card.touches.is_empty() {
                "（无）".to_string()
            } else {
                card.touches.join(",")
            }
        );
    }
    println!("共 {} 张", cards.len());
    Ok(0)
}

fn check(ws: &Path, args: &Args) -> Result<i32, String> {
    let strict = args.flag("strict");
    let cards = knowledge::discover(ws);
    println!(
        "知识卡校验 {}（source_sha vs touches 命中文件的哈希）",
        knowledge::DIR
    );
    if cards.is_empty() {
        println!("没有卡片可校验");
        return Ok(0);
    }

    let (mut ok, mut stale, mut unbound) = (0usize, 0usize, 0usize);
    for card in &cards {
        match knowledge::check(ws, card) {
            knowledge::Status::Ok { files } => {
                ok += 1;
                println!(
                    "  ok       {:<18} {} 个文件 · sha={}",
                    card.id,
                    files,
                    short(&card.source_sha)
                );
            }
            knowledge::Status::Stale {
                expected,
                actual,
                files,
            } => {
                stale += 1;
                let actual = match actual.as_deref() {
                    Some(a) => short(a),
                    None => "（touches 没有命中任何文件）".to_string(),
                };
                println!(
                    "  STALE    {:<18} 卡上 {} ≠ 实际 {}",
                    card.id,
                    short(&expected),
                    actual
                );
                if !files.is_empty() {
                    println!("           命中文件（{}）：{}", files.len(), sample(&files, 4));
                }
            }
            knowledge::Status::Unbound { files } => {
                unbound += 1;
                println!(
                    "  UNBOUND  {:<18} source_sha 为空（命中 {} 个文件，未校验）{}",
                    card.id,
                    files.len(),
                    if strict {
                        " · --strict 下算失败"
                    } else {
                        ""
                    }
                );
            }
        }
    }
    println!(
        "共 {} 张：{} ok · {} STALE · {} UNBOUND",
        cards.len(),
        ok,
        stale,
        unbound
    );
    if stale > 0 {
        println!("结论  FAIL（{} 张卡与源码不一致：过期知识不该当规范）", stale);
        return Ok(2);
    }
    if strict && unbound > 0 {
        println!("结论  FAIL（--strict：{} 张卡没有绑定源码）", unbound);
        return Ok(2);
    }
    println!("结论  PASS");
    Ok(0)
}

fn use_card(ws: &Path, args: &Args) -> Result<i32, String> {
    let id = args
        .sub(1)
        .ok_or("用法：ui knowledge use <id> --reason \"path-match:src/pay/x.rs\" [--score 0.8]")?
        .to_string();
    let card = knowledge::find(ws, &id)
        .ok_or_else(|| format!("没有 id 为 `{}` 的知识卡（{}/*.md）", id, knowledge::DIR))?;
    let reason = args.get("reason").unwrap_or("manual").to_string();
    let score = match args.get("score") {
        Some(v) => Some(
            v.parse::<f64>()
                .map_err(|_| format!("--score 要 0..1 的小数，收到 `{}`", v))?
                .clamp(0.0, 1.0),
        ),
        None => None,
    };

    let jp = resolve_journal(args, ws);
    let mut journal = Journal::open(&jp).map_err(|e| e.to_string())?;
    let event = journal
        .append(
            "knowledge/use",
            knowledge::use_payload(&card, &reason, score),
        )
        .map_err(|e| e.to_string())?;
    println!(
        "✓ knowledge/use seq={} card={} reason={} budget_tokens={} score={}",
        event.u64_at("seq").unwrap_or(0),
        card.id,
        reason,
        card.budget_tokens(),
        match score {
            Some(s) => format!("{:.2}", s),
            None => "—".to_string(),
        }
    );
    println!("  日志 {}", jp.display());
    Ok(0)
}

fn emit(ws: &Path, args: &Args) -> Result<i32, String> {
    let id = args
        .sub(1)
        .ok_or("用法：ui knowledge emit <id> [--confirmed-by human|none]")?
        .to_string();
    let card = knowledge::find(ws, &id)
        .ok_or_else(|| format!("没有 id 为 `{}` 的知识卡（{}/*.md）", id, knowledge::DIR))?;
    let confirmed_by = match args.get("confirmed-by") {
        Some(v) => {
            let v = v.trim();
            if v != "human" && v != "none" {
                return Err(format!("--confirmed-by 只支持 human | none，收到 `{}`", v));
            }
            v.to_string()
        }
        None => card.confirmed_by.clone(),
    };

    let jp = resolve_journal(args, ws);
    let mut journal = Journal::open(&jp).map_err(|e| e.to_string())?;
    // generated_by_seq = 这张卡对应的那次生成落在日志的哪一条：
    // 取写入 emit 之前的 seq（emit 自己那条是「记录」，不是「生成」）。
    let generated_by_seq = journal.seq();
    let event = journal
        .append(
            "knowledge/emit",
            knowledge::emit_payload(&card, generated_by_seq, &confirmed_by),
        )
        .map_err(|e| e.to_string())?;
    println!(
        "✓ knowledge/emit seq={} card={} source_sha={} generated_by_seq={} confirmed_by={} touches={}",
        event.u64_at("seq").unwrap_or(0),
        card.id,
        if card.source_sha.is_empty() {
            "—".to_string()
        } else {
            short(&card.source_sha)
        },
        generated_by_seq,
        confirmed_by,
        if card.touches.is_empty() {
            "（无）".to_string()
        } else {
            card.touches.join(",")
        }
    );
    println!("  日志 {}", jp.display());
    Ok(0)
}

/// `ui run` 的任务前确定性预取：命中 → 写 `knowledge/use` + 返回要拼在提示词前的正文；
/// 未命中 → `None`，调用方什么都不改（负样本必须是**零副作用**的）。
pub fn prefetch_for_run(
    agent: &mut Agent,
    ws: &Path,
    task: &str,
    max: usize,
) -> Result<Option<String>, String> {
    let hits = knowledge::prefetch(ws, task, max);
    if hits.is_empty() {
        return Ok(None);
    }
    for hit in &hits {
        append_log(
            agent,
            "knowledge/use",
            knowledge::use_payload(&hit.card, &hit.reason, None),
        )?;
        println!(
            "  知识卡 {} · {} · {} · {} tok",
            hit.card.id,
            hit.card.title,
            hit.reason,
            hit.card.budget_tokens()
        );
    }
    Ok(Some(knowledge::prefetch_block(&hits)))
}

fn short(sha: &str) -> String {
    let sha = sha.trim();
    if sha.is_empty() {
        return "—".to_string();
    }
    if sha.chars().count() > 12 {
        format!("{}…", sha.chars().take(12).collect::<String>())
    } else {
        sha.to_string()
    }
}

fn sample(files: &[String], max: usize) -> String {
    let mut out = files
        .iter()
        .take(max)
        .map(|f| f.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if files.len() > max {
        out.push_str(&format!(" …（共 {} 个）", files.len()));
    }
    out
}
