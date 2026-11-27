import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from bench_repos import eval_expect  # noqa: E402

repo = Path(".").resolve()
changed = [
    {"path": "tests/test_basic.py", "change": "modified"},
    {"path": "pytest_out.txt", "change": "added"},
]

passed, checks, reason = eval_expect(
    {"changed": ["tests/test_basic.py"], "no_extra_changes": True},
    repo,
    "",
    changed,
)
assert passed is False, (passed, checks, reason)
assert any(c["check"] == "no_extra_changes" and not c["passed"] for c in checks), checks

passed, checks, reason = eval_expect(
    {"changed": ["tests/test_basic.py"]},
    repo,
    "",
    changed,
)
assert passed is True, (passed, checks, reason)

passed, checks, reason = eval_expect(
    {"changed": ["tests/test_basic.py"], "no_extra_changes": True},
    repo,
    "",
    changed[:1],
)
assert passed is True, (passed, checks, reason)
assert all(c["passed"] for c in checks), checks

passed, checks, reason = eval_expect(
    {"changed_any": ["tests/test_basic.py", "tests/test_other.py"],
     "no_extra_changes": True},
    repo,
    "",
    [
        {"path": "tests/test_basic.py", "change": "modified"},
        {"path": "tests/test_other.py", "change": "modified"},
    ],
)
assert passed is True, (passed, checks, reason)

passed, checks, reason = eval_expect(
    {"no_extra_changes": True},
    repo,
    "",
    changed[:1],
)
assert passed is False, (passed, checks, reason)
assert any(c["check"] == "no_extra_changes" and not c["passed"] for c in checks), checks

print("PASS bench expect: no_extra_changes rejects only files outside the allowed set")
