//! 零依赖的本地 Web 用户端：一个页面、一条 SSE 事件流、一个审批回执口。
//!
//! 为什么要自己写 HTTP：内核零依赖是硬约束（构建环境不可靠）。这里的解析器只认
//! 「请求行 + 头 + Content-Length 正文」，够用一个聊天页面，也就一屏代码。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

use ui_core::agent::{ApprovalRequest, Hooks};
use ui_core::json::{self, Json};
use ui_core::prompt::Snapshot;

use ui_core::tools::ToolResult;
use ui_core::{Agent, Decision};

const CHAT_HTML: &str = include_str!("../assets/chat.html");

#[derive(Default)]
struct Hub {
    /// 已广播的事件（序号 + JSON 行），新连接用 `?since=` 补齐，重连不重复
    log: Mutex<Vec<(u64, String)>>,
    /// 事件序号
    seq: std::sync::atomic::AtomicU64,
    /// 每个 SSE 连接一个发送端
    subs: Mutex<Vec<Sender<String>>>,
    /// 等待浏览器回答的审批
    approval: Mutex<Option<Sender<Decision>>>,
    /// 工作线程的取消令牌：页面上的「停止」直接打这里
    cancel: Mutex<Option<ui_core::Cancel>>,
    /// 工具的完整输出（页面上点开再看）：按调用序号存
    tools: Mutex<Vec<Json>>,
    /// 启动检查结果：开机界面照它逐条点亮（真实结果，不是定时器）
    ready: Mutex<Option<Json>>,
    /// 是否播放开机动画（后台/脚本里关掉）
    splash: AtomicBool,
    /// 当前模式：页面切换后下一轮生效（只改严格度，不改日志格式）
    mode: Mutex<ui_core::Mode>,
    /// 当前会话名
    session: Mutex<String>,
    /// 待切换的会话：worker 在下一轮开始前应用
    pending_session: Mutex<Option<String>>,
    /// 工作区（会话文件按它解析）
    workspace: std::path::PathBuf,
    /// 实验包目录（默认 <workspace>/experiments）
    bundles: std::path::PathBuf,
    /// 访问令牌（空 = 不校验）
    token: String,
    /// 「跑一次」要执行的命令（None = 没有配置，按钮禁用）
    bench: Option<String>,
    lab_busy: AtomicBool,
    busy: AtomicBool,
}

impl Hub {
    fn push(&self, mut event: Json) {
        let n = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        event.set("seq", Json::from(n));
        event.set("ts", Json::from(ui_core::time::now_ms()));
        let line = event.to_compact();
        self.log.lock().unwrap().push((n, line.clone()));
        let mut subs = self.subs.lock().unwrap();
        subs.retain(|tx| tx.send(line.clone()).is_ok());
    }

    fn answer(&self, decision: Decision) {
        if let Some(tx) = self.approval.lock().unwrap().take() {
            let _ = tx.send(decision);
        }
    }

    /// 存一份完整工具输出，返回给页面用的 id
    fn store_tool(&self, name: &str, result: &ToolResult) -> u64 {
        let mut tools = self.tools.lock().unwrap();
        tools.push(Json::obj(vec![
            ("name", Json::from(name)),
            ("ok", Json::Bool(result.ok)),
            (
                "text",
                Json::from(match &result.error {
                    Some(e) if result.content.trim().is_empty() => e.clone(),
                    Some(e) => format!("{}\n\n[错误] {}", result.content, e),
                    None => result.content.clone(),
                }),
            ),
        ]));
        tools.len() as u64
    }

    fn tool(&self, id: u64) -> Option<Json> {
        self.tools.lock().unwrap().get((id as usize).checked_sub(1)?).cloned()
    }
}

struct WebHooks {
    hub: Arc<Hub>,
    /// `ui serve --yes`：自动批准，不再等浏览器（脚本/演示用）
    auto_yes: bool,
}

impl Hooks for WebHooks {
    fn on_delta(&mut self, text: &str) {
        // 逐字增量：页面先追加到当前气泡，最终的 assistant 事件会整段替换它。
        if text.is_empty() {
            return;
        }
        self.hub.push(Json::obj(vec![
            ("type", Json::from("delta")),
            ("text", Json::from(text)),
        ]));
    }

