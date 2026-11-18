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
const BG_DARK: &[u8] = include_bytes!("../assets/bg-dark.jpg");
const BG_LIGHT: &[u8] = include_bytes!("../assets/bg-light.jpg");

/// 请求体上限：本地页面只会发一小段 JSON。
/// 不设限时 `Content-Length: 9999999999` 会直接按声明值分配内存，本机任意程序
/// 都能把 Web 进程打爆；超过这个值一律 413，并且不读正文。
const MAX_BODY: usize = 4 * 1024 * 1024;

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

    /// 通用钩子：把内核写的**具名事件**（memory/used、effect/observed…）推给页面。
    ///
    /// 有了它，以后新增事件子类型不需要再往 Hooks trait 上加回调。
    /// 只推 `kind` 与 `payload`，不带 seq/hash——页面要的是"发生了什么"，
    /// 链的完整性由 `ui verify` 负责。
    fn on_event(&mut self, event: &Json) {
        let kind = match event.str_at("kind") {
            Some(k) => k.to_string(),
            None => return,
        };
        self.hub.push(Json::obj(vec![
            ("type", Json::from("journal")),
            ("kind", Json::from(kind)),
            (
                "payload",
                event.get("payload").cloned().unwrap_or(Json::Null),
            ),
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
        None => make_token()?,
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

/// `/api/file` 与 `/api/tree` 共用的路径校验：URL 解码 → 拒绝绝对路径与危险目录段 →
/// canonicalize → 结果必须仍在工作区内。返回（解码后的相对路径，绝对路径）。
///
/// 只过滤 `..` 不够：`/etc` 是绝对路径（`Path::join` 遇到绝对路径会整个替换根），
/// 符号链接还能把工作区里的名字指到外面；这两类都靠 canonicalize + `starts_with` 兜住。
fn resolve_in_workspace(
    workspace: &std::path::Path,
    raw: &str,
) -> Result<(String, std::path::PathBuf), String> {
    let rel = pct_decode(raw);
    let blocked = rel
        .split(|c| c == '/' || c == '\\')
        .any(is_blocked_component);
    if rel.starts_with('/') || rel.starts_with('\\') || blocked {
        return Err("路径不允许".to_string());
    }
    let root = std::fs::canonicalize(workspace).map_err(|_| "工作区不可读".to_string())?;
    let path = std::fs::canonicalize(root.join(&rel)).map_err(|_| "路径不存在".to_string())?;
    if !path.starts_with(&root) {
        return Err("路径越界".to_string());
    }
    Ok((rel, path))
}

/// 不展示、也不允许直达的目录名。大小写不敏感：Windows 文件系统本身不区分大小写，
/// 只挡小写 `secrets` 等于没挡（`Secrets/` 照样能读）。
fn is_blocked_component(name: &str) -> bool {
    name == ".."
        || name.eq_ignore_ascii_case(".ui")
        || name.eq_ignore_ascii_case(".git")
        || name.eq_ignore_ascii_case("secrets")
        || name.eq_ignore_ascii_case("target")
        || name.eq_ignore_ascii_case("node_modules")
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

    if content_length > MAX_BODY {
        // 超限就直接拒绝：既不分配 content_length 大小的 Vec，也不把正文读进来。
        // 连接随后关闭（Connection: close + drop），客户端不会把余下正文灌进来。
        let body = "payload too large\n";
        let head = format!(
            "HTTP/1.1 413 Payload Too Large\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(body.as_bytes())?;
        return stream.flush();
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
        // 背景板：GPT 生成后压成 JPEG，80KB 左右；随二进制走，不依赖外部 CDN。
        ("GET", "/bg/dark.jpg") => image(&mut stream, BG_DARK, "image/jpeg"),
        ("GET", "/bg/light.jpg") => image(&mut stream, BG_LIGHT, "image/jpeg"),
        // 静态快照：事件内嵌，不开 SSE。可分享、可截图、可离线打开。
        ("GET", "/snapshot") => {
            let page = snapshot_page(&hub);
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
        // 工作区文件树（只读）：与 /api/file 共用同一套路径校验。
        // 以前只挡 `..`，于是 `?path=/etc` 能列宿主机目录、`?path=secrets` 能列密钥目录。
        ("GET", "/api/tree") => {
            let ready = hub.ready.lock().unwrap().clone().unwrap_or(Json::Null);
            let root = std::path::PathBuf::from(ready.str_at("workspace").unwrap_or(""));
            let raw = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("path="))
                .unwrap_or("");
            let (rel, dir) = match resolve_in_workspace(&root, raw) {
                Ok(v) => v,
                Err(e) => return tree_error(&mut stream, &e),
            };
            let mut entries: Vec<Json> = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten().take(300) {
                    let name = e.file_name().to_string_lossy().to_string();
                    // 不展示 .git/target/node_modules/.ui/secrets（名字本身也是信息）
                    if is_blocked_component(&name) {
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
        // 工作区文件预览：只读、限 512 KB；路径校验与 /api/tree 完全共用。
        ("GET", "/api/file") => {
            let ready = hub.ready.lock().unwrap().clone().unwrap_or(Json::Null);
            let root = std::path::PathBuf::from(ready.str_at("workspace").unwrap_or(""));
            let raw = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("path="))
                .unwrap_or("");
            let (rel, path) = match resolve_in_workspace(&root, raw) {
                Ok((rel, path)) if !rel.is_empty() && path.is_file() => (rel, path),
                Ok(_) => return file_error(&mut stream, "路径越界或不是文件"),
                Err(e) => return file_error(&mut stream, &e),
            };
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let truncated = size > 512 * 1024;
            let mut bytes = Vec::new();
            if let Ok(f) = std::fs::File::open(&path) {
                let _ = f.take(512 * 1024).read_to_end(&mut bytes);
            }
            let binary = bytes.contains(&0);
            let text = if binary { String::new() } else { String::from_utf8_lossy(&bytes).to_string() };
            let out = Json::obj(vec![
                ("ok", Json::Bool(true)),
                ("path", Json::from(rel.as_str())),
                ("bytes", Json::from(size)),
                ("truncated", Json::Bool(truncated)),
                ("binary", Json::Bool(binary)),
                ("text", Json::from(text.as_str())),
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

/// 令牌：16 字节系统密码学随机源的十六进制（128 位）。
///
/// 旧实现用「时间纳秒 + pid」做 sha256：这些量本机其它程序都能猜到或枚举，
/// 猜到 token 就等于鉴权形同虚设。随机源失败时**硬失败**（返回 Err，让 serve
/// 直接退出），绝不退回时间戳派生——那会把「安全失败」变成「静默降级」。
fn make_token() -> Result<String, String> {
    make_token_with(fill_random)
}

fn make_token_with(mut fill: impl FnMut(&mut [u8]) -> Result<(), String>) -> Result<String, String> {
    let mut bytes = [0u8; 16];
    fill(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{:02x}", b)).collect())
}

#[cfg(unix)]
fn fill_random(buf: &mut [u8]) -> Result<(), String> {
    fill_random_from(std::path::Path::new("/dev/urandom"), buf)
}

#[cfg(unix)]
fn fill_random_from(path: &std::path::Path, buf: &mut [u8]) -> Result<(), String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("打开随机源失败: {}", e))?;
    f.read_exact(buf).map_err(|e| format!("读取随机源失败: {}", e))
}

#[cfg(windows)]
fn fill_random(buf: &mut [u8]) -> Result<(), String> {
    // BCryptGenRandom(NULL, buf, len, BCRYPT_USE_SYSTEM_PREFERRED_RNG)：
    // 系统首选 CSPRNG，零依赖（bcrypt.dll 是 Windows 自带组件）。返回 0 才算成功。
    #[link(name = "bcrypt")]
    extern "system" {
        fn BCryptGenRandom(
            algorithm: *mut core::ffi::c_void,
            buffer: *mut u8,
            len: u32,
            flags: u32,
        ) -> i32;
    }
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
    let len = u32::try_from(buf.len()).map_err(|_| "随机数长度超限".to_string())?;
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            len,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(format!("BCryptGenRandom 失败: 0x{:08x}", status))
    }
}

#[cfg(not(any(unix, windows)))]
fn fill_random(_buf: &mut [u8]) -> Result<(), String> {
    Err("当前平台没有可用的系统随机源".to_string())
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

/// 把要嵌进 `<script>` 的 JSON 变安全：`<` 一律写成 `\u003c`。
///
/// 日志内容里出现 `</script>` 会提前闭合脚本：快照打不开，而且是一枚存储型 XSS。
/// JSON 里 `<` 只可能出现在字符串内部，`\u003c` 仍是合法的 JSON 转义，
/// 页面读到的值不变，但 HTML 解析器再也看不到标签边界。
fn script_safe_json(json: &str) -> String {
    json.replace('<', "\\u003c")
}

/// `/snapshot` 的完整页面：把事件日志与启动信息注入静态 HTML。
fn snapshot_page(hub: &Hub) -> String {
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
        "<script>window.__UI_BOOT__=[{}];window.__UI_BUSY__={};window.__UI_READY__={};</script>\n</head>",
        script_safe_json(&boot),
        hub.busy.load(Ordering::SeqCst),
        script_safe_json(&ready.to_compact())
    );
    CHAT_HTML.replace("</head>", &inject)
}

fn html(stream: &mut TcpStream, body: &str) -> std::io::Result<()> {    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(b[i]);
                i += 1;
            }
            b'+' => { out.push(b' '); i += 1; }
            c => { out.push(c); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn image(stream: &mut TcpStream, body: &[u8], mime: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: public, max-age=86400\r\nConnection: close\r\n\r\n",
        mime,
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// 目录列表的错误形状保持 `{"entries":[],"error":...}`：页面只读 `entries`，
/// 旧版本也返回同样的键，避免前端因为字段缺失而报错。
fn tree_error(stream: &mut TcpStream, message: &str) -> std::io::Result<()> {
    let out = Json::obj(vec![
        ("path", Json::from("")),
        ("entries", Json::Arr(Vec::new())),
        ("error", Json::from(message)),
    ]);
    json_response(stream, &out.to_compact())
}

fn file_error(stream: &mut TcpStream, message: &str) -> std::io::Result<()> {
    let out = Json::obj(vec![
        ("ok", Json::Bool(false)),
        ("error", Json::from(message)),
    ]);
    json_response(stream, &out.to_compact())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_event_reaches_the_sse_log() {
        // 回归：`Hooks::on_event` 曾经是个"声明了但从不调用"的死钩子，
        // 于是网页端永远看不到 memory/used、effect/observed 这类具名事件。
        let hub = Arc::new(Hub::default());
        let mut hooks = WebHooks {
            hub: hub.clone(),
            auto_yes: true,
        };
        hooks.on_event(&Json::obj(vec![
            ("kind", Json::from("memory/used")),
            (
                "payload",
                Json::obj(vec![
                    ("id", Json::from("m-abc123")),
                    ("reason", Json::from("keyword-match:门禁")),
                    ("tokens", Json::from(12)),
                ]),
            ),
        ]));

        let log = hub.log.lock().unwrap();
        assert_eq!(log.len(), 1, "应当恰好推一条 SSE 事件");
        let line = &log[0].1;
        assert!(line.contains("\"type\":\"journal\""), "类型应当是 journal: {}", line);
        assert!(line.contains("memory/used"), "应当带 kind: {}", line);
        assert!(line.contains("m-abc123"), "应当带 payload: {}", line);
        assert!(!line.contains("hash"), "不该把链字段推给页面: {}", line);
    }

    // ── 测试用的最小 HTTP 收发 ──（只处理一条连接，够验证路由与响应）
    fn temp_workspace(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::AtomicU64;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "ui-server-{}-{}-{}",
            tag,
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn hub_for(ws: &std::path::Path) -> Arc<Hub> {
        let hub = Arc::new(Hub::default());
        *hub.ready.lock().unwrap() = Some(Json::obj(vec![
            ("workspace", Json::from(ws.to_string_lossy().to_string())),
            ("model", Json::from("test")),
        ]));
        hub
    }

    /// 起一个只处理一条连接的“服务器”，把原始请求发过去，返回（响应头, 响应体）。
    fn http_request(hub: Arc<Hub>, raw: &str) -> (String, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (task_tx, task_rx) = channel::<String>();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = handle(stream, hub, task_tx);
        });
        let mut client = TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        client.write_all(raw.as_bytes()).unwrap();
        let mut out = Vec::new();
        let _ = client.read_to_end(&mut out);
        // 旧代码在超大 Content-Length 上会卡在 read_exact；先关客户端，让服务线程拿到 EOF 退出。
        drop(client);
        drop(task_rx);
        server.join().unwrap();
        let text = String::from_utf8_lossy(&out).to_string();
        match text.split_once("\r\n\r\n") {
            Some((head, body)) => (head.to_string(), body.to_string()),
            None => (text, String::new()),
        }
    }

    fn http_get(hub: Arc<Hub>, target: &str) -> (String, String) {
        http_request(
            hub,
            &format!(
                "GET {} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n",
                target
            ),
        )
    }

    #[test]
    fn tree_rejects_absolute_and_secret_paths() {
        // 回归：`/api/tree` 曾只过滤 `..`，于是 ?path=/etc 能列宿主机目录、
        // ?path=secrets 能列密钥目录。修法是与 /api/file 共用 resolve_in_workspace。
        let ws = temp_workspace("tree");
        std::fs::create_dir_all(ws.join("secrets")).unwrap();
        std::fs::write(ws.join("secrets").join("token.txt"), "s3cr3t").unwrap();
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::write(ws.join("src").join("a.js"), "export const a = 1;\n").unwrap();

        // 正常子目录仍然能列（别把功能修坏）
        let (head, body) = http_get(hub_for(&ws), "/api/tree?path=src");
        assert!(head.contains("200 OK"), "{}", head);
        assert!(body.contains("\"a.js\""), "正常目录应能列出: {}", body);

        for target in [
            "/api/tree?path=/etc",
            "/api/tree?path=%2Fetc",
            "/api/tree?path=secrets",
            "/api/tree?path=..%2F..%2Fetc",
        ] {
            let (_head, body) = http_get(hub_for(&ws), target);
            assert!(body.contains("\"error\""), "{} 应当被拒绝: {}", target, body);
            assert!(
                !body.contains("token.txt"),
                "{} 泄露了 secrets: {}",
                target,
                body
            );
            assert!(
                !body.contains("\"passwd\""),
                "{} 泄露了宿主机目录: {}",
                target,
                body
            );
        }

        // 根目录列表也不该出现 secrets 这个名字（名字本身也是信息）
        let (_head, root_body) = http_get(hub_for(&ws), "/api/tree");
        assert!(
            !root_body.contains("secrets"),
            "根列表不应出现 secrets: {}",
            root_body
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[cfg(unix)]
    #[test]
    fn tree_rejects_symlink_escape() {
        // canonicalize 必须解掉符号链接：工作区里一个指向 /etc 的链接不能变成后门。
        let ws = temp_workspace("symlink");
        std::os::unix::fs::symlink("/etc", ws.join("escape")).unwrap();
        let (_head, body) = http_get(hub_for(&ws), "/api/tree?path=escape");
        assert!(body.contains("\"error\""), "符号链接越界应被拒绝: {}", body);
        assert!(!body.contains("\"passwd\""), "泄露了 /etc: {}", body);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn file_preview_uses_the_same_path_guard() {
        // /api/file 换用共享校验后不能把正常预览修坏，也不能放过 secrets。
        let ws = temp_workspace("file");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::write(ws.join("src").join("a.js"), "export const a = 1;\n").unwrap();
        std::fs::create_dir_all(ws.join("secrets")).unwrap();
        std::fs::write(ws.join("secrets").join("token.txt"), "s3cr3t").unwrap();

        let (_head, ok) = http_get(hub_for(&ws), "/api/file?path=src%2Fa.js");
        assert!(ok.contains("\"ok\":true"), "正常文件应能预览: {}", ok);
        assert!(ok.contains("export const a = 1"), "{}", ok);

        for target in [
            "/api/file?path=/etc/passwd",
            "/api/file?path=%2Fetc%2Fpasswd",
            "/api/file?path=secrets%2Ftoken.txt",
            "/api/file?path=..%2Fetc%2Fpasswd",
        ] {
            let (_head, body) = http_get(hub_for(&ws), target);
            assert!(body.contains("\"ok\":false"), "{} 应被拒绝: {}", target, body);
            assert!(!body.contains("s3cr3t"), "{} 泄露了 secrets: {}", target, body);
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn snapshot_escapes_script_close_in_events() {
        // 回归：日志 JSON 直接插进 <script>，内容里的 </script> 会截断脚本
        //（快照打不开 + 存储型 XSS）。输出时必须转义成 \u003c/。
        let ws = temp_workspace("snapshot");
        let hub = hub_for(&ws);
        let evil = "</script><script>alert(1)</script>";
        // ready 里的 workspace/model 同样来自外部输入（启动参数、模型名）
        *hub.ready.lock().unwrap() = Some(Json::obj(vec![
            ("workspace", Json::from(evil)),
            ("model", Json::from(evil)),
        ]));
        hub.push(Json::obj(vec![
            ("type", Json::from("user")),
            ("text", Json::from(evil)),
        ]));

        let (head, page) = http_get(hub, "/snapshot");
        assert!(head.contains("200 OK"), "{}", head);
        assert!(page.contains("window.__UI_BOOT__"), "快照应包含引导数据");
        assert!(
            page.contains("\\u003c/script>"),
            "事件里的 </script> 必须被转义"
        );
        assert!(
            !page.contains("<script>alert(1)"),
            "不应留下可执行的注入标签"
        );

        // 标签数量：只允许注入点自己那一个 <script>，日志内容不能新增标签。
        assert_eq!(
            page.matches("<script").count(),
            CHAT_HTML.matches("<script").count() + 1,
            "script 开标签只应多出注入点的那一个"
        );
        assert_eq!(
            page.matches("</script>").count(),
            CHAT_HTML.matches("</script>").count() + 1,
            "script 闭标签只应多出注入点的那一个"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn oversized_body_is_rejected_with_413() {
        // 回归：按 Content-Length 直接分配 Vec，超大请求能打爆内存。
        // 现在超限立即 413 + Connection: close，且不读正文。
        let ws = temp_workspace("body");
        let raw = format!(
            "POST /api/say HTTP/1.1\r\nHost: test\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        let (head, _body) = http_request(hub_for(&ws), &raw);
        assert!(head.starts_with("HTTP/1.1 413"), "应当返回 413: {}", head);
        assert!(
            head.to_ascii_lowercase().contains("connection: close"),
            "413 后必须关闭连接: {}",
            head
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn make_token_is_hex_from_os_randomness() {
        let a = make_token().expect("系统随机源必须可用");
        let b = make_token().expect("系统随机源必须可用");
        assert_eq!(a.len(), 32, "16 字节应当编码成 32 位十六进制: {}", a);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()), "{}", a);
        assert_ne!(a, b, "两次令牌不能相同");
    }

    #[test]
    fn token_entropy_failure_is_hard() {
        // 随机源失败不能退回“时间 + pid”派生，必须把错误往上抛（serve 因此退出）。
        let mut asked = 0;
        let out = make_token_with(|_| {
            asked += 1;
            Err("no entropy".to_string())
        });
        assert_eq!(asked, 1, "必须真的去取随机数");
        assert!(out.is_err(), "随机源失败时必须硬失败");
    }

    #[cfg(unix)]
    #[test]
    fn urandom_is_the_entropy_source() {
        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        fill_random(&mut a).expect("/dev/urandom 应可读");
        fill_random(&mut b).expect("/dev/urandom 应可读");
        assert_ne!(a, b, "两次随机数不应相同");
        assert!(
            fill_random_from(
                std::path::Path::new("/definitely/not/a/random/source"),
                &mut a
            )
            .is_err(),
            "打不开随机源必须返回错误，而不是静默降级"
        );
    }
}
