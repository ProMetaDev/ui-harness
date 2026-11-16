//! The tool surface: small, explicit, honest about effects.
//!
//! Each tool declares (a) the effects a *specific* call will have, (b) whether
//! repeating it after a crash is safe, (c) the parameters the prompt renders.
//! There is no capability "mode": effects are computed from the arguments.

use crate::json::Json;
use crate::policy::{Effect, FS_READ, FS_WRITE, PROC_EXEC};
use crate::skill::Catalog;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const MAX_OUTPUT_BYTES: usize = 16_000;

pub struct Ctx {
    pub workspace: PathBuf,
    /// 取消令牌：让长时间运行的工具（shell）能被 Ctrl-C 打断
    pub cancel: crate::cancel::Cancel,
    /// 外部 MCP 工具（没配置就是 None，`mcp_call` 会明确报错）
    pub mcp: Option<std::sync::Arc<std::sync::Mutex<crate::mcp::Hub>>>,
}

impl Default for Ctx {
    fn default() -> Self {
        Ctx {
            workspace: PathBuf::new(),
            cancel: crate::cancel::Cancel::new(),
            mcp: None,
        }
    }
}

impl Ctx {
    pub fn resolve(&self, p: &str) -> PathBuf {
        let path = Path::new(p);
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace.join(path)
        };
        // 在进入策略层之前折掉 `.`/`..`：否则 `<ws>/../etc/passwd` 仍然以
        // `<ws>/` 开头，`<ws>/**` 的 glob（`**` 跨分隔符）会把它当工作区内放行。
        lexical_normalize(&joined)
    }
}

/// 纯词法规范化，不碰文件系统，所以对尚不存在的写入目标也成立。
///
/// 相对路径一路 `..` 到根之上时保留 `..`，绝不悄悄变成「当前目录」——
/// 否则另一种形态的越界路径会重新匹配上工作区规则。
fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    // 没有 `..` 就不动：`./x` 这类写法要保持原样，否则相对 workspace
    // （库调用方可以传 "demo"、"."）构建出来的策略 pattern 会匹配不上。
    if !path
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return path.to_path_buf();
    }
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub ok: bool,
    pub content: String,
    pub error: Option<String>,
    pub meta: Json,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> ToolResult {
        ToolResult {
            ok: true,
            content: content.into(),
            error: None,
            meta: Json::Obj(vec![]),
        }
    }

    pub fn err(message: impl Into<String>) -> ToolResult {
        let m = message.into();
        ToolResult {
            ok: false,
            content: String::new(),
            error: Some(m),
            meta: Json::Obj(vec![]),
        }
    }
}

pub struct Tool {
    pub name: &'static str,
    pub summary: &'static str,
    pub idempotent: bool,
    pub params: &'static [(&'static str, &'static str, bool)],
    pub effects: fn(&Json, &Ctx) -> Vec<Effect>,
    pub run: fn(&Json, &Ctx) -> ToolResult,
}

#[cfg(unix)]
extern "C" {
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
}

/// 让命令自成进程组：超时/取消时能一次杀掉它和它的所有后代。
///
/// 只 kill 直接子进程时，`sleep 3 & wait` 这类命令会活到自然结束——
/// 超时这个硬约束就被一个后台进程绕过去了。
#[cfg(unix)]
fn isolate_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            if setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn isolate_group(_cmd: &mut Command) {}

/// 杀掉整棵进程树。负 pid 是「整个进程组」的约定。
fn kill_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        kill(-(pid as i32), 9);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn cap(text: &str) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_OUTPUT_BYTES;
    while !text.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!(
        "{}\n… [已截断到 {} 字节，用 offset/limit 继续读]",
        &text[..end], MAX_OUTPUT_BYTES
    )
}

fn arg_str(args: &Json, key: &str) -> Result<String, String> {
    args.str_at(key)
        .map(str::to_string)
        .ok_or_else(|| format!("缺少参数 `{}` (string)", key))
}

fn fs_read_effects(args: &Json, ctx: &Ctx) -> Vec<Effect> {
    let p = args.str_at("path").unwrap_or("");
    vec![Effect::new(FS_READ, ctx.resolve(p).to_string_lossy().to_string())]
}