    fn on_assistant(&mut self, text: &str) {
        self.hub.push(Json::obj(vec![
            ("type", Json::from("assistant")),
            ("text", Json::from(text)),
        ]));
    }

    fn on_snapshot(&mut self, snapshot: &Snapshot) {
        // 右栏「当前步」的数据源：分段 + 计价，和 journal 同一处产生。
        self.hub.push(Json::obj(vec![
            ("type", Json::from("snapshot")),
            ("id", Json::from(snapshot.id.as_str())),
            ("tokens", Json::from(snapshot.tokens)),
            ("bytes", Json::from(snapshot.bytes)),
            (
                "sections",
                Json::Arr(
                    snapshot
                        .sections
                        .iter()
                        .map(|s| {
                            Json::obj(vec![
                                ("id", Json::from(s.id)),
                                ("owner", Json::from(s.owner)),
                                ("tokens", Json::from(s.tokens)),
                                ("bytes", Json::from(s.bytes)),
                            ])
                        })
                        .collect(),
                ),
            ),
        ]));
    }

    fn on_decision(&mut self, call_id: &str, tool: &str, decision: Decision, reason: &str) {
        self.hub.push(Json::obj(vec![
            ("type", Json::from("decision")),
            ("callId", Json::from(call_id)),
            ("tool", Json::from(tool)),
            ("decision", Json::from(decision.name())),
            ("reason", Json::from(reason)),
        ]));
    }

    fn on_tool(&mut self, name: &str, _args: &Json, result: &ToolResult) {
        let detail = result
            .error
            .clone()
            .unwrap_or_else(|| result.content.lines().next().unwrap_or("").to_string());
        // 完整输出单独存：列表里只发摘要，点开时再按 id 取
        let id = self.hub.store_tool(name, result);
        self.hub.push(Json::obj(vec![
            ("type", Json::from("tool")),
            ("name", Json::from(name)),
            ("ok", Json::Bool(result.ok)),
            ("detail", Json::from(detail.trim())),
            ("id", Json::from(id)),
        ]));
    }

    fn on_budget(&mut self, tokens: u64, calls: u64) {
        self.hub.push(Json::obj(vec![
            ("type", Json::from("budget")),
            ("tokens", Json::from(tokens)),
            ("calls", Json::from(calls)),
        ]));
    }

    fn on_compact(&mut self, before: u64, after: u64, dropped: usize) {
        self.hub.push(Json::obj(vec![
            ("type", Json::from("compact")),
            ("before", Json::from(before)),
            ("after", Json::from(after)),
            ("dropped", Json::from(dropped)),
        ]));
    }

    fn on_approval(&mut self, req: &ApprovalRequest) -> Decision {
        if self.auto_yes {
            self.hub.push(Json::obj(vec![
                ("type", Json::from("approved")),
                ("verdict", Json::from("auto")),
                ("callId", Json::from(req.call_id.as_str())),
            ]));
            return Decision::Allow;
        }
        let (tx, rx) = channel::<Decision>();
        *self.hub.approval.lock().unwrap() = Some(tx);
        self.hub.push(Json::obj(vec![
            ("type", Json::from("approval")),
            ("callId", Json::from(req.call_id.as_str())),
            ("tool", Json::from(req.tool.as_str())),
            (
                "effects",
                Json::Arr(
                    req.effects
                        .iter()
                        .map(|e| Json::from(e.describe()))
                        .collect(),
                ),
            ),
            ("args", Json::from(req.args.to_pretty())),
            ("reason", Json::from(req.reason.as_str())),
        ]));
        // 浏览器不回答就按拒绝处理；「停止」也要能打断这个等待，否则按钮没反应。
        let started = std::time::Instant::now();
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(decision) => {
                    self.hub.push(Json::obj(vec![
                        ("type", Json::from("approved")),
                        ("verdict", Json::from(decision.name())),
                        ("callId", Json::from(req.call_id.as_str())),
                    ]));
                    return decision;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Decision::Deny,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let cancelled = self
                        .hub
                        .cancel
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|c| c.is_cancelled())
                        .unwrap_or(false);
                    if cancelled {
                        self.hub.push(Json::obj(vec![
                            ("type", Json::from("approved")),
                            ("verdict", Json::from("cancelled")),
                            ("callId", Json::from(req.call_id.as_str())),
                        ]));
                        return Decision::Deny;
                    }
                    if started.elapsed().as_secs() > 300 {
                        return Decision::Deny;
                    }
                }
            }
        }
    }
}

