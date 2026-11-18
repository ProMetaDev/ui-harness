#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 一条命令验证全部能力。任何一层 FAIL 就停在那层——上面的层不值得看。
set -uo pipefail
cd "$UI_ROOT"
export CARGO_TARGET_DIR="$UI_TARGET_DIR"
BIN="$CARGO_TARGET_DIR/release/ui"
WS=/tmp/ui-gate
PASS=0
FAIL=0

run() {
  local name="$1"
  shift
  if "$@" >/tmp/gate.out 2>&1; then
    printf 'PASS  L%s %s\n' "$LAYER" "$name"
    PASS=$((PASS + 1))
  else
    printf 'FAIL  L%s %s\n' "$LAYER" "$name"
    tail -8 /tmp/gate.out | sed 's/^/          /'
    FAIL=$((FAIL + 1))
  fi
}

check_ansi() {
  "$BIN" preview --cols 84 --rows 8 --ansi | grep -q $'\x1b\[38;5;179'
}

check_sizes() {
  for spec in 40x10 80x24 120x40; do
    "$BIN" preview --cols "${spec%x*}" --rows "${spec#*x}" >/dev/null || return 1
  done
}

check_skills() {
  "$BIN" skills --workspace demo | grep -q 'release-note'
}

check_workflow_json() {
  rm -rf "$WS"; mkdir -p "$WS"
  "$BIN" workflow --file fixtures/workflow-review.json --mock fixtures/mock.json \
    --workspace "$WS" --yes --json 2>/dev/null | grep -q '"ok":true'
}

check_gate_blocking() {
  cat > "$WS/wf-gate.json" <<'EOF'
{"name":"g","steps":[{"prompt":"危险动作","gate":true}]}
EOF
  "$BIN" workflow --file "$WS/wf-gate.json" --mock fixtures/mock.json \
    --workspace "$WS" --json >/dev/null 2>&1
  [ $? -eq 1 ]
}

check_isolate_apply() {
  cat > "$WS/wf-write.json" <<'EOF'
{"name":"w","steps":[{"parallel":[{"prompt":"写个文件"}]}]}
EOF
  cat > "$WS/write.json" <<'EOF'
{"model":"scripted","steps":[
  {"content":"写","toolCalls":[{"name":"fs_write","args":{"path":"out.txt","content":"hi"}}]},
  {"content":"done"}]}
EOF
  "$BIN" workflow --file "$WS/wf-write.json" --mock "$WS/write.json" \
    --workspace "$WS" --isolate --yes >/dev/null 2>&1 || return 1
  [ ! -f "$WS/out.txt" ] || return 1          # 隔离：父工作区不能被直接改
  "$BIN" apply --all --workspace "$WS" --yes --json | grep -q '"ok":true' || return 1
  [ "$(cat "$WS/out.txt")" = "hi" ] || return 1
  # 再 apply 一次应该判成漂移（文件已变）
  "$BIN" apply --all --workspace "$WS" --json >/dev/null 2>&1
  [ $? -eq 4 ]
}

check_viewer() {
  "$BIN" export --workspace "$WS" --html "$WS/trace.html" >/dev/null 2>&1 || return 1
  python3 tools/check_viewer.py "$WS/trace.html" >/dev/null 2>&1
}

check_verify_clean() {
  "$BIN" verify --workspace "$WS" | grep -q '哈希链完整'
}

bash tools/preflight.sh || true
echo
echo "== L1 内核（无终端、无模型）"
LAYER=1
# 构建必须和 dev.sh/test.sh 一样在副本里做：
# 同一个 target 目录混用两个源目录（X: 与 ~/ui）会让 cargo 的指纹错乱，
# 拿到过期的 ui-core 元数据（真实踩过：报「cannot find Sha256」）。
run "build" bash tools/dev.sh build --release
run "unit + integration tests" cargo test --offline
run "doctor" "$BIN" doctor --workspace demo
run "verify（干净日志）" check_verify_clean

echo "== L2 帧（纯函数）"
LAYER=2
run "frame 84x26" "$BIN" preview --cols 84 --rows 26
run "frame 边界尺寸" check_sizes
run "ansi 颜色码" check_ansi

