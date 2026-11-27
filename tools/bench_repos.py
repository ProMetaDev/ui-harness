#!/usr/bin/env python3
"""真实多仓库评测框架（只跑用户填的真实仓库；不内置任何仓库内容）。

它做四件事，每一件都只记录**实际观察到的事实**：

  1. 校验仓库：路径存在、是目录、非空、相对路径不得用 `..` 逃出 repos 文件所在目录；
  2. 每个仓库跑一次 `ui index --workspace <repo>`，记录文件数/符号数/耗时；
  3. 每个任务跑 `ui run --workspace <repo> --task <prompt> --yes`，记录 rc、tokens、
     工具调用数、耗时、工作区实际变化的文件；
  4. 每个任务跑 `ui verify --workspace <repo> --journal <本次日志>`，并做 expect 判定。

没有 provider（`--provider-cmd` / `--mock` / `--provider`）或显式 `--dry-run` 时，
只做 index，任务标记为 `dry-run`：**不执行 `ui run`，也不写任何“成功”结论**。
`--mock` 会真的执行 run（脚本化模型），但报告把 model.kind 标成 `scripted`，
它只用于自测管线，不是模型能力评测。

输出：`<out>/report.json` 与 `<out>/report.md`。

依赖：Python 3.8+ 标准库。不读写 API key；provider 命令原文不写入报告。
"""

import argparse
import datetime
import hashlib
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import time
from collections import Counter
from pathlib import Path

SCHEMA = 1
DEFAULT_TIMEOUT = 600
MAX_SCAN_FILES = 200000
MAX_CHANGED_RECORDS = 200
TAIL_CHARS = 4000

EXCLUDE_DIRS = {
    ".git", ".ui", "target", "node_modules", "dist", "build",
    ".venv", "venv", "__pycache__", ".tox", ".mypy_cache",
    ".pytest_cache", ".idea", ".vscode", ".next", ".nuxt",
}

LANG_BY_EXT = {
    ".rs": "rust", ".py": "python", ".go": "go", ".js": "javascript",
    ".mjs": "javascript", ".cjs": "javascript", ".ts": "typescript",
    ".tsx": "typescript", ".jsx": "javascript", ".java": "java",
    ".kt": "kotlin", ".kts": "kotlin", ".swift": "swift", ".c": "c",
    ".h": "c", ".cc": "cpp", ".cpp": "cpp", ".hpp": "cpp",
    ".cs": "csharp", ".rb": "ruby", ".php": "php", ".lua": "lua",
    ".sh": "shell", ".ps1": "powershell", ".md": "markdown",
    ".json": "json", ".toml": "toml", ".yaml": "yaml", ".yml": "yaml",
}

PLACEHOLDER_MARKERS = ("REPLACE_ME",)

SECRET_PATTERNS = [
    (re.compile(r"(?i)\b(api[_-]?key|access[_-]?token|secret|password)"
                r"\b(\s*[:=]\s*)([^\s,;\"']{4,})"), r"\1\2***"),
    (re.compile(r"\bsk-[A-Za-z0-9_-]{8,}"), "sk-***"),
    (re.compile(r"\bBearer\s+[A-Za-z0-9._-]{8,}"), "Bearer ***"),
]

ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")


class Fatal(Exception):
    pass


def eprint(*parts):
    print(*parts, file=sys.stderr)


def iso_now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")


def sha256_text(text):
    return hashlib.sha256((text or "").encode("utf-8", "replace")).hexdigest()


def redact(text):
    if not text:
        return ""
    out = text
    for pat, repl in SECRET_PATTERNS:
        out = pat.sub(repl, out)
    return out


def tail(text, limit=TAIL_CHARS):
    text = redact(text or "")
    if len(text) <= limit:
        return text
    return "...(截断)..." + text[-limit:]


def read_text(path, limit=4 * 1024 * 1024):
    data = Path(path).read_bytes()[:limit]
    return data.decode("utf-8", "replace")


# ---------------------------------------------------------------------------
# 输入
# ---------------------------------------------------------------------------

def load_entries(path, key):
    p = Path(path)
    if not p.is_file():
        raise Fatal("找不到 %s 文件：%s" % (key, path))
    try:
        raw = json.loads(p.read_text(encoding="utf-8-sig"))
    except Exception as exc:
        raise Fatal("%s 不是合法 JSON：%s（%s）" % (key, path, exc))
    if isinstance(raw, dict):
        for name in (key, key[:-1] if key.endswith("s") else key, "items"):
            if isinstance(raw.get(name), list):
                raw = raw[name]
                break
        else:
            raise Fatal("%s 顶层必须是 JSON 数组，或带 %s 数组的对象" % (key, key))
    if not isinstance(raw, list) or not raw:
        raise Fatal("%s 必须是非空 JSON 数组" % key)
    for i, entry in enumerate(raw):
        if not isinstance(entry, dict):
            raise Fatal("%s[%d] 必须是对象" % (key, i))
    return raw


def load_repos(path):
    rows = load_entries(path, "repos")
    seen = set()
    out = []
    for i, row in enumerate(rows):
        name = row.get("name")
        repo_path = row.get("path")
        if not isinstance(name, str) or not name.strip():
            raise Fatal("repos[%d].name 必须是非空字符串" % i)
        if not isinstance(repo_path, str) or not repo_path.strip():
            raise Fatal("repos[%d].path 必须是非空字符串" % i)
        name = name.strip()
        if name in seen:
            raise Fatal("repos 里 name 重复：%s" % name)
        seen.add(name)
        loc = row.get("loc")
        if loc is not None and not isinstance(loc, int):
            raise Fatal("repos[%d].loc 必须是整数或省略" % i)
        out.append({
            "name": name,
            "path": repo_path.strip(),
            "commit": (row.get("commit") or "").strip() if isinstance(row.get("commit"), str) else "",
            "language": (row.get("language") or "").strip() if isinstance(row.get("language"), str) else "",
            "loc": loc,
        })
    return out


