//! Model adapters: the transport only. Parsing (and repairing) tool arguments is
//! kernel policy, not adapter policy — see `parse_arguments`.
//!
//! `ScriptedModel` replays a JSON scenario, which is what makes the whole loop
//! testable offline. `ExternalModel` pipes the snapshot to a child process and
//! reads one JSON response, which is the seam a TypeScript frontend (or a helper
//! that owns TLS) plugs into later.

use crate::json::{self, Json};
use crate::prompt::Snapshot;

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Json,
    pub parse_error: Option<String>,
    pub raw: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage_tokens: Option<u64>,
    /// 适配器判断「模型没按协议输出」时置位：内核据此要求重写，而不是把散文当初答案。
    pub protocol_error: Option<String>,
}

impl ModelResponse {
    pub fn text(content: impl Into<String>) -> ModelResponse {
        ModelResponse {
            content: content.into(),
            tool_calls: Vec::new(),
            usage_tokens: None,
            protocol_error: None,
        }
    }
}

pub trait Model {
    fn name(&self) -> &str;
    /// `repair` 携带上一次参数解析失败的原因；`cancel` 让适配器在用户取消时中止
    /// （外部进程适配器会杀掉子进程，而不是等它跑完）。
    fn respond(
        &mut self,
        snapshot: &Snapshot,
        repair: Option<&str>,
        cancel: &crate::cancel::Cancel,
    ) -> Result<ModelResponse, String>;

    /// 流式版 `respond`：`on_delta` 收到的是「用于展示的增量」，**不是**最终结果。
    ///
    /// 默认实现退化成一次性回调整段内容，所以 ScriptedModel 和测试里的假模型
    /// 不用改。适配器只在确认整段协议解析完之后才返回 `ModelResponse`。
    fn respond_stream(
        &mut self,
        snapshot: &Snapshot,
        repair: Option<&str>,
        cancel: &crate::cancel::Cancel,
        on_delta: &mut dyn FnMut(&str),
    ) -> Result<ModelResponse, String> {
        let response = self.respond(snapshot, repair, cancel)?;
        on_delta(&response.content);
        Ok(response)
    }
}

/// Parse a tool-call argument string. Deliberately strict.
///
/// The only tolerance left is for the two things that are *not* the argument
/// payload itself: an outer markdown code fence, and the empty string meaning
/// "no arguments". Any malformed JSON — including a truncated object from a
/// cut-off stream — is an `Err`. The kernel turns that into a bounded
/// `model/repair` round; see `agent::call_model`.
///
/// Do **not** add bracket completion or tail-dropping here again: for a
/// truncated `fs_write`/`patch` those turn "half a file" into a successful
/// write, and for `shell` they execute a command prefix that was never meant
/// to run. Silent degradation is the failure mode this project refuses.
pub fn parse_arguments(raw: &str) -> Result<Json, String> {
    let cleaned = strip_fences(raw);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return Ok(Json::Obj(Vec::new()));
    }
    json::parse(trimmed)
}

fn strip_fences(s: &str) -> String {
    let mut out = s.trim().to_string();
    if out.starts_with("```") {
        out = out
            .lines()
            .filter(|l| !l.trim_start().starts_with("```"))
            .collect::<Vec<_>>()
            .join("\n");
    }
    out
}

struct ScriptStep {
    content: String,
    calls: Vec<(String, Option<Json>, Option<String>)>,
    usage: Option<u64>,
}

pub struct ScriptedModel {
    name: String,
    steps: Vec<ScriptStep>,
    cursor: usize,
    /// Steps to use instead when the kernel asks for a repair.
    repairs: Vec<ScriptStep>,
    repair_cursor: usize,
    /// 把本轮快照原文当回复返回（测试用：能断言「模型到底看见了什么」）。
    echo_snapshot: bool,
}

