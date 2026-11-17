#!/usr/bin/env bash
set -uo pipefail
status() {
  curl -s --max-time 20 "https://img.shields.io/github/actions/workflow/status/ProMetaDev/ui-harness/ci.yml?branch=main" \
    | grep -oE '>(passing|failing|pending|no status)<' | head -1 | tr -d '><'
}
for i in $(seq 1 12); do
  s=$(status); echo "  [$((i*20))s] $s"
  case "$s" in passing|failing) break ;; esac
  sleep 20
done
