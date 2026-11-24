//! `ui` — one binary, a handful of commands, no plugin registry.
//!
//!   ui doctor                 what the harness believes about this machine
//!   ui run --task "..."       run one turn (scripted scenario or external model)
//!   ui resume                 reconcile effects interrupted by a crash
//!   ui replay                 read the journal back, step by step
//!   ui policy                 explain a decision for an effect
//!   ui preview                render one full-screen frame (no tty needed)

mod frame;
mod knowledge;
mod memory;
mod render;
mod lab;
mod providers;
mod server;
mod term;

use ui_core::agent::{resume, Agent, ApprovalRequest, ApprovalResponse, ApprovalScope, Hooks};
use ui_core::budget::Limits;
use ui_core::compact::CompactPolicy;
use ui_core::json::{self, Json};
use ui_core::journal::{analyze_incomplete, Journal};
use ui_core::model::{ExternalModel, Model, ModelResponse, ScriptedModel};
use ui_core::policy::{Decision, Effect, Policy};
use ui_core::prompt::{inspect_environment, Message, Snapshot};
use ui_core::tools::{registry, ToolResult};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::fs::File;
use std::path::{Path, PathBuf};

/// Windows 控制台默认是 GBK，直接输出 UTF-8 会变成乱码（实机看到「UI 宸插惎鍔?」）。
/// 启动时把输入输出代码页都切到 65001。
#[cfg(windows)]
fn init_console() {
    extern "system" {
        fn SetConsoleOutputCP(code_page: u32) -> i32;
        fn SetConsoleCP(code_page: u32) -> i32;
    }
    unsafe {
        SetConsoleOutputCP(65001);
        SetConsoleCP(65001);
    }
}

#[cfg(not(windows))]
fn init_console() {}

/// `ui --version` / `ui -V` / `ui version`：回答"这是哪个构建"。
///
/// 一开始我们连这个都没有——外部基准的黄金任务恰好就是"给 CLI 加 --version"。
fn print_version() {
    println!(
        "ui {} (commit {}, built {})",
        env!("CARGO_PKG_VERSION"),
        option_env!("UI_COMMIT").unwrap_or("unknown"),
        option_env!("UI_BUILD_DATE").unwrap_or("unknown")
    );
}

fn main() {
    init_console();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "--version" || a == "-V")
        || argv.first().map(|a| a == "version").unwrap_or(false)
    {
        print_version();
        return;
    }
    let code = match dispatch(&argv) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("ui: {}", message);
            1
        }
    };
    std::process::exit(code);
}

struct Args {
    values: HashMap<String, String>,
    flags: Vec<String>,
    /// 命令之后的位置参数（`ui lab pack` 里的 `pack`）
    rest: Vec<String>,
    /// 同一个键重复出现的全部值（`--private a --private b`）。
    /// `values` 只会留最后一个，隐私清单这种"逐条累加"的选项不能丢。
    repeats: Vec<(String, String)>,
}

impl Args {
    fn parse(list: &[String]) -> (Option<String>, Args) {
        // 显式布尔开关表：这些选项**不吞**后面的词。
        //
        // 以前一律「`--x` 后面跟一个非 `--` 词就当它的值」，于是
        // `ui --yes run --task hi` 把 `run` 吃成 `yes=run`，子命令整个丢失。
        // 只列真正的布尔开关；键值选项（--task/--workspace/--mock ...）语义不变。
        const BOOL_FLAGS: &[&str] = &[
            "all",
            "ansi",
            "apply",
            "dry-run",
            "force",
            "full",
            "help",
            "isolate",
            "json",
            "no-approval",
            "no-env-scrub",
            "no-isolate",
            "no-memory",
            "no-redact",
            "no-splash",
            "no-tools",
            "raw",
            "rollback",
            "strict",
            "version",
            "watch-parent",
            "yes",
        ];
        let mut values = HashMap::new();
        let mut flags = Vec::new();
        let mut repeats: Vec<(String, String)> = Vec::new();
        let mut rest: Vec<String> = Vec::new();
        let mut command: Option<String> = None;
        let mut i = 0;
        while i < list.len() {
            let item = &list[i];
            if let Some(key) = item.strip_prefix("--") {
                if BOOL_FLAGS.contains(&key) {
                    // 布尔开关：永远不吃下一个词，子命令/位置参数原样留在原地
                    flags.push(key.to_string());
                    i += 1;
                } else if i + 1 < list.len() && !list[i + 1].starts_with("--") {
                    values.insert(key.to_string(), list[i + 1].clone());
                    repeats.push((key.to_string(), list[i + 1].clone()));
                    i += 2;
                } else {
                    flags.push(key.to_string());
                    i += 1;
                }
            } else {
                if command.is_none() {
                    command = Some(item.clone());
                } else {
                    rest.push(item.clone());
                }
                i += 1;
            }
        }
        (
            command,
            Args {
                values,
                flags,
                rest,
                repeats,
            },
        )
    }

    fn sub(&self, index: usize) -> Option<&str> {
        self.rest.get(index).map(String::as_str)
    }

    /// 内部转发用的空参数表（`ui team` 造好 plan 后转交 `ui workflow`）。
    /// 走构造函数而不是字面量：加字段时只有一处要改。
    fn empty() -> Args {
        Args {
            values: HashMap::new(),
            flags: Vec::new(),
            rest: Vec::new(),
            repeats: Vec::new(),
        }
    }

    /// 某个键的全部取值（按出现顺序）。重复声明的选项走这里，不走 `get`。
    fn many(&self, key: &str) -> Vec<String> {
        self.repeats
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    fn flag(&self, key: &str) -> bool {
        self.flags.iter().any(|f| f == key)
    }
}

fn workspace(args: &Args) -> PathBuf {
    let p = args
        .get("workspace")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    // 规范成绝对路径：否则 prompt 里显示 "demo"，模型会把 "demo/greet.js" 也带上，
    // 于是路径变成 demo/demo/greet.js（真机跑出来的 bug）。
    let p = std::fs::canonicalize(&p).unwrap_or(p);
    strip_verbatim(p)
}

/// Windows 的 canonicalize 会加 `\\?\` 前缀：功能上没问题，但日志和界面里很难看。
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p
    }
}

fn journal_path(ws: &Path) -> PathBuf {
    ws.join(".ui").join("journal.jsonl")
}

/// 会话目录：`<ws>/.ui/sessions/<名字>.jsonl`
fn sessions_dir(ws: &Path) -> PathBuf {
    ws.join(".ui").join("sessions")
}

/// 会话名只允许安全字符：会话名会变成文件名，不能让它跑出目录。
fn valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn session_path(ws: &Path, name: &str) -> PathBuf {
    // `default` 保持老路径，历史日志继续可读
    if name == "default" {
        journal_path(ws)
    } else {
        sessions_dir(ws).join(format!("{}.jsonl", name))
    }
}

/// `--journal` 可覆盖日志路径（并行子代理各写各的）；
/// `--session NAME` 选一个会话；都没给就是 default。
fn resolve_journal(args: &Args, ws: &Path) -> PathBuf {
    match args.get("journal") {
        Some(p) => {
            let p = PathBuf::from(p);
            if p.is_absolute() {
                p
            } else {
                ws.join(p)
            }
        }
        None => match args.get("session") {
            Some(name) if valid_session_name(name) => session_path(ws, name),
            // 名字非法时退回 default，而不是拼出一个奇怪的路径
            _ => journal_path(ws),
        },
    }
}