impl ScriptedModel {
    pub fn from_json(scenario: &Json) -> Result<ScriptedModel, String> {
        let name = scenario
            .str_at("model")
            .unwrap_or("scripted")
            .to_string();
        let steps = scenario
            .arr_at("steps")
            .ok_or_else(|| "scenario 缺少 steps 数组".to_string())?
            .iter()
            .map(parse_step)
            .collect::<Result<Vec<_>, _>>()?;
        let repairs = scenario
            .arr_at("repairs")
            .map(|r| r.iter().map(parse_step).collect::<Result<Vec<_>, _>>())
            .unwrap_or_else(|| Ok(Vec::new()))?;
        Ok(ScriptedModel {
            name,
            steps,
            cursor: 0,
            repairs,
            repair_cursor: 0,
            echo_snapshot: scenario.bool_at("echoSnapshot").unwrap_or(false),
        })
    }
}

fn parse_step(v: &Json) -> Result<ScriptStep, String> {
    let content = v.str_at("content").unwrap_or("").to_string();
    let mut calls = Vec::new();
    if let Some(list) = v.arr_at("toolCalls") {
        for (i, call) in list.iter().enumerate() {
            let name = call
                .str_at("name")
                .ok_or_else(|| format!("第 {} 个工具调用缺少 name", i))?
                .to_string();
            let args = call.get("args").cloned();
            let raw = call.str_at("argsRaw").map(str::to_string);
            let raw = if args.is_some() {
                None
            } else {
                Some(raw.unwrap_or_else(|| "{}".to_string()))
            };
            calls.push((name, args, raw));
        }
    }
    Ok(ScriptStep {
        content,
        calls,
        usage: v.u64_at("usageTokens"),
    })
}

fn step_to_response(step: &ScriptStep) -> ModelResponse {
    let calls = step
        .calls
        .iter()
        .enumerate()
        .map(|(i, (name, args, raw))| match (args, raw) {
            (Some(a), _) => ToolCall {
                id: format!("call-{}", i + 1),
                name: name.clone(),
                args: a.clone(),
                parse_error: None,
                raw: None,
            },
            (None, Some(r)) => match parse_arguments(r) {
                Ok(a) => ToolCall {
                    id: format!("call-{}", i + 1),
                    name: name.clone(),
                    args: a,
                    parse_error: None,
                    raw: Some(r.clone()),
                },
                Err(e) => ToolCall {
                    id: format!("call-{}", i + 1),
                    name: name.clone(),
                    args: Json::Obj(Vec::new()),
                    parse_error: Some(e),
                    raw: Some(r.clone()),
                },
            },
            (None, None) => ToolCall {
                id: format!("call-{}", i + 1),
                name: name.clone(),
                args: Json::Obj(Vec::new()),
                parse_error: None,
                raw: None,
            },
        })
        .collect();
    ModelResponse {
        content: step.content.clone(),
        tool_calls: calls,
        usage_tokens: step.usage,
        protocol_error: None,
    }
}

impl Model for ScriptedModel {
    fn name(&self) -> &str {
        &self.name
    }

    fn respond(
        &mut self,
        snapshot: &Snapshot,
        repair: Option<&str>,
        _cancel: &crate::cancel::Cancel,
    ) -> Result<ModelResponse, String> {
        if self.echo_snapshot {
            // 测试用：回复就是模型看到的快照原文。没有协议问题（content 非空、无 toolCalls）。
            return Ok(ModelResponse::text(snapshot.text.clone()));
        }
        if repair.is_some() && self.repair_cursor < self.repairs.len() {
            let step = &self.repairs[self.repair_cursor];
            self.repair_cursor += 1;
            return Ok(step_to_response(step));
        }
        if self.cursor >= self.steps.len() {
            return Err(format!(
                "scenario exhausted after {} steps (repair={:?})",
                self.steps.len(),
                repair.is_some()
            ));
        }
        let step = &self.steps[self.cursor];
        self.cursor += 1;
        Ok(step_to_response(step))
    }
}

