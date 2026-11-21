"""Which implementation the command-level tests drive.

With ``SENPAI_BINARY`` set, the tests that run the client or the reviewer as a
process run that binary instead of the Python scripts, so one suite checks
that both implementations keep the same contract.
"""
from __future__ import annotations

import os
import shlex
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BINARY = os.environ.get("SENPAI_BINARY", "")


def client() -> list[str]:
    return [BINARY] if BINARY else [sys.executable, str(ROOT / "scripts/task_review.py")]


def reviewer() -> list[str]:
    return [BINARY, "html-review"] if BINARY else [sys.executable, str(ROOT / "scripts/html_review.py")]


def reviewer_launcher() -> str:
    """A TASK_REVIEW_REVIEWER / RDG_CONTROL_COMMAND value naming the reviewer."""
    return shlex.join(reviewer()) if BINARY else str(ROOT / "scripts/html_review.py")
