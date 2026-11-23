//! The loop: seven contracts in one place.
//!
//! Every step builds a snapshot, asks the model, and dispatches tool calls through
//! policy, budget and journal. A malformed tool argument is repaired (bounded),
//! never fatal. A started-but-unfinished effect is reconciled on resume instead of
//! being silently repeated or silently lost.

use crate::budget::{Budget, BudgetExceeded, Dim, Limits};
use crate::json::Json;
use crate::journal::Journal;
use crate::model::{Model, ToolCall};
use crate::policy::{Decision, Effect, Policy, Rule, Verdict};
use crate::cancel::Cancel;
use crate::compact::{compact, CompactPolicy};
use crate::prompt::{build_snapshot, Message, Snapshot};
use crate::tools::{find, registry, Ctx, Tool, ToolResult};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// 落盘原文的保留策略，在一个进程里最多多久扫一次（毫秒）。
///
/// 清理挂在 run 边界，但 `ui serve` 是长驻的、每个任务都开一个边界；
/// 每次都遍历目录，等于把隐私维护变成了任务延迟。隐私侧的代价是
/// 「过期文件最多多活 60 秒」，容量侧的代价是零——反正还没到上限。
const SPILL_PRUNE_INTERVAL_MS: u64 = 60_000;

#[derive(Debug)]
pub enum AgentError {
    Budget(BudgetExceeded),
    Model(String),
    Journal(io::Error),
    /// 记忆文件坏了/写不进去：在写 `turn/start` 之前失败，不静默当成空记忆。
    Memory(String),
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::Budget(e) => write!(f, "{}", e),
            AgentError::Model(e) => write!(f, "模型调用失败: {}", e),
            AgentError::Journal(e) => write!(f, "日志写入失败: {}", e),
            AgentError::Memory(e) => write!(f, "记忆失败: {}", e),
        }
    }
}

impl std::error::Error for AgentError {}

impl From<io::Error> for AgentError {
    fn from(e: io::Error) -> Self {
        AgentError::Journal(e)
    }
}

impl From<BudgetExceeded> for AgentError {
    fn from(e: BudgetExceeded) -> Self {
        AgentError::Budget(e)
    }
}

/// 一次审批请求的可寻址身份。
///
/// `call_id` 标识“哪个工具调用”，`approval_id` 标识“哪一次等待人工回答”。
/// 两者都必须是全局唯一的：否则上一个任务的回答会落到下一个任务上，
/// 用户点「拒绝」却批准了另一件事。
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub approval_id: String,
    pub call_id: String,
    pub tool: String,
    pub args: Json,
    pub effects: Vec<Effect>,
    pub reason: String,
}

/// 批准的作用域。`y` 与 `a` 不是同一个决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalScope {
    Once,
    Session,
}

impl ApprovalScope {
    pub fn name(self) -> &'static str {
        match self {
            ApprovalScope::Once => "once",
            ApprovalScope::Session => "session",
        }
    }

    pub fn parse(s: &str) -> ApprovalScope {
        match s {
            "session" | "always" | "a" => ApprovalScope::Session,
            _ => ApprovalScope::Once,
        }
    }
}

/// Hooks 对一个审批请求的完整回答。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovalResponse {
    pub decision: Decision,
    pub scope: ApprovalScope,
    /// 这个回答是**怎么来的**：`None` = 有人明确表态；否则写清是谁替人做的决定
    /// （`timeout` 300 秒没人答、`cancelled` 被停止打断、`no-channel` 没有审批通道、
    /// `auto` 命令行 `--yes` 代批）。
    ///
    /// 这不是装饰：日志里一条 `verdict=deny` 如果分不清"人说了不"和"没人来得及说"，
    /// 复盘时就会把一次网络卡顿读成一次否决——而这两种情况的处置完全相反。
    pub via: Option<&'static str>,
}

impl ApprovalResponse {
    pub fn allow_once() -> ApprovalResponse {
        ApprovalResponse {
            decision: Decision::Allow,
            scope: ApprovalScope::Once,
            via: None,
        }
    }

    pub fn allow_session() -> ApprovalResponse {
        ApprovalResponse {
            decision: Decision::Allow,
            scope: ApprovalScope::Session,
            via: None,
        }
    }

    /// 标出"这个决定不是人做的"。构造后调用，保持三个快捷构造器不变。
    pub fn via(mut self, via: &'static str) -> ApprovalResponse {
        self.via = Some(via);
        self
    }

    /// 人明确说了"不"。决定不是人做的，用 `deny().via(…)` 标出来。
    pub fn deny() -> ApprovalResponse {
        ApprovalResponse {
            decision: Decision::Deny,
            scope: ApprovalScope::Once,
            via: None,
        }
    }

    /// 兼容只返回 `Decision` 的旧调用点。新的授权范围必须是“一次”，
    /// 不能把 allow 悄悄放大成会话规则。
    pub fn from_decision(decision: Decision) -> ApprovalResponse {
        ApprovalResponse {
            decision,
            scope: ApprovalScope::Once,
            via: None,
        }
    }

    /// 终端按键、页面按钮、人手写的脚本用的不是同一套词：`y` 是按键，`yes` 是人自然会打的字。
    /// 两者都只算「放这一次」；认不出来的仍旧落到拒绝——口子只在写明的位置放宽。
    pub fn parse_verdict(verdict: &str) -> ApprovalResponse {
        match verdict {
            "allow" | "once" | "allow-once" | "y" | "Y" | "yes" | "Yes" | "YES" => {
                ApprovalResponse::allow_once()
            }
            "always" | "session" | "allow-always" | "a" | "A" => ApprovalResponse::allow_session(),
            "deny" | "n" | "N" | "no" | "No" | "NO" => ApprovalResponse::deny(),
            // 认不出的词仍旧拒绝（安全默认不变），但留下记号：
            // 「人说了不」和「人说了句我们听不懂的话」在复盘时是完全不同的两件事。
            _ => ApprovalResponse::deny().via("unknown-word"),
        }
    }

    /// 这个词是不是明确写出来的批准/拒绝词。落到默认分支的词仍然按拒绝处理
    /// （安全默认不变），但界面要能分辨「你说的是拒绝」和「你说的是别的」——
    /// 把 `yes-pls` 静默当成拒绝，和把拒绝静默当成批准一样不可解释。
    pub fn is_known_verdict(verdict: &str) -> bool {
        matches!(
            verdict,
            "allow"
                | "once"
                | "allow-once"
                | "y"
                | "Y"
                | "yes"
                | "Yes"
                | "YES"
                | "always"
                | "session"
                | "allow-always"
                | "a"
                | "A"
                | "deny"
                | "n"
                | "N"
                | "no"
                | "No"
                | "NO"
        )
    }
}

pub struct ApprovalTicket<'a> {
    pub approval_id: &'a str,
    pub call_id: &'a str,
    pub response: ApprovalResponse,
}

pub trait Hooks {
    fn on_event(&mut self, _event: &Json) {}
    /// 模型正在逐字输出（用于展示的增量，不是最终结果；最终以 on_assistant 为准）。
    fn on_delta(&mut self, _text: &str) {}
    fn on_assistant(&mut self, _text: &str) {}
    /// 一步的快照已生成：右栏据此显示分段与计价（契约 1）。
    fn on_snapshot(&mut self, _snapshot: &Snapshot) {}
    /// 一次工具调用的策略结论（allow/ask/deny + 依据）（契约 3）。
    fn on_decision(&mut self, _call_id: &str, _tool: &str, _decision: Decision, _reason: &str) {}
    fn on_tool(&mut self, _name: &str, _args: &Json, _result: &ToolResult) {}
    /// 账本变化：前端据此实时更新预算条，而不是等一轮结束才知道花了多少。
    fn on_budget(&mut self, _tokens: u64, _tool_calls: u64) {}
    /// 历史被折叠：前端应把它显示出来，不能让上下文悄悄变短。
    fn on_compact(&mut self, _before: u64, _after: u64, _dropped: usize) {}
    fn on_approval(&mut self, _req: &ApprovalRequest) -> ApprovalResponse {
        // 没有审批通道：这不是"有人拒绝"，是"没人能回答"。
        // 记成 no-channel，日志才不会把一个基础设施缺失读成一次否决。
        ApprovalResponse::deny().via("no-channel")
    }
}

pub struct NullHooks;
impl Hooks for NullHooks {}

#[derive(Debug, Clone)]
pub struct TurnReport {
    pub reason: String,
    pub steps: usize,
    pub tool_calls: usize,
    pub tokens: u64,
    pub final_text: String,
}

pub struct Agent {
    pub journal: Journal,
    pub model: Box<dyn Model + Send>,
    pub policy: Policy,
    pub budget: Budget,
    pub tools: Vec<Tool>,
    pub workspace: PathBuf,
    pub goal: Option<String>,
    pub max_steps: usize,
    pub max_repairs: usize,
    /// 历史折叠策略（跨轮生效）
    pub compact: CompactPolicy,
    /// 取消令牌：UI 在后台线程置位，模型适配器与循环据此中止
    pub cancel: Cancel,
    /// 当前 run 的取消令牌。长驻 worker 每个任务一个；run_turn 绝不能清空它。
    active_cancel: Cancel,
    /// 跨轮对话历史：同一会话里多次 run_turn 共享它。
    pub history: Vec<Message>,
    /// 已加载的 skill 正文（会作为独立段落进入快照，可计价）
    pub loaded_skills: Vec<(String, String)>,
    /// 工具输出在上下文里最多保留多少字符（0 = 不截断）。超出部分落盘。
    pub tool_chars: usize,
    /// 每个 turn 第一次写操作前自动建 checkpoint；`UI_AUTO_CHECKPOINT=0` 可关。
    pub auto_checkpoint: bool,
    /// 记忆（workspace 级，跨会话）。`memory_enabled=false` 时完全不动它。
    pub memory: crate::memory::Store,
    pub memory_enabled: bool,
    /// 单轮注入预算（tokens）。0 = 不注入；`UI_MEMORY_BUDGET` 覆盖默认 300。
    pub memory_tokens: u64,
    /// 本轮命中的记忆（`ui run` 结束后打印；也是 `memory/used` 的来源）。
    pub memory_hits: Vec<crate::memory::Hit>,
    /// 检索用的 query 输入。CLI 在 `knowledge` 预取改写 `input` 之前把原始任务放这里，
    /// 避免「知识卡正文」把记忆检索带偏；为 None 时直接用本轮 input。
    pub memory_query: Option<String>,
    /// 运行模式：只调决策严格度，不改日志格式
    pub mode: crate::mode::Mode,
    /// 本轮是否已经写过 `privacy/redact-off`（一轮一条，别刷事件流）
    ///
    /// 为什么不写在 `begin_run` 里：CLI 是 `begin_run()` **之后**才应用
    /// `--no-redact` 的（实测漏记了一整轮）。挂在第一次下笔之前，调用顺序就
    /// 无所谓了，而且它保证出现在第一段原文前面——这才是这条记号的意义。
    redact_off_noted: bool,
    /// 子进程环境卫生：默认把名字看起来像密钥的环境变量从 shell 子进程里剔掉
    /// （`--no-env-scrub` 关）。判据与 `ui secrets` 共用 `secrets::key_is_secret`。
    pub env_scrub: bool,
    /// 插件集（`--plugin` 挂上来的工具与效果声明；没装就是 None）
    pub plugins: Option<std::sync::Arc<crate::plugin::Set>>,
    /// 观测式效果：shell/插件/MCP 执行前后取差量，把实际改动写进日志
    pub env_observe: bool,
    /// inotify watcher（Linux）。有它就不扫描，直接 drain 事件。
    pub watcher: Option<crate::observe::Watcher>,
    /// OS 沙箱配置（Linux Landlock）
    pub sandbox: crate::sandbox::Config,
    /// 外部 MCP server（未配置时为 None）。
    pub mcp: Option<std::sync::Arc<std::sync::Mutex<crate::mcp::Hub>>>,

    tool_seq: u64,
    /// 上一次按保留策略扫落盘原文（两个目录）的时刻（毫秒）。
    /// 归零意味着"还没扫过"，第一次 run 边界一定会扫。
    spill_pruned_at: u64,
    /// 本次 model 调用（含 repair 重试）实际计入的估算 token，用于对账。
    last_model_estimated: u64,
    turn: u64,
    approval_seq: u64,
}

/// 读文件留痕（自由函数：只借 journal 与 workspace，避免和 `&self.tools` 冲突）。
fn record_reads(
    journal: &mut Journal,
    workspace: &Path,
    tool: &str,
    call_id: &str,
    effects: &[Effect],
) -> Result<(), AgentError> {
    for e in effects {
        if e.kind != "fs.read" {
            continue;
        }
        let p = Path::new(&e.target);
        let meta = match std::fs::metadata(p) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() || meta.len() > 8 * 1024 * 1024 {
            continue;
        }
        let bytes = match std::fs::read(p) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let rel = p
            .strip_prefix(workspace)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/");
        journal.append(
            "fs/read",
            Json::obj(vec![
                ("callId", Json::from(call_id)),
                ("tool", Json::from(tool)),
                ("path", Json::from(rel.as_str())),
                ("sha256", Json::from(crate::sha256::hex(&bytes))),
                ("bytes", Json::from(meta.len())),
            ]),
        )?;
    }
    Ok(())
}

impl Agent {
    /// 账上那条「全文在 X」现在还指得到东西吗？
    ///
    /// 保留策略会把过期的落盘原文删掉（`spill::prune`），历史里的指针也就跟着断了。
    /// 还原历史时不能照抄那句话：模型会去 `fs_read` 一份不存在的文件，
    /// 然后得出"上次什么都没读到"的结论，再跑一次那条隐私命令——
    /// 隐私侧最坏的情况不是读不到，是以为读不到所以重读一遍。
    /// 日志里那句"正文在哪"要看目录。`.ui/tool-output/` 在只读例外里，模型
    /// 自己 `fs_read` 就捞得回来，所以指针要给它路径；`.ui/tool-private/` 不在
    /// 例外里——那是按隐私清单扣留的内容，只有人（和重放）看得见。把后者也写成
    /// "全文在 X"，等于给它一条必然被拒、却值得反复试探的引用，第二层隐私塌成
    /// 一层。
    fn pointer_note(&self, file: &str) -> String {
        let p = Path::new(file);
        let exists = if p.is_absolute() {
            p.is_file()
        } else {
            self.workspace.join(p).is_file()
        };
        let withheld = p
            .parent()
            .map(|d| d.ends_with(crate::spill::PRIVATE_DIR))
            .unwrap_or(false);
        if withheld {
            if exists {
                "正文按隐私清单扣留（只给人看，模型读不到）".to_string()
            } else {
                "正文按隐私清单扣留，原文已过保留期".to_string()
            }
        } else if exists {
            format!("全文在 {}", file)
        } else {
            format!("全文（原 {}）已按保留策略清理，找不回来了", file)
        }
    }

