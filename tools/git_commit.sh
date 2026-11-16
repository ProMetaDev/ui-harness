#!/usr/bin/env bash
set -euo pipefail
cd /mnt/x/dsh_ws/harness-v2

if git rev-parse HEAD >/dev/null 2>&1; then
  echo "已有提交，跳过首次提交"
else
  git commit -q -m "ui: 首个可用原型

内核（ui-core，16 个模块）
- 七条契约：快照/效果/决策/预算/日志/委派/修复
- 追加式 JSONL 日志：fsync + SHA-256 哈希链 + 断尾修复 + 多写者
- 效果集策略：最长字面前缀，批准只对字面目标生效
- 真实路径校验（符号链接越界拒绝）、shell 进程组超时
- 三种模式（quiet/standard/strict），deny 是硬边界
- 上下文压缩：锚点 + 最近 N + 确定性摘要
- MCP：stdio JSON-RPC，外部工具记为 proc.exec mcp:<server>:<tool>

命令行（ui-cli）
- run/chat/tui/serve/resume/replay/verify/export/skills/workflow/apply/policy
- audit --as：用另一个模式重判历史
- lab：实验包（产物哈希/输入血缘/结论对账/多 seed 对照表）

前端
- 手写 HTTP + SSE，零运行时依赖
- 四视图（对话/工作区/能力/实验室）+ 命令面板 + 看板娘 UU

质量
- 128 个测试，23 项门禁（tools/debug.sh）
- 一键演示：tools/proto.sh"
  echo "已提交: $(git rev-parse --short HEAD)"
fi

git log --oneline | head -3
