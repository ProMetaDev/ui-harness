//! 零依赖的本地 Web 用户端：一个页面、一条 SSE 事件流、一个审批回执口。
//!
//! 为什么要自己写 HTTP：内核零依赖是硬约束（构建环境不可靠）。这里的解析器只认
//! 「请求行 + 头 + Content-Length 正文」，够用一个聊天页面，也就一屏代码。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

use ui_core::agent::{ApprovalRequest, ApprovalResponse, ApprovalScope, Hooks};
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

/// 一个排队或正在跑的任务。
///
/// 令牌跟着任务走，不跟着 worker 走：在队列里就能被取消，取消也只影响
/// 这一个任务——上一任务留下的「已取消」绝不能顺到下一任务头上。
#[derive(Clone)]
struct WebTask {
    id: String,
    text: String,
    session: String,
    cancel: ui_core::Cancel,
}

/// 等待浏览器回答的一次审批。
///
/// 三个字段都是必需的：`approval_id` 让回答能指名道姓，`call_id` 让页面
/// 能把回执对到正确的调用上（一个审批对应一次调用），`tx` 是唤醒内核的通道。
struct PendingApproval {
    approval_id: String,
    call_id: String,
    tx: Sender<ApprovalResponse>,
}

#[derive(Default)]
struct Hub {
    /// 已广播的事件（序号 + JSON 行），新连接用 `?since=` 补齐，重连不重复
    log: Mutex<Vec<(u64, String)>>,
    /// 事件序号
    seq: std::sync::atomic::AtomicU64,
    /// 当前 log 里最早还保留的 seq；客户端 since 更旧时明确告知丢了窗口
    min_seq: std::sync::atomic::AtomicU64,
    /// 进程启动标识：客户端重连时据此发现服务端已经重启、seq 从 0 重来
    epoch: u64,
    /// 每个 SSE 连接一个发送端
    subs: Mutex<Vec<Sender<String>>>,
    /// 等待浏览器回答的审批
    approval: Mutex<Option<PendingApproval>>,
    /// 正在跑的任务（None = 空闲）
    active: Mutex<Option<WebTask>>,
    /// 排队中、worker 还没取走的任务
    queue: Mutex<Vec<WebTask>>,
    /// 排队期间被撤掉的任务（id + 原文）：日志只能由 worker 单点写，
    /// HTTP 线程把账放在这儿，worker 下一次醒来时补上 `turn/skipped`。
    skipped: Mutex<Vec<(String, String)>>,
    /// 任务序号：给页面一个能贴住「停止哪个」的稳定 ID
    task_seq: std::sync::atomic::AtomicU64,
    /// 工具的完整输出（页面上点开再看）：按调用序号存
    tools: Mutex<Vec<Json>>,
    /// tools 被裁剪后，第一条对应的全局 id
    tool_base: Mutex<u64>,
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

fn hub_lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // 一个 panic 不该把整个 Web 服务永久变成 500：锁里的状态可能只是
    // 半更新的，但继续提供只读/后续任务比全部 unwrap 连锁失败好。
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Hub {
    /// 网页端**唯一**的脱敏点。
    ///
    /// 为什么在这里而不是前端：`/snapshot` 会把整个 `hub.log` 原样嵌进一份
    /// **可分享**的 HTML。前端打码只保护屏幕，保护不了"另存为 / 转发"。
    /// 判据与 `Journal` 落账门共用 `secrets`（单一来源），所以两边不会漂。
    ///
    /// 返回值写进事件的 `redacted` 字段：页面能说"这里抹过几处"，
    /// 而不是让人以为原本就是这样。
    fn redact(&self, event: &mut Json) -> usize {
        ui_core::secrets::redact_json(event)
    }

    fn clear_log(&self) {
        hub_lock(&self.log).clear();
        self.min_seq.store(0, Ordering::SeqCst);
    }

    fn min_seq(&self) -> u64 {
        self.min_seq.load(Ordering::SeqCst)
    }

    fn push(&self, mut event: Json) {
        // log → subs 是唯一锁顺序；取号也必须在 log 临界区内。
        // 否则并发事件可能乱序，订阅交接也可能同时收到 backlog 和 live 副本。
        let hits = self.redact(&mut event);
        if hits > 0 {
            event.set("redacted", Json::from(hits as u64));
        }
        // 每个事件带上产生它的会话：/snapshot 和前端据此丢弃切换前/切换后
        // 串进来的旧会话事件。
        let session = hub_lock(&self.session).clone();
        if !session.is_empty() {
            event.set("session", Json::from(session.as_str()));
        }
        let mut log = hub_lock(&self.log);
        let n = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        event.set("seq", Json::from(n));
        event.set("ts", Json::from(ui_core::time::now_ms()));
        let line = event.to_compact();
        log.push((n, line.clone()));
        // 日志不是内存泄漏：只保留最近一段，SSE 的 since 仍能补最近窗口。
        const MAX_HUB_LOG: usize = 5000;
        if log.len() > MAX_HUB_LOG {
            let excess = log.len() - MAX_HUB_LOG;
            log.drain(0..excess);
        }
        if let Some((first, _)) = log.first() {
            self.min_seq.store(*first, Ordering::SeqCst);
        }
        let mut subs = hub_lock(&self.subs);
        subs.retain(|tx| tx.send(line.clone()).is_ok());
    }

    /// 原子地复制待补事件并注册实时订阅。调用方释放锁后再写 backlog。
    fn subscribe(&self, since: u64) -> (Vec<String>, std::sync::mpsc::Receiver<String>) {
        let log = hub_lock(&self.log);
        let backlog = log
            .iter()
            .filter(|(n, _)| *n > since)
            .map(|(_, line)| line.clone())
            .collect();
        let (tx, rx) = channel::<String>();
        hub_lock(&self.subs).push(tx);
        (backlog, rx)
    }

    /// 浏览器回答一次审批，必须指名道姓。
    ///
    /// 旧实现是 `take().send(decision)`：谁回答都算、答几次都行，于是
    /// 「双击批准」= 批准两次，「页面开着上一轮的批准按钮」= 把答案投给
    /// 下一次审批。这里对不上身份就把原审批**放回去**，让它继续等人。
    fn answer(
        &self,
        approval_id: &str,
        call_id: &str,
        response: ApprovalResponse,
    ) -> Answer {
        let mut slot = self.approval.lock().unwrap();
        let pending = match slot.take() {
            Some(p) => p,
            // 没有人等在问：审批已超时/被取消/这一轮根本没有审批
            None => return Answer::Gone,
        };
        if pending.approval_id != approval_id || pending.call_id != call_id {
            let stale = Answer::Stale {
                expected_approval_id: pending.approval_id.clone(),
                expected_call_id: pending.call_id.clone(),
            };
            *slot = Some(pending);
            return stale;
        }
        let approval_id = pending.approval_id.clone();
        let call_id = pending.call_id.clone();
        match pending.tx.send(response) {
            Ok(()) => Answer::Accepted {
                approval_id,
                call_id,
            },
            // 内核那一头已经不在了（这一轮被取消后 drop 掉了接收端）
            Err(_) => Answer::Gone,
        }
    }

    /// 排队用的新任务：一个 ID、一个只属于它的取消令牌。
    fn new_task(&self, text: String) -> WebTask {
        let n = self.task_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let pending = self.pending_session.lock().unwrap().clone();
        let session = pending.unwrap_or_else(|| self.session.lock().unwrap().clone());
        WebTask {
            id: format!("t{}", n),
            text,
            session,
            cancel: ui_core::Cancel::new(),
        }
    }

    /// worker 取下一个待执行任务（队列头）。排队中被取消的已经不在队列里。
    ///
    /// 取走的同时就登记为 active：否则「worker 已拿走、还没写进 active」这一小段里
    /// 点停止会谁都对不上，`/api/cancel` 如实回答 idle，而任务照跑——
    /// 按钮按下去没反应，比报错更难查。
    fn take_queued(&self) -> Option<WebTask> {
        let task = {
            let mut q = hub_lock(&self.queue);
            if q.is_empty() {
                None
            } else {
                Some(q.remove(0))
            }
        };
        if let Some(t) = &task {
            *hub_lock(&self.active) = Some(t.clone());
        }
        task
    }

    /// 给 worker 的一记空信号：干活看的是队列，这颗子弹只是把它从 `recv` 上叫醒。
    fn wake_signal() -> WebTask {
        WebTask {
            id: String::new(),
            text: String::new(),
            session: String::new(),
            cancel: ui_core::Cancel::new(),
        }
    }

    /// 取走并清空「排队时被撤掉」的账（由 worker 写进日志）。
    fn take_skipped(&self) -> Vec<(String, String)> {
        std::mem::take(&mut *self.skipped.lock().unwrap())
    }

    /// 当前在跑的任务 + 排队中的任务 ID（给 /api/state 与页面用）。
    fn task_view(&self) -> (Option<String>, Vec<String>) {
        let active = hub_lock(&self.active).as_ref().map(|t| t.id.clone());
        let queued = hub_lock(&self.queue)
            .iter()
            .map(|t| t.id.clone())
            .collect();
        (active, queued)
    }

    /// 「停止」按钮的后端语义：先停正在跑的，再退回排队里的，都没有就是空闲。
    ///
    /// 带 `target` 时只动那一个任务。取消只置标志、不清标志——
    /// 令牌是每任务一个，所以「取消过」不会顺着队列流到下一个任务。
    fn cancel_task(&self, target: Option<&str>) -> Cancelled {
        let active = self.active.lock().unwrap().clone();
        if let Some(task) = active {
            if matches_target(&task.id, target) {
                task.cancel.cancel();
                // 审批正等着人点：取消要立刻打断那个等待，不然停止按钮
                // 要等到 300 秒超时才生效。
                self.clear_approval();
                return Cancelled::Active(task.id);
            }
        }
        let picked: Vec<WebTask> = {
            let mut q = self.queue.lock().unwrap();
            let hit: Vec<WebTask> = q
                .iter()
                .filter(|t| matches_target(&t.id, target))
                .cloned()
                .collect();
            q.retain(|t| !hit.iter().any(|h| h.id == t.id));
            hit
        };
        if picked.is_empty() {
            return Cancelled::Idle;
        }
        let ids: Vec<String> = picked.iter().map(|t| t.id.clone()).collect();
        for task in &picked {
            task.cancel.cancel();
            // 撤掉的这一条得留下"为什么没跑"。写日志的人只有 worker 一个，
            // 所以这里只记账，由 worker 醒来时补成 turn/skipped。
            self.skipped.lock().unwrap().push((
                task.id.clone(),
                task.text.chars().take(200).collect::<String>(),
            ));
        }
        Cancelled::Queued(ids)
    }

    /// 丢掉正在等待的审批：任务被取消了，答案没人要了。
    ///
    /// 先放锁再 push——`push` 拿的是 log/subs 两把锁，别在 approval 的
    /// 临界区里广播，锁顺序留一条就够（log → subs）。
    fn clear_approval(&self) {
        let pending = hub_lock(&self.approval).take();
        if let Some(pending) = pending {
            self.push(Json::obj(vec![
                ("type", Json::from("approval-stale")),
                ("approvalId", Json::from(pending.approval_id.as_str())),
                ("callId", Json::from(pending.call_id.as_str())),
            ]));
        }
    }

    /// 正在跑的任务是否已被取消（审批等待循环用它打断自己）。
    fn active_cancelled(&self) -> bool {
        self.active
            .lock()
            .unwrap()
            .as_ref()
            .map(|t| t.cancel.is_cancelled())
            .unwrap_or(false)
    }

    /// 入队，返回它排在第几位（0 = 下一个就轮到自己，给页面一个「前面还有几条」）。
    fn enqueue(&self, task: &WebTask) -> usize {
        let mut q = self.queue.lock().unwrap();
        let position = q.len();
        q.push(task.clone());
        position
    }

    /// 当前等待回答的审批身份（页面刷新后靠它把回执点对那张卡）。
    fn approval_view(&self) -> Option<(String, String)> {
        self.approval
            .lock()
            .unwrap()
            .as_ref()
            .map(|p| (p.approval_id.clone(), p.call_id.clone()))
    }

    /// 存一份完整工具输出，返回给页面用的 id
    fn store_tool(&self, name: &str, result: &ToolResult) -> u64 {
        let mut base = self.tool_base.lock().unwrap();
        let mut tools = self.tools.lock().unwrap();
        // 同一条判据：点开「展开」看到的是完整输出，它比摘要更容易带出凭据。
        let mut text = Json::from(match &result.error {
            Some(e) if result.content.trim().is_empty() => e.clone(),
            Some(e) => format!("{}\n\n[错误] {}", result.content, e),
            None => result.content.clone(),
        });
        let hits = ui_core::secrets::redact_json(&mut text);
        let session = hub_lock(&self.session).clone();
        tools.push(Json::obj(vec![
            ("name", Json::from(name)),
            ("ok", Json::Bool(result.ok)),
            ("text", text),
            ("redacted", Json::from(hits as u64)),
            ("session", Json::from(session.as_str())),
        ]));
        // 完整输出也必须有上限：一个长会话不能让所有历史工具输出永久驻留。
        const MAX_TOOLS: usize = 500;
        if tools.len() > MAX_TOOLS {
            let excess = tools.len() - MAX_TOOLS;
            tools.drain(0..excess);
            *base += excess as u64;
        }
        *base + tools.len() as u64
    }

    fn tool(&self, id: u64) -> Option<Json> {
        let base = *self.tool_base.lock().unwrap();
        let idx = id.checked_sub(base + 1)?;
        let item = self.tools.lock().unwrap().get(idx as usize).cloned()?;
        let current = hub_lock(&self.session).clone();
        if item.str_at("session") != Some(current.as_str()) {
            return None;
        }
        Some(item)
    }
}

/// 认不出的批准词仍然按拒绝处理（安全默认不变），但不能**静默**降级：
/// 页面写着"已拒绝"，人以为自己点的是"允许"，是查不出来的那一类事故。
fn verdict_note(verdict: &str) -> Option<&'static str> {
    if verdict.is_empty() {
        return Some("请求里没有 verdict，按拒绝处理");
    }
    if ApprovalResponse::is_known_verdict(verdict) {
        return None;
    }
    Some("不认识的批准词，按拒绝处理")
}