    /// 切换到另一个会话：换日志文件，并按新日志重建对话历史。
    ///
    /// 会话在磁盘上就是「另一个 JSONL」，不需要新概念；但历史必须跟着换，
    /// 否则会把上一条会话的上下文带进新会话（那是数据泄漏）。
    pub fn switch_journal(&mut self, path: &Path) -> Result<(), AgentError> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let journal = crate::journal::Journal::open(path)?;
        let events = match crate::journal::Journal::read(path) {
            Ok((events, None)) => events,
            Ok((events, Some(c))) if c.torn => events,
            Ok((_, Some(c))) => {
                return Err(AgentError::Journal(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("会话日志损坏：{}", c.reason),
                )))
            }
            Err(e) => return Err(AgentError::Journal(e)),
        };
        let mut history: Vec<crate::prompt::Message> = Vec::new();
        for e in &events {
            match e.str_at("kind") {
                Some("turn/start") => {
                    if let Some(p) = e.str_at("prompt") {
                        history.push(crate::prompt::Message::user(p));
                    }
                }
                Some("model/response") => {
                    if let Some(c) = e.str_at("content") {
                        if !c.is_empty() {
                            history.push(crate::prompt::Message::assistant(c));
                        }
                    }
                }
                // 评审 P1-1f：切换会话时工具结果也要还原，否则模型"忘了"上一条会话里
                // 工具返回过什么。旧日志没有这个事件，只能退化成"没有工具输出"。
                Some("tool/output") => {
                    if let Some(tool) = e.str_at("tool") {
                        let ok = e.bool_at("ok").unwrap_or(true);
                        // 扣留的那一档没有正文（只有字数与指纹）。还原时必须
                        // 补一句指针，否则模型看到一条空工具结果，会以为
                        // "上次读了但什么都没读到"，然后重跑那次读取。
                        let text = match e.str_at("text") {
                            Some(t) if !t.is_empty() => t.to_string(),
                            _ => match e.str_at("file") {
                                Some(f) => format!(
                                    "（输出 {} 字，来源命中隐私清单，正文未写入日志；{}）",
                                    e.u64_at("chars").unwrap_or(0),
                                    self.pointer_note(f)
                                ),
                                None => String::new(),
                            },
                        };
                        if !text.is_empty() {
                            history.push(crate::prompt::Message::tool(tool, ok, &text));
                        }
                    }
                }
                _ => {}
            }
        }
        // 会话日志是另一个文件，`Journal::open` 会按 UI_REDACT 重新起一份开关；
        // 这一轮的设置以 Agent 当前这份为准，不然人关掉了脱敏会在换会话时悄悄复原。
        let redact = self.journal.redacting();
        self.journal = journal;
        self.journal.set_redact(redact);
        // turn 编号来自日志，不是全局 run 计数；换日志必须跟着换，否则
        // 空会话第一轮可能从 57 开始，切回来又会重复 4、5。
        self.turn = self.journal.turn_count();
        self.history = history;
        self.begin_run();
        Ok(())
    }

    /// 开始一次新的 run：换上一个全新的取消令牌，并清零只能属于一次任务的状态。
    pub fn begin_run(&mut self) {
        self.begin_run_with_cancel(None);
    }

    /// 长驻 worker 为每个任务创建一个令牌；排队期间即可取消，
    /// 且上一任务的「已取消」绝不能污染下一任务。
    pub fn begin_run_with_cancel(&mut self, cancel: Option<Cancel>) {
        let cancel = cancel.unwrap_or_else(Cancel::new);
        // TUI 复用同一个 token：上一轮的 Esc 不能粘到这一轮。hard cancel
        // （父进程死亡）在 reset 里不会被清掉。
        cancel.reset();
        self.active_cancel = cancel.clone();
        self.cancel = cancel;
        self.budget.reset();
        self.tool_seq = 0;
        self.last_model_estimated = 0;
        self.approval_seq = 0;
        self.policy.reset_session_rules();
        self.loaded_skills.clear();
        self.memory_hits.clear();
        self.memory_query = None;
        // 「这一轮的日志是原文」这条记号由第一次落账补写（见 `note_redact_off`），
        // 这里只负责让它在新的一轮重新写一次。
        self.redact_off_noted = false;
        // run 边界顺手把过期的落盘原文清掉（见 `prune_spill`）。
        self.prune_spill();
    }

    /// 关掉脱敏这件事要在日志里看得见：`--no-redact` 是给人留的后门，
    /// 但三个月后翻日志的人得能回答「这段原文是谁允许的」。
    /// 一轮只写一条，写在第一段原文进日志之前。
    fn note_redact_off(&mut self) {
        if self.journal.redacting() || self.redact_off_noted {
            return;
        }
        self.redact_off_noted = true;
        // append_raw：这条记号本身就是给原文作证的，别让它被门过一遍手
        let _ = self.journal.append_raw(
            "privacy/redact-off",
            Json::obj(vec![("reason", Json::from("--no-redact / UI_REDACT=0"))]),
        );
    }

    /// 工具输出**落账前**脱敏（默认开）。
    ///
    /// 关掉的唯一后果是 `.ui/journal.jsonl` 里留下密钥原文——日志会被
    /// `ui replay` 重印、被 `ui export --html` 打包、被人贴进 issue，
    /// 它是这个项目里外流面最大的一个文件。所以默认脱敏，`--no-redact`
    /// 回退，且回退本身会在日志里留下一条 `privacy/redact-off`。
    ///
    /// `UI_REDACT=0` 与 `--no-redact` 同义：`Journal::open` 已经读过环境变量，
    /// 这里只是给它一个改的入口——开关只有一份状态，改在哪里都立刻生效，
    /// 调用顺序不再重要（CLI 过去就在 `begin_run()` 之后才改，漏记过一整轮）。
    pub fn set_redact_output(&mut self, on: bool) {
        self.journal.set_redact(on);
    }

    pub fn redact_output(&self) -> bool {
        self.journal.redacting()
    }

    pub fn new(
        journal: Journal,
        model: Box<dyn Model + Send>,
        workspace: PathBuf,
        limits: Limits,
    ) -> Agent {
        let policy = Policy::new(Policy::defaults(&workspace), &workspace);
        // 事件缓冲有上限，轮次必须来自独立计数
        let turn = journal.turn_count();
        // watcher 要在 workspace 被 move 之前建好
        let observe_on = std::env::var("UI_OBSERVE").map(|v| v != "0").unwrap_or(true);
        // UI_OBSERVE_SCAN=1 强制走全量扫描（对照 benchmark 用，也是逃生口：
        // 某些网络/overlay 文件系统上 inotify 事件不可靠）
        let force_scan = std::env::var("UI_OBSERVE_SCAN").map(|v| v == "1").unwrap_or(false);
        let watcher = if observe_on && !force_scan {
            crate::observe::Watcher::new(&workspace)
        } else {
            None
        };
        // 记忆是 workspace 级的：换会话不换记忆（设计稿 §3.1）。
        // UI_MEMORY=0 关闭读取；UI_MEMORY_BUDGET 覆盖单轮 300 tokens。
        let memory = crate::memory::Store::open(&workspace);
        let memory_enabled = std::env::var("UI_MEMORY").map(|v| v != "0").unwrap_or(true);
        let memory_tokens = std::env::var("UI_MEMORY_BUDGET")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(crate::memory::DEFAULT_MAX_TOKENS);
        let cancel = Cancel::new();
        Agent {
            journal,
            model,
            policy,
            budget: Budget::new(limits),
            tools: registry(),
            workspace,
            goal: None,
            max_steps: limits.max_steps,
            max_repairs: 2,
            compact: CompactPolicy::default(),
            cancel: cancel.clone(),
            active_cancel: cancel,
            history: Vec::new(),
            loaded_skills: Vec::new(),
            tool_chars: 2000,
            auto_checkpoint: std::env::var("UI_AUTO_CHECKPOINT")
                .map(|v| v != "0")
                .unwrap_or(true),
            memory,
            memory_enabled,
            memory_tokens,
            memory_hits: Vec::new(),
            memory_query: None,
            mode: crate::mode::Mode::default(),
            redact_off_noted: false,
            env_scrub: true,
            // 观测式效果默认开：大仓库上可以 UI_OBSERVE=0 关掉（代价是审计看不到实际改动）
            env_observe: std::env::var("UI_OBSERVE")
                .map(|v| v != "0")
                .unwrap_or(true),
            // 优先用 inotify：每次调用只 drain，不再全量扫描。
            // 建不起来（平台不支持 / watch 超限）就退回扫描。
            watcher,
            sandbox: crate::sandbox::Config {
                fs: std::env::var("UI_SANDBOX").map(|v| v != "0").unwrap_or(true),
                net: std::env::var("UI_SANDBOX_NET").map(|v| v == "1").unwrap_or(false),
                extra_writable: Vec::new(),
            },
            plugins: None,
            mcp: None,
            tool_seq: 0,
            spill_pruned_at: 0,
            last_model_estimated: 0,
            turn,
            approval_seq: 0,
        }
    }

    /// 目标路径的**真实**位置是否跑出工作区（符号链接、挂载点）。
    /// 词法检查（`..`）在 `Ctx::resolve` 里做，这一层补的是"字面在里、实际在外"。
    fn escapes_workspace(&self, target: &str) -> bool {
        let path = Path::new(target);
        let real = match std::fs::canonicalize(path) {
            Ok(p) => p,
            Err(_) => {
                // 写新文件时目标还不存在：找到最近的存在祖先，再拼回剩下的部分
                let mut probe = path.parent();
                loop {
                    match probe {
                        Some(p) if p.as_os_str().is_empty() => return false,
                        Some(p) => match std::fs::canonicalize(p) {
                            Ok(real) => {
                                break real.join(path.file_name().unwrap_or_default())
                            }
                            Err(_) => probe = p.parent(),
                        },
                        None => return false,
                    }
                }
            }
        };
        let ws =
            std::fs::canonicalize(&self.workspace).unwrap_or_else(|_| self.workspace.clone());
        !real.starts_with(&ws)
    }

    fn ctx(&self) -> Ctx {
        Ctx {
            workspace: self.workspace.clone(),
            cancel: self.active_cancel.clone(),
            mcp: self.mcp.clone(),
            env_scrub: self.env_scrub,
            plugins: self.plugins.clone(),
            env_observe: self.env_observe,
            sandbox: self.sandbox.clone(),
        }
    }

    pub fn turn_number(&self) -> u64 {
        self.turn
    }

    /// 落账那道门的 agent 侧包装：只管两件事——
    /// ①「这一轮的日志是原文」这条记号要在第一段原文进盘之前补上；
    /// ② 事件写进 `Journal::append`，脱敏在那里发生（状态只有 Journal 那一份，
    /// 避免出现"agent 以为关了、日志还开着"这种两处开关互相漂移的 bug）。
    ///
    /// 新事件请走这里。例外只有 `journal_tool_output`：它自己按"模式脱敏 +
    /// 来源扣留"两档处理过正文，需要 `journal.append_raw`，见那里的注释。
    ///
    /// 它是 `pub` 的：`ui-cli/src/server.rs` 的 worker 线程也在落账（排队时被撤
    /// 掉的那条消息），走这里才能连带 `privacy/redact-off` 那条记号一起补上。
    pub fn log_event(&mut self, kind: &str, payload: Json) -> Result<(), AgentError> {
        self.note_redact_off();
        self.journal.append(kind, payload)?;
        Ok(())
    }

    /// 落盘一份原文（0600 + 内核命名），返回它的绝对路径。
    ///
    /// `withheld` 决定放哪一个目录：命中隐私清单的正文进 `.ui/tool-private/`
    /// （工具读不到，只有人能 `cat`），截断缓存进 `.ui/tool-output/`
    /// （模型可以用 `fs_read` 按需读回全文）。两个目录的隐私语义相反，
    /// 放一起就等于把"读例外"开给了本该被它挡住的那些内容。
    ///
    /// 写失败在这里不当成功用：返回 Err 让调用方把「指针是空的」如实写进日志，
    /// 否则账上会出现一条指向不存在文件的引用（`fs_read` 撞上时既难看又难查）。
    fn spill_original(
        &self,
        withheld: bool,
        tool: &str,
        content: &str,
    ) -> std::io::Result<std::path::PathBuf> {
        let rel = crate::spill::dir_with(withheld);
        let dir = self.workspace.join(rel);
        crate::spill::ensure_dir(&dir).map_err(|e| {
            // 相对名进日志与上下文：绝对路径里可能带着用户名、项目名，
            // 而 `file_error` 这类字段是会被人贴进 issue 的。
            std::io::Error::new(e.kind(), format!("建 {} 失败: {}", rel, e))
        })?;
        let path = dir.join(crate::spill::file_name(
            self.tool_seq,
            self.journal.writer(),
            self.journal.seq(),
            tool,
        ));
        crate::spill::write_private(&path, content)?;
        Ok(path)
    }

    /// 按保留策略清一遍落盘原文（见 `crate::spill`）。
    ///
    /// 「日志里没有」不等于「这台机器上没有」：扣留与截断写下的原文如果没有
    /// 生命周期，这个目录就是凭据的永久留底，而且能无限涨。所以在 run 边界
    /// 顺手清，并把清了多少写进日志——删东西不留账，比不删更难解释。
    /// 一个进程里最多 60 秒扫一次目录，长驻 serve 才不会被这个 IO 拖住。
    fn prune_spill(&mut self) {
        let now = crate::time::now_ms();
        if now.saturating_sub(self.spill_pruned_at) < SPILL_PRUNE_INTERVAL_MS {
            return;
        }
        self.spill_pruned_at = now;
        let policy = crate::spill::Policy::from_env();
        // 两个目录**共用一份预算**：策略问的是"这台机器上还留着几份原文"，
        // 分开算等于每个目录各领一份，200 个变 400 个。
        let dirs = crate::spill::dirs(&self.workspace);
        let broken: Vec<String> = dirs
            .iter()
            .filter(|d| crate::spill::unreadable(d))
            .map(|d| {
                d.file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect();
        if !broken.is_empty() {
            // 列不出来 ≠ 里面是空的，这时候报"已清理 0 个"是最难查的误导；先说，
            // 再照常清列得出来的那一个。只报目录名：绝对路径里可能带着用户名与
            // 项目名，而这一行会被人原样贴进 issue。
            let _ = self.log_event(
                "privacy/spill-error",
                Json::obj(vec![("dirs", Json::from(broken.join(", ").as_str()))]),
            );
        }
        let p = crate::spill::prune_all(&dirs, &policy, now);
        if p.removed == 0 && p.failed == 0 && !p.held {
            return;
        }
        let mut fields = vec![
            ("removed", Json::from(p.removed as u64)),
            ("freedBytes", Json::from(p.freed_bytes)),
            ("keptFiles", Json::from(p.kept.files as u64)),
            ("keptBytes", Json::from(p.kept.bytes)),
            ("policy", Json::from(policy.describe().as_str())),
        ];
        if p.held {
            fields.push(("held", Json::Bool(true)));
        }
        if p.failed > 0 {
            // 说了要删却没删掉的：留着，并且让下一次扫描再试一遍
            fields.push(("failed", Json::from(p.failed as u64)));
        }
        if p.skipped_foreign > 0 {
            fields.push(("skippedForeign", Json::from(p.skipped_foreign as u64)));
        }
        let _ = self.log_event("privacy/spill-pruned", Json::obj(fields));
    }

    /// 这次调用的**效果目标**里有没有隐私清单上的路径。
    ///
    /// 只看效果，不看参数原文：`fs_write` 的 content 是正文，一段讲到
    /// "secrets" 的散文不该被当成读到了私密内容（回归测试
    /// `source_judgement_reads_effects_not_prose` 钉着这一点）。而效果目标
    /// 是工具自述「这次会碰什么」：`fs.*` 给解析后的绝对路径，`shell` 给整条
    /// 命令串，`proc.net` 给 URL——口径刚好。
    ///
    /// 工作区前缀要先挖掉（`mentioned_in_under`）：工作区自己住在 `~/secrets/`
    /// 下面时，不能每次读写都算命中。
    ///
    /// 这一层是补读取侧的洞：`cat secrets/x` 在工具层挡不住（沙箱按设计只限写、
    /// 不限读，见 `sandbox.rs`），那就至少别让它的结果顺手抄进一份会外流的日志里。
    ///
    /// **豁免跟着工具层那唯一一条读例外走**。`.ui/tool-output/` 是模型本来就能
    /// `fs_read` 的地方，把"重读截断缓存"算成隐私来源有两个实测的坏处：每重读一次
    /// 就往 `.ui/tool-private/` 复制一份，以及那条"原文已按保留策略清理"的诊断从
    /// 日志里消失——replay 变空白，人反而查不到发生了什么。这个豁免成立的前提是
    /// 同一份正文不会既进扣留档又进可读档，那一条由 `retain_tool_output` 兜住。
    fn private_source(&self, call: &ToolCall) -> Option<String> {
        let tool = find(&self.tools, &call.name)?;
        let ctx = self.ctx();
        for effect in (tool.effects)(&call.args, &ctx) {
            if crate::protected::is_allowed_read(&self.workspace, Path::new(&effect.target)) {
                continue;
            }
            if let Some(hit) = crate::protected::mentioned_in_under(&self.workspace, &effect.target)
            {
                return Some(hit);
            }
        }
        None
    }

    /// 把「模型实际看到的工具输出」写进日志：`switch_journal` 重建历史时
    /// 才有东西可还原（评审 P1-1f）。
    ///
    /// 落账前过两道，顺序是有意的：
    /// 1. **模式脱敏**（`secrets::redact`）：不管来源是谁，长得像凭据的就换掉，
    ///    并在事件里留 `redacted: N`——看轨迹的人知道这里抹过几处；
    /// 2. **来源扣留**（命中隐私清单）：整段原文不进日志，只留字数、指纹和
    ///    指回 0600 落盘文件的路径。凭据正则救不了「人把清单加到一份普通
    ///    文本上」的情况，那一档只能靠"它从哪来"。
    ///
    /// 模型上下文里仍是原文（`kept` 由调用方推进 history）：它是在这次会话里
    /// 被批准去读这个东西的；要收口的是**会外流的那一份**。
    fn journal_tool_output(
        &mut self,
        call_id: &str,
        tool: &str,
        ok: bool,
        kept: &str,
        private: Option<String>,
    ) -> Result<(), AgentError> {
        self.note_redact_off();
        let mut fields: Vec<(&str, Json)> = vec![
            ("callId", Json::from(call_id)),
            ("tool", Json::from(tool)),
            ("ok", Json::Bool(ok)),
        ];
        let (text, hits) = if self.journal.redacting() {
            crate::secrets::redact(kept)
        } else {
            (kept.to_string(), 0)
        };
        if hits > 0 {
            fields.push(("redacted", Json::from(hits)));
        }
        match private {
            Some(declared) => {
                // 原文另存 0600，日志里只留能对齐、不能用的东西
                self.tool_seq += 1;
                let chars = kept.chars().count();
                fields.push(("text", Json::from("")));
                fields.push(("withheld", Json::from(declared.as_str())));
                fields.push(("chars", Json::from(chars)));
                fields.push(("sha256", Json::from(crate::sha256::hex(kept.as_bytes()).as_str())));
                match self.spill_original(true, tool, kept) {
                    Ok(path) => {
                        let rel = path
                            .strip_prefix(&self.workspace)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .replace('\\', "/");
                        fields.push(("file", Json::from(rel.as_str())));
                    }
                    // 落盘这一份没写成：那就没有"全文在 X"这回事，如实记下原因。
                    // 扣留本身照旧生效（日志里仍然没有正文），但读例外的那条路
                    // 不存在了——指针写错比不写更糟，因为后面每一步都会信它。
                    Err(e) => {
                        fields.push(("file_error", Json::from(e.to_string().as_str())));
                    }
                }
            }
            None => fields.push(("text", Json::from(text.as_str()))),
        }
        // 不走 `log_event`、也不给 `Journal::append` 那道门过：这一条的正文在
        // 上面已经按两档处理过了，`redacted` 计数指的是那里的一次抹除，
        // 再过一遍会把计数含义洗掉。
        self.journal.append_raw("tool/output", Json::obj(fields))?;
        Ok(())
    }

    /// 工具输出超过 `tool_chars` 时落盘，上下文里只留首尾 + 指针。
    ///
    /// 每个工具结果都会在后续每一步被重发一次，长输出是上下文成本的主要来源。
    /// 完整内容留在磁盘上，模型需要时可以 `fs_read` 重读——按需展开，而不是常驻。
    ///
    /// `withheld`（这次的来源在隐私清单上）时**不往模型可读的目录落那一份**：
    /// 截断缓存存在的意义是"内容还在，随时读回来"，而对被扣留的输出来说，
    /// 在 `.ui/tool-output/` 再写一份就是刚抹掉的原文换个地方原样躺好。全文只留
    /// `journal_tool_output` 写进 `.ui/tool-private/` 的那一份，上下文里照旧是
    /// 首尾两截——执行没被改，模型也确实拿到过输出。
    fn retain_tool_output(
        &mut self,
        tool: &str,
        full: &str,
        withheld: bool,
    ) -> Result<String, AgentError> {
        let total = full.chars().count();
        if self.tool_chars == 0 || total <= self.tool_chars {
            return Ok(full.to_string());
        }
        self.tool_seq += 1;
        // 名字里必须带 writer + 日志序号：`tool_seq` 是每个 run 归零的（见
        // `begin_run`），只用它会让后一个 run 覆盖前一个 run 的截断缓存——
        // 上一轮历史里那条「完整内容在 …」的指针就会读到别人的输出。
        // 命名规则本身在 `crate::spill::file_name`，清理那边按同一套口径认文件。
        let rel;
        let mut original: Option<String> = None;
        let spilled = if withheld {
            None
        } else {
            Some(self.spill_original(false, tool, full))
        };
        match spilled {
            None => {
                rel = "（全文按隐私清单扣留，不在模型可读的目录留副本）".to_string();
            }
            Some(Ok(path)) => {
                let rel_path = path
                    .strip_prefix(&self.workspace)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                rel = format!("；完整内容在 {}，需要时用 fs_read 读它", rel_path);
                original = Some(rel_path);
            }
            // 落盘失败：那句"完整内容在 X"就不能说。原文现在只活在这一轮的
            // 上下文里，历史上没有任何地方能把它读回来。
            Some(Err(e)) => {
                rel = format!("（未落盘：{}）", e);
                let _ = self.log_event(
                    "privacy/spill-error",
                    Json::obj(vec![
                        ("stage", Json::from("tool/trim")),
                        ("tool", Json::from(tool)),
                        ("error", Json::from(e.to_string().as_str())),
                    ]),
                );
            }
        }

        let head_n = self.tool_chars * 2 / 3;
        let tail_n = self.tool_chars / 3;
        let head: String = full.chars().take(head_n).collect();
        let tail: String = full.chars().skip(total - tail_n).collect();
        let kept = format!("{}\n…[输出共 {} 字符，已截断{}]\n{}", head, total, rel, tail);

        // `file` 字段是给 `spill::dangling` 看的：原文被保留策略清掉之后，
        // 账上这条指针就该被指出来，而不是留着让人去 fs_read 一份没了的文件。
        let mut fields = vec![
            ("tool", Json::from(tool)),
            ("totalChars", Json::from(total)),
            ("keptChars", Json::from(kept.chars().count())),
        ];
        match original {
            Some(p) => fields.push(("file", Json::from(p.as_str()))),
            None => fields.push(("fileError", Json::from(rel.as_str()))),
        }
        self.log_event("tool/trim", Json::obj(fields))?;
        Ok(kept)
    }

    /// 把 skill 正文装进上下文（工具加载与工作流预加载共用），并留痕。
    pub fn load_skill(&mut self, name: &str, body: &str, source: &str) -> Result<(), AgentError> {
        if self.loaded_skills.iter().any(|(n, _)| n == name) {
            return Ok(());
        }
        self.loaded_skills.push((name.to_string(), body.to_string()));
        self.log_event(
            "skill/load",
            Json::obj(vec![
                ("name", Json::from(name)),
                ("bytes", Json::from(body.len())),
                ("source", Json::from(source)),
            ]),
        )?;
        Ok(())
    }

    /// 跑一轮。**不重置任何东西**——预算、工具序号、会话审批规则、取消令牌
    /// 都属于 run 边界，由调用方在开跑前 `begin_run` / `begin_run_with_cancel`
    /// 划出来。
    ///
    /// 这一条是刻意的：以前每轮开头顺手 `cancel.reset()`，长驻 worker 里
    /// 「刚在队列中被取消的任务」一开跑就复活了；预算同理，上一任务烧掉的
    /// token 会串进下一任务的账单。run 边界由持有取消令牌的人划，才不会串。
    /// 可能改动 workspace 的工具。自动 checkpoint 只在这些调用前触发；
    /// 读/搜索/skill 等不应为每个 turn 付全量快照成本。
    fn may_write_workspace(tool: &str) -> bool {
        matches!(
            tool,
            "fs_write"
                | "fs_edit"
                | "patch"
                | "fs_mv"
                | "fs_delete"
                | "shell"
                | "plugin_call"
                | "mcp_call"
                | "todo"
        )
    }

    pub fn run_turn(&mut self, input: &str, hooks: &mut dyn Hooks) -> Result<TurnReport, AgentError> {
        // 每轮只读一次：先 refresh（坏链就在这里 fail closed，连 turn/start 都不写），
        // 再用「本轮输入 + goal + 最近两条 user」检索一次。工具结果不触发隐式重检索。
        if self.memory_enabled {
            self.memory.refresh().map_err(AgentError::Memory)?;
        }
        let query_input = self.memory_query.take().unwrap_or_else(|| input.to_string());
        let memory_query =
            crate::memory::build_query(&query_input, self.goal.as_deref(), &self.history);
        self.memory_hits = if self.memory_enabled && self.memory_tokens > 0 {
            self.memory.retrieve(
                &memory_query,
                &crate::memory::Config {
                    top_k: crate::memory::DEFAULT_TOP_K,
                    max_tokens: self.memory_tokens,
                    now_ms: crate::time::now_ms(),
                },
            )
        } else {
            Vec::new()
        };
        let memory_text = crate::memory::render(&self.memory_hits);

        self.turn += 1;
        let turn = self.turn;
        self.log_event(
            "turn/start",
            Json::obj(vec![("turn", Json::from(turn)), ("mode", Json::from(self.mode.name())), ("prompt", Json::from(input))]),
        )?;

        self.history.push(Message::user(input));
        let mut steps = 0usize;
        let mut tool_calls = 0usize;
        let mut last_text = String::new();
        let mut empty_replies = 0usize;
        let mut memory_logged = false;
        let mut checkpointed = false;

        loop {
            if self.active_cancel.is_cancelled() {
                return self.finish(turn, "cancelled", steps, tool_calls, last_text);
            }
            if steps >= self.max_steps {
                return self.finish(turn, "step_budget", steps, tool_calls, last_text);
            }
            if let Err(e) = self.budget.assert_room(Dim::WallMs, 0.0) {
                self.journal
                    .append("turn/end", Json::obj(vec![("turn", Json::from(turn)), ("reason", Json::from("budget")), ("error", Json::from(e.to_string()))]))?;
                return Err(AgentError::Budget(e));
            }

            // 历史超阈值就折叠一次，并且记账：前端能看见「折叠了多少」。
            if let Some(out) = compact(&mut self.history, &self.compact) {
                self.log_event(
                    "compact",
                    Json::obj(vec![
                        ("beforeTokens", Json::from(out.before_tokens)),
                        ("afterTokens", Json::from(out.after_tokens)),
                        ("dropped", Json::from(out.dropped)),
                        ("digest", Json::from(out.digest.as_str())),
                    ]),
                )?;
                hooks.on_compact(out.before_tokens, out.after_tokens, out.dropped);
            }

            // skill 目录每步重扫：文件可能刚被写出来，且只有几个小文件
            let catalog = crate::skill::Catalog::discover(&self.workspace);
            let catalog_text = catalog.render_catalog();
            let snapshot = build_snapshot(
                &self.workspace,
                &self.tools,
                &self.history,
                self.goal.as_deref(),
                None,
                &catalog_text,
                &self.loaded_skills,
                &memory_text,
            );
            if let Err(e) = self.budget.charge(Dim::Tokens, snapshot.tokens as f64) {
                self.log_event(
                    "turn/end",
                    Json::obj(vec![
                        ("turn", Json::from(turn)),
                        ("reason", Json::from("budget")),
                        ("error", Json::from(e.to_string())),
                    ]),
                )?;
                return Err(AgentError::Budget(e));
            }
            self.log_event("prompt/snapshot", snapshot.to_json())?;
            hooks.on_snapshot(&snapshot);
            hooks.on_budget(
                self.budget.used(Dim::Tokens) as u64,
                self.budget.used(Dim::ToolCalls) as u64,
            );

            self.last_model_estimated = snapshot.tokens as u64;
            let mut response = match self.call_model(&snapshot, hooks) {
                Ok(r) => r,
                Err(AgentError::Model(e)) if self.active_cancel.is_cancelled() => {
                    self.log_event(
                        "turn/end",
                        Json::obj(vec![
                            ("turn", Json::from(turn)),
                            ("reason", Json::from("cancelled")),
                            ("error", Json::from(e)),
                        ]),
                    )?;
                    return Ok(TurnReport {
                        reason: "cancelled".to_string(),
                        steps,
                        tool_calls,
                        tokens: self
                            .budget
                            .snapshot()
                            .get("used")
                            .and_then(|u| u.u64_at("tokens"))
                            .unwrap_or(0),
                        final_text: "(已取消)".to_string(),
                    });
                }
                Err(AgentError::Budget(e)) => {
                    self.log_event(
                        "turn/end",
                        Json::obj(vec![
                            ("turn", Json::from(turn)),
                            ("reason", Json::from("budget")),
                            ("error", Json::from(e.to_string())),
                        ]),
                    )?;
                    return Err(AgentError::Budget(e));
                }
                Err(e) => {
                    // 不允许 turn/start 悬挂：模型错误也要写 turn/end，
                    // 否则 verify/doctor 会把这一轮当成崩溃未完成。
                    self.log_event(
                        "turn/end",
                        Json::obj(vec![
                            ("turn", Json::from(turn)),
                            ("reason", Json::from("model_error")),
                            ("error", Json::from(e.to_string())),
                        ]),
                    )?;
                    return Err(e);
                }
            };
            // 记忆命中只有在模型真的收到这一份快照后才记账。之前放在
            // call_model 之前：取消/模型失败也会写 memory/used，命中计数
            // 会奖励一份模型从未读到的上下文。
            if !memory_logged && !self.memory_hits.is_empty() {
                let used: Vec<(String, String, u64)> = self
                    .memory_hits
                    .iter()
                    .map(|hit| {
                        (
                            hit.entry.id.clone(),
                            hit.reason.clone(),
                            crate::memory::budget_tokens(&hit.entry.text),
                        )
                    })
                    .collect();
                for (id, reason, tokens) in used {
                    let payload = crate::memory::used_payload(&id, &reason, tokens);
                    self.log_event("memory/used", payload.clone())?;
                    hooks.on_event(&Json::obj(vec![
                        ("kind", Json::from("memory/used")),
                        ("payload", payload),
                    ]));
                    self.memory
                        .record_used(&id, &reason, tokens)
                        .map_err(AgentError::Memory)?;
                }
                memory_logged = true;
            }

            if self.active_cancel.is_cancelled() {
                return self.finish(turn, "cancelled", steps, tool_calls, last_text);
            }

            // 模型给出的 id 只在本条回复内有意义。内核必须把它升级成
            // 全局唯一 id，否则 resume、审批回执和 viewer 都会串到别的调用上。
            self.canonicalize_call_ids(&mut response);

            self.log_event(
                "model/response",
                Json::obj(vec![
                    ("model", Json::from(self.model.name())),
                    ("content", Json::from(response.content.as_str())),
                    (
                        "toolCalls",
                        Json::Arr(
                            response
                                .tool_calls
                                .iter()
                                .map(|c| {
                                    Json::obj(vec![
                                        ("name", Json::from(c.name.as_str())),
                                        ("args", c.args.clone()),
                                        ("parseError", c.parse_error.clone().map(Json::from).unwrap_or(Json::Null)),
                                    ])
                                })
                                .collect(),
                        ),
                    ),
                ]),
            )?;

            if let Some(usage) = response.usage_tokens {
                self.budget
                    .reconcile_tokens(self.last_model_estimated, usage);
                let rate = self.budget.limits.usd_per_mtok;
                if rate > 0.0 {
                    let cost = usage as f64 * rate / 1_000_000.0;
                    if let Err(e) = self.budget.charge(Dim::Usd, cost) {
                        self.log_event(
                            "turn/end",
                            Json::obj(vec![
                                ("turn", Json::from(turn)),
                                ("reason", Json::from("budget")),
                                ("error", Json::from(e.to_string())),
                            ]),
                        )?;
                        return Err(AgentError::Budget(e));
                    }
                }
            }
            if !response.content.is_empty() {
                last_text = response.content.clone();
                // 跨轮历史：模型说的话也要记住，否则下一轮它不知道刚才说过什么
                self.history.push(Message::assistant(response.content.clone()));
                hooks.on_assistant(&response.content);
            }
            if response.tool_calls.is_empty() && response.content.trim().is_empty() {
                // 空回复不是完成。静默结束会让"没做完"看起来像"做完了"。
                empty_replies += 1;
                self.log_event(
                    "model/empty",
                    Json::obj(vec![
                        ("turn", Json::from(turn)),
                        ("attempt", Json::from(empty_replies)),
                    ]),
                )?;
                if empty_replies >= 2 {
                    return self.finish(turn, "empty_reply", steps, tool_calls, last_text);
                }
                self.history.push(Message::note(
                    "你上一条回复是空的。请继续未完成的工作；如果已经完成，用一句话给出结论。",
                ));
                continue;
            }
            empty_replies = 0;

            if response.tool_calls.is_empty() {
                return self.finish(turn, "done", steps, tool_calls, last_text);
            }

            for call in &response.tool_calls {
                if self.active_cancel.is_cancelled() {
                    return self.finish(turn, "cancelled", steps, tool_calls, last_text);
                }
                // 写操作前留基线：没有它，review/revert 只能靠用户记得先手动
                // checkpoint。只做一次；失败要显式留一条 checkpoint/error，
                // 不能假装自动基线已经存在。
                if self.auto_checkpoint
                    && !checkpointed
                    && Self::may_write_workspace(&call.name)
                {
                    checkpointed = true;
                    match crate::checkpoint::create_limited(
                        &self.workspace,
                        &format!("auto-before-turn-{}", turn),
                    ) {
                        Ok(cp) => {
                            let mut payload = cp;
                            payload.set("tool", Json::from(call.name.as_str()));
                            self.log_event("checkpoint/auto", payload)?;
                        }
                        Err(e) => {
                            let _ = self.log_event(
                                "checkpoint/error",
                                Json::obj(vec![
                                    ("tool", Json::from(call.name.as_str())),
                                    ("error", Json::from(e)),
                                ]),
                            );
                        }
                    }
                }
                let (result, name) = match self.dispatch(call, hooks) {
                    Ok(v) => v,
                    Err(AgentError::Budget(e)) => {
                        self.log_event(
                            "turn/end",
                            Json::obj(vec![
                                ("turn", Json::from(turn)),
                                ("reason", Json::from("budget")),
                                ("error", Json::from(e.to_string())),
                            ]),
                        )?;
                        return Err(AgentError::Budget(e));
                    }
                    Err(e) => return Err(e),
                };
                tool_calls += 1;
                hooks.on_tool(&name, &call.args, &result);

                // skill 正文进上下文这件事必须留痕：谁、多少字节、什么时候
                if name == "skill" && result.ok {
                    if let Some(skill_name) = call.args.str_at("name") {
                        self.load_skill(skill_name, &result.content, "tool")?;
                    }
                }

                // 工具输出不全量常驻上下文：太长就落盘，上下文里只留首尾 + 指针。
                // 每个工具结果会在后面每一步被重发一次，这里是成本的主要来源。
                let full = result.error.as_deref().unwrap_or(&result.content);
                // 来源判定要用**原始调用**算：kept 已经不含参数，而"这次读的是
                // 不是隐私路径"只由参数决定。它还得赶在截断落盘前面——那一份副本
                // 放进哪个目录，取决于这次是不是按清单扣留。
                let private = self.private_source(call);
                let kept = self.retain_tool_output(&name, full, private.is_some())?;
                // 模型看到的仍是原文，落账的那一份先脱敏/扣留（见 journal_tool_output）
                self.journal_tool_output(&call.id, &name, result.ok, &kept, private)?;
                self.history.push(Message::tool(&name, result.ok, &kept));
            }

            steps += 1;
        }
    }

    fn canonicalize_call_ids(&self, response: &mut crate::model::ModelResponse) {
        let anchor = self.journal.seq();
        let writer = self.journal.writer();
        for (index, call) in response.tool_calls.iter_mut().enumerate() {
            call.id = format!("{}-{}-{}", writer, anchor, index + 1);
        }
    }

    /// 把一次逻辑 model 调用（可能包含 repair 重试）的 usage 和估算合并。
    fn finish_model_response(
        &mut self,
        mut response: crate::model::ModelResponse,
        total_usage: u64,
        usage_seen: bool,
        estimated: u64,
    ) -> crate::model::ModelResponse {
        if usage_seen {
            response.usage_tokens = Some(total_usage);
        }
        self.last_model_estimated = estimated;
        response
    }

    fn call_model(
        &mut self,
        snapshot: &Snapshot,
        hooks: &mut dyn Hooks,
    ) -> Result<crate::model::ModelResponse, AgentError> {
        let mut repair_note: Option<String> = None;
        let mut attempts = 0usize;
        let mut total_usage = 0u64;
        let mut usage_seen = false;
        let mut estimated = snapshot.tokens as u64;
        loop {
            let response = match self.model.respond_stream(
                snapshot,
                repair_note.as_deref(),
                &self.active_cancel,
                &mut |delta| hooks.on_delta(delta),
            ) {
                Ok(r) => r,
                Err(e) => {
                    let _ = self
                        .journal
                        .append("model/error", Json::obj(vec![("error", Json::from(e.as_str()))]));
                    return Err(AgentError::Model(e));
                }
            };

            if let Some(usage) = response.usage_tokens {
                total_usage = total_usage.saturating_add(usage);
                usage_seen = true;
            }

            let broken: Vec<&ToolCall> = response
                .tool_calls
                .iter()
                .filter(|c| c.parse_error.is_some())
                .collect();

            // 适配器发现「整条回复不是协议 JSON」时，和参数坏掉同等对待：
            // 先要求重写，而不是把散文当成最终答案。
            if let Some(err) = response.protocol_error.clone() {
                attempts += 1;
                self.log_event(
                    "model/repair",
                    Json::obj(vec![
                        ("attempt", Json::from(attempts)),
                        ("tool", Json::from("(protocol)")),
                        ("error", Json::from(err.as_str())),
                        ("raw", Json::from(response.content.as_str())),
                        ("usage", response.usage_tokens.map(Json::from).unwrap_or(Json::Null)),
                        ("estimatedTokens", Json::from(estimated)),
                    ]),
                )?;
                if attempts > self.max_repairs {
                    return Err(AgentError::Model(format!(
                        "协议错误重试次数用尽：{}",
                        err
                    )));
                }
                let note = format!(
                    "你上一条输出不是约定的 JSON 对象（{}）。请只输出一个 JSON 对象：\
                     {{\"content\": \"给用户看的话\", \"toolCalls\": [{{\"name\": \"工具名\", \"args\": {{}}}}]}}。",
                    err
                );
                let extra = snapshot.tokens as u64 + crate::budget::estimate_tokens(&note);
                if let Err(e) = self.budget.charge(Dim::Tokens, extra as f64) {
                    return Err(AgentError::Budget(e));
                }
                estimated = estimated.saturating_add(extra);
                self.history.push(Message::note(note.clone()));
                repair_note = Some(note);
                continue;
            }

            if broken.is_empty() {
                return Ok(self.finish_model_response(
                    response,
                    total_usage,
                    usage_seen,
                    estimated,
                ));
            }

            let error = broken[0].parse_error.clone().unwrap_or_default();
            attempts += 1;
            self.log_event(
                "model/repair",
                Json::obj(vec![
                    ("attempt", Json::from(attempts)),
                    ("tool", Json::from(broken[0].name.as_str())),
                    ("error", Json::from(error.as_str())),
                    ("raw", Json::from(broken[0].raw.clone().unwrap_or_default())),
                    ("usage", response.usage_tokens.map(Json::from).unwrap_or(Json::Null)),
                    ("estimatedTokens", Json::from(estimated)),
                ]),
            )?;

            if attempts > self.max_repairs {
                // 不能让“永远输出坏 JSON”的模型以一条散文回复结束并 exit 0。
                return Err(AgentError::Model(format!(
                    "工具参数 JSON 修复次数用尽：{}",
                    error
                )));
            }
            let note = format!(
                "上一个工具调用的参数不是合法 JSON（{}）。请重新输出完整、合法的 JSON 参数，不要截断。",
                error
            );
            let extra = snapshot.tokens as u64 + crate::budget::estimate_tokens(&note);
            if let Err(e) = self.budget.charge(Dim::Tokens, extra as f64) {
                return Err(AgentError::Budget(e));
            }
            estimated = estimated.saturating_add(extra);
            self.history.push(Message::note(note.clone()));
            repair_note = Some(note);
        }
    }

    /// 发起一次可寻址、可记账的人工审批。
    ///
    /// 审批不是「模型问完就过去了」的一条提示：每个 ask 都有自己的 id，
    /// 回答时必须同时写清决定与授权范围。`Once` 不留下会话规则，`Session` 才留下。
    fn request_approval(
        &mut self,
        hooks: &mut dyn Hooks,
        context: &str,
        call_id: &str,
        tool: &str,
        args: &Json,
        effects: &[Effect],
        reason: &str,
    ) -> Result<ApprovalResponse, AgentError> {
        if self.active_cancel.is_cancelled() {
            return Ok(ApprovalResponse::deny().via("cancelled"));
        }
        self.approval_seq += 1;
        let approval_id = format!(
            "ap-{}-{}-{}",
            self.journal.writer(),
            self.journal.seq(),
            self.approval_seq
        );
        let request = ApprovalRequest {
            approval_id: approval_id.clone(),
            call_id: call_id.to_string(),
            tool: tool.to_string(),
            args: args.clone(),
            effects: effects.to_vec(),
            reason: reason.to_string(),
        };
        self.log_event(
            "approval/requested",
            Json::obj(vec![
                ("approvalId", Json::from(approval_id.as_str())),
                ("callId", Json::from(call_id)),
                ("tool", Json::from(tool)),
                ("context", Json::from(context)),
                (
                    "effects",
                    Json::Arr(effects.iter().map(Effect::to_json).collect()),
                ),
                ("reason", Json::from(reason)),
            ]),
        )?;
        let answer = hooks.on_approval(&request);
        let mut record = vec![
            ("approvalId", Json::from(approval_id.as_str())),
            ("callId", Json::from(call_id)),
            ("tool", Json::from(tool)),
            ("verdict", Json::from(answer.decision.name())),
            ("scope", Json::from(answer.scope.name())),
            ("context", Json::from(context)),
        ];
        // 决定是谁做的：人、超时、取消、自动批准、还是没有通道。缺了它，
        // `verdict=deny` 就同时表示"不同意"和"没人来得及同意"。
        if let Some(via) = answer.via {
            record.push(("via", Json::from(via)));
        }
        self.log_event("approval/answered", Json::obj(record))?;
        Ok(answer)
    }

    fn dispatch(
        &mut self,
        call: &ToolCall,
        hooks: &mut dyn Hooks,
    ) -> Result<(ToolResult, String), AgentError> {
        let name = call.name.clone();

        if let Some(err) = &call.parse_error {
            let result = ToolResult::err(format!("BAD_JSON: {}", err));
            self.log_event(
                "tool/end",
                Json::obj(vec![
                    ("callId", Json::from(call.id.as_str())),
                    ("tool", Json::from(name.as_str())),
                    ("ok", Json::Bool(false)),
                    ("error", Json::from("BAD_JSON")),
                ]),
            )?;
            return Ok((result, name));
        }

        // 只拷出执行需要的三样（两个函数指针 + 一个布尔），不把 &Tool 借过来：
        // 下面每一处写日志、走审批都要 &mut self，而工具表就挂在 self 上。
        let (effects_fn, run_fn, idempotent) = match find(&self.tools, &name) {
            Some(t) => (t.effects, t.run, t.idempotent),
            None => {
                let result = ToolResult::err(format!("未知工具: {}", name));
                self.log_event(
                    "tool/end",
                    Json::obj(vec![
                        ("callId", Json::from(call.id.as_str())),
                        ("tool", Json::from(name.as_str())),
                        ("ok", Json::Bool(false)),
                        ("error", Json::from("UNKNOWN_TOOL")),
                    ]),
                )?;
                return Ok((result, name));
            }
        };

        let ctx = self.ctx();
        let effects = effects_fn(&call.args, &ctx);
        // 词法上在 workspace 里，不代表真实位置也在：符号链接可以指向外面。
        // 策略只看字面路径，所以在这层补一道「真实路径」检查。
        // 模式只调严格度，不改日志格式：所以 quiet 跑出来的这一轮，
        // 事后可以用 strict 重判（见 `ui audit --as`）。
        let mut verdict = self.policy.decide(&effects);
        let base = verdict.decision;
        // 模式只调「策略没有明确表态」的部分：本次会话里刚批准过的效果不能再被
        // strict 抬回 Ask——抬回去等于「本轮都允许」那颗按钮按下去没反应，
        // 而屏幕上已经写过「已批准」。
        let adjusted = effects
            .iter()
            .map(|e| {
                let own = verdict
                    .reasons
                    .iter()
                    .find(|(effect, _, _)| effect == e)
                    .map(|(_, d, _)| *d)
                    .unwrap_or(base);
                if own == Decision::Allow && self.policy.session_approved(e) {
                    Decision::Allow
                } else {
                    self.mode.adjust(&e.kind, own)
                }
            })
            .fold(Decision::Allow, |acc, d| {
                if d.severity() > acc.severity() {
                    d
                } else {
                    acc
                }
            });
        if adjusted != base {
            let reason = format!(
                "模式 {} 把 {} 调整为 {}（原策略：{}）",
                self.mode.name(),
                base.name(),
                adjusted.name(),
                verdict.reason
            );
            verdict = Verdict {
                decision: adjusted,
                reasons: verdict.reasons,
                reason,
            };
        }
        if verdict.decision != Decision::Deny {
            // Ask 也必须做真实路径检查：Strict 会把 Allow 抬成 Ask，如果只检查
            // Allow，strict 反而成了绕过 symlink 越界检查的模式。
            // 一个工具可能声明多个 fs.* 效果（fs_mv 的 from / to 就是两个）。
            // 只检查第一个等于默认相信第二个：`to` 经符号链接指向工作区外时
            // 会被 `escapes_workspace` 漏掉，而 `run_mv` 自己的字面检查看不出来。
            let escapes: Vec<(Effect, Decision, String)> = effects
                .iter()
                .filter(|e| e.kind.starts_with("fs."))
                .filter(|e| self.escapes_workspace(&e.target))
                .map(|e| {
                    let reason = format!(
                        "{} 解析后的真实路径在工作区之外（符号链接）",
                        e.target
                    );
                    (e.clone(), Decision::Deny, reason)
                })
                .collect();
            if !escapes.is_empty() {
                let reason = if escapes.len() == 1 {
                    escapes[0].2.clone()
                } else {
                    format!(
                        "{} 个文件效果解析后的真实路径在工作区之外（符号链接）：{}",
                        escapes.len(),
                        escapes
                            .iter()
                            .map(|(e, _, _)| e.target.as_str())
                            .collect::<Vec<_>>()
                            .join("、")
                    )
                };
                verdict = Verdict {
                    decision: Decision::Deny,
                    reasons: escapes,
                    reason,
                };
            }
        }
        self.log_event(
            "tool/request",
            Json::obj(vec![
                ("callId", Json::from(call.id.as_str())),
                ("tool", Json::from(name.as_str())),
                ("args", call.args.clone()),
                (
                    "effects",
                    Json::Arr(effects.iter().map(Effect::to_json).collect()),
                ),
                ("decision", Json::from(verdict.decision.name())),
                ("reason", Json::from(verdict.reason.as_str())),
            ]),
        )?;
        hooks.on_decision(&call.id, &name, verdict.decision, &verdict.reason);

        let mut allowed = verdict.decision != Decision::Deny;
        if verdict.decision == Decision::Deny {
            // 拒绝时必须告诉模型「它请求的是什么」，否则它只会换个说法反复撞墙。
            let targets = effects
                .iter()
                .map(|e| e.describe())
                .collect::<Vec<_>>()
                .join("；");
            let result = ToolResult::err(format!(
                "策略拒绝（{}）：{}。工作区是 {}，路径要么相对它，要么落在它里面。",
                verdict.reason,
                targets,
                self.workspace.display()
            ));
            self.log_event(
                "tool/end",
                Json::obj(vec![
                    ("callId", Json::from(call.id.as_str())),
                    ("tool", Json::from(name.as_str())),
                    ("ok", Json::Bool(false)),
                    ("error", Json::from("DENIED")),
                ]),
            )?;
            return Ok((result, name));
        }

        if verdict.decision == Decision::Ask {
            let answer = self.request_approval(
                hooks,
                "dispatch",
                &call.id,
                &name,
                &call.args,
                &effects,
                &verdict.reason,
            )?;
            if self.active_cancel.is_cancelled() {
                let result = ToolResult::err("调用已取消");
                self.log_event(
                    "tool/end",
                    Json::obj(vec![
                        ("callId", Json::from(call.id.as_str())),
                        ("tool", Json::from(name.as_str())),
                        ("ok", Json::Bool(false)),
                        ("error", Json::from("CANCELLED")),
                    ]),
                )?;
                return Ok((result, name));
            }
            match (answer.decision, answer.scope) {
                (Decision::Allow, ApprovalScope::Session) => {
                    // 只有明确选择「本次会话允许」才产生会话规则。
                    for effect in &effects {
                        let rule = Rule::new(&effect.kind, &effect.target, Decision::Allow)
                            .with_note("本次会话中批准");
                        self.policy.add_rule(rule);
                    }
                    allowed = true;
                }
                (Decision::Allow, ApprovalScope::Once) => {
                    // `y` 只批准眼前这一次；不得把它放大成后续同类调用的通行证。
                    allowed = true;
                }
                (Decision::Ask, _) => {
                    // The UI did not decide (headless): fail closed, but say why.
                    let result = ToolResult::err("需要人工批准，但当前没有可用的审批通道");
                    self.log_event(
                        "tool/end",
                        Json::obj(vec![
                            ("callId", Json::from(call.id.as_str())),
                            ("tool", Json::from(name.as_str())),
                            ("ok", Json::Bool(false)),
                            ("error", Json::from("APPROVAL_UNAVAILABLE")),
                        ]),
                    )?;
                    return Ok((result, name));
                }
                (Decision::Deny, _) => allowed = false,
            }
        }

        if !allowed {
            let result = ToolResult::err("用户拒绝了这次调用");
            self.log_event(
                "tool/end",
                Json::obj(vec![
                    ("callId", Json::from(call.id.as_str())),
                    ("tool", Json::from(name.as_str())),
                    ("ok", Json::Bool(false)),
                    ("error", Json::from("REJECTED")),
                ]),
            )?;
            return Ok((result, name));
        }

        if let Err(e) = self.budget.charge(Dim::ToolCalls, 1.0) {
            self.log_event(
                "tool/end",
                Json::obj(vec![
                    ("callId", Json::from(call.id.as_str())),
                    ("tool", Json::from(name.as_str())),
                    ("ok", Json::Bool(false)),
                    ("error", Json::from("BUDGET")),
                ]),
            )?;
            // 不能用 Ok(tool_error) 混过去：那会让模型继续转，最后以 "done"
            // 和 exit 0 结束。预算失败是这一轮失败。
            return Err(AgentError::Budget(e));
        }

        self.log_event(
            "tool/start",
            Json::obj(vec![
                ("callId", Json::from(call.id.as_str())),
                ("tool", Json::from(name.as_str())),
                ("args", call.args.clone()),
                ("idempotent", Json::Bool(idempotent)),
            ]),
        )?;
        // 输入血缘：读文件也要留指纹。否则「结果可复现」只做了一半——
        // 数据换了版本，产物哈希却还是对的。
        record_reads(&mut self.journal, &self.workspace, &name, &call.id, &effects)?;

        // 观测式效果：这三个通道是"信任边界"——它们声明的是「要不要执行」，
        // 内部到底读了什么、写了什么，声明里没有。执行前后取差量，
        // 把**实际改动**记进日志，审计才回答得了"到底改了什么"。
        //
        // 两条路径：有 inotify watcher 就 drain 事件（O(1)，不扫描）；
        // 否则退回全量扫描比差（O(文件数)）。
        let trust_boundary = matches!(name.as_str(), "shell" | "plugin_call" | "mcp_call");
        let watching = self.env_observe && trust_boundary && self.watcher.is_some();
        if watching {
            // 先清掉历史事件，剩下的就都是这次工具调用产生的
            if let Some(w) = self.watcher.as_mut() {
                let _ = w.drain();
            }
        }
        let before = if self.env_observe && trust_boundary && !watching {
            Some(crate::observe::scan(&self.workspace))
        } else {
            None
        };

        let result = run_fn(&call.args, &ctx);
        hooks.on_budget(
            self.budget.used(Dim::Tokens) as u64,
            self.budget.used(Dim::ToolCalls) as u64,
        );
        self.log_event(
            "tool/end",
            Json::obj(vec![
                ("callId", Json::from(call.id.as_str())),
                ("tool", Json::from(name.as_str())),
                ("ok", Json::Bool(result.ok)),
                (
                    "error",
                    result.error.clone().map(Json::from).unwrap_or(Json::Null),
                ),
                ("meta", result.meta.clone()),
            ]),
        )?;

        if self.env_observe && trust_boundary {
            // 有 watcher：事件驱动；溢出/不可信时如实标注，不假装完整
            let observed = if watching {
                match self.watcher.as_mut().and_then(|w| w.drain()) {
                    Some(ch) => Some((ch, false)),
                    None => Some((crate::observe::Changes::default(), true)),
                }
            } else {
                before.map(|b| {
                    let after = crate::observe::scan(&self.workspace);
                    (crate::observe::diff(&b, &after), false)
                })
            };

            if let Some((ch, overflowed)) = observed {
                // 没变就不记：日志里塞满「什么都没发生」会淹没真正的改动
                if !ch.is_empty() || ch.truncated || overflowed {
                    let payload = Json::obj(vec![
                        ("callId", Json::from(call.id.as_str())),
                        ("tool", Json::from(name.as_str())),
                        ("summary", Json::from(ch.summary())),
                        ("changes", ch.to_json()),
                        ("overflowed", Json::Bool(overflowed)),
                        ("source", Json::from(if watching { "inotify" } else { "scan" })),
                    ]);
                    self.log_event("effect/observed", payload.clone())?;
                    hooks.on_event(&Json::obj(vec![
                        ("kind", Json::from("effect/observed")),
                        ("payload", payload),
                    ]));
                }
            }
        }
        Ok((result, name))
    }

    fn finish(
        &mut self,
        turn: u64,
        reason: &str,
        steps: usize,
        tool_calls: usize,
        text: String,
    ) -> Result<TurnReport, AgentError> {
        self.log_event(
            "turn/end",
            Json::obj(vec![
                ("turn", Json::from(turn)),
                ("reason", Json::from(reason)),
                ("steps", Json::from(steps)),
                ("toolCalls", Json::from(tool_calls)),
            ]),
        )?;
        Ok(TurnReport {
            reason: reason.to_string(),
            steps,
            tool_calls,
            tokens: self.budget.snapshot().get("used").and_then(|u| u.u64_at("tokens")).unwrap_or(0),
            final_text: text,
        })
    }
}

/// Reconcile effects that were started but never finished. Idempotent tools are
/// re-run; side-effecting tools are recorded as `effect/unknown` unless a human
/// explicitly allows the replay.
pub struct ResumeReport {
    pub reconciled: Vec<String>,
    pub needs_human: Vec<String>,
}

pub fn resume(agent: &mut Agent, hooks: &mut dyn Hooks) -> Result<ResumeReport, AgentError> {
    // 未完成的调用必须从**文件**里流式取，不能靠内存里的事件缓冲：
    // 事件缓冲有 2000 条上限，大日志上的未完成调用可能早就被挤出去了。
    let report = crate::stream_verify::verify_stream(agent.journal.path())
        .map_err(AgentError::Journal)?;
    let mut reconciled = Vec::new();
    let mut needs_human = Vec::new();

    for event in &report.open_tool_events {
        let call_id = event.str_at("callId").unwrap_or("?").to_string();
        let tool_name = event.str_at("tool").unwrap_or("?").to_string();
        let args = event.get("args").cloned().unwrap_or(Json::Obj(Vec::new()));

        // 只拷出两个函数指针和一个布尔，不把 `&Tool` 借过来：下面每一处
        // 写日志、走人工审批都要 `&mut agent`，而工具表挂在 agent 上。
        let ctx = agent.ctx();
        let tool = find(&agent.tools, &tool_name).map(|t| (t.effects, t.run, t.idempotent));
        match tool {
            None => {
                agent.log_event(
                    "effect/unknown",
                    Json::obj(vec![
                        ("callId", Json::from(call_id.as_str())),
                        ("tool", Json::from(tool_name.as_str())),
                        ("reason", Json::from("工具已不存在，无法判断是否生效")),
                    ]),
                )?;
                needs_human.push(format!("{}: 工具 {} 已不存在", call_id, tool_name));
            }
            Some((effects_fn, run_fn, true)) => {
                let effects = effects_fn(&args, &ctx);
                let verdict = agent.policy.decide(&effects);
                if verdict.decision != Decision::Allow {
                    agent.log_event(
                        "effect/unknown",
                        Json::obj(vec![
                            ("callId", Json::from(call_id.as_str())),
                            ("tool", Json::from(tool_name.as_str())),
                            ("reason", Json::from(format!(
                                "可重放工具当前策略不允许重跑：{}",
                                verdict.reason
                            ))),
                        ]),
                    )?;
                    needs_human.push(format!(
                        "{}: {} 当前策略不是 allow，未重放",
                        call_id, tool_name
                    ));
                    continue;
                }
                let result = run_fn(&args, &ctx);
                agent.log_event(
                    "effect/rerun",
                    Json::obj(vec![
                        ("callId", Json::from(call_id.as_str())),
                        ("tool", Json::from(tool_name.as_str())),
                        ("reason", Json::from("可重放，安全重跑")),
                    ]),
                )?;
                agent.log_event(
                    "tool/end",
                    Json::obj(vec![
                        ("callId", Json::from(call_id.as_str())),
                        ("tool", Json::from(tool_name.as_str())),
                        ("ok", Json::Bool(result.ok)),
                        ("error", Json::from("RESUMED")),
                    ]),
                )?;
                reconciled.push(format!("{}: 重跑了可重放的 {}", call_id, tool_name));
            }
            Some((effects_fn, run_fn, false)) => {
                let effects = effects_fn(&args, &ctx);
                let answer = agent.request_approval(
                    hooks,
                    "resume",
                    &call_id,
                    &tool_name,
                    &args,
                    &effects,
                    "崩溃中断：这个调用可能已经生效，重放会造成二次副作用",
                )?;
                if answer.decision == Decision::Allow {
                    let result = run_fn(&args, &ctx);
                    agent.log_event(
                        "effect/rerun",
                        Json::obj(vec![
                            ("callId", Json::from(call_id.as_str())),
                            ("tool", Json::from(tool_name.as_str())),
                            ("reason", Json::from("人工允许重放")),
                        ]),
                    )?;
                    agent.log_event(
                        "tool/end",
                        Json::obj(vec![
                            ("callId", Json::from(call_id.as_str())),
                            ("tool", Json::from(tool_name.as_str())),
                            ("ok", Json::Bool(result.ok)),
                            ("error", Json::from("RESUMED_BY_HUMAN")),
                        ]),
                    )?;
                    reconciled.push(format!("{}: 人工允许后重放了 {}", call_id, tool_name));
                } else {
                    agent.log_event(
                        "effect/unknown",
                        Json::obj(vec![
                            ("callId", Json::from(call_id.as_str())),
                            ("tool", Json::from(tool_name.as_str())),
                            ("reason", Json::from("有副作用且未获准重放，状态未知")),
                        ]),
                    )?;
                    needs_human.push(format!(
                        "{}: {} 可能已经生效，需要人工确认结果",
                        call_id, tool_name
                    ));
                }
            }
        }
    }

    if let Some(turn) = &report.open_turn_event {
        let n = turn.u64_at("turn").unwrap_or(0);
        agent.log_event(
            "turn/end",
            Json::obj(vec![
                ("turn", Json::from(n)),
                ("reason", Json::from("resumed")),
            ]),
        )?;
    }

    Ok(ResumeReport {
        reconciled,
        needs_human,
    })
}

pub fn describe_snapshot(s: &Snapshot) -> String {
    let mut out = format!("snapshot {} · {} tokens · {} bytes\n", s.id, s.tokens, s.bytes);
    for section in &s.sections {
        out.push_str(&format!(
            "  {:<8} {:<8} {:>6} tok  {:>6} B  {}\n",
            section.id,
            section.owner,
            section.tokens,
            section.bytes,
            section.volatility
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ui-agent-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn model(v: &Json) -> Box<dyn Model + Send> {
        Box::new(crate::model::ScriptedModel::from_json(v).unwrap())
    }

    #[test]
    fn long_tool_output_goes_to_disk_and_only_a_slice_stays_in_context() {
        let ws = workspace("trim");
        // 用 fs_read 产生大输出（读不需要审批），先造一个大文件
        let big = (1..=400)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(ws.join("big.txt"), &big).unwrap();
        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let script = Json::obj(vec![
            ("model", Json::from("scripted")),
            (
                "steps",
                Json::Arr(vec![
                    Json::obj(vec![
                        ("content", Json::from("读个大文件")),
                        (
                            "toolCalls",
                            Json::Arr(vec![Json::obj(vec![
                                ("name", Json::from("fs_read")),
                                ("args", Json::obj(vec![("path", Json::from("big.txt"))])),
                            ])]),
                        ),
                    ]),
                    Json::obj(vec![("content", Json::from("好了"))]),
                ]),
            ),
        ]);

        let mut agent = Agent::new(journal, model(&script), ws.clone(), Limits::default());
        agent.tool_chars = 300;
        let report = agent.run_turn("看看输出", &mut NullHooks).unwrap();
        assert_eq!(report.reason, "done");

        let kept = agent
            .history
            .iter()
            .find(|m| m.role == "tool")
            .expect("应有工具结果");
        assert!(kept.content.contains("已截断"), "{}", kept.content);
        assert!(kept.content.contains("tool-output"), "应给出指针：{}", kept.content);
        assert!(
            kept.content.chars().count() < 900,
            "上下文里只能留一小片，实际 {}",
            kept.content.chars().count()
        );

        // 全量必须留在磁盘上，而且模型能通过 fs_read 再读
        let dir = ws.join(".ui").join("tool-output");
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        let full = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(
            full.chars().count() > kept.content.chars().count() * 2,
            "落盘的内容必须明显多于上下文里保留的（{} vs {}）",
            full.chars().count(),
            kept.content.chars().count()
        );

        // 这件事要留痕：谁、多大、截到多少
        let agent_journal = Journal::load(ws.join(".ui").join("journal.jsonl")).unwrap();
        let trim = agent_journal
            .iter()
            .find(|e| e.str_at("kind") == Some("tool/trim"))
            .expect("应有 tool/trim 事件");
        assert!(trim.u64_at("totalChars").unwrap() > trim.u64_at("keptChars").unwrap());
    }

    #[test]
    fn short_tool_output_is_not_touched() {
        let ws = workspace("short");
        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let script = Json::obj(vec![
            ("model", Json::from("scripted")),
            (
                "steps",
                Json::Arr(vec![
                    Json::obj(vec![
                        ("content", Json::from("读个小文件")),
                        (
                            "toolCalls",
                            Json::Arr(vec![Json::obj(vec![
                                ("name", Json::from("fs_write")),
                                (
                                    "args",
                                    Json::obj(vec![
                                        ("path", Json::from("small.txt")),
                                        ("content", Json::from("hi")),
                                    ]),
                                ),
                            ])]),
                        ),
                    ]),
                    Json::obj(vec![("content", Json::from("好了"))]),
                ]),
            ),
        ]);
        let mut agent = Agent::new(journal, model(&script), ws.clone(), Limits::default());
        agent.run_turn("写个小文件", &mut NullHooks).unwrap();
        let kept = agent
            .history
            .iter()
            .find(|m| m.role == "tool")
            .expect("应有工具结果");
        assert!(!kept.content.contains("已截断"));
        assert!(!ws.join(".ui").join("tool-output").exists());
    }

    #[test]
    fn runs_a_write_then_read_turn() {
        let ws = workspace("loop");
        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let script = Json::obj(vec![
            ("model", Json::from("scripted")),
            (
                "steps",
                Json::Arr(vec![
                    Json::obj(vec![
                        ("content", Json::from("先写文件")),
                        (
                            "toolCalls",
                            Json::Arr(vec![Json::obj(vec![
                                ("name", Json::from("fs_write")),
                                (
                                    "args",
                                    Json::obj(vec![
                                        ("path", Json::from("hello.txt")),
                                        ("content", Json::from("hi")),
                                    ]),
                                ),
                            ])]),
                        ),
                    ]),
                    Json::obj(vec![
                        ("content", Json::from("写完并核对")),
                        (
                            "toolCalls",
                            Json::Arr(vec![Json::obj(vec![
                                ("name", Json::from("fs_read")),
                                ("args", Json::obj(vec![("path", Json::from("hello.txt"))])),
                            ])]),
                        ),
                    ]),
                    Json::obj(vec![("content", Json::from("完成"))]),
                ]),
            ),
        ]);
        let mut agent = Agent::new(journal, model(&script), ws.clone(), Limits::default());
        let mut hooks = NullHooks;
        let report = agent.run_turn("写个文件", &mut hooks).unwrap();
        assert_eq!(report.reason, "done");
        assert_eq!(report.tool_calls, 2);
        assert_eq!(std::fs::read_to_string(ws.join("hello.txt")).unwrap(), "hi");
        assert!(report.final_text.contains("完成"));

        // The turn is fully journaled: snapshot -> request -> start -> end.
        let kinds: Vec<&str> = agent
            .journal
            .events
            .iter()
            .filter_map(|e| e.str_at("kind"))
            .collect();
        assert!(kinds.contains(&"prompt/snapshot"));
        assert!(kinds.contains(&"tool/request"));
        assert_eq!(
            kinds.iter().filter(|k| **k == "tool/start").count(),
            2,
            "every call must be write-ahead logged"
        );
    }

    #[test]
    fn repairs_malformed_arguments_instead_of_dying() {
        let ws = workspace("repair");
        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let script = Json::obj(vec![
            ("model", Json::from("scripted")),
            (
                "steps",
                Json::Arr(vec![
                    Json::obj(vec![
                        ("content", Json::from("试着写")),
                        (
                            "toolCalls",
                            Json::Arr(vec![Json::obj(vec![
                                ("name", Json::from("fs_write")),
                                ("argsRaw", Json::from("<<not json at all>>")),
                            ])]),
                        ),
                    ]),
                    Json::obj(vec![("content", Json::from("完成"))]),
                ]),
            ),
            (
                "repairs",
                Json::Arr(vec![Json::obj(vec![
                    ("content", Json::from("重写参数")),
                    (
                        "toolCalls",
                        Json::Arr(vec![Json::obj(vec![
                            ("name", Json::from("fs_write")),
                            (
                                "args",
                                Json::obj(vec![
                                    ("path", Json::from("bad.txt")),
                                    ("content", Json::from("fixed")),
                                ]),
                            ),
                        ])]),
                    ),
                ])]),
            ),
        ]);
        let mut agent = Agent::new(journal, model(&script), ws.clone(), Limits::default());
        let mut hooks = NullHooks;
        let report = agent.run_turn("写个坏文件", &mut hooks).unwrap();
        assert_eq!(report.reason, "done");
        assert_eq!(std::fs::read_to_string(ws.join("bad.txt")).unwrap(), "fixed");
        let repairs = agent
            .journal
            .events
            .iter()
            .filter(|e| e.str_at("kind") == Some("model/repair"))
            .count();
        assert_eq!(repairs, 1, "the kernel must ask once, then accept the fix");
    }

    #[test]
    fn stops_when_budget_is_exhausted() {
        let ws = workspace("budget");
        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let mut steps: Vec<Json> = (0..10)
            .map(|_| {
                Json::obj(vec![
                    ("content", Json::from("loop")),
                    (
                        "toolCalls",
                        Json::Arr(vec![Json::obj(vec![
                            ("name", Json::from("fs_read")),
                            ("args", Json::obj(vec![("path", Json::from("nope"))])),
                        ])]),
                    ),
                ])
            })
            .collect();
        steps.push(Json::obj(vec![("content", Json::from("done"))]));
        let script = Json::obj(vec![
            ("model", Json::from("scripted")),
            ("steps", Json::Arr(steps)),
        ]);
        let limits = Limits {
            tokens: 200_000,
            tool_calls: 2,
            wall_ms: 60_000,
            usd: 1.0,
            usd_per_mtok: 0.0,
            max_steps: 24,
        };
        let mut agent = Agent::new(journal, model(&script), ws, limits);
        let mut hooks = NullHooks;
        let err = agent.run_turn("循环", &mut hooks).unwrap_err();
        assert!(
            matches!(err, AgentError::Budget(_)),
            "tool-call budget exhaustion must abort the turn, got {:?}",
            err
        );

        // The call that crosses the limit gets a tool/end BUDGET, then the turn ends.
        let denied = agent
            .journal
            .events
            .iter()
            .filter(|e| e.str_at("error") == Some("BUDGET"))
            .count();
        assert_eq!(denied, 1);
        assert!(
            agent.journal.events.iter().any(|e| {
                e.str_at("kind") == Some("turn/end")
                    && e.str_at("reason") == Some("budget")
            }),
            "budget exhaustion must close the turn with reason=budget"
        );
        assert_eq!(
            agent
                .journal
                .events
                .iter()
                .filter(|e| e.str_at("kind") == Some("tool/start"))
                .count(),
            2,
            "only the first two calls may actually execute"
        );
    }

    #[test]
    fn repair_attempts_are_billed_and_reconciled() {
        let ws = workspace("repair-usage");
        std::fs::write(ws.join("a.txt"), "hello").unwrap();
        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let script = Json::obj(vec![
            ("model", Json::from("scripted")),
            (
                "steps",
                Json::Arr(vec![
                    Json::obj(vec![
                        ("content", Json::from("bad")),
                        ("usageTokens", Json::from(1_000u64)),
                        (
                            "toolCalls",
                            Json::Arr(vec![Json::obj(vec![
                                ("name", Json::from("fs_read")),
                                ("argsRaw", Json::from("{")),
                            ])]),
                        ),
                    ]),
                    Json::obj(vec![("content", Json::from("done"))]),
                ]),
            ),
            (
                "repairs",
                Json::Arr(vec![Json::obj(vec![
                    ("content", Json::from("fixed")),
                    ("usageTokens", Json::from(1_000u64)),
                ])]),
            ),
        ]);
        let mut agent = Agent::new(journal, model(&script), ws, Limits::default());
        let report = agent.run_turn("修一下", &mut NullHooks).unwrap();
        assert_eq!(report.reason, "done");
        assert_eq!(
            agent.budget.used(Dim::Tokens),
            2_000.0,
            "两次尝试的 provider usage 都必须进账，不能只算最后一次 (estimated={})",
            agent.last_model_estimated
        );
        assert!(
            agent
                .journal
                .events
                .iter()
                .any(|e| e.str_at("kind") == Some("model/repair")
                    && e.u64_at("usage") == Some(1_000)),
            "repair 事件必须记录第一次尝试的 usage"
        );
    }

    #[test]
    fn usd_budget_is_charged_from_provider_usage() {
        let ws = workspace("usd-budget");
        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let script = Json::obj(vec![
            ("model", Json::from("scripted")),
            (
                "steps",
                Json::Arr(vec![Json::obj(vec![
                    ("content", Json::from("done")),
                    ("usageTokens", Json::from(1_000_000u64)),
                ])]),
            ),
        ]);
        let limits = Limits {
            usd: 0.5,
            usd_per_mtok: 1.0,
            ..Limits::default()
        };
        let mut agent = Agent::new(journal, model(&script), ws, limits);
        let err = agent.run_turn("花钱", &mut NullHooks).unwrap_err();
        assert!(
            matches!(err, AgentError::Budget(_)),
            "1M tokens × $1/M 必须超过 $0.5 预算，got {:?}",
            err
        );
    }

    #[test]
    fn switch_journal_resets_the_turn_counter() {
        let ws = workspace("switch-turn");
        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let script = Json::obj(vec![
            ("model", Json::from("scripted")),
            ("steps", Json::Arr(vec![Json::obj(vec![("content", Json::from("ok"))])])),
        ]);
        let mut agent = Agent::new(journal, model(&script), ws.clone(), Limits::default());
        let other = ws.join(".ui").join("other.jsonl");
        Journal::open(&other).unwrap();
        agent.switch_journal(&other).unwrap();
        assert_eq!(agent.turn_number(), 0, "空会话的 turn 必须从 0 开始");
    }

    #[cfg(unix)]
    #[test]
    fn shell_output_is_spilled_in_full_not_capped_before_spill() {
        let ws = workspace("shell-spill");
        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let script = Json::obj(vec![
            ("model", Json::from("scripted")),
            (
                "steps",
                Json::Arr(vec![
                    Json::obj(vec![
                        ("content", Json::from("跑")),
                        (
                            "toolCalls",
                            Json::Arr(vec![Json::obj(vec![
                                ("name", Json::from("shell")),
                                (
                                    "args",
                                    Json::obj(vec![(
                                        "command",
                                        Json::from("python3 -c 'print(\"x\" * 20000)'"),
                                    )]),
                                ),
                            ])]),
                        ),
                    ]),
                    Json::obj(vec![("content", Json::from("好了"))]),
                ]),
            ),
        ]);
        let mut agent = Agent::new(journal, model(&script), ws.clone(), Limits::default());
        agent.tool_chars = 200;
        let mut hooks = Recorder {
            answer: ApprovalResponse::allow_once(),
            asks: Vec::new(),
        };
        agent.run_turn("跑长输出", &mut hooks).unwrap();

        let dir = ws.join(".ui").join("tool-output");
        let file = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.is_file())
            .expect("长 shell 输出必须落盘");
        let full = std::fs::read_to_string(file).unwrap();
        assert!(
            full.len() >= 20_000,
            "spill 必须是完整输出，不是 16KB cap 后的片段: {}",
            full.len()
        );
        assert!(
            agent.history.iter().any(|m| m.role == "tool" && m.content.contains("完整内容在")),
            "模型上下文必须有 spill 指针"
        );
    }

    #[test]
    fn history_carries_across_turns() {
        let ws = workspace("history");
        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let script = Json::obj(vec![
            ("model", Json::from("scripted")),
            (
                "steps",
                Json::Arr(vec![
                    Json::obj(vec![("content", Json::from("第一轮答话"))]),
                    Json::obj(vec![("content", Json::from("第二轮答话"))]),
                ]),
            ),
        ]);
        let mut agent = Agent::new(journal, model(&script), ws, Limits::default());
        let mut hooks = NullHooks;
        agent.run_turn("第一问", &mut hooks).unwrap();
        agent.run_turn("第二问", &mut hooks).unwrap();

        let snaps: Vec<&Json> = agent
            .journal
            .events
            .iter()
            .filter(|e| e.str_at("kind") == Some("prompt/snapshot"))
            .collect();
        assert_eq!(snaps.len(), 2, "每轮至少一次快照");

        let history_bytes = |e: &Json| {
            e.arr_at("sections")
                .and_then(|s| s.iter().find(|x| x.str_at("id") == Some("history")))
                .and_then(|x| x.u64_at("bytes"))
                .unwrap_or(0)
        };
        assert!(
            history_bytes(snaps[1]) > history_bytes(snaps[0]),
            "第二轮必须看得到第一轮：{} vs {}",
            history_bytes(snaps[0]),
            history_bytes(snaps[1])
        );
        assert_eq!(agent.history.len(), 4, "两轮 = 两条问 + 两条答");
    }

    #[test]
    fn compacts_history_so_conversation_can_continue() {
        use crate::compact::{history_tokens, CompactPolicy};

        let ws = workspace("compact");
        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let steps: Vec<Json> = (0..12)
            .map(|i| {
                Json::obj(vec![(
                    "content",
                    Json::from(format!("第 {} 轮回答，内容要占一点位置好触发折叠。", i)),
                )])
            })
            .collect();
        let script = Json::obj(vec![
            ("model", Json::from("scripted")),
            ("steps", Json::Arr(steps)),
        ]);
        let mut agent = Agent::new(journal, model(&script), ws, Limits::default());
        agent.compact = CompactPolicy {
            max_history_tokens: 200,
            keep_recent: 4,
            keep_first_user: true,
        };
        let mut hooks = NullHooks;
        for i in 0..12 {
            agent.run_turn(&format!("问题 {}", i), &mut hooks).unwrap();
        }

        let compacts = agent
            .journal
            .events
            .iter()
            .filter(|e| e.str_at("kind") == Some("compact"))
            .count();
        assert!(compacts > 0, "超过阈值必须折叠，并且记进日志");

        let tokens = history_tokens(&agent.history);
        assert!(tokens < 800, "历史应被压住，实际 {}", tokens);
        assert!(
            agent
                .history
                .iter()
                .any(|m| m.role == "user" && m.content.contains("问题 0")),
            "任务锚点（第一条用户消息）不能被折掉"
        );
        assert!(
            agent.history.iter().any(|m| m.role == "note" && m.content.contains("已折叠")),
            "被折叠的部分要留下摘要"
        );
    }

    #[test]
    fn cancel_stops_the_turn_and_keeps_history() {
        struct Cancelling;
        impl Model for Cancelling {
            fn name(&self) -> &str {
                "cancelling"
            }
            fn respond(
                &mut self,
                _s: &Snapshot,
                _r: Option<&str>,
                cancel: &crate::cancel::Cancel,
            ) -> Result<crate::model::ModelResponse, String> {
                cancel.cancel();
                Ok(crate::model::ModelResponse::text("这句话不该被采用"))
            }
        }

        let ws = workspace("cancel");
        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let mut agent = Agent::new(journal, Box::new(Cancelling), ws, Limits::default());
        let mut hooks = NullHooks;
        let report = agent.run_turn("干点活", &mut hooks).unwrap();

        assert_eq!(report.reason, "cancelled");
        let starts = agent
            .journal
            .events
            .iter()
            .filter(|e| e.str_at("kind") == Some("tool/start"))
            .count();
        assert_eq!(starts, 0, "取消后不应再执行任何工具");
        assert!(
            agent.history.iter().any(|m| m.role == "user" && m.content == "干点活"),
            "取消不丢历史：下一轮还能接着聊"
        );

        // 下一轮开始时取消标记被复位
        let journal2 = Journal::open(agent.journal.path()).unwrap();
        let mut agent2 = Agent::new(
            journal2,
            model(&Json::obj(vec![
                ("model", Json::from("scripted")),
                ("steps", Json::Arr(vec![Json::obj(vec![("content", Json::from("好"))])])),
            ])),
            std::path::PathBuf::from("/tmp"),
            Limits::default(),
        );
        assert!(!agent2.cancel.is_cancelled());
        agent2.run_turn("再来一次", &mut hooks).unwrap();
    }

    #[test]
    fn loading_a_skill_adds_a_priced_section() {
        use std::fs;
        let ws = workspace("skill");
        let dir = ws.join("skills").join("demo-skill");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: demo-skill\ndescription: 演示用\n---\n这一步必须输出三行。",
        )
        .unwrap();

        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let script = Json::obj(vec![
            ("model", Json::from("scripted")),
            (
                "steps",
                Json::Arr(vec![
                    Json::obj(vec![
                        ("content", Json::from("先加载 skill")),
                        (
                            "toolCalls",
                            Json::Arr(vec![Json::obj(vec![
                                ("name", Json::from("skill")),
                                ("args", Json::obj(vec![("name", Json::from("demo-skill"))])),
                            ])]),
                        ),
                    ]),
                    Json::obj(vec![("content", Json::from("按 skill 做了"))]),
                ]),
            ),
        ]);
        let mut agent = Agent::new(journal, model(&script), ws, Limits::default());
        let mut hooks = NullHooks;
        agent.run_turn("用 demo-skill 干活", &mut hooks).unwrap();

        assert_eq!(agent.loaded_skills.len(), 1);
        assert_eq!(agent.loaded_skills[0].0, "demo-skill");

        let loads = agent
            .journal
            .events
            .iter()
            .filter(|e| e.str_at("kind") == Some("skill/load"))
            .count();
        assert_eq!(loads, 1, "加载必须留痕");

        // 第二次快照里应该出现独立的 skill-body 段（可计价，不是塞进 history）
        let snaps: Vec<&Json> = agent
            .journal
            .events
            .iter()
            .filter(|e| e.str_at("kind") == Some("prompt/snapshot"))
            .collect();
        let last = snaps.last().unwrap();
        let body = last
            .arr_at("sections")
            .and_then(|s| s.iter().find(|x| x.str_at("id") == Some("skill-body")))
            .expect("skill-body 段必须存在");
        assert!(body.u64_at("bytes").unwrap_or(0) > 0);
        assert_eq!(body.str_at("owner"), Some("skill"));
    }

    #[test]
    fn empty_reply_is_not_treated_as_done() {
        let ws = workspace("empty");
        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let script = Json::obj(vec![
            ("model", Json::from("scripted")),
            (
                "steps",
                Json::Arr(vec![
                    Json::obj(vec![("content", Json::from(""))]),
                    Json::obj(vec![("content", Json::from("这才是结论"))]),
                ]),
            ),
        ]);
        let mut agent = Agent::new(journal, model(&script), ws, Limits::default());
        let mut hooks = NullHooks;
        let report = agent.run_turn("干点活", &mut hooks).unwrap();

        assert_eq!(report.reason, "done");
        assert_eq!(report.final_text, "这才是结论", "空回复后必须重来一次");
        assert_eq!(
            agent
                .journal
                .events
                .iter()
                .filter(|e| e.str_at("kind") == Some("model/empty"))
                .count(),
            1
        );
    }

    #[test]
    fn two_empty_replies_end_honestly() {
        let ws = workspace("empty2");
        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let script = Json::obj(vec![
            ("model", Json::from("scripted")),
            (
                "steps",
                Json::Arr(vec![
                    Json::obj(vec![("content", Json::from(""))]),
                    Json::obj(vec![("content", Json::from("  "))]),
                ]),
            ),
        ]);
        let mut agent = Agent::new(journal, model(&script), ws, Limits::default());
        let mut hooks = NullHooks;
        let report = agent.run_turn("干点活", &mut hooks).unwrap();
        assert_eq!(report.reason, "empty_reply", "不能报成功");
    }

    #[test]
    fn protocol_violation_asks_for_a_rewrite() {
        struct Drifting {
            calls: usize,
        }
        impl Model for Drifting {
            fn name(&self) -> &str {
                "drifting"
            }
            fn respond(
                &mut self,
                _s: &Snapshot,
                repair: Option<&str>,
                _c: &crate::cancel::Cancel,
            ) -> Result<crate::model::ModelResponse, String> {
                self.calls += 1;
                if self.calls == 1 {
                    // 第一条是散文，不是协议 JSON
                    Ok(crate::model::ModelResponse {
                        content: "我来看看文件 [fs_read(path=x)]".to_string(),
                        tool_calls: Vec::new(),
                        usage_tokens: None,
                        protocol_error: Some("不是 JSON 对象".to_string()),
                    })
                } else {
                    assert!(repair.is_some(), "重写时必须带上错误说明");
                    Ok(crate::model::ModelResponse::text("这次按协议来了"))
                }
            }
        }

        let ws = workspace("protocol");
        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let mut agent = Agent::new(journal, Box::new(Drifting { calls: 0 }), ws, Limits::default());
        let mut hooks = NullHooks;
        let report = agent.run_turn("干点活", &mut hooks).unwrap();

        assert_eq!(report.reason, "done");
        assert_eq!(report.final_text, "这次按协议来了");
        assert_eq!(
            agent
                .journal
                .events
                .iter()
                .filter(|e| e.str_at("kind") == Some("model/repair"))
                .count(),
            1,
            "协议违规要留痕"
        );
    }

    #[test]
    fn deltas_flow_through_hooks_and_final_text_replaces_them() {
        struct Streaming;
        impl Model for Streaming {
            fn name(&self) -> &str {
                "streaming"
            }
            fn respond(
                &mut self,
                _s: &Snapshot,
                _r: Option<&str>,
                _c: &crate::cancel::Cancel,
            ) -> Result<crate::model::ModelResponse, String> {
                Ok(crate::model::ModelResponse::text("最终回复"))
            }
            fn respond_stream(
                &mut self,
                _s: &Snapshot,
                _r: Option<&str>,
                _c: &crate::cancel::Cancel,
                on_delta: &mut dyn FnMut(&str),
            ) -> Result<crate::model::ModelResponse, String> {
                on_delta("逐");
                on_delta("字");
                Ok(crate::model::ModelResponse::text("最终回复"))
            }
        }

        #[derive(Default)]
        struct Recorder {
            deltas: Vec<String>,
            assistant: Vec<String>,
        }
        impl Hooks for Recorder {
            fn on_delta(&mut self, text: &str) {
                self.deltas.push(text.to_string());
            }
            fn on_assistant(&mut self, text: &str) {
                self.assistant.push(text.to_string());
            }
        }

        let ws = workspace("delta");
        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let mut agent = Agent::new(journal, Box::new(Streaming), ws, Limits::default());
        let mut hooks = Recorder::default();
        let report = agent.run_turn("说点什么", &mut hooks).unwrap();

        assert_eq!(report.reason, "done");
        assert_eq!(hooks.deltas, vec!["逐", "字"], "delta 必须逐条到达 hooks");
        assert_eq!(hooks.assistant, vec!["最终回复"], "最终文本走 on_assistant");
    }

    #[test]
    fn denied_effects_never_reach_the_tool() {
        let ws = workspace("deny");
        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let script = Json::obj(vec![
            ("model", Json::from("scripted")),
            (
                "steps",
                Json::Arr(vec![
                    Json::obj(vec![
                        ("content", Json::from("想写外面")),
                        (
                            "toolCalls",
                            Json::Arr(vec![Json::obj(vec![
                                ("name", Json::from("fs_write")),
                                (
                                    "args",
                                    Json::obj(vec![
                                        ("path", Json::from("/etc/ui-should-not-exist")),
                                        ("content", Json::from("nope")),
                                    ]),
                                ),
                            ])]),
                        ),
                    ]),
                    Json::obj(vec![("content", Json::from("好，不写"))]),
                ]),
            ),
        ]);
        let mut agent = Agent::new(journal, model(&script), ws.clone(), Limits::default());
        let mut hooks = NullHooks;
        agent.run_turn("写 /etc", &mut hooks).unwrap();

        let denied = agent
            .journal
            .events
            .iter()
            .filter(|e| e.str_at("error") == Some("DENIED"))
            .count();
        assert_eq!(denied, 1);
        assert!(!std::path::Path::new("/etc/ui-should-not-exist").exists());
    }

    // ── Phase 0：控制面（身份、作用域、run 边界）────────────────────────

    /// 每次都按预设答案回答，并把「被问的是谁」记下来。
    struct Recorder {
        answer: ApprovalResponse,
        asks: Vec<(String, String)>,
    }

    impl Hooks for Recorder {
        fn on_approval(&mut self, req: &ApprovalRequest) -> ApprovalResponse {
            self.asks
                .push((req.approval_id.clone(), req.call_id.clone()));
            self.answer
        }
    }

    /// 每轮脚本 = 「写 a.txt」「再写 a.txt」「收工」。
    ///
    /// 用 fs_write 而不是 shell：严格模式下写盘就要问人，能稳定走进审批分支，
    /// 又不用在测试里派生子进程。
    fn writes_a_twice(turns: usize) -> Json {
        let call = || {
            let args = Json::obj(vec![
                ("path", Json::from("a.txt")),
                ("content", Json::from("内容")),
            ]);
            let one = Json::obj(vec![
                ("name", Json::from("fs_write")),
                ("args", args),
            ]);
            Json::obj(vec![("toolCalls", Json::Arr(vec![one]))])
        };
        let mut steps = Vec::new();
        for _ in 0..turns {
            steps.push(call());
            steps.push(call());
            steps.push(Json::obj(vec![("content", Json::from("好了"))]));
        }
        Json::obj(vec![
            ("model", Json::from("scripted")),
            ("steps", Json::Arr(steps)),
        ])
    }

    fn strict_agent(name: &str, script: &Json) -> Agent {
        let ws = workspace(name);
        std::fs::create_dir_all(ws.join(".ui")).unwrap();
        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let mut agent = Agent::new(journal, model(script), ws, Limits::default());
        agent.mode = crate::mode::Mode::Strict;
        agent
    }

    #[test]
    fn approval_ids_address_every_question_and_answer() {
        let script = writes_a_twice(1);
        let mut agent = strict_agent("approval-identity", &script);
        let mut hooks = Recorder {
            answer: ApprovalResponse::allow_once(),
            asks: Vec::new(),
        };
        agent.run_turn("写两次", &mut hooks).unwrap();

        // 同一轮里的两次询问，必须是两个互不相同的身份
        assert_eq!(hooks.asks.len(), 2, "两次写各问一次");
        assert_ne!(hooks.asks[0].0, hooks.asks[1].0, "approval_id 不能重复");
        assert_ne!(hooks.asks[0].1, hooks.asks[1].1, "call_id 不能重复");
        for (approval_id, call_id) in &hooks.asks {
            assert!(approval_id.starts_with(&format!("ap-{}-", agent.journal.writer())));
            let asked = agent
                .journal
                .events
                .iter()
                .filter(|e| {
                    e.str_at("kind") == Some("approval/requested")
                        && e.str_at("approvalId") == Some(approval_id.as_str())
                })
                .count();
            let answered = agent
                .journal
                .events
                .iter()
                .filter(|e| {
                    e.str_at("kind") == Some("approval/answered")
                        && e.str_at("approvalId") == Some(approval_id.as_str())
                        && e.str_at("callId") == Some(call_id.as_str())
                })
                .count();
            assert_eq!((asked, answered), (1, 1), "一问一答要按同一个身份对上");
        }
    }

    /// 没有审批通道时的拒绝，不能在人眼里和「人说不」长得一样。
    /// 安全默认不变（仍旧拒绝），但账本必须写清这个决定是谁做的。
    #[test]
    fn answered_rows_say_who_decided() {
        struct Silent;
        impl Hooks for Silent {}

        let script = writes_a_twice(1);
        let mut agent = strict_agent("approval-via", &script);
        agent.run_turn("写一次", &mut Silent).unwrap();
        let row = agent
            .journal
            .events
            .iter()
            .find(|e| e.str_at("kind") == Some("approval/answered"))
            .expect("没人回答也要留下回执");
        assert_eq!(row.str_at("verdict"), Some("deny"));
        assert_eq!(
            row.str_at("via"),
            Some("no-channel"),
            "「没人能回答」要写进日志，否则复盘时它和一次否决无法区分"
        );

        // 人明确表态：不写 via——它是"代答"的记号，不是每行都有的标签。
        let mut agent = strict_agent("approval-via-human", &script);
        let mut hooks = Recorder {
            answer: ApprovalResponse::allow_once(),
            asks: Vec::new(),
        };
        agent.run_turn("写一次", &mut hooks).unwrap();
        let row = agent
            .journal
            .events
            .iter()
            .find(|e| e.str_at("kind") == Some("approval/answered"))
            .expect("有回执");
        assert_eq!(row.str_at("via"), None, "人自己答的，不该被标成代答: {:?}", row);

        // 听不懂的词：拒绝 + 记号，两者一起落账。
        let mut agent = strict_agent("approval-via-word", &script);
        let mut hooks = Recorder {
            answer: ApprovalResponse::parse_verdict("yes-pls"),
            asks: Vec::new(),
        };
        agent.run_turn("写一次", &mut hooks).unwrap();
        let row = agent
            .journal
            .events
            .iter()
            .find(|e| e.str_at("kind") == Some("approval/answered"))
            .expect("有回执");
        assert_eq!((row.str_at("verdict"), row.str_at("via")), (Some("deny"), Some("unknown-word")));
    }

    /// 「y」和「yes」必须是同一个口子，也不能顺手把范围放大成会话。
    #[test]
    fn verdict_words_allows_only_as_much_as_they_say() {
        let yes = ApprovalResponse::parse_verdict("yes");
        assert_eq!(
            (yes.decision.name(), yes.scope.name()),
            ("allow", "once"),
            "yes 只放这一次"
        );
        assert_eq!(ApprovalResponse::parse_verdict("y").decision.name(), "allow");
        assert_eq!(
            ApprovalResponse::parse_verdict("session").scope.name(),
            "session",
            "放大范围必须用明确的词"
        );
        // 文档里写过的词必须真的能用：照抄 `allow-once` 却得到静默拒绝，
        // 是最难查的那类集成事故——它看起来"回答了"，只是回答成了"不"。
        assert_eq!(
            (
                ApprovalResponse::parse_verdict("allow-once").decision.name(),
                ApprovalResponse::parse_verdict("allow-once").scope.name()
            ),
            ("allow", "once")
        );
        assert_eq!(
            ApprovalResponse::parse_verdict("allow-always").scope.name(),
            "session"
        );
        for no in ["", "n", "no", "maybe", "allow-once-but-only-kind-of"] {
            assert_eq!(
                ApprovalResponse::parse_verdict(no).decision.name(),
                "deny",
                "认出来的样子一律按拒绝处理：{no}"
            );
        }
        // 同样是 deny，来源不同：明确说了「不」，和我们听不懂他说什么。
        assert_eq!(ApprovalResponse::parse_verdict("no").via, None);
        assert_eq!(ApprovalResponse::parse_verdict("deny").via, None);
        assert_eq!(
            ApprovalResponse::parse_verdict("maybe").via,
            Some("unknown-word"),
            "听不懂的拒绝要留记号"
        );
        assert_eq!(
            ApprovalResponse::parse_verdict("").via,
            Some("unknown-word"),
            "空回答也一样"
        );
        // 「按拒绝」和「不懂你的词」是两件事：前者是决定，后者是 bug。
        // 界面据此在回执里注明，不至于让人以为自己的「允许」按钮失效了。
        for known in ["allow", "once", "yes", "always", "session", "deny", "no", "a", "y"] {
            assert!(
                ApprovalResponse::is_known_verdict(known),
                "{known} 是文档承诺过的词"
            );
        }
        for unknown in ["", "maybe", "ALLOW", "yes-pls", "approved"] {
            assert!(
                !ApprovalResponse::is_known_verdict(unknown),
                "{unknown} 不该被当成明确表态"
            );
        }
    }

    #[test]
    fn allow_once_approves_exactly_one_call() {
        let script = writes_a_twice(1);
        let mut agent = strict_agent("allow-once", &script);
        let base = agent.policy.rules.len();
        let mut hooks = Recorder {
            answer: ApprovalResponse::allow_once(),
            asks: Vec::new(),
        };
        let report = agent.run_turn("写两次", &mut hooks).unwrap();
        assert_eq!(report.reason, "done");
        assert_eq!(hooks.asks.len(), 2, "「仅这一次」不能免掉第二次");
        assert_eq!(agent.policy.session_rule_count(), 0);
        assert_eq!(agent.policy.rules.len(), base, "一次批准不能改规则表");
        assert_eq!(std::fs::read_to_string(agent.workspace.join("a.txt")).unwrap(), "内容");
    }

    #[test]
    fn allow_session_widens_only_until_the_run_boundary() {
        let script = writes_a_twice(2);
        let mut agent = strict_agent("allow-session", &script);
        let base = agent.policy.rules.len();
        let mut hooks = Recorder {
            answer: ApprovalResponse::allow_session(),
            asks: Vec::new(),
        };
        agent.run_turn("第一个任务", &mut hooks).unwrap();
        assert_eq!(hooks.asks.len(), 1, "会话内授权应当免掉同一 run 的第二次询问");
        assert_eq!(agent.policy.session_rule_count(), 1);

        // 换新任务：上一轮的授权必须作废。长驻 worker（serve / chat / workflow）
        // 不重置的话，用户点的是上一条消息的「本轮都允许」。
        agent.begin_run();
        assert_eq!(agent.policy.session_rule_count(), 0, "换任务要清掉会话规则");
        assert_eq!(agent.policy.rules.len(), base, "只能裁掉会话规则，基线要原样");
        assert_eq!(
            agent
                .policy
                .explain(&Effect::new(
                    crate::policy::FS_WRITE,
                    agent.workspace.join("a.txt").to_string_lossy().replace('\\', "/").as_str()
                ))
                .0,
            Decision::Allow,
            "基线里的 workspace 写权限不能被一起裁掉"
        );

        let mut second = Recorder {
            answer: ApprovalResponse::allow_once(),
            asks: Vec::new(),
        };
        agent.run_turn("第二个任务", &mut second).unwrap();
        assert_eq!(second.asks.len(), 2, "第二个任务要重新问");
    }

    #[test]
    fn begin_run_clears_budget_tool_sequence_and_cancellation() {
        let ws = workspace("begin-run");
        std::fs::write(ws.join("big.txt"), "x".repeat(4000)).unwrap();
        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let step = |name: &str| {
            Json::obj(vec![
                ("content", Json::from(name)),
                (
                    "toolCalls",
                    Json::Arr(vec![Json::obj(vec![
                        ("name", Json::from("fs_read")),
                        ("args", Json::obj(vec![("path", Json::from("big.txt"))])),
                    ])]),
                ),
            ])
        };
        let script = Json::obj(vec![
            ("model", Json::from("scripted")),
            (
                "steps",
                Json::Arr(vec![
                    step("第一次读"),
                    Json::obj(vec![("content", Json::from("好了"))]),
                    step("第二次读"),
                    Json::obj(vec![("content", Json::from("真的好了"))]),
                ]),
            ),
        ]);
        let mut agent = Agent::new(journal, model(&script), ws.clone(), Limits::default());
        agent.tool_chars = 200;
        let mut hooks = NullHooks;
        agent.run_turn("读大文件", &mut hooks).unwrap();
        assert!(agent.budget.used(Dim::Tokens) > 0.0, "第一跑要真的花掉点东西");
        assert_eq!(agent.tool_seq, 1, "第一跑截断过一次输出");
        assert_eq!(agent.policy.session_rule_count(), 0);
        // 模拟「这一轮被停止」：令牌一旦被置位，同一个令牌上的下一轮会立刻结束
        agent.cancel.cancel();
        agent.begin_run();
        assert!(
            !agent.cancel.is_cancelled(),
            "新 run 不能继承上一轮的取消状态（否则第二条消息永远发不出去）"
        );
        assert!(
            !agent.active_cancel.is_cancelled(),
            "active_cancel 才是这一轮真正看的令牌"
        );
        assert_eq!(agent.budget.used(Dim::Tokens), 0.0, "账本要按任务重新计");
        assert_eq!(agent.tool_seq, 0);
        agent.run_turn("再读一次", &mut hooks).unwrap();
        // 两次截断不能写到同一个文件里：tool_seq 归零了，名字里还得有 writer + seq
        let dir = ws.join(".ui").join("tool-output");
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(files.len(), 2, "两个 run 的截断缓存不能互相覆盖: {:?}", files);
        assert_ne!(files[0], files[1]);
        let trim_files: Vec<String> = agent
            .journal
            .events
            .iter()
            .filter(|e| e.str_at("kind") == Some("tool/trim"))
            .filter_map(|e| e.str_at("file").map(str::to_string))
            .collect();
        assert_eq!(trim_files.len(), 2);
        assert_ne!(trim_files[0], trim_files[1], "两次截断要指向不同的文件");
        for name in &files {
            // 每一片都还是自己那一轮的完整输出（4000 个 x，外加读文件工具的头）
            let body = std::fs::read_to_string(dir.join(name)).unwrap();
            assert!(
                body.contains(&"x".repeat(3000)),
                "{} 被后来的 run 覆盖了（{} 字符）",
                name,
                body.chars().count()
            );
        }
        // 历史里那条指针必须还能读回它自己的那一轮
        let first = trim_files[0].trim_start_matches(".ui/tool-output/").to_string();
        assert!(std::fs::read_to_string(dir.join(first)).is_ok());
    }

    // ── Phase 1-A：落账前脱敏 / 按来源扣留 ──────────────────────────────

    /// 一步「调工具」加一步「收尾」的最小脚本。
    fn one_call(name: &str, args: Json) -> Json {
        Json::obj(vec![
            ("model", Json::from("scripted")),
            (
                "steps",
                Json::Arr(vec![
                    Json::obj(vec![(
                        "toolCalls",
                        Json::Arr(vec![Json::obj(vec![
                            ("name", Json::from(name)),
                            ("args", args.clone()),
                        ])]),
                    )]),
                    Json::obj(vec![("content", Json::from("好了"))]),
                ]),
            ),
        ])
    }

    /// 日志开成真的 JSONL 而不是内存缓冲：这一组要验的是**磁盘上那一份**
    /// 里有没有原文，内存里的事件列表证明不了不出盘。
    fn disk_agent(name: &str, script: &Json) -> (Agent, PathBuf) {
        let ws = workspace(name);
        let journal = Journal::open(ws.join(".ui").join("journal.jsonl")).unwrap();
        let mut agent = Agent::new(journal, model(script), ws.clone(), Limits::default());
        // 钉住开关：这一组测的是"开着/关着各是怎样"，不能跟着环境里的
        // UI_REDACT 漂——否则同一条测试在两个人机器上结论不一样。
        agent.set_redact_output(true);
        (agent, ws)
    }

    fn rows(agent: &Agent, kind: &str) -> Vec<Json> {
        agent
            .journal
            .events
            .iter()
            .filter(|e| e.str_at("kind") == Some(kind))
            .cloned()
            .collect()
    }

    fn row(agent: &Agent, kind: &str) -> Json {
        rows(agent, kind)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("日志里没有 {}", kind))
    }

    fn journal_text(ws: &Path) -> String {
        std::fs::read_to_string(ws.join(".ui").join("journal.jsonl")).unwrap_or_default()
    }

    /// 工具输出里长得像凭据的东西，在**写进日志之前**换掉。
    ///
    /// 两档的顺序是有意的，这里验第一档：来源不在隐私清单上（一份普通笔记），
    /// 凭据正则就是唯一还能起作用的那一层。
    #[test]
    fn credential_shaped_output_is_masked_before_it_reaches_the_journal() {
        let key = "AKIAIOSFODNN7EXAMPLE"; // 测试夹具：密钥形状，非真凭据（allowlist secret）
        let script = one_call(
            "fs_read",
            Json::obj(vec![("path", Json::from("notes.txt"))]),
        );
        let (mut agent, ws) = disk_agent("redact-output", &script);
        std::fs::write(ws.join("notes.txt"), format!("备份用 {} 结束\n", key)).unwrap();
        agent.run_turn("读一份带凭据的笔记", &mut NullHooks).unwrap();

        let out = row(&agent, "tool/output");
        let text = out.str_at("text").unwrap();
        assert!(
            !text.contains(key),
            "落账的那一份必须是抹过的：{}",
            text
        );
        assert!(
            text.contains("[REDACTED:aws key]"),
            "要留下「这里抹过、抹的是什么类型」的痕迹：{}",
            text
        );
        assert!(
            out.num_at("redacted").unwrap_or(0.0) >= 1.0,
            "抹了几处要入账，否则看轨迹的人不知道自己少了什么：{:?}",
            out
        );
        // 磁盘上那一份是最终答案：回放、导出、发给别人用的都是它
        assert!(
            !journal_text(&ws).contains(key),
            "原文不能从日志文件里读回来"
        );
        // 模型上下文里仍是原文——它是这次会话里被批准读这个文件的。
        // 一起抹掉会让模型以为自己读到了空文件，然后重读、再重读。
        assert!(
            agent.history.iter().any(|m| m.content.contains(key)),
            "脱敏只管会外流的那一份，不管上下文"
        );
        // 笔记本身没被改：脱敏是记账时的处理，不是对数据的编辑
        assert!(std::fs::read_to_string(ws.join("notes.txt"))
            .unwrap()
            .contains(key));
    }

    /// 参数比输出更早外流：`tool/request` 带 args，`model/response` 带
    /// toolCalls，`fs_write` 的 `content` 里塞什么日志里就有什么。
    #[test]
    fn credentials_in_arguments_are_masked_wherever_they_are_written() {
        let token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"; // 测试夹具：密钥形状，非真凭据（allowlist secret）
        let script = one_call(
            "fs_write",
            Json::obj(vec![
                ("path", Json::from("out/notes.txt")),
                ("content", Json::from(format!("git push 用的 {}\n", token))),
            ]),
        );
        let (mut agent, ws) = disk_agent("redact-args", &script);
        agent.run_turn("写一份带凭据的笔记", &mut NullHooks).unwrap();

        let req = row(&agent, "tool/request");
        let arg_content = req
            .get("args")
            .and_then(|a| a.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string();
        assert!(
            !arg_content.contains(token),
            "tool/request 的 args 里还剩原文：{}",
            arg_content
        );
        assert!(arg_content.contains("[REDACTED"), "要看得出抹过：{}", arg_content);
        assert!(
            req.num_at("redacted").unwrap_or(0.0) >= 1.0,
            "参数侧的抹除同样要计数：{:?}",
            req
        );

        // `tool/start` 里还有第二份 args。这一条是被"扫整份日志文件"抓出来的：
        // 逐个字段想「哪里可能有原文」想不全，落账点收成一个门、验证扫全文。
        let start = row(&agent, "tool/start");
        let started = start
            .get("args")
            .and_then(|a| a.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string();
        assert!(
            !started.contains(token),
            "tool/start 的那份 args 也要过手：{}",
            started
        );

        let resp = row(&agent, "model/response");
        let sent = resp
            .arr_at("toolCalls")
            .and_then(|c| c.first())
            .and_then(|c| c.get("args"))
            .and_then(|a| a.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string();
        assert!(
            !sent.contains(token),
            "model/response 里的 toolCalls 也是原文的落点：{}",
            sent
        );

        // 整份日志扫一遍：这一档要挡的是「导出里带凭据」
        assert!(
            !journal_text(&ws).contains(token),
            "日志文件里不该有原文凭据"
        );
        // 执行没被改动：写进文件的是模型请求的那段原文。日志管记录，
        // 管不了执行——真要挡写入是策略与隐私清单的活（Phase 1-B）。
        assert!(std::fs::read_to_string(ws.join("out/notes.txt"))
            .unwrap()
            .contains(token));
    }

    /// 第二档：来源命中隐私清单时，整段原文不进日志，只留字数、指纹和
    /// 指回 0600 落盘文件的指针。
    ///
    /// 走 `shell` 而不是 `fs_read`：`cat secrets/x` 是读取侧唯一挡不住的那条路
    /// （Landlock 只限制写），也正是这一档存在的理由。也因此只有真子进程能验
    /// 这条链路——判定逻辑本身的跨平台覆盖在
    /// `source_judgement_reads_effects_not_prose`。
    #[test]
    #[cfg(unix)]
    fn output_from_a_private_source_leaves_only_a_pointer() {
        let canary = "MUMBLE-CANARY-9182hqfp\n";
        let script = one_call(
            "shell",
            Json::obj(vec![("command", Json::from("cat secrets/api.txt"))]),
        );
        let (mut agent, ws) = disk_agent("redact-source", &script);
        std::fs::create_dir_all(ws.join("secrets")).unwrap();
        std::fs::write(ws.join("secrets/api.txt"), canary).unwrap();
        // shell 的效果是 proc.exec：默认要问人，这里代答「允许这一次」
        let mut hooks = Recorder {
            answer: ApprovalResponse::allow_once(),
            asks: Vec::new(),
        };
        agent.run_turn("把私密目录里的东西打出来", &mut hooks).unwrap();

        let out = row(&agent, "tool/output");
        assert_eq!(
            out.str_at("withheld"),
            Some("secrets"),
            "按来源扣留：{:?}",
            out
        );
        assert_eq!(out.str_at("text"), Some(""), "正文不进日志");
        assert_eq!(out.num_at("chars"), Some(canary.chars().count() as f64));
        let digest = out.str_at("sha256").unwrap_or_default().to_string();
        let spill_rel = out.str_at("file").unwrap_or_default().to_string();
        assert!(
            spill_rel.starts_with(".ui/tool-private/") && spill_rel.ends_with("-shell.txt"),
            "按清单扣留的原文要落在只给人看的那个目录：{}",
            spill_rel
        );
        assert!(
            !ws.join(".ui/tool-output").join(
                std::path::Path::new(&spill_rel)
                    .file_name()
                    .unwrap_or_default()
            ).exists(),
            "同一份正文不该在模型读得到的目录里再留一份"
        );
        let spill = std::fs::read_to_string(ws.join(&spill_rel)).unwrap();
        assert!(spill.contains(canary.trim_end()), "原文只留在落盘文件里");
        assert_eq!(
            digest,
            crate::sha256::hex(spill.as_bytes()),
            "日志里的指纹要对得上落盘的原文，否则复盘时无从校验"
        );
        assert!(
            !journal_text(&ws).contains(canary.trim_end()),
            "被扣留的正文不能从日志里读回来"
        );
        // 落盘的那一份走的是 0600：权限再是默认的 0644，就等于把「原文不出盘」
        // 换成「同机任何用户都能读」。
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(ws.join(&spill_rel)).unwrap().permissions().mode() & 0o777,
            0o600,
            "落盘的那一份只能本机用户可读"
        );
        // 模型那一份照旧是原文（它被批准执行了这条命令）
        assert!(agent.history.iter().any(|m| m.content.contains(canary.trim_end())));

        // 关掉模式脱敏不会关掉按来源扣留：后者看的是「从哪来」，与正则无关
        let (mut off, ws2) = disk_agent("redact-source-off", &script);
        std::fs::create_dir_all(ws2.join("secrets")).unwrap();
        std::fs::write(ws2.join("secrets/api.txt"), canary).unwrap();
        off.set_redact_output(false);
        let mut hooks = Recorder {
            answer: ApprovalResponse::allow_once(),
            asks: Vec::new(),
        };
        off.run_turn("再打一次", &mut hooks).unwrap();
        let out = row(&off, "tool/output");
        assert_eq!(out.str_at("withheld"), Some("secrets"), "扣留这一档不受 --no-redact 影响");
        assert_eq!(out.str_at("text"), Some(""));
        assert_eq!(out.num_at("redacted"), None, "没跑正则就不该报抹除计数");

        // 两档同时命中时都要留痕：一条既来自隐私清单、又长得像凭据的输出，
        // 日志里应看到 withheld（正文没了）+ redacted（正则确实动过手）。
        let key = "AKIAIOSFODNN7EXAMPLE"; // 测试夹具：密钥形状，非真凭据（allowlist secret）
        let keyed = one_call(
            "shell",
            Json::obj(vec![("command", Json::from("cat secrets/aws.txt"))]),
        );
        let (mut both, ws3) = disk_agent("redact-source-key", &keyed);
        std::fs::create_dir_all(ws3.join("secrets")).unwrap();
        std::fs::write(ws3.join("secrets/aws.txt"), format!("{}\n", key)).unwrap();
        let mut hooks = Recorder {
            answer: ApprovalResponse::allow_once(),
            asks: Vec::new(),
        };
        both.run_turn("读一份真凭据", &mut hooks).unwrap();
        let out = row(&both, "tool/output");
        assert_eq!(out.str_at("withheld"), Some("secrets"));
        assert_eq!(out.str_at("text"), Some(""));
        assert!(out.num_at("redacted").unwrap_or(0.0) >= 1.0, "计数要说正则也动过手");
        assert!(!journal_text(&ws3).contains(key), "两档叠起来的结果是日志里两头都没有");
        let spill = std::fs::read_to_string(
            ws3.join(out.str_at("file").unwrap_or_default()),
        )
        .unwrap();
        assert!(spill.contains(key), "原文只剩 0600 那一份");
    }

    /// 被扣留的那一次调用，不在模型可读的目录留下第二份原文。
    ///
    /// 截断缓存（`.ui/tool-output/`）是隐私清单上唯一开了读例外的地方。长输出
    /// 如果按来源扣留了，却又因为"要截断"在那儿再落一份，那刚抹掉的正文就换了
    /// 个位置原样躺着——两档各干各的，合起来什么都没挡住。
    #[test]
    #[cfg(unix)]
    fn a_withheld_call_does_not_leave_a_readable_copy() {
        let secret = "LONG-CANARY-77e1bb\n";
        let script = one_call(
            "shell",
            Json::obj(vec![("command", Json::from("cat secrets/big.txt"))]),
        );
        let (mut agent, ws) = disk_agent("no-double-copy", &script);
        std::fs::create_dir_all(ws.join("secrets")).unwrap();
        let mut body = String::new();
        for i in 0..80 {
            body.push_str(&format!("row {} {}\n", i, secret));
        }
        std::fs::write(ws.join("secrets/big.txt"), &body).unwrap();
        // 超过这一档才会走"截断 + 落盘"那条路（CLI 默认 0=不截断）
        agent.tool_chars = 200;
        let mut hooks = Recorder {
            answer: ApprovalResponse::allow_once(),
            asks: Vec::new(),
        };
        agent.run_turn("把大文件打出来", &mut hooks).unwrap();
        let out = row(&agent, "tool/output");
        assert_eq!(
            out.str_at("withheld"),
            Some("secrets"),
            "先确认这一条真被扣留：{:?}",
            out
        );
        let readable = std::fs::read_dir(ws.join(".ui/tool-output"))
            .map(|rd| rd.count())
            .unwrap_or(0);
        assert_eq!(readable, 0, "模型可读的目录里不该有这一次的任何副本");
        let held = std::fs::read_dir(ws.join(".ui/tool-private"))
            .map(|rd| rd.count())
            .unwrap_or(0);
        assert_eq!(held, 1, "全文只留扣留档那一份，别多也别少");
        assert!(
            out.str_at("file")
                .unwrap_or_default()
                .starts_with(".ui/tool-private/"),
            "指针要指到那份唯一的原文：{:?}",
            out.str_at("file")
        );
    }

    /// 扣留原文留下的那个指针，不能是模型回捞正文的通行证。
    ///
    /// 落账时写一句"全文在 X"，模型下一轮 `fs_read X` 就能把正文原样捞回来，
    /// 第二层隐私塌成一层。所以按清单扣留的内容落 `.ui/tool-private/`
    /// （隐私清单上唯一开读例外的是 `.ui/tool-output/`，那里只放截断缓存），
    /// 这一条把两个目录的**差别**钉住：模型能不能自己读回来。
    #[test]
    #[cfg(unix)]
    fn a_withheld_pointer_is_not_a_way_back_in() {
        let canary = "SILENT-CANARY-4471kq\n";
        let script = one_call(
            "shell",
            Json::obj(vec![("command", Json::from("cat secrets/api.txt"))]),
        );
        let (mut agent, ws) = disk_agent("pointer-locked", &script);
        std::fs::create_dir_all(ws.join("secrets")).unwrap();
        std::fs::write(ws.join("secrets/api.txt"), canary).unwrap();
        let mut hooks = Recorder {
            answer: ApprovalResponse::allow_once(),
            asks: Vec::new(),
        };
        agent.run_turn("打出来", &mut hooks).unwrap();
        let rel = row(&agent, "tool/output")
            .str_at("file")
            .unwrap_or_default()
            .to_string();
        assert!(rel.starts_with(".ui/tool-private/"), "落点：{}", rel);
        // 两个目录唯一的实质差别就是这一句
        assert!(
            !crate::protected::is_allowed_read(&ws, &ws.join(&rel)),
            "扣留目录不能开读例外"
        );
        assert!(
            crate::protected::is_allowed_read(&ws, &ws.join(".ui/tool-output/0001-deadbeef-9-shell.txt")),
            "截断缓存要能按需读回，否则截断等于丢数据"
        );

        // 端到端：下一轮真去读那个指针，拿回来的必须是拒绝，不是正文
        let name = rel.rsplit('/').next().unwrap_or("").to_string();
        let back = one_call("fs_read", Json::obj(vec![("path", Json::from(rel.as_str()))]));
        let (mut agent2, ws2) = disk_agent("pointer-locked-again", &back);
        std::fs::create_dir_all(ws2.join(".ui/tool-private")).unwrap();
        std::fs::write(ws2.join(&rel), canary).unwrap();
        agent2.run_turn("把上次那份全文再读回来", &mut NullHooks).unwrap();
        let said: String = agent2
            .history
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !said.contains(canary.trim_end()),
            "指针不该变成回捞正文的通道：{}",
            said
        );
        assert!(said.contains(&name), "要说清是**被挡**，不是「这里没东西」：{}", said);
    }

    /// 来源判定本身（不跑子进程，任何平台都能验）：判据是**效果目标**，
    /// 不是参数原文，也不是执行结果——它必须在工具跑之前就说得清
    /// 「这次会不会碰到隐私路径」。
    #[test]
    fn source_judgement_reads_effects_not_prose() {
        let script = one_call("fs_read", Json::obj(vec![("path", Json::from("a.txt"))]));
        let (agent, _) = disk_agent("redact-source-judge", &script);
        let call = |name: &str, args: Json| crate::model::ToolCall {
            id: "c".into(),
            name: name.into(),
            args,
            parse_error: None,
            raw: None,
        };
        let plain = agent.private_source(&call(
            "fs_write",
            Json::obj(vec![
                ("path", Json::from("out/copy.txt")),
                ("content", Json::from("一段讲到 secrets 的散文，不是路径")),
            ]),
        ));
        assert!(plain.is_none(), "写到普通路径：正文照旧落账，散文里的词不算：{:?}", plain);
        let judged = agent.private_source(&call(
            "fs_write",
            Json::obj(vec![
                ("path", Json::from("secrets/copy.txt")),
                ("content", Json::from("x")),
            ]),
        ));
        assert_eq!(judged.as_deref(), Some("secrets"), "目标在清单上就按来源扣留");
        let execed = agent.private_source(&call(
            "shell",
            Json::obj(vec![("command", Json::from("cat secrets/api.txt"))]),
        ));
        assert_eq!(execed.as_deref(), Some("secrets"), "shell 的效果目标是整条命令串");
        // 重读截断缓存：模型本来就能读那个目录，算成隐私来源只会换来每读一次
        // 复制一份、并把诊断信息从日志里抹掉
        let reread = agent.private_source(&call(
            "fs_read",
            Json::obj(vec![("path", Json::from(".ui/tool-output/0001-shell.txt"))]),
        ));
        assert_eq!(reread, None, "读模型本来就能读的地方不构成隐私来源");
        // 豁免只有那一条：同层的另一个目录放的正是"日志里连片段都别留"的内容
        let grab = agent.private_source(&call(
            "fs_read",
            Json::obj(vec![("path", Json::from(".ui/tool-private/0001-shell.txt"))]),
        ));
        assert_eq!(grab.as_deref(), Some(".ui"), "例外不包括 tool-private");
        let log = agent.private_source(&call(
            "fs_read",
            Json::obj(vec![("path", Json::from(".ui/journal.jsonl"))]),
        ));
        assert_eq!(log.as_deref(), Some(".ui"), "日志自己不是豁免区");
    }

    /// `--no-redact` 是给「日志就是我调试要看的原文」留的后门，但关掉的代价
    /// 必须留下记录：日后看到日志里有明文，得能回答「谁允许的」。
    #[test]
    fn turning_redaction_off_leaves_plaintext_and_says_so() {
        let key = "AKIAIOSFODNN7EXAMPLE"; // 测试夹具：密钥形状，非真凭据（allowlist secret）
        let script = one_call(
            "fs_read",
            Json::obj(vec![("path", Json::from("notes.txt"))]),
        );
        let (mut agent, ws) = disk_agent("redact-off", &script);
        std::fs::write(ws.join("notes.txt"), format!("备份用 {} 结束\n", key)).unwrap();
        // 照 CLI 的顺序来：先划 run 边界，再应用 --no-redact。
        // 记号如果挂在 begin_run 上就会漏（实测漏过一轮），挂在落账前就不会。
        agent.begin_run();
        agent.set_redact_output(false);
        agent.run_turn("读笔记", &mut NullHooks).unwrap();

        let out = row(&agent, "tool/output");
        assert!(
            out.str_at("text").unwrap_or_default().contains(key),
            "关掉了就该看到原文"
        );
        assert_eq!(out.num_at("redacted"), None);
        let off = rows(&agent, "privacy/redact-off");
        assert_eq!(
            off.len(),
            1,
            "这个选择本身要入账，一轮一次就够"
        );
        assert!(
            off[0]
                .str_at("reason")
                .unwrap_or_default()
                .contains("no-redact"),
            "要说清是被哪个开关关的：{:?}",
            off[0]
        );
        assert!(journal_text(&ws).contains(key), "开了后门日志里就该有原文");
        // 顺序也要成立：记号必须出现在第一段原文**之前**，否则翻日志的人
        // 是在读到明文之后才知道这里不该有明文。
        let off_seq = off[0].u64_at("seq").unwrap_or(0);
        let first_plain = row(&agent, "tool/output").u64_at("seq").unwrap_or(0);
        assert!(
            off_seq < first_plain,
            "记号要赶在原文前面：{} vs {}",
            off_seq,
            first_plain
        );

        // 默认那一档：不写这条事件——它是异常状态的记号，不是每轮的装饰
        let (mut on, ws2) = disk_agent("redact-on", &script);
        std::fs::write(ws2.join("notes.txt"), format!("备份用 {} 结束\n", key)).unwrap();
        on.begin_run();
        on.run_turn("读笔记", &mut NullHooks).unwrap();
        assert!(
            rows(&on, "privacy/redact-off").is_empty(),
            "没关就不要写这条"
        );
        assert!(!journal_text(&ws2).contains(key));
    }

    /// 人自己打的那句话，就算没被执行，也不该原样进日志。
    ///
    /// 复现过的真实路径：网页端在 worker 忙时收进队列、随后被「撤销」的消息，
    /// 由 `ui-cli/src/server.rs` 直接落账（当时绕过了门），实测 AKIA… 明文进盘。
    /// 这条测试用同一个 kind、同一道门，盯住"正文来源不是模型输出"这一类。
    #[test]
    fn a_cancelled_message_still_passes_the_door() {
        let key = "AKIAIOSFODNN7EXAMPLE"; // 测试夹具：密钥形状，非真凭据（allowlist secret）
        let empty = Json::obj(vec![("steps", Json::Arr(Vec::new()))]);
        let (mut agent, ws) = disk_agent("skipped", &empty);
        agent.begin_run();
        // 和 server.rs 的 worker 一模一样：只走 log_event，不碰 journal.append
        agent
            .log_event(
                "turn/skipped",
                Json::obj(vec![
                    ("task", Json::from("t2")),
                    ("reason", Json::from("cancelled-while-queued")),
                    ("text", Json::from(format!("部署要用 {}，别贴进 issue", key))),
                ]),
            )
            .unwrap();
        let row = row(&agent, "turn/skipped");
        assert!(
            !row.str_at("text").unwrap_or_default().contains(key),
            "落账的那一份要抹干净：{:?}",
            row.str_at("text")
        );
        assert_eq!(row.u64_at("redacted"), Some(1), "并且要说清抹了几处");
        assert!(
            !journal_text(&ws).contains(key),
            "盘上的日志也不能有原文"
        );
    }
}