fn fs_read_run(args: &Json, ctx: &Ctx) -> ToolResult {
    let rel = match arg_str(args, "path") {
        Ok(v) => v,
        Err(e) => return ToolResult::err(e),
    };
    let p = ctx.resolve(&rel);
    let meta = match fs::metadata(&p) {
        Ok(m) => m,
        Err(_) => return ToolResult::err(format!("文件不存在: {}", rel)),
    };
    if meta.is_dir() {
        return ToolResult::err(format!("这是一个目录: {}", rel));
    }
    // 整个文件读进内存：没有上限时，一个巨大的日志文件就能把进程撑爆。
    // 超过阈值就要求模型自己用 offset/limit 分页读（它本来就有这个参数）。
    const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;
    if meta.len() > MAX_READ_BYTES {
        return ToolResult::err(format!(
            "文件太大（{} 字节，上限 {}）：请用 offset/limit 分页读，或用 shell 的 head/tail/grep",
            meta.len(),
            MAX_READ_BYTES
        ));
    }
    let text = match fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) => return ToolResult::err(format!("读取失败: {}", e)),
    };
    let lines: Vec<&str> = text.lines().collect();
    let offset = args.u64_at("offset").unwrap_or(1).max(1) as usize;
    let limit = args.u64_at("limit").unwrap_or(200).max(1) as usize;
    let start = (offset - 1).min(lines.len());
    // offset/limit 来自模型参数：`u64::MAX` 会让 `start + limit` 溢出（debug
    // panic，release 回绕后 `lines[start..end]` 越界 panic）。
    let end = start.saturating_add(limit).min(lines.len());
    let mut body = String::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        body.push_str(&format!("{:5}  {}\n", start + i + 1, line));
    }
    if end < lines.len() {
        body.push_str(&format!("… 还有 {} 行\n", lines.len() - end));
    }
    ToolResult {
        ok: true,
        content: cap(&body),
        error: None,
        meta: Json::obj(vec![
            ("lines", Json::from(lines.len())),
            ("shown", Json::from(end - start)),
        ]),
    }
}

fn fs_write_effects(args: &Json, ctx: &Ctx) -> Vec<Effect> {
    let p = args.str_at("path").unwrap_or("");
    vec![Effect::new(FS_WRITE, ctx.resolve(p).to_string_lossy().to_string())]
}

fn fs_write_run(args: &Json, ctx: &Ctx) -> ToolResult {
    let rel = match arg_str(args, "path") {
        Ok(v) => v,
        Err(e) => return ToolResult::err(e),
    };
    let content = match arg_str(args, "content") {
        Ok(v) => v,
        Err(e) => return ToolResult::err(e),
    };
    let p = ctx.resolve(&rel);
    if let Some(parent) = p.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return ToolResult::err(format!("创建目录失败: {}", e));
        }
    }
    let tmp = p.with_extension(format!("ui-tmp-{}", std::process::id()));
    if let Err(e) = fs::write(&tmp, &content) {
        return ToolResult::err(format!("写入失败: {}", e));
    }
    if let Err(e) = fs::rename(&tmp, &p) {
        let _ = fs::remove_file(&tmp);
        return ToolResult::err(format!("落盘失败: {}", e));
    }
    ToolResult::ok(format!("写入 {} 字节到 {}", content.len(), p.display()))
}

fn fs_edit_effects(args: &Json, ctx: &Ctx) -> Vec<Effect> {
    let p = args.str_at("path").unwrap_or("");
    vec![Effect::new(FS_WRITE, ctx.resolve(p).to_string_lossy().to_string())]
}

fn fs_edit_run(args: &Json, ctx: &Ctx) -> ToolResult {
    let rel = match arg_str(args, "path") {
        Ok(v) => v,
        Err(e) => return ToolResult::err(e),
    };
    let old = match arg_str(args, "old") {
        Ok(v) => v,
        Err(e) => return ToolResult::err(e),
    };
    let new = match arg_str(args, "new") {
        Ok(v) => v,
        Err(e) => return ToolResult::err(e),
    };
    let replace_all = args.bool_at("replaceAll").unwrap_or(false);
    let p = ctx.resolve(&rel);
    let before = match fs::read_to_string(&p) {
        Ok(t) => t,
        Err(_) => return ToolResult::err(format!("文件不存在: {}", rel)),
    };
    let count = before.matches(&old).count();
    if count == 0 {
        return ToolResult::err("没找到要替换的原文");
    }
    if count > 1 && !replace_all {
        return ToolResult::err(format!(
            "原文出现 {} 次；请给更精确的片段，或设 replaceAll: true",
            count
        ));
    }
    let after = if replace_all {
        before.replace(&old, &new)
    } else {
        before.replacen(&old, &new, 1)
    };
    if let Err(e) = fs::write(&p, after) {
        return ToolResult::err(format!("写入失败: {}", e));
    }
    ToolResult::ok(format!("已编辑 {}（{} 处）", p.display(), count))
}

