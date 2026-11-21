from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


class SenpaiCommandTests(unittest.TestCase):
    def run_senpai(self, command: Path, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run([str(command), *args], text=True, capture_output=True, timeout=60)

    def test_command_reaches_the_client(self) -> None:
        result = self.run_senpai(ROOT / "bin/senpai", "--help")
        self.assertEqual(0, result.returncode, result.stderr)
        for name in ("review", "review-doc", "verify", "consume"):
            self.assertIn(name, result.stdout)

    def test_command_works_through_a_relative_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            folder = Path(tmp).resolve()
            link = folder / "senpai"
            link.symlink_to(os.path.relpath(ROOT / "bin/senpai", folder))
            result = self.run_senpai(link, "--help")
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("review-doc", result.stdout)


if __name__ == "__main__":
    unittest.main()