pub fn serve(
    agent: Agent,
    addr: &str,
    ready: Json,
    splash: bool,
    auto_yes: bool,
    bundles: Option<&str>,
    bench: Option<&str>,
    token: Option<&str>,
    session_arg: Option<&str>,
) -> Result<(), String> {
    let mut hub0 = Hub::default();
    hub0.bench = bench.map(str::to_string);
    hub0.token = match token {
        // `--token off` 明确表示不校验（本地脚本/截图用）
        Some("off") | Some("none") => String::new(),
        Some(t) => t.to_string(),
        None => make_token(),
    };
    hub0.bundles = match bundles {
        Some(p) => std::path::PathBuf::from(p),
        None => std::path::Path::new(ready.str_at("workspace").unwrap_or(".")).join("experiments"),
    };
    hub0.workspace = std::path::PathBuf::from(ready.str_at("workspace").unwrap_or("."));
    *hub0.session.lock().unwrap() = session_arg.unwrap_or("default").to_string();
    let hub = Arc::new(hub0);
    let (task_tx, task_rx) = channel::<String>();
    hub.splash.store(splash, Ordering::SeqCst);
    *hub.mode.lock().unwrap() = agent.mode;

    let model_label = ready.str_at("model").unwrap_or("").to_string();
    let ws_label = ready.str_at("workspace").unwrap_or("").to_string();
    *hub.ready.lock().unwrap() = Some(ready);

    // 开场一条会话信息：页面用它显示模型与工作区，快照里也留得下
    hub.push(Json::obj(vec![
        ("type", Json::from("session")),
        ("model", Json::from(model_label.as_str())),
        ("workspace", Json::from(ws_label.as_str())),
    ]));

    // Agent 独占一个工作线程：HTTP 线程只负责转发
    {
        let hub = Arc::clone(&hub);
        let mut agent = agent;
        // 「停止」按钮要用它：令牌是 Arc 里的原子标志，跨线程安全
        *hub.cancel.lock().unwrap() = Some(agent.cancel.clone());
        std::thread::spawn(move || {
            while let Ok(text) = task_rx.recv() {
                hub.busy.store(true, Ordering::SeqCst);
                agent.mode = *hub.mode.lock().unwrap();
                // 会话切换：换日志 + 重建历史（不能把上一条会话的上下文带过来）
                if let Some(name) = hub.pending_session.lock().unwrap().take() {
                    let path = session_file(&hub.workspace, &name);
                    match agent.switch_journal(&path) {
                        Ok(()) => {
                            *hub.session.lock().unwrap() = name.clone();
                            hub.push(Json::obj(vec![
                                ("type", Json::from("session-switched")),
                                ("session", Json::from(name.as_str())),
                            ]));
                        }
                        Err(e) => {
                            hub.push(Json::obj(vec![
                                ("type", Json::from("error")),
                                ("message", Json::from(format!("切换会话失败: {}", e))),
                            ]));
                        }
                    }
                }
                let mut hooks = WebHooks {
                    hub: Arc::clone(&hub),
                    auto_yes,
                };
                match agent.run_turn(&text, &mut hooks) {
                    Ok(report) => hub.push(Json::obj(vec![
                        ("type", Json::from("turn")),
                        ("reason", Json::from(report.reason.as_str())),
                        ("steps", Json::from(report.steps)),
                        ("calls", Json::from(report.tool_calls)),
                    ])),
                    Err(e) => hub.push(Json::obj(vec![
                        ("type", Json::from("error")),
                        ("message", Json::from(e.to_string())),
                    ])),
                }
                hub.busy.store(false, Ordering::SeqCst);
            }
        });
    }

    let listener = TcpListener::bind(addr).map_err(|e| format!("监听 {} 失败: {}", addr, e))?;
    if hub.token.is_empty() {
        println!("UI 已启动  http://{}", addr);
        println!("鉴权       已关闭（--token off）：本机任何程序都能调用");
    } else {
        println!("UI 已启动  http://{}/?t={}", addr, hub.token);
        println!("鉴权       需要 token：首次带 ?t= 打开，之后浏览器用 Cookie");
    }
    println!("工作区     {}", agent_workspace_hint());
    println!("Ctrl-C 退出");

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let hub = Arc::clone(&hub);
        let task_tx = task_tx.clone();
        std::thread::spawn(move || {
            let _ = handle(stream, hub, task_tx);
        });
    }
    Ok(())
}

