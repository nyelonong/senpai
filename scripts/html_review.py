#!/usr/bin/env python3
"""A second reviewer backend for the task-review protocol: one local HTML page.

``RDG_CONTROL_COMMAND=/path/to/html_review.py`` makes the pre-commit gate in
``scripts/task_review.py`` open this reviewer instead of RDG. Both speak review
protocol v1 -- one JSON request on argv or stdin, one JSON result line on
stdout -- so the client cannot tell the two apart and either one can be chosen.

Shape of a run:

    html_review.py --review '{"version":"1", ...}'   # or the request on stdin
    -> validate the request exactly like RDG's ReviewRequest::validate
    -> collect the diff with the client's own snapshot code
    -> serve one self-contained page on 127.0.0.1 behind a random token
    -> wait for a human decision
    -> print exactly one JSON result line, exit 0

The diff comes from ``task_review`` rather than from a second implementation of
git plumbing, so this reviewer can never show a different change than the one
the client fingerprints and re-verifies.

A request it cannot understand is a diagnostic on stderr and a non-zero exit. A
window closed without a decision is ``cancelled`` with no annotations, which the
client treats as "no approval": this backend fails closed like the client does.

Environment:

    HTML_REVIEW_NO_BROWSER=1   serve the page without opening it; the URL is
                               printed on stderr either way
    HTML_REVIEW_URL_FILE=PATH  publish the live page URL to a private file for
                               callers that cannot read the streamed stderr;
                               remove the file when the review ends
"""
from __future__ import annotations

import argparse
import json
import os
import re
import secrets
import sys
import threading
import time
import urllib.parse
import webbrowser
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Callable, Sequence

_SCRIPTS = os.path.dirname(os.path.abspath(__file__))
if _SCRIPTS not in sys.path:
    sys.path.insert(0, _SCRIPTS)

import markdown_doc  # noqa: E402  (line-oriented rendering, beside this script)
import task_review  # noqa: E402  (the client lives beside this script and is not a package)

PROTOCOL_VERSION = task_review.PROTOCOL_VERSION
PRODUCER = "html_review.py"
HOST = "127.0.0.1"

# Set to 1 (or any truthy word) to keep the review page from being opened. Tests
# set it; a human who prefers to paste the printed URL can too.
NO_BROWSER_ENV = "HTML_REVIEW_NO_BROWSER"

# The live URL file supports callers that cannot read streamed diagnostics.
# It is written 0600 while the review runs and removed when it ends.
URL_FILE_ENV = "HTML_REVIEW_URL_FILE"

# A review that nobody can finish must not outlive its usefulness: a client
# killed with SIGKILL cannot clean this process up, so the reviewer bounds
# itself. The lifetime matches the client's own default, and the idle bound
# catches a page that was printed but never opened.
LIFETIME_SECONDS = 1800.0
IDLE_SECONDS = 900.0
LIFETIME_ENV = "HTML_REVIEW_LIFETIME"
IDLE_ENV = "HTML_REVIEW_IDLE"

# A page that is closing is either gone or about to be replaced by a reload, and
# a browser may report the close before or after it requests the replacement.
# Each page served carries a number; a close withdraws the review only when it
# names the newest page and no newer page is requested within this long.
CLOSE_GRACE_SECONDS = 5.0

EXIT_OK = 0
EXIT_ERROR = 2

# JetBrains Mono, SIL Open Font License 1.1 (see assets/fonts/OFL.txt). It ships
# with the reviewer and is served from this same origin rather than fetched, so
# the page keeps working offline and its CSP still names no third party.
ASSET_FONTS = Path(__file__).resolve().parents[1] / "assets" / "fonts"
FONT_FILES = {
    "jetbrains-mono-400.woff2": ASSET_FONTS / "jetbrains-mono-400.woff2",
    "jetbrains-mono-700.woff2": ASSET_FONTS / "jetbrains-mono-700.woff2",
    "inter-latin-var.woff2": ASSET_FONTS / "inter-latin-var.woff2",
    "source-serif-4-latin-var.woff2": ASSET_FONTS / "source-serif-4-latin-var.woff2",
}

# Third-party code is vendored, never fetched: the page must work with no
# network, and its policy may keep naming no origin but this one.
ASSET_VENDOR = Path(__file__).resolve().parents[1] / "assets" / "vendor"
VENDOR_FILES = {
    "markdown-it.min.js": "text/javascript; charset=utf-8",
    "markdown-it-task-lists.min.js": "text/javascript; charset=utf-8",
    "markdown-it-footnote.min.js": "text/javascript; charset=utf-8",
    "highlight.min.js": "text/javascript; charset=utf-8",
    "mermaid.min.js": "text/javascript; charset=utf-8",
}

