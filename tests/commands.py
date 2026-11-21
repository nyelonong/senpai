"""The senpai binary under test and the contract values its callers rely on.

``SENPAI_BINARY`` overrides the binary; by default the tests run the release
build, so run ``cargo build --release`` first.
"""
from __future__ import annotations

import json
import os
import shlex
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
BINARY = os.environ.get("SENPAI_BINARY") or str(ROOT / "target/release/senpai")

EXIT_OK = 0
EXIT_NOT_APPROVED = 1
EXIT_ERROR = 2

PROTOCOL_VERSION = "1"
PRODUCER = "task_review.py"
REVIEWER_PRODUCER = "html_review.py"
LEDGER_NAME = "task-review-approval.jsonl"
EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
MAX_GUIDE_BYTES = 48 * 1024

REVIEWER_ENV = "TASK_REVIEW_REVIEWER"
LAUNCHER_ENV = "RDG_CONTROL_COMMAND"
TIMEOUT_ENV = "TASK_REVIEW_TIMEOUT"
NO_BROWSER_ENV = "HTML_REVIEW_NO_BROWSER"
URL_FILE_ENV = "HTML_REVIEW_URL_FILE"
LIFETIME_ENV = "HTML_REVIEW_LIFETIME"
IDLE_ENV = "HTML_REVIEW_IDLE"


def client() -> list[str]:
    return [BINARY]


def reviewer() -> list[str]:
    return [BINARY, "html-review"]


def reviewer_launcher() -> str:
    """A TASK_REVIEW_REVIEWER / RDG_CONTROL_COMMAND value naming the reviewer."""
    return shlex.join(reviewer())


def read_entries(path: "str | Path") -> list[dict[str, Any]]:
    """Every well-formed ledger line, skipping malformed ones as the client does."""
    try:
        text = Path(path).read_text(encoding="utf-8", errors="replace")
    except FileNotFoundError:
        return []
    entries = []
    for line in text.splitlines():
        try:
            value = json.loads(line.strip()) if line.strip() else None
        except ValueError:
            continue
        if isinstance(value, dict):
            entries.append(value)
    return entries
