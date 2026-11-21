"""The Rust client must agree with the last Python client byte for byte.

The Python reference is extracted from git history at REFERENCE, so the
comparison keeps running after the Python code left the tree; the test skips
when that commit is not available. Each case builds a repository state, runs
both implementations on it, and requires identical output: fingerprints,
status reports, review-page payloads, and document renderings. Approvals
recorded by one implementation must verify under the other.
"""
from __future__ import annotations

import atexit
import json
import os
import random
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
REFERENCE = "7659d0c784c15cbb700e3a4fa6b445986beb6f2c"
sys.path.insert(0, str(ROOT / "tests"))

import commands  # noqa: E402


def extract_reference() -> "Path | None":
    archive = subprocess.run(["git", "-C", str(ROOT), "archive", REFERENCE, "scripts", "assets"],
                             capture_output=True)
    if archive.returncode != 0:
        return None
    folder = Path(tempfile.mkdtemp(prefix="senpai-reference-"))
    atexit.register(shutil.rmtree, folder, True)
    subprocess.run(["tar", "-x", "-C", str(folder)], input=archive.stdout, check=True)
    return folder


REFERENCE_DIR = extract_reference()
if REFERENCE_DIR is not None:
    sys.path.insert(0, str(REFERENCE_DIR / "scripts"))
    import html_review  # noqa: E402
    import task_review  # noqa: E402
    PYTHON_CLIENT = [sys.executable, str(REFERENCE_DIR / "scripts/task_review.py")]
GIT_ENV = {
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_SYSTEM": os.devnull,
    "GIT_TERMINAL_PROMPT": "0",
    "GIT_AUTHOR_NAME": "Parity",
    "GIT_AUTHOR_EMAIL": "parity@example.com",
    "GIT_COMMITTER_NAME": "Parity",
    "GIT_COMMITTER_EMAIL": "parity@example.com",
}
NAMES = [
    "a.py", "README.md", "src/main.rs", "src/lib/util.go", "docs/plan notes.md", "ünïcödé.txt",
    "emoji-😀.txt", "space dir/file.txt", "deep/a/b/c/d.txt", "Makefile", "bin/tool.sh",
    "data.bin", "src/lib/zeta.ts", "src/lib/Alpha.ts", "tab\tname.txt", "日本語/ファイル.md",
]
APPROVE = '{"version":"1","review_id":"__REVIEW_ID__","decision":"approve","annotations":[]}'
STUB = r'''#!/usr/bin/env python3
import json, sys
request = json.loads(sys.argv[2])
print(json.dumps({"version": "1", "review_id": request["review_id"], "decision": "approve",
                  "annotations": [{"text": "kept", "target": {"scope": "review"}}]}))
'''


def content(rng: random.Random) -> bytes:
    kind = rng.choice(["text", "text", "text", "crlf", "nonl", "empty", "binary", "unicode", "long"])
    if kind == "text":
        return "".join(f"line {rng.randint(0, 99)}\n" for _ in range(rng.randint(1, 12))).encode()
    if kind == "crlf":
        return b"one\r\ntwo\r\n"
    if kind == "nonl":
        return b"no trailing newline"
    if kind == "empty":
        return b""
    if kind == "binary":
        return bytes(rng.randrange(256) for _ in range(rng.randint(1, 64))) + b"\0"
    if kind == "unicode":
        return "café   sep \x0c feed\nnaïve\n".encode()
    return ("x" * 200 + "\n").encode() * rng.randint(1, 30)