# Inline style and script are all the page needs, and same-origin requests are
# the only network it may make: the review payload, the decision, the beacon that
# reports a closed window, and the bundled code font. Everything else -- images,
# other origins, frames -- stays denied.
CONTENT_SECURITY_POLICY = (
    # One directive per name: a repeated script-src silently replaces the first,
    # which would block the page's own inline script.
    "default-src 'none'; style-src 'unsafe-inline'; script-src 'self' 'unsafe-inline'; "
    "connect-src 'self'; font-src 'self'"
)
SECURITY_HEADERS = (
    ("Content-Security-Policy", CONTENT_SECURITY_POLICY),
    ("X-Content-Type-Options", "nosniff"),
    ("Referrer-Policy", "no-referrer"),
    ("Cache-Control", "no-store"),
)

MAX_BODY_BYTES = 8 << 20
MAX_RENDERED_LINES_PER_FILE = 1500

HUNK_HEADER = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(.*)$")
URL_PATTERN = re.compile(r"http://127\.0\.0\.1:\d+/[A-Za-z0-9_-]+/")

GitRunner = Callable[..., bytes]


class DecisionError(Exception):
    """The review page asked for something the protocol cannot carry."""


# --------------------------------------------------------------------------
# request
# --------------------------------------------------------------------------


def parse_request(text: str) -> task_review.ReviewRequest:
    """Validates a request the way RDG's ``ReviewRequest::validate`` does.

    The client already owns path normalisation, duplicate-root detection and the
    protocol version; this adds RDG's two remaining rules, that a review id and
    a criterion must be more than whitespace.
    """
    request = task_review.parse_request_json(text)
    if not request.review_id.strip():
        raise task_review.ResultError("review_id must not be empty")
    if not request.criterion.strip():
        raise task_review.ResultError("criterion must not be empty")
    return request


def read_request_text(argument: "str | None") -> str:
    """The request is the ``--review`` value, or stdin when the value is absent."""
    if argument:
        return argument
    return sys.stdin.read()


# --------------------------------------------------------------------------
# diff
# --------------------------------------------------------------------------


@dataclass(frozen=True)
class DiffLine:
    """One rendered patch line: ``context``, ``add``, ``remove`` or ``meta``.

    ``old_line`` and ``new_line`` are 1-based line numbers on that side of the
    diff, or ``None`` when the line does not exist there. Only the first three
    types anchor a comment; a header line has no position to point at.
    """

    type: str
    old_line: "int | None"
    new_line: "int | None"
    text: str

    @property
    def selectable(self) -> bool:
        return self.type in ("context", "add", "remove")

    def payload(self) -> dict[str, Any]:
        return {
            "type": self.type,
            "old_line": self.old_line,
            "new_line": self.new_line,
            "text": self.text,
        }


@dataclass(frozen=True)
class FileDiff:
    repository: str
    path: str
    old_path: "str | None"
    status: str
    binary: bool
    added: int
    removed: int
    lines: tuple[DiffLine, ...]
    text: "str | None" = None
    language: "str | None" = None

    def payload(self) -> dict[str, Any]:
        payload = {
            "repository": self.repository,
            "path": self.path,
            "old_path": self.old_path,
            "status": self.status,
            "binary": self.binary,
            "added": self.added,
            "removed": self.removed,
            "lines": [line.payload() for line in self.lines],
        }
        if self.language:
            payload["language"] = self.language
        if self.text is not None:
            payload["text"] = self.text
        return payload


def parse_patch(patch: bytes) -> "tuple[list[DiffLine], int, int, bool]":
    """Splits one file patch into display lines.

    Returns the lines, how many were added and removed, and whether the patch is
    binary (a binary patch has no lines a human can comment on).
    """
    lines: list[DiffLine] = []
    added = 0
    removed = 0
    binary = False
    old_line: "int | None" = None
    new_line: "int | None" = None
    in_hunk = False

    for raw in task_review.split_patch_lines(patch.decode("utf-8", "replace")):
        header = HUNK_HEADER.match(raw)
        if header is not None:
            in_hunk = True
            # "@@ -0,0 +1,4 @@" has no old side yet; 0 means absent, not line 0.
            old_line = int(header.group(1)) or None
            new_line = int(header.group(3)) or None
            lines.append(DiffLine("meta", None, None, raw))
            continue
        if not in_hunk:
            if raw.startswith("Binary files ") or raw == "GIT binary patch":
                binary = True
            if raw:
                lines.append(DiffLine("meta", None, None, raw))
            continue
        if raw.startswith("\\"):
            lines.append(DiffLine("meta", None, None, raw))
        elif raw.startswith("+"):
            added += 1
            lines.append(DiffLine("add", None, new_line, raw[1:]))
            new_line = (new_line or 0) + 1
        elif raw.startswith("-"):
            removed += 1
            lines.append(DiffLine("remove", old_line, None, raw[1:]))
            old_line = (old_line or 0) + 1
        elif raw == "" or raw.startswith(" "):
            # git writes a bare empty line for a blank source line.
            lines.append(DiffLine("context", old_line, new_line, raw[1:] if raw else ""))
            old_line = (old_line or 0) + 1
            new_line = (new_line or 0) + 1
        else:
            lines.append(DiffLine("meta", None, None, raw))
    return lines, added, removed, binary


def status_for(entry: task_review.StatusEntry) -> str:
    """The change kind shown on a file, taken from the client's status entry."""
    if entry.kind == "untracked":
        return "added"
    if entry.original_path:
        return "renamed"
    if entry.mode_worktree == "000000":
        return "deleted"
    return "modified"


