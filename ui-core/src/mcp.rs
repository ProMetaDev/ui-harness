//! MCP 客户端：把外部 MCP server 的工具接进内核。
//!
//! 设计取舍：不把外部工具伪装成内置工具，而是给模型一个 `mcp_call` 工具，
//! 参数是 `{server, tool, args}`。这样：
//! * 策略只需要一条规则（`proc.exec mcp:<server>:<tool>`），默认要审批；
//! * 日志里留下的是"调了哪个服务器的哪个工具"，审计读得懂；
//! * 不需要动静态的工具注册表。

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
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
            let line = Self::read_line(&self.rx, Duration::from_secs(20))?;
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
    pub fn start(config: &Json) -> Result<Hub, String> {
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
            let mut child = cmd
                .spawn()
                .map_err(|e| format!("启动 MCP server {} 失败: {}", name, e))?;
            let stdin = child.stdin.take().ok_or("拿不到 MCP stdin")?;
            let stdout: ChildStdout = child.stdout.take().ok_or("拿不到 MCP stdout")?;
            let (tx, rx) = channel::<String>();
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
            let _ = s.client.child.kill();
        }
    }
}