/// Pipes `{snapshot, repair}` to a child process on stdin and reads one JSON
/// response from stdout. The child owns transport details (TLS, retries, auth).
pub struct ExternalModel {
    name: String,
    command: String,
}

impl ExternalModel {
    pub fn new(name: &str, command: &str) -> ExternalModel {
        ExternalModel {
            name: name.to_string(),
            command: command.to_string(),
        }
    }

/// 把一条命令行拆成 argv：支持双引号包住带空格的参数。
/// 只在 Windows 的适配器启动路径用（`cmd /C` 的引号处理太容易出事）。
#[cfg(windows)]
fn split_command_line(cmd: &str) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut started = false;
    for ch in cmd.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if quoted {
        return None; // 引号没闭合，交给 shell 处理
    }
    if started {
        out.push(cur);
    }
    Some(out)
}

#[cfg(windows)]
fn needs_shell(cmd: &str) -> bool {
    cmd.contains('|') || cmd.contains('>') || cmd.contains('&') || cmd.contains('<')
}

/// 启动模型子进程并把快照写进它的 stdin；`respond` 与 `respond_stream` 共用。
    fn spawn_child(
        &self,
        snapshot: &Snapshot,
        repair: Option<&str>,
    ) -> Result<std::process::Child, String> {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let payload = Json::obj(vec![
            ("snapshotId", Json::from(snapshot.id.as_str())),
            ("prompt", Json::from(snapshot.text.as_str())),
            ("repair", repair.map(Json::from).unwrap_or(Json::Null)),
        ]);

        #[cfg(windows)]
        let mut cmd = {
            // Windows 的 cmd /C 对「以引号开头的命令」有去引号的历史行为，
            // 无论用 arg 还是 raw_arg 都可能把 "C:\...\node.exe" 变成非法命令（实机踩到两次）。
            // 适配器命令本质是「程序 + 参数」，直接拆成 argv 起进程最稳；
            // 只有真的带 shell 元字符（| > & 等）时才退回 cmd。
            match Self::split_command_line(&self.command) {
                Some(parts) if !parts.is_empty() && !Self::needs_shell(&self.command) => {
                    let mut c = Command::new(&parts[0]);
                    c.args(&parts[1..]);
                    c
                }
                _ => {
                    use std::os::windows::process::CommandExt;
                    let mut c = Command::new("cmd");
                    c.arg("/C").raw_arg(&self.command);
                    c
                }
            }
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = Command::new("sh");
            // 不用 -l：登录 shell 会先跑 profile，profile 里任何一句 echo 都会
            // 混进 stdout，把「一行 JSON」的协议弄坏（真实踩过：多了个 "nvm"）。
            c.arg("-c").arg(&self.command);
            c
        };

        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("启动模型进程失败: {}", e))?;
        {
            // 写完必须立刻关掉 stdin：适配器读到 EOF 才会开始干活
            let mut stdin = child.stdin.take().ok_or("无法写入模型进程 stdin")?;
            stdin
                .write_all(payload.to_compact().as_bytes())
                .map_err(|e| format!("写入模型进程失败: {}", e))?;
        }
        Ok(child)
    }
}

/// 把适配器最后的协议 JSON 翻成内核的 `ModelResponse`。
fn response_from_json(value: &Json) -> ModelResponse {
    let content = value.str_at("content").unwrap_or("").to_string();
    let mut calls = Vec::new();
    for (i, call) in value.arr_at("toolCalls").into_iter().flatten().enumerate() {
        let name = call.str_at("name").unwrap_or("").to_string();
        let (args, parse_error, raw) = match call.str_at("argsRaw") {
            Some(r) => match parse_arguments(r) {
                Ok(a) => (a, None, Some(r.to_string())),
                Err(e) => (Json::Obj(Vec::new()), Some(e), Some(r.to_string())),
            },
            None => (
                call.get("args").cloned().unwrap_or(Json::Obj(Vec::new())),
                None,
                None,
            ),
        };
        calls.push(ToolCall {
            id: call
                .str_at("id")
                .map(str::to_string)
                .unwrap_or_else(|| format!("call-{}", i + 1)),
            name,
            args,
            parse_error,
            raw,
        });
    }
    ModelResponse {
        content,
        tool_calls: calls,
        usage_tokens: value.u64_at("usageTokens"),
        protocol_error: value.str_at("protocolError").map(str::to_string),
    }
}