def load_tasks(path):
    rows = load_entries(path, "tasks")
    seen = set()
    out = []
    for i, row in enumerate(rows):
        task_id = row.get("id")
        prompt = row.get("prompt")
        if not isinstance(task_id, str) or not task_id.strip():
            raise Fatal("tasks[%d].id 必须是非空字符串" % i)
        if not isinstance(prompt, str) or not prompt.strip():
            raise Fatal("tasks[%d].prompt 必须是非空字符串" % i)
        task_id = task_id.strip()
        if task_id in seen:
            raise Fatal("tasks 里 id 重复：%s" % task_id)
        seen.add(task_id)
        expect = row.get("expect")
        if expect is not None and not isinstance(expect, (str, dict)):
            raise Fatal("tasks[%d].expect 必须是字符串、对象或省略" % i)
        verify = row.get("verify")
        if verify is not None and not isinstance(verify, str):
            raise Fatal("tasks[%d].verify 必须是字符串或省略" % i)
        out.append({"id": task_id, "prompt": prompt, "expect": expect, "verify": verify})
    return out


# ---------------------------------------------------------------------------
# 路径与仓库校验
# ---------------------------------------------------------------------------

def is_placeholder(text):
    upper = text.upper()
    return any(marker in upper for marker in PLACEHOLDER_MARKERS)


def validate_repo(raw, base_dir):
    """返回 (status, resolved_path_or_None, reason)。status: ok|skipped|invalid。"""
    if is_placeholder(raw):
        return "skipped", None, "path 是占位符（REPLACE_ME/...），请填真实仓库路径"
    if any(ord(ch) < 32 for ch in raw):
        return "invalid", None, "path 含控制字符"
    p = Path(raw)
    base = base_dir.resolve()
    try:
        if p.is_absolute():
            resolved = p.resolve()
        else:
            if ".." in p.parts:
                resolved = (base / p).resolve()
                try:
                    common = os.path.commonpath([str(base), str(resolved)])
                except ValueError:
                    return "invalid", None, "path 中的 .. 越界（与 repos 文件不在同一根）"
                if common != str(base):
                    return "invalid", None, (
                        "path 中的 .. 越界：%s 不在 repos 文件目录 %s 内；"
                        "相对路径不允许逃出该目录，请改用绝对路径" % (raw, base)
                    )
            else:
                resolved = (base / p).resolve()
    except OSError as exc:
        return "invalid", None, "解析 path 失败：%s" % exc
    if not resolved.exists():
        return "invalid", None, "路径不存在：%s" % resolved
    if not resolved.is_dir():
        return "invalid", None, "路径不是目录：%s" % resolved
    try:
        with os.scandir(str(resolved)) as it:
            nonempty = any(True for _ in it)
    except OSError as exc:
        return "invalid", None, "读目录失败：%s" % exc
    if not nonempty:
        return "invalid", None, "目录为空：%s" % resolved
    return "ok", resolved, ""


def safe_repo_rel(repo, raw):
    """expect 里的相对路径：不得绝对，也不得用 .. 逃出仓库。"""
    p = Path(raw)
    if p.is_absolute():
        raise ValueError("expect 路径必须是仓库内相对路径：%s" % raw)
    if ".." in p.parts:
        raise ValueError("expect 路径不允许含 ..：%s" % raw)
    resolved = (repo / p).resolve()
    try:
        common = os.path.commonpath([str(repo.resolve()), str(resolved)])
    except ValueError:
        raise ValueError("expect 路径越界：%s" % raw)
    if common != str(repo.resolve()):
        raise ValueError("expect 路径越界：%s" % raw)
    return resolved


# ---------------------------------------------------------------------------
# 子进程与 UI 调用
# ---------------------------------------------------------------------------

def run_proc(cmd, cwd, timeout_sec, env=None):
    started = time.time()
    stdout, stderr = "", ""
    timed_out = False
    rc = None
    try:
        kwargs = {}
        if os.name == "posix":
            kwargs["start_new_session"] = True
        else:
            kwargs["creationflags"] = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
        proc = subprocess.Popen(
            [str(c) for c in cmd],
            cwd=str(cwd),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            env=env,
            **kwargs
        )
        try:
            stdout, stderr = proc.communicate(timeout=timeout_sec if timeout_sec else None)
        except subprocess.TimeoutExpired:
            timed_out = True
            if os.name == "posix":
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except OSError:
                    proc.kill()
            else:
                proc.kill()
            stdout, stderr = proc.communicate()
        rc = proc.returncode
    except FileNotFoundError:
        stderr = "找不到可执行文件：%s" % cmd[0]
    except Exception as exc:  # noqa: BLE001 - 单次调用失败要能隔离
        stderr = "启动子进程失败：%s" % exc
    return {
        "rc": rc,
        "durationMs": int((time.time() - started) * 1000),
        "timedOut": timed_out,
        "stdout": redact(stdout or ""),
        "stderr": redact(stderr or ""),
    }


def parse_json_block(text):
    m = re.search(r"\{.*\}", text or "", re.S)
    if not m:
        return None
    try:
        return json.loads(m.group(0))
    except Exception:
        return None


def git_meta(repo):
    if not (repo / ".git").exists():
        return None, None, None
    r = run_proc(["git", "-C", str(repo), "rev-parse", "--short", "HEAD"], repo, 10)
    commit = (r["stdout"].strip().splitlines() or [""])[0] if r["rc"] == 0 else ""
    r2 = run_proc(["git", "-C", str(repo), "status", "--porcelain", "-uno"], repo, 10)
    dirty = None
    if r2["rc"] == 0:
        dirty = len([line for line in r2["stdout"].splitlines() if line.strip()])
    return commit or None, dirty, r["stderr"] or r2["stderr"]


def manifest_stats(repo):
    manifest = repo / ".ui" / "index" / "manifest.json"
    if not manifest.is_file():
        return {"indexedLines": 0, "manifestFiles": 0, "extensions": []}
    try:
        doc = json.loads(manifest.read_text(encoding="utf-8"))
    except Exception:
        return {"indexedLines": 0, "manifestFiles": 0, "extensions": []}
    files = doc.get("files") if isinstance(doc, dict) else None
    if not isinstance(files, list):
        files = []
    lines = 0
    ext_count = Counter()
    for entry in files:
        if not isinstance(entry, dict):
            continue
        try:
            lines += int(entry.get("lines") or 0)
        except (TypeError, ValueError):
            pass
        suffix = Path(str(entry.get("path") or "")).suffix.lower()
        if suffix:
            ext_count[suffix] += 1
    languages = []
    for suffix, count in ext_count.most_common(5):
        languages.append({"ext": suffix, "language": LANG_BY_EXT.get(suffix, "other"), "files": count})
    return {"indexedLines": lines, "manifestFiles": len(files), "extensions": languages}