@unittest.skipIf(REFERENCE_DIR is None, f"the Python reference {REFERENCE[:7]} is not in this clone")
class RustParityTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.base = Path(os.path.realpath(self._tmp.name))
        self.env = {**os.environ, **GIT_ENV, task_review.TIMEOUT_ENV: "60"}
        for name in (task_review.REVIEWER_ENV, task_review.LAUNCHER_ENV):
            self.env.pop(name, None)

    def git(self, root: Path, *args: str | bytes, input: bytes | None = None) -> bytes:
        return subprocess.run(["git", "-C", str(root), *args], check=True, capture_output=True,
                              env=self.env, input=input).stdout

    def build(self, seed: int) -> Path:
        rng = random.Random(seed)
        root = self.base / f"repo{seed}"
        root.mkdir()
        self.git(root, "init", "-q", "-b", "main", ".")
        tracked = rng.sample(NAMES, rng.randint(3, len(NAMES)))
        for name in tracked:
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content(rng))
            if rng.random() < 0.2:
                path.chmod(0o755)
        if rng.random() < 0.3:
            (root / "link").symlink_to(tracked[0])
        empty_head = rng.random() < 0.1
        if not empty_head:
            self.git(root, "add", "-A")
            self.git(root, "commit", "-q", "-m", "initial")
        if rng.random() < 0.3 and not empty_head:
            blob = self.git(root, "hash-object", "-w", "--stdin", input=b"latin-1 name\n").decode().strip()
            self.git(root, "update-index", "--add", "--cacheinfo", f"100644,{blob},caf\xe9.txt".encode("latin-1"))
            self.git(root, "commit", "-q", "-m", "a name that is not UTF-8")
        for name in tracked:
            path = root / name
            roll = rng.random()
            if roll < 0.25:
                path.write_bytes(content(rng))
            elif roll < 0.35:
                path.unlink()
            elif roll < 0.45 and not empty_head:
                target = root / f"moved/{Path(name).name}"
                target.parent.mkdir(exist_ok=True)
                self.git(root, "mv", name, str(target.relative_to(root)))
            elif roll < 0.5:
                path.chmod(0o644 if os.access(path, os.X_OK) else 0o755)
            if rng.random() < 0.15 and path.exists():
                self.git(root, "add", name)
        for index in range(rng.randint(0, 4)):
            path = root / rng.choice(["new", "src/new", "untracked dir"]) / f"file{index}{rng.choice(['.py', '.md', ''])}"
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content(rng))
        if rng.random() < 0.2:
            (root / "dangling").symlink_to("nowhere")
        return root

    def both(self, *args: str, **extra: str) -> tuple[subprocess.CompletedProcess, subprocess.CompletedProcess]:
        env = {**self.env, **extra}
        python = subprocess.run([*PYTHON_CLIENT, *args], capture_output=True, env=env, timeout=120)
        rust = subprocess.run([commands.BINARY, *args], capture_output=True, env=env, timeout=120)
        return python, rust

    def assertSame(self, python: subprocess.CompletedProcess, rust: subprocess.CompletedProcess, label: str) -> None:
        self.assertEqual(python.returncode, rust.returncode, f"{label}: {python.stderr!r} / {rust.stderr!r}")
        self.assertEqual(python.stdout, rust.stdout, label)

    def scopes(self, root: Path) -> list[list[str]]:
        changed = [line.split("\t")[-1] for line in self.git(root, "status", "--porcelain").decode("utf-8", "replace").splitlines()]
        top = sorted({name.strip('"').split("/")[0] for name in changed if name})
        return [[], *([[top[0]]] if top else []), *([top[:2]] if len(top) > 1 else [])]

    def test_status_and_preflight_agree_on_generated_states(self) -> None:
        for seed in range(60):
            root = self.build(seed)
            for scope in self.scopes(root):
                with self.subTest(seed=seed, scope=scope):
                    paths = ["--paths", ",".join(scope)] if scope else []
                    self.assertSame(*self.both("status", "--repo", str(root), *paths), f"status {seed} {scope}")
                    self.assertSame(
                        *self.both("preflight", "--criterion", "c", "--repo", str(root), *paths),
                        f"preflight {seed} {scope}",
                    )

    def test_multi_repository_fingerprints_agree(self) -> None:
        first, second = self.build(101), self.build(102)
        request = json.dumps({"version": "1", "review_id": "r", "criterion": "c",
                              "repositories": [{"root": str(first)}, {"root": str(second), "paths": ["src"]}]})
        self.assertSame(*self.both("status", "--json", request), "multi-repository status")

    def served_payload(self, flag: str, request: str) -> dict:
        url_file = self.base / f"url-{time.monotonic_ns()}"
        env = {**self.env, "HTML_REVIEW_NO_BROWSER": "1", "HTML_REVIEW_URL_FILE": str(url_file)}
        process = subprocess.Popen([commands.BINARY, "html-review", flag, request], env=env,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.addCleanup(lambda: process.poll() is None and process.kill())
        deadline = time.monotonic() + 30
        while not url_file.exists() or not url_file.read_text().strip():
            if process.poll() is not None:
                self.fail(process.communicate()[1].decode(errors="replace"))
            self.assertLess(time.monotonic(), deadline, "the Rust reviewer never published its page")
            time.sleep(0.05)
        url = url_file.read_text().strip()
        api = "api/doc" if flag == "--review-doc" else "api/review"
        with urllib.request.urlopen(url + api, timeout=30) as response:
            body = response.read()
        cancel = urllib.request.Request(url + ("api/doc-decision" if flag == "--review-doc" else "api/decision"),
                                        data=b'{"decision":"cancelled","annotations":[]}', method="POST",
                                        headers={"Content-Type": "application/json"})
        urllib.request.urlopen(cancel, timeout=30).read()
        process.communicate(timeout=30)
        return body

    def test_review_page_payloads_agree(self) -> None:
        for seed in range(0, 60, 6):
            root = self.build(seed)
            with self.subTest(seed=seed):
                request = task_review.build_request("ship it", [task_review.Repository(root=str(root))])
                expected = json.dumps(html_review.build_payload(request), separators=(",", ":")).encode()
                self.assertEqual(expected, self.served_payload("--review", request.to_json()))

    def documents(self) -> list[Path]:
        corpus = [path for path in (ROOT / "docs").rglob("*.md")] if (ROOT / "docs").exists() else []
        corpus += [ROOT / "README.md", ROOT / "assets/PROVENANCE.md"]
        extra = os.environ.get("SENPAI_MARKDOWN_CORPUS")
        if extra:
            corpus += sorted(Path(extra).rglob("*.md"))[:400]
        return corpus

    def test_document_payloads_agree(self) -> None:
        samples = self.base / "samples"
        samples.mkdir()
        kitchen = samples / "kitchen.md"
        kitchen.write_text(
            "﻿# Title ##\r\n\r\n- [x] done *it* **b** ~~s~~ `c` [l](u)\n  wrapped\n1) one\n> quote\n"
            "| a | b |\n|---|---|\n***\n```py\ncode\n\n````\n```\nafter __bold__ snake_case\n\x1c\n x\n",
            encoding="utf-8",
        )
        (samples / "latin.md").write_bytes(b"caf\xe9 *x*\n")
        for path in [kitchen, samples / "latin.md", *self.documents()]:
            with self.subTest(document=str(path)):
                request = task_review.DocumentRequest(version="1", review_id="r", criterion="c", path=str(path))
                expected = json.dumps(html_review.build_document_payload(request), separators=(",", ":")).encode()
                self.assertEqual(expected, self.served_payload("--review-doc", request.to_json()))

    def test_approvals_verify_across_implementations(self) -> None:
        stub = self.base / "stub"
        stub.write_text(STUB)
        stub.chmod(0o755)
        for writer, reader in ((PYTHON_CLIENT, [commands.BINARY]), ([commands.BINARY], PYTHON_CLIENT)):
            root = self.build(200 + len(writer))
            env = {**self.env, task_review.REVIEWER_ENV: str(stub)}
            reviewed = subprocess.run([*writer, "review", "--criterion", "c", "--repo", str(root)],
                                      capture_output=True, env=env, timeout=120)
            self.assertEqual(0, reviewed.returncode, reviewed.stderr)
            verified = subprocess.run([*reader, "verify", "--repo", str(root)], capture_output=True, env=env, timeout=120)
            self.assertEqual(0, verified.returncode, verified.stdout + verified.stderr)
            consumed = subprocess.run([*reader, "consume", "--repo", str(root)], capture_output=True, env=env, timeout=120)
            self.assertEqual(0, consumed.returncode, consumed.stderr)
            again = subprocess.run([*writer, "verify", "--repo", str(root)], capture_output=True, env=env, timeout=120)
            self.assertEqual(1, again.returncode, "a consumed approval must not verify in either implementation")

            document = root / "plan.md"
            document.write_text("# Plan\n\n- step\n")
            reviewed = subprocess.run([*writer, "review-doc", "--criterion", "plan ok", "--doc", str(document)],
                                      capture_output=True, env={**env, task_review.REVIEWER_ENV: str(stub)}, timeout=120)
            self.assertEqual(0, reviewed.returncode, reviewed.stderr)
            verified = subprocess.run([*reader, "verify-doc", "--doc", str(document)], capture_output=True, env=env, timeout=120)
            self.assertEqual(0, verified.returncode, verified.stdout + verified.stderr)


if __name__ == "__main__":
    unittest.main()
