#!/usr/bin/env bash
set -uo pipefail
echo "python3: $(python3 --version 2>&1)"
python3 - <<'PY'
mods = {}
for m in ("numpy", "sklearn", "torch"):
    try:
        mod = __import__(m)
        mods[m] = getattr(mod, "__version__", "?")
    except Exception as e:
        mods[m] = "缺失"
print("模块:", mods)
PY
echo "核数: $(nproc)"