def index_repo(repo, ui, timeout_sec, out):
    if not ui:
        raise Fatal("缺少 ui 可执行文件（--ui 或 UI_BIN）")
    r = run_proc([ui, "index", "--workspace", str(repo)], repo, timeout_sec)
    parsed = parse_json_block(r["stdout"])
    stats = manifest_stats(repo)
    block = {
        "rc": r["rc"],
        "durationMs": r["durationMs"],
        "timedOut": r["timedOut"],
        "files": None,
        "symbols": None,
        "reused": None,
        "updated": None,
        "removed": None,
        "indexedLines": stats["indexedLines"],
        "manifestFiles": stats["manifestFiles"],
        "extensions": stats["extensions"],
        "stdoutTail": tail(r["stdout"]),
        "stderrTail": tail(r["stderr"]),
        "error": None,
    }
    if parsed:
        for key in ("files", "symbols", "reused", "updated", "removed"):
            val = parsed.get(key)
            if isinstance(val, int):
                block[key] = val
    if r["timedOut"]:
        block["error"] = "ui index 超时（%ss）" % timeout_sec
    elif r["rc"] != 0:
        block["error"] = "ui index rc=%s" % r["rc"]
    elif not parsed:
        block["error"] = "ui index 输出无法解析为 JSON（索引可能已落盘，见 manifest 统计）"
    return block


# ---------------------------------------------------------------------------
# 工作区快照（changed files）
# ---------------------------------------------------------------------------

def _under_any(path, roots):
    try:
        here = str(Path(path).resolve())
    except OSError:
        return False
    for root in roots:
        if here == root or here.startswith(root + os.sep):
            return True
    return False


def snapshot_workspace(root, extra_excludes=None):
    started = time.time()
    before = {}
    scanned = 0
    truncated = False
    roots = []
    for item in extra_excludes or []:
        try:
            roots.append(str(Path(item).resolve()))
        except OSError:
            pass
    for dirpath, dirnames, filenames in os.walk(str(root), followlinks=False):
        dirnames[:] = [d for d in dirnames if d not in EXCLUDE_DIRS
                       and not _under_any(Path(dirpath) / d, roots)]
        for filename in filenames:
            path = Path(dirpath) / filename
            if roots and _under_any(path, roots):
                continue
            try:
                st = path.stat()
            except OSError:
                continue
            try:
                rel = path.relative_to(root).as_posix()
            except ValueError:
                rel = path.name
            before[rel] = (st.st_size, st.st_mtime_ns)
            scanned += 1
            if scanned >= MAX_SCAN_FILES:
                truncated = True
                break
        if truncated:
            break
    return before, {"scannedFiles": scanned, "truncated": truncated,
                    "durationMs": int((time.time() - started) * 1000)}


def diff_workspace(before, after):
    added = sorted(set(after) - set(before))
    deleted = sorted(set(before) - set(after))
    modified = sorted(p for p in (set(before) & set(after)) if before[p] != after[p])
    rows = []
    for path in added:
        rows.append({"path": path, "change": "added"})
    for path in modified:
        rows.append({"path": path, "change": "modified"})
    for path in deleted:
        rows.append({"path": path, "change": "deleted"})
    return rows, len(rows)


# ---------------------------------------------------------------------------
# 日志解析（只读本次任务的 journal）
# ---------------------------------------------------------------------------

def parse_journal(path):
    info = {
        "exists": path.is_file(),
        "events": 0,
        "toolStarts": 0,
        "snapshotTokens": 0,
        "lastTurnEnd": None,
        "parseError": None,
    }
    if not path.is_file():
        return info
    try:
        with path.open("r", encoding="utf-8", errors="replace") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                info["events"] += 1
                try:
                    event = json.loads(line)
                except Exception as exc:
                    info["parseError"] = "journal 第 %d 行不是 JSON：%s" % (info["events"], exc)
                    break
                kind = event.get("kind")
                if kind == "tool/start":
                    info["toolStarts"] += 1
                elif kind == "prompt/snapshot":
                    try:
                        info["snapshotTokens"] += int(event.get("tokens") or 0)
                    except (TypeError, ValueError):
                        pass
                elif kind == "turn/end":
                    info["lastTurnEnd"] = event
    except OSError as exc:
        info["parseError"] = "读 journal 失败：%s" % exc
    return info


def parse_status_line(text):
    clean = ANSI_RE.sub("", text or "")
    tokens = re.search(r"\btok\s+(\d+)/", clean)
    calls = re.search(r"\bcalls\s+(\d+)/", clean)
    return {
        "tokens": int(tokens.group(1)) if tokens else None,
        "toolCalls": int(calls.group(1)) if calls else None,
    }


def parse_verify(text):
    clean = ANSI_RE.sub("", text or "")
    out = {"events": None, "turns": None, "toolStarts": None,
           "openTurn": None, "openTools": None,
           "chainOk": "哈希链完整" in clean, "warn": "WARN" in clean}
    m = re.search(r"事件\s+(\d+)", clean)
    if m:
        out["events"] = int(m.group(1))
    m = re.search(r"轮次\s+(\d+)\s+工具调用\s+(\d+)", clean)
    if m:
        out["turns"] = int(m.group(1))
        out["toolStarts"] = int(m.group(2))
    m = re.search(r"未完成 turn=(\w+)\s+tools=(\d+)", clean)
    if m:
        out["openTurn"] = m.group(1).lower() == "true"
        out["openTools"] = int(m.group(2))
    return out


# ---------------------------------------------------------------------------
# expect 判定
# ---------------------------------------------------------------------------

EXPECT_KEYS = {
    "answer_contains", "answer_contains_any", "answer_not_contains",
    "answer_min_chars", "changed", "changed_any", "unchanged",
    "files", "contains", "not_contains",
}