/// 工作区在启动时已经打印过；这里只留一句提示（避免泄漏绝对路径给页面）
fn agent_workspace_hint() -> &'static str {
    "(见启动参数 --workspace)"
}

fn handle(mut stream: TcpStream, hub: Arc<Hub>, task_tx: Sender<String>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    // 路由只看路径：查询串留给页面自己（例如 ?shot=1）
    let raw_path = parts.next().unwrap_or("/").to_string();
    let path = raw_path.split('?').next().unwrap_or("/").to_string();

    let mut content_length = 0usize;
    let mut headers: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let h = header.trim();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
        if let Some(rest) = h.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = rest.trim().parse().unwrap_or(0);
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    let body = String::from_utf8_lossy(&body).to_string();

    // 查询串：`since=<seq>` 用于断线重连只补新事件
    let query = raw_path.split_once('?').map(|(_, q)| q).unwrap_or("");

    // ── 鉴权 ──
    // 绑的是 127.0.0.1，局域网进不来；真正的威胁是「本机其它程序」和
    // 「你浏览器里访问的恶意网页能 POST 到 localhost」。所以默认要求 token。
    if !hub.token.is_empty() {
        let bearer = headers
            .get("cookie")
            .map(|c| c.contains(&format!("ui_token={}", hub.token)))
            .unwrap_or(false);
        let from_query = query
            .split('&')
            .any(|kv| kv == format!("t={}", hub.token));
        if !bearer && !from_query {
            return unauthorized(&mut stream, path.starts_with("/api"));
        }
        if from_query && !bearer {
            // 首次带 ?t= 进来：种 Cookie 并跳回同路径，URL 里不留 token
            let set = format!(
                "HTTP/1.1 303 See Other\r\nLocation: {}\r\nSet-Cookie: ui_token={}; Path=/; HttpOnly; SameSite=Lax\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                path, hub.token
            );
            return stream.write_all(set.as_bytes());
        }
    }

    match (method.as_str(), path.as_str()) {
        ("GET", "/") => html(&mut stream, CHAT_HTML),
        // 静态快照：事件内嵌，不开 SSE。可分享、可截图、可离线打开。
        ("GET", "/snapshot") => {
            let boot = hub
                .log
                .lock()
                .unwrap()
                .iter()
                .map(|(_, line)| line.clone())
                .collect::<Vec<_>>()
                .join(",");
            let ready = hub.ready.lock().unwrap().clone().unwrap_or(Json::Null);
            let inject = format!(
                "<script>window.__UI_BOOT__=[{}];window.__UI_READY__={};</script>\n</head>",
                boot,
                ready.to_compact()
            );
            let page = CHAT_HTML.replace("</head>", &inject);
            html(&mut stream, &page)
        }
        ("GET", "/api/ready") => {
            let payload = hub
                .ready
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(Json::Null);
            let mut with_splash = payload;
            with_splash.set("splash", Json::Bool(hub.splash.load(Ordering::SeqCst)));
            json_response(&mut stream, &with_splash.to_compact())
        }
        // 侧栏用的元信息：工具、技能、模式、工作区根
        ("GET", "/api/meta") => {
            let ready = hub.ready.lock().unwrap().clone().unwrap_or(Json::Null);
            let tools = ui_core::tools::registry()
                .iter()
                .map(|t| {
                    Json::obj(vec![
                        ("name", Json::from(t.name)),
                        ("summary", Json::from(t.summary)),
                    ])
                })
                .collect::<Vec<_>>();
            let ws = ready.str_at("workspace").unwrap_or("");
            let skills = ui_core::skill::Catalog::discover(std::path::Path::new(ws))
                .skills
                .iter()
                .map(|s| {
                    Json::obj(vec![
                        ("name", Json::from(s.name.as_str())),
                        ("description", Json::from(s.description.as_str())),
                    ])
                })
                .collect::<Vec<_>>();
            let meta = Json::obj(vec![
                ("model", Json::from(ready.str_at("model").unwrap_or(""))),
                ("workspace", Json::from(ws)),
                ("budget", ready.get("budget").cloned().unwrap_or(Json::Null)),
                ("tools", Json::Arr(tools)),
                ("skills", Json::Arr(skills)),
            ]);
            json_response(&mut stream, &meta.to_compact())
        }
        // 工作区文件树（只读、限深限宽，拒绝越界路径）
        ("GET", "/api/tree") => {
            let ready = hub.ready.lock().unwrap().clone().unwrap_or(Json::Null);
            let root = std::path::PathBuf::from(ready.str_at("workspace").unwrap_or(""));
            let rel = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("path="))
                .unwrap_or("")
                .replace("%2F", "/");
            if rel.contains("..") {
                return json_response(&mut stream, "{\"entries\":[],\"error\":\"路径越界\"}");
            }
            let dir = if rel.is_empty() {
                root.clone()
            } else {
                root.join(&rel)
            };
            let mut entries: Vec<Json> = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten().take(300) {
                    let name = e.file_name().to_string_lossy().to_string();
                    if name == ".git" || name == "target" || name == "node_modules" {
                        continue;
                    }
                    let meta = e.metadata().ok();
                    entries.push(Json::obj(vec![
                        ("name", Json::from(name.as_str())),
                        (
                            "dir",
                            Json::Bool(meta.as_ref().map(|m| m.is_dir()).unwrap_or(false)),
                        ),
                        (
                            "size",
                            Json::from(meta.as_ref().map(|m| m.len()).unwrap_or(0)),
                        ),
                    ]));
                }
            }
            entries.sort_by(|a, b| {
                let ad = a.bool_at("dir") == Some(true);
                let bd = b.bool_at("dir") == Some(true);
                bd.cmp(&ad).then_with(|| {
                    a.str_at("name")
                        .unwrap_or("")
                        .cmp(b.str_at("name").unwrap_or(""))
                })
            });
            let out = Json::obj(vec![
                ("path", Json::from(rel.as_str())),
                ("entries", Json::Arr(entries)),
            ]);
            json_response(&mut stream, &out.to_compact())
        }
        // 实验室：列出实验包并逐个校验（校验逻辑与 CLI 共用 ui-cli/src/lab.rs）
        ("GET", "/api/lab/bundles") => {
            let list = crate::lab::list_bundles(&hub.bundles);
            let out = Json::obj(vec![
                ("root", Json::from(hub.bundles.to_string_lossy().to_string())),
                ("bundles", Json::Arr(list)),
            ]);
            json_response(&mut stream, &out.to_compact())
        }
        // 实验室：读某个包的 report.md（只允许包名，不允许路径）
        ("GET", path_) if path_.starts_with("/api/lab/report") => {
            let name = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("name="))
                .unwrap_or("");
            if name.contains('/') || name.contains("..") || name.is_empty() {
                return json_response(&mut stream, "{\"error\":\"非法包名\"}");
            }
            let dir = hub.bundles.join(name);
            let read = |f: &str| {
                std::fs::read_to_string(dir.join(f)).unwrap_or_else(|_| String::new())
            };
            let out = Json::obj(vec![
                ("name", Json::from(name)),
                ("report", Json::from(read("report.md"))),
                ("meta", Json::from(read("meta.json"))),
                ("claims", Json::from(read("claims.json"))),
            ]);
            json_response(&mut stream, &out.to_compact())
        }
        // 实验室：跑一次（后台执行，结果用 SSE 通知，页面据事件刷新列表）
        ("POST", "/api/lab/run") => {
            if hub.bench.is_none() {
                return json_response(&mut stream, "{\"ok\":false,\"reason\":\"未配置 --bench-cmd\"}");
            }
            if hub
                .lab_busy
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return json_response(&mut stream, "{\"ok\":false,\"reason\":\"已经在跑了\"}");
            }
            let cmd = hub.bench.clone().unwrap_or_default();
            let hub2 = Arc::clone(&hub);
            hub.push(Json::obj(vec![
                ("type", Json::from("lab-run")),
                ("phase", Json::from("start")),
            ]));
            std::thread::spawn(move || {
                let started = std::time::Instant::now();
                let status = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&cmd)
                    .output();
                let (code, tail) = match status {
                    Ok(out) => {
                        let text = String::from_utf8_lossy(&out.stdout).to_string();
                        let last = text
                            .lines()
                            .filter(|l| !l.trim().is_empty())
                            .last()
                            .unwrap_or("")
                            .to_string();
                        (out.status.code().unwrap_or(-1), last)
                    }
                    Err(e) => (-1, e.to_string()),
                };
                hub2.push(Json::obj(vec![
                    ("type", Json::from("lab-run")),
                    ("phase", Json::from("end")),
                    ("code", Json::from(code)),
                    ("ms", Json::from(started.elapsed().as_millis() as u64)),
                    ("tail", Json::from(tail)),
                ]));
                hub2.lab_busy.store(false, Ordering::SeqCst);
            });
            json_response(&mut stream, "{\"ok\":true}")
        }
        // 会话：列表 / 切换 / 新建
        ("GET", "/api/sessions") => {
            let current = hub.session.lock().unwrap().clone();
            json_response(&mut stream, &session_list(&hub.workspace, &current).to_compact())
        }
        ("POST", "/api/session") => {
            let value = json::parse(body.trim()).unwrap_or(Json::Null);
            let name = value.str_at("name").unwrap_or("").to_string();
            if !valid_session(&name) {
                return json_response(&mut stream, "{\"ok\":false,\"reason\":\"非法会话名\"}");
            }
            if !session_file(&hub.workspace, &name).exists() && name != "default" {
                return json_response(&mut stream, "{\"ok\":false,\"reason\":\"会话不存在\"}");
            }
            *hub.pending_session.lock().unwrap() = Some(name.clone());
            json_response(&mut stream, "{\"ok\":true}")
        }
        ("POST", "/api/session/new") => {
            let value = json::parse(body.trim()).unwrap_or(Json::Null);
            let name = value.str_at("name").unwrap_or("").to_string();
            if !valid_session(&name) {
                return json_response(&mut stream, "{\"ok\":false,\"reason\":\"非法会话名\"}");
            }
            let path = session_file(&hub.workspace, &name);
            if path.exists() {
                return json_response(&mut stream, "{\"ok\":false,\"reason\":\"会话已存在\"}");
            }
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::OpenOptions::new().create(true).append(true).open(&path);
            *hub.pending_session.lock().unwrap() = Some(name);
            json_response(&mut stream, "{\"ok\":true}")
        }
        ("GET", "/api/state") => {
            let state = Json::obj(vec![
                ("busy", Json::Bool(hub.busy.load(Ordering::SeqCst))),
                ("seq", Json::from(hub.seq.load(Ordering::SeqCst))),
            ]);
            json_response(&mut stream, &state.to_compact())
        }
        ("POST", "/api/mode") => {
            let value = json::parse(body.trim()).unwrap_or(Json::Null);
            match value.str_at("mode").and_then(ui_core::Mode::parse) {
                Some(m) => {
                    *hub.mode.lock().unwrap() = m;
                    hub.push(Json::obj(vec![
                        ("type", Json::from("mode")),
                        ("mode", Json::from(m.name())),
                    ]));
                    json_response(&mut stream, "{\"ok\":true}")
                }
                None => json_response(&mut stream, "{\"ok\":false,\"reason\":\"bad mode\"}"),
            }
        }
        ("POST", "/api/cancel") => {
            if let Some(cancel) = hub.cancel.lock().unwrap().as_ref() {
                cancel.cancel();
                hub.push(Json::obj(vec![
                    ("type", Json::from("cancel-requested")),
                ]));
                json_response(&mut stream, "{\"ok\":true}")
            } else {
                json_response(&mut stream, "{\"ok\":false,\"reason\":\"no running turn\"}")
            }
        }
        ("GET", path_) if path_.starts_with("/api/tool/") => {
            let id = path_.trim_start_matches("/api/tool/").parse::<u64>();
            match id.ok().and_then(|i| hub.tool(i)) {
                Some(tool) => json_response(&mut stream, &tool.to_compact()),
                None => {
                    let _ = stream
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                    Ok(())
                }
            }
        }
        ("POST", "/api/say") => {
            let value = json::parse(body.trim()).unwrap_or(Json::Null);
            let text = value.str_at("text").unwrap_or("").trim().to_string();
            if text.is_empty() {
                return json_response(&mut stream, "{\"ok\":false}");
            }
            hub.push(Json::obj(vec![
                ("type", Json::from("user")),
                ("text", Json::from(text.as_str())),
            ]));
            let _ = task_tx.send(text);
            json_response(&mut stream, "{\"ok\":true}")
        }
        ("POST", "/api/approve") => {
            let value = json::parse(body.trim()).unwrap_or(Json::Null);
            let verdict = value.str_at("verdict").unwrap_or("deny");
            let decision = match verdict {
                "allow" | "always" => Decision::Allow,
                _ => Decision::Deny,
            };
            hub.answer(decision);
            hub.push(Json::obj(vec![
                ("type", Json::from("approved")),
                ("verdict", Json::from(verdict)),
            ]));
            json_response(&mut stream, "{\"ok\":true}")
        }
        ("GET", "/api/events") => sse(&mut stream, hub, query),
        _ => {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
            Ok(())
        }
    }
}

