# MCP：外部工具接入

## 用法

```bash
ui run --task "..." --mcp fixtures/mcp/demo.json
```

配置文件：

```json
{
  "servers": [
    { "name": "demo", "cmd": ["python3", "path/to/server.py"] }
  ]
}
```

启动时按顺序：拉起进程 → `initialize` 握手 → `notifications/initialized` → `tools/list`。
任何一个 server 起不来，启动直接失败——**不静默降级**，否则用户会以为工具在，其实不在。

## 为什么不把外部工具伪装成内置工具

内置工具是静态注册表（`&'static str` + 函数指针）。要让 MCP 工具"长得像内置的"，
得把整张表改成动态的，风险大、收益低。

所以选择：**一个内置工具 `mcp_call`**，参数 `{server, tool, args}`。

```json
{"name":"mcp_call","args":{"server":"demo","tool":"echo","args":{"text":"hi"}}}
```

换来三个好处：

1. **策略只需一条规则**：效果是 `proc.exec`，目标 `mcp:demo:echo`，默认 `ask`；
   想放行某个工具就写 `proc.exec mcp:demo:echo → allow`，粒度天然正确。
2. **审计读得懂**：日志里是"调了哪个服务器的哪个工具"，不是一堆匿名调用。
3. **风险集中**：所有外部调用走同一个闸门，一处加固到处生效。

代价：模型得知道有哪些工具。所以启动时把 `server:tool — 描述` 注入提示词，
`mcp_call` 的工具说明里也写明了这一点。

## 实测

```
$ ui run --task "用外部工具算个和" --mock sc.json --workspace /tmp/ui-mcp --yes \
    --mcp fixtures/mcp/demo.json
  → mcp_call  ok   echo: 你好 MCP
  → mcp_call  ok   sum: 42.0
结束原因 done · 2 步 · 2 次工具 · 约 1560 tokens
```

日志里的决策（`tools/show_first_request.py` 读出来的）：

```json
{"seq":4,"tool":"mcp_call","effects":[{"kind":"proc.exec","target":"mcp:demo:echo"}],
 "decision":"ask","reason":"执行命令"}
```

不配 `--mcp` 时同一个剧本：

```
→ mcp_call  失败   没有配置 MCP server（启动时加 --mcp <config.json>）
```

## 还没做的

- **工具级 schema 透传**：现在参数是自由 JSON，模型容易传错；应该把 `inputSchema`
  转成我们的参数表，让校验在内核里发生。
- **resources / prompts**：只接了 tools，MCP 的另外两类没接。
- **超时与重启**：server 挂了只会在调用时报错，不会自动重连。
- **能力视图里显示 MCP 工具**：`/api/meta` 现在只列内置的 `mcp_call`。