def eval_expect(expect, repo, answer, changed_rows):
    """返回 (passed|None, checks, reason)。passed=None 表示没有可判定项。"""
    if expect is None:
        return None, [], "未提供 expect，不做任务判定"
    checks = []

    def add(name, passed, detail=""):
        checks.append({"check": name, "passed": bool(passed),
                       "detail": str(detail)[:500]})

    changed = {row["path"]: row["change"] for row in changed_rows}
    if isinstance(expect, str):
        add("answer_contains", expect in answer,
            "answer 中%s出现 %r" % ("未" if expect not in answer else "", expect))
    elif isinstance(expect, dict):
        unknown = sorted(set(expect) - EXPECT_KEYS)
        if unknown:
            add("known_keys", False, "未知 expect 字段：%s" % ", ".join(unknown))
        if "answer_contains" in expect:
            needles = expect.get("answer_contains") or []
            if not isinstance(needles, list):
                add("answer_contains", False, "answer_contains 必须是字符串数组")
            else:
                for needle in needles:
                    add("answer_contains:%s" % needle, str(needle) in answer,
                        "answer 中查找 %r" % needle)
        if "answer_contains_any" in expect:
            needles = expect.get("answer_contains_any") or []
            if not isinstance(needles, list) or not needles:
                add("answer_contains_any", False, "answer_contains_any 必须是非空数组")
            else:
                hit = [n for n in needles if str(n) in answer]
                add("answer_contains_any", bool(hit), "命中：%s" % (hit or "无"))
        if "answer_not_contains" in expect:
            needles = expect.get("answer_not_contains") or []
            if not isinstance(needles, list):
                add("answer_not_contains", False, "answer_not_contains 必须是字符串数组")
            else:
                hit = [n for n in needles if str(n) in answer]
                add("answer_not_contains", not hit, "不应出现但出现：%s" % (hit or "无"))
        if "answer_min_chars" in expect:
            try:
                need = int(expect.get("answer_min_chars"))
                add("answer_min_chars", len(answer.strip()) >= need,
                    "answer 长度 %d，要求 >= %d" % (len(answer.strip()), need))
            except (TypeError, ValueError):
                add("answer_min_chars", False, "answer_min_chars 必须是整数")
        if "changed" in expect:
            needed = expect.get("changed") or []
            if not isinstance(needed, list):
                add("changed", False, "changed 必须是字符串数组")
            else:
                missing = [p for p in needed if str(p) not in changed]
                add("changed", not missing, "未变化：%s" % (missing or "无"))
        if "changed_any" in expect:
            needed = expect.get("changed_any") or []
            if not isinstance(needed, list) or not needed:
                add("changed_any", False, "changed_any 必须是非空数组")
            else:
                hit = [p for p in needed if str(p) in changed]
                add("changed_any", bool(hit), "已变化：%s" % (hit or "无"))
        if "unchanged" in expect:
            want = bool(expect.get("unchanged"))
            add("unchanged", (len(changed) == 0) == want,
                "实际变化 %d 个文件，unchanged=%s" % (len(changed), want))
        if "files" in expect:
            needed = expect.get("files") or []
            if not isinstance(needed, list):
                add("files", False, "files 必须是字符串数组")
            else:
                missing, unsafe = [], []
                for rel in needed:
                    try:
                        p = safe_repo_rel(repo, str(rel))
                    except ValueError as exc:
                        unsafe.append(str(exc))
                        continue
                    if not p.is_file():
                        missing.append(str(rel))
                add("files", not missing and not unsafe,
                    "缺失：%s%s" % (missing or "无",
                                    "；不安全路径：%s" % unsafe if unsafe else ""))
        if "contains" in expect:
            specs = expect.get("contains") or []
            if not isinstance(specs, list):
                add("contains", False, "contains 必须是数组")
            else:
                bad = []
                for i, spec in enumerate(specs):
                    if not isinstance(spec, dict) or "path" not in spec or "text" not in spec:
                        bad.append("第 %d 项缺 path/text" % i)
                        continue
                    try:
                        p = safe_repo_rel(repo, str(spec["path"]))
                    except ValueError as exc:
                        bad.append(str(exc))
                        continue
                    if not p.is_file():
                        bad.append("%s 不存在" % spec["path"])
                    elif str(spec["text"]) not in read_text(p):
                        bad.append("%s 中找不到 %r" % (spec["path"], spec["text"]))
                add("contains", not bad, "；".join(bad) if bad else "全部命中")
        if "not_contains" in expect:
            specs = expect.get("not_contains") or []
            if not isinstance(specs, list):
                add("not_contains", False, "not_contains 必须是数组")
            else:
                bad = []
                for i, spec in enumerate(specs):
                    if not isinstance(spec, dict) or "path" not in spec or "text" not in spec:
                        bad.append("第 %d 项缺 path/text" % i)
                        continue
                    try:
                        p = safe_repo_rel(repo, str(spec["path"]))
                    except ValueError as exc:
                        bad.append(str(exc))
                        continue
                    if p.is_file() and str(spec["text"]) in read_text(p):
                        bad.append("%s 中出现了 %r" % (spec["path"], spec["text"]))
                add("not_contains", not bad, "；".join(bad) if bad else "均未出现")
    else:
        add("expect_type", False, "expect 只能是字符串或对象")

    if not checks:
        return None, checks, "expect 为空，不做任务判定"
    passed = all(c["passed"] for c in checks)
    failed = [c["check"] for c in checks if not c["passed"]]
    return passed, checks, ("全部通过" if passed else "未通过：%s" % ", ".join(failed))


# ---------------------------------------------------------------------------
# 单个任务
# ---------------------------------------------------------------------------

def base_task_row(task, index_no):
    return {
        "id": task["id"],
        "promptPreview": task["prompt"][:160],
        "promptChars": len(task["prompt"]),
        "promptSha256": sha256_text(task["prompt"]),
        "expectSpec": task.get("expect"),
        "status": "pending",
        "statusReason": "",
        "error": None,
        "changedFiles": [],
        "changedCount": 0,
        "indexOk": None,
        "run": None,
        "verify": None,
        "taskVerify": None,
        "expect": None,
    }


def skipped_task_row(task):
    row = base_task_row(task, 0)
    row["status"] = "skipped"
    row["statusReason"] = "仓库未执行（占位符/路径无效/索引失败）"
    row["run"] = {"skipped": True, "reason": row["statusReason"]}
    row["verify"] = {"skipped": True, "reason": row["statusReason"]}
    row["taskVerify"] = {"skipped": True, "reason": row["statusReason"]}
    row["expect"] = {"passed": None, "checks": [], "reason": row["statusReason"]}
    return row