fn fs_search_effects(_args: &Json, ctx: &Ctx) -> Vec<Effect> {
    vec![Effect::new(
        FS_READ,
        ctx.workspace.to_string_lossy().to_string(),
    )]
}

fn fs_search_run(args: &Json, ctx: &Ctx) -> ToolResult {
    let pattern = match arg_str(args, "pattern") {
        Ok(v) => v,
        Err(e) => return ToolResult::err(e),
    };
    let matcher = Matcher::new(&pattern);
    let glob = args.str_at("glob").unwrap_or("**/*").to_string();
    let max = args.u64_at("maxResults").unwrap_or(60).min(200) as usize;
    let mut hits: Vec<String> = Vec::new();
    walk(&ctx.workspace, &mut |path, text| {
        // glob 一律按「相对 workspace」匹配。真实验收里模型写 `*.rs` 时，
        // 用绝对路径去比会一个都匹配不到，于是它只好整文件通读——token 就是这么烧掉的。
        let rel = path
            .strip_prefix(&ctx.workspace)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if hits.len() >= max || !crate::policy::glob_match(&glob, &rel) {
            return;
        }
        for (i, line) in text.lines().enumerate() {
            if hits.len() >= max {
                return;
            }
            if matcher.is_match(line) {
                let shown: String = line.trim().chars().take(200).collect();
                hits.push(format!(
                    "{}:{}: {}",
                    path.strip_prefix(&ctx.workspace)
                        .unwrap_or(path)
                        .to_string_lossy(),
                    i + 1,
                    shown
                ));
            }
        }
    });
    if hits.is_empty() {
        ToolResult::ok("没有匹配")
    } else {
        ToolResult::ok(cap(&hits.join("\n")))
    }
}

fn walk(dir: &Path, visit: &mut impl FnMut(&Path, &str)) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if matches!(
            name.as_str(),
            "node_modules" | ".git" | ".ui" | "target" | ".cargo"
        ) {
            continue;
        }
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            walk(&path, visit);
        } else if meta.len() < 512 * 1024 {
            if let Ok(text) = fs::read_to_string(&path) {
                visit(&path, &text);
            }
        }
    }
}

/// A tiny matcher: literal text plus `.` and `*`. Not a full regex engine —
/// no dependencies, and (unlike the old backtracking version) no exponential
/// worst case either.
pub struct Matcher {
    parts: Vec<Part>,
}

enum Part {
    Lit(char),
    Any,
    Star,
}

impl Matcher {
    pub fn new(pattern: &str) -> Matcher {
        // 前后各补一个 `*` 把「从任意位置开始、末尾允许剩余」的子串语义
        // 表达成整串匹配，然后用带星号回溯点的贪心算法。
        let mut parts = vec![Part::Star];
        for c in pattern.chars() {
            match c {
                '.' => parts.push(Part::Any),
                '*' => parts.push(Part::Star),
                c => parts.push(Part::Lit(c)),
            }
        }
        parts.push(Part::Star);
        Matcher { parts }
    }

    pub fn is_match(&self, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        let mut pi = 0usize;
        let mut ti = 0usize;
        // 最近一个 `*` 的位置，以及它当前吃到的文本位置。
        let mut star: Option<usize> = None;
        let mut star_ti = 0usize;
        loop {
            match self.parts.get(pi) {
                Some(Part::Star) => {
                    star = Some(pi);
                    star_ti = ti;
                    pi += 1;
                }
                Some(Part::Any) if ti < chars.len() => {
                    pi += 1;
                    ti += 1;
                }
                Some(Part::Lit(c)) if ti < chars.len() && chars[ti] == *c => {
                    pi += 1;
                    ti += 1;
                }
                _ => {
                    if pi >= self.parts.len() && ti == chars.len() {
                        return true;
                    }
                    // 让最近那个 `*` 多吃一个字符再试一次；没有星号就失败。
                    match star {
                        Some(sp) if star_ti < chars.len() => {
                            star_ti += 1;
                            ti = star_ti;
                            pi = sp + 1;
                        }
                        _ => return false,
                    }
                }
            }
        }
    }
}

fn shell_effects(args: &Json, _ctx: &Ctx) -> Vec<Effect> {
    vec![Effect::new(PROC_EXEC, args.str_at("command").unwrap_or(""))]
}