MAX_HIGHLIGHT_BYTES = 400_000

LANGUAGE_ALIASES = {
    "py": "python", "pyi": "python", "js": "javascript", "mjs": "javascript",
    "cjs": "javascript", "jsx": "javascript", "ts": "typescript", "tsx": "typescript",
    "rs": "rust", "go": "go", "yml": "yaml", "sh": "bash", "bash": "bash",
    "zsh": "bash", "md": "markdown", "markdown": "markdown", "html": "xml",
    "htm": "xml", "vue": "xml", "rb": "ruby", "pl": "perl", "cs": "csharp",
    "kt": "kotlin", "swift": "swift", "scala": "scala", "lua": "lua",
}


def language_for(path: str) -> "str | None":
    """The language hint for a path, or None when we should not guess."""
    name = path.split("/")[-1].lower()
    if name in ("makefile", "dockerfile", "gemfile", "rakefile"):
        return {"makefile": "makefile", "dockerfile": "dockerfile", "gemfile": "ruby", "rakefile": "ruby"}[name]
    dot = name.rfind(".")
    if dot <= 0:
        return None
    return LANGUAGE_ALIASES.get(name[dot + 1 :])


def read_for_highlighting(root: str, path: str, binary: bool) -> "str | None":
    """The new-side file text, so a reviewer can tokenize whole files.

    A removed line has no pre-image on disk, so the page shows those as plain
    text; added and context lines get the real thing. A file too large to be
    worth sending is simply not highlighted.
    """
    if binary:
        return None
    absolute = os.path.join(root, *path.split("/"))
    try:
        if os.path.getsize(absolute) > MAX_HIGHLIGHT_BYTES:
            return None
        with open(absolute, "rb") as handle:
            raw = handle.read()
    except OSError:
        return None
    if b"\x00" in raw:
        return None
    return raw.decode("utf-8", "replace")


def collect_file_diffs(
    repository: task_review.Repository,
    git: GitRunner = task_review.run_git,
) -> "list[FileDiff]":
    """Reads one repository's diff through the client's own snapshot code."""
    root = repository.root
    baseline = task_review.resolve_baseline(root, git)
    entries = task_review.parse_porcelain_v2(
        git(root, ["status", "--porcelain=v2", "-z", "--untracked-files=all"])
    )
    selected = [entry for entry in entries if task_review.matches_scope(entry.path, repository.paths)]
    diffs: list[FileDiff] = []
    for entry in sorted(selected, key=lambda item: item.path):
        patch = task_review.entry_patch(root, baseline, entry, git)
        lines, added, removed, binary = parse_patch(patch)
        diffs.append(
            FileDiff(
                repository=root,
                path=entry.path,
                old_path=entry.original_path,
                status=status_for(entry),
                binary=binary,
                added=added,
                removed=removed,
                lines=tuple(lines),
                text=read_for_highlighting(root, entry.path, binary),
                language=language_for(entry.path),
            )
        )
    return diffs


def build_payload(
    request: task_review.ReviewRequest,
    git: GitRunner = task_review.run_git,
) -> dict[str, Any]:
    """The review document the page renders: criterion, repositories, diff."""
    if request.guide is not None:
        task_review.validate_guide_snapshot(request.guide, task_review.collect_snapshot(request.repositories, git))
    repositories = []
    total_files = total_added = total_removed = 0
    for repository in request.repositories:
        diffs = collect_file_diffs(repository, git)
        added = sum(item.added for item in diffs)
        removed = sum(item.removed for item in diffs)
        total_files += len(diffs)
        total_added += added
        total_removed += removed
        repositories.append(
            {
                "root": repository.root,
                "paths": list(repository.paths) if repository.paths else None,
                "added": added,
                "removed": removed,
                "files": [item.payload() for item in diffs],
            }
        )
    return {
        "version": PROTOCOL_VERSION,
        "review_id": request.review_id,
        "criterion": request.criterion,
        "repositories": repositories,
        "totals": {"files": total_files, "added": total_added, "removed": total_removed},
        "guide": build_guide(request.guide, repositories),
    }


