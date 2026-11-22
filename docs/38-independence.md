# 独立性约束：零依赖是产品边界

> 结论：**`ui-core` / `ui-cli` 保持 Rust std-only，不引入第三方 crate、云服务、Cursor/VS Code 运行时依赖。**
>
> Cursor 的亮点只能“内建实现”，不能通过引入 SDK、框架或云服务来获得。

## 1. 先定义“无依赖”

“无依赖”必须分三层，否则会自相矛盾：

### L0 内核层（必须零依赖）

`ui-core` + `ui-cli`：

- 只用 Rust std / OS syscall；
- `Cargo.toml` 不新增第三方依赖；
- `Cargo.lock` 只有 workspace 自己的包；
- 启动不依赖网络；
- 不依赖 Cursor / VS Code / 任何云 API；
- 前端资源随二进制走，不用 CDN、npm、框架运行时。

### L1 可选 OS 工具（缺失要显式降级）

- `sh` / `cmd`：shell 工具；
- `grep`：grep 工具，缺失退化为内置字面搜索；
- `python3`：部分 skill / 测试；
- `git`：部分测试/工程脚本；
- LibreOffice：可选视觉渲染。

这些不是内核依赖；缺失时应明确报错或降级，不能假装成功。

### L2 可选 skill / 企业 adapter（不进核心二进制）

- `docx-safe-edit` 需要 Python + lxml：它是 skill pack，不是 core；
- OIDC / Postgres / 容器 runner：企业 adapter，可选、独立进程/独立构建；
- Kafka/Redis/向量库：不进入 L0。

**如果要求“连 L1/L2 也不能有”，那企业 SSO/Postgres/容器和 docx skill 都必须重写或删除。**  
这是能力与独立性的硬 trade-off，不能两边都宣称。

## 2. 当前事实

- Rust 核心：`Cargo.lock` 只有 `ui-core` / `ui-cli`；
- Web 资源：单页 HTML/CSS/JS，无 CDN；
- 本地 journal：JSONL + 文件锁；
- 沙箱：Linux Landlock / Windows Job，不依赖第三方运行时；
- docx skill：确实需要 Python + lxml，属于 L2；
- shell/grep：确实调用 OS 工具，属于 L1。

所以准确说法是：

> **内核零第三方依赖；产品不是“零外部程序”。**

不能把这两句话混在一起宣传。

## 3. 硬规则

1. `Cargo.toml` / `Cargo.lock` 不新增第三方 crate；
2. `ui-core` / `ui-cli` 不引入 HTTP/TLS/DB/JSON 第三方库；
3. 不在启动路径访问网络；
4. 不引入 npm / bundler / 前端框架 / CDN；
5. 不接 Cursor/VS Code 的运行时 API 作为必需依赖；
6. 外部工具只作为 L1，缺失显式降级；
7. 企业 adapter 不得链接进 core 默认构建；
8. 任何“为了企业功能加个 crate”的 PR 默认拒绝，除非改独立层。

## 4. Cursor 亮点怎么“无依赖”吸收

| Cursor 亮点 | 不允许 | 我们的实现 |
|---|---|---|
| 代码库索引 | tree-sitter / embedding / 向量库 | 自建文件/符号/trigram 索引，std + 自写 JSON |
| 内联 diff | `similar` 等 diff crate | 自写 Myers/LCS diff，复用现有 unified patch |
| checkpoint/undo | git 依赖 | `.ui/checkpoints` + 内容寻址 + journal |
| context chips | 前端框架 | 原生 HTML/JS + 自建事件 |
| Agent timeline | React/Vue | 原生 DOM + SSE |
| Rules/skills | 模板引擎 | Markdown + 约定目录 + 自写校验 |
| metrics | Prometheus client crate | 自写 text exposition |
| trace | OTel SDK | 自写 JSON trace + 可选文件导出 |

原则：

> **可以学交互和机制，不可以学“用库解决”。**

## 5. 企业路线的影响

原路线里的 M1/M2/M4 会破坏 L0：

- OIDC 需要 TLS/JWT/JWKS；
- Postgres 需要数据库客户端；
- 容器 runner 需要容器运行时。

两种选择：

### A. 推荐：核心零依赖，企业 adapter 独立

- core 继续 std-only；
- `ui-enterprise` 独立二进制/独立 crate，可用第三方依赖；
- core 通过文件/进程协议与 adapter 通信；
- 企业部署可以没有 Cursor/云，但允许 Postgres/OIDC/容器这些**显式企业依赖**。

### B. 严格全产品零依赖

- 放弃 Postgres，继续本地 JSONL/文件；
- 放弃 OIDC，做本地账号或反向代理；
- 放弃容器 runner，使用 OS 沙箱；
- 放弃 Prometheus/OTel SDK，只做文本指标；
- 企业 HA/多用户能力大幅缩水。

**A 是工程上可持续的；B 是“独立性绝对优先”的取舍。**

## 6. 门禁

`tools/verify_independence.sh` 检查：

- `Cargo.lock` 只有 workspace 包；
- core/cli 没有第三方依赖；
- 前端资源没有外部 `src/href`；
- 仓库没有 `package.json` / `node_modules` 被跟踪；
- 文档明确 L0/L1/L2 边界。

这不是性能测试，是**防止无依赖卖点被悄悄破坏**。

## 7. 结论

> **零依赖不是“什么都不用”，而是“核心不把命运交给第三方”。**

- 内核：std-only；
- 工具：可选 OS；
- 企业：可选 adapter；
- Cursor：只学设计，不引依赖；
- 任何新增依赖都必须先问：它属于 L0、L1 还是 L2？
