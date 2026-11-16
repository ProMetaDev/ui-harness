# 插件：能力声明必须显式

## 用法

```json
// .ui/plugins.json
{
  "plugins": [
    {
      "name": "demo",
      "cmd": "python3 .ui/demo.py",
      "tools": [
        { "name": "echo",  "summary": "原样返回", "effects": [] },
        { "name": "write", "summary": "写文件",   "effects": ["fs.write out.txt"] }
      ]
    }
  ]
}
```

```bash
ui plugins                 # 列出插件和它们声明的能力（不启动任何进程）
ui run --task "..." --plugins other.json
```

插件协议：stdin 收一行 `{plugin, tool, args}`，stdout 写文本结果，非零退出码算失败。

## 三条硬规矩

1. **`effects` 必须显式出现**，哪怕是 `[]`。"没写"和"没有副作用"必须能区分——
   前者加载失败，后者才是真空效果集。
2. **效果类型必须已知**（`fs.read` / `fs.write` / `proc.exec` / `net.fetch` / `secret.read`）。
   写错一个字母就加载失败，不会悄悄变成 allow。
3. **相对目标按工作区解析**。声明 `fs.write out.txt` 指的是工作区里的 `out.txt`；
   写 `fs.write **` 这种通配符没有意义，会被策略当成字面路径拒绝。

## 为什么不做"热加载任意代码"

那等于给审计开一个后门：一个没有声明效果的插件，调用了什么、改了什么，日志里都说不清。
现在插件的能力面是**加载时静态确定的**，所以：

- 策略能在调用前判定（日志里 `tool/request.effects` 就是声明的那几条）；
- 审计能回答"这个插件本来被允许做什么"；
- 加插件不需要改内核代码。

## 实测

```
$ ui plugins
配置 /tmp/ui-plug/.ui/plugins.json · 1 个插件
  demo（2 个工具）cmd=python3 /tmp/ui-plug/.ui/demo.py
    - echo — 原样返回  [无副作用]
    - write — 写文件  [fs.write out.txt]
```

调用后日志里（`plugin_call` 的 effects）：

```json
{"kind":"fs.write","target":"/tmp/ui-plug/out.txt"}
```

拒绝的情况：

```
没有声明 effects（没有副作用也要写 []）
未知效果 `fs.rede`（允许：fs.read, fs.write, proc.exec, net.fetch, secret.read）
```

## 还没做的

- 插件只支持"一次调用"，没有长驻进程/会话；
- 没有版本与依赖声明；
- 没有插件市场/签名校验。