impl Model for ExternalModel {
    fn name(&self) -> &str {
        &self.name
    }

    fn respond(&mut self, snapshot: &Snapshot, repair: Option<&str>, cancel: &crate::cancel::Cancel) -> Result<ModelResponse, String> {
        let mut child = self.spawn_child(snapshot, repair)?;

        // 输出放到线程里读（避免管道写满死锁），主线程轮询子进程并检查取消
        let mut stdout = child.stdout.take().ok_or("拿不到模型进程 stdout")?;
        let mut stderr = child.stderr.take().ok_or("拿不到模型进程 stderr")?;
        let out_handle = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = std::io::Read::read_to_string(&mut stdout, &mut s);
            s
        });
        let err_handle = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
            s
        });

        let started = std::time::Instant::now();
        let status = loop {
            if cancel.is_cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                return Err("cancelled".to_string());
            }
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) => {
                    if started.elapsed().as_secs() > 180 {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err("模型进程超时（180s）".to_string());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(e) => return Err(format!("等待模型进程失败: {}", e)),
            }
        };

        let stdout_text = out_handle.join().unwrap_or_default();
        let stderr_text = err_handle.join().unwrap_or_default();
        if !status.success() {
            return Err(format!(
                "模型进程退出码 {:?}: {}",
                status.code(),
                stderr_text
            ));
        }
        let text = stdout_text;
        let value = json::parse(text.trim()).map_err(|e| {
            // 适配器输出不是 JSON 时，它自己的 stderr 才是答案；丢掉它等于蒙眼调试
            let hint: String = stderr_text.chars().take(400).collect();
            let head: String = text.chars().take(200).collect();
            format!(
                "模型输出不是 JSON: {}（stdout {} 字节，开头: {:?}，stderr: {}）",
                e,
                text.len(),
                head,
                if hint.trim().is_empty() { "空" } else { hint.trim() }
            )
        })?;
        Ok(response_from_json(&value))
    }

    fn respond_stream(
        &mut self,
        snapshot: &Snapshot,
        repair: Option<&str>,
        cancel: &crate::cancel::Cancel,
        on_delta: &mut dyn FnMut(&str),
    ) -> Result<ModelResponse, String> {
        let mut child = self.spawn_child(snapshot, repair)?;

        let stdout = child.stdout.take().ok_or("拿不到模型进程 stdout")?;
        let mut stderr = child.stderr.take().ok_or("拿不到模型进程 stderr")?;

        // 逐行读 stdout：每来一行就交给主线程，主线程再回调 on_delta。
        // 回调不能跨线程（`&mut dyn FnMut` 不是 Send），所以行通过 channel 回来。
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let out_handle = std::thread::spawn(move || {
            use std::io::BufRead;
            let reader = std::io::BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let err_handle = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
            s
        });

        let started = std::time::Instant::now();
        let mut final_value: Option<Json> = None;
        let mut raw = String::new();
        let mut reader_done = false;
        loop {
            if cancel.is_cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                return Err("cancelled".to_string());
            }
            if reader_done {
                // stdout 已 EOF，但进程可能还没退出（例如它自己还在收尾）：
                // 继续轮询而不是阻塞 wait，取消和超时在这段时间也必须有效。
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {
                        if started.elapsed().as_secs() > 180 {
                            let _ = child.kill();
                            let _ = child.wait();
                            return Err("模型进程超时（180s）".to_string());
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(e) => return Err(format!("等待模型进程失败: {}", e)),
                }
                continue;
            }
            match rx.recv_timeout(std::time::Duration::from_millis(25)) {
                Ok(line) => {
                    handle_stream_line(&line, on_delta, &mut final_value, &mut raw);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // 进程退出后子线程会读到 EOF，随后 channel 断开；在那之前
                    // 只超时才算失败（不能用 try_wait 提前退出，否则会漏掉尾巴）。
                    if started.elapsed().as_secs() > 180 {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err("模型进程超时（180s）".to_string());
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    reader_done = true;
                }
            }
        }
        let status = child
            .wait()
            .map_err(|e| format!("等待模型进程失败: {}", e))?;
        let stderr_text = err_handle.join().unwrap_or_default();
        let _ = out_handle.join();

        if !status.success() {
            return Err(format!(
                "模型进程退出码 {:?}: {}",
                status.code(),
                stderr_text
            ));
        }
        // 没有逐行协议时退回「整个 stdout 是一个 JSON 对象」的老格式：
        // 非流式适配器（或旧的自定义模型进程）仍然能用。
        let value = match final_value {
            Some(v) => v,
            None => json::parse(raw.trim()).map_err(|e| {
                let hint: String = stderr_text.chars().take(400).collect();
                let head: String = raw.chars().take(200).collect();
                format!(
                    "模型流没有给出最终结果: {}（stdout {} 字节，开头: {:?}，stderr: {}）",
                    e,
                    raw.len(),
                    head,
                    if hint.trim().is_empty() { "空" } else { hint.trim() }
                )
            })?,
        };
        Ok(response_from_json(&value))
    }
}