def build_guide(guide: "dict[str, Any] | None", repositories: list[dict[str, Any]]) -> dict[str, Any]:
    files = [file for repo in repositories for file in repo["files"]]
    indices = {(file["repository"], file["path"]): index for index, file in enumerate(files)}
    chapters = []
    covered = set()
    if guide is not None:
        for chapter in guide["chapters"]:
            members = []
            for reference in chapter["files"]:
                identity = (reference["repository"], reference["path"])
                if identity not in indices:
                    raise task_review.ResultError(f"guide references a file outside the captured diff: {reference['path']}")
                index = indices[identity]
                if index in covered:
                    raise task_review.ResultError(f"duplicate guide file reference: {reference['path']}")
                covered.add(index)
                members.append(index)
            chapters.append({"title": chapter["title"], "summary": chapter["summary"], "file_indices": members})
        remaining = [index for index in range(len(files)) if index not in covered]
        if remaining:
            chapters.append({"title": "Other changes", "summary": "These files were not assigned to an authored chapter. Review them too.", "file_indices": remaining})
    else:
        groups: dict[tuple[str, str], list[int]] = {}
        for index, file in enumerate(files):
            directory = file["path"].rsplit("/", 1)[0] if "/" in file["path"] else "Root files"
            groups.setdefault((file["repository"], directory), []).append(index)
        for (root, directory), members in groups.items():
            title = f"{root} · {directory}" if len(repositories) > 1 else directory
            chapters.append({"title": title, "summary": "Grouped by directory, not by behavior.", "file_indices": members})
    result = {"authored": guide is not None, "chapters": chapters}
    if guide is not None:
        for key in ("summary", "before", "after", "flow"):
            if key in guide:
                result[key] = guide[key]
    else:
        result["summary"] = "No agent-authored guide was supplied. These directory groups show every scoped file without inferring what the change does."
    return result


def parse_document_request(text: str) -> task_review.DocumentRequest:
    """A document request names one markdown file and the criterion it is judged
    against. Everything else about it matches the diff request, so the approval
    the client records means the same thing in both modes."""
    try:
        payload = json.loads(text)
    except ValueError as error:
        raise task_review.DocumentError(f"parsing document request JSON: {error}") from None
    if not isinstance(payload, dict):
        raise task_review.DocumentError("a document request must be a JSON object")
    version = payload.get("version")
    if version != PROTOCOL_VERSION:
        raise task_review.DocumentError(f"unsupported review protocol version: {version}")
    review_id = payload.get("review_id")
    if not isinstance(review_id, str) or not review_id.strip():
        raise task_review.DocumentError("review_id must not be empty")
    criterion = payload.get("criterion")
    if not isinstance(criterion, str) or not criterion.strip():
        raise task_review.DocumentError("criterion must not be empty")
    raw_path = payload.get("path")
    if not isinstance(raw_path, str) or not raw_path.strip():
        raise task_review.DocumentError("a document review needs a document path")
    path = os.path.abspath(os.path.expanduser(raw_path))
    if not os.path.isfile(path):
        raise task_review.DocumentError(f"the document does not exist: {raw_path}")
    return task_review.DocumentRequest(
        version=version, review_id=review_id, criterion=criterion, path=path
    )


def build_document_payload(request: task_review.DocumentRequest) -> dict[str, Any]:
    """The document the page renders: criterion, path, and one row per line."""
    try:
        with open(request.path, "r", encoding="utf-8", errors="replace") as handle:
            text = handle.read()
    except OSError as error:
        raise task_review.DocumentError(f"the document could not be read: {error}") from None
    # The page renders blocks; the ranges they carry are what a comment names,
    # so the two must come from the same grouping pass.
    grouped = mark_fence_languages(markdown_doc.blocks(text))
    return {
        "version": PROTOCOL_VERSION,
        "review_id": request.review_id,
        "criterion": request.criterion,
        "path": request.path,
        "name": os.path.basename(request.path),
        "lines": len(text.splitlines()),
        "rows": markdown_doc.rows(text),
        "blocks": grouped,
    }


def mark_fence_languages(blocks: "list[dict[str, Any]]") -> "list[dict[str, Any]]":
    """Record each fenced block's language, from its opening fence.

    The page does the tokenizing, with a real grammar engine; all the server
    owes it is which language a fence claims, so the badge and the engine agree.
    """
    for block in blocks:
        if block.get("kind") != "code" or "source" not in block:
            continue
        opening = block["source"].split("\n")[0].strip()
        info = opening.lstrip("`~").strip()
        block["language"] = info.split()[0] if info.split() else ""
    return blocks


def document_line_index(payload: dict[str, Any]) -> set[int]:
    """Every line number a comment may point at in this document.

    A block is rendered once but may span several source lines, so the whole
    range of every block is commentable: a comment on the first, a middle, or
    the last line of a paragraph is the same place in the file. A line no block
    covers stays invalid, so a stale page still cannot invent a position.
    """
    return {
        line
        for block in payload.get("blocks") or []
        for line in range(block["start"], block["end"] + 1)
    }



def line_index(payload: dict[str, Any]) -> "dict[tuple[str, str, str], set[int]]":
    """Every line number a comment may point at, keyed by target.

    A stale page cannot make up a position: a target outside this index is
    rejected rather than passed on to the client.
    """
    index: "dict[tuple[str, str, str], set[int]]" = {}
    for repository in payload["repositories"]:
        for item in repository["files"]:
            for line in item["lines"]:
                if line["new_line"] is not None:
                    index.setdefault((item["repository"], item["path"], "new"), set()).add(line["new_line"])
                if line["old_line"] is not None:
                    index.setdefault((item["repository"], item["path"], "old"), set()).add(line["old_line"])
    return index


# --------------------------------------------------------------------------
# decision
# --------------------------------------------------------------------------


def build_result(
    request: task_review.ReviewRequest,
    decision: str,
    annotations: "list[dict[str, Any]]",
) -> dict[str, Any]:
    return {
        "version": PROTOCOL_VERSION,
        "review_id": request.review_id,
        "decision": decision,
        "annotations": list(annotations),
    }