echo "== L3 终端（伪 tty）"
LAYER=3
run "pty 全屏冒烟" bash tools/tui_smoke.sh

echo "== L4 功能闭环（skill / 工作流 / 隔离 / 复盘）"
LAYER=4
run "skills 列表" check_skills
run "workflow --json" check_workflow_json
run "web 端（HTTP + SSE）" bash tools/serve_smoke.sh 7802
run "网页聊天流（离线校验）" bash tools/verify_chat_stream.sh
run "web 端能力（停止/展开/重连）" bash tools/probe_web_features.sh 7803
run "gate 未批准则停" check_gate_blocking
run "隔离 + apply + 漂移" check_isolate_apply
run "实验包（打包 + 校验 + 篡改检测）" bash tools/verify_lab.sh
run "输入血缘（数据换版本要能发现）" bash tools/verify_lineage.sh
run "多 seed 对照表（版本不一致拒绝求均值）" bash tools/verify_table.sh
run "ML 基准（3 任务 × 3 seed）" bash tools/bench_ml.sh
run "结论对账（声明必须有产物背书）" bash tools/verify_claims.sh
run "实验室跑一次（并发保护 + SSE）" bash tools/verify_lab_run.sh
run "空状态守卫（元事件不该藏欢迎页）" bash tools/check_empty_guard.sh
run "MCP 工具（外部 server 可调用 + 策略留痕）" bash tools/verify_mcp.sh
run "访问鉴权（401 / Cookie / --token off）" bash tools/verify_auth.sh
run "工具白名单（--no-tools / --tools）" bash tools/verify_tool_filter.sh
run "新工具（grep/patch/todo/tree/fs_mv）" bash tools/verify_tools2.sh
run "多 provider（注册表 / 切换 / 报错）" bash tools/verify_providers.sh
run "插件（能力声明 + 调用 + 拒绝非法声明）" bash tools/verify_plugins_v2.sh
run "契约自检（T1-T7，本地实跑）" bash tools/bench_self.sh
run "CRLF+BOM 文件打补丁（Windows 回归）" bash tools/verify_crlf_patch.sh
run "会话（新建/列表/互不干扰）" bash tools/verify_sessions.sh
run "会话 Web（列表/切换/历史不串）" bash tools/verify_web_sessions.sh
run "验证包（12 个攻击场景）" bash tools/kit.sh
run "密钥隔离（评审 P0-1 回归）" bash tools/verify_secrets_isolation.sh
run "Web /api/tree 无路径穿越（评审 P1-1a）" bash tools/verify_api_traversal.sh
run "版本接口（--version/-V/version 一致）" bash tools/verify_version.sh
run "密钥扫描（工作树里不能有密钥形状）" bash tools/scan_secrets.sh
run "观测式效果（shell 实际改动入日志）" bash tools/verify_observe.sh
run "OS 沙箱（Landlock：区外写入被拒）" bash tools/verify_sandbox.sh
run "日志不可写（必须 fail closed）" bash tools/verify_failclosed.sh
run "子进程 env 清理（密钥不进子进程）" bash tools/verify_env_scrub.sh
run "system prompt 追加 + 白名单进提示词" bash tools/verify_sysprompt.sh
run "协作模式（拆解 → 并行隔离 → 汇总）" bash tools/verify_team.sh
run "隔离副本 manifest（xxh 格式）" bash tools/verify_manifest.sh
run "知识卡来源绑定（bind / STALE）" bash tools/verify_knowledge.sh
run "确定性检索（理论 P6 / H2）" bash tools/verify_h2_determinism.sh
run "观测是下界（理论结论 A）" bash tools/verify_observation_lower_bound.sh
run "干净检出的 HEAD 可构建" bash tools/verify_head_clean.sh
run "Windows 构建 fail-closed（失败必须非零）" bash tools/verify_build_failclosed.sh
run "Windows target 下测试代码可编译" bash tools/verify_win_tests_compile.sh
run "导出查看器并校验" check_viewer

echo
echo "== $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