/// 交互/单次 run 打开已有会话时恢复历史。
///
/// 之前只有 `ui serve` 的 `switch_journal` 会重建历史，`ui run --session demo`
/// 却是空历史继续往同一个 journal 追加：模型看不到上一轮，日志却把它记成
/// 同一会话的下一轮。这里复用同一条重建路径。
fn restore_session_history(agent: &mut Agent, jp: &Path) -> Result<(), String> {
    let nonempty = Journal::read(jp)
        .map(|(events, _)| !events.is_empty())
        .unwrap_or(false);
    if nonempty {
        agent.switch_journal(jp).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// `ui sessions`：列出会话（名字、事件数、更新时间）；`--new NAME` 新建。
fn cmd_sessions(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let dir = sessions_dir(&ws);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    if let Some(name) = args.get("new") {
        if !valid_session_name(name) {
            return Err("会话名只能用字母、数字、- 和 _（最长 64）".to_string());
        }
        let path = session_path(&ws, name);
        if path.exists() {
            return Err(format!("会话 `{}` 已经存在", name));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        println!("已新建会话 {}", name);
        println!("用法：ui run --session {} ...", name);
        return Ok(0);
    }

    let mut rows: Vec<(String, u64, u64, u64)> = Vec::new(); // name, events, bytes, mtime
    for e in std::fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten() {
        let p = e.path();
        if p.extension().map(|x| x != "jsonl").unwrap_or(true) {
            continue;
        }
        let name = p.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let events = std::fs::read_to_string(&p)
            .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count() as u64)
            .unwrap_or(0);
        let bytes = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        let mtime = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        rows.push((name, events, bytes, mtime));
    }
    // default 永远在列表里（老日志也用得上）
    let dpath = journal_path(&ws);
    if dpath.exists() {
        let events = std::fs::read_to_string(&dpath)
            .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count() as u64)
            .unwrap_or(0);
        let bytes = std::fs::metadata(&dpath).map(|m| m.len()).unwrap_or(0);
        let mtime = std::fs::metadata(&dpath)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        rows.push(("default".to_string(), events, bytes, mtime));
    }
    rows.sort_by(|a, b| b.3.cmp(&a.3));

    if rows.is_empty() {
        println!("还没有会话。新建一个：ui sessions --new work");
        return Ok(0);
    }
    println!("{:<20} {:>8} {:>10}  最后活动", "会话", "事件", "字节");
    for (name, events, bytes, mtime) in rows {
        let age = if mtime == 0 {
            "-".to_string()
        } else {
            let secs = (ui_core::time::now_ms().saturating_sub(mtime)) / 1000;
            if secs < 60 {
                format!("{} 秒前", secs)
            } else if secs < 3600 {
                format!("{} 分钟前", secs / 60)
            } else if secs < 86400 {
                format!("{} 小时前", secs / 3600)
            } else {
                format!("{} 天前", secs / 86400)
            }
        };
        println!("{:<20} {:>8} {:>10}  {}", name, events, bytes, age);
    }
    Ok(0)
}

fn cmd_index(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    if let Some(q) = args.get("query") {
        let hits = ui_core::index::search(&ws, q, 20)?;
        if args.flag("json") {
            println!("{}", Json::Arr(hits).to_compact());
        } else if hits.is_empty() {
            println!("没有匹配（先运行 ui index 建索引）");
        } else {
            for h in hits {
                println!(
                    "{}  {} 行  {}",
                    h.str_at("path").unwrap_or("?"),
                    h.u64_at("lines").unwrap_or(0),
                    h.u64_at("score").unwrap_or(0)
                );
            }
        }
        return Ok(0);
    }
    let out = if args.flag("incremental") {
        ui_core::index::build_incremental(&ws)?
    } else {
        ui_core::index::build(&ws)?
    };
    println!("{}", out.to_pretty());
    Ok(0)
}

fn cmd_checkpoint(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let label = args.get("label").unwrap_or("manual");
    let cp = ui_core::checkpoint::create(&ws, label)?;
    println!("{}", cp.to_pretty());
    Ok(0)
}

fn cmd_checkpoints(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    if args.flag("gc") {
        let keep = match args.get("keep") {
            Some(v) => Some(
                v.parse::<usize>()
                    .map_err(|_| format!("--keep 需要非负整数: {}", v))?,
            ),
            None => None,
        };
        let dry_run = !args.flag("yes");
        let out = ui_core::checkpoint::gc(&ws, keep, dry_run)?;
        if args.flag("json") {
            println!("{}", out.to_compact());
        } else {
            println!("{}", out.to_pretty());
            if dry_run {
                println!("dry-run：确认后加 --yes 才会删盘");
            }
        }
        return Ok(0);
    }
    let rows = ui_core::checkpoint::list(&ws)?;
    if args.flag("json") {
        println!("{}", Json::Arr(rows).to_compact());
        return Ok(0);
    }
    if rows.is_empty() {
        println!("还没有 checkpoint。先运行：ui checkpoint --label before-edit");
        return Ok(0);
    }
    println!("{:<32} {:>6}  label", "checkpoint", "files");
    for row in rows {
        println!(
            "{:<32} {:>6}  {}",
            row.str_at("id").unwrap_or("?"),
            row.u64_at("files").unwrap_or(0),
            row.str_at("label").unwrap_or("")
        );
    }
    Ok(0)
}

fn cmd_diff(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let id = args.get("checkpoint");
    let report = ui_core::checkpoint::diff(&ws, id)?;
    if args.flag("json") {
        println!("{}", report.to_json().to_compact());
    } else {
        print!("{}", report.render());
    }
    Ok(0)
}

fn cmd_review(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let id = args.get("checkpoint");
    let report = ui_core::checkpoint::diff(&ws, id.as_deref())?;
    let accept = args.get("accept");
    if accept.is_none() {
        if args.flag("json") {
            println!("{}", report.to_json().to_compact());
        } else {
            print!("{}", report.render());
            println!("\n用 --accept <path#n,...|all> --yes 接受 hunk；其余 hunk 会回退到 checkpoint。");
        }
        return Ok(0);
    }
    if !args.flag("yes") {
        return Err("review 会改写工作区；确认后加 --yes".to_string());
    }
    let accepted: Vec<String> = accept
        .unwrap()
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let mut out = ui_core::checkpoint::review(&ws, report.checkpoint.as_str(), &accepted)?;
    let jp = resolve_journal(args, &ws);
    let seq = match ui_core::Journal::open(&jp).and_then(|mut j| {
        j.append(
            "review/apply",
            Json::obj(vec![
                ("actor", Json::from("cli")),
                ("checkpoint", Json::from(report.checkpoint.as_str())),
                (
                    "accepted",
                    Json::Arr(accepted.iter().map(|a| Json::from(a.as_str())).collect()),
                ),
                ("written", Json::from(out.u64_at("written").unwrap_or(0))),
                ("removed", Json::from(out.u64_at("removed").unwrap_or(0))),
                ("backup", out.get("backup").cloned().unwrap_or(Json::Null)),
            ]),
        )
    }) {
        Ok(event) => event.u64_at("seq").unwrap_or(0),
        Err(e) => {
            return Err(format!(
                "review 已写回文件，但审计失败: {}；需要人工补账",
                e
            ))
        }
    };
    out.set("journalSeq", Json::from(seq));
    println!("{}", out.to_pretty());
    Ok(0)
}

fn cmd_revert(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let id = args
        .get("checkpoint")
        .ok_or_else(|| "revert 需要 --checkpoint <id>".to_string())?;
    let path = args
        .get("path")
        .ok_or_else(|| "revert 需要 --path <workspace 相对路径>".to_string())?;
    if !args.flag("yes") {
        return Err(format!("revert 会改写 {}；确认后加 --yes", path));
    }
    let out = ui_core::checkpoint::revert_file(&ws, id, path)?;
    let jp = resolve_journal(args, &ws);
    let seq = match ui_core::Journal::open(&jp).and_then(|mut j| {
        j.append(
            "review/revert",
            Json::obj(vec![
                ("actor", Json::from("cli")),
                ("checkpoint", Json::from(id)),
                ("path", Json::from(path)),
                (
                    "action",
                    out.str_at("action").map(Json::from).unwrap_or(Json::Null),
                ),
            ]),
        )
    }) {
        Ok(event) => event.u64_at("seq").unwrap_or(0),
        Err(e) => {
            return Err(format!(
                "revert 已写回文件，但审计失败: {}；需要人工补账",
                e
            ))
        }
    };
    let mut out = out;
    out.set("journalSeq", Json::from(seq));
    println!("{}", out.to_pretty());
    Ok(0)
}

fn cmd_restore(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let id = args
        .get("checkpoint")
        .ok_or_else(|| "restore 需要 --checkpoint <id>".to_string())?;
    if !args.flag("yes") {
        return Err(format!(
            "restore 会把工作区文件写回 checkpoint {}；确认后加 --yes",
            id
        ));
    }
    let out = if args.flag("prune") {
        ui_core::checkpoint::restore_prune(&ws, id)?
    } else {
        ui_core::checkpoint::restore(&ws, id)?
    };
    println!("{}", out.to_pretty());
    Ok(0)
}

/// 人话版的文件大小：`ui spill` 要说清"这里攒了多少原文"，字节数没人看得懂。
fn human_bytes(n: u64) -> String {
    const K: u64 = 1024;
    if n < K {
        return format!("{} B", n);
    }
    if n < K * K {
        return format!("{} KiB", (n + K - 1) / K);
    }
    if n < K * K * K {
        return format!("{} MiB", (n + K * K - 1) / (K * K));
    }
    format!("{} GiB", (n + K * K * K - 1) / (K * K * K))
}

/// `k`/`m`/`g` 后缀的容量写法：`--max-bytes 64m` 比 `67108864` 少一个错数量级的机会。
fn parse_size(s: &str) -> Result<u64, String> {
    // 解析本体在 `spill::parse_size`：命令行与环境变量必须认同一套写法，
    // 否则"这里能写 1k、那里只能写 1024"会变成要靠试出来的东西。
    ui_core::spill::parse_size(s)
        .ok_or_else(|| format!("`{}` 不是字节数（认 512b/1k/256m/2g，或纯数字）", s))
}

/// `ui spill`：看与清落盘的**原文**（`.ui/tool-output/` ＋ `.ui/tool-private/`）。
///
/// 两个目录放的都是已经从日志里抹掉的正文，区别只有一条：`tool-output/` 是被
/// 截断的长输出，模型按需读得回来（只读例外）；`tool-private/` 是按隐私清单扣留
/// 的原文，模型读不到，只有人和重放看得见。保留策略管的是"这台机器上还留着多少
/// 份原文"，所以个数与字节上限都按**两目录合计**算——分开算等于把上限翻倍。
///
/// 日志干净不等于这台机器干净：所以内核一边在 run 边界按保留
/// 策略自动清（`ui_core::spill::Policy`），一边给人留一个能随时问一句
/// 「现在盘上还有什么、按现在这一档会留下什么」的入口。
///
/// `clean` 默认只按策略清；`--all` 是无视策略全清（出门前那一下），代价是
/// 本轮历史里"完整内容在 …"的指针全部失效，所以它要 `--yes`。
fn cmd_spill(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let dirs = ui_core::spill::dirs(&ws);
    let label = |d: &std::path::Path| {
        d.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let verb = args.sub(0).unwrap_or("status");
    if !matches!(verb, "status" | "clean") {
        return Err(format!(
            "ui spill 只认 status|clean，收到 `{}`（`ui spill --help`）",
            verb
        ));
    }
    let as_json = args.flag("json");
    let dry = args.flag("dry-run");
    let now = ui_core::time::now_ms();

    // 命令行 > 环境变量 > 默认。三个上限各留一个覆盖口：跑一次性的清理时，
    // 不该逼人为了临时改一档去动 shell 配置。
    let mut policy = ui_core::spill::Policy::from_env();
    if let Some(v) = args.get("older-than") {
        policy.max_age_days = v
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("--older-than 要的是天数，收到 `{}`", v))?;
    }
    if let Some(v) = args.get("max-files") {
        policy.max_files = v
            .trim()
            .parse::<usize>()
            .map_err(|_| format!("--max-files 要的是个数，收到 `{}`", v))?;
    }
    if let Some(v) = args.get("max-bytes") {
        policy.max_bytes = parse_size(v)?;
    }
    let all = args.flag("all");
    if all {
        if verb != "clean" {
            return Err("--all 只对 `ui spill clean` 有意义".to_string());
        }
        if !args.flag("yes") {
            return Err(
                "--all 会清掉**全部**落盘原文（包括这一轮还指着的那些），日志里的指针会一起失效。\
                 确认就加 --yes；想按保留策略走就别加 --all。"
                    .to_string(),
            );
        }
        policy.max_age_days = 0;
        policy.max_files = 0;
        policy.max_bytes = 0;
        policy.keep_all = false;
    }

    let broken: Vec<String> = dirs
        .iter()
        .filter(|d| ui_core::spill::unreadable(d))
        .map(|d| label(d))
        .collect();
    if !broken.is_empty() {
        // 读不出来 ≠ 里面是空的。把这种情况报成"干净"是最难查的误导。
        // 只报目录名：绝对路径里带着用户名与项目名，而这一行会被贴进 issue。
        return Err(format!(
            "{} 存在但列不出来（权限或挂载问题）——这里可能仍有原文，先修可读性再谈清理",
            broken.join(", ")
        ));
    }
    // 两个目录**合起来**算一份账：上限管的是"这台机器上留了多少份原文"，
    // 分开算等于每个目录各领一份完整预算。
    let listed = ui_core::spill::listings(&dirs);
    let foot = ui_core::spill::footprints(&dirs);
    let plan = ui_core::spill::plan_all(&dirs, &policy, now);
    let doomed = &plan.remove;
    let doomed_bytes = plan.bytes;
    // 账上那些指向这里、但文件已经不在的指针：清理是对的，但断了要说
    let jp = resolve_journal(args, &ws);
    let dangling: Vec<String> = match std::fs::File::open(&jp) {
        Ok(_) => {
            let (events, _) = Journal::read(&jp).map_err(|e| format!("{}: {}", jp.display(), e))?;
            ui_core::spill::dangling(&events, &ws)
        }
        Err(_) => Vec::new(),
    };

    // 身份按**全路径**认：两个目录里的命名规则一样，只比文件名会有撞上的可能
    let going: Vec<std::path::PathBuf> = doomed.iter().map(|e| e.path.clone()).collect();
    let from = |e: &ui_core::spill::Entry| {
        e.path
            .parent()
            .map(|p| label(p))
            .unwrap_or_else(|| "?".to_string())
    };
    let prune_result = if verb == "clean" && !dry {
        Some(ui_core::spill::prune_all(&dirs, &policy, now))
    } else {
        None
    };

    if as_json {
        let rows = Json::Arr(
            listed
                .iter()
                .map(|e| {
                    Json::obj(vec![
                        ("name", Json::from(e.name.as_str())),
                        ("bytes", Json::from(e.bytes)),
                        ("ageDays", Json::from((now.saturating_sub(e.touched_ms) as f64) / 86400000.0)),
                        ("mine", Json::Bool(e.mine)),
                        ("from", Json::from(from(e).as_str())),
                        (
                            "scheduled",
                            Json::Bool(going.iter().any(|n| *n == e.path)),
                        ),
                    ])
                })
                .collect(),
        );
        let mut payload = vec![
            ("workspace", Json::from(ws.to_string_lossy().as_ref())),
            (
                "dirs",
                Json::Arr(
                    dirs.iter()
                        .map(|d| Json::from(d.to_string_lossy().as_ref()))
                        .collect(),
                ),
            ),
            ("policy", Json::from(policy.describe().as_str())),
            (
                "envWarnings",
                Json::Arr(
                    ui_core::spill::Policy::warnings()
                        .into_iter()
                        .map(|w| Json::from(w.as_str()))
                        .collect(),
                ),
            ),
            ("keepAll", Json::Bool(policy.keep_all)),
            ("files", Json::from(listed.len() as u64)),
            ("bytes", Json::from(foot.bytes)),
            ("foreign", Json::from(foot.foreign as u64)),
            ("cleaned", Json::Bool(verb == "clean" && !dry)),
            (
                "willRemove",
                Json::Arr(
                    doomed
                        .iter()
                        .map(|e| Json::from(e.name.as_str()))
                        .collect(),
                ),
            ),
            ("willRemoveBytes", Json::from(doomed_bytes)),
            (
                "danglingPointers",
                Json::Arr(
                    dangling
                        .iter()
                        .map(|d| Json::from(d.as_str()))
                        .collect(),
                ),
            ),
            ("entries", rows),
        ];
        if let Some(p) = &prune_result {
            payload.push(("removed", Json::from(p.removed as u64)));
            payload.push(("freedBytes", Json::from(p.freed_bytes)));
            payload.push(("failed", Json::from(p.failed as u64)));
        }
        println!("{}", Json::obj(payload).to_compact());
        return Ok(0);
    }

    println!(
        "落盘原文：{}（模型可读） ＋ {}（只给人看）",
        dirs[0].display(),
        dirs[1].display()
    );
    println!(
        "  现存 {} 个文件、{}（其中 {} 个不是内核命名，任何清理都不会碰）",
        listed.len(),
        human_bytes(foot.bytes),
        foot.foreign
    );
    println!(
        "  保留策略：{}{}",
        policy.describe(),
        if all { "（本次被 --all 覆盖成「一个不留」）" } else { "" }
    );
    for w in ui_core::spill::Policy::warnings() {
        // 设了却没生效的旋钮：不说就等于骗人"这一档在管着"
        println!("  ⚠ {}", w);
    }
    if listed.is_empty() {
        println!("  盘上没有原文。日志里那些被抹掉/扣留的正文从没落到过这两个目录。");
    } else if !listed.iter().any(|e| e.mine) {
        println!("  一个都不是内核命名的——这里的东西不是我写的，我不删。");
    } else {
        const SHOW: usize = 12;
        for e in listed.iter().take(SHOW) {
            let age = now.saturating_sub(e.touched_ms) / 1000;
            let age_s = if age < 3600 {
                format!("{} 分钟前", age / 60)
            } else if age < 86400 {
                format!("{} 小时前", age / 3600)
            } else {
                format!("{} 天前", age / 86400)
            };
            println!(
                "  {} {:>9} {:>9}  {}",
                if going.iter().any(|n| *n == e.path) {
                    "会清"
                } else if e.mine {
                    "留下"
                } else {
                    "外来"
                },
                age_s,
                human_bytes(e.bytes),
                if from(e) == label(std::path::Path::new(ui_core::spill::PRIVATE_DIR)) {
                    format!("{}  [扣留原文]", e.name)
                } else {
                    e.name.clone()
                }
            );
        }
        if listed.len() > SHOW {
            println!("  … 还有 {} 个（--json 看全部）", listed.len() - SHOW);
        }
    }
    println!(
        "  按这一档：{} 个、{} 会被清掉",
        doomed.len(),
        human_bytes(doomed_bytes)
    );
    if policy.keep_all && !all {
        println!("  注意：现在是「全留」——removed=0 的意思是没清，不是这里没东西");
    }
    if !dangling.is_empty() {
        println!("  日志里有 {} 条指针指向已经不存在的原文：", dangling.len());
        for d in dangling.iter().take(8) {
            println!("    · {}", d);
        }
        println!("    （这是清理的正常结果：正文出过盘，但保留期过了。日志里的字数与指纹仍然可核对。）");
    }
    match &prune_result {
        Some(p) => {
            println!(
                "\n已清：删掉 {} 个、释放 {}",
                p.removed,
                human_bytes(p.freed_bytes)
            );
            if p.failed > 0 {
                println!("  有 {} 个删不掉（只读挂载或被占用）——它们还在盘上，下次再试", p.failed);
            }
            if p.skipped_foreign > 0 {
                println!("  跳过 {} 个非内核命名的文件", p.skipped_foreign);
            }
            println!("  剩下 {} 个、{}", p.kept.files, human_bytes(p.kept.bytes));
        }
        None if verb == "clean" => println!("\n--dry-run：以上清单是将被删的，没有动盘"),
        None => println!("\n要按这一档清一次：ui spill clean（先看清单加 --dry-run）"),
    }
    Ok(0)
}

fn dispatch(argv: &[String]) -> Result<i32, String> {
    let (command, args) = Args::parse(argv);
    // 隐私清单必须在任何命令跑起来之前登记完：工具侧、策略层、HTTP 共用
    // 同一份判定，晚一步就会有某条路先按默认清单判过一次。
    apply_private(&args)?;
    match command.as_deref() {
        Some("doctor") => cmd_doctor(&args),
        Some("run") => cmd_run(&args),
        Some("chat") => cmd_chat(&args),
        Some("preview") => cmd_preview(&args),
        Some("tui") => cmd_tui(&args),
        Some("serve") => cmd_serve(&args),
        Some("resume") => cmd_resume(&args),
        Some("replay") => cmd_replay(&args),
        Some("verify") => cmd_verify(&args),
        Some("export") => cmd_export(&args),
        Some("secrets") => cmd_secrets(&args),
        Some("spill") => cmd_spill(&args),
        Some("skills") => cmd_skills(&args),
        Some("workflow") => cmd_workflow(&args),
        Some("team") => cmd_team(&args),
        Some("prompt") => cmd_prompt(&args),
        Some("providers") => cmd_providers(&args),
        Some("plugins") => cmd_plugins(&args),
        Some("sessions") => cmd_sessions(&args),
        Some("index") => cmd_index(&args),
        Some("checkpoint") => cmd_checkpoint(&args),
        Some("checkpoints") => cmd_checkpoints(&args),
        Some("diff") => cmd_diff(&args),
        Some("review") => cmd_review(&args),
        Some("revert") => cmd_revert(&args),
        Some("restore") => cmd_restore(&args),
        Some("apply") => cmd_apply(&args),
        Some("audit") => cmd_audit(&args),
        Some("lab") => cmd_lab(&args),
        Some("policy") => cmd_policy(&args),
        Some("knowledge") => knowledge::cmd(&args),
        Some("remember") => memory::cmd_remember(&args),
        Some("forget") => memory::cmd_forget(&args),
        Some("memory") => memory::cmd(&args),
        Some(other) => Err(format!(
            "未知命令 `{}`（doctor|run|chat|preview|tui|resume|replay|verify|export|secrets|spill|skills|workflow|apply|policy|knowledge|remember|forget|memory|index|checkpoint|checkpoints|diff|review|revert|restore）",
            other
        )),
        None => {
            println!("{}", help());
            Ok(0)
        }
    }
}

/// 把命令行与环境变量上声明的隐私路径登记进进程级清单。
///
/// 三条来源，全部累加：重复的 `--private a --private b`、单个值里的
/// `a;b` / `a,b`，以及 `UI_PRIVATE`（给容器与 CI 用的——那里改不动命令行）。
/// 声明只在进程启动时发生一次，模型的工具写不到这张表，否则「把 secrets
/// 从清单里删掉」就成了一次它能自主发起的升级。
fn apply_private(args: &Args) -> Result<(), String> {
    for raw in args.many("private") {
        // 一条都不剩就是写错了（`--private ""`、`--private "  "`、引号漏了
        // 只剩个分号）。静默接受比不声明更糟：人会以为已经挡住了。
        let parts = split_list(&raw);
        if parts.is_empty() {
            return Err(format!(
                "--private 没有收到可用的路径：{:?}（要一次给多条用 ; 或 , 分隔）",
                raw
            ));
        }
        for part in parts {
            ui_core::protected::add_private(&part)?;
        }
    }
    // 环境变量给不了就当作没设；设了却是 `";"` 这种，同样按写错处理。
    if let Ok(env) = std::env::var("UI_PRIVATE") {
        if !env.trim().is_empty() {
            let parts = split_list(&env);
            if parts.is_empty() {
                return Err(format!("UI_PRIVATE 没有收到可用的路径：{:?}", env));
            }
            for part in parts {
                ui_core::protected::add_private(&part)?;
            }
        }
    }
    Ok(())
}

/// 拆列表值：`;` 与 `,` 都当分隔符，空段丢掉。
fn split_list(raw: &str) -> Vec<String> {
    raw.split([';', ','])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn help() -> String {
    "ui — 一个只认七条契约的 harness\n\n\
     ui doctor                    报告 harness 对这台机器的真实认知\n\
     ui run --task \"...\" --mock F 跑一轮（F 是剧本 JSON）\n\
     ui chat --mock F             交互式会话：多轮、同一份日志\n\
     ui serve --port 7788         网页端：浏览器里和模型对话（本地，零依赖）\n\
     ui serve --trusted-proxy loopback --trusted-proxy-secret S   受信代理注入 X-UI-User/X-UI-Roles（M1，缺 S 拒绝启动）\n\
     ui resume                    对账崩溃打断的副作用\n\
     ui replay                    回放日志\n\
     ui verify                    日志自检：哈希链、未完成调用\n\
     ui export --html F           导出单文件轨迹查看器（双击即看；默认脱敏，--raw 出原样）\n\
     ui secrets [--redact-out F]  扫日志与工具输出里的疑似密钥（只报位置，命中退 1）\n\
     ui spill [clean]             看/清落盘原文 .ui/tool-output/ ＋ .ui/tool-private/（--dry-run 先给清单；--all --yes 一个不留）\n\
     ui skills                    列出工作区里的 skill\n\
     ui workflow --file F         按文件里的步骤跑工作流（每步一次 turn）\n\
     ui workflow --file F --isolate   并行子代理各用一份工作区副本\n\
     ui team --task \"复杂任务\" --agents 3   协作模式：自动拆成并行子任务再跑\n\
     ui apply --id ID [--yes]     把某个隔离子代理的改动写回工作区（--yes = 一个可回退的事务）\n\
     ui apply --rollback [--txn ID] --yes   把某次 apply 退回写回之前的原样\n\
     ui audit --as strict         用另一个模式重判历史：哪些动作会被拦\n\
     ui lab pack --name e1        打包一次实验：产物 + 哈希 + 血缘 + 环境\n\
     ui lab claim --name acc --value 0.9   声明结论（必须被产物背书）\n\
     ui lab verify <包目录>       校验：产物哈希 / 输入版本 / 结论 / 日志链\n\
     ui lab table <包...>         多 seed 对照表（输入不一致拒绝求均值）\n\
     ui knowledge list            列出 docs/knowledge 下的知识卡（id/确认位/来源 sha/touches）\n\
     ui knowledge check           校验卡上的 source_sha 与 touches 命中文件是否一致（STALE 退 2）\n\
     ui knowledge use <id> --reason \"...\"   手工写一条 knowledge/use（验证证据链）\n\
     ui knowledge emit <id>       写一条 knowledge/emit（卡片来源绑定 + 确认位）\n\
     ui remember \"<text>\"        写一条记忆（--kind fact|pref|pitfall|build --key k --scope workspace|project --expires DAYS）\n\
     ui forget <id> --reason \"...\"  写墓碑：下轮起不再注入（未知 id 退 1）\n\
     ui memory list [--all]       列出记忆（默认只列有效；--all 含墓碑/过期）\n\
     ui memory check              校验记忆文件哈希链 + id/内容一致（损坏退 2）\n\
     ui prompt --memory-query \"...\"  单独渲染一次记忆检索（不写事件、不跑 turn）\n\
     ui index [--incremental]     建零依赖代码索引（--incremental 复用未变文件）\n\
     ui index --query \"foo\"       按路径/符号查相关文件\n\
     ui checkpoint [--label L]    给当前 workspace 存一个零依赖内容寻址快照\n\
     ui checkpoints [--json]      列出 checkpoint\n\
     ui checkpoints --gc --keep 50 [--yes]  清理旧 checkpoint/未引用 blob（默认 dry-run）\n\
     ui diff [--checkpoint ID]    对比当前 workspace 与 checkpoint（默认最新）\n\
     ui review [--checkpoint ID]  列出 hunk；--accept path#n,all --yes 只保留选中的 hunk\n\
     ui revert --checkpoint ID --path P --yes   只回退一个文件（新增文件则删除）\n\
     ui restore --checkpoint ID [--prune] --yes   从 checkpoint 写回（--prune 删除未跟踪新文件）\n\
     ui tui                       全屏终端界面\n\
     ui policy --effect fs.write --target /etc/passwd\n\
     ui preview --cols 84 --rows 22   渲染一帧界面（不需要终端）\n\n\
     通用参数: --workspace PATH  --task-file PATH  --max-tokens N  --max-calls N  --max-steps N  --yes\n\
     审阅:     ui run --checkpoint 在开跑前存基线；之后用 ui diff / ui review / ui revert\n\
              自动基线默认开启（每个 turn 首次写操作前），--no-auto-checkpoint 关闭\n\
     隐私:     --private PATH（可重复，也可 a;b 一条给多个）或 UI_PRIVATE=\"a;b\"\n\
             默认清单 .ui/.git/secrets 之外再挡住的路径：工具、策略、网页端共用同一份\n\
     落账:     工具输出写进日志前默认抹掉凭据形状（要原文加 --no-redact，日志会记下这个选择）\n\
     模式:     --mode quiet | standard | strict\n\
     模型:     --mock 剧本.json   或   UI_MODEL_CMD=\"node adapters/deepseek.mjs\"\n\
     网页端:   --port N  --bundles 目录  --bench-cmd \"命令\"\n\
               --trusted-proxy loopback|<ip>  --trusted-proxy-secret <secret>（两者必须同时给）\n\
     外部工具: --mcp mcp.json（stdio JSON-RPC，工具效果记为 proc.exec mcp:<server>:<tool>）"
        .to_string()
}

fn cmd_doctor(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let env = inspect_environment(&ws);
    println!("环境（由真实进程探测，不是模板）");
    for line in env.render().lines() {
        println!("  {}", line);
    }

    // 诚实标注能力边界：shell / 插件 / MCP 是"声明式"的信任边界——
    // 我们只仲裁"要不要执行这条命令"，不仲裁命令内部做了什么。
    println!("\n信任边界（说清楚，不含糊）");
    // 沙箱状态：这是"能不能阻止"和"只能事后记账"的分界线，必须如实打
    let sb = ui_core::sandbox::describe(&ui_core::sandbox::Config::default());
    println!("  shell   ：{}", sb);
    println!("            执行前后还会对比工作区，把**实际改动**记入 effect/observed（UI_OBSERVE=0 可关）");
    #[cfg(windows)]
    println!("            注意：Windows 只有 Job Object（进程树 + 资源上限），**没有**文件写入限制");
    println!("  plugin  ：效果按声明仲裁；声明之外的行为无法拦截");
    println!("  mcp_call：同上，外部 server 在进程内自主");
    println!("            读取两侧都不限制——沙箱挡的是写，不是读");
    println!("  结论    ：Linux 上 shell 有写入与网络沙箱；Windows 上只有资源与回收约束；两边的**读**都靠策略与清单，不靠内核");

    // 隐私清单：判定只有一份（protected.rs），工具、策略、网页端、导出
    // 都问它。doctor 要把「此刻生效的是哪几条」原样打出来——不然人加了
    // `--private`，却没有任何地方能确认它真的被登记了。
    println!("\n隐私清单（工具不可读、不可写；网页端不可直达）");
    for entry in ui_core::protected::private_entries() {
        let from = if ui_core::protected::is_declared(&entry) {
            "声明"
        } else {
            "默认"
        };
        println!("  · [{}] {}", from, entry);
    }
    println!("  追加：--private PATH（可重复、可 a;b）或 UI_PRIVATE=\"a;b\"。只有命令行能加，模型加工具加不了");

    // 落账前的脱敏管的是**日志**，管不到落盘缓存：被截断的长输出在
    // .ui/tool-output/，按清单扣留的原文在 .ui/tool-private/。它们是 0600，
    // 但仍然是原文——打包工作区、整个目录发给别人的人会把这份也一起带走。
    // doctor 必须把脚印与**保留策略**一起打出来，否则"日志里没有"会被读成
    // "这台机器上没有"，而"会自动清"又被读成"已经清过了"。
    let spill_dirs = ui_core::spill::dirs(&ws);
    let policy = ui_core::spill::Policy::from_env();
    let now = ui_core::time::now_ms();
    let foot = ui_core::spill::footprints(&spill_dirs);
    let plan = ui_core::spill::plan_all(&spill_dirs, &policy, now);
    let unreadable = spill_dirs.iter().any(|d| ui_core::spill::unreadable(d));
    println!(
        "\n落盘原文（{} 个文件，{}）：{} ＋ {}",
        foot.files,
        human_bytes(foot.bytes),
        spill_dirs[0].display(),
        spill_dirs[1].display()
    );
    if unreadable {
        println!("  ⚠ 有目录存在但列不出来（权限/挂载）：这里可能仍有原文，上面的数字不可信");
    }
    println!("  日志里抹掉或扣留的那一份正文就在这儿；保留策略 {}（run 边界自动执行）", policy.describe());
    for w in ui_core::spill::Policy::warnings() {
        println!("  ⚠ {}——这一档没生效，原文会按上面那行攒着", w);
    }
    println!("  下一次会自动清掉 {} 个、{}；看清单与手动清：ui spill / ui spill clean",
        plan.remove.len(), human_bytes(plan.bytes));
    if foot.foreign > 0 {
        println!(
            "  其中 {} 个不是内核命名——清理不碰它们（要删请自己确认是什么）",
            foot.foreign
        );
    }
    if policy.keep_all {
        println!("  ⚠ 现在是「全留」档：自动清理被 UI_SPILL_KEEP=all 关着，原文会一直攒着");
    }
    // 账上指向已删原文的指针：清理的正常结果，但得有人说出来
    let jp = resolve_journal(args, &ws);
    if jp.is_file() {
        if let Ok((events, _)) = Journal::read(&jp) {
            let dangling = ui_core::spill::dangling(&events, &ws);
            if !dangling.is_empty() {
                println!(
                    "  日志里有 {} 条指针指向已过保留期的原文（字数与指纹仍可核对，正文找不回来了）",
                    dangling.len()
                );
            }
        }
    }
    println!("  出门前查一遍：ui secrets 会连它一起扫，命中即退出码 1");

    println!("\n工具与效果");
    for tool in registry() {
        let sample = match tool.name {
            "fs_read" | "fs_edit" => Json::obj(vec![("path", Json::from("src/main.rs"))]),
            "fs_write" => Json::obj(vec![
                ("path", Json::from("src/main.rs")),
                ("content", Json::from("")),
            ]),
            "fs_search" => Json::obj(vec![("pattern", Json::from("TODO"))]),
            _ => Json::obj(vec![("command", Json::from("echo hi"))]),
        };
        let ctx = ui_core::tools::Ctx {
            workspace: ws.clone(),
            ..Default::default()
        };
        let effects = (tool.effects)(&sample, &ctx);
        let policy = Policy::new(Policy::defaults(&ws), &ws);
        let verdict = policy.decide(&effects);
        println!(
            "  {:<9} {:<14} {}",
            tool.name,
            if tool.idempotent { "可重放" } else { "有副作用" },
            verdict
                .reasons
                .iter()
                .map(|(e, d, _)| format!("{}={}", e.kind, d.name()))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    let jp = resolve_journal(args, &ws);
    match Journal::load(&jp) {
        Ok(events) => {
            println!("\n日志 {}", jp.display());
            println!("  {} 条事件，链完整", events.len());
            let inc = analyze_incomplete(&events);
            if inc.open_turn.is_some() || !inc.open_tools.is_empty() {
                println!(
                    "  ⚠ 有未完成工作: turn={:?} tools={}",
                    inc.open_turn.is_some(),
                    inc.open_tools.len()
                );
            }
        }
        Err(e) => println!("\n日志 {}: {}", jp.display(), e),
    }
    Ok(0)
}

/// `--provider NAME` / `.ui/providers.json`：把「用哪个模型」变成可切换的配置。
/// 适配器仍然是外部进程，内核只负责选谁、传什么模型名。
fn provider_model(args: &Args) -> Option<Result<Box<dyn Model + Send>, String>> {
    let ws = workspace(args);
    let path = args
        .get("providers")
        .map(PathBuf::from)
        .unwrap_or_else(|| providers::default_path(&ws));
    if !path.exists() && args.get("provider").is_none() {
        return None;
    }
    Some((|| {
        let reg = providers::Registry::load(&path)?;
        let p = reg
            .pick(args.get("provider"))
            .ok_or_else(|| {
                let names: Vec<&str> = reg.providers.iter().map(|p| p.name.as_str()).collect();
                format!(
                    "没有名为 `{}` 的 provider（可用：{}）",
                    args.get("provider").unwrap_or("?"),
                    names.join(", ")
                )
            })?;
        // 适配器是 shell 命令：用环境变量前缀把模型名和附加配置传进去
        let mut head = String::new();
        for (k, v) in &p.env {
            head.push_str(&format!("{}={} ", k, v));
        }
        if !p.model.is_empty() {
            head.push_str(&format!("UI_MODEL={} ", p.model));
        }
        let cmd = format!("{}{}", head, p.cmd.join(" "));
        Ok(Box::new(ExternalModel::new(&p.name, &cmd)) as Box<dyn Model + Send>)
    })())
}

/// `ui providers`：列出配置里的 provider（不启动任何进程）。
/// 本地插件必须显式 `--plugins FILE` 加载。
///
/// 不再自动信任工作区里的 `.ui/plugins.json`：仓库可以自带这个文件，
/// 而插件调用本身是本地代码执行；自动加载等于把“克隆仓库”变成“执行仓库”。
fn attach_plugins(agent: &mut Agent, args: &Args) -> Result<(), String> {
    let path = match args.get("plugins") {
        Some(p) => std::path::PathBuf::from(p),
        None => return Ok(()),
    };
    let set = ui_core::plugin::Set::load(&path)?;
    ui_core::prompt::add_extra_tools(set.catalog());
    agent.plugins = Some(std::sync::Arc::new(set));
    Ok(())
}

/// `ui plugins`：列出插件与它们声明的能力（不启动任何进程）。
fn cmd_plugins(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let path = match args.get("plugins") {
        Some(p) => std::path::PathBuf::from(p),
        None => ws.join(".ui").join("plugins.json"),
    };
    if !path.exists() {
        return Err(format!("没有插件配置：{}", path.display()));
    }
    let set = ui_core::plugin::Set::load(&path)?;
    println!("配置 {} · {} 个插件", path.display(), set.plugins.len());
    for p in &set.plugins {
        println!("  {}（{} 个工具）cmd={}", p.name, p.tools.len(), p.cmd);
        for t in &p.tools {
            let eff: Vec<String> = t
                .effects
                .iter()
                .map(|e| format!("{} {}", e.kind, e.target))
                .collect();
            println!(
                "    - {} — {}  [{}]",
                t.name,
                if t.summary.is_empty() { "?" } else { &t.summary },
                if eff.is_empty() { "无副作用".to_string() } else { eff.join(", ") }
            );
        }
    }
    Ok(0)
}

fn cmd_providers(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let path = args
        .get("providers")
        .map(PathBuf::from)
        .unwrap_or_else(|| providers::default_path(&ws));
    if !path.exists() {
        return Err(format!(
            "没有 provider 配置：{}\n可以写一份，例如：\n{}",
            path.display(),
            r#"{"default":"deepseek","providers":{"deepseek":{"cmd":"node tools/deepseek.mjs","model":"deepseek-flash"}}}"#
        ));
    }
    let reg = providers::Registry::load(&path)?;
    println!("配置 {} · 默认 {}", path.display(), if reg.default.is_empty() { "（未设）" } else { &reg.default });
    for p in &reg.providers {
        let mark = if p.name == reg.default { "*" } else { " " };
        println!("{} {:<12} model={:<18} cmd={}", mark, p.name, if p.model.is_empty() { "-" } else { &p.model }, p.cmd.join(" "));
    }
    Ok(0)
}

fn build_model(args: &Args) -> Result<Box<dyn Model + Send>, String> {
    if let Some(path) = args.get("mock") {
        let text = std::fs::read_to_string(path).map_err(|e| format!("读剧本失败: {}", e))?;
        let scenario = json::parse(&text).map_err(|e| format!("剧本不是合法 JSON: {}", e))?;
        return Ok(Box::new(ScriptedModel::from_json(&scenario)?));
    }
    // provider 注册表优先于裸的 UI_MODEL_CMD（它至少会校验名字存在）
    if let Some(found) = provider_model(args) {
        return found;
    }
    if let Ok(cmd) = std::env::var("UI_MODEL_CMD") {
        return Ok(Box::new(ExternalModel::new("external", &cmd)));
    }
    Err("需要 --mock 剧本、--provider NAME（配 .ui/providers.json），或设置 UI_MODEL_CMD 指向一个模型进程".to_string())
}

/// The interactive surface: one session, many turns, the same journal.
fn cmd_chat(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let limits = limits_from(args);
    let model = build_model(args)?;
    let jp = resolve_journal(args, &ws);
    let journal = Journal::open(&jp).map_err(|e| e.to_string())?;
    let mut agent = Agent::new(journal, model, ws.clone(), limits);
    restore_session_history(&mut agent, &jp)?;
    agent.compact = compact_policy(args);
    agent.tool_chars = tool_chars(args);
    if args.flag("no-auto-checkpoint") {
        agent.auto_checkpoint = false;
    }
    agent.mode = mode_from(args);
    attach_mcp(&mut agent, args)?;
    attach_plugins(&mut agent, args)?;
    apply_tool_filter(&mut agent, args);
    if args.flag("no-env-scrub") {
        agent.env_scrub = false;
    }
    if args.flag("no-redact") {
        // 关的是**落账前**那一道脱敏：日志里会留下原文，并且每轮开始先写
        // 一条 privacy/redact-off，三个月后翻日志的人能看出这段是原文。
        agent.set_redact_output(false);
    }
    if let Some(text) = args.get("append-system-prompt") {
        ui_core::prompt::set_extra_system(text);
    }

    println!("ui · workspace {}", ws.display());
    println!("/status 账本 · /policy 规则 · /journal 日志 · /quit 退出");
    let mut line = String::new();
    loop {
        print!("\n› ");
        let _ = io::stdout().flush();
        line.clear();
        match io::stdin().read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => return Err(format!("读输入失败: {}", e)),
        }
        let input = line.trim().to_string();
        if input.is_empty() {
            continue;
        }
        match input.as_str() {
            "/quit" | "/exit" => break,
            "/status" => println!(
                "{}",
                render::status_line(agent.model.name(), &agent.budget.snapshot(), &limits)
            ),
            "/journal" => println!("{}", jp.display()),
            "/policy" => {
                for rule in &agent.policy.rules {
                    println!(
                        "  {:<10} {:<44} {}",
                        rule.effect,
                        rule.pattern,
                        rule.decision.name()
                    );
                }
            }
            _ => {
                let mut hooks = ConsoleHooks::new(args.flag("yes"), false);
                // 一条输入 = 一次 run：预算、工具序号、「本次会话允许」都从
                // 这里重新起算。所以 /status 显示的是**这一条**的账本，
                // 整个会话的累计在日志里（`ui replay` 看得见）。
                agent.begin_run();
                match agent.run_turn(&input, &mut hooks) {
                    Ok(report) => println!(
                        "\n{}\n第 {} 轮结束 · {} · {} 次工具",
                        render::status_line(agent.model.name(), &agent.budget.snapshot(), &limits),
                        agent.turn_number(),
                        report.reason,
                        report.tool_calls
                    ),
                    Err(e) => println!("错误: {}", e),
                }
            }
        }
    }
    Ok(0)
}

/// 启动检查项：开机界面显示的就是这些**真实结果**，不是定时器。
fn boot_step(id: &str, ok: bool, detail: impl Into<String>, started: std::time::Instant) -> Json {
    Json::obj(vec![
        ("id", Json::from(id)),
        ("ok", Json::Bool(ok)),
        ("detail", Json::from(detail.into())),
        ("ms", Json::from(started.elapsed().as_millis() as u64)),
    ])
}

/// 网页端：浏览器里和模型对话（本地 HTTP + SSE，零依赖）
fn cmd_serve(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    // 配置在产生任何副作用（打开 journal、写 .ui/）之前先校验：受信代理没有
    // 共享密钥就拒绝启动，而不是先起来再"忽略身份头"。
    let (trusted_proxy, trusted_proxy_secret) = trusted_proxy_config(args)?;
    let port = args.get("port").unwrap_or("7788");
    let addr = format!("127.0.0.1:{}", port);

    let mut steps: Vec<Json> = Vec::new();

    let t = std::time::Instant::now();
    let model = build_model(args)?;
    steps.push(boot_step("config", true, format!("model {}", model.name()), t));

    // journal：打不开就把降级路径走通（临时日志），并在界面上如实说明
    let jp = resolve_journal(args, &ws);
    let t = std::time::Instant::now();
    let mut degraded = false;
    let journal = match Journal::open(&jp) {
        Ok(j) => {
            steps.push(boot_step(
                "journal",
                true,
                format!("{} 条事件 · 链完整", j.events.len()),
                t,
            ));
            j
        }
        Err(e) => {
            degraded = true;
            let tmp =
                std::env::temp_dir().join(format!("ui-degraded-{}.jsonl", std::process::id()));
            let j =
                Journal::open(&tmp).map_err(|e2| format!("{}；临时日志也打不开: {}", e, e2))?;
            steps.push(boot_step(
                "journal",
                false,
                format!("原日志不可用（{}）→ 已改用临时日志", e),
                t,
            ));
            j
        }
    };

    let t = std::time::Instant::now();
    let skills = ui_core::skill::Catalog::discover(&ws).skills.len();
    steps.push(boot_step("skills", true, format!("{} 个 skill", skills), t));

    let t = std::time::Instant::now();
    let writable = std::fs::create_dir_all(ws.join(".ui")).is_ok();
    steps.push(boot_step(
        "workspace",
        writable,
        if writable {
            ws.to_string_lossy().to_string()
        } else {
            "工作区不可写".to_string()
        },
        t,
    ));

    let limits = limits_from(args);
    let budget_tokens = limits.tokens;
    let mut agent = Agent::new(journal, model, ws.clone(), limits);
    agent.compact = compact_policy(args);
    agent.tool_chars = tool_chars(args);
    if args.flag("no-auto-checkpoint") {
        agent.auto_checkpoint = false;
    }
    agent.mode = mode_from(args);
    attach_mcp(&mut agent, args)?;
    attach_plugins(&mut agent, args)?;
    apply_tool_filter(&mut agent, args);
    if args.flag("no-env-scrub") {
        agent.env_scrub = false;
    }
    if args.flag("no-redact") {
        // 关的是**落账前**那一道脱敏：日志里会留下原文，并且每轮开始先写
        // 一条 privacy/redact-off，三个月后翻日志的人能看出这段是原文。
        agent.set_redact_output(false);
    }
    if let Some(text) = args.get("append-system-prompt") {
        ui_core::prompt::set_extra_system(text);
    }
    let label = agent.model.name().to_string();
    let ws_label = ws.to_string_lossy().to_string();

    let ready = Json::obj(vec![
        ("ok", Json::Bool(!degraded && writable)),
        ("degraded", Json::Bool(degraded)),
        ("model", Json::from(label.as_str())),
        ("workspace", Json::from(ws_label.as_str())),
        ("journal", Json::from(jp.to_string_lossy().to_string())),
        ("tools", Json::from(ui_core::tools::registry().len())),
        ("budget", Json::from(budget_tokens)),
        ("steps", Json::Arr(steps)),
    ]);
    let splash =
        !args.flag("no-splash") && std::env::var("UI_SPLASH").map(|v| v != "0").unwrap_or(true);

    server::serve(agent, &addr, ready, splash, args.flag("yes"), args.get("bundles"), args.get("bench-cmd"), args.get("token"), args.get("session"), trusted_proxy.as_deref(), trusted_proxy_secret.as_deref())?;
    Ok(0)
}

/// 解析受信代理配置：`--trusted-proxy` 必须配非空 `--trusted-proxy-secret`。
///
/// 校验本身在 `server` 里（那里也有一道，防止别的入口绕过），这里只负责把
/// 命令行上的空串/纯空格归一成"没给"。
fn trusted_proxy_config(args: &Args) -> Result<(Option<String>, Option<String>), String> {
    // `--trusted-proxy` 后面没跟值（被解析成布尔开关）或给了空串，都不该被
    // 静默当成"没开"：那会把一次配置错误变成"企业鉴权悄悄关掉"。
    if args.flag("trusted-proxy") {
        return Err("--trusted-proxy 需要一个值：loopback 或受信代理 IP".to_string());
    }
    let proxy = match args.get("trusted-proxy") {
        Some(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        Some(_) => {
            return Err("--trusted-proxy 不能为空：要 loopback 或受信代理 IP".to_string());
        }
        None => None,
    };
    if proxy.is_none() {
        // 没开受信代理时 `--trusted-proxy-secret` 没有意义，直接丢掉；本地模式
        // 既不解析 X-UI-*，也不读这个密钥。
        return Ok((None, None));
    }
    let secret = args
        .get("trusted-proxy-secret")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    server::check_trusted_proxy_secret(proxy.as_deref(), secret.as_deref())?;
    Ok((proxy, secret))
}

/// 渲染一帧全屏界面（纯函数，不需要 tty）。用来定版式，也方便贴进 issue。
fn cmd_preview(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let width = args.get("cols").and_then(|v| v.parse().ok()).unwrap_or(84usize);
    let height = args.get("rows").and_then(|v| v.parse().ok()).unwrap_or(22usize);

    let mut state = frame::ScreenState::default();
    state.workspace = ws.to_string_lossy().to_string();
    state.model = "scripted-demo".into();
    state.tokens = 1719;
    state.calls = 3;
    state.lines = vec![
        frame::Line::user("让 greet 处理空名字：空字符串时不该拼出 \"hi \"。"),
        frame::Line::assistant("先读一下 greet.js 看现状，再决定改哪里。"),
        frame::Line::tool_call("fs_read", true, "→ 1  export function greet(name) {"),
        frame::Line::assistant("空字符串会拼出 hi ，改成兜底。只动返回语句。"),
        frame::Line::tool_call("fs_edit", true, "→ 已编辑 demo/greet.js（1 处）"),
        frame::Line::tool_call("fs_read", true, "→ 2    return `hi ${name || 'there'}`;"),
        frame::Line::warn("历史段已占 144 tok；本轮预算还剩 98%"),
        frame::Line::assistant("改完了：空名字现在返回 hi there。要不要顺手跑一下测试？"),
        frame::Line::user("跑吧。"),
        frame::Line::assistant("需要执行命令，先要你批准。"),
    ];
    state.rail.step = 3;
    state.rail.snapshot = Some(frame::RailSnapshot {
        id: "8f2a1c7d9e0b4a12".into(),
        tokens: 1719,
        bytes: 6840,
        sections: vec![
            frame::RailSection {
                id: "core".into(),
                owner: "kernel".into(),
                tokens: 520,
            },
            frame::RailSection {
                id: "env".into(),
                owner: "kernel".into(),
                tokens: 96,
            },
            frame::RailSection {
                id: "tools".into(),
                owner: "tools".into(),
                tokens: 340,
            },
            frame::RailSection {
                id: "history".into(),
                owner: "session".into(),
                tokens: 763,
            },
        ],
    });
    state
        .rail
        .push_decision("c1", "fs_read", "allow", "读取 demo/greet.js");
    state
        .rail
        .push_decision("c2", "fs_edit", "allow", "编辑 demo/greet.js");
    state
        .rail
        .push_decision("c3", "shell", "ask", "执行 cargo test");
    state.approval = if args.flag("no-approval") {
        None
    } else {
        Some(frame::Approval {
            tool: "shell".into(),
            effects: vec!["proc.exec  cargo test".into()],
            args: "{\n  \"command\": \"cargo test\"\n}".into(),
        })
    };

    if args.flag("ansi") {
        for row in frame::frame(&state, width, height) {
            let mut line = String::new();
            for span in &row.spans {
                line.push_str(term::sgr(span.style));
                line.push_str(&span.text);
            }
            println!("{}\x1b[0m", line);
        }
    } else {
        for row in frame::frame_text(&state, width, height) {
            println!("{}", row);
        }
    }
    Ok(0)
}

/// 全屏对话：输入行发送，模型跑一轮，历史跨轮累积。
fn cmd_tui(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let limits = limits_from(args);
    let model = build_model(args)?;
    let initial = if let Some(t) = args.get("task") {
        Some(t.trim().to_string())
    } else if let Some(path) = args.get("task-file") {
        Some(
            std::fs::read_to_string(path)
                .map_err(|e| format!("读任务文件失败: {}", e))?
                .trim()
                .to_string(),
        )
    } else {
        None
    };

    let jp = resolve_journal(args, &ws);
    let journal = Journal::open(&jp).map_err(|e| e.to_string())?;

    // 终端里的开机块：和网页端同一套真实检查，只是印在滚动区里（不占全屏）
    if !args.flag("no-splash") && std::env::var("UI_SPLASH").map(|v| v != "0").unwrap_or(true) {
        println!("\n  \x1b[38;5;99mU:I\x1b[0m");
        println!("  \x1b[2mUnleash your imagination\x1b[0m\n");
        println!("  \x1b[1mINITIALIZING UI\x1b[0m");
        println!("    ✓ config     {}", model.name());
        println!("    ✓ journal    {} 条事件", journal.events.len());
        println!(
            "    ✓ skills     {} 个",
            ui_core::skill::Catalog::discover(&ws).skills.len()
        );
        println!("    ✓ workspace  {}", ws.display());
        println!("  \x1b[32mOK\x1b[0m\n");
    }

    let mut state = frame::ScreenState::default();
    state.workspace = ws.to_string_lossy().to_string();
    state.model = model.name().to_string();
    state.hint = "Enter 发送 · 运行中 Esc/Ctrl-C 取消 · Esc 退出 · /help".to_string();
    state.lines.push(frame::Line::info(format!(
        "UI · {} · 输入任务开始，/help 看命令",
        model.name()
    )));

    let tui = term::Tui::enter().map_err(|e| e.to_string())?;
    let mut hooks = TuiHooks {
        state,
        tui,
        allow_all: args.flag("yes"),
    };
    let mut agent = Agent::new(journal, model, ws.clone(), limits);
    agent.compact = compact_policy(args);
    agent.tool_chars = tool_chars(args);
    if args.flag("no-auto-checkpoint") {
        agent.auto_checkpoint = false;
    }
    agent.mode = mode_from(args);
    attach_mcp(&mut agent, args)?;
    attach_plugins(&mut agent, args)?;
    apply_tool_filter(&mut agent, args);
    if args.flag("no-env-scrub") {
        agent.env_scrub = false;
    }
    if args.flag("no-redact") {
        // 关的是**落账前**那一道脱敏：日志里会留下原文，并且每轮开始先写
        // 一条 privacy/redact-off，三个月后翻日志的人能看出这段是原文。
        agent.set_redact_output(false);
    }
    if let Some(text) = args.get("append-system-prompt") {
        ui_core::prompt::set_extra_system(text);
    }
    agent.cancel = hooks.tui.cancel_token();
    hooks.redraw();

    if let Some(task) = initial {
        run_one(&mut agent, &mut hooks, &task);
    }

    loop {
        hooks.redraw();
        let key = match hooks.tui.wait_key() {
            Ok(k) => k,
            Err(_) => break,
        };
        match key {
            term::Key::Esc | term::Key::CtrlC => break,
            term::Key::Enter => {}
            term::Key::Char(c) => {
                hooks.state.input.push(c);
                continue;
            }
            term::Key::Backspace => {
                hooks.state.input.pop();
                continue;
            }
            term::Key::PageUp | term::Key::Up => {
                hooks.state.scroll = hooks.state.scroll.saturating_add(4);
                continue;
            }
            term::Key::PageDown | term::Key::Down => {
                hooks.state.scroll = hooks.state.scroll.saturating_sub(4);
                continue;
            }
            term::Key::Home => {
                hooks.state.scroll = 9999;
                continue;
            }
            term::Key::End => {
                hooks.state.scroll = 0;
                continue;
            }
            _ => continue,
        }

        let text = hooks.state.input.trim().to_string();
        if text.is_empty() {
            continue;
        }
        hooks.state.input.clear();

        if text.starts_with('/') {
            handle_command(&mut agent, &mut hooks, &text);
            continue;
        }
        run_one(&mut agent, &mut hooks, &text);
    }

    drop(hooks); // 先还原终端，再打印普通文本
    println!("会话结束，日志: {}", jp.display());
    Ok(0)
}

fn handle_command(agent: &mut Agent, hooks: &mut TuiHooks, text: &str) {
    match text {
        "/status" => {
            let snap = agent.budget.snapshot();
            let used = snap.get("used");
            hooks.state.lines.push(frame::Line::info(format!(
                "tok {} · calls {} · wall {} ms · 历史 {} 条",
                used.and_then(|u| u.u64_at("tokens")).unwrap_or(0),
                used.and_then(|u| u.u64_at("toolCalls")).unwrap_or(0),
                used.and_then(|u| u.u64_at("wallMs")).unwrap_or(0),
                agent.history.len()
            )));
        }
        "/policy" => {
            for rule in &agent.policy.rules {
                hooks.state.lines.push(frame::Line::info(format!(
                    "{:<10} {:<44} {}",
                    rule.effect,
                    rule.pattern,
                    rule.decision.name()
                )));
            }
        }
        "/clear" => hooks.state.lines.clear(),
        "/help" => hooks.state.lines.push(frame::Line::info(
            "/status 账本 · /policy 规则 · /clear 清屏 · /quit 退出（Esc 也行）",
        )),
        "/quit" | "/exit" => hooks.state.lines.push(frame::Line::info("按 Esc 退出")),
        other => hooks
            .state
            .lines
            .push(frame::Line::warn(format!("未知命令 {}，试试 /help", other))),
    }
}

fn run_one(agent: &mut Agent, hooks: &mut TuiHooks, text: &str) {
    // 一条输入 = 一次 run：预算、工具序号、上一次的「本次会话允许」都重新起算。
    // 令牌必须**继续用 TUI 自己那一个**——Esc 取消拿的是 `tui.cancel_token()`，
    // 换成新令牌就等于把退出键和这一轮断开，界面再也取消不了。
    agent.begin_run_with_cancel(Some(hooks.tui.cancel_token()));
    hooks.state.rail.begin_turn();
    hooks.state.lines.push(frame::Line::user(text.to_string()));
    hooks.state.scroll = 0;
    hooks.redraw();

    // 跑一轮期间：Esc / Ctrl-C 变成"取消"，不再当输入
    hooks.tui.set_busy(true);
    let outcome = agent.run_turn(text, hooks);
    hooks.tui.set_busy(false);

    match outcome {
        Ok(report) => {
            let note = if report.reason == "cancelled" {
                "—— 已取消（历史保留，可继续）".to_string()
            } else {
                format!(
                    "—— 结束：{} · {} 步 · {} 次工具",
                    report.reason, report.steps, report.tool_calls
                )
            };
            hooks.state.lines.push(frame::Line::info(note));
        }
        Err(e) => hooks.state.lines.push(frame::Line::warn(format!("—— 出错：{}", e))),
    }
    if let Some(used) = agent.budget.snapshot().get("used") {
        hooks.state.tokens = used.u64_at("tokens").unwrap_or(0);
        hooks.state.calls = used.u64_at("toolCalls").unwrap_or(0);
        hooks.state.wall_ms = used.u64_at("wallMs").unwrap_or(0);
    }
    hooks.redraw();
}

struct TuiHooks {
    state: frame::ScreenState,
    tui: term::Tui,
    allow_all: bool,
}

impl TuiHooks {
    fn redraw(&mut self) {
        let _ = self.tui.draw(&self.state);
    }
}

impl Hooks for TuiHooks {
    fn on_assistant(&mut self, text: &str) {
        self.state.lines.push(frame::Line::assistant(text));
        self.redraw();
    }

    fn on_snapshot(&mut self, snapshot: &Snapshot) {
        self.state.rail.step += 1;
        self.state.rail.snapshot = Some(frame::RailSnapshot::from_snapshot(snapshot));
        self.redraw();
    }

    fn on_decision(&mut self, call_id: &str, tool: &str, decision: Decision, reason: &str) {
        self.state
            .rail
            .push_decision(call_id, tool, decision.name(), reason);
        self.redraw();
    }

    fn on_budget(&mut self, tokens: u64, tool_calls: u64) {
        self.state.tokens = tokens;
        self.state.calls = tool_calls;
        self.redraw();
    }

    fn on_compact(&mut self, before: u64, after: u64, dropped: usize) {
        self.state.lines.push(frame::Line::warn(format!(
            "—— 上下文折叠：{} 条较早消息，{} → {} tok（摘要已记入日志）",
            dropped, before, after
        )));
        self.redraw();
    }

    fn on_tool(&mut self, name: &str, _args: &Json, result: &ToolResult) {
        let detail = result
            .error
            .clone()
            .unwrap_or_else(|| result.content.lines().next().unwrap_or("").to_string());
        self.state.lines.push(frame::Line::tool_call(
            name,
            result.ok,
            format!("→ {}", detail.trim_start()),
        ));
        self.redraw();
    }

    fn on_approval(&mut self, req: &ApprovalRequest) -> ApprovalResponse {
        if self.allow_all {
            self.state
                .lines
                .push(frame::Line::info(format!("[auto] 已批准 {}", req.tool)));
            self.state.rail.resolve(&req.call_id, "allow-session");
            self.redraw();
            return ApprovalResponse::allow_session();
        }
        self.state.approval = Some(frame::Approval {
            tool: req.tool.clone(),
            effects: req.effects.iter().map(|e| e.describe()).collect(),
            args: req.args.to_pretty(),
        });
        self.redraw();
        loop {
            match self.tui.wait_key() {
                // `y` = 只批这一次，`a` = 本次会话内同类免审。右侧栏把范围
                // 印出来，用户才知道自己刚按下去的是哪一种。
                Ok(term::Key::Char('y')) | Ok(term::Key::Char('Y')) => {
                    self.state.approval = None;
                    self.state.rail.resolve(&req.call_id, "allow-once");
                    self.redraw();
                    return ApprovalResponse::allow_once();
                }
                Ok(term::Key::Char('a')) | Ok(term::Key::Char('A')) => {
                    self.allow_all = true;
                    self.state.approval = None;
                    self.state.rail.resolve(&req.call_id, ApprovalScope::Session.name());
                    self.redraw();
                    return ApprovalResponse::allow_session();
                }
                Ok(term::Key::Char('n'))
                | Ok(term::Key::Char('N'))
                | Ok(term::Key::Esc)
                | Ok(term::Key::CtrlC) => {
                    self.state.approval = None;
                    self.state.rail.resolve(&req.call_id, "deny");
                    self.redraw();
                    return ApprovalResponse::deny();
                }
                Ok(_) => {}
                Err(_) => {
                    // 终端读不到按键（会话被挂断、stdin 关了）：不是人按了「否」。
                    self.state.rail.resolve(&req.call_id, "deny");
                    return ApprovalResponse::deny().via("no-input");
                }
            }
        }
    }
}

/// `--tool-chars N`：工具输出在上下文里最多留多少字符。
///
/// 默认 0（不截断）：A/B 实测在中等读取量的任务上，截断会逼模型重读，
/// 反而多花 7.5% token。留给"一次输出十几 KB"的场景按需开启。
/// `--mcp <config.json>`：拉起外部 MCP server，把工具清单注进提示词。
fn attach_mcp(agent: &mut Agent, args: &Args) -> Result<(), String> {
    let path = match args.get("mcp") {
        Some(p) => p,
        None => return Ok(()),
    };
    let text = std::fs::read_to_string(path).map_err(|e| format!("读 MCP 配置失败: {}", e))?;
    let cfg = json::parse(&text).map_err(|e| format!("MCP 配置不是 JSON: {}", e))?;
    let hub = ui_core::mcp::Hub::start(&cfg, &agent.workspace, &agent.sandbox)?;
    ui_core::prompt::set_extra_tools(hub.catalog());
    agent.mcp = Some(std::sync::Arc::new(std::sync::Mutex::new(hub)));
    Ok(())
}

/// `--no-tools` / `--tools a,b,c`：每轮的工具白名单（A4）。
/// 关掉工具时提示词里也不会列出它们——不列比列了再拒绝更诚实。
fn apply_tool_filter(agent: &mut Agent, args: &Args) {
    if args.flag("no-tools") {
        agent.tools.clear();
        return;
    }
    if let Some(list) = args.get("tools") {
        let want: Vec<&str> = list.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        agent.tools.retain(|t| want.iter().any(|w| *w == t.name));
    }
}

fn mode_from(args: &Args) -> ui_core::Mode {
    args.get("mode")
        .and_then(ui_core::Mode::parse)
        .unwrap_or_default()
}

fn tool_chars(args: &Args) -> usize {
    // 0 仍表示“显式关闭截断”，但不能是默认值：默认关闭会让一次 8MiB
    // 的工具输出原样常驻 history。2000 与 Agent::new 的内核默认一致；
    // 需要完整上下文时显式 `--tool-chars 0`。
    args.get("tool-chars")
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000)
}

/// 上下文折叠策略：`--compact-tokens N`（0 = 关闭）、`--keep-recent K`。
fn compact_policy(args: &Args) -> CompactPolicy {
    let mut p = CompactPolicy::default();
    if let Some(v) = args.get("compact-tokens").and_then(|v| v.parse::<u64>().ok()) {
        p.max_history_tokens = if v == 0 { u64::MAX } else { v };
    }
    if let Some(v) = args.get("keep-recent").and_then(|v| v.parse::<usize>().ok()) {
        p.keep_recent = v.max(2);
    }
    p
}

fn limits_from(args: &Args) -> Limits {
    let mut limits = Limits::default();
    if let Some(v) = args.get("max-tokens").and_then(|v| v.parse().ok()) {
        limits.tokens = v;
    }
    if let Some(v) = args.get("max-calls").and_then(|v| v.parse().ok()) {
        limits.tool_calls = v;
    }
    if let Some(v) = args.get("max-wall-ms").and_then(|v| v.parse().ok()) {
        limits.wall_ms = v;
    }
    if let Some(v) = args.get("max-usd").and_then(|v| v.parse().ok()) {
        limits.usd = v;
    }
    if let Some(v) = args.get("usd-per-mtok").and_then(|v| v.parse().ok()) {
        limits.usd_per_mtok = v;
    }
    // 长任务需要更多步：默认 24 步对"跑一小时"远远不够
    if let Some(v) = args.get("max-steps").and_then(|v| v.parse().ok()) {
        limits.max_steps = v;
    }
    limits
}

fn cmd_run(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    // Chinese text passed through a Windows -> WSL argv bridge can arrive mangled,
    // so a task file is the reliable path for non-ASCII prompts.
    let task = if let Some(t) = args.get("task") {
        t.to_string()
    } else if let Some(path) = args.get("task-file") {
        std::fs::read_to_string(path).map_err(|e| format!("读任务文件失败: {}", e))?
    } else {
        return Err("缺少 --task 或 --task-file".to_string());
    };
    let limits = limits_from(args);
    // 记忆检索用**原始任务**：knowledge 预取会把卡片正文拼进首个提示词，
    // 那不该改变「这一轮该命中哪条记忆」。
    let memory_input = task.clone();

    let model = build_model(args)?;

    let jp = resolve_journal(args, &ws);
    let journal = Journal::open(&jp).map_err(|e| e.to_string())?;
    println!("workspace {}", ws.display());
    println!("journal   {}", jp.display());
    // 审阅需要基线；显式开启，避免每次 run 都对大仓库做全量快照。
    if args.flag("checkpoint") {
        let cp = ui_core::checkpoint::create(&ws, "before-run")?;
        println!(
            "checkpoint {} · {} files",
            cp.str_at("id").unwrap_or("?"),
            cp.u64_at("files").unwrap_or(0)
        );
    }

    let mut agent = Agent::new(journal, model, ws.clone(), limits);
    // 默认 `ui run` 仍是一次独立运行；只有显式 `--session` 才恢复该会话历史。
    if args.get("session").is_some() {
        restore_session_history(&mut agent, &jp)?;
    }
    // run 边界：Agent::new 之后第一件事就是划它。晚一步，`--skills` 预加载
    // 的正文和 knowledge 命中会被 reset 抹掉；早一步也没有意义（新对象本来就是
    // 干净状态），真正要紧的是别在 `agent.cancel.clone()` 之后才划——那样
    // watch-parent 线程抓到的是**上一个**令牌，取消永远传不进来。
    agent.begin_run();
    agent.memory_query = Some(memory_input);
    agent.compact = compact_policy(args);
    agent.tool_chars = tool_chars(args);
    if args.flag("no-auto-checkpoint") {
        agent.auto_checkpoint = false;
    }
    agent.mode = mode_from(args);
    attach_mcp(&mut agent, args)?;
    attach_plugins(&mut agent, args)?;
    apply_tool_filter(&mut agent, args);
    if args.flag("no-env-scrub") {
        agent.env_scrub = false;
    }
    if args.flag("no-redact") {
        // 关的是**落账前**那一道脱敏：日志里会留下原文，并且每轮开始先写
        // 一条 privacy/redact-off，三个月后翻日志的人能看出这段是原文。
        agent.set_redact_output(false);
    }
    if args.flag("no-memory") {
        agent.memory_enabled = false;
        agent.memory_hits.clear();
    }
    if let Some(text) = args.get("append-system-prompt") {
        ui_core::prompt::set_extra_system(text);
    }
    // `--skills a,b`：并行子代理或脚本化调用时，直接在开场前预加载
    preload_skills(&mut agent, &ws, &skill_list(args))?;
    // K2：任务开始前做一次**确定性**知识预取。
    // 命中 → 正文拼进首个提示词前 + 每张卡一条 knowledge/use；未命中 → 行为与之前完全一致。
    let task = match knowledge::prefetch_for_run(&mut agent, &ws, &task, knowledge::MAX_PREFETCH)? {
        Some(block) => format!("{}\n任务：{}", block, task),
        None => task,
    };
    let mut hooks = ConsoleHooks::new(args.flag("yes"), false);

    // 取消传播：父进程（或包装它的工作流）一死，stdin 管道关闭 → 读到 EOF → 自我取消
    if args.flag("watch-parent") {
        let cancel = agent.cancel.clone();
        std::thread::spawn(move || {
            let mut stdin = io::stdin();
            let mut buf = [0u8; 1];
            let _ = stdin.read(&mut buf);
            // 父进程死亡是不可逆的：用硬取消，避免被下一轮的 reset 抹掉
            cancel.cancel_hard();
        });
    }

    let report = agent
        .run_turn(&task, &mut hooks)
        .map_err(|e| e.to_string())?;

    println!("\n{}", render::status_line(agent.model.name(), &agent.budget.snapshot(), &limits));
    println!(
        "结束原因 {} · {} 步 · {} 次工具 · 约 {} tokens",
        report.reason, report.steps, report.tool_calls, report.tokens
    );
    if !agent.memory_hits.is_empty() {
        println!(
            "\n记忆注入 {} 条（正文进快照 memory 段，已写 memory/used）：",
            agent.memory_hits.len()
        );
        for hit in &agent.memory_hits {
            println!(
                "  {} · {} · {} tok · {}",
                hit.entry.id,
                hit.reason,
                ui_core::memory::budget_tokens(&hit.entry.text),
                hit.entry.text
            );
        }
    }
    // 并行子代理靠这个文件把「答案」带回父进程（stdout 里混着日志，不可靠）
    if let Some(path) = args.get("result-file") {
        if let Some(parent) = Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(path, &report.final_text).map_err(|e| format!("写结果文件失败: {}", e))?;
    }
    // 只有真正 done 才算成功：empty_reply / budget / ceiling 这些都得让脚本能看见。
    // （之前一律 Ok(0)，评测工具因此误判过一轮。）
    if report.reason == "done" {
        Ok(0)
    } else {
        eprintln!("未正常完成：{}", report.reason);
        Ok(1)
    }
}

/// 用另一个模式重判历史：当时放行的动作，换成严格模式会不会被拦？
///
/// 这是「模式只改严格度、不改日志格式」带来的能力：日志里有决策级的效果记录，
/// 所以可以事后重判。
fn cmd_audit(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let jp = resolve_journal(args, &ws);
    let events = Journal::load(&jp).map_err(|e| e.to_string())?;
    let target = ui_core::Mode::parse(args.get("as").unwrap_or("strict"))
        .ok_or("--as 只支持 quiet | standard | strict")?;
    let policy = Policy::new(Policy::defaults(&ws), &ws);

    let mut actions = 0usize;
    let mut rows: Vec<Json> = Vec::new();
    let mut escalated = 0usize;
    let mut relaxed = 0usize;

    for e in &events {
        if e.str_at("kind") != Some("tool/request") {
            continue;
        }
        let effects: Vec<Effect> = e
            .arr_at("effects")
            .map(|a| a.iter().filter_map(Effect::from_json).collect())
            .unwrap_or_default();
        if effects.is_empty() {
            continue;
        }
        actions += 1;
        let base = policy.decide(&effects).decision;
        let judged = effects
            .iter()
            .map(|f| target.adjust(&f.kind, base))
            .fold(Decision::Allow, |acc, d| {
                if d.severity() > acc.severity() {
                    d
                } else {
                    acc
                }
            });
        if judged == base {
            continue;
        }
        if judged.severity() > base.severity() {
            escalated += 1;
        } else {
            relaxed += 1;
        }
        rows.push(Json::obj(vec![
            ("seq", Json::from(e.u64_at("seq").unwrap_or(0))),
            ("tool", Json::from(e.str_at("tool").unwrap_or(""))),
            ("effect", Json::from(effects[0].describe())),
            ("was", Json::from(base.name())),
            ("now", Json::from(judged.name())),
        ]));
    }

    if args.flag("json") {
        println!(
            "{}",
            Json::obj(vec![
                ("journal", Json::from(jp.to_string_lossy().to_string())),
                ("as", Json::from(target.name())),
                ("actions", Json::from(actions)),
                ("escalated", Json::from(escalated)),
                ("relaxed", Json::from(relaxed)),
                ("rows", Json::Arr(rows)),
            ])
            .to_compact()
        );
        return Ok(0);
    }

    println!("日志 {}", jp.display());
    println!("动作 {} 个 · 按 {} 重判", actions, target.name());
    if rows.is_empty() {
        println!("\n结论：这个模式下的判定完全一致，没有任何动作会被改变。");
        return Ok(0);
    }
    println!(
        "\n{} 个动作会被收紧（当时放行、这个模式下要问你），{} 个会被放宽：",
        escalated, relaxed
    );
    for row in &rows {
        println!(
            "  seq {:<5} {:<10} {:<44} {} → {}",
            row.u64_at("seq").unwrap_or(0),
            row.str_at("tool").unwrap_or(""),
            row.str_at("effect").unwrap_or(""),
            row.str_at("was").unwrap_or(""),
            row.str_at("now").unwrap_or("")
        );
    }
    Ok(0)
}

// ── 科研专项 · 实验包（A：可复现；C：数据血缘） ──────────────────────────
//
// 一个实验包 = 产物 + 哈希 + 血缘（哪次调用/哪条命令产出）+ 环境指纹 + 日志副本。
// 规则只有一条：**没有产物就没有结论**——包里没东西，报告就是空的。

fn sha256_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(ui_core::sha256::hex(&bytes))
}

fn env_fingerprint() -> Json {
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
    let os = read("/etc/os-release")
        .lines()
        .find(|l| l.starts_with("PRETTY_NAME="))
        .map(|l| l.trim_start_matches("PRETTY_NAME=").trim_matches('"').to_string())
        .unwrap_or_default();
    let kernel = std::process::Command::new("uname")
        .arg("-r")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let cpus = read("/proc/cpuinfo")
        .lines()
        .filter(|l| l.starts_with("processor"))
        .count();
    let python = std::process::Command::new("python3")
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "absent".to_string());
    Json::obj(vec![
        ("os", Json::from(os.as_str())),
        ("kernel", Json::from(kernel.as_str())),
        ("arch", Json::from(std::env::consts::ARCH)),
        ("cpus", Json::from(cpus)),
        ("python", Json::from(python.as_str())),
    ])
}

fn cmd_lab(args: &Args) -> Result<i32, String> {
    match args.sub(0).unwrap_or("") {
        "pack" => lab_pack(args),
        "verify" => lab_verify(args),
        "table" => lab_table(args),
        "claim" => lab_claim(args),
        other => Err(format!(
            "未知子命令 `{}`（ui lab pack | ui lab verify <bundle>）",
            other
        )),
    }
}

fn lab_pack(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let jp = resolve_journal(args, &ws);
    let events = Journal::load(&jp).map_err(|e| e.to_string())?;
    let name = args.get("name").unwrap_or("exp").to_string();
    let bundle = match args.get("out") {
        Some(p) => PathBuf::from(p),
        None => ws.join("experiments").join(&name),
    };
    let art_dir = bundle.join("artifacts");
    std::fs::create_dir_all(&art_dir).map_err(|e| e.to_string())?;

    // 血缘起点：从日志里找出「被写出来的文件」，记下是哪次调用写的
    let mut produced: std::collections::BTreeMap<String, Json> = Default::default();
    let mut commands: Vec<Json> = Vec::new();
    for e in &events {
        if e.str_at("kind") != Some("tool/request") {
            continue;
        }
        let tool = e.str_at("tool").unwrap_or("");
        let decision = e.str_at("decision").unwrap_or("");
        let seq = e.u64_at("seq").unwrap_or(0);
        if tool == "shell" {
            commands.push(Json::obj(vec![
                ("seq", Json::from(seq)),
                ("command", Json::from(e.get("args").and_then(|a| a.str_at("command")).unwrap_or(""))),
                ("decision", Json::from(decision)),
            ]));
        }
        if decision == "deny" {
            continue;
        }
        for f in e.arr_at("effects").cloned().unwrap_or_default() {
            let kind = f.str_at("kind").unwrap_or("");
            if kind != "fs.write" && kind != "fs.edit" {
                continue;
            }
            let target = f.str_at("target").unwrap_or("");
            let p = Path::new(target);
            if !p.starts_with(&ws) || !p.is_file() {
                continue;
            }
            if p.strip_prefix(&ws)
                .map(|r| r.to_string_lossy().contains(".ui"))
                .unwrap_or(false)
            {
                continue;
            }
            if let Ok(rel) = p.strip_prefix(&ws) {
                produced.insert(
                    rel.to_string_lossy().replace('\\', "/"),
                    Json::obj(vec![
                        ("seq", Json::from(seq)),
                        ("tool", Json::from(tool)),
                        ("provenance", Json::from("journal")),
                    ]),
                );
            }
        }
    }

    // 运行前被读过的文件是输入，不是产物。仅靠 wall-clock mtime 判定时，
    // WSL2 墙钟回跳会让“运行前输入”看起来比首事件还新，随后 inputs 被
    // produced 抢先吞掉（2026-11-23 的真实 flake）。用 fs/read 记录的
    // path+sha 做一次身份判定：内容没变就绝对不能记成产物。
    let mut read_hashes: std::collections::BTreeMap<String, String> = Default::default();
    for e in &events {
        if e.str_at("kind") != Some("fs/read") {
            continue;
        }
        if let (Some(path), Some(hash)) = (e.str_at("path"), e.str_at("sha256")) {
            read_hashes.insert(path.to_string(), hash.to_string());
        }
    }

    // 脚本自己写出来的文件（shell 跑实验的产物）没有 fs.write 事件。
    // 用「首事件之后被改过」做**弱血缘**，并在清单里明确标出来——不假装是强证据。
    let run_start = events.first().and_then(|e| e.u64_at("ts")).unwrap_or(0);
    let mut from_mtime: Vec<String> = Vec::new();
    fn walk_fresh(dir: &Path, root: &Path, start_ms: u64, out: &mut Vec<String>) {
        let rd = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return,
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if matches!(
                name.as_str(),
                ".ui" | "target" | "node_modules" | ".git" | "experiments" | "bundles" | "claims.json"
            ) {
                continue;
            }
            let p = e.path();
            if p.is_dir() {
                walk_fresh(&p, root, start_ms, out);
                continue;
            }
            let mtime = e
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            if mtime >= start_ms {
                if let Ok(rel) = p.strip_prefix(root) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    walk_fresh(&ws, &ws, run_start, &mut from_mtime);
    for rel in from_mtime {
        if produced.contains_key(&rel) {
            continue;
        }
        // 读入未变的文件是输入；即使 mtime 因墙钟回跳落到 run_start 之后，
        // 也不能把它升级成产物。
        if let Some(hash) = read_hashes.get(&rel) {
            let current = sha256_file(&ws.join(&rel)).unwrap_or_default();
            if current == *hash {
                continue;
            }
        }
        produced.insert(
            rel,
            Json::obj(vec![
                ("seq", Json::from(0)),
                ("tool", Json::from("(脚本产出)")),
                ("provenance", Json::from("mtime")),
            ]),
        );
    }

    // 复制产物 + 记哈希
    let mut manifest: Vec<Json> = Vec::new();
    for (rel, origin) in &produced {
        let src = ws.join(rel);
        // 产物目录在 workspace 里就叫 artifacts/；复制进 bundle 的 artifacts/ 时要剥掉这层前缀，
        // 否则会变成 artifacts/artifacts/metrics.json，对照表按目录扫数值就扫不到。
        let inner = rel.strip_prefix("artifacts/").unwrap_or(rel.as_str());
        let dst = art_dir.join(inner);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::copy(&src, &dst).map_err(|e| format!("复制 {} 失败: {}", rel, e))?;
        let bytes = std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
        let hash = sha256_file(&dst).unwrap_or_default();
        manifest.push(Json::obj(vec![
            ("path", Json::from(rel.as_str())),
            ("sha256", Json::from(hash.as_str())),
            ("bytes", Json::from(bytes)),
            ("producedBy", origin.clone()),
        ]));
    }

    // 输入血缘：日志里的 fs/read 事件（Agent 在执行前记的）。
    // **产物自己不算输入**：agent 常会读回自己刚写的 metrics.json 做确认，
    // 如果把它记成输入，多 seed 对照表就会因为"同一路径哈希不同"拒绝求均值——
    // 而产物理所当然每个 seed 都不同。这是真模型跑出来的问题。
    let mut inputs: Vec<Json> = Vec::new();
    for e in &events {
        if e.str_at("kind") != Some("fs/read") {
            continue;
        }
        let path = e.str_at("path").unwrap_or("");
        if produced.contains_key(path) {
            continue;
        }
        inputs.push(Json::obj(vec![
            ("path", Json::from(path)),
            ("sha256", Json::from(e.str_at("sha256").unwrap_or(""))),
            ("bytes", Json::from(e.u64_at("bytes").unwrap_or(0))),
            (
                "readBy",
                Json::obj(vec![
                    ("seq", Json::from(e.u64_at("seq").unwrap_or(0))),
                    ("tool", Json::from(e.str_at("tool").unwrap_or(""))),
                ]),
            ),
        ]));
    }

    // 日志副本：审计要看原始记录，不看我们的转述
    let journal_copy = bundle.join("journal.jsonl");
    let _ = std::fs::copy(&jp, &journal_copy);

    let meta = Json::obj(vec![
        ("name", Json::from(name.as_str())),
        ("createdMs", Json::from(ui_core::time::now_ms())),
        ("workspace", Json::from(ws.to_string_lossy().to_string())),
        ("journalEvents", Json::from(events.len())),
        ("env", env_fingerprint()),
        ("commands", Json::Arr(commands.clone())),
    ]);

    let manifest_text = manifest
        .iter()
        .map(|m| m.to_compact())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(bundle.join("manifest.jsonl"), format!("{}\n", manifest_text))
        .map_err(|e| e.to_string())?;
    let inputs_text = inputs
        .iter()
        .map(|m| m.to_compact())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(bundle.join("inputs.jsonl"), format!("{}\n", inputs_text))
        .map_err(|e| e.to_string())?;

    // 结论声明跟着包走：校验时逐条和产物对账
    let mut claims: Vec<Json> = Vec::new();
    let claims_src = ws.join("claims.json");
    if let Ok(text) = std::fs::read_to_string(&claims_src) {
        if let Ok(v) = json::parse(&text) {
            claims = v.arr_at("claims").cloned().unwrap_or_default();
            let _ = std::fs::copy(&claims_src, bundle.join("claims.json"));
        }
    }
    std::fs::write(bundle.join("meta.json"), meta.to_pretty()).map_err(|e| e.to_string())?;

    let mut report = String::new();
    report.push_str(&format!("# 实验包 · {}\n\n", name));
    report.push_str(&format!("- 工作区：{}\n", ws.display()));
    report.push_str(&format!(
        "- 环境：{} · kernel {} · {} 核 · {}\n",
        meta.get("env").and_then(|e| e.str_at("os")).unwrap_or("?"),
        meta.get("env").and_then(|e| e.str_at("kernel")).unwrap_or("?"),
        meta.get("env").and_then(|e| e.u64_at("cpus")).unwrap_or(0),
        meta.get("env").and_then(|e| e.str_at("python")).unwrap_or("?")
    ));
    report.push_str(&format!("- 日志事件：{}\n\n", events.len()));
    report.push_str("## 产物\n\n| 文件 | 字节 | sha256 | 来源 |\n|---|---|---|---|\n");
    for m in &manifest {
        let origin = m.get("producedBy").cloned().unwrap_or(Json::Null);
        report.push_str(&format!(
            "| {} | {} | {}… | seq {} · {} |\n",
            m.str_at("path").unwrap_or(""),
            m.u64_at("bytes").unwrap_or(0),
            &m.str_at("sha256").unwrap_or("")[..12.min(m.str_at("sha256").unwrap_or("").len())],
            origin.u64_at("seq").unwrap_or(0),
            origin.str_at("tool").unwrap_or("")
        ));
    }
    if !inputs.is_empty() {
        report.push_str("\n## 读过的输入\n\n| 文件 | 字节 | sha256 | 读取于 |\n|---|---|---|---|\n");
        for m in &inputs {
            let by = m.get("readBy").cloned().unwrap_or(Json::Null);
            report.push_str(&format!(
                "| {} | {} | {}… | seq {} · {} |\n",
                m.str_at("path").unwrap_or(""),
                m.u64_at("bytes").unwrap_or(0),
                &m.str_at("sha256").unwrap_or("")[..12.min(m.str_at("sha256").unwrap_or("").len())],
                by.u64_at("seq").unwrap_or(0),
                by.str_at("tool").unwrap_or("")
            ));
        }
    }
    if !claims.is_empty() {
        report.push_str("\n## 声明的结论\n\n| 结论 | 值 |\n|---|---|\n");
        for c in &claims {
            report.push_str(&format!(
                "| {} | {} |\n",
                c.str_at("name").unwrap_or(""),
                c.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0)
            ));
        }
    }
    if !commands.is_empty() {
        report.push_str("\n## 跑过的命令\n\n");
        for c in &commands {
            report.push_str(&format!(
                "- seq {} `{}` ({})\n",
                c.u64_at("seq").unwrap_or(0),
                c.str_at("command").unwrap_or(""),
                c.str_at("decision").unwrap_or("")
            ));
        }
    }
    report.push_str(
        "\n## 怎么验证\n\n```bash\nui lab verify <这个目录>\n```\n\n\
         包里没有的产物，报告里就不该有它的结论。\n",
    );
    std::fs::write(bundle.join("report.md"), report).map_err(|e| e.to_string())?;

    println!("实验包 {}", bundle.display());
    println!("产物 {} 个 · 命令 {} 条 · 日志 {} 事件", manifest.len(), commands.len(), events.len());
    for m in &manifest {
        println!(
            "  {}  {}  {}…",
            m.str_at("path").unwrap_or(""),
            m.u64_at("bytes").unwrap_or(0),
            &m.str_at("sha256").unwrap_or("")[..12.min(m.str_at("sha256").unwrap_or("").len())]
        );
    }
    println!("\n验证：ui lab verify {}", bundle.display());
    Ok(0)
}

/// 产物在包里的落盘位置。
///
/// manifest 记的是 **workspace 相对路径**（实验产物通常就叫 `artifacts/metrics.json`），
/// 而包内统一放在 `bundle/artifacts/` 下，所以拼接时必须剥掉这层前缀，
/// 否则会变成 `artifacts/artifacts/metrics.json`，校验时报「缺失」。
fn bundle_artifact_path(bundle: &std::path::Path, rel: &str) -> PathBuf {
    bundle
        .join("artifacts")
        .join(rel.strip_prefix("artifacts/").unwrap_or(rel))
}

fn lab_verify(args: &Args) -> Result<i32, String> {
    let bundle = args
        .sub(1)
        .map(PathBuf::from)
        .or_else(|| args.get("bundle").map(PathBuf::from))
        .ok_or("用法：ui lab verify <bundle 目录>")?;
    let manifest_path = bundle.join("manifest.jsonl");
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("读不到 {}: {}", manifest_path.display(), e))?;

    let mut checked = 0usize;
    let mut bad: Vec<String> = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let rec = match json::parse(line) {
            Ok(v) => v,
            Err(e) => {
                bad.push(format!("manifest 行不是 JSON: {}", e));
                continue;
            }
        };
        let rel = rec.str_at("path").unwrap_or("");
        let want = rec.str_at("sha256").unwrap_or("");
        let file = bundle_artifact_path(&bundle, rel);
        match sha256_file(&file) {
            Some(got) if got == want => checked += 1,
            Some(got) => bad.push(format!("{} 哈希不符（记录 {}…，实际 {}…）", rel, &want[..8.min(want.len())], &got[..8.min(got.len())])),
            None => bad.push(format!("{} 缺失", rel)),
        }
    }

    // 日志副本也要能自检（哈希链 + 未完成调用）
    let mut journal_ok = false;
    if let Ok((events, corrupt)) = Journal::read(bundle.join("journal.jsonl")) {
        journal_ok = corrupt.is_none();
        if !journal_ok {
            bad.push("journal.jsonl 链不完整".to_string());
        } else {
            let inc = analyze_incomplete(&events);
            if inc.open_turn.is_some() || !inc.open_tools.is_empty() {
                bad.push(format!(
                    "日志里有未完成的调用（turn={} 工具={}）",
                    inc.open_turn.is_some(),
                    inc.open_tools.len()
                ));
            }
        }
    } else {
        bad.push("读不到 journal.jsonl".to_string());
    }

    // 输入血缘：数据换了版本，结果就不成立。工作区不在本机时跳过（包是可移植的）。
    let mut input_checked = 0usize;
    let mut input_skipped = false;
    let meta_ws = std::fs::read_to_string(bundle.join("meta.json"))
        .ok()
        .and_then(|t| json::parse(&t).ok())
        .and_then(|m| m.str_at("workspace").map(PathBuf::from));
    let ws = args.get("workspace").map(PathBuf::from).or(meta_ws);
    match ws {
        Some(ws) if ws.is_dir() => {
            if let Ok(text) = std::fs::read_to_string(bundle.join("inputs.jsonl")) {
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    let rec = match json::parse(line) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let rel = rec.str_at("path").unwrap_or("");
                    let want = rec.str_at("sha256").unwrap_or("");
                    match sha256_file(&ws.join(rel)) {
                        Some(got) if got == want => input_checked += 1,
                        Some(_) => bad.push(format!("输入 {} 已改变（数据版本和当初不一样了）", rel)),
                        None => bad.push(format!("输入 {} 在日志里出现过，但现在找不到", rel)),
                    }
                }
            }
        }
        _ => input_skipped = true,
    }

    // 结论对账：声明的每个数字都必须能在产物里找到。
    // 绑定语法是 `文件.json#/字段/数组下标`（解析与核验都在 lab.rs）；旧格式
    // （只写名字）仍能过，但会被标成 loose 并打印警告——那正是「别的字段数值
    // 接近 → 巧合通过」的来源。
    let claim_report = std::fs::read_to_string(bundle.join("claims.json"))
        .ok()
        .and_then(|text| json::parse(&text).ok())
        .map(|doc| lab::verify_claims(&bundle.join("artifacts"), &doc))
        .unwrap_or_default();
    bad.extend(claim_report.issues.iter().cloned());

    if args.flag("json") {
        let doc = Json::obj(vec![
            ("ok", Json::Bool(bad.is_empty())),
            ("bundle", Json::from(bundle.to_string_lossy().to_string())),
            ("artifacts", Json::from(checked)),
            ("inputs", Json::from(input_checked)),
            ("inputsSkipped", Json::Bool(input_skipped)),
            ("journalOk", Json::Bool(journal_ok)),
            ("claimsOk", Json::from(claim_report.ok)),
            ("claimsTotal", Json::from(claim_report.total)),
            ("loose", Json::Bool(claim_report.loose)),
            (
                "warnings",
                Json::Arr(
                    claim_report
                        .warnings
                        .iter()
                        .map(|w| Json::from(w.as_str()))
                        .collect(),
                ),
            ),
            (
                "issues",
                Json::Arr(bad.iter().map(|b| Json::from(b.as_str())).collect()),
            ),
            ("claims", claim_report.claims_json()),
        ]);
        // stdout 只留一行 JSON；人能读的摘要、警告与问题走 stderr。
        println!("{}", doc.to_compact());
        eprintln!(
            "实验包 {} · 产物 {} · 输入 {} · 结论 {}/{} 对得上 · 日志链 {}{}",
            bundle.display(),
            checked,
            input_checked,
            claim_report.ok,
            claim_report.total,
            if journal_ok { "完整" } else { "有问题" },
            if input_skipped { "（工作区不在本机，跳过输入校验）" } else { "" }
        );
        for w in &claim_report.warnings {
            eprintln!("{}", w);
        }
        for b in &bad {
            eprintln!("  - {}", b);
        }
        return Ok(if bad.is_empty() { 0 } else { 3 });
    }

    println!("实验包 {}", bundle.display());
    println!(
        "产物 {} 个校验通过 · 输入 {} 个仍一致 · 结论 {}/{} 对得上 · 日志链 {}{}",
        checked,
        input_checked,
        claim_report.ok,
        claim_report.total,
        if journal_ok { "完整" } else { "有问题" },
        if input_skipped { "（工作区不在本机，跳过输入校验）" } else { "" }
    );
    for w in &claim_report.warnings {
        println!("{}", w);
    }
    if bad.is_empty() {
        println!("\n结论：包是自洽的——产物哈希、日志链、无未完成调用都对得上。");
        Ok(0)
    } else {
        println!("\n发现 {} 个问题：", bad.len());
        for b in &bad {
            println!("  - {}", b);
        }
        Ok(3)
    }
}

