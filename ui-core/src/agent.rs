//! The loop: seven contracts in one place.
//!
//! Every step builds a snapshot, asks the model, and dispatches tool calls through
//! policy, budget and journal. A malformed tool argument is repaired (bounded),
//! never fatal. A started-but-unfinished effect is reconciled on resume instead of
//! being silently repeated or silently lost.

use crate::budget::{Budget, BudgetExceeded, Dim, Limits};
use crate::json::Json;
use crate::journal::{analyze_incomplete, Journal};
use crate::model::{Model, ToolCall};
use crate::policy::{Decision, Effect, Policy, Rule, Verdict};
use crate::cancel::Cancel;
use crate::compact::{compact, CompactPolicy};
use crate::prompt::{build_snapshot, Message, Snapshot};
use crate::tools::{find, registry, Ctx, Tool, ToolResult};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum AgentError {
    Budget(BudgetExceeded),
    Model(String),
    Journal(io::Error),
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::Budget(e) => write!(f, "{}", e),
            AgentError::Model(e) => write!(f, "模型调用失败: {}", e),
            AgentError::Journal(e) => write!(f, "日志写入失败: {}", e),
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

pub struct ApprovalRequest {
    pub call_id: String,
    pub tool: String,
    pub args: Json,
    pub effects: Vec<Effect>,
    pub reason: String,
}

pub trait Hooks {
    fn on_event(&mut self, _event: &Json) {}
    /// 模型正在逐字输出（用于展示的增量，不是最终结果；最终以 on_assistant 为准）。
    fn on_delta(&mut self, _text: &str) {}
    fn on_assistant(&mut self, _text: &str) {}
    fn on_tool(&mut self, _name: &str, _args: &Json, _result: &ToolResult) {}
    /// 账本变化：前端据此实时更新预算条，而不是等一轮结束才知道花了多少。
    fn on_budget(&mut self, _tokens: u64, _tool_calls: u64) {}
    /// 历史被折叠：前端应把它显示出来，不能让上下文悄悄变短。
    fn on_compact(&mut self, _before: u64, _after: u64, _dropped: usize) {}
    fn on_approval(&mut self, _req: &ApprovalRequest) -> Decision {
        Decision::Deny
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
    /// 跨轮对话历史：同一会话里多次 run_turn 共享它。
    pub history: Vec<Message>,
    /// 已加载的 skill 正文（会作为独立段落进入快照，可计价）
    pub loaded_skills: Vec<(String, String)>,
    /// 工具输出在上下文里最多保留多少字符（0 = 不截断）。超出部分落盘。
    pub tool_chars: usize,
    /// 运行模式：只调决策严格度，不改日志格式
    pub mode: crate::mode::Mode,
    /// ?? MCP ??(??)
    pub mcp: Option<std::sync::Arc<std::sync::Mutex<crate::mcp::Hub>>>,
    
    tool_seq: u64,
    turn: u64,
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
    pub fn new(
        journal: Journal,
        model: Box<dyn Model + Send>,
        workspace: PathBuf,
        limits: Limits,
    ) -> Agent {
        let policy = Policy::new(Policy::defaults(&workspace), &workspace);
        let turn = journal
            .events
            .iter()
            .filter(|e| e.str_at("kind") == Some("turn/start"))
            .count() as u64;
        Agent {
            journal,
            model,
            policy,
            budget: Budget::new(limits),
            tools: registry(),
            workspace,
            goal: None,
            max_steps: 24,
            max_repairs: 2,
            compact: CompactPolicy::default(),
            cancel: Cancel::new(),
            history: Vec::new(),
            loaded_skills: Vec::new(),
            tool_chars: 2000,
            mode: crate::mode::Mode::default(),
            mcp: None,
            tool_seq: 0,
            turn,
        }
    }

    /// 目标路径的**真实**位置是否跑出工作区（符号链接、挂载点）。    /// 词法检查（`..`）在 `Ctx::resolve` 里做，这一层补的是"字面在里、实际在外"。
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
            cancel: self.cancel.clone(),
            mcp: self.mcp.clone(),
        }
    }

    pub fn turn_number(&self) -> u64 {
        self.turn
    }

    /// 工具输出超过 `tool_chars` 时落盘，上下文里只留首尾 + 指针。
    ///
    /// 每个工具结果都会在后续每一步被重发一次，长输出是上下文成本的主要来源。
    /// 完整内容留在磁盘上，模型需要时可以 `fs_read` 重读——按需展开，而不是常驻。
    fn retain_tool_output(&mut self, tool: &str, full: &str) -> Result<String, AgentError> {
        let total = full.chars().count();
        if self.tool_chars == 0 || total <= self.tool_chars {
            return Ok(full.to_string());
        }
        self.tool_seq += 1;
        let dir = self.workspace.join(".ui").join("tool-output");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{:04}-{}.txt", self.tool_seq, tool));
        let _ = std::fs::write(&path, full);
        let rel = path
            .strip_prefix(&self.workspace)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        let head_n = self.tool_chars * 2 / 3;
        let tail_n = self.tool_chars / 3;
        let head: String = full.chars().take(head_n).collect();
        let tail: String = full.chars().skip(total - tail_n).collect();
        let kept = format!(
            "{}\n…[输出共 {} 字符，已截断；完整内容在 {}，需要时用 fs_read 读它]\n{}",
            head, total, rel, tail
        );

        self.journal.append(
            "tool/trim",
            Json::obj(vec![
                ("tool", Json::from(tool)),
                ("totalChars", Json::from(total)),
                ("keptChars", Json::from(kept.chars().count())),
                ("file", Json::from(rel.as_str())),
            ]),
        )?;
        Ok(kept)
    }

    /// 把 skill 正文装进上下文（工具加载与工作流预加载共用），并留痕。
    pub fn load_skill(&mut self, name: &str, body: &str, source: &str) -> Result<(), AgentError> {
        if self.loaded_skills.iter().any(|(n, _)| n == name) {
            return Ok(());
        }
        self.loaded_skills.push((name.to_string(), body.to_string()));
        self.journal.append(
            "skill/load",
            Json::obj(vec![
                ("name", Json::from(name)),
                ("bytes", Json::from(body.len())),
                ("source", Json::from(source)),
            ]),
        )?;
        Ok(())
    }

    pub fn run_turn(&mut self, input: &str, hooks: &mut dyn Hooks) -> Result<TurnReport, AgentError> {
        self.turn += 1;
        let turn = self.turn;
        self.cancel.reset();
        self.journal.append(
            "turn/start",
            Json::obj(vec![("turn", Json::from(turn)), ("mode", Json::from(self.mode.name())), ("prompt", Json::from(input))]),
        )?;

        self.history.push(Message::user(input));
        let mut steps = 0usize;
        let mut tool_calls = 0usize;
        let mut last_text = String::new();
        let mut empty_replies = 0usize;

        loop {
            if self.cancel.is_cancelled() {
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
                self.journal.append(
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
            );
            if let Err(e) = self.budget.charge(Dim::Tokens, snapshot.tokens as f64) {
                self.journal.append(
                    "turn/end",
                    Json::obj(vec![
                        ("turn", Json::from(turn)),
                        ("reason", Json::from("budget")),
                        ("error", Json::from(e.to_string())),
                    ]),
                )?;
                return Err(AgentError::Budget(e));
            }
            self.journal.append("prompt/snapshot", snapshot.to_json())?;
            hooks.on_budget(
                self.budget.used(Dim::Tokens) as u64,
                self.budget.used(Dim::ToolCalls) as u64,
            );

            let response = match self.call_model(&snapshot, hooks) {
                Ok(r) => r,
                Err(AgentError::Model(e)) if self.cancel.is_cancelled() => {
                    self.journal.append(
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
                Err(e) => return Err(e),
            };
            if self.cancel.is_cancelled() {
                return self.finish(turn, "cancelled", steps, tool_calls, last_text);
            }

            self.journal.append(
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
                self.budget.reconcile_tokens(snapshot.tokens, usage);
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
                self.journal.append(
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
                if self.cancel.is_cancelled() {
                    return self.finish(turn, "cancelled", steps, tool_calls, last_text);
                }
                let (result, name) = self.dispatch(call, hooks)?;
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
                let kept = self.retain_tool_output(&name, full)?;
                self.history.push(Message::tool(&name, result.ok, &kept));
            }

            steps += 1;
        }
    }

    fn call_model(
        &mut self,
        snapshot: &Snapshot,
        hooks: &mut dyn Hooks,
    ) -> Result<crate::model::ModelResponse, AgentError> {
        let mut repair_note: Option<String> = None;
        let mut attempts = 0usize;
        loop {
            let response = match self.model.respond_stream(
                snapshot,
                repair_note.as_deref(),
                &self.cancel,
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

            let broken: Vec<&ToolCall> = response
                .tool_calls
                .iter()
                .filter(|c| c.parse_error.is_some())
                .collect();

            // 适配器发现「整条回复不是协议 JSON」时，和参数坏掉同等对待：
            // 先要求重写，而不是把散文当成最终答案。
            if let Some(err) = response.protocol_error.clone() {
                attempts += 1;
                self.journal.append(
                    "model/repair",
                    Json::obj(vec![
                        ("attempt", Json::from(attempts)),
                        ("tool", Json::from("(protocol)")),
                        ("error", Json::from(err.as_str())),
                        ("raw", Json::from(response.content.as_str())),
                    ]),
                )?;
                if attempts > self.max_repairs {
                    return Ok(response);
                }
                let note = format!(
                    "你上一条输出不是约定的 JSON 对象（{}）。请只输出一个 JSON 对象：\
                     {{\"content\": \"给用户看的话\", \"toolCalls\": [{{\"name\": \"工具名\", \"args\": {{}}}}]}}。",
                    err
                );
                self.history.push(Message::note(note.clone()));
                repair_note = Some(note);
                continue;
            }

            if broken.is_empty() {
                return Ok(response);
            }

            let error = broken[0].parse_error.clone().unwrap_or_default();
            attempts += 1;
            self.journal.append(
                "model/repair",
                Json::obj(vec![
                    ("attempt", Json::from(attempts)),
                    ("tool", Json::from(broken[0].name.as_str())),
                    ("error", Json::from(error.as_str())),
                    ("raw", Json::from(broken[0].raw.clone().unwrap_or_default())),
                ]),
            )?;

            if attempts > self.max_repairs {
                // Keep going: the structured failure becomes a tool result the
                // model can read next step, instead of killing the turn.
                return Ok(response);
            }
            let note = format!(
                "上一个工具调用的参数不是合法 JSON（{}）。请重新输出完整、合法的 JSON 参数，不要截断。",
                error
            );
            self.history.push(Message::note(note.clone()));
            repair_note = Some(note);
        }
    }

    fn dispatch(
        &mut self,
        call: &ToolCall,
        hooks: &mut dyn Hooks,
    ) -> Result<(ToolResult, String), AgentError> {
        let name = call.name.clone();

        if let Some(err) = &call.parse_error {
            let result = ToolResult::err(format!("BAD_JSON: {}", err));
            self.journal.append(
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

        let tool = match find(&self.tools, &name) {
            Some(t) => t,
            None => {
                let result = ToolResult::err(format!("未知工具: {}", name));
                self.journal.append(
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
        let effects = (tool.effects)(&call.args, &ctx);
        // 词法上在 workspace 里，不代表真实位置也在：符号链接可以指向外面。
        // 策略只看字面路径，所以在这层补一道「真实路径」检查。
        // 模式只调严格度，不改日志格式：所以 quiet 跑出来的这一轮，
        // 事后可以用 strict 重判（见 `ui audit --as`）。
        let mut verdict = self.policy.decide(&effects);
        let base = verdict.decision;
        let adjusted = effects
            .iter()
            .map(|e| self.mode.adjust(&e.kind, base))
            .fold(Decision::Allow, |acc, d| if d.severity() > acc.severity() { d } else { acc });
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
        if verdict.decision == Decision::Allow {
            if let Some(target) = effects.iter().find(|e| e.kind.starts_with("fs.")) {
                if self.escapes_workspace(&target.target) {
                    let reason = format!(
                        "{} 解析后的真实路径在工作区之外（符号链接）",
                        target.target
                    );
                    verdict = Verdict {
                        decision: Decision::Deny,
                        reasons: vec![(target.clone(), Decision::Deny, reason.clone())],
                        reason,
                    };
                }
            }
        }
        self.journal.append(
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
            self.journal.append(
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
            let request = ApprovalRequest {
                call_id: call.id.clone(),
                tool: name.clone(),
                args: call.args.clone(),
                effects: effects.clone(),
                reason: verdict.reason.clone(),
            };
            let answer = hooks.on_approval(&request);
            self.journal.append(
                "approval/answered",
                Json::obj(vec![
                    ("callId", Json::from(call.id.as_str())),
                    ("tool", Json::from(name.as_str())),
                    ("verdict", Json::from(answer.name())),
                ]),
            )?;
            match answer {
                Decision::Allow => {
                    // "allow always": turn the approval into a durable-looking rule
                    // for this session, per effect.
                    for effect in &effects {
                        let rule = Rule::new(&effect.kind, &effect.target, Decision::Allow)
                            .with_note("本次会话中批准");
                        self.policy.add_rule(rule);
                    }
                    allowed = true;
                }
                Decision::Ask => {
                    // The UI did not decide (headless): fail closed, but say why.
                    let result = ToolResult::err("需要人工批准，但当前没有可用的审批通道");
                    self.journal.append(
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
                Decision::Deny => allowed = false,
            }
        }

        if !allowed {
            let result = ToolResult::err("用户拒绝了这次调用");
            self.journal.append(
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
            let result = ToolResult::err(e.to_string());
            self.journal.append(
                "tool/end",
                Json::obj(vec![
                    ("callId", Json::from(call.id.as_str())),
                    ("tool", Json::from(name.as_str())),
                    ("ok", Json::Bool(false)),
                    ("error", Json::from("BUDGET")),
                ]),
            )?;
            return Ok((result, name));
        }

        self.journal.append(
            "tool/start",
            Json::obj(vec![
                ("callId", Json::from(call.id.as_str())),
                ("tool", Json::from(name.as_str())),
                ("args", call.args.clone()),
                ("idempotent", Json::Bool(tool.idempotent)),
            ]),
        )?;
        // 输入血缘：读文件也要留指纹。否则「结果可复现」只做了一半——
        // 数据换了版本，产物哈希却还是对的。
        record_reads(&mut self.journal, &self.workspace, &name, &call.id, &effects)?;
        let result = (tool.run)(&call.args, &ctx);
        hooks.on_budget(
            self.budget.used(Dim::Tokens) as u64,
            self.budget.used(Dim::ToolCalls) as u64,
        );
        self.journal.append(
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
        self.journal.append(
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
    let incomplete = analyze_incomplete(&agent.journal.events);
    let mut reconciled = Vec::new();
    let mut needs_human = Vec::new();

    for event in &incomplete.open_tools {
        let call_id = event.str_at("callId").unwrap_or("?").to_string();
        let tool_name = event.str_at("tool").unwrap_or("?").to_string();
        let args = event.get("args").cloned().unwrap_or(Json::Obj(Vec::new()));

        match find(&agent.tools, &tool_name) {
            None => {
                agent.journal.append(
                    "effect/unknown",
                    Json::obj(vec![
                        ("callId", Json::from(call_id.as_str())),
                        ("tool", Json::from(tool_name.as_str())),
                        ("reason", Json::from("工具已不存在，无法判断是否生效")),
                    ]),
                )?;
                needs_human.push(format!("{}: 工具 {} 已不存在", call_id, tool_name));
            }
            Some(tool) if tool.idempotent => {
                let ctx = Ctx {
                    workspace: agent.workspace.clone(),
                    cancel: agent.cancel.clone(),
                    mcp: agent.mcp.clone(),
                };
                let result = (tool.run)(&args, &ctx);
                agent.journal.append(
                    "effect/rerun",
                    Json::obj(vec![
                        ("callId", Json::from(call_id.as_str())),
                        ("tool", Json::from(tool_name.as_str())),
                        ("reason", Json::from("可重放，安全重跑")),
                    ]),
                )?;
                agent.journal.append(
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
            Some(tool) => {
                let effects = (tool.effects)(
                    &args,
                    &Ctx {
                        workspace: agent.workspace.clone(),
                        cancel: agent.cancel.clone(),
                        mcp: agent.mcp.clone(),
                    },
                );
                let request = ApprovalRequest {
                    call_id: call_id.clone(),
                    tool: tool_name.clone(),
                    args: args.clone(),
                    effects,
                    reason: "崩溃中断：这个调用可能已经生效，重放会造成二次副作用".to_string(),
                };
                let answer = hooks.on_approval(&request);
                agent.journal.append(
                    "approval/answered",
                    Json::obj(vec![
                        ("callId", Json::from(call_id.as_str())),
                        ("tool", Json::from(tool_name.as_str())),
                        ("verdict", Json::from(answer.name())),
                        ("context", Json::from("resume")),
                    ]),
                )?;
                if answer == Decision::Allow {
                    let result = (tool.run)(
                        &args,
                        &Ctx {
                            workspace: agent.workspace.clone(),
                            cancel: agent.cancel.clone(),
                            mcp: agent.mcp.clone(),
                        },
                    );
                    agent.journal.append(
                        "effect/rerun",
                        Json::obj(vec![
                            ("callId", Json::from(call_id.as_str())),
                            ("tool", Json::from(tool_name.as_str())),
                            ("reason", Json::from("人工允许重放")),
                        ]),
                    )?;
                    agent.journal.append(
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
                    agent.journal.append(
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

    if let Some(turn) = &incomplete.open_turn {
        let n = turn.u64_at("turn").unwrap_or(0);
        agent.journal.append(
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
        };
        let mut agent = Agent::new(journal, model(&script), ws, limits);
        let mut hooks = NullHooks;
        let report = agent.run_turn("循环", &mut hooks).unwrap();

        // Calls past the limit never run; they come back as structured errors so
        // the model can see the wall instead of the harness dying.
        let denied = agent
            .journal
            .events
            .iter()
            .filter(|e| e.str_at("error") == Some("BUDGET"))
            .count();
        assert_eq!(denied, 8);
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
        assert_eq!(report.reason, "done");
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
}
