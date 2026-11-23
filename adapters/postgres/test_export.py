import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent


class ExportTest(unittest.TestCase):
    def test_export_escapes_and_preserves_hash_fields(self):
        with tempfile.TemporaryDirectory() as d:
            j = Path(d) / "journal.jsonl"
            rows = [
                {
                    "seq": 1,
                    "ts": 1790000000000,
                    "kind": "turn/start",
                    "turn": 1,
                    "prompt": "a\tb\nc",
                    "prev": "",
                    "hash": "h1",
                },
                {
                    "seq": 2,
                    "ts": 1790000001000,
                    "kind": "approval/answered",
                    "approvalId": "ap-1",
                    "verdict": "deny",
                    "prev": "h1",
                    "hash": "h2",
                },
            ]
            j.write_text("".join(json.dumps(r) + "\n" for r in rows), encoding="utf-8")
            p = subprocess.run(
                [
                    sys.executable,
                    str(HERE / "export_journal.py"),
                    "--journal",
                    str(j),
                    "--session",
                    "s1",
                    "--workspace",
                    "w1",
                ],
                text=True,
                capture_output=True,
                check=True,
            )
            out = p.stdout
            self.assertIn("COPY ui_events", out)
            self.assertIn("w1\ts1\t1\tturn/start", out)
            # JSON 内已经是 \t\n，COPY 层再把反斜杠转义成 \\t\\n。
            self.assertIn("a\\\\tb\\\\nc", out)
            self.assertIn("h1\th2", out)
            self.assertIn("\\.", out)

    def test_bad_line_fails_closed(self):
        with tempfile.TemporaryDirectory() as d:
            j = Path(d) / "journal.jsonl"
            j.write_text("{not-json}\n", encoding="utf-8")
            p = subprocess.run(
                [sys.executable, str(HERE / "export_journal.py"), "--journal", str(j)],
                text=True,
                capture_output=True,
            )
            self.assertNotEqual(p.returncode, 0)
            self.assertIn("journal.jsonl:1", p.stderr)


if __name__ == "__main__":
    unittest.main()