/// 多 seed 聚合：把多个实验包合成一张表。
///
/// 顺序很重要：**先验输入一致，再谈数值聚合**——数据版本不同的实验，
/// 放在一张表里求均值是学术不端的温床。
fn lab_table(args: &Args) -> Result<i32, String> {
    let dirs: Vec<PathBuf> = args
        .rest
        .iter()
        .skip(1)
        .map(PathBuf::from)
        .collect();
    if dirs.is_empty() {
        return Err("用法：ui lab table <bundle1> <bundle2> [...] [--json]".to_string());
    }

    struct Bundle {
        name: String,
        env: Json,
        inputs: std::collections::BTreeMap<String, String>,
        numbers: std::collections::BTreeMap<String, f64>,
    }

    fn collect_numbers(value: &Json, prefix: &str, out: &mut std::collections::BTreeMap<String, f64>) {
        match value {
            Json::Num(n) => {
                out.insert(prefix.to_string(), *n);
            }
            Json::Obj(items) => {
                for (k, v) in items {
                    let key = if prefix.is_empty() { k.clone() } else { format!("{}.{}", prefix, k) };
                    collect_numbers(v, &key, out);
                }
            }
            _ => {}
        }
    }

    let mut bundles: Vec<Bundle> = Vec::new();
    for dir in &dirs {
        let meta = std::fs::read_to_string(dir.join("meta.json"))
            .map_err(|e| format!("{} 读不到 meta.json: {}", dir.display(), e))?;
        let meta = json::parse(&meta).map_err(|e| format!("{} meta.json 不是 JSON: {}", dir.display(), e))?;
        let name = meta
            .str_at("name")
            .map(str::to_string)
            .unwrap_or_else(|| dir.file_name().unwrap_or_default().to_string_lossy().to_string());
        let env = meta.get("env").cloned().unwrap_or(Json::Null);

        let mut inputs = std::collections::BTreeMap::new();
        if let Ok(text) = std::fs::read_to_string(dir.join("inputs.jsonl")) {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                if let Ok(rec) = json::parse(line) {
                    if let (Some(p), Some(h)) = (rec.str_at("path"), rec.str_at("sha256")) {
                        inputs.insert(p.to_string(), h.to_string());
                    }
                }
            }
        }

        // 只有 .json 产物参与数值聚合；别的类型（图、模型权重）不猜。
        // 递归扫：产物可能在 artifacts/ 的子目录里。
        fn scan_json(dir: &std::path::Path, numbers: &mut std::collections::BTreeMap<String, f64>) {
            let rd = match std::fs::read_dir(dir) {
                Ok(r) => r,
                Err(_) => return,
            };
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    scan_json(&p, numbers);
                    continue;
                }
                if !p.extension().map(|e| e == "json").unwrap_or(false) {
                    continue;
                }
                if let Ok(text) = std::fs::read_to_string(&p) {
                    if let Ok(v) = json::parse(&text) {
                        collect_numbers(&v, "", numbers);
                    }
                }
            }
        }
        let mut numbers = std::collections::BTreeMap::new();
        scan_json(&dir.join("artifacts"), &mut numbers);

        bundles.push(Bundle { name, env, inputs, numbers });
    }

    // 输入一致性：同一个文件在不同包里哈希必须相同。
    // 产物路径一律不算输入（哪怕旧包里漏进了 inputs.jsonl）：它们每个 seed 本来就不同。
    let mut all_inputs: std::collections::BTreeMap<String, Vec<(String, String)>> = Default::default();
    for b in &bundles {
        for (path, hash) in &b.inputs {
            if path.starts_with("artifacts/") {
                continue;
            }
            all_inputs
                .entry(path.clone())
                .or_default()
                .push((b.name.clone(), hash.clone()));
        }
    }
    let mut input_rows: Vec<Json> = Vec::new();
    let mut drift = 0usize;
    for (path, versions) in &all_inputs {
        let mut uniq: Vec<&str> = versions.iter().map(|(_, h)| h.as_str()).collect();
        uniq.sort_unstable();
        uniq.dedup();
        let consistent = uniq.len() == 1;
        if !consistent {
            drift += 1;
        }
        input_rows.push(Json::obj(vec![
            ("path", Json::from(path.as_str())),
            ("consistent", Json::Bool(consistent)),
            ("versions", Json::from(uniq.len())),
            (
                "covered",
                Json::from(versions.len()),
            ),
        ]));
    }

    // 数值聚合：只聚合出现 ≥2 次的指标
    let mut keys: Vec<String> = Vec::new();
    for b in &bundles {
        for k in b.numbers.keys() {
            if !keys.contains(k) {
                keys.push(k.clone());
            }
        }
    }
    // seed / id 是标识不是指标，别混进均值里
    const NOT_METRICS: &[&str] = &["seed", "id", "index", "idx", "n", "count"];
    keys.retain(|k| !NOT_METRICS.contains(&k.as_str()));
    keys.sort();
    let mut metric_rows: Vec<Json> = Vec::new();
    for k in &keys {
        let mut vals: Vec<f64> = Vec::new();
        for b in &bundles {
            if let Some(v) = b.numbers.get(k) {
                vals.push(*v);
            }
        }
        if vals.len() < 2 {
            continue;
        }
        let n = vals.len() as f64;
        let mean = vals.iter().sum::<f64>() / n;
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let var = vals.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / n;
        metric_rows.push(Json::obj(vec![
            ("metric", Json::from(k.as_str())),
            ("n", Json::from(vals.len())),
            ("mean", Json::from(((mean * 10000.0).round()) / 10000.0)),
            ("min", Json::from(min)),
            ("max", Json::from(max)),
            ("std", Json::from(((var.sqrt() * 10000.0).round()) / 10000.0)),
            (
                "values",
                Json::Arr(vals.iter().map(|v| Json::from(*v)).collect()),
            ),
        ]));
    }

    let envs: Vec<Json> = bundles
        .iter()
        .map(|b| {
            Json::obj(vec![
                ("bundle", Json::from(b.name.as_str())),
                ("os", Json::from(b.env.str_at("os").unwrap_or("?"))),
                ("kernel", Json::from(b.env.str_at("kernel").unwrap_or("?"))),
                ("cpus", Json::from(b.env.u64_at("cpus").unwrap_or(0))),
                ("python", Json::from(b.env.str_at("python").unwrap_or("?"))),
            ])
        })
        .collect();

    let env_same = {
        let first = envs[0].clone();
        envs.iter().all(|e| {
            e.str_at("os") == first.str_at("os")
                && e.str_at("kernel") == first.str_at("kernel")
                && e.u64_at("cpus") == first.u64_at("cpus")
        })
    };

    if args.flag("json") {
        println!(
            "{}",
            Json::obj(vec![
                ("bundles", Json::from(bundles.len())),
                ("inputDrift", Json::from(drift)),
                ("envConsistent", Json::Bool(env_same)),
                ("inputs", Json::Arr(input_rows)),
                ("metrics", Json::Arr(metric_rows)),
                ("env", Json::Arr(envs)),
            ])
            .to_compact()
        );
        return Ok(if drift == 0 { 0 } else { 3 });
    }

    println!("# 对照表 · {} 个实验包\n", bundles.len());
    println!("| 包 | 输入 | JSON 指标 |");
    println!("|---|---|---|");
    for b in &bundles {
        println!("| {} | {} | {} |", b.name, b.inputs.len(), b.numbers.len());
    }

    println!("\n## 输入一致性\n");
    if input_rows.is_empty() {
        println!("（包里没有输入记录）");
    } else {
        println!("| 文件 | 一致 | 版本数 | 覆盖包数 |");
        println!("|---|---|---|---|");
        for r in &input_rows {
            println!(
                "| {} | {} | {} | {} |",
                r.str_at("path").unwrap_or(""),
                if r.bool_at("consistent") == Some(true) { "✅" } else { "❌" },
                r.u64_at("versions").unwrap_or(0),
                r.u64_at("covered").unwrap_or(0)
            );
        }
    }

    println!("\n## 指标\n");
    if metric_rows.is_empty() {
        println!("（没有在两个以上包里都出现的数值指标）");
    } else {
        println!("| 指标 | n | 均值 | 标准差 | 最小 | 最大 | 各次 |");
        println!("|---|---|---|---|---|---|---|");
        for r in &metric_rows {
            let vals = r
                .arr_at("values")
                .map(|a| a.iter().filter_map(|v| v.as_f64()).map(|v| format!("{:.4}", v)).collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            println!(
                "| {} | {} | {:.4} | {:.4} | {:.4} | {:.4} | {} |",
                r.str_at("metric").unwrap_or(""),
                r.u64_at("n").unwrap_or(0),
                r.get("mean").and_then(|v| v.as_f64()).unwrap_or(0.0),
                r.get("std").and_then(|v| v.as_f64()).unwrap_or(0.0),
                r.get("min").and_then(|v| v.as_f64()).unwrap_or(0.0),
                r.get("max").and_then(|v| v.as_f64()).unwrap_or(0.0),
                vals
            );
        }
    }

    println!("\n## 环境\n");
    println!("| 包 | OS | kernel | 核数 | Python |");
    println!("|---|---|---|---|---|");
    for e in &envs {
        println!(
            "| {} | {} | {} | {} | {} |",
            e.str_at("bundle").unwrap_or(""),
            e.str_at("os").unwrap_or("?"),
            e.str_at("kernel").unwrap_or("?"),
            e.u64_at("cpus").unwrap_or(0),
            e.str_at("python").unwrap_or("?")
        );
    }

    println!();
    if drift > 0 {
        println!("结论：有 {} 个输入在不同包里版本不一致——**这张表不能求均值**。", drift);
        Ok(3)
    } else if !env_same {
        println!("结论：输入一致，但环境不同（OS/kernel/核数）；跨机器比数值要谨慎。");
        Ok(0)
    } else {
        println!("结论：输入与环境都一致，这张表上的均值可以作为结论。");
        Ok(0)
    }
}

