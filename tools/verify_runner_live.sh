#!/usr/bin/env bash
# M3 runner live 测试：真实 Docker + 本地 HTTP runner。
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
set -euo pipefail
cd "$UI_ROOT"

IMAGE="${UI_RUNNER_IMAGE:-python:3.11-slim}"
if ! command -v docker >/dev/null 2>&1 || ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  echo "SKIP: docker/$IMAGE 不可用"
  exit 0
fi
PORT="${UI_RUNNER_PORT:-18089}"
WS=$(mktemp -d /tmp/ui-runner-ws.XXXXXX)
LOG=/tmp/ui-runner.log
TOKEN=test-token
python3 adapters/runner/runner.py --port "$PORT" --workspace-root "$WS" --image "$IMAGE" --token "$TOKEN" >"$LOG" 2>&1 &
SRV=$!
cleanup() {
  kill "$SRV" 2>/dev/null || true
  docker ps -q --filter "name=ui-runner-" | xargs -r docker kill >/dev/null 2>&1 || true
  rm -rf "$WS"
}
trap cleanup EXIT

for _ in $(seq 1 50); do
  if curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then break; fi
  sleep 0.2
done
curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null
python3 adapters/runner/test_live.py \
  --base "http://127.0.0.1:$PORT" --token "$TOKEN" --workspace "$WS"
