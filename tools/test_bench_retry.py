import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bench_repos  # noqa: E402


def fake_run_task(repo_rec, repo, task, ctx, task_index):
    attempt = fake_run_task.calls
    fake_run_task.calls += 1
    if attempt == 0:
        return {
            "status": "failed",
            "changedCount": 0,
            "run": {"rc": 1, "reason": "model_error", "tokens": 100, "toolCalls": 0},
        }
    return {
        "status": "success",
        "changedCount": 0,
        "run": {"rc": 0, "reason": "done", "tokens": 250, "toolCalls": 3},
    }


fake_run_task.calls = 0
bench_repos.run_task = fake_run_task
row = bench_repos.run_task_with_retries(None, Path("."), {"id": "t"}, {"attempts": 2}, 1)
assert row["attempts"] == 2, row
assert row["retry"]["used"] == 1, row
assert row["retry"]["totalTokens"] == 350, row
print("PASS bench retry: model_error retried and token usage recorded")