/// 声明一条结论（写进工作区的 claims.json）。
///
/// 约定：**结论必须显式声明**，不能从自由文本里猜数字——猜错了比不查更糟。
fn lab_claim(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let name = args.get("name").ok_or("用法：ui lab claim --name accuracy --value 0.72")?;
    let value: f64 = args
        .get("value")
        .ok_or("用法：ui lab claim --name accuracy --value 0.72")?
        .parse()
        .map_err(|_| "--value 必须是数字")?;
    let path = args
        .get("out")
        .map(PathBuf::from)
        .unwrap_or_else(|| ws.join("claims.json"));

    let mut list: Vec<Json> = if let Ok(text) = std::fs::read_to_string(&path) {
        json::parse(&text)
            .ok()
            .and_then(|v| v.arr_at("claims").cloned())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    list.retain(|c| c.str_at("name") != Some(name));
    list.push(Json::obj(vec![
        ("name", Json::from(name)),
        ("value", Json::from(value)),
    ]));
    let doc = Json::obj(vec![("claims", Json::Arr(list.clone()))]);
    std::fs::write(&path, doc.to_pretty()).map_err(|e| e.to_string())?;
    println!("已声明 {} 条结论 → {}", list.len(), path.display());
    for c in &list {
        println!("  {} = {}", c.str_at("name").unwrap_or(""), c.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0));
    }
    Ok(0)
}

