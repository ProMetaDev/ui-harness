import hashlib
import json
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from bench_repos import write_report  # noqa: E402


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def base_report(mode):
    return {
        "generatedAt": "2026-11-27T00:00:00+00:00",
        "mode": mode,
        "dryRunReason": "test" if mode == "dry-run" else "",
        "model": {"kind": "none" if mode == "dry-run" else "external", "label": "test"},
        "ui": {"bin": "ui", "version": "test"},
        "inputs": {
            "repos": {"name": "repos.json", "sha256": "a" * 64, "bytes": 10},
            "tasks": {"name": "tasks.json", "sha256": "b" * 64, "bytes": 20},
        },
        "host": {"system": "Test", "release": "1", "machine": "x", "python": "3"},
        "limits": {"timeoutSec": 60, "attempts": 2},
        "repos": [],
        "notes": [],
        "durationMs": 1,
    }


with tempfile.TemporaryDirectory() as tmp:
    report = base_report("real")
    report["repos"] = [{
        "name": "repo",
        "path": "/repo",
        "status": "ok",
        "tasks": [{
            "id": "t1",
            "status": "success",
            "attempts": 2,
            "changedCount": 0,
            "run": {"skipped": False, "tokens": 250, "toolCalls": 3, "durationMs": 1},
            "verify": {"ok": True},
            "taskVerify": {"ok": True},
            "expect": {"passed": True},
            "retry": {"used": 1, "totalTokens": 350},
        }],
    }]
    json_path, md_path, manifest_path = write_report(report, Path(tmp) / "real")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    assert manifest["files"]["report.json"]["sha256"] == sha256(json_path)
    assert manifest["files"]["report.md"]["sha256"] == sha256(md_path)
    assert manifest["inputs"]["tasks"]["sha256"] == "b" * 64
    assert manifest["limits"]["attempts"] == 2
    assert report["summary"]["tasks"]["attempts"] == 2
    assert report["summary"]["tasks"]["executed"] == 1
    assert report["summary"]["tasks"]["retried"] == 1
    assert report["summary"]["tasks"]["tokens"] == 350
    assert report["summary"]["tasks"]["tokensKnown"] == 1

    dry = base_report("dry-run")
    dry["repos"] = [{
        "name": "repo",
        "path": "/repo",
        "status": "ok",
        "tasks": [{
            "id": "t1",
            "status": "dry-run",
            "run": {"skipped": True},
            "verify": {"skipped": True},
            "expect": {"passed": None},
        }],
    }]
    write_report(dry, Path(tmp) / "dry")
    assert dry["summary"]["tasks"]["attempts"] == 0
    assert dry["summary"]["tasks"]["tokens"] == 0

print("PASS bench evidence: manifest hashes and retry-aware totals are recorded")
