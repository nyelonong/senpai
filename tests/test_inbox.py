"""The on-demand inbox: reviews and questions from many clients, one service."""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))
import commands  # noqa: E402

GIT_ENV = {
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_SYSTEM": os.devnull,
    "GIT_TERMINAL_PROMPT": "0",
    "GIT_AUTHOR_NAME": "Inbox",
    "GIT_AUTHOR_EMAIL": "inbox@example.com",
    "GIT_COMMITTER_NAME": "Inbox",
    "GIT_COMMITTER_EMAIL": "inbox@example.com",
}


def get_json(url: str) -> object:
    with urllib.request.urlopen(url, timeout=5) as response:
        return json.loads(response.read())


def post(url: str, payload: object) -> int:
    request = urllib.request.Request(url, data=json.dumps(payload).encode(), method="POST",
                                     headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            return response.status
    except urllib.error.HTTPError as error:
        return error.code


def wait_for(condition, timeout: float = 10.0, message: str = "condition"):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = condition()
        if value:
            return value
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for {message}")


class InboxTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = Path(os.path.realpath(tempfile.mkdtemp(prefix="sp")))
        self.addCleanup(shutil.rmtree, self.temp, ignore_errors=True)
        self.home = self.temp / "h"
        self.env = {
            **os.environ,
            **GIT_ENV,
            commands.INBOX_ENV: "1",
            commands.HOME_ENV: str(self.home),
            commands.NO_BROWSER_ENV: "1",
            commands.TIMEOUT_ENV: "60",
        }
        self.processes: list[subprocess.Popen] = []
        self.addCleanup(self.stop_service)

    def stop_service(self, binary: str = commands.BINARY) -> None:
        for process in self.processes:
            if process.poll() is None:
                process.kill()
                process.communicate()
        subprocess.run([binary, "serve", "--stop"], env=self.env, capture_output=True, timeout=20)

    def repo(self, name: str) -> Path:
        root = self.temp / name
        root.mkdir()
        for args in (["init", "-q"], ["add", "."]):
            if args[0] == "add":
                (root / "notes.txt").write_text("one\n")
            subprocess.run(["git", *args], cwd=root, env=self.env, check=True)
        subprocess.run(["git", "commit", "-qm", "init"], cwd=root, env=self.env, check=True)
        (root / "notes.txt").write_text("one\ntwo\n")
        return root

    def start(self, args: list[str], url_file: Path | None = None, binary: str = commands.BINARY) -> subprocess.Popen:
        env = dict(self.env)
        if url_file is not None:
            env[commands.URL_FILE_ENV] = str(url_file)
        process = subprocess.Popen([binary, *args], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.processes.append(process)
        return process

    def review(self, root: Path, criterion: str = "ship it") -> tuple[subprocess.Popen, str]:
        url_file = self.temp / f"{root.name}.url"
        process = self.start(["review", "--criterion", criterion, "--repo", str(root)], url_file)

        def published() -> str | None:
            if process.poll() is not None:
                self.fail(process.communicate()[1].decode(errors="replace"))
            text = url_file.read_text().strip() if url_file.exists() else ""
            return text or None

        return process, wait_for(published, message="the review URL")

    def inbox(self, binary: str = commands.BINARY) -> dict:
        result = subprocess.run([binary, "inbox"], env=self.env, capture_output=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        return json.loads(result.stdout)

    def items(self, inbox_url: str) -> list[dict]:
        return get_json(inbox_url + "api/items")["items"]

    def finish(self, process: subprocess.Popen) -> tuple[int, dict | None, str]:
        out, err = process.communicate(timeout=30)
        return process.returncode, json.loads(out) if out.strip() else None, err.decode(errors="replace")

    def verify(self, root: Path) -> int:
        return subprocess.run([commands.BINARY, "verify", "--repo", str(root)], env=self.env,
                              capture_output=True, timeout=30).returncode

    # -- reviews -----------------------------------------------------------

    def test_review_is_decided_through_the_inbox(self) -> None:
        root = self.repo("one")
        process, url = self.review(root)
        inbox = self.inbox()["inbox"]
        [item] = self.items(inbox)
        self.assertEqual(item["kind"], "diff")
        self.assertEqual(item["title"], "ship it")
        self.assertEqual(item["places"], [str(root)])
        self.assertTrue(url.endswith(item["url"]))
        payload = get_json(url + "api/review")
        self.assertEqual(payload["repositories"][0]["files"][0]["path"], "notes.txt")
        self.assertEqual(post(url + "api/decision", {"decision": "approve", "annotations": []}), 204)
        code, result, err = self.finish(process)
        self.assertEqual(code, commands.EXIT_OK, err)
        self.assertEqual(result["decision"], "approve")
        self.assertEqual(self.verify(root), commands.EXIT_OK)
        self.assertEqual(self.items(inbox), [])

    def test_two_reviews_are_decided_independently(self) -> None:
        first, second = self.repo("first"), self.repo("second")
        first_process, first_url = self.review(first, "first change")
        second_process, second_url = self.review(second, "second change")
        inbox = self.inbox()["inbox"]
        self.assertEqual(first_url.split("/")[2], second_url.split("/")[2], "one service hosts both")
        self.assertEqual(sorted(item["title"] for item in self.items(inbox)), ["first change", "second change"])
        feedback = {"decision": "request-changes",
                    "annotations": [{"text": "split this", "target": {"scope": "review"}}]}
        self.assertEqual(post(second_url + "api/decision", feedback), 204)
        code, result, _ = self.finish(second_process)
        self.assertEqual((code, result["decision"]), (commands.EXIT_NOT_APPROVED, "request-changes"))
        self.assertEqual([item["title"] for item in self.items(inbox)], ["first change"])
        self.assertEqual(post(first_url + "api/decision", {"decision": "approve", "annotations": []}), 204)
        code, result, _ = self.finish(first_process)
        self.assertEqual((code, result["decision"]), (commands.EXIT_OK, "approve"))
        self.assertEqual(self.verify(first), commands.EXIT_OK)
        self.assertNotEqual(self.verify(second), commands.EXIT_OK)

    def test_document_review_through_the_inbox(self) -> None:
        document = self.temp / "plan.md"
        document.write_text("# Plan\n\nDo the thing.\n")
        url_file = self.temp / "doc.url"
        process = self.start(["review-doc", "--doc", str(document), "--criterion", "approve the plan"], url_file)
        url = wait_for(lambda: url_file.exists() and url_file.read_text().strip(), message="the document URL")
        inbox = self.inbox()["inbox"]
        [item] = self.items(inbox)
        self.assertEqual((item["kind"], item["places"]), ("document", [str(document)]))
        comment = {"text": "say which thing", "target": {"scope": "line", "path": str(document),
                                                         "start_line": 3, "end_line": 3}}
        self.assertEqual(post(url + "api/doc-decision", {"decision": "request-changes", "annotations": [comment]}), 204)
        code, result, err = self.finish(process)
        self.assertEqual(code, commands.EXIT_NOT_APPROVED, err)
        self.assertEqual(result["annotations"][0]["target"]["start_line"], 3)

    def test_closing_the_page_keeps_the_review_waiting(self) -> None:
        process, url = self.review(self.repo("one"))
        urllib.request.urlopen(url, timeout=5).read()
        self.assertEqual(post(url + "api/closing?page=1", {}), 204)
        time.sleep(6)
        self.assertIsNone(process.poll())
        self.assertEqual(len(self.items(self.inbox()["inbox"])), 1)

    def test_invalid_decision_is_rejected_and_the_review_keeps_waiting(self) -> None:
        process, url = self.review(self.repo("one"))
        self.assertEqual(post(url + "api/decision", {"decision": "request-changes", "annotations": []}), 400)
        self.assertIsNone(process.poll())

    def test_page_lifetime_still_applies(self) -> None:
        self.env[commands.TIMEOUT_ENV] = "0"
        self.env[commands.LIFETIME_ENV] = "1"
        process, url = self.review(self.repo("one"))
        urllib.request.urlopen(url, timeout=5).read()
        code, result, _ = self.finish(process)
        self.assertEqual((code, result["decision"]), (commands.EXIT_NOT_APPROVED, "cancelled"))

    def test_unopened_page_is_cancelled_after_the_idle_limit(self) -> None:
        self.env[commands.TIMEOUT_ENV] = "0"
        self.env[commands.IDLE_ENV] = "1"
        process, _ = self.review(self.repo("one"))
        code, result, _ = self.finish(process)
        self.assertEqual((code, result["decision"]), (commands.EXIT_NOT_APPROVED, "cancelled"))

    def test_decision_after_the_review_ended_is_refused(self) -> None:
        process, url = self.review(self.repo("one"))
        self.assertEqual(post(url + "api/decision", {"decision": "approve", "annotations": []}), 204)
        self.finish(process)
        self.assertIn(post(url + "api/decision", {"decision": "approve", "annotations": []}), (404, 409))

    def test_home_owned_by_another_user_is_refused(self) -> None:
        self.env[commands.HOME_ENV] = "/usr"
        result = subprocess.run([commands.BINARY, "inbox"], env=self.env, capture_output=True, timeout=10)
        self.assertEqual(result.returncode, commands.EXIT_ERROR)
        self.assertIn("belongs to another user", result.stderr.decode())
        stop = subprocess.run([commands.BINARY, "serve", "--stop"], env=self.env, capture_output=True, timeout=10)
        self.assertEqual(stop.returncode, commands.EXIT_ERROR)
        self.assertIn("belongs to another user", stop.stderr.decode())

    def test_interrupted_client_withdraws_its_review(self) -> None:
        process, _ = self.review(self.repo("one"))
        inbox = self.inbox()["inbox"]
        self.assertEqual(len(self.items(inbox)), 1)
        process.send_signal(signal.SIGINT)
        code, result, _ = self.finish(process)
        self.assertEqual((code, result), (commands.EXIT_ERROR, None))
        wait_for(lambda: self.items(inbox) == [], message="the review to be withdrawn")

    def test_killed_client_withdraws_its_review(self) -> None:
        process, _ = self.review(self.repo("one"))
        inbox = self.inbox()["inbox"]
        process.kill()
        process.communicate()
        wait_for(lambda: self.items(inbox) == [], message="the review to be withdrawn")

    def test_socket_path_too_long_fails_closed(self) -> None:
        self.env[commands.HOME_ENV] = str(self.temp / ("d" * 120))
        result = subprocess.run([commands.BINARY, "inbox"], env=self.env, capture_output=True, timeout=10)
        self.assertEqual(result.returncode, commands.EXIT_ERROR)
        self.assertIn("too long", result.stderr.decode())

    def test_timed_out_client_withdraws_its_review(self) -> None:
        self.env[commands.TIMEOUT_ENV] = "2"
        process, _ = self.review(self.repo("one"))
        inbox = self.inbox()["inbox"]
        code, _, err = self.finish(process)
        self.assertEqual(code, commands.EXIT_ERROR)
        self.assertIn("timed out", err)
        wait_for(lambda: self.items(inbox) == [], message="the review to be withdrawn")

    def test_stopped_service_fails_the_review_closed(self) -> None:
        root = self.repo("one")
        process, _ = self.review(root)
        stopped = subprocess.run([commands.BINARY, "serve", "--stop"], env=self.env, capture_output=True, timeout=20)
        self.assertEqual(json.loads(stopped.stdout), {"stopped": True})
        code, result, err = self.finish(process)
        self.assertEqual((code, result), (commands.EXIT_ERROR, None))
        self.assertIn("stopped before a decision", err)
        self.assertNotEqual(self.verify(root), commands.EXIT_OK)

    def test_killed_service_fails_the_review_closed(self) -> None:
        root = self.repo("one")
        process, url = self.review(root)
        port = int(url.split(":")[2].split("/")[0])
        pid = int(subprocess.run(["lsof", "-t", "-sTCP:LISTEN", f"-iTCP:{port}"], capture_output=True,
                                 text=True).stdout.split()[0])
        os.kill(pid, signal.SIGKILL)
        code, result, _ = self.finish(process)
        self.assertEqual((code, result), (commands.EXIT_ERROR, None))
        self.assertNotEqual(self.verify(root), commands.EXIT_OK)
        self.assertEqual(self.inbox()["pending"], 0, "a new service starts over the stale socket")

    # -- the service -------------------------------------------------------

    def test_concurrent_starts_share_one_service(self) -> None:
        processes = [self.start(["inbox"]) for _ in range(5)]
        urls = {json.loads(process.communicate(timeout=30)[0])["inbox"] for process in processes}
        self.assertEqual(len(urls), 1)

    def test_service_files_are_private(self) -> None:
        self.inbox()
        self.assertEqual(self.home.stat().st_mode & 0o777, 0o700)
        self.assertEqual((self.home / "senpai.sock").stat().st_mode & 0o077, 0)

    def test_stale_socket_is_replaced(self) -> None:
        self.home.mkdir(mode=0o700)
        stale = socket.socket(socket.AF_UNIX)
        stale.bind(str(self.home / "senpai.sock"))
        stale.close()
        self.assertEqual(self.inbox()["pending"], 0)

    def test_idle_service_exits(self) -> None:
        self.env[commands.SERVE_IDLE_ENV] = "1"
        self.inbox()
        wait_for(lambda: not (self.home / "senpai.sock").exists(), message="the idle service to exit")

    def test_service_from_another_build_restarts_when_idle(self) -> None:
        old = self.temp / "old-senpai"
        new = self.temp / "new-senpai"
        shutil.copy2(commands.BINARY, old)
        shutil.copy2(commands.BINARY, new)
        os.utime(old, (1_000_000_000, 1_000_000_000))
        first = self.inbox(str(old))["inbox"]
        second = self.inbox(str(new))["inbox"]
        self.assertNotEqual(first, second)

    def test_service_from_another_build_with_pending_items_is_kept(self) -> None:
        old = self.temp / "old-senpai"
        shutil.copy2(commands.BINARY, old)
        os.utime(old, (1_000_000_000, 1_000_000_000))
        asking = self.start(["ask", "--question", "keep me?"], binary=str(old))
        inbox = wait_for(lambda: self.home.exists() and (self.home / "senpai.sock").exists() and
                         self.inbox(str(old))["inbox"], message="the old service")
        wait_for(lambda: self.items(inbox), message="the question")
        result = subprocess.run([commands.BINARY, "inbox"], env=self.env, capture_output=True, timeout=30)
        self.assertEqual(result.returncode, commands.EXIT_ERROR)
        self.assertIn("another build", result.stderr.decode())
        asking.kill()
        asking.communicate()
        self.stop_service(str(old))

    def test_inbox_page_is_served(self) -> None:
        inbox = self.inbox()["inbox"]
        with urllib.request.urlopen(inbox, timeout=5) as response:
            page = response.read().decode()
            self.assertIn("senpai inbox", page)
            self.assertIn('"itemsUrl":"api/items"', page)
            self.assertIn("default-src 'none'", response.headers["Content-Security-Policy"])
        base = inbox.rsplit("/", 2)[0]
        for path in ("/", "/wrong-token/", "/wrong-token/api/items"):
            with self.assertRaises(urllib.error.HTTPError) as caught:
                urllib.request.urlopen(base + path, timeout=5)
            self.assertEqual(caught.exception.code, 404)

    # -- questions ---------------------------------------------------------

    def ask(self, *args: str) -> tuple[subprocess.Popen, str, dict]:
        process = self.start(["ask", *args])
        inbox = wait_for(lambda: (self.home / "senpai.sock").exists() and self.inbox()["inbox"],
                         message="the service")
        item = wait_for(lambda: self.items(inbox) and self.items(inbox)[0], message="the question")
        return process, inbox, item

    def test_question_answered_with_an_option(self) -> None:
        process, inbox, item = self.ask("--question", "Which port?", "--option", "8080", "--option", "9090",
                                        "--context", "/srv/app")
        self.assertEqual((item["kind"], item["title"], item["options"], item["places"]),
                         ("question", "Which port?", ["8080", "9090"], ["/srv/app"]))
        self.assertEqual(post(inbox + "api/answer", {"id": item["id"], "option": "7070", "text": ""}), 400)
        self.assertEqual(post(inbox + "api/answer", {"id": item["id"], "option": None, "text": "  "}), 400)
        self.assertEqual(post(inbox + "api/answer", {"id": item["id"], "option": "9090", "text": " free one "}), 204)
        self.assertEqual(post(inbox + "api/answer", {"id": item["id"], "option": "9090", "text": ""}), 400)
        code, result, err = self.finish(process)
        self.assertEqual(code, commands.EXIT_OK, err)
        self.assertEqual(result, {"version": "1", "question_id": item["id"], "option": "9090", "text": "free one"})

    def test_question_answered_in_free_text(self) -> None:
        process, inbox, item = self.ask("--question", "Name for the flag?")
        self.assertEqual(post(inbox + "api/answer", {"id": item["id"], "text": "dry-run"}), 204)
        code, result, _ = self.finish(process)
        self.assertEqual((code, result["option"], result["text"]), (commands.EXIT_OK, None, "dry-run"))

    def test_question_dismissed(self) -> None:
        process, inbox, item = self.ask("--question", "Delete the cache?")
        self.assertEqual(post(inbox + "api/answer", {"id": item["id"], "dismissed": True}), 204)
        code, result, _ = self.finish(process)
        self.assertEqual((code, result["dismissed"]), (commands.EXIT_NOT_APPROVED, True))

    def test_cancelled_question_is_withdrawn(self) -> None:
        process, inbox, _ = self.ask("--question", "Still there?")
        process.send_signal(signal.SIGINT)
        process.communicate(timeout=30)
        wait_for(lambda: self.items(inbox) == [], message="the question to be withdrawn")

    def test_question_times_out(self) -> None:
        self.env[commands.TIMEOUT_ENV] = "1"
        process, inbox, _ = self.ask("--question", "Anyone?")
        code, result, err = self.finish(process)
        self.assertEqual((code, result), (commands.EXIT_ERROR, None))
        self.assertIn("timed out", err)
        wait_for(lambda: self.items(inbox) == [], message="the question to be withdrawn")

    def test_question_usage_errors(self) -> None:
        for args in ([], ["--question", "  "], ["--question"], ["--bogus", "x"]):
            result = subprocess.run([commands.BINARY, "ask", *args], env=self.env, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, commands.EXIT_ERROR, args)
            self.assertIn("usage: senpai ask", result.stderr.decode())
        self.assertFalse(self.home.exists(), "usage errors start no service")


if __name__ == "__main__":
    unittest.main()