def dry_run_task_row(task, reason):
    row = base_task_row(task, 0)
    row["status"] = "dry-run"
    row["statusReason"] = reason
    row["run"] = {"skipped": True, "reason": reason}
    row["verify"] = {"skipped": True, "reason": reason}
    row["taskVerify"] = {"skipped": True, "reason": reason}
    row["expect"] = {"passed": None, "checks": [], "reason": "dry-run 未执行任务，不做 expect 判定"}
    return row


def task_verify_command(task, repo, timeout_sec):
    cmd = task.get("verify")
    if not cmd:
        return {"skipped": True, "reason": "任务未定义 verify 命令"}
    if os.name == "posix":
        argv = ["/bin/sh", "-c", cmd]
    else:
        argv = ["cmd", "/c", cmd]
    r = run_proc(argv, repo, timeout_sec)
    return {
        "skipped": False,
        "rc": r["rc"],
        "durationMs": r["durationMs"],
        "timedOut": r["timedOut"],
        "ok": r["rc"] == 0,
        "stdoutTail": tail(r["stdout"]),
        "stderrTail": tail(r["stderr"]),
    }


def run_task(repo_rec, repo, task, ctx, task_index):
    row = base_task_row(task, task_index)
    row["indexOk"] = True

    safe = re.sub(r"[^A-Za-z0-9._-]+", "-", task["id"])[:40] or "task"
    journal_rel = ".ui/bench/%02d-%s.jsonl" % (task_index, safe)
    journal_path = repo / journal_rel
    try:
        if journal_path.exists():
            journal_path.unlink()
    except OSError:
        pass

    answer_path = ctx["out"] / "answers" / ctx["repo_slug"] / ("%02d-%s.txt" % (task_index, safe))
    answer_path.parent.mkdir(parents=True, exist_ok=True)

    env = dict(os.environ)
    if ctx["provider_cmd"]:
        env["UI_MODEL_CMD"] = ctx["provider_cmd"]

    cmd = [ctx["ui"], "run", "--workspace", str(repo), "--task", task["prompt"],
           "--yes", "--journal", journal_rel, "--result-file", str(answer_path)]
    if ctx["mock"]:
        cmd.extend(["--mock", ctx["mock"]])
    elif ctx["provider"]:
        cmd.extend(["--provider", ctx["provider"]])

    extra_excludes = ([ctx["out"]] if _under_any(ctx["out"], [str(repo.resolve())]) else [])
    before, scan_before = snapshot_workspace(repo, extra_excludes)
    run = run_proc(cmd, repo, ctx["timeout"], env=env)
    after, scan_after = snapshot_workspace(repo, extra_excludes)

    answer = read_text(answer_path, 200000) if answer_path.is_file() else ""
    journal = parse_journal(journal_path)
    status = parse_status_line(run["stdout"])
    tokens = status["tokens"] if status["tokens"] is not None else journal["snapshotTokens"]
    tokens_source = "status_line" if status["tokens"] is not None else "journal_prompt_snapshot_sum"

    last_end = journal["lastTurnEnd"] or {}
    reason = last_end.get("reason") or ""
    steps = last_end.get("steps")
    tool_calls = last_end.get("toolCalls")
    if tool_calls is None:
        tool_calls = status["toolCalls"]
    if tool_calls is None:
        tool_calls = journal["toolStarts"]

    changed_rows, changed_count = diff_workspace(before, after)

    row["run"] = {
        "skipped": False,
        "rc": run["rc"],
        "timedOut": run["timedOut"],
        "durationMs": run["durationMs"],
        "tokens": tokens,
        "tokensSource": tokens_source,
        "toolCalls": tool_calls,
        "toolStarts": journal["toolStarts"],
        "steps": steps,
        "reason": reason,
        "done": run["rc"] == 0 and reason == "done",
        "journal": journal_rel,
        "answerChars": len(answer),
        "answerSha256": sha256_text(answer),
        "answerPreview": answer[:1000],
        "stdoutTail": tail(run["stdout"]),
        "stderrTail": tail(run["stderr"]),
        "scanBeforeMs": scan_before["durationMs"],
        "scanAfterMs": scan_after["durationMs"],
        "scanTruncated": bool(scan_before["truncated"] or scan_after["truncated"]),
        "journalEvents": journal["events"],
        "journalParseError": journal["parseError"],
    }
    row["changedFiles"] = changed_rows[:MAX_CHANGED_RECORDS]
    row["changedCount"] = changed_count

    # verify：即使 run 失败也校验已有日志；日志不存在时明确报 skipped。
    if journal["exists"]:
        vr = run_proc([ctx["ui"], "verify", "--workspace", str(repo),
                       "--journal", journal_rel], repo, ctx["timeout"])
        parsed = parse_verify(vr["stdout"])
        open_turn = parsed["openTurn"]
        open_tools = parsed["openTools"]
        clean = (vr["rc"] == 0 and parsed["chainOk"] and not parsed["warn"]
                 and open_turn is False and open_tools == 0)
        row["verify"] = {
            "skipped": False,
            "rc": vr["rc"],
            "durationMs": vr["durationMs"],
            "timedOut": vr["timedOut"],
            "ok": clean,
            "chainOk": parsed["chainOk"],
            "warn": parsed["warn"],
            "events": parsed["events"],
            "turns": parsed["turns"],
            "toolStarts": parsed["toolStarts"],
            "openTurn": open_turn,
            "openTools": open_tools,
            "stdoutTail": tail(vr["stdout"]),
            "stderrTail": tail(vr["stderr"]),
        }
    else:
        row["verify"] = {
            "skipped": True,
            "rc": None,
            "ok": False,
            "reason": "本次 run 没有生成 journal（%s）" % journal_rel,
        }

    row["taskVerify"] = task_verify_command(task, repo, ctx["timeout"])

    passed, checks, expect_reason = eval_expect(task.get("expect"), repo, answer, changed_rows)
    row["expect"] = {"passed": passed, "checks": checks, "reason": expect_reason}

    failures = []
    if run["timedOut"]:
        failures.append("run 超时")
    elif run["rc"] != 0:
        failures.append("run rc=%s%s" % (run["rc"], "（%s）" % reason if reason else ""))
    if run["rc"] == 0 and reason != "done":
        failures.append("run 结束原因不是 done（%s）" % (reason or "未知"))
    if not row["verify"].get("ok"):
        failures.append("verify 未通过")
    if row["taskVerify"].get("skipped") is False and not row["taskVerify"].get("ok"):
        failures.append("任务 verify rc=%s" % row["taskVerify"].get("rc"))
    if passed is False:
        failures.append("expect 未通过")
    row["failureReasons"] = failures
    row["status"] = "failed" if failures else "success"
    row["statusReason"] = "；".join(failures)
    row["error"] = "；".join(failures) or None
    return row


