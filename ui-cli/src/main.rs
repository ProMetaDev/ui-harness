//! `ui` — one binary, a handful of commands, no plugin registry.
//!
//!   ui doctor                 what the harness believes about this machine
//!   ui run --task "..."       run one turn (scripted scenario or external model)
//!   ui resume                 reconcile effects interrupted by a crash
//!   ui replay                 read the journal back, step by step
//!   ui policy                 explain a decision for an effect
//!   ui preview                render one full-screen frame (no tty needed)

mod frame;
mod render;
mod lab;
mod providers;
mod server;
mod term;

use ui_core::agent::{resume, Agent, ApprovalRequest, Hooks};
use ui_core::budget::Limits;
use ui_core::compact::CompactPolicy;
use ui_core::json::{self, Json};
use ui_core::journal::{analyze_incomplete, Journal};
use ui_core::model::{ExternalModel, Model, ModelResponse, ScriptedModel};
use ui_core::policy::{Decision, Effect, Policy};
use ui_core::prompt::{inspect_environment, Message, Snapshot};
use ui_core::tools::{registry, ToolResult};
use std::collections::HashMap;
use std::io::{self, Read, Write};
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
}

impl Args {
    fn parse(list: &[String]) -> (Option<String>, Args) {
        let mut values = HashMap::new();
        let mut flags = Vec::new();
        let mut rest: Vec<String> = Vec::new();
        let mut command: Option<String> = None;
        let mut i = 0;
        while i < list.len() {
            let item = &list[i];
            if let Some(key) = item.strip_prefix("--") {
                if i + 1 < list.len() && !list[i + 1].starts_with("--") {
                    values.insert(key.to_string(), list[i + 1].clone());
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
        (command, Args { values, flags, rest })
    }

    fn sub(&self, index: usize) -> Option<&str> {
        self.rest.get(index).map(String::as_str)
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

fn dispatch(argv: &[String]) -> Result<i32, String> {
    let (command, args) = Args::parse(argv);
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
        Some("skills") => cmd_skills(&args),
        Some("workflow") => cmd_workflow(&args),
        Some("team") => cmd_team(&args),
        Some("prompt") => cmd_prompt(&args),
        Some("providers") => cmd_providers(&args),
        Some("plugins") => cmd_plugins(&args),
        Some("sessions") => cmd_sessions(&args),
        Some("apply") => cmd_apply(&args),
        Some("audit") => cmd_audit(&args),
        Some("lab") => cmd_lab(&args),
        Some("policy") => cmd_policy(&args),
        Some(other) => Err(format!(
            "未知命令 `{}`（doctor|run|chat|preview|tui|resume|replay|verify|export|skills|workflow|apply|policy）",
            other
        )),
        None => {
            println!("{}", help());
            Ok(0)
        }
    }
}

fn help() -> String {
    "ui — 一个只认七条契约的 harness\n\n\
     ui doctor                    报告 harness 对这台机器的真实认知\n\
     ui run --task \"...\" --mock F 跑一轮（F 是剧本 JSON）\n\
     ui chat --mock F             交互式会话：多轮、同一份日志\n\
     ui serve --port 7788         网页端：浏览器里和模型对话（本地，零依赖）\n\
     ui resume                    对账崩溃打断的副作用\n\
     ui replay                    回放日志\n\
     ui verify                    日志自检：哈希链、未完成调用\n\
     ui export --html F           导出单文件轨迹查看器（双击即看）\n\
     ui skills                    列出工作区里的 skill\n\
     ui workflow --file F         按文件里的步骤跑工作流（每步一次 turn）\n\
     ui workflow --file F --isolate   并行子代理各用一份工作区副本\n\
     ui team --task \"复杂任务\" --agents 3   协作模式：自动拆成并行子任务再跑\n\
     ui apply --id ID [--yes]     把某个隔离子代理的改动写回工作区\n\
     ui audit --as strict         用另一个模式重判历史：哪些动作会被拦\n\
     ui lab pack --name e1        打包一次实验：产物 + 哈希 + 血缘 + 环境\n\
     ui lab claim --name acc --value 0.9   声明结论（必须被产物背书）\n\
     ui lab verify <包目录>       校验：产物哈希 / 输入版本 / 结论 / 日志链\n\
     ui lab table <包...>         多 seed 对照表（输入不一致拒绝求均值）\n\
     ui tui                       全屏终端界面\n\
     ui policy --effect fs.write --target /etc/passwd\n\
     ui preview --cols 84 --rows 22   渲染一帧界面（不需要终端）\n\n\
     通用参数: --workspace PATH  --task-file PATH  --max-tokens N  --max-calls N  --yes\n\
     模式:     --mode quiet | standard | strict\n\
     模型:     --mock 剧本.json   或   UI_MODEL_CMD=\"node adapters/deepseek.mjs\"\n\
     网页端:   --port N  --bundles 目录  --bench-cmd \"命令\"\n\
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
    println!("  shell   ：获得批准后，命令内部的读写/联网不受效果策略约束");
    println!("  plugin  ：效果按声明仲裁；声明之外的行为无法拦截");
    println!("  mcp_call：同上，外部 server 在进程内自主");
    println!("  结论    ：把它们当「可执行程序」，不是「沙箱里的受限代码」");

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
/// `.ui/plugins.json` 或 `--plugins FILE`：加载本地插件（能力声明必须显式）。
fn attach_plugins(agent: &mut Agent, args: &Args) -> Result<(), String> {
    let ws = agent.workspace.clone();
    let path = match args.get("plugins") {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let p = ws.join(".ui").join("plugins.json");
            if !p.exists() {
                return Ok(());
            }
            p
        }
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
    agent.compact = compact_policy(args);
    agent.tool_chars = tool_chars(args);
    agent.mode = mode_from(args);
    attach_mcp(&mut agent, args)?;
    attach_plugins(&mut agent, args)?;
    apply_tool_filter(&mut agent, args);
    if args.flag("no-env-scrub") {
        agent.env_scrub = false;
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
    agent.mode = mode_from(args);
    attach_mcp(&mut agent, args)?;
    attach_plugins(&mut agent, args)?;
    apply_tool_filter(&mut agent, args);
    if args.flag("no-env-scrub") {
        agent.env_scrub = false;
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

    server::serve(agent, &addr, ready, splash, args.flag("yes"), args.get("bundles"), args.get("bench-cmd"), args.get("token"), args.get("session"))?;
    Ok(0)
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
    agent.mode = mode_from(args);
    attach_mcp(&mut agent, args)?;
    attach_plugins(&mut agent, args)?;
    apply_tool_filter(&mut agent, args);
    if args.flag("no-env-scrub") {
        agent.env_scrub = false;
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

    fn on_approval(&mut self, req: &ApprovalRequest) -> Decision {
        if self.allow_all {
            self.state
                .lines
                .push(frame::Line::info(format!("[auto] 已批准 {}", req.tool)));
            self.state.rail.resolve(&req.call_id, "allow");
            self.redraw();
            return Decision::Allow;
        }
        self.state.approval = Some(frame::Approval {
            tool: req.tool.clone(),
            effects: req.effects.iter().map(|e| e.describe()).collect(),
            args: req.args.to_pretty(),
        });
        self.redraw();
        loop {
            match self.tui.wait_key() {
                Ok(term::Key::Char('y')) | Ok(term::Key::Char('Y')) => {
                    self.state.approval = None;
                    self.state.rail.resolve(&req.call_id, "allow");
                    self.redraw();
                    return Decision::Allow;
                }
                Ok(term::Key::Char('a')) | Ok(term::Key::Char('A')) => {
                    self.allow_all = true;
                    self.state.approval = None;
                    self.state.rail.resolve(&req.call_id, "allow");
                    self.redraw();
                    return Decision::Allow;
                }
                Ok(term::Key::Char('n'))
                | Ok(term::Key::Char('N'))
                | Ok(term::Key::Esc)
                | Ok(term::Key::CtrlC) => {
                    self.state.approval = None;
                    self.state.rail.resolve(&req.call_id, "deny");
                    self.redraw();
                    return Decision::Deny;
                }
                Ok(_) => {}
                Err(_) => {
                    self.state.rail.resolve(&req.call_id, "deny");
                    return Decision::Deny;
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
    let hub = ui_core::mcp::Hub::start(&cfg)?;
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
    args.get("tool-chars")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
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

    let model = build_model(args)?;

    let jp = resolve_journal(args, &ws);
    let journal = Journal::open(&jp).map_err(|e| e.to_string())?;
    println!("workspace {}", ws.display());
    println!("journal   {}", jp.display());

    let mut agent = Agent::new(journal, model, ws.clone(), limits);
    agent.compact = compact_policy(args);
    agent.tool_chars = tool_chars(args);
    agent.mode = mode_from(args);
    attach_mcp(&mut agent, args)?;
    attach_plugins(&mut agent, args)?;
    apply_tool_filter(&mut agent, args);
    if args.flag("no-env-scrub") {
        agent.env_scrub = false;
    }
    if let Some(text) = args.get("append-system-prompt") {
        ui_core::prompt::set_extra_system(text);
    }
    // `--skills a,b`：并行子代理或脚本化调用时，直接在开场前预加载
    preload_skills(&mut agent, &ws, &skill_list(args))?;
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

    // 结论对账：声明的每个数字都必须能在产物里找到
    let mut claims_ok = 0usize;
    let mut claims_total = 0usize;
    if let Ok(text) = std::fs::read_to_string(bundle.join("claims.json")) {
        if let Ok(doc) = json::parse(&text) {
            let leaves = numeric_leaves(&bundle.join("artifacts"));
            for c in doc.arr_at("claims").cloned().unwrap_or_default() {
                claims_total += 1;
                let name = c.str_at("name").unwrap_or("");
                let want = c.get("value").and_then(|v| v.as_f64()).unwrap_or(f64::NAN);
                let tol = (want.abs() * 1e-3).max(1e-9);
                let hit = want.is_finite() && leaves.values().any(|v| (v - want).abs() <= tol);
                if hit {
                    claims_ok += 1;
                } else {
                    bad.push(format!("结论 {} = {} 在产物里找不到对应数值", name, want));
                }
            }
        }
    }

    println!("实验包 {}", bundle.display());
    println!(
        "产物 {} 个校验通过 · 输入 {} 个仍一致 · 结论 {}/{} 对得上 · 日志链 {}{}",
        checked,
        input_checked,
        claims_ok,
        claims_total,
        if journal_ok { "完整" } else { "有问题" },
        if input_skipped { "（工作区不在本机，跳过输入校验）" } else { "" }
    );
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

/// 产物里的所有数值叶子（用于结论对账）
fn numeric_leaves(dir: &Path) -> std::collections::BTreeMap<String, f64> {
    fn walk(value: &Json, prefix: &str, out: &mut std::collections::BTreeMap<String, f64>) {
        match value {
            Json::Num(n) => {
                out.insert(prefix.to_string(), *n);
            }
            Json::Obj(items) => {
                for (k, v) in items {
                    let key = if prefix.is_empty() { k.clone() } else { format!("{}.{}", prefix, k) };
                    walk(v, &key, out);
                }
            }
            _ => {}
        }
    }
    let mut out = std::collections::BTreeMap::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().map(|e| e == "json").unwrap_or(false) {
                if let Ok(text) = std::fs::read_to_string(&p) {
                    if let Ok(v) = json::parse(&text) {
                        walk(&v, "", &mut out);
                    }
                }
            }
        }
    }
    out
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
    let snap = ui_core::prompt::build_snapshot(&ws, &tools, &[], None, None, "", &[]);
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
    let journal = Journal::open(&jp).map_err(|e| e.to_string())?;
    let inc = analyze_incomplete(&journal.events);
    println!("journal {}", jp.display());
    println!(
        "未完成: turn={} 工具={}",
        inc.open_turn.is_some(),
        inc.open_tools.len()
    );
    if inc.open_turn.is_none() && inc.open_tools.is_empty() {
        println!("没有需要恢复的工作");
        return Ok(0);
    }

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
    let (events, corrupt) = Journal::read(&jp).map_err(|e| e.to_string())?;

    match &corrupt {
        Some(c) if !c.torn => {
            println!("FAIL  日志损坏 seq={:?}: {}", c.seq, c.reason);
            println!("      文件 {}", jp.display());
            return Ok(2);
        }
        Some(c) => println!("WARN  尾部撕裂，可修: {}", c.reason),
        None => println!("ok    哈希链完整"),
    }

    let inc = analyze_incomplete(&events);
    println!("日志  {}", jp.display());
    println!("事件  {}", events.len());
    if let (Some(first), Some(last)) = (events.first(), events.last()) {
        println!(
            "序号  {}..{}",
            first.u64_at("seq").unwrap_or(0),
            last.u64_at("seq").unwrap_or(0)
        );
    }
    let turns = events
        .iter()
        .filter(|e| e.str_at("kind") == Some("turn/start"))
        .count();
    let calls = events
        .iter()
        .filter(|e| e.str_at("kind") == Some("tool/start"))
        .count();
    println!("轮次 {}  工具调用 {}", turns, calls);
    println!(
        "未完成 turn={} tools={}",
        inc.open_turn.is_some(),
        inc.open_tools.len()
    );
    if !inc.open_tools.is_empty() {
        println!("提示  有未完成副作用，跑 `ui resume` 对账");
    }
    Ok(0)
}

/// 导出单文件 HTML 轨迹查看器：数据内嵌，双击即看，可离线转发。
fn cmd_export(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let jp = resolve_journal(args, &ws);
    let events = Journal::load(&jp).map_err(|e| e.to_string())?;
    let limits = limits_from(args);

    let doc = Json::obj(vec![
        ("workspace", Json::from(ws.to_string_lossy().as_ref())),
        ("journal", Json::from(jp.to_string_lossy().as_ref())),
        (
            "limits",
            Json::obj(vec![
                ("maxTokens", Json::from(limits.tokens)),
                ("maxToolCalls", Json::from(limits.tool_calls)),
            ]),
        ),
        ("events", Json::Arr(events)),
    ]);

    // 内嵌 JSON 时转义 `</`，否则事件里出现 `</script>` 会把文档截断。
    let data = doc.to_compact().replace("</", "<\\/");
    let html = VIEWER.replace("__UI_DATA__", &data);

    let out = args.get("html").unwrap_or("out/trace.html");
    if let Some(parent) = Path::new(out).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(out, html).map_err(|e| format!("写文件失败: {}", e))?;
    let abs = std::fs::canonicalize(out).unwrap_or_else(|_| PathBuf::from(out));
    println!("已导出 {}", abs.display());
    if let Some(win) = windows_path(&abs) {
        println!("Windows 路径 {}", win);
        println!("双击打开；或把上一条路径粘进浏览器地址栏。");
    } else {
        println!("用浏览器打开它即可；数据已内嵌，可离线转发。");
    }
    Ok(0)
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
    let mut wf_args = Args {
        values: std::collections::HashMap::new(),
        flags: Vec::new(),
        rest: Vec::new(),
    };
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
    let mut apply_args = Args {
        values: std::collections::HashMap::new(),
        flags: Vec::new(),
        rest: Vec::new(),
    };
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
    agent.mode = mode_from(args);
    attach_mcp(&mut agent, args)?;
    attach_plugins(&mut agent, args)?;
    apply_tool_filter(&mut agent, args);
    if args.flag("no-env-scrub") {
        agent.env_scrub = false;
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
                        call_id: format!("workflow-{}", n),
                        tool: "workflow".to_string(),
                        args: Json::obj(vec![
                            ("step", Json::from(n)),
                            ("prompt", Json::from(task.prompt.as_str())),
                        ]),
                        effects: Vec::new(),
                        reason: format!("工作流「{}」第 {} 步需要确认", wf.name, n),
                    };
                    if hooks.on_approval(&req) != Decision::Allow {
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
}

/// 隔离副本里不需要的东西：日志、产物、依赖、node_modules
const ISOLATE_SKIP: &[&str] = &[".ui", "out", "target", "node_modules", ".git", "secrets"];
const MAX_COMPARE_BYTES: u64 = 2 * 1024 * 1024;

fn copy_file(from: &Path, to: &Path) -> std::io::Result<u64> {
    let size = from.metadata().map(|m| m.len()).unwrap_or(0);
    // 大文件先试写时复制（btrfs/xfs 上是 O(1)；ext4 会自动退化成普通复制）
    if size >= 1024 * 1024 {
        #[cfg(not(windows))]
        if let Ok(status) = std::process::Command::new("cp")
            .arg("--reflink=auto")
            .arg(from)
            .arg(to)
            .status()
        {
            if status.success() {
                return Ok(size);
            }
        }
    }
    std::fs::copy(from, to)?;
    Ok(size)
}

fn hash_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(ui_core::sha256::hex(&bytes))
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
            bytes += copy_file(&from, &to)?;
            files += 1;
            if let Ok(rel) = from.strip_prefix(root) {
                if let Some(h) = hash_file(&from) {
                    manifest.push((rel.to_string_lossy().to_string(), h));
                }
            }
        }
    }
    Ok((files, bytes))
}

fn collect_changed(base: &Path, root: &Path, dir: &Path, out: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if ISOLATE_SKIP.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_changed(base, root, &path, out);
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        let original = base.join(&rel);
        let meta = entry.metadata().ok();
        let too_big = meta.map(|m| m.len() > MAX_COMPARE_BYTES).unwrap_or(false);
        let same = if too_big {
            true // 大文件不逐字节比，按未改动处理
        } else {
            match (std::fs::read(&path), std::fs::read(&original)) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            }
        };
        if !same {
            out.push(rel.to_string_lossy().to_string());
        }
    }
}

fn changed_files(base: &Path, workdir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    collect_changed(base, workdir, workdir, &mut out);
    out.sort();
    out
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
                        let text = manifest
                            .iter()
                            .map(|(rel, h)| format!("{}\t{}", rel, h))
                            .collect::<Vec<_>>()
                            .join("\n");
                        let _ = std::fs::write(agent_root.join("manifest.txt"), text);
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

            let changed = workdir
                .as_ref()
                .map(|w| changed_files(&ws, w))
                .unwrap_or_default();

            ChildResult {
                id,
                ok,
                summary,
                journal,
                workdir,
                changed,
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

/// 把子代理的改动拷回工作区。
///
/// 退出码：0 = 顺利；3 = 有冲突（多源内容不同）；4 = 有漂移（父工作区被改过）。
/// 两者都有时返回 3（冲突更严重）。`--json` 把结果输出成一行 JSON。
fn cmd_apply(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let json_out = args.flag("json");
    let jp = resolve_journal(args, &ws);
    let events = Journal::load(&jp).map_err(|e| e.to_string())?;

    // JSON 模式下人类可读信息走 stderr，stdout 只留一行 JSON
    let note = |msg: String| {
        if json_out {
            eprintln!("{}", msg);
        } else {
            say(msg);
        }
    };

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
        let current = hash_file(&ws.join(rel));
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
    if write {
        for (rel, src) in &plan {
            if !src.is_file() {
                note(format!("  跳过（副本里已不存在）: {}", rel));
                continue;
            }
            let dst = ws.join(rel);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::copy(src, &dst).map_err(|e| format!("写回 {} 失败: {}", rel, e))?;
            written.push(rel.clone());
        }
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
    } else {
        note(format!("\n已写回 {} 个文件到 {}", written.len(), ws.display()));
    }

    let exit = if !conflicts.is_empty() {
        3
    } else if !drifts.is_empty() {
        4
    } else {
        0
    };

    if json_out {
        let arr = |items: &[String]| Json::Arr(items.iter().map(|s| Json::from(s.as_str())).collect());
        let would: Vec<String> = plan.iter().map(|(r, _)| r.clone()).collect();
        println!(
            "{}",
            Json::obj(vec![
                ("ok", Json::Bool(exit == 0)),
                ("exit", Json::from(exit)),
                ("workspace", Json::from(ws.to_string_lossy().to_string())),
                ("wouldWrite", arr(&would)),
                ("written", arr(&written)),
                ("conflicts", arr(&conflicts)),
                ("drift", arr(&drifts)),
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
    let events = Journal::load(&jp).map_err(|e| e.to_string())?;
    if args.flag("json") {
        for e in &events {
            println!("{}", e.to_compact());
        }
        return Ok(0);
    }
    println!("{} 条事件 · {}", events.len(), jp.display());
    let mut prev_snapshot: Option<Snapshot> = None;
    for e in &events {
        let row = render::event_row(e);
        if !row.trim().is_empty() {
            println!("{}", row);
        }
        if e.str_at("kind") == Some("prompt/snapshot") {
            for line in render::section_rows(e) {
                println!("{}", line);
            }
        }
        let _ = &prev_snapshot;
        let _ = &mut prev_snapshot;
    }
    Ok(0)
}

fn cmd_policy(args: &Args) -> Result<i32, String> {
    let ws = workspace(args);
    let effect_kind = args.get("effect").ok_or("缺少 --effect")?;
    let target = args.get("target").ok_or("缺少 --target")?;
    let policy = Policy::new(Policy::defaults(&ws), &ws);
    let (decision, reason) = policy.explain(&Effect::new(effect_kind, target));
    println!("{} -> {} ({})", effect_kind, decision.name(), reason);
    println!("workspace {}", ws.display());
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

    fn on_approval(&mut self, req: &ApprovalRequest) -> Decision {
        if self.auto_yes || self.allow_all {
            self.emit(format!("  [自动批准] {}", req.tool));
            return Decision::Allow;
        }
        let effects: Vec<String> = req.effects.iter().map(|e| e.describe()).collect();
        self.emit(render::approval_prompt(&req.tool, &effects, &req.args));
        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() {
            return Decision::Deny;
        }
        match line.trim() {
            "y" | "Y" => Decision::Allow,
            "a" | "A" => {
                self.allow_all = true;
                Decision::Allow
            }
            _ => Decision::Deny,
        }
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


