import json
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from bench_repos import Fatal, load_tasks  # noqa: E402


def load_rows(rows):
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "tasks.json"
        path.write_text(json.dumps(rows, ensure_ascii=False), encoding="utf-8")
        return load_tasks(str(path))


try:
    load_rows([{
        "id": "modify-without-verify",
        "prompt": "modify the code",
        "expect": {"changed": ["src/app.py"]},
    }])
except Fatal as exc:
    assert "必须提供 verify" in str(exc), exc
else:
    raise AssertionError("changed/changed_any task without verify must be rejected")

rows = load_rows([
    {
        "id": "modify-with-verify",
        "prompt": "modify the code",
        "expect": {"changed": ["src/app.py"], "no_extra_changes": True},
        "verify": "python3 -m pytest -q",
    },
    {
        "id": "read-only",
        "prompt": "explain the code",
        "expect": {"unchanged": True, "answer_min_chars": 20},
    },
])
assert rows[0]["verify"] == "python3 -m pytest -q"
assert rows[1]["verify"] is None

print("PASS bench tasks: modification expectations require an executable verify")