def cancelled_result(request: task_review.ReviewRequest) -> dict[str, Any]:
    return build_result(request, "cancelled", [])


SEMANTIC_MESSAGES = {
    "the reviewer requested changes without feedback": "a request for changes needs at least one comment",
    "the reviewer returned annotations for a cancelled review": "a cancelled review must not carry annotations",
}


def parse_decision(
    payload: Any,
    request: task_review.ReviewRequest,
    index: "dict[tuple[str, str, str], set[int]]",
) -> dict[str, Any]:
    """Validates a decision from the page and returns the protocol result.

    The states the client treats as errors are refused here instead, so a bug in
    the page can only ever cost a review, never produce a result the client has
    to distrust.
    """
    if not isinstance(payload, dict):
        raise DecisionError("the decision must be a JSON object")
    decision = payload.get("decision")
    if decision not in task_review.DECISIONS:
        raise DecisionError(f"unknown decision: {decision!r}")
    raw_annotations = payload.get("annotations")
    if not isinstance(raw_annotations, list):
        raise DecisionError("the annotations must be a list")

    annotations: list[dict[str, Any]] = []
    for item in raw_annotations:
        if not task_review.is_annotation(item):
            raise DecisionError("malformed annotation")
        text = item["text"].strip()
        if not text:
            raise DecisionError("an annotation must carry text")
        target = item["target"]
        if target.get("scope") != "line":
            # A review-level comment is exactly a scope and nothing else.
            annotations.append({"text": text, "target": {"scope": "review"}})
            continue
        key = (target["repository"], target["path"], target["side"])
        available = index.get(key)
        if available is None:
            raise DecisionError(f"the comment target {key[1]!r} is not part of this review")
        for field in ("start_line", "end_line"):
            if target[field] not in available:
                raise DecisionError(f"{field} {target[field]} is not a line of {key[1]!r} in this review")
        annotations.append(
            {
                "text": text,
                "target": {
                    "scope": "line",
                    "repository": target["repository"],
                    "path": target["path"],
                    "side": target["side"],
                    "start_line": target["start_line"],
                    "end_line": target["end_line"],
                },
            }
        )

    result = build_result(request, decision, annotations)
    # The client's own rules: no changes without feedback, no feedback on a
    # cancelled review. Its errors are this module's errors here.
    try:
        task_review.validate_decision_semantics(result)
    except task_review.TaskReviewError as error:
        # The rule is the client's; the wording is this reviewer's, because a
        # human reading it is looking at a page, not at RDG.
        raise DecisionError(SEMANTIC_MESSAGES.get(str(error), str(error))) from None
    return result


def parse_document_decision(
    payload: Any,
    request: task_review.DocumentRequest,
    lines: set[int],
) -> dict[str, Any]:
    """The document counterpart of :func:`parse_decision`.

    A comment may only name a line the document actually has, which is what
    makes "line 42" in a review mean the same thing in the page and in the
    approval the client records.
    """
    if not isinstance(payload, dict):
        raise DecisionError("the decision must be a JSON object")
    decision = payload.get("decision")
    if decision not in task_review.DECISIONS:
        raise DecisionError(f"unknown decision: {decision!r}")
    raw_annotations = payload.get("annotations")
    if not isinstance(raw_annotations, list):
        raise DecisionError("the annotations must be a list")

    annotations: list[dict[str, Any]] = []
    for item in raw_annotations:
        if not task_review.is_document_annotation(item, request.path):
            raise DecisionError("malformed annotation")
        text = item["text"].strip()
        if not text:
            raise DecisionError("an annotation must carry text")
        target = item["target"]
        if target.get("scope") != "line":
            annotations.append({"text": text, "target": {"scope": "review"}})
            continue
        for field in ("start_line", "end_line"):
            if target[field] not in lines:
                raise DecisionError(f"{field} {target[field]} is not a line of {request.path}")
        annotations.append(
            {
                "text": text,
                "target": {
                    "scope": "line",
                    "path": request.path,
                    "start_line": target["start_line"],
                    "end_line": target["end_line"],
                },
            }
        )

    result = {
        "version": PROTOCOL_VERSION,
        "review_id": request.review_id,
        "decision": decision,
        "annotations": annotations,
    }
    try:
        task_review.validate_decision_semantics(result)
    except task_review.TaskReviewError as error:
        raise DecisionError(SEMANTIC_MESSAGES.get(str(error), str(error))) from None
    return result


# --------------------------------------------------------------------------
# the page
# --------------------------------------------------------------------------

PAGE_TEMPLATE = (Path(__file__).resolve().parents[1] / "assets" / "review-page.html").read_text(encoding="utf-8")