/// `/api/approve` 的三种结局，必须让页面分辨得出来。
#[derive(Debug)]
enum Answer {
    Accepted {
        approval_id: String,
        call_id: String,
    },
    /// 身份不符：原审批还在等，页面该刷新成当前那张卡
    Stale {
        expected_approval_id: String,
        expected_call_id: String,
    },
    /// 没有在等的审批了（超时/取消/这一轮没有审批）
    Gone,
}

/// `/api/cancel` 打到了什么上。
#[derive(Debug)]
enum Cancelled {
    Active(String),
    Queued(Vec<String>),
    Idle,
}

/// `target` 为空表示「不管是谁都算」，否则必须点名对上 ID。
fn matches_target(id: &str, target: Option<&str>) -> bool {
    match target {
        None => true,
        Some(wanted) => wanted == id,
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

    fn on_approval(&mut self, req: &ApprovalRequest) -> ApprovalResponse {
        if self.auto_yes {
            self.hub.push(Json::obj(vec![
                ("type", Json::from("approved")),
                ("verdict", Json::from("auto")),
                ("scope", Json::from(ApprovalScope::Once.name())),
                ("approvalId", Json::from(req.approval_id.as_str())),
                ("callId", Json::from(req.call_id.as_str())),
                // 页面和日志说同一件事：这笔是 `--yes` 替人批的。
                ("via", Json::from("auto")),
            ]));
            // `--yes` 是命令行替人点的批准：日志里必须留下 via=auto，
            // 否则这份轨迹看上去像有人逐条看过现场。
            return ApprovalResponse::allow_once().via("auto");
        }
        let (tx, rx) = channel::<ApprovalResponse>();
        *self.hub.approval.lock().unwrap() = Some(PendingApproval {
            approval_id: req.approval_id.clone(),
            call_id: req.call_id.clone(),
            tx,
        });
        self.hub.push(Json::obj(vec![
            ("type", Json::from("approval")),
            ("approvalId", Json::from(req.approval_id.as_str())),
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
                Ok(response) => {
                    let mut done = Json::obj(vec![
                        ("type", Json::from("approved")),
                        ("verdict", Json::from(response.decision.name())),
                        ("scope", Json::from(response.scope.name())),
                        ("approvalId", Json::from(req.approval_id.as_str())),
                        ("callId", Json::from(req.call_id.as_str())),
                    ]);
                    // 页面显示的"已拒绝"有可能是我们没听懂那句话：和日志一样，
                    // 代答要当场说清是谁替人做的决定，不能只留在 JSONL 里。
                    if let Some(via) = response.via {
                        done.set("via", Json::from(via));
                    }
                    self.hub.push(done);
                    return response;
                }
                // 接收端被 hub 丢弃（取消或超时清理），没人要这个答案了
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    let verdict = if self.hub.active_cancelled() {
                        "cancelled"
                    } else {
                        "stale"
                    };
                    self.push_end(verdict, req);
                    // 账本要分得清"没人回答了"和"有人说了不"：前者是取消/失效，
                    // 后者是决定。两者都记成 deny 会让复盘把卡顿读成否决。
                    return ApprovalResponse::deny().via(verdict);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if self.hub.active_cancelled() {
                        self.hub.clear_approval();
                        self.push_end("cancelled", req);
                        return ApprovalResponse::deny().via("cancelled");
                    }
                    if started.elapsed().as_secs() > 300 {
                        self.hub.clear_approval();
                        self.push_end("timeout", req);
                        return ApprovalResponse::deny().via("timeout");
                    }
                }
            }
        }
    }
}

/// 收尾事件不属于 `Hooks` 契约，单独放一个 impl。
impl WebHooks {
    /// 审批没有正常收到回答时的收尾事件（cancelled / timeout / stale）。
    fn push_end(&self, verdict: &str, req: &ApprovalRequest) {
        self.hub.push(Json::obj(vec![
            ("type", Json::from("approved")),
            ("verdict", Json::from(verdict)),
            ("approvalId", Json::from(req.approval_id.as_str())),
            ("callId", Json::from(req.call_id.as_str())),
        ]));
    }
}

/// 把 journal 的事件翻译成页面消费的 UI 事件。
///
/// 只翻译“聊天里需要看见”的四类：user / assistant / tool / turn。
/// 审批、快照、预算等内部事件不重放，避免重连时伪造一张已经过期的审批卡。
fn replay_journal_into_hub(hub: &Hub, path: &std::path::Path) {
    let events = match ui_core::Journal::read(path) {
        Ok((events, _)) => events,
        Err(_) => return,
    };
    for event in events {
        let Some(kind) = event.str_at("kind") else {
            continue;
        };
        let ui_event = match kind {
            "turn/start" => event.str_at("prompt").map(|text| {
                Json::obj(vec![
                    ("type", Json::from("user")),
                    ("text", Json::from(text)),
                ])
            }),
            "model/response" => event.str_at("content").filter(|s| !s.is_empty()).map(|text| {
                Json::obj(vec![
                    ("type", Json::from("assistant")),
                    ("text", Json::from(text)),
                ])
            }),
            "tool/output" => {
                let detail = event
                    .str_at("text")
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .or_else(|| {
                        event.str_at("file").map(|file| {
                            format!(
                                "输出 {} 字，原文在 {}（需要时用 fs_read）",
                                event.u64_at("chars").unwrap_or(0),
                                file
                            )
                        })
                    })
                    .unwrap_or_default();
                let clipped: String = detail.chars().take(4000).collect();
                Some(Json::obj(vec![
                    ("type", Json::from("tool")),
                    ("name", Json::from(event.str_at("tool").unwrap_or("tool"))),
                    ("ok", Json::Bool(event.bool_at("ok").unwrap_or(true))),
                    ("detail", Json::from(clipped.as_str())),
                ]))
            }
            "turn/end" => Some(Json::obj(vec![
                ("type", Json::from("turn")),
                ("reason", Json::from(event.str_at("reason").unwrap_or("done"))),
                ("steps", Json::from(event.u64_at("steps").unwrap_or(0))),
                ("calls", Json::from(event.u64_at("toolCalls").unwrap_or(0))),
            ])),
            _ => None,
        };
        if let Some(event) = ui_event {
            hub.push(event);
        }
    }
}

pub fn serve(
    mut agent: Agent,
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
    hub0.epoch = ui_core::time::now_ms();
    let initial_session = session_arg.unwrap_or("default").to_string();
    *hub0.session.lock().unwrap() = initial_session.clone();
    let hub = Arc::new(hub0);
    // 页面事件源同样从 journal 恢复：否则 `ui serve` 一重启，日志还在，
    // 页面却是空的；切会话时目标历史也永远不会出现。
    let initial_path = session_file(&hub.workspace, &initial_session);
    if initial_path.exists() {
        // 页面看到的历史必须和模型下一次看到的上下文一致；只 replay UI
        // 不恢复 Agent 历史，会让用户以为它在续聊，模型其实失忆。
        agent
            .switch_journal(&initial_path)
            .map_err(|e| format!("恢复初始会话失败: {}", e))?;
    }
    replay_journal_into_hub(&hub, &initial_path);
    // 通道只是「队列里有活」的信号：真正执行哪个任务以 hub.queue 为准，
    // 这样排队期间被取消（已从队列摘走）的任务不会被 worker 再捡起来。
    let (task_tx, task_rx) = channel::<WebTask>();
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
        std::thread::spawn(move || {
            while let Ok(signal) = task_rx.recv() {
                let _ = signal;
                // 排队期间被撤掉的任务：这一轮压根不该开始——不烧预算、不叫模型。
                // 但它**必须留下痕迹**：这条任务确实存在过、被撤掉了、为什么没跑。
                // 只在 SSE 里喊一嗓子等于事后查不到，日志才是这套内核的账本。
                for (id, text) in hub.take_skipped() {
                    // 走 `log_event`，不走 `journal.append`：这里的 text 是
                    // **人自己打的那句话**，排队时撤掉不等于它没外流过——
                    // 实测直接把 AKIA… 写进了 journal.jsonl。
                    let _ = agent.log_event(
                        "turn/skipped",
                        Json::obj(vec![
                            ("task", Json::from(id.as_str())),
                            ("reason", Json::from("cancelled-while-queued")),
                            ("text", Json::from(text.as_str())),
                        ]),
                    );
                }
                // 队列为准：排队中被取消的任务已被 cancel_task 摘走，这里拿不到就是空闲。
                let task = match hub.take_queued() {
                    Some(t) => t,
                    None => continue,
                };
                hub.busy.store(true, Ordering::SeqCst);
                agent.mode = *hub_lock(&hub.mode);
                // 会话切换跟着任务走：任务入队时记录了它属于哪个会话。
                // 用全局 pending 值会把切换前排队的旧任务一起搬到新会话。
                let task_session = task.session.clone();
                if !task_session.is_empty()
                    && task_session != *hub_lock(&hub.session)
                {
                    let path = session_file(&hub.workspace, &task_session);
                    match agent.switch_journal(&path) {
                        Ok(()) => {
                            *hub_lock(&hub.session) = task_session.clone();
                            // 只清掉与本次任务相同的 pending，避免把更晚的切换吞掉。
                            let mut pending = hub_lock(&hub.pending_session);
                            if pending.as_deref() == Some(task_session.as_str()) {
                                *pending = None;
                            }
                            drop(pending);
                            // 先清掉旧会话事件，再通知切会话，最后重放目标历史。
                            // 顺序反了会让前端先清空、再被旧事件污染。
                            hub.clear_log();
                            hub.push(Json::obj(vec![
                                ("type", Json::from("session-switched")),
                                ("session", Json::from(task_session.as_str())),
                            ]));
                            replay_journal_into_hub(&hub, &path);
                        }
                        Err(e) => {
                            *hub_lock(&hub.active) = None;
                            hub.busy.store(false, Ordering::SeqCst);
                            hub.push(Json::obj(vec![
                                ("type", Json::from("error")),
                                ("taskId", Json::from(task.id.as_str())),
                                ("message", Json::from(format!("切换会话失败: {}", e))),
                            ]));
                            continue;
                        }
                    }
                }
                // 会话已经切到 task.session，现在发 user 事件；前端不会因为
                // 随后的 session-switched 清空而丢掉第一条消息。
                hub.push(Json::obj(vec![
                    ("type", Json::from("user")),
                    ("taskId", Json::from(task.id.as_str())),
                    ("text", Json::from(task.text.as_str())),
                ]));
                // 刚认领就被撤：`take_queued` 已经把它记成 active，所以这一路也会走到这儿。
                // 同样是"没开始"，账要照记，active 要清掉，否则停止按钮下次点到旧任务。
                if task.cancel.is_cancelled() {
                    *hub_lock(&hub.active) = None;
                    let _ = agent.log_event(
                        "turn/skipped",
                        Json::obj(vec![
                            ("task", Json::from(task.id.as_str())),
                            ("reason", Json::from("cancelled-on-claim")),
                            (
                                "text",
                                Json::from(
                                    task.text.chars().take(200).collect::<String>().as_str(),
                                ),
                            ),
                        ]),
                    );
                    hub.push(Json::obj(vec![
                        ("type", Json::from("task-cancelled")),
                        ("taskId", Json::from(task.id.as_str())),
                        ("stage", Json::from("queued")),
                    ]));
                    hub.busy.store(false, Ordering::SeqCst);
                    continue;
                }
                // run 边界由 worker 划：预算、工具序号、会话审批规则、取消令牌
                // 全部换成这一份任务自己的，上一任务留下的东西一样带不过来。
                agent.begin_run_with_cancel(Some(task.cancel.clone()));
                // active 在 `take_queued` 里就登记了：认领与"可被停止"之间不留窗口。
                hub.push(Json::obj(vec![
                    ("type", Json::from("task-started")),
                    ("taskId", Json::from(task.id.as_str())),
                ]));
                let mut hooks = WebHooks {
                    hub: Arc::clone(&hub),
                    auto_yes,
                };
                let report = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    agent.run_turn(&task.text, &mut hooks)
                })) {
                    Ok(report) => report,
                    Err(_) => {
                        // 一个坏工具/坏插件/坏模型输出不能把整个 worker 带走。
                        // 清理 active/approval/busy，留下事件，继续接下一轮。
                        *hub_lock(&hub.active) = None;
                        hub.clear_approval();
                        hub.busy.store(false, Ordering::SeqCst);
                        hub.push(Json::obj(vec![
                            ("type", Json::from("error")),
                            ("taskId", Json::from(task.id.as_str())),
                            ("message", Json::from("worker panicked; turn aborted")),
                        ]));
                        continue;
                    }
                };
                *hub_lock(&hub.active) = None;
                // 这一轮结束后不该再有人能回答它的审批
                hub.clear_approval();
                match report {
                    Ok(report) => hub.push(Json::obj(vec![
                        ("type", Json::from("turn")),
                        ("taskId", Json::from(task.id.as_str())),
                        ("reason", Json::from(report.reason.as_str())),
                        ("steps", Json::from(report.steps)),
                        ("calls", Json::from(report.tool_calls)),
                    ])),
                    Err(e) => hub.push(Json::obj(vec![
                        ("type", Json::from("error")),
                        ("taskId", Json::from(task.id.as_str())),
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
    // 这里印的是本机终端，不是页面：路径要印真的。「不给绝对路径」那条约束
    // 属于 HTTP 响应（agent_workspace_hint 的原始理由），启动横幅照抄它只会让人
    // 看不到自己到底在哪个工作区上开审批。
    println!("工作区     {}", hub.workspace.display());
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
    // 名字过滤看的是 URL 里的段，符号链接能把一个安全名字指到受保护目录
    // （`notes/link -> ../secrets`）。canonicalize 之后必须按真实路径再判一次，
    // 判据和工具侧共用同一个函数。
    if ui_core::protected::is_private_path(&root, &path) {
        return Err("路径不允许".to_string());
    }
    Ok((rel, path))
}

/// 不展示、也不允许直达的目录名。大小写不敏感：Windows 文件系统本身不区分大小写，
/// 只挡小写 `secrets` 等于没挡（`Secrets/` 照样能读）。
fn is_blocked_component(name: &str) -> bool {
    // 安全边界与工具侧共用 ui-core::protected 那一份清单（默认三项 +
    // `--private` 追加）：浏览器里能直达的目录不该比模型能读到的更宽，
    // 否则 `/api/file?path=.ui/journal.jsonl` 就是一条绕过策略层、
    // 把整轮提示词和工具结果捞出去的旁路。清单本身大小写不敏感。
    if name == ".." || ui_core::protected::is_private_dir_name(name) {
        return true;
    }
    // 这两个只是展示噪音（构建产物、依赖树），不是隐私边界。
    name.eq_ignore_ascii_case("target") || name.eq_ignore_ascii_case("node_modules")
}

/// 列目录时这个条目要不要藏起来。
///
/// 名字能判的先按名字判（默认清单与单段声明都是名字型的），只有人追加过
/// 声明时才补一次完整路径判定——`--private vendor/keys` 这种多段声明按
/// 名字永远排不掉，浏览器里就会露出一个能点进去的 `keys/`。工具侧的
/// `walk` 已经按同一形状做了，两边不一致的话「看得见」和「读得到」就分家了。
fn blocked_entry(workspace: &std::path::Path, path: &std::path::Path, name: &str) -> bool {
    is_blocked_component(name)
        || (ui_core::protected::has_declarations()
            && ui_core::protected::is_private_path(workspace, path))
}

fn handle(mut stream: TcpStream, hub: Arc<Hub>, task_tx: Sender<WebTask>) -> std::io::Result<()> {
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

    // CSRF：Cookie 是 SameSite=Lax，但 Lax 只保证 same-site，不保证 same-origin；
    // 127.0.0.1 上另一个端口的页面也能带 Cookie 发简单 POST。浏览器会自动带
    // Origin / Sec-Fetch-Site，必须在这里拒绝跨源写请求。
    if method != "GET" && method != "HEAD" && !same_origin_request(&headers) {
        return json_response_status(
            &mut stream,
            "403 Forbidden",
            "{\"ok\":false,\"error\":\"cross-origin request rejected\"}",
        );
    }

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
            let limit = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("limit="))
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|n| *n > 0);
            let page = snapshot_page(&hub, limit);
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
                // 隐私判定按「相对工作区的连续段」做，所以基准必须是
                // canonicalize 过的工作区：拿没解析过的 ready.workspace 去
                // strip_prefix 会失败，整条绝对路径就被当成一串目录名扫了。
                let ws_root = std::fs::canonicalize(&root).unwrap_or(root.clone());
                for e in rd.flatten().take(300) {
                    let name = e.file_name().to_string_lossy().to_string();
                    // 不展示 .git/target/node_modules/.ui/secrets（名字本身也是信息）
                    if blocked_entry(&ws_root, &e.path(), &name) {
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
        ("GET", "/api/checkpoints") => match ui_core::checkpoint::list(&hub.workspace) {
            Ok(rows) => json_response(&mut stream, &Json::Arr(rows).to_compact()),
            Err(e) => json_response_status(
                &mut stream,
                "500 Internal Server Error",
                &Json::obj(vec![("ok", Json::Bool(false)), ("error", Json::from(e))]).to_compact(),
            ),
        },
        ("GET", "/api/diff") => {
            let cid = query
                .split('&')
                .find_map(|kv| kv.split_once('='))
                .filter(|(k, _)| *k == "checkpoint")
                .map(|(_, v)| v.to_string());
            let result = match cid.as_deref() {
                Some(id) => ui_core::checkpoint::diff(&hub.workspace, Some(id)),
                None => ui_core::checkpoint::diff(&hub.workspace, None),
            };
            match result {
                Ok(report) => json_response(&mut stream, &report.to_json().to_compact()),
                Err(e) => json_response_status(
                    &mut stream,
                    "400 Bad Request",
                    &Json::obj(vec![("ok", Json::Bool(false)), ("error", Json::from(e))])
                        .to_compact(),
                ),
            }
        }
        ("POST", "/api/review") => {
            if hub_lock(&hub.active).is_some() || !hub_lock(&hub.queue).is_empty() {
                return json_response_status(
                    &mut stream,
                    "409 Conflict",
                    "{\"ok\":false,\"error\":\"任务运行中，先停止再 review\"}",
                );
            }
            let value = json::parse(body.trim()).unwrap_or(Json::Null);
            if value.bool_at("confirm") != Some(true) {
                return json_response_status(
                    &mut stream,
                    "400 Bad Request",
                    "{\"ok\":false,\"error\":\"review 需要 confirm:true\"}",
                );
            }
            let checkpoint = value.str_at("checkpoint").unwrap_or("");
            if checkpoint.is_empty() {
                return json_response_status(
                    &mut stream,
                    "400 Bad Request",
                    "{\"ok\":false,\"error\":\"缺少 checkpoint\"}",
                );
            }
            let accepted: Vec<String> = value
                .arr_at("accepted")
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            match ui_core::checkpoint::review(&hub.workspace, checkpoint, &accepted) {
                Ok(out) => json_response(&mut stream, &out.to_compact()),
                Err(e) => json_response_status(
                    &mut stream,
                    "400 Bad Request",
                    &Json::obj(vec![("ok", Json::Bool(false)), ("error", Json::from(e))])
                        .to_compact(),
                ),
            }
        }
        ("GET", "/api/state") => {
            // 页面刷新后要靠这三样把按钮恢复成真相：在跑哪个、排队有哪些、
            // 哪一张审批卡还等着人点。
            let (active, queued) = hub.task_view();
            let pending = hub.approval_view();
            let state = vec![
                ("busy", Json::Bool(hub.busy.load(Ordering::SeqCst))),
                ("seq", Json::from(hub.seq.load(Ordering::SeqCst))),
                ("minSeq", Json::from(hub.min_seq())),
                ("epoch", Json::from(hub.epoch)),
                (
                    "taskId",
                    match active {
                        Some(id) => Json::from(id),
                        None => Json::Null,
                    },
                ),
                (
                    "queued",
                    Json::Arr(queued.iter().map(|id| Json::from(id.as_str())).collect()),
                ),
                (
                    "approvalId",
                    match pending.as_ref().map(|p| p.0.clone()) {
                        Some(id) => Json::from(id),
                        None => Json::Null,
                    },
                ),
                (
                    "approvalCallId",
                    match pending.as_ref().map(|p| p.1.clone()) {
                        Some(id) => Json::from(id),
                        None => Json::Null,
                    },
                ),
            ];
            json_response(&mut stream, &Json::obj(state).to_compact())
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
            // 两种取消要分得开：停正在跑的那一轮，还是撤掉还在排队的那条消息。
            // 不带 taskId = 老语义（停当前的），带 = 只动点名那个。
            let value = json::parse(body.trim()).unwrap_or(Json::Null);
            let target = value
                .str_at("taskId")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    query
                        .split('&')
                        .find_map(|kv| kv.split_once('='))
                        .filter(|(k, _)| *k == "task")
                        .map(|(_, v)| pct_decode(v))
                });
            let (ok, outcome) = match hub.cancel_task(target.as_deref()) {
                Cancelled::Active(id) => {
                    hub.push(Json::obj(vec![
                        ("type", Json::from("cancel-requested")),
                        ("taskId", Json::from(id.as_str())),
                        ("stage", Json::from("running")),
                    ]));
                    (true, format!("running:{}", id))
                }
                Cancelled::Queued(ids) => {
                    hub.push(Json::obj(vec![
                        ("type", Json::from("task-cancelled")),
                        ("taskIds", Json::Arr(ids.iter().map(|i| Json::from(i.as_str())).collect())),
                        ("stage", Json::from("queued")),
                    ]));
                    // 叫醒 worker 去补那笔「没跑」的账：不叫的话，最后一条被撤的消息
                    // 可能永远等不到下一次唤醒，日志里就只剩「没做」。
                    let _ = task_tx.send(Hub::wake_signal());
                    (true, format!("queued:{}", ids.join(",")))
                }
                Cancelled::Idle => (false, "idle".to_string()),
            };
            json_response(
                &mut stream,
                &Json::obj(vec![
                    ("ok", Json::Bool(ok)),
                    ("outcome", Json::from(outcome.as_str())),
                    (
                        "reason",
                        Json::from(if ok {
                            ""
                        } else {
                            "现在没有正在跑或排队的任务"
                        }),
                    ),
                ])
                .to_compact(),
            )
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
            let task = hub.new_task(text.clone());
            let task_id = task.id.clone();
            // user 事件不在 HTTP 线程发：如果这次入队前有会话切换，worker 会先
            // 发 session-switched，前端会清空 DOM；这里先发 user 就会被清掉。
            // 由 worker 在切换完成后再发，保证“先切会话，再显示这条消息”。
            // 先入队再发信号：worker 只看队列，反序会让一条消息永远没人捡。
            let position = hub.enqueue(&task);
            let _ = task_tx.send(task);
            json_response(
                &mut stream,
                &Json::obj(vec![
                    ("ok", Json::Bool(true)),
                    ("taskId", Json::from(task_id.as_str())),
                    ("queued", Json::from(position)),
                ])
                .to_compact(),
            )
        }
        ("POST", "/api/approve") => {
            let value = json::parse(body.trim()).unwrap_or(Json::Null);
            // 空串与不认识的词都落到拒绝（安全默认），但回执里要说出来：
            // 静默把 `yes-pls` 变成拒绝，和静默把拒绝变成批准一样不可解释。
            let verdict = value.str_at("verdict").unwrap_or("");
            let approval_id = value.str_at("approvalId").unwrap_or("");
            let call_id = value.str_at("callId").unwrap_or("");
            let response = ApprovalResponse::parse_verdict(verdict);
            // 身份必须点名对上：旧实现谁答都算，于是「双击批准」= 批准两次，
            // 开着的老页面还能把答案投给下一张根本不同的审批卡。
            let reply = match hub.answer(approval_id, call_id, response) {
                Answer::Accepted { approval_id, call_id } => {
                    let mut reply = Json::obj(vec![
                        ("ok", Json::Bool(true)),
                        ("approvalId", Json::from(approval_id.as_str())),
                        ("callId", Json::from(call_id.as_str())),
                        ("verdict", Json::from(response.decision.name())),
                        ("scope", Json::from(response.scope.name())),
                    ]);
                    if let Some(note) = verdict_note(verdict) {
                        reply.set("note", Json::from(note));
                    }
                    reply
                }
                Answer::Stale { expected_approval_id, expected_call_id } => {
                    let mut reply = Json::obj(vec![
                        ("ok", Json::Bool(false)),
                        ("reason", Json::from("stale")),
                        ("expectedApprovalId", Json::from(expected_approval_id.as_str())),
                        ("expectedCallId", Json::from(expected_call_id.as_str())),
                    ]);
                    // 认错人之外还可能认错词：两种失败都得当场说清，
                    // 否则页面只会显示"没批下来"，人去猜是哪一边的问题。
                    reply.set("verdict", Json::from(response.decision.name()));
                    if let Some(note) = verdict_note(verdict) {
                        reply.set("note", Json::from(note));
                    }
                    reply
                }
                Answer::Gone => Json::obj(vec![
                    ("ok", Json::Bool(false)),
                    ("reason", Json::from("no-pending-approval")),
                ]),
            };
            json_response(&mut stream, &reply.to_compact())
        }
        ("GET", "/api/events") => sse(&mut stream, hub, query),
        _ => {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
            Ok(())
        }
    }
}

/// 没有 token：不给页面、不给数据，也不泄露任何工作区信息。
/// 浏览器跨源写请求防护。
///
/// 没有 Origin / Sec-Fetch-Site 的请求（curl、本地脚本）保持兼容；浏览器发起
/// 的 fetch/表单一定会带其中一个，跨源值会被拒绝。
fn same_origin_request(headers: &std::collections::HashMap<String, String>) -> bool {
    if let Some(site) = headers.get("sec-fetch-site") {
        return matches!(site.as_str(), "same-origin" | "none");
    }
    let origin = match headers.get("origin") {
        Some(o) => o,
        None => return true,
    };
    let host = match headers.get("host") {
        Some(h) => h,
        None => return false,
    };
    origin == &format!("http://{}", host) || origin == &format!("https://{}", host)
}

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
fn snapshot_page(hub: &Hub, limit: Option<usize>) -> String {
    let current = hub_lock(&hub.session).clone();
    let mut lines: Vec<String> = hub_lock(&hub.log)
        .iter()
        .filter(|(_, line)| {
            // 旧日志/测试事件没有 session 字段：保留，避免把兼容格式全过滤掉。
            match ui_core::json::parse(line) {
                Ok(event) => event
                    .str_at("session")
                    .map(|s| s == current)
                    .unwrap_or(true),
                Err(_) => true,
            }
        })
        .map(|(_, line)| line.clone())
        .collect();
    if let Some(limit) = limit {
        let start = lines.len().saturating_sub(limit.min(5_000));
        lines.drain(0..start);
    }
    let boot = lines.join(",");
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
    json_response_status(stream, "200 OK", body)
}

fn json_response_status(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
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
    let (backlog, rx) = hub.subscribe(since);
    // since 早于当前保留窗口时，先明确告诉客户端“前面已经丢了”，
    // 不要让页面以为这就是完整历史。
    let min_seq = hub.min_seq();
    if min_seq > 0 && since + 1 < min_seq {
        let notice = Json::obj(vec![
            ("type", Json::from("events-dropped")),
            ("from", Json::from(since + 1)),
            ("to", Json::from(min_seq - 1)),
            ("minSeq", Json::from(min_seq)),
        ]);
        if write!(stream, "data: {}\n\n", notice.to_compact()).is_err() {
            return Ok(());
        }
    }
    for line in backlog {
        if write!(stream, "data: {}\n\n", line).is_err() {
            return Ok(());
        }
    }
    stream.flush()?;

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

    #[test]
    fn concurrent_push_keeps_sequence_and_subscription_lossless() {
        let hub = Arc::new(Hub::default());
        let (backlog, rx) = hub.subscribe(0);
        assert!(backlog.is_empty());
        const COUNT: u64 = 32;
        let barrier = Arc::new(std::sync::Barrier::new(COUNT as usize));
        let mut workers = Vec::new();
        for i in 0..COUNT {
            let hub = hub.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                hub.push(Json::obj(vec![
                    ("type", Json::from("test")),
                    ("value", Json::from(i)),
                ]));
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        let log = hub.log.lock().unwrap();
        assert_eq!(log.len(), COUNT as usize);
        for (index, (seq, line)) in log.iter().enumerate() {
            let expected = index as u64 + 1;
            assert_eq!(*seq, expected, "日志序号必须严格递增");
            assert!(
                line.contains(&format!("\"seq\":{}", expected)),
                "事件内容与日志序号必须一致: {}",
                line
            );
        }
        drop(log);
        let mut live = Vec::new();
        for _ in 0..COUNT {
            live.push(rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap());
        }
        let logged: Vec<_> = hub
            .log
            .lock()
            .unwrap()
            .iter()
            .map(|(_, line)| line.clone())
            .collect();
        assert_eq!(live, logged, "实时流必须与日志同序，且不能丢失或重复");
    }

    #[test]
    fn approval_answer_must_name_the_approval_it_answers() {
        let hub = Hub::default();
        // 没有等待者：不能说成功
        assert!(
            matches!(hub.answer("ap-1-0", "call-1", ApprovalResponse::allow_once()), Answer::Gone),
            "没有等待者时不能返回成功"
        );
        let (tx, rx) = channel::<ApprovalResponse>();
        *hub.approval.lock().unwrap() = Some(PendingApproval {
            approval_id: "ap-1-0".into(),
            call_id: "call-1".into(),
            tx,
        });
        // 认错人：原审批必须被放回，继续等人，而不是被这一次点击消耗掉
        match hub.answer("ap-2-0", "call-9", ApprovalResponse::allow_session()) {
            Answer::Stale { expected_approval_id, expected_call_id } => {
                assert_eq!(expected_approval_id, "ap-1-0");
                assert_eq!(expected_call_id, "call-1");
            }
            other => panic!("身份不符必须回报 Stale，拿到 {:?}", other),
        }
        assert!(
            hub.approval.lock().unwrap().is_some(),
            "被拒绝的回执不能把原审批一起丢掉"
        );
        // 点名对上：一次成功，且只成功一次
        assert!(matches!(
            hub.answer("ap-1-0", "call-1", ApprovalResponse::allow_session()),
            Answer::Accepted { .. }
        ));
        assert_eq!(rx.recv().unwrap(), ApprovalResponse::allow_session());
        assert!(
            matches!(hub.answer("ap-1-0", "call-1", ApprovalResponse::deny()), Answer::Gone),
            "第二次回答必须失败"
        );

        let (tx, rx) = channel::<ApprovalResponse>();
        *hub.approval.lock().unwrap() = Some(PendingApproval {
            approval_id: "ap-3-0".into(),
            call_id: "call-3".into(),
            tx,
        });
        drop(rx);
        assert!(
            matches!(hub.answer("ap-3-0", "call-3", ApprovalResponse::allow_once()), Answer::Gone),
            "接收端断开必须返回失败"
        );
    }

    #[test]
    fn web_surface_redacts_before_it_leaves_the_process() {
        // 回归（2026-11-20 发现）：journal 已经脱敏，但 SSE 与 /snapshot 原样带出凭据。
        // /snapshot 是**可分享**的 HTML，所以"屏幕上看得到"和"文件里存着"是两件事。
        // 夹具：AWS 形状，故意写成假值（allowlist secret）。
        const CANARY: &str = "AKIAWEBREDACTTESTCANARY"; // allowlist secret（测试夹具，非真凭据）
        let hub = Arc::new(Hub::default());
        hub.push(Json::obj(vec![
            ("type", Json::from("tool")),
            ("name", Json::from("shell")),
            ("detail", Json::from(format!("export KEY={}", CANARY))),
        ]));

        {
            let log = hub.log.lock().unwrap();
            assert_eq!(log.len(), 1);
            let line = &log[0].1;
            assert!(!line.contains(CANARY), "SSE 事件不能带明文：{}", line);
            assert!(line.contains("redacted"), "应当标出抹过几处：{}", line);
        }

        // 点开「展开」看到的全文是第二处，同样要抹
        let id = hub.store_tool("shell", &ToolResult::ok(format!("cat .env\nKEY={}", CANARY)));
        let tools = hub.tools.lock().unwrap();
        let text = tools[(id - 1) as usize].str_at("text").unwrap_or("");
        assert!(!text.contains(CANARY), "工具全文库不能带明文：{}", text);
        assert!(text.contains("[REDACTED"), "应当替换成标记：{}", text);
    }

    #[test]
    fn approval_hook_emits_one_addressed_receipt() {
        let hub = Arc::new(Hub::default());
        let mut hooks = WebHooks {
            hub: hub.clone(),
            auto_yes: false,
        };
        let req = ApprovalRequest {
            approval_id: "ap-7-0".into(),
            call_id: "call-42".into(),
            tool: "shell".into(),
            args: Json::Null,
            effects: Vec::new(),
            reason: "test".into(),
        };
        let worker = std::thread::spawn(move || hooks.on_approval(&req));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while hub.approval.lock().unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "审批等待者未及时注册");
            std::thread::yield_now();
        }
        // 页面把 approvalId + callId 一起带回来，服务端才知道答的是哪一张卡
        assert!(matches!(
            hub.answer("ap-7-0", "call-42", ApprovalResponse::allow_session()),
            Answer::Accepted { .. }
        ));
        assert_eq!(worker.join().unwrap(), ApprovalResponse::allow_session());
        let log = hub.log.lock().unwrap();
        let receipts: Vec<_> = log
            .iter()
            .filter(|(_, line)| line.contains("\"type\":\"approved\""))
            .collect();
        assert_eq!(receipts.len(), 1, "一次审批只能有一条权威回执");
        assert!(receipts[0].1.contains("\"callId\":\"call-42\""));
        assert!(receipts[0].1.contains("\"approvalId\":\"ap-7-0\""));
        assert!(receipts[0].1.contains("\"verdict\":\"allow\""));
        // 作用域必须原样传出去：allow-session 不是 allow-once
        assert!(receipts[0].1.contains("\"scope\":\"session\""));
        // 人当场按下的按钮：不该被标成"代答"
        assert!(
            !receipts[0].1.contains("\"via\""),
            "人表态的回执不该有 via: {}",
            receipts[0].1
        );
        let asks: Vec<_> = log
            .iter()
            .filter(|(_, line)| line.contains("\"type\":\"approval\""))
            .collect();
        assert_eq!(asks.len(), 1);
        assert!(asks[0].1.contains("\"approvalId\":\"ap-7-0\""));
        drop(log);
        assert!(hub.approval.lock().unwrap().is_none(), "答完必须清空等待槽");
    }

    /// 页面那句"审批结果：deny"也可能是我们没听懂那句话：`via` 要一路带到 SSE，
    /// 否则只有日志能分辨"有人说不"和"没人答对"，界面上仍然在骗人。
    #[test]
    fn unrecognized_verdict_reaches_the_page_as_provenance() {
        let hub = Arc::new(Hub::default());
        let mut hooks = WebHooks {
            hub: hub.clone(),
            auto_yes: false,
        };
        let req = ApprovalRequest {
            approval_id: "ap-8-0".into(),
            call_id: "call-43".into(),
            tool: "shell".into(),
            args: Json::Null,
            effects: Vec::new(),
            reason: "test".into(),
        };
        let worker = std::thread::spawn(move || hooks.on_approval(&req));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while hub.approval.lock().unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "审批等待者未及时注册");
            std::thread::yield_now();
        }
        assert!(matches!(
            hub.answer("ap-8-0", "call-43", ApprovalResponse::parse_verdict("yes-pls")),
            Answer::Accepted { .. }
        ));
        let answer = worker.join().unwrap();
        assert_eq!(answer.decision, Decision::Deny, "听不懂仍旧按拒绝");
        assert_eq!(answer.via, Some("unknown-word"));
        let log = hub.log.lock().unwrap();
        let line = log
            .iter()
            .rev()
            .find(|(_, l)| l.contains("\"type\":\"approved\""))
            .map(|(_, l)| l.clone())
            .expect("要有回执");
        assert!(line.contains("\"via\":\"unknown-word\""), "{}", line);
    }

    #[test]
    fn queued_tasks_can_be_cancelled_before_the_worker_starts() {
        let hub = Hub::default();
        let first = hub.new_task("第一条".into());
        let second = hub.new_task("第二条".into());
        assert_eq!(hub.enqueue(&first), 0, "第一条下一个就轮到");
        assert_eq!(hub.enqueue(&second), 1, "第二条排在后面");
        match hub.cancel_task(Some(&second.id)) {
            Cancelled::Queued(ids) => assert_eq!(ids, vec!["t2".to_string()]),
            other => panic!("撤排队中的必须回报 Queued，拿到 {:?}", other),
        }
        assert_eq!(hub.take_queued().map(|t| t.id), Some("t1".to_string()));
        assert!(
            hub.take_queued().is_none(),
            "排队中被取消的任务不能被 worker 再次捡起"
        );
        assert!(
            !first.cancel.is_cancelled(),
            "取消第二条绝不能把第一条也标成已取消"
        );
        assert!(second.cancel.is_cancelled(), "撤掉的任务自己要带已取消的令牌");
        // 撤掉的这笔必须留下账：worker 醒来时按它补 turn/skipped，
        // 否则日志里只能看到「没做」，分不清是撤了、丢了、还是根本没提交。
        let skipped = hub.take_skipped();
        assert_eq!(skipped.len(), 1, "排队取消要留下一笔账");
        assert_eq!(skipped[0].0, "t2");
        assert_eq!(skipped[0].1, "第二条", "账上得写清撤掉的是哪句话");
        assert!(hub.take_skipped().is_empty(), "取走即清空：同一笔不能记两次");
    }

    /// worker 认领任务与「本轮开始」之间不能有空窗：
    /// 那段时间里点停止，绝不能回答「没有东西可取消」然后让任务照跑。
    #[test]
    fn a_claimed_task_is_stoppable_before_the_turn_starts() {
        let hub = Hub::default();
        let first = hub.new_task("第一条".into());
        hub.enqueue(&first);
        let claimed = hub.take_queued().expect("队列里有任务");
        match hub.cancel_task(Some(&claimed.id)) {
            Cancelled::Active(id) => assert_eq!(id, "t1"),
            other => panic!("认领之后必须还能被点名停止，拿到 {:?}", other),
        }
        assert!(claimed.cancel.is_cancelled(), "令牌要真的落下");
        assert!(
            hub.take_skipped().is_empty(),
            "已经认领的不算「排队时被撤」，别记两笔"
        );
    }

    #[test]
    fn cancel_hits_running_first_and_never_leaks_to_the_next_task() {
        let hub = Hub::default();
        let running = hub.new_task("正在跑".into());
        let queued = hub.new_task("排队中".into());
        *hub.active.lock().unwrap() = Some(running.clone());
        hub.enqueue(&queued);
        // 不带 taskId：只停在正在跑的那一轮，排队的条子留着下一轮用
        match hub.cancel_task(None) {
            Cancelled::Active(id) => assert_eq!(id, "t1"),
            other => panic!("正在跑时必须回报 Active，拿到 {:?}", other),
        }
        assert!(running.cancel.is_cancelled());
        assert!(hub.active_cancelled(), "审批等待循环要能看见这次取消");
        assert!(!queued.cancel.is_cancelled(), "排队的没有被波及");
        match hub.cancel_task(Some("t404")) {
            Cancelled::Idle => {}
            other => panic!("点名不存在时不该说取消了谁，拿到 {:?}", other),
        }
        // 令牌跟着任务走：新任务是干净的，上一轮的「已取消」带不过来
        let next = hub.new_task("下一个".into());
        assert!(!next.cancel.is_cancelled());
        assert_eq!(next.id, "t3", "任务 ID 全局递增，页面才能贴住停的是哪条");
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
        let mut hub = Hub::default();
        hub.workspace = ws.to_path_buf();
        let hub = Arc::new(hub);
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
        let (task_tx, task_rx) = channel::<WebTask>();
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

    fn http_post(hub: Arc<Hub>, path: &str, body: &str) -> (String, String) {
        http_request(
            hub,
            &format!(
                "POST {} HTTP/1.1\r\nHost: test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                path,
                body.len(),
                body
            ),
        )
    }

    #[test]
    fn checkpoint_diff_review_api_round_trip() {
        let ws = temp_workspace("api-review");
        std::fs::write(ws.join("a.txt"), "one\ntwo\n").unwrap();
        let cp = ui_core::checkpoint::create(&ws, "before").unwrap();
        let id = cp.str_at("id").unwrap().to_string();
        let hub = hub_for(&ws);

        let (head, listed) = http_get(hub.clone(), "/api/checkpoints");
        assert!(head.contains("200 OK"), "{}", head);
        let rows = ui_core::json::parse(listed.trim()).unwrap();
        assert!(rows
            .as_arr()
            .map(|a| a.iter().any(|r| r.str_at("id") == Some(id.as_str())))
            .unwrap_or(false));

        std::fs::write(ws.join("a.txt"), "one\nTWO\n").unwrap();
        let (head, diff) = http_get(
            hub.clone(),
            &format!("/api/diff?checkpoint={}", id),
        );
        assert!(head.contains("200 OK"), "{}", head);
        let report = ui_core::json::parse(diff.trim()).unwrap();
        let hunk = report
            .arr_at("changes")
            .and_then(|c| c.first())
            .and_then(|c| c.arr_at("hunks"))
            .and_then(|h| h.first())
            .and_then(|h| h.str_at("id"))
            .expect("hunk id")
            .to_string();

        let body = format!(
            "{{\"checkpoint\":\"{}\",\"accepted\":[\"{}\"],\"confirm\":true}}",
            id, hunk
        );
        let (head, out) = http_post(hub, "/api/review", &body);
        assert!(head.contains("200 OK"), "{} {}", head, out);
        assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "one\nTWO\n");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn cross_origin_post_is_rejected_before_it_reaches_the_agent() {
        let ws = temp_workspace("csrf");
        let hub = hub_for(&ws);
        let body = "{\"text\":\"rm -rf\"}";
        let raw = format!(
            "POST /api/say HTTP/1.1\r\nHost: 127.0.0.1:1\r\nOrigin: http://127.0.0.1:9999\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let (head, body) = http_request(hub, &raw);
        assert!(
            head.contains("403"),
            "cross-origin POST must be 403: {} body={}",
            head,
            body
        );
        assert!(body.contains("cross-origin"), "{}", body);
    }

    #[test]
    fn approve_route_needs_the_exact_approval_identity() {
        // 回归：`/api/approve` 过去只看 verdict——老页面、双击、别的标签页，
        // 谁都能把答案投给当前那张卡。现在必须点名，认错人则原审批继续等。
        let ws = temp_workspace("approve-route");
        let hub = hub_for(&ws);
        let (tx, _rx) = channel::<ApprovalResponse>();
        *hub.approval.lock().unwrap() = Some(PendingApproval {
            approval_id: "ap-1-0".into(),
            call_id: "call-1".into(),
            tx,
        });
        let (_head, out) = http_post(
            hub.clone(),
            "/api/approve",
            r#"{"verdict":"allow","approvalId":"ap-0-9","callId":"call-0"}"#,
        );
        assert!(out.contains("\"reason\":\"stale\""), "认错人要能分辨: {}", out);
        assert!(
            out.contains("\"expectedApprovalId\":\"ap-1-0\""),
            "要告诉页面当前是哪张卡: {}",
            out
        );
        assert!(
            hub.approval.lock().unwrap().is_some(),
            "被拒绝的回执不能把原审批一起丢掉"
        );
        // 没有等待者时也不能假装成功
        let pending = hub.approval.lock().unwrap().take();
        drop(pending);
        let (_head, out) = http_post(
            hub.clone(),
            "/api/approve",
            r#"{"verdict":"allow","approvalId":"ap-1-0","callId":"call-1"}"#,
        );
        assert!(
            out.contains("\"reason\":\"no-pending-approval\""),
            "审批已结束时要说清楚: {}",
            out
        );
        // 点名对上：allow 还要区分 once / session
        let (tx, rx) = channel::<ApprovalResponse>();
        *hub.approval.lock().unwrap() = Some(PendingApproval {
            approval_id: "ap-2-0".into(),
            call_id: "call-2".into(),
            tx,
        });
        let (_head, out) = http_post(
            hub.clone(),
            "/api/approve",
            r#"{"verdict":"always","approvalId":"ap-2-0","callId":"call-2"}"#,
        );
        assert!(out.contains("\"ok\":true"), "{}", out);
        assert!(out.contains("\"scope\":\"session\""), "作用域必须回显: {}", out);
        assert_eq!(rx.recv().unwrap(), ApprovalResponse::allow_session());

        // 认错人时也要说清用的是哪个词——否则页面只显示"没批下来"，
        // 人会去点第二次，而不是改自己写的 verdict。
        let (tx, _rx) = channel::<ApprovalResponse>();
        *hub.approval.lock().unwrap() = Some(PendingApproval {
            approval_id: "ap-3-0".into(),
            call_id: "call-3".into(),
            tx,
        });
        let (_head, out) = http_post(
            hub.clone(),
            "/api/approve",
            r#"{"verdict":"yes-pls","approvalId":"ap-9","callId":"call-9"}"#,
        );
        assert!(out.contains("\"reason\":\"stale\""), "{}", out);
        assert!(
            out.contains("\"verdict\":\"deny\""),
            "不认识的词要如实按拒绝回显: {}",
            out
        );
        assert!(out.contains("note"), "要说明为什么是拒绝: {}", out);

        // 认得词但没点名：仍然只是 stale，不该带 note
        let (_head, out) = http_post(
            hub.clone(),
            "/api/approve",
            r#"{"verdict":"allow","approvalId":"ap-9","callId":"call-9"}"#,
        );
        assert!(out.contains("\"reason\":\"stale\""), "{}", out);
        assert!(!out.contains("\"note\""), "认识的词不该被提醒: {}", out);

        // 点名的未知词：按拒绝处理但要说出来
        let (tx, rx) = channel::<ApprovalResponse>();
        *hub.approval.lock().unwrap() = Some(PendingApproval {
            approval_id: "ap-4-0".into(),
            call_id: "call-4".into(),
            tx,
        });
        let (_head, out) = http_post(
            hub.clone(),
            "/api/approve",
            r#"{"approvalId":"ap-4-0","callId":"call-4"}"#,
        );
        assert!(out.contains("\"ok\":true"), "{}", out);
        assert!(out.contains("\"verdict\":\"deny\""), "{}", out);
        assert!(out.contains("note"), "缺 verdict 也要说明: {}", out);
        // 账本上还得更正"是谁替人做的决定"：没人答 ≠ 有人说了不。
        assert_eq!(rx.recv().unwrap(), ApprovalResponse::deny().via("unknown-word"));
    }

    #[test]
    fn cancel_route_separates_running_from_queued() {
        // 回归：「停止」曾只有一个全局令牌——排队中的消息撤不掉，
        // 而且取消状态会顺着令牌流到下一条消息。
        let ws = temp_workspace("cancel-route");
        let hub = hub_for(&ws);
        let (_head, out) = http_post(hub.clone(), "/api/cancel", "{}");
        assert!(out.contains("\"ok\":false"), "空闲时不该谎称停了什么: {}", out);
        assert!(out.contains("idle"), "{}", out);

        let first = hub.new_task("第一条".into());
        let second = hub.new_task("第二条".into());
        *hub.active.lock().unwrap() = Some(first.clone());
        hub.enqueue(&second);
        // 只撤排在后面的那条
        let (_head, out) = http_post(hub.clone(), "/api/cancel", r#"{"taskId":"t2"}"#);
        assert!(out.contains("queued:t2"), "{}", out);
        assert!(second.cancel.is_cancelled(), "撤掉的那条必须带已取消令牌");
        assert!(!first.cancel.is_cancelled(), "正在跑的那轮不该被牵连");
        // 不带 taskId：停在正在跑的
        let (_head, out) = http_post(hub.clone(), "/api/cancel", "{}");
        assert!(out.contains("running:t1"), "{}", out);
        assert!(first.cancel.is_cancelled());
        // 状态查询要能告诉页面「在跑哪个 / 队列里还有谁」
        *hub.active.lock().unwrap() = None;
        let third = hub.new_task("第三条".into());
        hub.enqueue(&third);
        let (_head, state) = http_get(hub, "/api/state");
        assert!(state.contains("\"taskId\":null"), "空闲时不该有在跑的任务: {}", state);
        assert!(state.contains("\"queued\":[\"t3\"]"), "排队列表要能看见: {}", state);
    }

    /// 清单是进程级共享状态，而 `cargo test` 多线程跑用例：碰它的那个用例
    /// 自己串行。用例会结束在空清单上，所以别的用例不受牵连。
    fn private_list_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn declared_private_paths_stay_invisible_to_the_browser() {
        let _g = private_list_guard();
        ui_core::protected::set_privates(&["vendor/keys"]).unwrap();
        let ws = temp_workspace("priv");
        std::fs::create_dir_all(ws.join("vendor").join("keys")).unwrap();
        std::fs::write(ws.join("vendor").join("keys").join("deploy.pem"), "PRIV").unwrap();
        std::fs::create_dir_all(ws.join("vendor").join("licenses")).unwrap();
        std::fs::write(ws.join("vendor").join("licenses").join("MIT.txt"), "ok").unwrap();
        std::fs::create_dir_all(ws.join("notes")).unwrap();
        std::fs::write(ws.join("notes").join("a.md"), "hi").unwrap();
        let hub = hub_for(&ws);

        // 直达文件：多段声明也要挡住，判定看的是完整路径而不是最后一段名字
        for target in [
            "/api/file?path=vendor/keys/deploy.pem",
            "/api/tree?path=vendor/keys",
            "/api/tree?path=vendor%2Fkeys%2Fdeploy.pem",
        ] {
            let (_head, body) = http_get(hub.clone(), target);
            assert!(body.contains("\"error\""), "{} 应当被拒绝: {}", target, body);
            assert!(!body.contains("PRIV"), "{} 泄露了内容: {}", target, body);
        }

        // 父目录的列表里不能出现 `keys`：名字本身是信息，而且点了就该 404
        let (head, body) = http_get(hub.clone(), "/api/tree?path=vendor");
        assert!(head.contains("200 OK"), "{}", head);
        assert!(!body.contains("keys"), "列表里露出了声明的隐私目录: {}", body);
        assert!(
            body.contains("licenses"),
            "同级的正常目录被误伤了: {}",
            body
        );

        // 没声明过的路径一切照旧——否则这条测试只是在证明"全挡了"
        let (head, body) = http_get(hub.clone(), "/api/file?path=notes/a.md");
        assert!(head.contains("200 OK"), "{}", head);
        assert!(body.contains("hi"), "{}", body);

        let _ = std::fs::remove_dir_all(&ws);
        ui_core::protected::set_privates(&[]).unwrap();
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
        // 大小写变体必须一起挡：Windows 文件系统不区分大小写，只挡小写等于没挡。
        std::fs::create_dir_all(ws.join("Secrets")).unwrap();
        std::fs::write(ws.join("Secrets").join("token.txt"), "s3cr3t-caps").unwrap();
        // .ui 里是整轮提示词与工具结果——它是隐私边界，不是"隐藏目录"。
        std::fs::create_dir_all(ws.join(".ui")).unwrap();
        std::fs::write(ws.join(".ui").join("journal.jsonl"), "{\"kind\":\"prompt/snapshot\"}").unwrap();
        // target/node_modules 只是噪音，但走的是同一个判据，不能有旁路。
        std::fs::create_dir_all(ws.join("target")).unwrap();
        std::fs::write(ws.join("target").join("build.txt"), "artifact").unwrap();
        std::fs::create_dir_all(ws.join("node_modules")).unwrap();
        std::fs::write(ws.join("node_modules").join("pkg.txt"), "dep").unwrap();

        let (_head, ok) = http_get(hub_for(&ws), "/api/file?path=src%2Fa.js");
        assert!(ok.contains("\"ok\":true"), "正常文件应能预览: {}", ok);
        assert!(ok.contains("export const a = 1"), "{}", ok);

        for target in [
            "/api/file?path=/etc/passwd",
            "/api/file?path=%2Fetc%2Fpasswd",
            "/api/file?path=secrets%2Ftoken.txt",
            "/api/file?path=..%2Fetc%2Fpasswd",
            "/api/file?path=Secrets%2Ftoken.txt",
            "/api/file?path=SECRETS%2Ftoken.txt",
            "/api/file?path=.ui%2Fjournal.jsonl",
            "/api/file?path=%2Eui%2Fjournal.jsonl",
            "/api/file?path=target%2Fbuild.txt",
            "/api/file?path=node_modules%2Fpkg.txt",
            "/api/file?path=src%2F..%2F..%2Fetc%2Fpasswd",
        ] {
            let (_head, body) = http_get(hub_for(&ws), target);
            assert!(body.contains("\"ok\":false"), "{} 应被拒绝: {}", target, body);
            assert!(!body.contains("s3cr3t"), "{} 泄露了 secrets: {}", target, body);
            assert!(
                !body.contains("prompt/snapshot"),
                "{} 泄露了内核日志: {}",
                target,
                body
            );
        }
        // 根列表同理：名字本身也是信息——"这里有 .ui/有 journal"已经泄了一半。
        let (_head, root) = http_get(hub_for(&ws), "/api/tree");
        for leak in ["secrets", "Secrets", ".ui", "journal", "target", "node_modules"] {
            assert!(
                !root.contains(&format!("\"name\":\"{leak}\"")),
                "根列表出现了 {leak}: {root}"
            );
        }
        assert!(root.contains("\"src\""), "正常目录仍要能列出: {root}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn tool_cache_is_isolated_by_session() {
        let hub = Arc::new(Hub::default());
        *hub.session.lock().unwrap() = "a".into();
        let id = hub.store_tool("shell", &ToolResult::ok("secret-output"));
        *hub.session.lock().unwrap() = "b".into();
        assert!(
            hub.tool(id).is_none(),
            "切到 b 后不能按 id 读到 a 的工具全文"
        );
        *hub.session.lock().unwrap() = "a".into();
        assert!(hub.tool(id).is_some());
    }

    #[test]
    fn hub_log_is_capped_and_reports_the_missing_window() {
        let hub = Arc::new(Hub::default());
        for i in 0..5_050u64 {
            hub.push(Json::obj(vec![
                ("type", Json::from("note")),
                ("i", Json::from(i)),
            ]));
        }
        let log = hub_lock(&hub.log);
        assert_eq!(log.len(), 5_000);
        let first = log.first().map(|(n, _)| *n).unwrap();
        let last = log.last().map(|(n, _)| *n).unwrap();
        assert_eq!(first, 51);
        assert_eq!(last, 5_050);
        assert_eq!(hub.min_seq(), first);
    }

    #[test]
    fn poisoning_the_hub_log_does_not_kill_future_pushes() {
        let hub = Arc::new(Hub::default());
        let h = Arc::clone(&hub);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = h.log.lock().unwrap();
            panic!("intentional lock poison");
        }));
        hub.push(Json::obj(vec![("type", Json::from("note"))]));
        assert_eq!(hub_lock(&hub.log).len(), 1);
    }

    #[test]
    fn replays_journal_into_chat_events() {
        let ws = temp_workspace("replay-journal");
        let path = ws.join(".ui").join("journal.jsonl");
        let mut journal = ui_core::Journal::open(&path).unwrap();
        journal
            .append("turn/start", Json::obj(vec![("prompt", Json::from("旧问题"))]))
            .unwrap();
        journal
            .append(
                "model/response",
                Json::obj(vec![("content", Json::from("旧回答"))]),
            )
            .unwrap();
        journal
            .append(
                "tool/output",
                Json::obj(vec![
                    ("tool", Json::from("fs_read")),
                    ("ok", Json::Bool(true)),
                    ("text", Json::from("文件内容")),
                ]),
            )
            .unwrap();
        journal
            .append(
                "turn/end",
                Json::obj(vec![("reason", Json::from("done"))]),
            )
            .unwrap();
        drop(journal);

        let hub = hub_for(&ws);
        *hub.session.lock().unwrap() = "default".into();
        replay_journal_into_hub(&hub, &path);
        let log = hub.log.lock().unwrap();
        let types: Vec<String> = log
            .iter()
            .filter_map(|(_, line)| ui_core::json::parse(line).ok())
            .filter_map(|e| e.str_at("type").map(str::to_string))
            .collect();
        assert_eq!(
            types,
            vec!["user", "assistant", "tool", "turn"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn snapshot_limit_only_embeds_the_recent_window() {
        let ws = temp_workspace("snapshot-limit");
        let hub = hub_for(&ws);
        for i in 0..10 {
            hub.push(Json::obj(vec![
                ("type", Json::from("note")),
                ("i", Json::from(i)),
            ]));
        }
        let page = snapshot_page(&hub, Some(3));
        assert_eq!(
            page.matches("\"type\":\"note\"").count(),
            3,
            "snapshot?limit=3 只应嵌入最近 3 条事件"
        );
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