/// `ui prompt`：打印当前会发给模型的快照（调试提示词用，也用来验收 A3）。
fn cmd_prompt(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    if let Some(text) = args.get("append-system-prompt") {
        ui_core::prompt::set_extra_system(text);
    }
    let mut tools = ui_core::tools::registry();
    if args.flag("no-tools") {
        tools.clear();
    }
    if let Some(list) = args.get("tools") {
        let want: Vec<&str> = list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        tools.retain(|t| want.iter().any(|w| *w == t.name));
    }
    let snap = {
        // `--memory-query "..."`：单独渲染一次检索结果（不写事件、不跑 turn），
        // 用来回答「这个 query 会注入哪几条记忆」。没有它时快照与以前完全一样。
        let memory_text = match args.get("memory-query") {
            Some(query) => {
                let mut store = ui_core::memory::Store::open(&ws);
                store.refresh().map_err(|e| e.to_string())?;
                let budget = std::env::var("UI_MEMORY_BUDGET")
                    .ok()
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(ui_core::memory::DEFAULT_MAX_TOKENS);
                let hits = store.retrieve(
                    query,
                    &ui_core::memory::Config {
                        top_k: ui_core::memory::DEFAULT_TOP_K,
                        max_tokens: budget,
                        now_ms: ui_core::time::now_ms(),
                    },
                );
                ui_core::memory::render(&hits)
            }
            None => String::new(),
        };
        ui_core::prompt::build_snapshot(&ws, &tools, &[], None, None, "", &[], &memory_text)
    };
    if args.flag("json") {
        println!("{}", snap.to_json().to_compact());
        return Ok(0);
    }
    if args.flag("full") {
        println!("{}", snap.text);
        return Ok(0);
    }
    println!(
        "快照 {} · {} 段 · {} tok · {} 字节",
        snap.id,
        snap.sections.len(),
        snap.tokens,
        snap.bytes
    );
    for s in &snap.sections {
        println!("  {:<10} {:>7} 字节 {:>6} tok  [{}]", s.id, s.bytes, s.tokens, s.owner);
    }
    Ok(0)
}

fn cmd_resume(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let jp = resolve_journal(args, &ws);
    // 「有没有活要恢复」必须从**文件**里流式数出来，不能看 `Journal::open`
    // 留在内存里的那段尾巴：事件缓冲有上限（2000 条），大日志里崩溃现场
    // 早就被挤出去了，于是 `ui resume` 对着一个明明没收尾的日志说
    // 「没有需要恢复的工作」——而真正的对账（`resume()` 内部走 verify_stream）
    // 是看得见的。同一个问题用两套事实，假阴性就是这么长出来的。
    let stream = ui_core::stream_verify::verify_stream(&jp).map_err(|e| e.to_string())?;
    println!("journal {}", jp.display());
    if let Some(c) = &stream.corrupt {
        if !c.torn {
            // 正文被改过：链已经不是真的了，在它上面写恢复事件等于把伪造
            // 的现场坐实。先让人看，再决定要不要继续。
            println!("FAIL  日志损坏 seq={:?}: {}", c.seq, c.reason);
            println!("      未完成状态无法信任——先跑 `ui verify`，别在这里对账。");
            return Ok(2);
        }
        println!("WARN  尾部撕裂，已按最后一条完整事件对账: {}", c.reason);
    }
    println!(
        "未完成: turn={} 工具={}",
        stream.open_turn.is_some(),
        stream.open_tools.len()
    );
    if stream.open_turn.is_none() && stream.open_tools.is_empty() {
        println!("没有需要恢复的工作");
        return Ok(0);
    }

    let journal = Journal::open(&jp).map_err(|e| e.to_string())?;
    let mut agent = Agent::new(
        journal,
        Box::new(UnavailableModel),
        ws.clone(),
        limits_from(args),
    );
    let mut hooks = ConsoleHooks::new(args.flag("yes"), false);
    let report = resume(&mut agent, &mut hooks).map_err(|e| e.to_string())?;
    for line in &report.reconciled {
        println!("  已对账 {}", line);
    }
    for line in &report.needs_human {
        println!("  ⚠ 需要人确认 {}", line);
    }
    Ok(0)
}

/// 日志自检：链是否完整、有没有崩溃留下的未完成调用。调试的第一步。
fn cmd_verify(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let jp = resolve_journal(args, &ws);

    // 流式校验：内存 O(1)，不随日志大小增长。
    // 之前走 `Journal::read`（把每条事件都变成 Json 留在内存里），
    // 实测 7 万条要 80 MB —— 审计大日志时会先被自己撑死。
    let r = ui_core::stream_verify::verify_stream(&jp).map_err(|e| e.to_string())?;

    match &r.corrupt {
        Some(c) if !c.torn => {
            println!("FAIL  日志损坏 seq={:?}: {}", c.seq, c.reason);
            println!("      文件 {}", jp.display());
            return Ok(2);
        }
        Some(c) => println!("WARN  尾部撕裂，可修: {}", c.reason),
        None => println!("ok    哈希链完整"),
    }

    println!("日志  {}", jp.display());
    println!("事件  {}", r.events);
    if r.events > 0 {
        println!("序号  {}..{}", r.first_seq, r.last_seq);
    }
    println!("轮次 {}  工具调用 {}", r.turns, r.tool_starts);
    println!(
        "未完成 turn={} tools={}",
        r.open_turn.is_some(),
        r.open_tools.len()
    );
    if !r.open_tools.is_empty() {
        println!("提示  有未完成副作用，跑 `ui resume` 对账");
    }
    Ok(0)
}

/// 导出单文件 HTML 轨迹查看器：数据内嵌，双击即看，可离线转发。
fn cmd_export(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let jp = resolve_journal(args, &ws);
    let limits = limits_from(args);
    // 导出默认脱敏：这份 HTML 的定位就是「可离线转发」（尾部提示就是这么写的），
    // 而日志里装着完整提示词、工具参数和工具输出。要原样留档得显式说
    // `--raw`，让「带密钥导出」变成一个要主动选择的行为。
    let raw = args.flag("raw");

    // 先流式校验：**不导出损坏的日志**（尾部撕裂可以，正文被改不行）。
    // 这一步很便宜（30 万条 ~0.5s），但能保证后面原样拷贝的行都是可信的。
    let report = ui_core::stream_verify::verify_stream(&jp).map_err(|e| e.to_string())?;
    if let Some(c) = &report.corrupt {
        if !c.torn {
            eprintln!("日志损坏（seq={:?}）：{}", c.seq, c.reason);
            eprintln!("拒绝导出——导出的轨迹必须可信。先跑 `ui verify` 看看。");
            return Ok(2);
        }
    }

    // 头部（不含 events）：先构造一个小 JSON，再去掉末尾的 `}`，后面接 events 数组
    let head = Json::obj(vec![
        ("workspace", Json::from(ws.to_string_lossy().as_ref())),
        ("journal", Json::from(jp.to_string_lossy().as_ref())),
        (
            "limits",
            Json::obj(vec![
                ("maxTokens", Json::from(limits.tokens)),
                ("maxToolCalls", Json::from(limits.tool_calls)),
            ]),
        ),
        ("eventCount", Json::from(report.events as u64)),
        ("redact", Json::from(if raw { "off" } else { "secrets" })),
    ])
    .to_compact();
    let head = &head[..head.len().saturating_sub(1)]; // 去掉结尾的 }

    let out = args.get("html").unwrap_or("out/trace.html");
    if let Some(parent) = Path::new(out).parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // 流式写：模板切成前缀/后缀，中间边读日志边写，内存与日志大小无关。
    let (prefix, suffix) = match VIEWER.find("__UI_DATA__") {
        Some(i) => (&VIEWER[..i], &VIEWER[i + "__UI_DATA__".len()..]),
        None => return Err("查看器模板里没有 __UI_DATA__ 占位符".to_string()),
    };

    let file = File::create(out).map_err(|e| format!("写文件失败: {}", e))?;
    let mut w = std::io::BufWriter::new(file);
    let mut redacted = 0usize;

    w.write_all(prefix.as_bytes())
        .map_err(|e| format!("写文件失败: {}", e))?;
    w.write_all(head.as_bytes())
        .map_err(|e| format!("写文件失败: {}", e))?;
    w.write_all(b",\"events\":[")
        .map_err(|e| format!("写文件失败: {}", e))?;

    let src = File::open(&jp).map_err(|e| format!("{}: {}", jp.display(), e))?;
    // 只复制 verify_stream 已经验证过的那段字节。不能在 verify 之后重新按
    // 行读到 EOF：活写者可能在这期间追加，导出就会混入未验证事件；撕裂尾行
    // 也会被原样写进 __UI_DATA__，让整个查看器变成非法 JSON。
    let limit = report.good_bytes as u64;
    let mut reader = BufReader::new(src).take(limit);
    let mut buf = String::new();
    let mut first = true;
    loop {
        buf.clear();
        let n = reader
            .read_line(&mut buf)
            .map_err(|e| format!("读日志失败: {}", e))?;
        if n == 0 {
            break;
        }
        let line = buf.trim_end();
        if line.is_empty() {
            continue;
        }
        if !first {
            w.write_all(b",").map_err(|e| format!("写文件失败: {}", e))?;
        }
        first = false;
        // 脱敏：解析→清洗→只在真的动了的时候重新序列化。
        // 没命中的行原样写出，事件数、字段顺序、数字格式全都不变——
        // 「导出必须等于日志」这条（tests/cli.rs 会数行数）不能因为脱敏而失真。
        let payload = if raw {
            line.to_string()
        } else {
            match ui_core::json::parse(line) {
                Ok(mut ev) => {
                    let n = ui_core::secrets::redact_json(&mut ev);
                    redacted += n;
                    if n == 0 {
                        line.to_string()
                    } else {
                        ev.to_compact()
                    }
                }
                // 撕裂的尾行：verify_stream 已经放行，这里没法解析就保持原样
                Err(_) => line.to_string(),
            }
        };
        // 事件里出现 `</script>` 会截断文档，必须转义
        let escaped = payload.replace("</", "<\\/");
        w.write_all(escaped.as_bytes())
            .map_err(|e| format!("写文件失败: {}", e))?;
    }

    w.write_all(b"]}")
        .map_err(|e| format!("写文件失败: {}", e))?;
    w.write_all(suffix.as_bytes())
        .map_err(|e| format!("写文件失败: {}", e))?;
    w.flush().map_err(|e| format!("写文件失败: {}", e))?;
    drop(w);

    let abs = std::fs::canonicalize(out).unwrap_or_else(|_| PathBuf::from(out));
    println!("已导出 {}", abs.display());
    if raw {
        println!("--raw：事件按日志原文写出，未做密钥脱敏。转发前自己确认一遍。");
    } else if redacted > 0 {
        println!(
            "脱敏：{} 处疑似密钥已换成 [REDACTED:…]（留档要原样就加 --raw）",
            redacted
        );
    } else {
        println!("脱敏：未发现疑似密钥（默认开启，--raw 关闭）");
    }
    if let Some(win) = windows_path(&abs) {
        println!("Windows 路径 {}", win);
        println!("双击打开；或把上一条路径粘进浏览器地址栏。");
    } else {
        println!("用浏览器打开它即可；数据已内嵌，可离线转发。");
    }
    Ok(0)
}