/// 没有 token：不给页面、不给数据，也不泄露任何工作区信息。
fn unauthorized(stream: &mut TcpStream, api: bool) -> std::io::Result<()> {
    if api {
        let body = "{\"error\":\"unauthorized\"}";
        let head = format!(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes())?;
        return stream.write_all(body.as_bytes());
    }
    let body = "<!doctype html><meta charset=utf-8><title>需要 token</title>\
         <body style=\"font:14px/1.7 system-ui;padding:12vh 10vw;color:#333\">\
         <h1 style=\"font-size:18px\">需要访问 token</h1>\
         <p>启动 <code>ui serve</code> 时终端会打印带 token 的地址，用它打开一次即可（之后走 Cookie）。</p>\
         </body>";
    let head = format!(
        "HTTP/1.1 401 Unauthorized\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())
}

/// 令牌：时间 + pid 派生的 12 位十六进制，够用且不引入依赖。
fn make_token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let digest = ui_core::sha256::hex(format!("{}-{}", nanos, pid).as_bytes());
    digest[..12].to_string()
}

/// 会话名 → 日志文件。default 保持老路径（历史日志继续可读）。
fn valid_session(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn session_file(ws: &std::path::Path, name: &str) -> std::path::PathBuf {
    if name == "default" {
        ws.join(".ui").join("journal.jsonl")
    } else {
        ws.join(".ui").join("sessions").join(format!("{}.jsonl", name))
    }
}

/// 列出会话：名字、事件数、最后活动时间
fn session_list(ws: &std::path::Path, current: &str) -> Json {
    let mut rows: Vec<Json> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(ws.join(".ui").join("sessions")) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                if let Some(stem) = p.file_stem() {
                    names.push(stem.to_string_lossy().to_string());
                }
            }
        }
    }
    if ws.join(".ui").join("journal.jsonl").exists() {
        names.push("default".to_string());
    }
    for name in names {
        let p = session_file(ws, &name);
        let events = std::fs::read_to_string(&p)
            .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0);
        let mtime = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        rows.push(Json::obj(vec![
            ("name", Json::from(name.clone())),
            ("events", Json::from(events)),
            ("mtime", Json::from(mtime)),
            ("current", Json::Bool(name == current)),
        ]));
    }
    rows.sort_by(|a, b| b.u64_at("mtime").cmp(&a.u64_at("mtime")));
    Json::obj(vec![
        ("current", Json::from(current)),
        ("sessions", Json::Arr(rows)),
    ])
}

fn html(stream: &mut TcpStream, body: &str) -> std::io::Result<()> {    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

fn json_response(stream: &mut TcpStream, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// SSE：先补历史，再持续推送。页面刷新后能把聊天恢复出来。
fn sse(stream: &mut TcpStream, hub: Arc<Hub>, query: &str) -> std::io::Result<()> {
    // 断线重连：`?since=<seq>` 只补之后的事件，避免整页历史再来一遍
    let since: u64 = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("since="))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
    )?;
    let (tx, rx) = channel::<String>();
    for (n, line) in hub.log.lock().unwrap().iter() {
        if *n > since {
            let _ = write!(stream, "data: {}\n\n", line);
        }
    }
    let _ = stream.flush();
    hub.subs.lock().unwrap().push(tx);

    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(15)) {
            Ok(line) => {
                if write!(stream, "data: {}\n\n", line).is_err() {
                    break;
                }
                if stream.flush().is_err() {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // 心跳：让浏览器知道连接还活着
                if write!(stream, ": ping\n\n").is_err() || stream.flush().is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    Ok(())
}