def render_page(token: str, mode: str = "diff", page: int = 0) -> bytes:
    """The one self-contained page, with its own token and mode baked into the
    boot JSON. A document review loads the same page and renders a document."""
    boot = json.dumps(
        {
            "url": f"/{token}/",
            "page": page,
            "maxLines": MAX_RENDERED_LINES_PER_FILE,
            "mode": mode,
            "reviewUrl": "api/review" if mode == "diff" else "api/doc",
            "decisionUrl": "api/decision" if mode == "diff" else "api/doc-decision",
        },
        separators=(",", ":"),
    )
    logic = (Path(__file__).resolve().parents[1] / "assets" / "review-diff.js").read_text(encoding="utf-8")
    return PAGE_TEMPLATE.replace("__HTML_REVIEW_BOOT__", boot).replace("__HTML_DIFF_VIEW__", logic).encode("utf-8")


# --------------------------------------------------------------------------
# server
# --------------------------------------------------------------------------


class ReviewHandler(BaseHTTPRequestHandler):
    """Serves the page, the review payload, and the decision, behind the token."""

    protocol_version = "HTTP/1.1"
    server_version = "html-review"
    sys_version = ""

    @property
    def session(self) -> "ReviewSession":
        return self.server.session  # type: ignore[attr-defined]

    def log_message(self, *args: Any) -> None:
        """Silence the default request log: it would print the token and paths."""

    def route(self) -> "str | None":
        # A token that does not match is a probe, not a user: only count the
        # requests that prove somebody actually opened the page.
        path = urllib.parse.urlsplit(self.path).path
        parts = [part for part in path.split("/") if part]
        if parts and secrets.compare_digest(parts[0], self.session.token):
            self.session._requests += 1
        if not parts or not secrets.compare_digest(parts[0], self.session.token):
            return None
        rest = parts[1:]
        if not rest:
            return "page"
        if rest == ["api", "review"]:
            return "review"
        if rest == ["api", "decision"]:
            return "decision"
        if rest == ["api", "doc"]:
            return "doc"
        if rest == ["api", "doc-decision"]:
            return "doc-decision"
        if rest == ["api", "closing"]:
            return "closing"
        # The code font is bundled with the reviewer and served from this same
        # origin, so the page needs no external resource and works offline.
        if len(rest) == 2 and rest[0] == "font" and rest[1] in FONT_FILES:
            return "font"
        if len(rest) == 2 and rest[0] == "vendor" and rest[1] in VENDOR_FILES:
            return "vendor"
        return None

    def respond(self, status: int, body: bytes = b"", content_type: str = "text/plain; charset=utf-8") -> None:
        self.send_response(status)
        for name, value in SECURITY_HEADERS:
            self.send_header(name, value)
        if status != 204:  # a 204 carries no body, so it carries no length either
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)

    def fail(self, status: int, message: str) -> None:
        body = json.dumps({"error": message}).encode("utf-8")
        self.respond(status, body, "application/json; charset=utf-8")

    def read_body(self) -> "bytes | None":
        length = self.headers.get("Content-Length")
        if length is None:
            self.fail(411, "a decision needs a Content-Length")
            return None
        try:
            size = int(length)
        except ValueError:
            self.fail(400, "an unusable Content-Length")
            return None
        if size < 0 or size > MAX_BODY_BYTES:
            self.fail(413, "the decision is too large")
            return None
        return self.rfile.read(size) if size else b""

    def do_GET(self) -> None:
        route = self.route()
        if route == "page":
            page = self.session.next_page()
            self.respond(200, render_page(self.session.token, self.session.mode, page), "text/html; charset=utf-8")
            return
        if route == "review":
            self.respond(
                200,
                json.dumps(self.session.payload, separators=(",", ":")).encode("utf-8"),
                "application/json; charset=utf-8",
            )
            return
        if route == "doc":
            self.respond(
                200,
                json.dumps(self.session.payload, separators=(",", ":")).encode("utf-8"),
                "application/json; charset=utf-8",
            )
            return
        if route == "font":
            name = urllib.parse.urlsplit(self.path).path.split("/")[-1]
            self.respond(200, FONT_FILES[name].read_bytes(), "font/woff2")
            return
        if route == "vendor":
            name = urllib.parse.urlsplit(self.path).path.split("/")[-1]
            self.respond(200, (ASSET_VENDOR / name).read_bytes(), VENDOR_FILES[name])
            return
        if route is None:
            self.fail(404, "not found")
            return
        self.fail(405, f"{route} needs POST")

    def do_POST(self) -> None:
        route = self.route()
        if route in ("page", "review", "doc", "font", "vendor"):
            self.fail(405, f"{route} needs POST")
            return
        if route == "closing":
            page = urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query).get("page", [""])[0]
            if not page.isascii() or not page.isdigit() or len(page) > 9 or int(page) < 1:
                self.fail(400, "a closing report needs the number of the page that is closing")
                return
            self.session.begin_close(int(page))
            self.respond(204)
            return
        if route not in ("decision", "doc-decision"):
            self.fail(404, "not found")
            return
        body = self.read_body()
        if body is None:
            return
        try:
            decoded = json.loads(body.decode("utf-8"))
        except (ValueError, UnicodeDecodeError):
            self.fail(400, "the decision is not valid JSON")
            return
        try:
            if route == "doc-decision":
                result = parse_document_decision(decoded, self.session.request, self.session.index)
            else:
                result = parse_decision(decoded, self.session.request, self.session.index)
        except DecisionError as error:
            self.fail(400, str(error))
            return
        self.respond(204)
        self.session.submit(result)