# ---------------------------------------------------------------------------
# 报告
# ---------------------------------------------------------------------------

def summarize(report):
    repos = report["repos"]
    tasks = [t for r in repos for t in r["tasks"]]
    summary = {
        "repos": {
            "total": len(repos),
            "ok": sum(1 for r in repos if r["status"] == "ok"),
            "skipped": sum(1 for r in repos if r["status"] == "skipped"),
            "invalid": sum(1 for r in repos if r["status"] == "invalid"),
            "error": sum(1 for r in repos if r["status"] == "error"),
        },
        "tasks": {
            "total": len(tasks),
            "success": sum(1 for t in tasks if t["status"] == "success"),
            "failed": sum(1 for t in tasks if t["status"] == "failed"),
            "dryRun": sum(1 for t in tasks if t["status"] == "dry-run"),
            "skipped": sum(1 for t in tasks if t["status"] == "skipped"),
            "error": sum(1 for t in tasks if t["status"] == "error"),
        },
        "index": {
            "ok": sum(1 for r in repos if (r.get("index") or {}).get("error") is None
                      and (r.get("index") or {}).get("rc") == 0),
            "failed": sum(1 for r in repos if (r.get("index") or {}).get("error") is not None),
        },
        "durationMs": report.get("durationMs", 0),
    }
    return summary


def md_escape(value):
    return str(value).replace("|", "\\|").replace("\n", " ")


def write_report_md(report, path):
    lines = []
    mode = report["mode"]
    lines.append("# 真实多仓库评测报告")
    lines.append("")
    lines.append("- 生成时间：%s" % report["generatedAt"])
    lines.append("- 模式：**%s**" % mode)
    lines.append("- 模型：%s" % report["model"]["label"])
    lines.append("- ui：%s" % (report.get("ui", {}).get("version") or "unknown"))
    lines.append("- 耗时：%.1f 秒" % (report.get("durationMs", 0) / 1000.0))
    lines.append("")
    if mode == "dry-run":
        lines.append("> **dry-run：没有执行模型，也没有任务成功结论。**")
        lines.append("> 本报告只验证仓库校验、索引和报告结构；`success=0` 是事实，不是失败率。")
        lines.append("> 要得到真实结果，必须配置 provider 后重跑，并把 commit/模型/时间一起归档。")
        lines.append("")
    else:
        lines.append("> **real 模式：`ui run` 真的执行了。** 结果只对此次 repo commit、")
        lines.append("> provider、任务集和时间成立；`--mock` 的 scripted 结果只验证管线，不代表模型能力。")
        lines.append("")

    s = report["summary"]
    lines.append("## 汇总")
    lines.append("")
    lines.append("| 项 | total | ok/success | skip | fail/invalid | dry-run |")
    lines.append("|---|---:|---:|---:|---:|---:|")
    lines.append("| repos | %d | %d | %d | %d | - |" % (
        s["repos"]["total"], s["repos"]["ok"], s["repos"]["skipped"],
        s["repos"]["invalid"] + s["repos"]["error"]))
    lines.append("| tasks | %d | %d | %d | %d | %d |" % (
        s["tasks"]["total"], s["tasks"]["success"], s["tasks"]["skipped"],
        s["tasks"]["failed"] + s["tasks"]["error"], s["tasks"]["dryRun"]))
    lines.append("")

    for repo in report["repos"]:
        lines.append("## %s" % repo["name"])
        lines.append("")
        lines.append("- path：`%s`" % repo["path"])
        lines.append("- status：**%s**%s" % (
            repo["status"],
            "（%s）" % repo["statusReason"] if repo.get("statusReason") else ""))
        if repo.get("commit"):
            lines.append("- commit：`%s`" % repo["commit"])
        if repo.get("dirtyFiles") is not None:
            lines.append("- git 未提交文件：%s（仅 `-uno` 跟踪文件）" % repo["dirtyFiles"])
        if repo.get("language"):
            lines.append("- language（声明）：%s" % repo["language"])
        if repo.get("languages"):
            seen = "、".join("%s(%s)" % (x["language"], x["files"]) for x in repo["languages"])
            lines.append("- 索引识别：%s" % seen)
        if repo.get("locDeclared") is not None:
            lines.append("- loc（声明）：%d" % repo["locDeclared"])
        idx = repo.get("index")
        if idx:
            lines.append("- index：rc=%s · files=%s · symbols=%s · lines=%s · %d ms" % (
                idx.get("rc"), idx.get("files"), idx.get("symbols"),
                idx.get("indexedLines"), idx.get("durationMs")))
            if idx.get("error"):
                lines.append("- index error：%s" % idx["error"])
        lines.append("")
        if not repo["tasks"]:
            continue
        lines.append("| task | status | run rc | tokens | calls | changed | verify | expect | ms |")
        lines.append("|---|---|---:|---:|---:|---:|---|---|---:|")
        for task in repo["tasks"]:
            run = task.get("run") or {}
            verify = task.get("verify") or {}
            expect = task.get("expect") or {}
            expect_txt = "n/a"
            if expect.get("passed") is True:
                expect_txt = "pass"
            elif expect.get("passed") is False:
                expect_txt = "FAIL"
            verify_txt = "skip" if verify.get("skipped") else (
                "ok" if verify.get("ok") else "FAIL rc=%s" % verify.get("rc"))
            lines.append("| %s | %s | %s | %s | %s | %s | %s | %s | %s |" % (
                md_escape(task["id"]), md_escape(task["status"]),
                "" if run.get("rc") is None else run.get("rc"),
                "" if run.get("tokens") is None else run.get("tokens"),
                "" if run.get("toolCalls") is None else run.get("toolCalls"),
                task.get("changedCount", 0),
                verify_txt, expect_txt,
                run.get("durationMs", "")))
            if task.get("failureReasons"):
                lines.append("")
                lines.append("  - **%s 失败原因**：%s" % (task["id"], "；".join(task["failureReasons"])))
        lines.append("")

    lines.append("## 说明与边界")
    lines.append("")
    for note in report.get("notes", []):
        lines.append("- %s" % note)
    lines.append("- `expect.passed=null` 表示没有 expect 或处于 dry-run，不是“通过”。")
    lines.append("- 本报告不出具“通过/不通过”总评，也不与 SWE-bench 等榜单比较。")
    lines.append("")
    path.write_text("\n".join(lines), encoding="utf-8")