/// `ui secrets`：把「日志里到底有没有凭据」变成一个能查、能出门的问题。
///
/// 扫的是最容易被转发出去的两类东西：
/// - 当前 journal（`--journal` / `--session` 指定）：`ui export`、viewer、
///   快照都从它取数；
/// - `.ui/tool-output/`：长输出在这里留了**全文**，上下文里只是截断片段。
/// - `.ui/tool-private/`：按隐私清单扣留的**原文**，日志里连片段都没有。
///
/// 刻意不扫 `.ui/memory/` 和 `.ui/sessions/` 之外的内核状态：前者在写入时
/// 就有 `memory::looks_secret` 硬闸（命中直接拒绝写），后者就是 journal 本身。
///
/// 找到东西时退出码 1，方便 CI 把它当一道闸。
fn cmd_secrets(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let jp = resolve_journal(args, &ws);
    let as_json = args.flag("json");

    let mut hits: Vec<(String, Json)> = Vec::new();
    let mut scanned = 0usize;

    // (来源标签, 文件)
    let mut sources: Vec<(String, PathBuf)> = Vec::new();
    if jp.is_file() {
        sources.push(("journal".to_string(), jp.clone()));
    }
    // 两个落盘目录都要扫。`tool-private/` 里放的正是"日志里没有、盘上却留着"
    // 的那一份——漏掉它，`ui secrets` 说"干净"就只覆盖了最不需要管的那一半。
    for dir in ui_core::spill::dirs(&ws) {
        let tag = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_file() {
                    sources.push((
                        format!(
                            "{}/{}",
                            tag,
                            p.file_name().unwrap_or_default().to_string_lossy()
                        ),
                        p,
                    ));
                }
            }
        }
    }
    // 记忆库是**原文**的另一个藏身处：`memory/write` 走的是 `append_raw`
    // （正文要原样召回，抹了就是记错），所以它不归门管，归这里管。
    let memory = ws.join(".ui").join("memory.jsonl");
    if memory.is_file() {
        sources.push(("memory".to_string(), memory));
    }
    if sources.is_empty() {
        // 两个落盘目录都要点名：只报一个，读的人会以为另一个不存在
        let where_ = ui_core::spill::dirs(&ws)
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<String>>()
            .join(" ＋ ");
        println!("没有可扫的东西：{}、{}、.ui/memory.jsonl 都不存在", jp.display(), where_);
        return Ok(0);
    }

    for (label, path) in &sources {
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) => return Err(format!("{}: {}", path.display(), e)),
        };
        let mut reader = std::io::BufReader::new(file);
        let mut buf = String::new();
        let mut line_no = 0usize;
        loop {
            buf.clear();
            let n = reader
                .read_line(&mut buf)
                .map_err(|e| format!("{}: {}", path.display(), e))?;
            if n == 0 {
                break;
            }
            line_no += 1;
            let line = buf.trim_end();
            if line.is_empty() {
                continue;
            }
            // journal 一行一事件；工具输出是整个文件一行一行来。
            // 两种都按行扫，因为判据是子串级的。
            scanned += 1;
            let marks = ui_core::secrets::find(line);
            if marks.is_empty() {
                continue;
            }
            let labels: Vec<&str> = marks.iter().map(|m| m.label).collect();
            let mut seen = labels.clone();
            seen.sort();
            seen.dedup();
            hits.push((
                label.clone(),
                Json::obj(vec![
                    ("source", Json::from(label.as_str())),
                    ("line", Json::from(line_no as u64)),
                    ("count", Json::from(marks.len() as u64)),
                    (
                        "labels",
                        Json::Arr(seen.iter().map(|l| Json::from(*l)).collect()),
                    ),
                ]),
            ));
        }
    }

    if as_json {
        let payload = Json::obj(vec![
            ("workspace", Json::from(ws.to_string_lossy().as_ref())),
            ("scannedLines", Json::from(scanned as u64)),
            (
                "hits",
                Json::Arr(hits.iter().map(|(_, j)| j.clone()).collect()),
            ),
        ]);
        println!("{}", payload.to_compact());
    } else {
        println!(
            "扫描 {} 行（{} 个来源）",
            scanned,
            sources.len()
        );
        if hits.is_empty() {
            println!("未发现疑似密钥。");
        }
        for (label, hit) in &hits {
            println!(
                "  {} 第 {} 行：{} 处（{}）",
                label,
                hit.u64_at("line").unwrap_or(0),
                hit.u64_at("count").unwrap_or(0),
                hit.arr_at("labels")
                    .map(|a| a
                        .iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join("、"))
                    .unwrap_or_default()
            );
        }
        if !hits.is_empty() {
            println!(
                "\n只报位置和类型，不复述内容——要出门跑 `ui export`（默认脱敏），\n\
                 要留原样加 `--raw`；日志本身要一份干净副本：`ui secrets --redact-out <路径>`。"
            );
        }
    }

    // 一份脱敏副本：新日志自己得能过 `ui verify`，所以走 Journal::open +
    // append（重建哈希链），而不是把清洗后的行直接抄过去。
    if let Some(out) = args.get("redact-out") {
        let src = sources
            .iter()
            .find(|(label, _)| label == "journal")
            .map(|(_, p)| p.clone())
            .ok_or_else(|| "没有 journal 可脱敏（--journal/--session 指对了吗）".to_string())?;
        let dst = if Path::new(out).is_absolute() {
            PathBuf::from(out)
        } else {
            ws.join(out)
        };
        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut written = 0usize;
        let mut changed = 0usize;
        {
            let mut journal = Journal::open(&dst).map_err(|e| e.to_string())?;
            let mut reader = std::io::BufReader::new(
                std::fs::File::open(&src).map_err(|e| format!("{}: {}", src.display(), e))?,
            );
            let mut buf = String::new();
            loop {
                buf.clear();
                if reader
                    .read_line(&mut buf)
                    .map_err(|e| format!("{}: {}", src.display(), e))?
                    == 0
                {
                    break;
                }
                let line = buf.trim_end();
                if line.is_empty() {
                    continue;
                }
                match ui_core::json::parse(line) {
                    Ok(mut ev) => {
                        if ui_core::secrets::redact_json(&mut ev) > 0 {
                            changed += 1;
                        }
                        // 信封字段（kind/seq/ts/prev/hash/w）描述的是**原**日志：
                        // 副本必须由 Journal 自己重新编号、重新签链，抄过来
                        // 一条就红一条（append 会把同名字段当成新键再塞一遍）。
                        let kind = ev.str_at("kind").unwrap_or("event").to_string();
                        ev.remove("kind");
                        for envelope in ["seq", "ts", "prev", "hash", "w"] {
                            ev.remove(envelope);
                        }
                        journal
                            .append(&kind, ev)
                            .map_err(|e| e.to_string())?;
                        written += 1;
                    }
                    Err(_) => return Err(format!("{} 第 {} 行不是合法事件", src.display(), written + 1)),
                }
            }
        }
        println!(
            "脱敏副本 {}：{} 条事件，其中 {} 条被清洗",
            dst.display(),
            written,
            changed
        );
    }

    if hits.is_empty() {
        Ok(0)
    } else {
        Ok(1)
    }
}

/// 把 WSL 的 `/mnt/x/...` 翻成 `X:\...`，省得用户自己换算。
fn windows_path(p: &Path) -> Option<String> {
    let s = p.to_string_lossy();
    let rest = s.strip_prefix("/mnt/")?;
    let mut it = rest.splitn(2, '/');
    let drive = it.next()?;
    if drive.len() != 1 {
        return None;
    }
    let tail = it.next().unwrap_or("");
    Some(format!(
        "{}:\\{}",
        drive.to_uppercase(),
        tail.replace('/', "\\")
    ))
}

fn cmd_skills(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let cat = ui_core::skill::Catalog::discover(&ws);
    if cat.is_empty() {
        println!("没有 skill。放在 {}/skills/<名字>/SKILL.md 或 .ui/skills/ 下。", ws.display());
        return Ok(0);
    }
    println!("{} 个 skill（正文按需加载）：", cat.skills.len());
    for s in &cat.skills {
        println!("  {:<20} {:>6} B  {}", s.name, s.body.len(), s.description);
    }
    Ok(0)
}

/// journal 写入的 String 化包装：CLI 层统一用 String 错误
fn append_log(agent: &mut Agent, kind: &str, payload: Json) -> Result<(), String> {
    agent
        .journal
        .append(kind, payload)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 工作流：顺序任务 + 并行子代理。每一步/每个子代理都写进同一份父日志。
/// `--json` 时进度走 stderr，stdout 只有一行结果 JSON。
/// 协作模式：把复杂任务拆成并行子任务，交给现有的 workflow 引擎跑。
///
/// 引擎侧的能力（并行、隔离副本、冲突/漂移闸、每子代理独立日志）都已经有了，
/// 这里只补"自动拆解"这一层：让模型出一个计划，转成 workflow 文件，再委派执行。
fn cmd_team(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let task = match args.get("task") {
        Some(t) => t.to_string(),
        None => return Err("用法：ui team --task \"复杂任务\" [--agents 3] [--isolate]".to_string()),
    };
    let agents = args
        .get("agents")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(3)
        .clamp(2, 6);

    println!("协作模式 · 目标：{}", task);
    println!("子代理：{} 个（并行）", agents);

    // ── 1. 规划：单独一个 agent，安静模式，不让它动手 ──
    let model = build_model(args)?;
    let jp = resolve_journal(args, &ws);
    let journal = Journal::open(&jp).map_err(|e| e.to_string())?;
    let mut planner = Agent::new(journal, model, ws.clone(), limits_from(args));
    planner.mode = ui_core::Mode::Quiet;
    planner.tool_chars = tool_chars(args);
    // 规划阶段只给一个工具：提交计划。
    // 正文里写 JSON 会违反内核的输出协议（整条回复只能是一个 {content,toolCalls}），
    // 真模型会拒绝；用工具提交既合规又好审计。
    planner.tools = vec![ui_core::tools::plan_tool()];
    let mut hooks = ConsoleHooks::new(args.flag("yes"), args.flag("json"));
    let prompt = format!(
        "把下面这个任务拆成 {agents} 个可以并行的子任务，每个子任务要能独立完成、边界清楚。\n\
         用 plan 工具提交：parallel 里每项是 {{\"prompt\": \"子任务说明\"}}，summary 写汇总阶段的提示。\n\
         不要修改任何文件，也不要输出 JSON 正文。\n\n\
         任务：{task}"
    );
    let report = planner
        .run_turn(&prompt, &mut hooks)
        .map_err(|e| e.to_string())?;
    // 计划优先从工具调用里取（协议内），其次才退回正文里抠 JSON（剧本模式用）
    let mut plan = plan_from_journal(&jp).or_else(|| extract_plan(&report.final_text));
    if plan.is_none() {
        eprintln!("（没有拿到 plan 工具调用，要求重试一次）");
        let repair = "你刚才没有调用 plan 工具。请现在调用 plan，参数 parallel 是子任务数组。";
        let _ = planner.run_turn(repair, &mut hooks);
        plan = plan_from_journal(&jp).or_else(|| extract_plan(&report.final_text));
    }
    let plan = plan.ok_or_else(|| "没有拿到计划（plan 工具未被调用）".to_string())?;

    let items = plan.arr_at("parallel").cloned().unwrap_or_default();
    if items.is_empty() {
        return Err("计划里没有 parallel 子任务".to_string());
    }
    println!("计划：{} 个并行子任务", items.len());
    for (i, it) in items.iter().enumerate() {
        println!("  {}. {}", i + 1, it.str_at("prompt").unwrap_or(""));
    }

    // ── 2. 转成 workflow 的格式（引擎认识的那种）──
    // 结构：第 1 步并行跑所有子任务，第 2 步汇总。
    let summary = plan
        .str_at("summary")
        .unwrap_or("汇总各子任务的结果，给出结论")
        .to_string();
    let wf_json = Json::obj(vec![
        ("name", Json::from(format!("team-{}", agents))),
        (
            "steps",
            Json::Arr(vec![
                Json::obj(vec![("parallel", Json::Arr(items.clone()))]),
                Json::obj(vec![("prompt", Json::from(summary.as_str()))]),
            ]),
        ),
    ]);

    let dir = ws.join(".ui");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stamp = ui_core::time::now_ms();
    let plan_path = dir.join(format!("team-{}.json", stamp));
    std::fs::write(&plan_path, wf_json.to_pretty()).map_err(|e| e.to_string())?;
    println!("计划文件：{}", plan_path.display());

    // ── 3. 委派给 workflow 执行器（并行 + 隔离 + 闸门都在那边）──
    let mut wf_args = Args::empty();
    wf_args.values.insert(
        "file".to_string(),
        plan_path.to_string_lossy().to_string(),
    );
    wf_args.values.insert(
        "workspace".to_string(),
        ws.to_string_lossy().to_string(),
    );
    for key in ["mock", "agents", "max-tokens", "max-calls", "mode", "mcp"] {
        if let Some(v) = args.get(key) {
            wf_args.values.insert(key.to_string(), v.to_string());
        }
    }
    if args.flag("yes") {
        wf_args.flags.push("yes".to_string());
    }
    if args.flag("json") {
        wf_args.flags.push("json".to_string());
    }
    // 协作模式默认隔离：子代理各用一份工作区副本，改动靠 apply 闸合并
    if args.flag("isolate") || !args.flag("no-isolate") {
        wf_args.flags.push("isolate".to_string());
    }
    println!("交给 workflow 执行（并行 + 隔离）…");
    let wf_code = cmd_workflow(&wf_args)?;

    // ── 4. 把子代理的改动合并回主工作区（冲突/漂移闸都在 apply 里）──
    let mut apply_args = Args::empty();
    apply_args.values.insert(
        "workspace".to_string(),
        ws.to_string_lossy().to_string(),
    );
    apply_args.flags.push("all".to_string());
    if args.flag("yes") {
        apply_args.flags.push("yes".to_string());
    }
    // 默认**不自动落地**：先给出可审查的改动清单，合并要显式 --apply。
    // 理由：自动落地让"子代理改了什么"越过人的眼睛，而恢复成本远高于审查成本。
    let auto_apply = args.flag("apply");
    let apply_code = if auto_apply {
        println!("\n合并各子代理的改动…");
        let code = cmd_apply(&apply_args)?;
        if code != 0 {
            eprintln!("合并时有问题（退出码 {}），见上面的冲突/漂移报告", code);
        }
        code
    } else {
        println!("\n子代理产出（未合并）：");
        let events = Journal::load(&jp).map_err(|e| e.to_string())?;
        let mut any = false;
        for e in &events {
            if e.str_at("kind") != Some("delegate/end") {
                continue;
            }
            let changed = e.arr_at("changed").cloned().unwrap_or_default();
            if changed.is_empty() {
                continue;
            }
            any = true;
            let id = e
                .str_at("workdir")
                .and_then(|w| w.rsplit('/').next())
                .unwrap_or("?")
                .to_string();
            let files: Vec<String> = changed
                .iter()
                .filter_map(|c| c.as_str().map(str::to_string))
                .collect();
            println!("  {} → {}", id, files.join(", "));
            println!("    合并：ui apply --id {} --yes", id);
        }
        if !any {
            println!("  （没有子代理产出改动）");
        }
        println!("\n想自动合并：ui team ... --apply");
        0
    };

    // ── 5. 汇总用一个新的 agent（在 apply 之后看真实磁盘）──
    let model2 = build_model(args)?;
    let journal2 = Journal::open(&jp).map_err(|e| e.to_string())?;
    let mut summarizer = Agent::new(journal2, model2, ws.clone(), limits_from(args));
    summarizer.mode = mode_from(args);
    summarizer.tool_chars = tool_chars(args);
    attach_mcp(&mut summarizer, args)?;
    attach_plugins(&mut summarizer, args)?;

    // ── 6. 验收：跑一条命令，失败就不许说完成（no artifact, no claim）──
    let mut accept_ok: Option<bool> = None;
    if let Some(cmd) = args.get("accept") {
        println!("\n── 验收：{} ──", cmd);
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(&ws)
            .output();
        let (code, tail) = match out {
            Ok(o) => {
                let text = format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                );
                let tail = text
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .rev()
                    .take(6)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                (o.status.code().unwrap_or(-1), tail)
            }
            Err(e) => (-1, e.to_string()),
        };
        accept_ok = Some(code == 0);
        if code == 0 {
            println!("验收通过");
        } else {
            println!("验收失败（退出码 {}）", code);
            println!("{}", tail);
        }
        append_log(
            &mut summarizer,
            "team/accept",
            Json::obj(vec![
                ("cmd", Json::from(cmd)),
                ("ok", Json::Bool(code == 0)),
                ("code", Json::from(code)),
                ("tail", Json::from(tail.as_str())),
            ]),
        )?;
    }

    let mut hooks2 = ConsoleHooks::new(args.flag("yes"), args.flag("json"));
    println!("\n── 汇总 ──");
    let accept_line = match accept_ok {
        Some(true) => "验收命令已通过（退出码 0）。".to_string(),
        Some(false) => "**验收命令失败**——结论必须写明失败，不能宣布完成。".to_string(),
        None => "本次没有配置验收命令。".to_string(),
    };
    let final_prompt = format!(
        "各子代理已完成并已合并回工作区。现在做汇总：\n\
         1) 用工具核对每项产出是否真的落地（不要只信描述）；\n\
         2) 说明哪些完成、哪些没完成、原因是什么；\n\
         3) 给出一句话结论。\n\n\
         验收状态：{accept_line}\n\
         原始目标：{task}\n汇总提示：{summary}"
    );
    let final_report = summarizer
        .run_turn(&final_prompt, &mut hooks2)
        .map_err(|e| e.to_string())?;

    println!(
        "\n协作结束 · workflow={} · apply={} · 验收={} · 汇总 {} 步 / {} tok",
        wf_code,
        apply_code,
        match accept_ok {
            Some(true) => "通过",
            Some(false) => "失败",
            None => "未配置",
        },
        final_report.steps,
        final_report.tokens
    );
    if wf_code != 0 {
        return Ok(wf_code);
    }
    if apply_code != 0 {
        return Ok(apply_code);
    }
    // 验收失败就是失败，哪怕 agent 说完成了
    Ok(if accept_ok == Some(false) { 3 } else { 0 })
}

/// 从日志里取最后一次 plan 工具调用的参数。
fn plan_from_journal(jp: &Path) -> Option<Json> {
    let events = Journal::load(jp).ok()?;
    let mut found: Option<Json> = None;
    for e in &events {
        if e.str_at("kind") != Some("tool/request") || e.str_at("tool") != Some("plan") {
            continue;
        }
        if e.str_at("decision") == Some("deny") {
            continue;
        }
        if let Some(args) = e.get("args") {
            found = Some(args.clone());
        }
    }
    found
}

/// 从模型的回答里抠出 JSON 计划：容忍 ```json 围栏和前后废话。
fn extract_plan(text: &str) -> Option<Json> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    let slice = &text[start..=end];
    json::parse(slice).ok()
}