def publish_url(path: "str | None", url: str, stream: "Any" = None) -> None:
    """Publishes the page URL so a caller that cannot see stderr can still open it."""
    if not path:
        return
    out = sys.stderr if stream is None else stream
    try:
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
            handle.write(url + "\n")
    except OSError as error:
        # A review must not fail because a convenience file could not be written.
        out.write(f"{PRODUCER}: unable to publish the review URL to {path} ({error})\n")
        out.flush()


def withdraw_url(path: "str | None") -> None:
    """Removes the published URL: the token dies with the server."""
    if not path:
        return
    try:
        os.unlink(path)
    except OSError:
        pass


class ReviewSession:
    """One review: a bound server, the token that gates it, and its result."""

    def __init__(
        self,
        request: Any,
        payload: dict[str, Any],
        *,
        token: "str | None" = None,
        open_browser: bool = True,
        host: str = HOST,
        lifetime_seconds: float = LIFETIME_SECONDS,
        idle_seconds: float = IDLE_SECONDS,
        close_grace_seconds: float = CLOSE_GRACE_SECONDS,
        mode: str = "diff",
    ) -> None:
        self.request = request
        self.payload = payload
        self.mode = mode
        # A diff indexes (repository, path, side); a document indexes plain line
        # numbers, because a specification has no repository and no old side.
        self.index = document_line_index(payload) if mode == "document" else line_index(payload)
        self.token = token if token is not None else secrets.token_urlsafe(24)
        self.open_browser = open_browser
        self._decided = threading.Event()
        self._result: "dict[str, Any] | None" = None
        self._lock = threading.Lock()
        self._closed = False
        self._serving = False
        self._httpd = ThreadingHTTPServer((host, 0), ReviewHandler)
        self._httpd.daemon_threads = True
        self._httpd.session = self  # type: ignore[attr-defined]
        self._requests = 0
        self._reason: "str | None" = None
        self.lifetime_seconds = lifetime_seconds
        self.idle_seconds = idle_seconds
        self.close_grace_seconds = close_grace_seconds
        self._closing: "threading.Timer | None" = None
        self._page = 0

    @property
    def host(self) -> str:
        return str(self._httpd.server_address[0])

    @property
    def port(self) -> int:
        return int(self._httpd.server_address[1])

    @property
    def base_url(self) -> str:
        return f"http://{self.host}:{self.port}"

    @property
    def url(self) -> str:
        return f"{self.base_url}/{self.token}/"

    def start(self) -> "ReviewSession":
        """Serves in the background so a caller can talk to the server itself."""
        if self._serving:
            return self
        self._serving = True
        thread = threading.Thread(target=self._httpd.serve_forever, name="html-review", daemon=True)
        thread.start()
        return self

    def submit(self, result: dict[str, Any]) -> None:
        with self._lock:
            if self._result is not None:
                return
            self._result = result
        self._decided.set()
        # shutdown() blocks until serve_forever() returns, so it must not run in
        # the handler thread that serve_forever() is currently waiting on.
        threading.Thread(target=self._httpd.shutdown, name="html-review-stop", daemon=True).start()

    def stop(self) -> None:
        """Ends the review with no decision: withdrawn, never approved."""
        self.submit(cancelled_result(self.request))

    def next_page(self) -> int:
        """Numbers a page being served; a newer page cancels a pending close."""
        with self._lock:
            self._page += 1
            page = self._page
        self.cancel_close()
        return page

    def begin_close(self, page: int) -> None:
        """A page is closing: withdraw the review unless a newer page exists or
        is requested within the grace period."""
        with self._lock:
            if self._result is not None or self._closing is not None or page != self._page:
                return
            self._closing = threading.Timer(self.close_grace_seconds, self.stop)
            self._closing.daemon = True
            self._closing.start()

    def cancel_close(self) -> None:
        with self._lock:
            closing, self._closing = self._closing, None
        if closing is not None:
            closing.cancel()

    def result(self, timeout: "float | None" = None) -> "dict[str, Any] | None":
        if not self._decided.wait(timeout):
            return None
        with self._lock:
            return self._result

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        self.cancel_close()
        self._httpd.shutdown()
        self._httpd.server_close()

    def watch_lifetime(self, *, poll: float = 0.5) -> threading.Thread:
        """Ends the review when nobody is left to receive it.

        A client killed with SIGKILL, a crash, or a closed laptop cannot clean
        this process up, and without a bound it would hold its port and wait
        for a decision that can no longer be recorded. Three conditions end it:
        the parent is gone, the wall-clock budget is spent, or the page was
        never opened.
        """
        original_parent = os.getppid()
        started = time.monotonic()

        def gone() -> bool:
            return os.getppid() != original_parent

        def poll_loop() -> None:
            while not self._decided.wait(poll):
                if gone():
                    self.expire("the client process ended")
                    return
                if time.monotonic() - started >= self.lifetime_seconds:
                    self.expire("the review lifetime expired")
                    return
                if self.idle_seconds and time.monotonic() - started >= self.idle_seconds and self._requests == 0:
                    self.expire("the review page was never opened")
                    return

        thread = threading.Thread(target=poll_loop, name="html-review-watchdog", daemon=True)
        thread.start()
        return thread

    def expire(self, reason: str) -> None:
        """Withdraws the review. An expired review is never an approval."""
        if self._reason is None:
            self._reason = reason
        self.stop()

    def serve_and_wait(
        self,
        stream: "Any" = None,
        url_file: "str | None" = None,
    ) -> dict[str, Any]:
        """Opens the page, serves until a decision arrives, and returns it."""
        out = sys.stderr if stream is None else stream
        self.start()
        out.write(f"{PRODUCER}: review page {self.url}\n")
        out.flush()
        publish_url(url_file, self.url, out)
        # Bounded from the moment the page is published, so a client that dies
        # uncatchably cannot leave this process holding its port forever.
        self.watch_lifetime()
        try:
            if self.open_browser:
                try:
                    webbrowser.open(self.url, new=1)
                except Exception as error:  # a headless box must not fail the gate
                    out.write(f"{PRODUCER}: unable to open a browser ({error}); open the URL above\n")
                    out.flush()
            self._decided.wait()
            result = self.result()
        finally:
            withdraw_url(url_file)
        self.close()
        # Nothing decided is a withdrawn review, never an approval.
        return result if result is not None else cancelled_result(self.request)