def write_report(report, out):
    out.mkdir(parents=True, exist_ok=True)
    report["summary"] = summarize(report)
    json_path = out / "report.json"
    md_path = out / "report.md"
    json_path.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    write_report_md(report, md_path)
    return json_path, md_path


# ---------------------------------------------------------------------------
# 主流程
# ---------------------------------------------------------------------------

def resolve_ui(value):
    if not value:
        return None
    p = Path(value).expanduser()
    if p.is_absolute():
        return str(p)
    if os.sep in value or "/" in value:
        return str(p.resolve())
    return value


def repo_slug(name):
    return re.sub(r"[^A-Za-z0-9._-]+", "-", name)[:60] or "repo"


def run_bench(args):
    started = time.time()
    out = Path(args.out).expanduser().resolve() if args.out else (
        Path(__file__).resolve().parent.parent / "bench" / "out"
        / ("run-" + datetime.datetime.now().strftime("%Y%m%d-%H%M%S")))
    repos_raw = load_repos(args.repos)
    tasks = load_tasks(args.tasks)
    repos_base = Path(args.repos).expanduser().resolve().parent
    ui = resolve_ui(args.ui or os.environ.get("UI_BIN"))

    if args.only:
        wanted = set(x.strip() for x in args.only.split(",") if x.strip())
        repos_raw = [r for r in repos_raw if r["name"] in wanted]
        if not repos_raw:
            raise Fatal("--only 没有匹配到任何仓库：%s" % args.only)
    if args.task:
        wanted = set(x.strip() for x in args.task.split(",") if x.strip())
        tasks = [t for t in tasks if t["id"] in wanted]
        if not tasks:
            raise Fatal("--task 没有匹配到任何任务：%s" % args.task)

    provider_cmd = args.provider_cmd
    mock = str(Path(args.mock).expanduser().resolve()) if args.mock else None
    if mock and not Path(mock).is_file():
        raise Fatal("--mock 文件不存在：%s" % mock)
    if provider_cmd and mock:
        raise Fatal("--provider-cmd 与 --mock 只能二选一")
    if args.provider and (provider_cmd or mock):
        raise Fatal("--provider 与 --provider-cmd/--mock 只能二选一")

    dry_run = bool(args.dry_run) or not (provider_cmd or mock or args.provider)
    dry_run_reason = ""
    if args.dry_run:
        dry_run_reason = "显式 --dry-run"
    elif not (provider_cmd or mock or args.provider):
        dry_run_reason = "未提供 provider（--provider-cmd / --mock / --provider）"
    mode = "dry-run" if dry_run else "real"

    if dry_run:
        model = {"kind": "none", "label": "none（dry-run，不调用模型）"}
    elif mock:
        model = {"kind": "scripted", "label": "scripted mock（仅验证管线，不代表模型能力）"}
    elif args.provider:
        model = {"kind": "external", "label": "provider:%s" % args.provider}
    else:
        model = {"kind": "external", "label": "UI_MODEL_CMD（命令原文不落盘）"}

    ui_version = None
    report = {
        "schema": SCHEMA,
        "generatedAt": iso_now(),
        "mode": mode,
        "dryRunReason": dry_run_reason,
        "model": model,
        "ui": {"bin": ui or "", "version": None},
        "limits": {"timeoutSec": args.timeout},
        "repos": [],
        "summary": {},
        "notes": [],
        "durationMs": 0,
    }

    valid_repos = []
    for raw in repos_raw:
        status, resolved, reason = validate_repo(raw["path"], repos_base)
        rec = {
            "name": raw["name"],
            "path": str(resolved) if resolved else raw["path"],
            "rawPath": raw["path"],
            "commit": raw["commit"] or None,
            "dirtyFiles": None,
            "language": raw["language"] or "",
            "languages": [],
            "locDeclared": raw["loc"],
            "status": status,
            "statusReason": reason,
            "error": reason if status == "invalid" else None,
            "index": None,
            "tasks": [],
        }
        if status == "skipped":
            eprint("SKIP repo=%s path=%s（%s）" % (rec["name"], raw["path"], reason))
            rec["tasks"] = [skipped_task_row(t) for t in tasks]
        elif status == "invalid":
            eprint("INVALID repo=%s path=%s（%s）" % (rec["name"], raw["path"], reason))
            rec["tasks"] = [skipped_task_row(t) for t in tasks]
        else:
            valid_repos.append((raw, resolved, rec))
        report["repos"].append(rec)

    if valid_repos:
        if not ui:
            found = shutil.which("ui")
            if not found:
                raise Fatal("有可执行的仓库，但找不到 ui 二进制；用 --ui 指定，或设置 UI_BIN")
            ui = found
        elif os.sep not in ui and "/" not in ui and not shutil.which(ui):
            raise Fatal("--ui 给的是命令名 `%s`，但 PATH 里找不到；请给绝对路径" % ui)

    if valid_repos:
        ver = run_proc([ui, "--version"], Path.cwd(), 15)
        if ver["rc"] != 0:
            raise Fatal("ui 无法运行（%s）：%s" % (ui, tail(ver["stderr"] or ver["stdout"], 800)))
        ui_version = (ver["stdout"].strip().splitlines() or [""])[0][:200] or None
        report["ui"] = {"bin": ui, "version": ui_version}

    for raw, resolved, rec in valid_repos:
        rec["tasks"] = []
        try:
            idx = index_repo(resolved, ui, args.timeout, out)
        except Fatal:
            raise
        except Exception as exc:  # noqa: BLE001
            idx = {"rc": None, "durationMs": 0, "timedOut": False, "files": None,
                   "symbols": None, "error": "索引异常：%s" % exc, "indexedLines": 0}
        rec["index"] = idx
        if idx.get("error"):
            rec["status"] = "error"
            rec["statusReason"] = idx["error"]
            rec["error"] = idx["error"]
            eprint("ERROR repo=%s index：%s" % (rec["name"], idx["error"]))
            rec["tasks"] = [skipped_task_row(t) for t in tasks]
            continue

        rec["status"] = "ok"
        detected_commit, dirty, _ = git_meta(resolved)
        rec["commit"] = raw["commit"] or detected_commit
        rec["dirtyFiles"] = dirty
        stats = manifest_stats(resolved)
        rec["languages"] = stats["extensions"]
        if not rec["language"] and stats["extensions"]:
            rec["language"] = stats["extensions"][0]["language"]
        rec["indexOk"] = True
        eprint("INDEX repo=%s files=%s symbols=%s lines=%s %dms" % (
            rec["name"], idx.get("files"), idx.get("symbols"),
            idx.get("indexedLines"), idx.get("durationMs")))

        if dry_run:
            rec["tasks"] = [dry_run_task_row(t, dry_run_reason) for t in tasks]
            for row in rec["tasks"]:
                row["indexOk"] = True
            continue

        ctx = {
            "ui": ui,
            "timeout": args.timeout,
            "provider_cmd": provider_cmd,
            "mock": mock,
            "provider": args.provider,
            "out": out,
            "repo_slug": repo_slug(rec["name"]),
        }
        for i, task in enumerate(tasks, start=1):
            try:
                row = run_task(rec, resolved, task, ctx, i)
            except Exception as exc:  # noqa: BLE001 - 单任务失败不能终止全部
                row = base_task_row(task, i)
                row["status"] = "error"
                row["statusReason"] = "任务执行异常：%s" % exc
                row["error"] = row["statusReason"]
                row["run"] = {"skipped": True, "reason": row["statusReason"]}
                row["verify"] = {"skipped": True, "reason": row["statusReason"]}
                row["taskVerify"] = {"skipped": True, "reason": row["statusReason"]}
                row["expect"] = {"passed": None, "checks": [], "reason": row["statusReason"]}
                eprint("ERROR repo=%s task=%s：%s" % (rec["name"], task["id"], exc))
            rec["tasks"].append(row)
            eprint("TASK repo=%s task=%s status=%s run_rc=%s tokens=%s calls=%s changed=%s" % (
                rec["name"], task["id"], row["status"],
                (row.get("run") or {}).get("rc"),
                (row.get("run") or {}).get("tokens"),
                (row.get("run") or {}).get("toolCalls"),
                row.get("changedCount")))

    if dry_run:
        report["notes"] = [
            "dry-run：%s。本报告没有执行任何 ui run，success=0 不是任务失败率。" % dry_run_reason,
            "dry-run 仍会执行 ui index 并记录文件/符号/耗时；任务的行状态是 dry-run，不是成功。",
            "要得到真实结果：配置 provider（--provider-cmd/--mock/--provider）后去掉 --dry-run 重跑。",
            "--mock 是脚本化模型，只验证管线，不是模型能力评测。",
            "占位符 REPLACE_ME 的仓库被 SKIP，不产生结果。",
            "报告不记录 provider 命令原文、环境变量或 API key；只记录 UI_BIN 与版本。",
        ]
    else:
        report["notes"] = [
            "real 模式：ui run 实际执行；结果只对本次 repo commit、provider、任务集和运行时间成立。",
            "单任务失败被隔离并记录，不会中断其他任务；任务失败是评测数据，不等同于工具崩溃。",
            "changedFiles 由运行前后工作区 size+mtime 快照得出，不读取 git 历史；被 .gitignore 的文件也算。",
            "tokens 优先取 ui run 状态行，取不到才用 journal 中 prompt/snapshot 的 tokens 求和。",
            "报告不记录 provider 命令原文、环境变量或 API key；只记录 UI_BIN 与版本。",
        ]

    report["durationMs"] = int((time.time() - started) * 1000)
    json_path, md_path = write_report(report, out)
    summary = report["summary"]

    print("报告 %s" % json_path)
    print("     %s" % md_path)
    print("mode=%s · repos ok=%d skipped=%d invalid=%d error=%d · tasks success=%d failed=%d dry-run=%d skipped=%d error=%d" % (
        mode,
        summary["repos"]["ok"], summary["repos"]["skipped"],
        summary["repos"]["invalid"], summary["repos"]["error"],
        summary["tasks"]["success"], summary["tasks"]["failed"],
        summary["tasks"]["dryRun"], summary["tasks"]["skipped"],
        summary["tasks"]["error"]))
    if mode == "dry-run":
        print("dry-run：没有执行模型，不能把本报告当作真实评测结果。")

    harness_error = (summary["repos"]["invalid"] + summary["repos"]["error"] > 0
                     or summary["tasks"]["error"] > 0)
    return 2 if harness_error else 0