fn cmd_workflow(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let json_out = args.flag("json");
    let note = |msg: String| {
        if json_out {
            eprintln!("{}", msg);
        } else {
            say(msg);
        }
    };

    let file = args.get("file").ok_or("缺少 --file")?;
    let wf = ui_core::workflow::load(Path::new(file))?;
    let model = build_model(args)?;
    let jp = resolve_journal(args, &ws);
    let journal = Journal::open(&jp).map_err(|e| e.to_string())?;

    let mut agent = Agent::new(journal, model, ws.clone(), limits_from(args));
    agent.compact = compact_policy(args);
    agent.tool_chars = tool_chars(args);
    if args.flag("no-auto-checkpoint") {
        agent.auto_checkpoint = false;
    }
    agent.mode = mode_from(args);
    attach_mcp(&mut agent, args)?;
    attach_plugins(&mut agent, args)?;
    apply_tool_filter(&mut agent, args);
    if args.flag("no-env-scrub") {
        agent.env_scrub = false;
    }
    if args.flag("no-redact") {
        // 关的是**落账前**那一道脱敏：日志里会留下原文，并且每轮开始先写
        // 一条 privacy/redact-off，三个月后翻日志的人能看出这段是原文。
        agent.set_redact_output(false);
    }
    if let Some(text) = args.get("append-system-prompt") {
        ui_core::prompt::set_extra_system(text);
    }
    let mut hooks = ConsoleHooks::new(args.flag("yes"), json_out);

    append_log(
        &mut agent,
        "workflow/start",
        Json::obj(vec![
            ("name", Json::from(wf.name.as_str())),
            ("steps", Json::from(wf.steps.len())),
            ("tasks", Json::from(wf.task_count())),
        ]),
    )?;
    note(format!(
        "工作流 {} · {} 步 · {} 个任务 · workspace {}",
        wf.name,
        wf.steps.len(),
        wf.task_count(),
        ws.display()
    ));

    let mut delegates: Vec<Json> = Vec::new();
    let mut stopped: Option<(usize, String)> = None;

    for (i, node) in wf.steps.iter().enumerate() {
        let n = i + 1;
        match node {
            ui_core::workflow::Node::Task(task) => {
                // 每一步是一次独立的 run：上一部的预算、工具序号、以及
                // 「本次会话允许」的规则都不该顺着工作流往后淌。
                // 边界必须划在 preload_skills **之前**——晚一步就等于把这一步
                // 刚预加载的 skill 清掉；早一步也没别的地方可放。
                agent.begin_run();
                append_log(
                    &mut agent,
                    "workflow/step",
                    Json::obj(vec![
                        ("index", Json::from(n)),
                        ("prompt", Json::from(task.prompt.as_str())),
                        (
                            "skills",
                            Json::Arr(task.skills.iter().map(|s| Json::from(s.as_str())).collect()),
                        ),
                    ]),
                )?;
                note(format!("\n── 第 {}/{} 步 ──", n, wf.steps.len()));
                preload_skills(&mut agent, &ws, &task.skills)?;

                if task.gate {
                    let req = ApprovalRequest {
                        // 闸门也要有身份：日志里「批准了哪一步」不能只写
                        // `workflow-3`——同一份日志跑第二遍就是同一串 ID。
                        approval_id: format!("gate-{}", agent.journal.seq()),
                        call_id: format!("workflow-{}", n),
                        tool: "workflow".to_string(),
                        args: Json::obj(vec![
                            ("step", Json::from(n)),
                            ("prompt", Json::from(task.prompt.as_str())),
                        ]),
                        effects: Vec::new(),
                        reason: format!("工作流「{}」第 {} 步需要确认", wf.name, n),
                    };
                    // 工作流闸门没有「会话级」这回事：下一步就是另一个 run。
                    if hooks.on_approval(&req).decision != Decision::Allow {
                        stopped = Some((n, "gated".to_string()));
                        break;
                    }
                }

                let report = agent
                    .run_turn(&task.prompt, &mut hooks)
                    .map_err(|e| e.to_string())?;
                if report.reason != "done" {
                    stopped = Some((n, report.reason.clone()));
                    break;
                }
            }

            ui_core::workflow::Node::Group(tasks) => {
                note(format!(
                    "\n── 第 {}/{} 步 · {} 个并行子代理 ──",
                    n,
                    wf.steps.len(),
                    tasks.len()
                ));
                let results = run_parallel(args, &ws, &wf.name, n, tasks)?;
                let mut failed = false;
                for r in &results {
                    let rec = Json::obj(vec![
                        ("id", Json::from(r.id.as_str())),
                        ("ok", Json::Bool(r.ok)),
                        ("summary", Json::from(r.summary.as_str())),
                        ("journal", Json::from(r.journal.to_string_lossy().as_ref())),
                        (
                            "workdir",
                            r.workdir
                                .as_ref()
                                .map(|p| Json::from(p.to_string_lossy().to_string()))
                                .unwrap_or(Json::Null),
                        ),
                        (
                            "changed",
                            Json::Arr(r.changed.iter().map(|c| Json::from(c.as_str())).collect()),
                        ),
                        (
                            "deleted",
                            Json::Arr(r.deleted.iter().map(|c| Json::from(c.as_str())).collect()),
                        ),
                    ]);
                    append_log(&mut agent, "delegate/end", rec.clone())?;
                    delegates.push(rec);
                    note(format!(
                        "  子代理 {} {} · {}",
                        r.id,
                        if r.ok { "ok" } else { "失败" },
                        r.summary
                    ));
                    failed |= !r.ok;
                }
                let text = results
                    .iter()
                    .map(|r| {
                        format!(
                            "### {} ({})\n{}",
                            r.id,
                            if r.ok { "ok" } else { "失败" },
                            r.summary
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if !text.is_empty() {
                    agent.history.push(Message::note(format!(
                        "并行子代理的结果（各自独立日志，父日志只记摘要）：\n{}",
                        text
                    )));
                }
                if failed {
                    stopped = Some((n, "delegate_failed".to_string()));
                    break;
                }
            }
        }
    }

    let (reason, exit) = match &stopped {
        None => ("done", 0),
        Some((n, why)) => {
            append_log(
                &mut agent,
                "workflow/end",
                Json::obj(vec![
                    ("name", Json::from(wf.name.as_str())),
                    ("reason", Json::from(why.as_str())),
                    ("stoppedAt", Json::from(*n)),
                ]),
            )?;
            if !json_out {
                say(format!("已停止：第 {} 步（{}）", n, why));
            }
            (why.as_str(), 1)
        }
    };
    if stopped.is_none() {
        append_log(
            &mut agent,
            "workflow/end",
            Json::obj(vec![
                ("name", Json::from(wf.name.as_str())),
                ("reason", Json::from("done")),
            ]),
        )?;
        note(format!("\n工作流完成 · 日志 {}", jp.display()));
    }

    if json_out {
        println!(
            "{}",
            Json::obj(vec![
                ("ok", Json::Bool(exit == 0)),
                ("exit", Json::from(exit)),
                ("workflow", Json::from(wf.name.as_str())),
                ("reason", Json::from(reason)),
                (
                    "stoppedAt",
                    stopped
                        .as_ref()
                        .map(|(n, _)| Json::from(*n))
                        .unwrap_or(Json::Null),
                ),
                ("delegates", Json::Arr(delegates)),
            ])
            .to_compact()
        );
    }

    Ok(exit)
}


/// `--skills a,b` → ["a", "b"]
fn skill_list(args: &Args) -> Vec<String> {
    args.get("skills")
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn preload_skills(agent: &mut Agent, ws: &Path, names: &[String]) -> Result<(), String> {    if names.is_empty() {
        return Ok(());
    }
    let catalog = ui_core::skill::Catalog::discover(ws);
    for name in names {
        match catalog.find(name) {
            Some(s) => {
                agent
                    .load_skill(name, &s.body, "workflow")
                    .map_err(|e| e.to_string())?;
                println!("  预加载 skill: {}（{} B）", name, s.body.len());
            }
            None => {
                append_log(
                    agent,
                    "skill/missing",
                    Json::obj(vec![("name", Json::from(name.as_str()))]),
                )?;
                println!("  ⚠ 没有 skill {}，跳过", name);
            }
        }
    }
    Ok(())
}

struct ChildResult {
    id: String,
    ok: bool,
    summary: String,
    journal: PathBuf,
    /// 隔离模式下子代理的工作区副本（None = 直接在工作区里跑）
    workdir: Option<PathBuf>,
    /// 相对工作区改动过的文件
    changed: Vec<String>,
    /// 子代理在副本里**删掉**的文件（不自动合并，只报告——删除不可逆）
    deleted: Vec<String>,
}

/// 隔离副本里不需要的东西：日志、产物、依赖、node_modules
// 跳过列表与比对侧共用一份，避免"复制了但永远扫不到"
const ISOLATE_SKIP: &[&str] = ui_core::isolate::SKIP;

/// 复制并**在同一次读取里**算出哈希。
///
/// 之前是 `copy_file` + `hash_file`：写完再整个读一遍，等于每份工作区读两遍。
/// 隔离副本动辄上千个文件，这一遍就是主要开销。
fn copy_and_hash(from: &Path, to: &Path) -> std::io::Result<(u64, Option<String>)> {
    use std::io::{Read, Write};

    // 只有 reflink 分支需要 size；Windows 上没有 reflink，别留下未使用变量
    #[cfg(not(windows))]
    let size = from.metadata().map(|m| m.len()).unwrap_or(0);

    // 大文件先试写时复制：btrfs/xfs 上是 O(1)。失败（ext4 等）就退回普通复制。
    // 不管走哪条路，哈希都必须把内容读一遍，所以这里只省了"写"。
    #[cfg(not(windows))]
    if size >= 1024 * 1024 && reflink(from, to).is_ok() {
        let hash = ui_core::isolate::hash_file(from);
        return Ok((size, hash));
    }

    let mut src = std::fs::File::open(from)?;
    let mut dst = std::fs::File::create(to)?;
    let mut hasher = ui_core::fasthash::Xxh64::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        dst.write_all(&buf[..n])?;
        total += n as u64;
    }
    dst.flush()?;
    Ok((total, Some(format!("xxh:{:016x}", hasher.finish()))))
}

/// 写时复制（Linux FICLONE）。不支持的文件系统会返回错误，调用方退回普通复制。
#[cfg(not(windows))]
fn reflink(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::raw::{c_int, c_ulong};
    use std::os::unix::io::AsRawFd;

    const FICLONE: c_ulong = 0x4004_9409;
    extern "C" {
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }

    let src = std::fs::File::open(from)?;
    let dst = std::fs::File::create(to)?;
    let rc = unsafe { ioctl(dst.as_raw_fd(), FICLONE, src.as_raw_fd() as c_ulong) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn hash_file(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut hasher = ui_core::sha256::Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(ui_core::sha256::hex(&hasher.finish()))
}

/// 复制工作区，同时记下「复制那一刻每个文件的哈希」——回写时用它检测漂移。
fn copy_tree(
    src: &Path,
    dst: &Path,
    root: &Path,
    manifest: &mut Vec<(String, String)>,
) -> std::io::Result<(usize, u64)> {
    std::fs::create_dir_all(dst)?;
    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if ISOLATE_SKIP.contains(&name.as_str()) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if from.is_dir() {
            let (f, b) = copy_tree(&from, &to, root, manifest)?;
            files += f;
            bytes += b;
        } else if from.is_file() {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // 一次读取：写副本 + 算哈希（manifest 用它检测回写时的漂移）。
            // 这里用 xxHash64 而不是 SHA-256：它只需要回答"改没改"，
            // 实测 SHA-256 359 MiB/s，是隔离复制的第二个大头（第一个是复制本身）。
            let (n, hash) = copy_and_hash(&from, &to)?;
            bytes += n;
            files += 1;
            if let (Ok(rel), Some(h)) = (from.strip_prefix(root), hash) {
                // 键格式与读取侧（ui_core::isolate::rel_key）保持一致
                manifest.push((
                    ui_core::isolate::rel_key(&rel.to_string_lossy()),
                    h,
                ));
            }
        }
    }
    Ok((files, bytes))
}

/// 找出副本里被改动、以及被删除的文件（逻辑在 `ui_core::isolate`，那里有单元测试）。
///
/// 删除**不自动合并**——那是不可逆操作。但必须报告：否则父工作区会留下一个
/// 本该消失的文件，而日志里毫无痕迹。
fn changed_files(_base: &Path, workdir: &Path) -> (Vec<String>, Vec<String>) {
    let manifest = read_manifest(workdir);
    let d = ui_core::isolate::diff(workdir, &manifest);
    (d.changed, d.deleted)
}

/// 并行子代理：默认各跑一个进程 + 独立日志；`--isolate` 时再各给一份工作区副本。
fn run_parallel(
    args: &Args,
    ws: &Path,
    wf: &str,
    step: usize,
    tasks: &[ui_core::workflow::Task],
) -> Result<Vec<ChildResult>, String> {
    let exe = std::env::current_exe().map_err(|e| format!("找不到自身可执行文件: {}", e))?;
    let mock = args.get("mock").map(str::to_string);
    let isolate = args.flag("isolate");

    let mut handles = Vec::new();
    for (j, task) in tasks.iter().enumerate() {
        let id = format!("{}-s{}-{}", wf, step, j + 1);
        let ws = ws.to_path_buf();
        let prompt = task.prompt.clone();
        let skills = task.skills.clone();
        let mock = mock.clone();
        let exe = exe.clone();

        handles.push(std::thread::spawn(move || {
            let agent_root = ws.join(".ui").join("agents").join(&id);
            let _ = std::fs::create_dir_all(&agent_root);

            // 隔离：复制一份工作区，子代理只在副本里动文件
            let workdir = if isolate {
                let copy = agent_root.join("ws");
                let _ = std::fs::remove_dir_all(&copy);
                let mut manifest: Vec<(String, String)> = Vec::new();
                match copy_tree(&ws, &copy, &ws, &mut manifest) {
                    Ok(_) => {
                        // manifest 是漂移检测的唯一依据：写不进去就必须失败，
                        // 不能 `let _ =` 吞掉——否则 changed 会变成"整个副本"，
                        // apply 全部判漂移，子代理的改动一个也合不回来。
                        let text = manifest
                            .iter()
                            .map(|(rel, h)| format!("{}\t{}", rel, h))
                            .collect::<Vec<_>>()
                            .join("\n");
                        if let Err(e) = std::fs::write(agent_root.join("manifest.txt"), text) {
                            return ChildResult {
                                id,
                                ok: false,
                                summary: format!("写 manifest 失败: {}", e),
                                journal: agent_root.join("journal.txt"),
                                workdir: None,
                                changed: Vec::new(),
                                deleted: Vec::new(),
                            };
                        }
                        Some(copy)
                    }
                    Err(e) => {
                        return ChildResult {
                            id,
                            ok: false,
                            summary: format!("复制工作区失败: {}", e),
                            journal: agent_root.join("journal.jsonl"),
                            workdir: None,
                            changed: Vec::new(),
                            deleted: Vec::new(),
                        }
                    }
                }
            } else {
                None
            };
            let run_ws = workdir.clone().unwrap_or_else(|| ws.clone());

            let task_file = agent_root.join("task.txt");
            let journal = agent_root.join("journal.jsonl");
            let result_file = agent_root.join("answer.md");
            if std::fs::write(&task_file, &prompt).is_err() {
                return ChildResult {
                    id,
                    ok: false,
                    summary: "写任务文件失败".to_string(),
                    journal,
                    workdir,
                    changed: Vec::new(),
                    deleted: Vec::new(),
                };
            }

            let mut cmd = std::process::Command::new(&exe);
            cmd.arg("run")
                .arg("--task-file")
                .arg(&task_file)
                .arg("--workspace")
                .arg(&run_ws)
                .arg("--journal")
                .arg(&journal)
                .arg("--result-file")
                .arg(&result_file)
                .arg("--yes");
            if let Some(m) = &mock {
                cmd.arg("--mock").arg(m);
            }
            if !skills.is_empty() {
                cmd.arg("--skills").arg(skills.join(","));
            }
            cmd.arg("--watch-parent");
            cmd.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let out = match cmd.spawn() {
                Ok(mut child) => {
                    // 握着 stdin 不放：父进程一死（含 SIGKILL），管道关闭，子进程读到 EOF 就取消
                    let _hold = child.stdin.take();
                    child.wait_with_output()
                }
                Err(e) => Err(e),
            };

            let (ok, summary) = match out {
                Ok(o) => {
                    // 优先读子进程写的答案文件；stdout 里混着日志，只作兜底
                    let answer = std::fs::read_to_string(&result_file)
                        .ok()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| {
                            let stdout = String::from_utf8_lossy(&o.stdout).to_string();
                            stdout
                                .lines()
                                .rev()
                                .find(|l| !l.trim().is_empty())
                                .unwrap_or("(无输出)")
                                .trim()
                                .to_string()
                        });
                    (o.status.success(), answer.chars().take(600).collect::<String>())
                }
                Err(e) => (false, format!("启动失败: {}", e)),
            };

            let (changed, deleted) = match workdir.as_ref() {
                Some(w) => {
                    let (c, d) = changed_files(&ws, w);
                    (c, d)
                }
                None => (Vec::new(), Vec::new()),
            };

            ChildResult {
                id,
                ok,
                summary,
                journal,
                workdir,
                changed,
                deleted,
            }
        }));
    }

    let mut results = Vec::new();
    for h in handles {
        match h.join() {
            Ok(r) => results.push(r),
            Err(_) => return Err("子代理线程崩溃".to_string()),
        }
    }
    Ok(results)
}

/// 读子代理的 manifest（复制那一刻的文件哈希）。
fn read_manifest(workdir: &Path) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let path = match workdir.parent() {
        Some(p) => p.join("manifest.txt"),
        None => return map,
    };
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            if let Some((rel, h)) = line.split_once('\t') {
                map.insert(rel.to_string(), h.to_string());
            }
        }
    }
    map
}

// ── apply 事务：留底 → 写回 → 校验 → 提交，任何一步失败整批还原 ──────────
//
// 原来这里是一条 `for … std::fs::copy(src, dst)?`：第 3 个文件写失败时，
// 前 2 个已经落地、第 3 个可能只写了一半（`copy` 先截断再拷），而 `?` 把
// 错误抛出去之后什么记录都不留——工作区停在「一半新一半旧」的状态，日志
// 假装无事发生，也没有任何手段退回原样。
//
// 现在一次 `--yes` 是一个事务：
//
// 1. **先记日志再动手**：`apply/begin` 写不进日志（`--journal` 指错、链断了、
//    目录只读）就直接失败，一个文件都不碰。改了东西却查不到，正是这套内核
//    要消灭的东西。
// 2. **留底**：每个目标的原样先拷进 `.ui/apply/<txn>/before/`；「当时不
//    存在」也记一笔——那种情况的回滚是删除。备份放在 `.ui` 里，模型读不到
//    （`protected::PROTECTED_DIRS`），它伪造不了「上一版长这样」。
// 3. **状态落盘**：`manifest.json` 走 staged → applying → committed /
//    rolled-back。崩在写回途中时，下一条 `ui apply` 认得出「上一半没写完」，
//    不会在残缺状态上再叠一次写回。
// 4. **写完就验**：内容哈希对不上源文件同样算失败，整批还原。
// 5. **还原讲分寸**：事务内部失败是无条件还原（盘上只有我们自己的笔迹）；
//    事后 `--rollback` 只还原「内容仍然是事务当初写进去的那份」，用户在
//    apply 之后自己改过的文件留给用户，留底也留着。

/// `.ui/apply/`：apply 事务的档案与留底。
fn apply_root(ws: &Path) -> PathBuf {
    ws.join(".ui").join("apply")
}

/// 相对路径摊平成备份文件名：分隔符换成 `+`，保留文件名和扩展名好让人翻。
fn flatten_rel(rel: &str) -> String {
    rel
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '+',
            other => other,
        })
        .collect()
}

fn json_arr(items: &[String]) -> Json {
    Json::Arr(items.iter().map(|s| Json::s(s.as_str())).collect())
}

/// 哈希字段的 JSON 表示：`null` = 那一边根本没有这个文件。
fn hash_json(h: Option<String>) -> Json {
    match h {
        Some(h) => Json::s(h.as_str()),
        None => Json::Null,
    }
}

fn read_apply_manifest(dir: &Path) -> Option<Json> {
    let text = std::fs::read_to_string(dir.join("manifest.json")).ok()?;
    json::parse(&text).ok()
}

/// 事务档案里的文件清单。
fn apply_manifest_entries(manifest: &Json) -> Vec<Json> {
    manifest.arr_at("entries").cloned().unwrap_or_default()
}

/// 工作区里所有 apply 事务目录，按名字排序。
///
/// 名字前缀是补零到 8 位的日志序号，所以字典序就是时间序。
fn apply_txn_dirs(ws: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = match std::fs::read_dir(apply_root(ws)) {
        Ok(rd) => rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect(),
        Err(_) => return Vec::new(),
    };
    dirs.sort();
    dirs
}

fn write_apply_manifest(
    dir: &Path,
    status: &str,
    txn: &str,
    ws: &Path,
    entries: &[Json],
) -> Result<(), String> {
    let manifest = Json::obj(vec![
        ("txn", Json::s(txn)),
        ("status", Json::s(status)),
        ("workspace", Json::s(ws.to_string_lossy().as_ref())),
        ("updatedAt", Json::n(ui_core::time::now_ms() as f64)),
        ("entries", Json::Arr(entries.to_vec())),
    ]);
    std::fs::write(dir.join("manifest.json"), manifest.to_pretty())
        .map_err(|e| format!("写事务档案 {} 失败: {}", dir.display(), e))
}

/// 上一次 apply 是不是死在写回途中，返回那个事务的目录与 ID。
///
/// 只认 `applying`：`staged` 表示崩在留底和落笔之间，工作区当时还是干净的，
/// 留一个没人动过的目录不是危险状态，不该拦住下一次合并。
fn unfinished_apply_txn(ws: &Path) -> Option<(PathBuf, String)> {
    for dir in apply_txn_dirs(ws) {
        let manifest = match read_apply_manifest(&dir) {
            Some(m) => m,
            None => continue,
        };
        if manifest.str_at("status") == Some("applying") {
            let id = txn_id_of(&dir, &manifest);
            return Some((dir, id));
        }
    }
    None
}

/// 把停在写回途中的事务标记成 abandoned：用户已经看过警告并选择「就在残缺
/// 状态上继续」。不记这一笔，这条闸会一辈子拦住后续 apply——每次都得 --force。
fn abandon_apply_txn(ws: &Path, jp: &Path, dir: &Path, txn: &str, note: &dyn Fn(String)) {
    if let Some(manifest) = read_apply_manifest(dir) {
        let _ = write_apply_manifest(
            dir,
            "abandoned",
            txn,
            ws,
            &apply_manifest_entries(&manifest),
        );
    }
    match Journal::open(jp) {
        Ok(mut journal) => {
            if let Err(e) = journal.append(
                "apply/abandoned",
                Json::obj(vec![("txn", Json::s(txn))]),
            ) {
                note(format!("  apply/abandoned 记不进日志: {}", e));
            }
        }
        Err(e) => note(format!("  日志 {} 打不开: {}", jp.display(), e)),
    }
}

/// 事务 ID：档案里写的那个，兜底用目录名。
fn txn_id_of(dir: &Path, manifest: &Json) -> String {
    match manifest.str_at("txn") {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string(),
    }
}

/// 一次事务式回写的结果。
#[derive(Default)]
struct ApplyRun {
    txn: Option<String>,
    /// 留底目录；committed 之后 `--rollback` 靠它
    dir: Option<PathBuf>,
    written: Vec<String>,
    restored: Vec<String>,
    error: Option<String>,
}

/// 按事务档案还原工作区后的账目。
#[derive(Default)]
struct ApplyUndo {
    /// 用留底覆盖回去的（`dry` 时是「打算覆盖」）
    restored: Vec<String>,
    /// 事务新建、现已删掉的（`dry` 时是「打算删」）
    removed: Vec<String>,
    /// 「apply 之后你自己动过」——没敢碰，留给人决定
    skipped: Vec<String>,
    /// 还原动作本身失败：留底也救不了，必须说出来
    errors: Vec<String>,
}

impl ApplyUndo {
    /// 是否已经和写回之前一模一样——这时留底才真的可以扔。
    fn complete(&self) -> bool {
        self.skipped.is_empty() && self.errors.is_empty()
    }

    fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.restored.is_empty() {
            parts.push(format!("还原 {} 个", self.restored.len()));
        }
        if !self.removed.is_empty() {
            parts.push(format!("删除 {} 个", self.removed.len()));
        }
        if !self.skipped.is_empty() {
            parts.push(format!("跳过 {} 个", self.skipped.len()));
        }
        if !self.errors.is_empty() {
            parts.push(format!("失败 {} 个", self.errors.len()));
        }
        if parts.is_empty() {
            "什么都没动".to_string()
        } else {
            parts.join("、")
        }
    }
}

/// 按事务档案把工作区退回写回之前。
///
/// `gate` = 只还原「盘上内容仍然是事务当初写进去的那份」。事后 `--rollback`
/// 必须开着它：用户自己改过的、或事后被删掉的文件不能被当成事务产物抹掉。
/// 事务内部失败的回滚不开——那时笔迹只有我们自己的，无条件还原才是对的。
///
/// `dry` = 只分类不动盘，给计划模式用。
fn restore_apply_entries(ws: &Path, dir: &Path, entries: &[Json], gate: bool, dry: bool) -> ApplyUndo {
    let mut undo = ApplyUndo::default();
    for e in entries {
        let rel = match e.str_at("rel") {
            Some(r) => r.to_string(),
            None => continue,
        };
        let dst = ws.join(&rel);
        let backup = e.str_at("backup").map(|b| dir.join("before").join(b));
        // 事务当初写进去的内容。没有它就没法判断「后来有没有人动过」。
        let after = e.str_at("after").map(str::to_string);
        let current = ui_core::isolate::hash_file(&dst);
        // 读不出哈希（ENOTDIR、权限）时不能当成「等于事务写的版本」：
        // 一次读失败就骗过闸门、把别人的东西覆盖掉，是最坏的一种漏法。
        let touched_by_us = matches!((&after, &current), (Some(a), Some(c)) if a == c);

        match backup {
            Some(b) => {
                if !b.is_file() {
                    undo
                        .errors
                        .push(format!("{}：留底 {} 不见了，无法还原", rel, b.display()));
                    continue;
                }
                if gate {
                    if !dst.exists() {
                        undo.skipped
                            .push(format!("{}：apply 之后被删掉了，不擅自复活", rel));
                        continue;
                    }
                    if !touched_by_us {
                        undo.skipped
                            .push(format!("{}：apply 之后内容又变过，保留现状", rel));
                        continue;
                    }
                }
                if dry {
                    undo.restored.push(rel);
                    continue;
                }
                if let Some(parent) = dst.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::copy(&b, &dst) {
                    Ok(_) => undo.restored.push(rel),
                    Err(err) => undo.errors.push(format!("{}：还原失败 {}", rel, err)),
                }
            }
            None => {
                // 写回之前这里没有文件：还原 = 删掉它
                if !dst.is_file() {
                    if dst.exists() {
                        undo.skipped.push(format!("{}：那是目录不是文件，没有删", rel));
                    }
                    continue;
                }
                if gate && !touched_by_us {
                    undo.skipped
                        .push(format!("{}：apply 之后内容又变过，保留现状", rel));
                    continue;
                }
                if dry {
                    undo.removed.push(rel);
                    continue;
                }
                match std::fs::remove_file(&dst) {
                    Ok(_) => {
                        // 事务顺手建出来的目录也一起收掉，不然「退回写回之前」
                        // 会留下一堆空壳。里面还有东西就删不动，忽略即可。
                        if let Some(parent) = dst.parent() {
                            let _ = std::fs::remove_dir(parent);
                        }
                        undo.removed.push(rel);
                    }
                    Err(err) => undo.errors.push(format!("{}：删除失败 {}", rel, err)),
                }
            }
        }
    }
    undo
}