def start_review_server(
    request: task_review.ReviewRequest,
    *,
    git: GitRunner = task_review.run_git,
    open_browser: bool = True,
    token: "str | None" = None,
    host: str = HOST,
) -> ReviewSession:
    """Builds the payload, binds an ephemeral port, and serves in the background.

    Tests drive the returned session directly instead of opening a browser.
    """
    payload = build_payload(request, git)
    return ReviewSession(request, payload, token=token, open_browser=open_browser, host=host).start()


# --------------------------------------------------------------------------
# command line
# --------------------------------------------------------------------------


def browser_enabled(environ: "dict[str, str]") -> bool:
    value = environ.get(NO_BROWSER_ENV, "").strip().lower()
    return value in ("", "0", "false", "no", "off")


def positive_seconds(environ: "dict[str, str]", name: str, default: float) -> float:
    """Reads a lifetime bound, treating junk as absent rather than guessing."""
    raw = environ.get(name, "").strip()
    if not raw:
        return default
    try:
        value = float(raw)
    except ValueError:
        return default
    return value if value > 0 else default


def emit(result: dict[str, Any], stream: "Any" = None) -> None:
    out = sys.stdout if stream is None else stream
    out.write(json.dumps(result, separators=(",", ":")) + "\n")
    out.flush()


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog=PRODUCER,
        description="Review one task-review request in a local browser page and print one result line.",
    )
    parser.add_argument(
        "--review",
        nargs="?",
        const="",
        default=None,
        metavar="JSON",
        help="a complete ReviewRequest JSON document; with no value the request is read from stdin",
    )
    parser.add_argument(
        "--review-doc",
        nargs="?",
        const="",
        default=None,
        metavar="JSON",
        help="a complete DocumentRequest JSON document for a specification or plan",
    )
    return parser


def main(
    argv: "Sequence[str] | None" = None,
    environ: "dict[str, str] | None" = None,
    *,
    git: GitRunner = task_review.run_git,
    open_browser: "bool | None" = None,
    stream: "Any" = None,
) -> int:
    environment = dict(os.environ) if environ is None else environ
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.review is None and args.review_doc is None:
        parser.print_usage(sys.stderr)
        sys.stderr.write(
            f"{PRODUCER}: a review request is required: --review JSON or --review-doc JSON\n"
        )
        return EXIT_ERROR
    try:
        if args.review_doc is not None:
            request = parse_document_request(read_request_text(args.review_doc))
            session = ReviewSession(
                request,
                build_document_payload(request),
                open_browser=browser_enabled(environment) if open_browser is None else open_browser,
                lifetime_seconds=positive_seconds(environment, LIFETIME_ENV, LIFETIME_SECONDS),
                idle_seconds=positive_seconds(environment, IDLE_ENV, IDLE_SECONDS),
                mode="document",
            )
        else:
            request = parse_request(read_request_text(args.review))
            session = ReviewSession(
                request,
                build_payload(request, git),
                open_browser=browser_enabled(environment) if open_browser is None else open_browser,
                lifetime_seconds=positive_seconds(environment, LIFETIME_ENV, LIFETIME_SECONDS),
                idle_seconds=positive_seconds(environment, IDLE_ENV, IDLE_SECONDS),
            )
    except (task_review.TaskReviewError, OSError) as error:
        sys.stderr.write(f"{PRODUCER}: {error}\n")
        return EXIT_ERROR
    emit(session.serve_and_wait(stream, url_file=environment.get(URL_FILE_ENV)))
    return EXIT_OK


if __name__ == "__main__":
    sys.exit(main())