def build_parser():
    p = argparse.ArgumentParser(
        description="真实多仓库评测框架（index / run / verify / expect，全部记录实际观察值）")
    p.add_argument("--repos", required=True, help="repos JSON（数组：name/path/commit?/language?/loc?）")
    p.add_argument("--tasks", required=True, help="tasks JSON（数组：id/prompt/expect?/verify?）")
    p.add_argument("--ui", help="ui 可执行文件路径（默认 UI_BIN 或 PATH 里的 ui）")
    p.add_argument("--provider", help="传给 ui run 的 --provider 名字（仓库需有 .ui/providers.json）")
    p.add_argument("--provider-cmd", help="设置 UI_MODEL_CMD 的模型命令；原文不写入报告")
    p.add_argument("--mock", help="传给 ui run 的 --mock 剧本文件（scripted，仅验证管线）")
    p.add_argument("--out", help="输出目录（默认 bench/out/run-<时间戳>）")
    p.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT,
                   help="单次子进程超时秒数（默认 %d）" % DEFAULT_TIMEOUT)
    p.add_argument("--dry-run", action="store_true",
                   help="强制只做 index，不执行 ui run（即使配了 provider）")
    p.add_argument("--only", help="只跑这些仓库名，逗号分隔")
    p.add_argument("--task", help="只跑这些任务 id，逗号分隔")
    return p


def main(argv):
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.timeout <= 0:
        parser.error("--timeout 必须是正整数")
    try:
        return run_bench(args)
    except Fatal as exc:
        eprint("ERROR: %s" % exc)
        return 3
    except KeyboardInterrupt:
        eprint("中断：未写报告")
        return 130


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