fn shell_run(args: &Json, ctx: &Ctx) -> ToolResult {
    let command = match arg_str(args, "command") {
        Ok(v) => v,
        Err(e) => return ToolResult::err(e),
    };
    let timeout = Duration::from_millis(args.u64_at("timeoutMs").unwrap_or(20_000).min(120_000));
    let started = Instant::now();

    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(&command);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = Command::new("sh");
        // 不用 -l：登录 shell 会 source profile，往输出里塞无关噪音（实测首行是 nvm）。
        c.arg("-c").arg(&command);
        c
    };

    // 自成进程组，超时/取消时才能连后代一起收掉
    isolate_group(&mut cmd);

    let mut child = match cmd
        .current_dir(&ctx.workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return ToolResult::err(format!("启动失败: {}", e)),
    };

    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out_handle = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let err_handle = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });

    let deadline = started + timeout;
    let mut timed_out = false;
    let status = loop {
        if ctx.cancel.is_cancelled() {
            kill_tree(child.id());
            let _ = child.wait();
            return ToolResult::err("cancelled：用户取消了这条命令");
        }
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    kill_tree(child.id());
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            Err(_) => break None,
        }
    };

    let out = out_handle.join().unwrap_or_default();
    let err = err_handle.join().unwrap_or_default();
    let duration = started.elapsed().as_millis() as u64;
    let combined = [out, err]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let code = status.and_then(|s| s.code()).unwrap_or(-1);
    ToolResult {
        ok: code == 0,
        content: cap(if combined.is_empty() {
            "(无输出)"
        } else {
            &combined
        }),
        error: if code == 0 {
            None
        } else {
            Some(format!("退出码 {}", code))
        },
        meta: Json::obj(vec![
            ("status", Json::from(code)),
            ("durationMs", Json::from(duration)),
            ("timedOut", Json::Bool(timed_out)),
        ]),
    }
}

fn skill_effects(_args: &Json, ctx: &Ctx) -> Vec<Effect> {
    vec![Effect::new(
        FS_READ,
        ctx.workspace.to_string_lossy().to_string(),
    )]
}

fn skill_run(args: &Json, ctx: &Ctx) -> ToolResult {
    let name = match arg_str(args, "name") {
        Ok(v) => v,
        Err(e) => return ToolResult::err(e),
    };
    let cat = Catalog::discover(&ctx.workspace);
    if cat.is_empty() {
        return ToolResult::err("这个工作区没有 skill（放在 skills/<名字>/SKILL.md）");
    }
    match cat.find(&name) {
        Some(s) => ToolResult::ok(s.body.clone()),
        None => {
            let names = cat
                .skills
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join("、");
            ToolResult::err(format!("没有叫 {} 的 skill。可用：{}", name, names))
        }
    }
}

pub fn registry() -> Vec<Tool> {
    vec![
        Tool {
            name: "mcp_call",
            summary: "调用外部 MCP 工具：{server, tool, args}（工具名见提示里的 MCP 清单）",
            idempotent: false,
            params: &[
                ("server", "string", true),
                ("tool", "string", true),
                ("args", "object", false),
            ],
            effects: |args, _ctx| {
                let server = args.str_at("server").unwrap_or("");
                let tool = args.str_at("tool").unwrap_or("");
                vec![Effect::new("proc.exec", format!("mcp:{}:{}", server, tool))]
            },
            run: |args, ctx| {
                let server = args.str_at("server").unwrap_or("");
                let tool = args.str_at("tool").unwrap_or("");
                if server.is_empty() || tool.is_empty() {
                    return ToolResult::err("mcp_call 需要 server 与 tool");
                }
                let hub = match &ctx.mcp {
                    Some(h) => h,
                    None => {
                        return ToolResult::err(
                            "没有配置 MCP server（启动时加 --mcp <config.json>）",
                        )
                    }
                };
                let payload = args.get("args").cloned().unwrap_or(Json::Obj(vec![]));
                let mut guard = match hub.lock() {
                    Ok(g) => g,
                    Err(_) => return ToolResult::err("MCP 锁已损坏"),
                };
                match guard.call(server, tool, payload) {
                    Ok(text) => ToolResult::ok(text),
                    Err(e) => ToolResult::err(e),
                }
            },
        },
        Tool {
            name: "fs_read",
            summary: "读 UTF-8 文本文件，可按行取窗口",
            idempotent: true,
            params: &[
                ("path", "string", true),
                ("offset", "int", false),
                ("limit", "int", false),
            ],
            effects: fs_read_effects,
            run: fs_read_run,
        },
        Tool {
            name: "fs_write",
            summary: "创建或整体覆盖一个文件（原子写）",
            idempotent: false,
            params: &[("path", "string", true), ("content", "string", true)],
            effects: fs_write_effects,
            run: fs_write_run,
        },
        Tool {
            name: "fs_edit",
            summary: "替换文件里一处精确原文",
            idempotent: false,
            params: &[
                ("path", "string", true),
                ("old", "string", true),
                ("new", "string", true),
                ("replaceAll", "bool", false),
            ],
            effects: fs_edit_effects,
            run: fs_edit_run,
        },
        Tool {
            name: "fs_search",
            summary: "按模式搜索文本（. 和 * 是通配）",
            idempotent: true,
            params: &[
                ("pattern", "string", true),
                ("glob", "string", false),
                ("maxResults", "int", false),
            ],
            effects: fs_search_effects,
            run: fs_search_run,
        },
        Tool {
            name: "skill",
            summary: "按名字加载一个 skill 的正文（目录见提示）",
            idempotent: true,
            params: &[("name", "string", true)],
            effects: skill_effects,
            run: skill_run,
        },
        Tool {
            name: "shell",
            summary: "执行一条命令",
            idempotent: false,
            params: &[("command", "string", true), ("timeoutMs", "int", false)],
            effects: shell_effects,
            run: shell_run,
        },
    ]
}

