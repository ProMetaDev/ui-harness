//! MCP 客户端：把外部 MCP server 的工具接进内核。
//!
//! 设计取舍：不把外部工具伪装成内置工具，而是给模型一个 `mcp_call` 工具，
//! 参数是 `{server, tool, args}`。这样：
//! * 策略只需要一条规则（`proc.exec mcp:<server>:<tool>`），默认要审批；
//! * 日志里留下的是"调了哪个服务器的哪个工具"，审计读得懂；
//! * 不需要动静态的工具注册表。

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{sync_channel, Receiver};
use std::time::Duration;

use crate::json::{self, Json};

pub struct ToolInfo {
    pub name: String,
    pub description: String,
}

struct Client {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    next_id: u64,
    #[cfg(windows)]
    job: Option<crate::winsandbox::Job>,
}

impl Client {
    fn read_line(rx: &Receiver<String>, timeout: Duration) -> Result<String, String> {
        match rx.recv_timeout(timeout) {
            Ok(line) => Ok(line),
            Err(_) => Err("MCP server 超时未响应".to_string()),
        }
    }

    fn request(&mut self, method: &str, params: Json) -> Result<Json, String> {
        self.next_id += 1;
        let id = self.next_id;
        let msg = Json::obj(vec![
            ("jsonrpc", Json::from("2.0")),
            ("id", Json::from(id)),
            ("method", Json::from(method)),
            ("params", params),
        ]);
        writeln!(self.stdin, "{}", msg.to_compact()).map_err(|e| format!("写 MCP 失败: {}", e))?;
        self.stdin.flush().map_err(|e| e.to_string())?;
        loop {
            let line = match Self::read_line(&self.rx, Duration::from_secs(20)) {
                Ok(line) => line,
                Err(e) => {
                    // 超时后不能留一个永远挂着的 MCP 子进程；杀掉，下一次
                    // 调用会明确失败，而不是悄悄复用半死连接。
                    #[cfg(windows)]
                    if let Some(job) = &self.job {
                        job.terminate();
                    }
                    let pid = self.child.id();
                    let _ = self.child.kill();
                    crate::exec::kill_tree(pid);
                    let _ = self.child.wait();
                    return Err(e);
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            let value = match json::parse(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if value.u64_at("id") != Some(id) {
                continue; // 通知或别的响应，跳过
            }
            if let Some(err) = value.get("error") {
                return Err(format!(
                    "MCP 报错: {}",
                    err.str_at("message").unwrap_or("未知错误")
                ));
            }
            return Ok(value.get("result").cloned().unwrap_or(Json::Null));
        }
    }

    fn notify(&mut self, method: &str, params: Json) -> Result<(), String> {
        let msg = Json::obj(vec![
            ("jsonrpc", Json::from("2.0")),
            ("method", Json::from(method)),
            ("params", params),
        ]);
        writeln!(self.stdin, "{}", msg.to_compact()).map_err(|e| e.to_string())?;
        self.stdin.flush().map_err(|e| e.to_string())
    }
}

pub struct Server {
    pub name: String,
    client: Client,
    pub tools: Vec<ToolInfo>,
}

pub struct Hub {
    pub servers: Vec<Server>,
}

impl Hub {
    /// 按配置拉起 server 并握手。任何一个失败都返回错误——静默降级会让人以为工具在，其实不在。
    pub fn start(
        config: &Json,
        workspace: &std::path::Path,
        sandbox: &crate::sandbox::Config,
    ) -> Result<Hub, String> {
        let list = config.arr_at("servers").cloned().unwrap_or_default();
        if list.is_empty() {
            return Err("MCP 配置里没有 servers".to_string());
        }
        let mut servers = Vec::new();
        for item in list {
            let name = item.str_at("name").unwrap_or("mcp").to_string();
            let argv: Vec<String> = item
                .arr_at("cmd")
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            if argv.is_empty() {
                return Err(format!("MCP server {} 没有 cmd", name));
            }
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            // MCP server 也是本地进程：统一走 exec 层的环境清理与沙箱。
            crate::exec::scrub_env(&mut cmd);
            let mut read_sandbox = sandbox.clone();
            // `python3 /abs/server.py` 这类解释器启动方式，解释器本身在 PATH
            // 里，但脚本不在。只追加脚本所在目录，避免 MCP 配置绝对路径时
            // 整个 server 因读侧不可达而超时。
            if let Some(script) = argv.get(1).map(Path::new) {
                if script.is_absolute() && script.is_file() {
                    if let Some(parent) = script.parent() {
                        read_sandbox.read_roots.push(parent.to_string_lossy().into_owned());
                    }
                }
            }
            crate::exec::apply_linux_sandbox(&mut cmd, workspace, &read_sandbox)?;
            crate::exec::isolate_group(&mut cmd);
            #[cfg(windows)]
            if sandbox.fs {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(crate::winsandbox::CREATE_SUSPENDED);
            }
            let mut child = cmd
                .spawn()
                .map_err(|e| format!("启动 MCP server {} 失败: {}", name, e))?;
            #[cfg(windows)]
            let job = if sandbox.fs {
                let job = match crate::winsandbox::Job::assign(
                    child.id(),
                    &crate::winsandbox::Limits::from_env(),
                ) {
                    Ok(job) => job,
                    Err(e) => {
                        let _ = child.kill();
                        return Err(format!("MCP Job Object 不可用: {}", e));
                    }
                };
                if let Err(e) = crate::winsandbox::Job::resume(child.id()) {
                    job.terminate();
                    let _ = child.kill();
                    return Err(format!("MCP Job 恢复失败: {}", e));
                }
                Some(job)
            } else {
                None
            };
            let stdin = child.stdin.take().ok_or("拿不到 MCP stdin")?;
            let stdout: ChildStdout = child.stdout.take().ok_or("拿不到 MCP stdout")?;
            // 有界通道：server 疯狂输出时最多堆 100 行，不能把整段输出
            // 无上限塞进内存。
            let (tx, rx) = sync_channel::<String>(100);
            std::thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    match line {
                        Ok(l) => {
                            if tx.send(l).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
            let mut client = Client {
                child,
                stdin,
                rx,
                next_id: 0,
                #[cfg(windows)]
                job,
            };
            client.request(
                "initialize",
                Json::obj(vec![
                    ("protocolVersion", Json::from("2024-11-05")),
                    (
                        "clientInfo",
                        Json::obj(vec![
                            ("name", Json::from("ui")),
                            ("version", Json::from(env!("CARGO_PKG_VERSION"))),
                        ]),
                    ),
                    ("capabilities", Json::obj(vec![])),
                ]),
            )?;
            let _ = client.notify("notifications/initialized", Json::obj(vec![]));
            let listed = client.request("tools/list", Json::obj(vec![]))?;
            let mut tools = Vec::new();
            for t in listed.arr_at("tools").cloned().unwrap_or_default() {
                tools.push(ToolInfo {
                    name: t.str_at("name").unwrap_or("").to_string(),
                    description: t.str_at("description").unwrap_or("").to_string(),
                });
            }
            servers.push(Server {
                name,
                client,
                tools,
            });
        }
        Ok(Hub { servers })
    }

    pub fn call(&mut self, server: &str, tool: &str, args: Json) -> Result<String, String> {
        let s = self
            .servers
            .iter_mut()
            .find(|s| s.name == server)
            .ok_or_else(|| format!("没有名为 {} 的 MCP server", server))?;
        if !s.tools.iter().any(|t| t.name == tool) {
            return Err(format!("server {} 没有工具 {}", server, tool));
        }
        let result = s.client.request(
            "tools/call",
            Json::obj(vec![
                ("name", Json::from(tool)),
                ("arguments", args),
            ]),
        )?;
        // MCP 返回 {content:[{type:"text",text:"..."}], isError?}
        let mut parts: Vec<String> = Vec::new();
        for c in result.arr_at("content").cloned().unwrap_or_default() {
            if let Some(text) = c.str_at("text") {
                parts.push(text.to_string());
            }
        }
        if parts.is_empty() {
            parts.push(result.to_compact());
        }
        Ok(parts.join("\n"))
    }

    /// 给提示词用的清单
    pub fn catalog(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for s in &self.servers {
            for t in &s.tools {
                out.push((
                    format!("{}:{}", s.name, t.name),
                    t.description.clone(),
                ));
            }
        }
        out
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        for s in &mut self.servers {
            #[cfg(windows)]
            if let Some(job) = &s.client.job {
                job.terminate();
            }
            let pid = s.client.child.id();
            let _ = s.client.child.kill();
            crate::exec::kill_tree(pid);
        }
    }
}