/// 处理一行流输出：`{"delta":...}` 只做展示回调；其余 JSON 视为最终结果。
fn handle_stream_line(
    line: &str,
    on_delta: &mut dyn FnMut(&str),
    final_value: &mut Option<Json>,
    raw: &mut String,
) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    raw.push_str(trimmed);
    raw.push('\n');
    if let Ok(value) = json::parse(trimmed) {
        if let Some(delta) = value.str_at("delta") {
            if !delta.is_empty() {
                on_delta(delta);
            }
        } else if value.get("content").is_some() || value.get("toolCalls").is_some() {
            // 只认协议对象；适配器偶尔多打印的日志行不能覆盖真正的结果。
            *final_value = Some(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_fenced_json() {
        assert_eq!(
            parse_arguments(r#"{"path":"a"}"#).unwrap().str_at("path"),
            Some("a")
        );
        assert_eq!(
            parse_arguments("```json\n{\"path\":\"b\"}\n```")
                .unwrap()
                .str_at("path"),
            Some("b")
        );
    }

    #[test]
    fn truncated_arguments_are_an_error_not_salvaged() {
        // 模型输出被截断（长度上限 / 网络中断）时，参数就是残缺的。旧实现会
        // 补上引号和 `}`，于是 fs_write 把半截文件当真写进去、shell 执行命令前缀。
        // 现在必须返回 Err，由内核走 model/repair，而不是静默降级。
        for raw in [
            // 缺收尾的引号和两个 `}`（旧实现补 `"` + `}` 后能解析）
            r#"{"path":"src/main.rs","content":"fn main() {"#,
            // 只缺一个 `}`（旧实现补一个 `}` 后能解析）
            r#"{"path":"a.txt","content":"no closing brace""#,
            // 旧实现还会从尾部逐字删到能凑出合法 JSON
            r#"{"path":"a.txt","content":"truncated tail"#,
        ] {
            assert!(
                parse_arguments(raw).is_err(),
                "截断的参数不能被补全成合法 JSON: {}",
                raw
            );
        }
    }

    #[test]
    fn reports_hopeless_arguments() {
        assert!(parse_arguments("not json at all").is_err());
    }

    fn test_snapshot() -> Snapshot {
        let ws = std::env::temp_dir().join(format!("ui-model-snapshot-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&ws);
        crate::prompt::build_snapshot(&ws, &[], &[], None, None, "", &[], "")
    }

    #[test]
    fn scripted_model_streams_its_whole_content_by_default() {
        // 默认 respond_stream 应退化成「一次回调整段内容」，现有实现因此不用改。
        let scenario = Json::obj(vec![
            ("model", Json::from("scripted")),
            (
                "steps",
                Json::Arr(vec![Json::obj(vec![("content", Json::from("整段内容"))])]),
            ),
        ]);
        let mut model = ScriptedModel::from_json(&scenario).unwrap();
        let snapshot = test_snapshot();
        let mut deltas: Vec<String> = Vec::new();
        let response = model
            .respond_stream(
                &snapshot,
                None,
                &crate::cancel::Cancel::new(),
                &mut |delta| deltas.push(delta.to_string()),
            )
            .unwrap();
        assert_eq!(response.content, "整段内容");
        assert_eq!(deltas, vec!["整段内容".to_string()]);
    }

    /// 写一个假「模型进程」：先读光 stdin，再按行吐协议。
    #[cfg(unix)]
    fn fake_model_script(name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ui-model-fake-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{}.sh", name));
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().to_string()
    }

    #[cfg(unix)]
    #[test]
    fn external_model_reads_delta_lines_then_the_final_result() {
        let command = fake_model_script(
            "stream",
            r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"delta":"你"}' '{"delta":"好"}' '{"content":"你好","toolCalls":[{"name":"fs_read","args":{"path":"a.txt"}}],"usageTokens":9}'
"#,
        );
        let mut model = ExternalModel::new("fake", &command);
        let snapshot = test_snapshot();
        let mut deltas: Vec<String> = Vec::new();
        let response = model
            .respond_stream(
                &snapshot,
                None,
                &crate::cancel::Cancel::new(),
                &mut |delta| deltas.push(delta.to_string()),
            )
            .unwrap();

        // delta 只用于展示：最终结果必须来自最后一行，而不是把增量拼起来。
        assert_eq!(deltas, vec!["你".to_string(), "好".to_string()]);
        assert_eq!(response.content, "你好");
        assert_eq!(response.usage_tokens, Some(9));
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "fs_read");
        assert_eq!(response.tool_calls[0].args.str_at("path"), Some("a.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn external_model_accepts_legacy_single_object_stdout() {
        // 老适配器（非流式）仍然只输出一个 JSON 对象、没有 delta 行。
        let command = fake_model_script(
            "legacy",
            r#"#!/bin/sh
cat >/dev/null
printf '%s' '{"content":"老格式","toolCalls":[]}'
"#,
        );
        let mut model = ExternalModel::new("legacy", &command);
        let snapshot = test_snapshot();
        let mut deltas: Vec<String> = Vec::new();
        let response = model
            .respond_stream(
                &snapshot,
                None,
                &crate::cancel::Cancel::new(),
                &mut |delta| deltas.push(delta.to_string()),
            )
            .unwrap();
        assert!(deltas.is_empty(), "老格式没有 delta 行");
        assert_eq!(response.content, "老格式");
    }

    #[cfg(unix)]
    #[test]
    fn external_model_streaming_still_honours_cancel() {
        // 吐出一个 delta 后就卡住；回调里置位取消，适配器必须立刻杀掉子进程。
        let command = fake_model_script(
            "cancel",
            r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"delta":"first"}'
exec sleep 30
"#,
        );
        let mut model = ExternalModel::new("cancel", &command);
        let snapshot = test_snapshot();
        let cancel = crate::cancel::Cancel::new();
        let cancel_in_callback = cancel.clone();
        let started = std::time::Instant::now();
        let mut deltas: Vec<String> = Vec::new();
        let result = model.respond_stream(&snapshot, None, &cancel, &mut |delta| {
            deltas.push(delta.to_string());
            cancel_in_callback.cancel();
        });
        assert_eq!(result.err().as_deref(), Some("cancelled"));
        assert_eq!(deltas, vec!["first".to_string()]);
        assert!(
            started.elapsed().as_secs() < 5,
            "取消后不能等子进程自己跑完"
        );
    }
}