pub fn find<'a>(tools: &'a [Tool], name: &str) -> Option<&'a Tool> {
    tools.iter().find(|t| t.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        let dir = std::env::temp_dir().join(format!("ui-tools-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        Ctx { workspace: dir, ..Default::default() }
    }

    #[test]
    fn writes_then_reads_then_edits() {
        let c = ctx();
        let tools = registry();

        let r = (find(&tools, "fs_write").unwrap().run)(
            &Json::obj(vec![
                ("path", Json::from("a.txt")),
                ("content", Json::from("hello\nworld\n")),
            ]),
            &c,
        );
        assert!(r.ok, "{:?}", r.error);

        let r = (find(&tools, "fs_read").unwrap().run)(
            &Json::obj(vec![
                ("path", Json::from("a.txt")),
                ("offset", Json::from(2u64)),
            ]),
            &c,
        );
        assert!(r.ok);
        assert!(r.content.contains("world"));

        let r = (find(&tools, "fs_edit").unwrap().run)(
            &Json::obj(vec![
                ("path", Json::from("a.txt")),
                ("old", Json::from("world")),
                ("new", Json::from("ui")),
            ]),
            &c,
        );
        assert!(r.ok, "{:?}", r.error);
    }

    #[test]
    fn edit_refuses_ambiguous_match() {
        let c = ctx();
        let tools = registry();
        (find(&tools, "fs_write").unwrap().run)(
            &Json::obj(vec![
                ("path", Json::from("dup.txt")),
                ("content", Json::from("aa bb aa")),
            ]),
            &c,
        );
        let r = (find(&tools, "fs_edit").unwrap().run)(
            &Json::obj(vec![
                ("path", Json::from("dup.txt")),
                ("old", Json::from("aa")),
                ("new", Json::from("cc")),
            ]),
            &c,
        );
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("2 次"));
    }

    #[test]
    fn search_glob_matches_relative_paths() {
        let c = ctx();
        (find(&registry(), "fs_write").unwrap().run)(
            &Json::obj(vec![
                ("path", Json::from("sub/one.rs")),
                ("content", Json::from("fn target_here() {}\n")),
            ]),
            &c,
        );
        // `*.rs` 只匹配当前层；`**/*.rs` 递归。两者都不该受绝对路径影响。
        let r = (find(&registry(), "fs_search").unwrap().run)(
            &Json::obj(vec![("pattern", Json::from("target_here"))]),
            &c,
        );
        assert!(r.ok, "{:?}", r.error);
        assert!(r.content.contains("sub/one.rs"), "{}", r.content);
    }

    #[test]
    fn shell_reports_exit_code() {
        let c = ctx();
        let tools = registry();
        let r = (find(&tools, "shell").unwrap().run)(
            &Json::obj(vec![("command", Json::from("exit 3"))]),
            &c,
        );
        assert!(!r.ok);
        assert_eq!(r.meta.num_at("status"), Some(3.0));
    }
}