/// 事务式回写：留底 → 写回 → 逐个验货 → 提交。失败时动过的已经全部还原。
fn run_apply_transaction(
    ws: &Path,
    jp: &Path,
    plan: &[(String, PathBuf)],
    note: &dyn Fn(String),
) -> ApplyRun {
    // ── 0. 日志先行：记不下来的改动就不该发生 ──
    let mut journal = match Journal::open(jp) {
        Ok(j) => j,
        Err(e) => {
            return ApplyRun {
                error: Some(format!("打不开日志 {}: {}", jp.display(), e)),
                ..Default::default()
            }
        }
    };
    // ID 前缀 = 这条 apply 之后要写的第一个日志序号：同一份日志内唯一，
    // 后缀的 writer 让并发进程也不撞车，而且名字能排出时间序。
    let txn = format!("{:08}-{}", journal.seq() + 1, journal.writer());
    if let Err(e) = journal.append(
        "apply/begin",
        Json::obj(vec![
            ("txn", Json::s(txn.as_str())),
            ("workspace", Json::s(ws.to_string_lossy().as_ref())),
            (
                "files",
                json_arr(&plan.iter().map(|(r, _)| r.clone()).collect::<Vec<_>>()),
            ),
        ]),
    ) {
        return ApplyRun {
            txn: Some(txn),
            error: Some(format!("写 apply/begin 失败: {}", e)),
            ..Default::default()
        };
    }

    let dir = apply_root(ws).join(&txn);
    let before = dir.join("before");
    if let Err(e) = std::fs::create_dir_all(&before) {
        return ApplyRun {
            txn: Some(txn),
            error: Some(format!("建事务目录 {} 失败: {}", dir.display(), e)),
            ..Default::default()
        };
    }

    // ── 1. 留底：一个字节都还没改，先确保每个目标的原样存得下来 ──
    let mut entries: Vec<Json> = Vec::new();
    let mut saved: Vec<PathBuf> = Vec::new();
    for (index, (rel, src)) in plan.iter().enumerate() {
        let dst = ws.join(rel);
        let existed = dst.is_file();
        let name = format!("{:04}-{}", index, flatten_rel(rel));
        let path = before.join(&name);
        if existed {
            if let Err(e) = std::fs::copy(&dst, &path) {
                for p in &saved {
                    let _ = std::fs::remove_file(p);
                }
                let _ = std::fs::remove_dir_all(&dir);
                return ApplyRun {
                    txn: Some(txn),
                    error: Some(format!("给 {} 留底失败: {}（工作区未改动）", rel, e)),
                    ..Default::default()
                };
            }
            saved.push(path);
        }
        entries.push(Json::obj(vec![
            ("rel", Json::s(rel.as_str())),
            ("src", Json::s(src.to_string_lossy().as_ref())),
            (
                "backup",
                if existed {
                    Json::s(name.as_str())
                } else {
                    Json::Null
                },
            ),
            (
                "before",
                hash_json(if existed {
                    ui_core::isolate::hash_file(&dst)
                } else {
                    None
                }),
            ),
            ("after", hash_json(ui_core::isolate::hash_file(src))),
        ]));
    }

    // ── 2. 档案：先 staged（此刻工作区仍然干净），落笔前改成 applying ──
    // 崩在两者之间会留下没有 manifest、或状态停在 staged 的目录，而工作区
    // 没动过——`unfinished_apply_txn` 因此不会拦住下一次合并。
    let mut failure: Option<String> = write_apply_manifest(&dir, "staged", &txn, ws, &entries)
        .err()
        .map(|e| {
            let _ = std::fs::remove_dir_all(&dir);
            e
        });

    // ── 3. 写回 + 验货 ──
    let mut written: Vec<String> = Vec::new();
    if failure.is_none() {
        failure = write_apply_manifest(&dir, "applying", &txn, ws, &entries).err();
    }
    if failure.is_none() {
        for e in &entries {
            let rel = match e.str_at("rel") {
                Some(r) => r.to_string(),
                None => {
                    failure = Some("事务档案里有条目没有 rel".to_string());
                    break;
                }
            };
            let src = match e.str_at("src") {
                Some(s) => PathBuf::from(s),
                None => {
                    failure = Some(format!("事务档案里 {} 没有源路径", rel));
                    break;
                }
            };
            if !src.is_file() {
                // 副本里这个文件又没了：工作区版本原样留着，跳过但不算失败
                note(format!("  跳过（副本里已不存在）: {}", rel));
                continue;
            }
            let dst = ws.join(&rel);
            if let Some(parent) = dst.parent() {
                if let Err(err) = std::fs::create_dir_all(parent) {
                    failure = Some(format!("建 {} 失败: {}", parent.display(), err));
                    break;
                }
            }
            if let Err(err) = std::fs::copy(&src, &dst) {
                failure = Some(format!("写回 {} 失败: {}", rel, err));
                break;
            }
            let want = ui_core::isolate::hash_file(&src);
            let got = ui_core::isolate::hash_file(&dst);
            if want.is_none() || want != got {
                failure = Some(format!(
                    "写回 {} 后校验不过：源 {:?}，盘上 {:?}",
                    rel, want, got
                ));
                break;
            }
            written.push(rel);
        }
    }

    // ── 4. 失败 → 整批还原（内部回滚不设哈希闸：盘上只有我们自己的笔迹）──
    if let Some(error) = failure {
        let undo = restore_apply_entries(ws, &dir, &entries, false, false);
        let _ = write_apply_manifest(&dir, "rolled-back", &txn, ws, &entries);
        if let Err(e) = journal.append(
            "apply/rolled-back",
            Json::obj(vec![
                ("txn", Json::s(txn.as_str())),
                ("stage", Json::s("write")),
                ("error", Json::s(error.as_str())),
                ("restored", json_arr(&undo.restored)),
                ("removed", json_arr(&undo.removed)),
                ("failed", json_arr(&undo.errors)),
            ]),
        ) {
            note(format!("  这笔回滚记不进日志: {}", e));
        }
        // 完全退回原样才扔留底；有残留就说明还有人要靠它。
        // 先算完账再搬字段：`ApplyUndo` 的部分字段被移走之后就不能再调方法了。
        let complete = undo.complete();
        let summary = undo.summary();
        let mut restored = undo.restored;
        restored.extend(undo.removed);
        let failed = undo.errors;
        for why in &failed {
            note(format!("  还原不了: {}", why));
        }
        if complete {
            let _ = std::fs::remove_dir_all(&dir);
            return ApplyRun {
                txn: Some(txn),
                restored,
                error: Some(error),
                ..Default::default()
            };
        }
        note(format!(
            "  留底留在 {}（{}）；没还原干净的那几个要人工处理",
            dir.display(),
            summary
        ));
        return ApplyRun {
            txn: Some(txn),
            dir: Some(dir),
            restored,
            error: Some(error),
            ..Default::default()
        };
    }

    // ── 5. 提交 ──
    // 记账失败不该把已经写好的文件退回：还原本身也是一次写，风险更大。
    // 文件已经在正确的位置，只需要把「这笔账没记上」说出来。
    if let Err(e) = write_apply_manifest(&dir, "committed", &txn, ws, &entries) {
        note(format!("  事务档案没更新: {}（文件已写回）", e));
    }
    if let Err(e) = journal.append(
        "apply/committed",
        Json::obj(vec![
            ("txn", Json::s(txn.as_str())),
            ("written", json_arr(&written)),
            ("backups", Json::s(dir.to_string_lossy().as_ref())),
        ]),
    ) {
        note(format!("  apply/committed 记不进日志: {}（文件已写回）", e));
    }
    ApplyRun {
        txn: Some(txn),
        dir: Some(dir),
        written,
        restored: Vec::new(),
        error: None,
    }
}

/// `ui apply --rollback [--txn ID] [--yes]`：把某次 apply 退回写回之前。
///
/// 默认退「最近一个还没退干净的事务」。只退「盘上仍然是事务当初写进去的那
/// 份」——apply 之后用户自己改过、或事后被删掉的文件不碰，留底也留着。
/// 退出码：0 = 退干净；4 = 有文件被跳过（内容又变过，要人工决定）；
/// 5 = 还原动作本身失败。
fn cmd_apply_rollback(
    args: &Args,
    ws: &Path,
    jp: &Path,
    json_out: bool,
    note: &dyn Fn(String),
) -> Result<i32, String> {
    let dirs = apply_txn_dirs(ws);
    if dirs.is_empty() {
        return Err(format!(
            "{} 里没有 apply 事务，没有可退的东西",
            apply_root(ws).display()
        ));
    }
    let (dir, manifest) = match args.get("txn") {
        Some(want) => {
            let dir = apply_root(ws).join(want);
            let manifest =
                read_apply_manifest(&dir).ok_or_else(|| format!("找不到事务 {} 的档案", want))?;
            (dir, manifest)
        }
        None => {
            // 从最新往回，找第一个还没退掉的事务
            let mut picked: Option<(PathBuf, Json)> = None;
            for dir in dirs.iter().rev() {
                let manifest = match read_apply_manifest(dir) {
                    Some(m) => m,
                    None => continue,
                };
                match manifest.str_at("status") {
                    Some("committed") | Some("applying") | Some("staged") => {
                        picked = Some((dir.clone(), manifest));
                        break;
                    }
                    _ => continue,
                }
            }
            picked.ok_or("最近的 apply 事务已经退回原样了")?
        }
    };
    let txn = txn_id_of(&dir, &manifest);
    let entries = apply_manifest_entries(&manifest);
    let status = manifest
        .str_at("status")
        .unwrap_or("未知")
        .to_string();
    note(format!(
        "事务 {}（档案状态 {}）· {} 个文件",
        txn,
        status,
        entries.len()
    ));

    let dry = !args.flag("yes");
    let undo = restore_apply_entries(ws, &dir, &entries, true, dry);
    for rel in &undo.restored {
        note(format!("  还原  {}", rel));
    }
    for rel in &undo.removed {
        note(format!("  删除  {}（事务新建的）", rel));
    }
    for why in &undo.skipped {
        note(format!("  保留  {}", why));
    }
    for why in &undo.errors {
        note(format!("  失败  {}", why));
    }

    if dry {
        note(format!(
            "\n（计划模式）{} · 加 --yes 才会真的退回",
            undo.summary()
        ));
    } else {
        // 退干净了才扔留底；跳过或失败的文件还要靠它做人
        if undo.complete() {
            let _ = std::fs::remove_dir_all(&dir);
        } else {
            let _ = write_apply_manifest(&dir, "partially-rolled-back", &txn, ws, &entries);
            note(format!("\n留底留在 {}", dir.display()));
        }
        // 还原已经落盘，日志只是记账：记不进去要说，但不能拦住还原。
        match Journal::open(jp) {
            Ok(mut journal) => {
                if let Err(e) = journal.append(
                    "apply/rolled-back",
                    Json::obj(vec![
                        ("txn", Json::s(txn.as_str())),
                        ("stage", Json::s("rollback")),
                        ("restored", json_arr(&undo.restored)),
                        ("removed", json_arr(&undo.removed)),
                        ("skipped", json_arr(&undo.skipped)),
                        ("failed", json_arr(&undo.errors)),
                    ]),
                ) {
                    note(format!("  apply/rolled-back 记不进日志: {}", e));
                }
            }
            Err(e) => note(format!(
                "  日志 {} 打不开: {}（还原已在磁盘上生效）",
                jp.display(),
                e
            )),
        }
        note(format!("\n{}", undo.summary()));
    }

    let exit = if !undo.errors.is_empty() {
        5
    } else if !undo.skipped.is_empty() {
        4
    } else {
        0
    };
    if json_out {
        println!(
            "{}",
            Json::obj(vec![
                ("ok", Json::Bool(exit == 0)),
                ("exit", Json::from(exit)),
                ("mode", Json::s(if dry { "plan" } else { "rollback" })),
                ("txn", Json::s(txn.as_str())),
                ("workspace", Json::s(ws.to_string_lossy().as_ref())),
                ("restored", json_arr(&undo.restored)),
                ("removed", json_arr(&undo.removed)),
                ("skipped", json_arr(&undo.skipped)),
                ("failed", json_arr(&undo.errors)),
            ])
            .to_compact()
        );
    }
    Ok(exit)
}

/// 把子代理的改动拷回工作区。
///
/// 退出码：0 = 顺利；3 = 有冲突（多源内容不同）；4 = 有漂移（父工作区被改过）；
/// 5 = 回写中途失败（已经把动过的文件全部还原）或上一次事务死在写回途中。
/// 冲突和漂移都有时返回 3（冲突更严重）。`--json` 把结果输出成一行 JSON。
///
/// `--yes` 不是「随便写」，是「开一个事务」：先给每个目标留原样，再写，
/// 写完逐个校验，任何一步不对就整批退回原样（见 `run_apply_transaction`）。
/// `--rollback [--txn ID]` 把某次 apply 退回去。
fn cmd_apply(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let json_out = args.flag("json");
    let jp = resolve_journal(args, &ws);

    // JSON 模式下人类可读信息走 stderr，stdout 只留一行 JSON
    let note = |msg: String| {
        if json_out {
            eprintln!("{}", msg);
        } else {
            say(msg);
        }
    };

    if args.flag("rollback") {
        return cmd_apply_rollback(args, &ws, &jp, json_out, &note);
    }

    let events = Journal::load(&jp).map_err(|e| e.to_string())?;

    let picked: Vec<Json> = if args.flag("all") {
        events
            .iter()
            .filter(|e| e.str_at("kind") == Some("delegate/end"))
            .cloned()
            .collect()
    } else {
        let id = args.get("id").ok_or("缺少 --id（或用 --all）")?;
        vec![events
            .iter()
            .rev()
            .find(|e| e.str_at("kind") == Some("delegate/end") && e.str_at("id") == Some(id))
            .ok_or_else(|| format!("日志里找不到子代理 {}", id))?
            .clone()]
    };

    // rel -> [(子代理 id, 副本工作区)]
    let mut by_path: std::collections::BTreeMap<String, Vec<(String, PathBuf)>> = Default::default();
    // 子代理删掉的文件：只报告，不自动删（删除不可逆）
    let mut deletions: Vec<String> = Vec::new();
    for rec in &picked {
        let id = rec.str_at("id").unwrap_or("?").to_string();
        let workdir = match rec.str_at("workdir") {
            Some(w) => PathBuf::from(w),
            None => {
                note(format!("子代理 {} 没有 workdir（当时没开 --isolate），跳过", id));
                continue;
            }
        };
        let changed: Vec<String> = rec
            .arr_at("changed")
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let deleted: Vec<String> = rec
            .arr_at("deleted")
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        // 删除不自动合并（不可逆），但必须**说出来**——否则父工作区会留下
        // 一个本该消失的文件，而日志假装无事发生。
        for rel in &deleted {
            note(format!(
                "删除  {}：子代理在副本里删掉了它；父工作区保留（删除不可逆，需人工处理）",
                rel
            ));
            deletions.push(rel.clone());
        }

        if changed.is_empty() {
            note(format!("子代理 {} 没有改动任何文件", id));
            continue;
        }
        for rel in changed {
            by_path
                .entry(rel)
                .or_default()
                .push((id.clone(), workdir.clone()));
        }
    }

    let force = args.flag("force");
    let write = args.flag("yes");
    let mut plan: Vec<(String, PathBuf)> = Vec::new();
    let mut conflicts: Vec<String> = Vec::new();
    let mut drifts: Vec<String> = Vec::new();

    for (rel, sources) in &by_path {
        let src = sources[0].1.join(rel);

        // 闸一：多个子代理都改过这个文件，内容必须一致
        if sources.len() > 1 {
            let first = std::fs::read(&src).ok();
            let identical = sources
                .iter()
                .all(|(_, w)| std::fs::read(w.join(rel)).ok() == first);
            if !identical {
                conflicts.push(rel.clone());
                note(format!(
                    "冲突  {}：{} 都改过且内容不同，跳过（需要人工合并）",
                    rel,
                    sources
                        .iter()
                        .map(|(i, _)| i.as_str())
                        .collect::<Vec<_>>()
                        .join(" / ")
                ));
                continue;
            }
        }

        // 闸二：父工作区里这个文件在子代理启动后被改过 / 删掉 / 新建过 → 漂移
        let expected = read_manifest(&sources[0].1).get(rel).cloned();
        // manifest 里新格式是 `xxh:<hex>`；旧格式（纯 64 位 hex）是 SHA-256
        let current = match expected.as_deref() {
            Some(e) if e.starts_with("xxh:") => ui_core::isolate::hash_file(&ws.join(rel)),
            _ => hash_file(&ws.join(rel)),
        };
        let drifted = match (&expected, &current) {
            (Some(e), Some(c)) => e != c,
            (Some(_), None) => true,
            (None, Some(_)) => true,
            (None, None) => false,
        };
        if drifted && !force {
            drifts.push(rel.clone());
            note(format!(
                "漂移  {}：子代理启动后工作区里这个文件变过，跳过（--force 可强制覆盖）",
                rel
            ));
            continue;
        }

        plan.push((rel.clone(), src));
    }

    let mut written: Vec<String> = Vec::new();
    let mut restored: Vec<String> = Vec::new();
    let mut txn: Option<String> = None;
    let mut backups: Option<PathBuf> = None;
    let mut apply_error: Option<String> = None;

    // 闸零：上一次事务死在写回途中时，工作区现在是「半份新内容」。这时候
    // 再叠一次写回，等于把残缺状态永久焊死，还查得出是哪一半新的——所以
    // 先停下来让人决定（退回原样，或确认就在残缺状态上继续）。
    let mut blocked: Option<String> = None;
    if write && !plan.is_empty() {
        if let Some((dir, dangling)) = unfinished_apply_txn(&ws) {
            if force {
                note(format!(
                    "上一次 apply 事务 {} 停在写回途中；--force 表示就在残缺状态上继续，已标记为 abandoned。",
                    dangling
                ));
                abandon_apply_txn(&ws, &jp, &dir, &dangling, &note);
            } else {
                note(format!(
                    "上一次 apply 事务 {} 停在写回途中（工作区可能只写了一半）。\n  先 `ui apply --rollback --txn {} --yes` 退回原样；确认要在残缺状态上继续，加 --force。",
                    dangling, dangling
                ));
                blocked = Some(dangling);
            }
        }
    }

    if write && !plan.is_empty() && blocked.is_none() {
        let run = run_apply_transaction(&ws, &jp, &plan, &note);
        txn = run.txn;
        backups = run.dir;
        written = run.written;
        restored = run.restored;
        apply_error = run.error;
    }

    note(format!(
        "计划回写 {} 个文件{}",
        plan.len(),
        if conflicts.is_empty() && drifts.is_empty() {
            String::new()
        } else {
            format!("，{} 个冲突、{} 个漂移待人工处理", conflicts.len(), drifts.len())
        }
    ));
    for (rel, _) in &plan {
        note(format!("  {}", rel));
    }
    if !write {
        note("\n（计划模式）加 --yes 才会真的写回工作区".to_string());
    } else if let Some(e) = &apply_error {
        note(format!(
            "\n回写失败：{}\n已自动还原 {} 个文件，工作区退回写回之前。",
            e,
            restored.len()
        ));
        if let Some(dir) = &backups {
            note(format!("留底留在 {}——有文件没能自动退干净，需要人工处理", dir.display()));
        }
    } else {
        note(format!("\n已写回 {} 个文件到 {}", written.len(), ws.display()));
        if let (Some(id), Some(dir)) = (&txn, &backups) {
            note(format!(
                "事务 {} · 原样留底在 {}\n想退回去：ui apply --rollback --txn {} --yes",
                id,
                dir.display(),
                id
            ));
        }
    }

    let exit = if blocked.is_some() || apply_error.is_some() {
        5
    } else if !conflicts.is_empty() {
        3
    } else if !drifts.is_empty() {
        4
    } else {
        0
    };

    if json_out {
        let would: Vec<String> = plan.iter().map(|(r, _)| r.clone()).collect();
        println!(
            "{}",
            Json::obj(vec![
                ("ok", Json::Bool(exit == 0)),
                ("exit", Json::from(exit)),
                ("workspace", Json::s(ws.to_string_lossy().as_ref())),
                ("wouldWrite", json_arr(&would)),
                ("written", json_arr(&written)),
                ("restored", json_arr(&restored)),
                ("conflicts", json_arr(&conflicts)),
                ("drift", json_arr(&drifts)),
                ("txn", match &txn { Some(id) => Json::s(id.as_str()), None => Json::Null }),
                ("unfinished", match &blocked { Some(id) => Json::s(id.as_str()), None => Json::Null }),
                ("error", match &apply_error { Some(e) => Json::s(e.as_str()), None => Json::Null }),
            ])
            .to_compact()
        );
    } else if by_path.is_empty() {
        note("没有可回写的改动".to_string());
    }

    Ok(exit)
}

const VIEWER: &str = include_str!("../assets/viewer.html");

fn cmd_replay(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let jp = resolve_journal(args, &ws);

    // 流式回放：逐行解析、用完就丢。
    // 之前 `Journal::load` 会把 7 万条事件（80 MB）全留在内存里，
    // 而回放本质上是"读一行、打一行"。
    let count = count_lines(&jp)?;

    if args.flag("json") {
        // --json 直接吐原始行，不解析——最快，也保证和磁盘上完全一致
        let file = File::open(&jp).map_err(|e| format!("{}: {}", jp.display(), e))?;
        let mut reader = BufReader::new(file);
        let mut buf = String::new();
        loop {
            buf.clear();
            let n = reader.read_line(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            let line = buf.trim_end();
            if !line.is_empty() {
                println!("{}", line);
            }
        }
        return Ok(0);
    }

    println!("{} 条事件 · {}", count, jp.display());
    let file = File::open(&jp).map_err(|e| format!("{}: {}", jp.display(), e))?;
    let mut reader = BufReader::new(file);
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        let line = buf.trim_end();
        if line.is_empty() {
            continue;
        }
        let event = match json::parse(line) {
            Ok(v) => v,
            Err(_) => continue, // 半条记录：verify 会明确指出，这里跳过
        };
        let row = render::event_row(&event);
        if !row.trim().is_empty() {
            println!("{}", row);
        }
        if event.str_at("kind") == Some("prompt/snapshot") {
            for line in render::section_rows(&event) {
                println!("{}", line);
            }
        }
    }
    Ok(0)
}

/// 数行数（不解析内容），用于回放前的那句"N 条事件"。
fn count_lines(path: &std::path::Path) -> Result<usize, String> {
    let file = File::open(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    let mut reader = BufReader::new(file);
    let mut n = 0usize;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let got = reader.read_until(b'\n', &mut buf).map_err(|e| e.to_string())?;
        if got == 0 {
            break;
        }
        if buf.iter().any(|b| !b.is_ascii_whitespace()) {
            n += 1;
        }
    }
    Ok(n)
}

fn cmd_policy(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let effect_kind = args.get("effect").ok_or("缺少 --effect")?;
    let target = args.get("target").ok_or("缺少 --target")?;
    let policy = Policy::new(Policy::defaults(&ws), &ws);
    let (decision, reason) = policy.explain(&Effect::new(effect_kind, target));
    println!("{} -> {} ({})", effect_kind, decision.name(), reason);
    println!("workspace {}", ws.display());
    // 隐私清单在策略**之前**起作用：工具层直接拒，规则表里的 Deny 只是
    // 第二道。人查一个 `--private` 路径时若只印 Deny 规则，会以为是某条
    // 模式命中的，其实是清单——两种「被挡」的可操作性不同，得说清。
    let path = std::path::Path::new(target);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        ws.join(path)
    };
    if ui_core::protected::is_private_path(&ws, &abs) {
        println!("隐私清单：命中——工具层在读到策略之前就已拒绝（--private / UI_PRIVATE 可追加）");
    }
    Ok(0)
}

/// 打印但不 panic：父进程死后 stdout 管道会 EPIPE，`println!` 会直接 panic，
/// 子代理就会在写 `turn/end` 之前死掉——取消传播也就断了。
fn say(msg: impl std::fmt::Display) {
    let mut out = io::stdout();
    let _ = writeln!(out, "{}", msg);
    let _ = out.flush();
}

struct ConsoleHooks {
    auto_yes: bool,
    allow_all: bool,
    /// `--json` 模式下进度一律走 stderr，stdout 只留结果 JSON
    stderr_only: bool,
}

impl ConsoleHooks {
    fn new(auto_yes: bool, stderr_only: bool) -> ConsoleHooks {
        ConsoleHooks {
            auto_yes,
            allow_all: false,
            stderr_only,
        }
    }

    fn emit(&self, msg: impl std::fmt::Display) {
        if self.stderr_only {
            eprintln!("{}", msg);
        } else {
            say(msg);
        }
    }
}

impl Hooks for ConsoleHooks {
    fn on_assistant(&mut self, text: &str) {
        self.emit(format!("\n\x1b[1m模型\x1b[0m {}", text));
    }

    fn on_tool(&mut self, name: &str, _args: &Json, result: &ToolResult) {
        let status = if result.ok { "ok" } else { "失败" };
        let first = result
            .error
            .clone()
            .unwrap_or_else(|| result.content.lines().next().unwrap_or("").to_string());
        say(format!("  → {:<9} {:<4} {}", name, status, first.trim()));
    }

    fn on_approval(&mut self, req: &ApprovalRequest) -> ApprovalResponse {
        if self.auto_yes || self.allow_all {
            self.emit(format!("  [自动批准] {}", req.tool));
            // 「以后都同意」确实是会话级的，但它的生命周期就是**这次进程**：
            // 报成 Session 让日志里能看出授权范围，而不是冒充一次性批准。
            // via=auto：这笔不是人当场点的，是 `--yes` 替人点的。
            return ApprovalResponse::allow_session().via("auto");
        }
        let effects: Vec<String> = req.effects.iter().map(|e| e.describe()).collect();
        self.emit(render::approval_prompt(&req.tool, &effects, &req.args));
        let mut line = String::new();
        // Ok(0) 是 EOF：stdin 是 /dev/null、管道已关、或非交互执行。
        // 这跟"读到了但听不懂"是两件事，混成一个记号会把"没人答"读成"答错了"。
        match io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => {
                // 读不到 stdin：按拒绝，但要说清"没人回答过"
                // ——否则日志里它和人手打了一个 `n` 一模一样。
                return ApprovalResponse::deny().via("no-input");
            }
            _ => {}
        }
        // 词表与内核共用一份：CLI、网页、脚本写的 JSON 里那些同义词，
        // 不该在不同入口得到不同结论。
        let answer = ApprovalResponse::parse_verdict(line.trim());
        // `a`（会话级批准）在本进程内继续免审——这是它的原意，不能因为换词表丢掉。
        if answer.decision == Decision::Allow && answer.scope == ApprovalScope::Session {
            self.allow_all = true;
        }
        answer
    }
}

struct UnavailableModel;

impl Model for UnavailableModel {
    fn name(&self) -> &str {
        "unavailable"
    }

    fn respond(
        &mut self,
        _s: &Snapshot,
        _r: Option<&str>,
        _c: &ui_core::cancel::Cancel,
    ) -> Result<ModelResponse, String> {
        Err("resume 不需要模型；这是内部错误".to_string())
    }
}


