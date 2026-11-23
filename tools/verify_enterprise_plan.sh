#!/usr/bin/env bash
# Phase 0 设计基线门禁：只检查“计划/设计/runbook 是否真的写了关键契约”，
# 不把文档存在伪装成企业能力已完成。
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
set -euo pipefail
cd "$UI_ROOT"

need_file() {
  [ -f "$1" ] || { echo "FAIL: 缺少 $1"; exit 1; }
}

need() {
  local file="$1" text="$2"
  grep -qF -- "$text" "$file" || {
    echo "FAIL: $file 缺少关键契约: $text"
    exit 1
  }
}

ROADMAP=docs/35-enterprise-roadmap.md
PHASE0=docs/36-enterprise-phase0.md
RUNBOOK=docs/37-enterprise-runbook.md
INDEP=docs/38-independence.md
M1=docs/41-enterprise-m1.md

need_file "$ROADMAP"
need_file "$PHASE0"
need_file "$RUNBOOK"
need_file "$INDEP"
need_file "$M1"

need "$ROADMAP" "Enterprise Single-Tenant v1"
need "$ROADMAP" "M6 试点"
need "$ROADMAP" "多租户"
need "$PHASE0" "session_heads"
need "$PHASE0" "approvals"
need "$PHASE0" "Runner 协议"
need "$PHASE0" "SLO"
need "$RUNBOOK" "备份与恢复"
need "$RUNBOOK" "Worker 排空"
need "$RUNBOOK" "事故响应"
need "$INDEP" "L0 内核层"
need "$INDEP" "L1 可选 OS 工具"
need "$INDEP" "L2 可选 skill / 企业 adapter"
need "$M1" "X-UI-User"
need "$M1" "OIDC/TLS/JWT 不进入"
need "$M1" "角色矩阵"

# 明确禁止把“文档完成”说成“企业已完成”
if grep -RqiE 'enterprise[- ]ready|已企业级|达到企业级' docs/35-enterprise-roadmap.md docs/36-enterprise-phase0.md docs/37-enterprise-runbook.md; then
  echo "FAIL: Phase 0 文档出现了未经验证的 'enterprise-ready' 结论"
  exit 1
fi

echo "PASS 企业 Phase 0 设计基线（路线图/架构/runbook 关键契约齐全）"
