from __future__ import annotations

import shutil
import subprocess
import unittest
from pathlib import Path

from scripts.maintenance import check_v8_boundary

CODEX_RS = Path(__file__).resolve().parents[2] / "codex-rs"


class CheckV8BoundaryTest(unittest.TestCase):
    def test_graph_without_v8_passes(self) -> None:
        packages = []

        def runner(*args: object, **kwargs: object) -> subprocess.CompletedProcess[str]:
            command = args[0]
            packages.append(command[command.index("-p") + 1])
            self.assertEqual(command[-2:], ["--target", "x86_64-pc-windows-msvc"])
            return subprocess.CompletedProcess(
                args=[], returncode=0, stdout="codex-core v0.0.0\nv8-like v1.0.0\n", stderr=""
            )

        self.assertEqual(
            check_v8_boundary.check_v8_boundary(
                ["--target", "x86_64-pc-windows-msvc"], runner=runner
            ),
            0,
        )
        self.assertEqual(packages, ["codex-cli", "codex-core"])

    def test_graph_with_v8_fails(self) -> None:
        def runner(*args: object, **kwargs: object) -> subprocess.CompletedProcess[str]:
            return subprocess.CompletedProcess(
                args=[], returncode=0, stdout="codex-cli v0.0.0\nv8 v152.2.0\n", stderr=""
            )

        self.assertEqual(check_v8_boundary.check_v8_boundary([], runner=runner), 1)

    def test_cargo_failure_is_preserved(self) -> None:
        def runner(*args: object, **kwargs: object) -> subprocess.CompletedProcess[str]:
            return subprocess.CompletedProcess(
                args=[], returncode=7, stdout="", stderr="boom\n"
            )

        self.assertEqual(check_v8_boundary.check_v8_boundary([], runner=runner), 7)

    @unittest.skipUnless(shutil.which("cargo"), "cargo is required")
    def test_workspace_keeps_v8_in_the_host(self) -> None:
        def runner(*args: object, **kwargs: object) -> subprocess.CompletedProcess[str]:
            check = kwargs.pop("check")
            return subprocess.run(*args, cwd=CODEX_RS, check=check, **kwargs)

        self.assertEqual(
            check_v8_boundary.check_v8_boundary(["--offline"], runner=runner), 0
        )
        host = subprocess.run(
            ["cargo", "tree", "-p", "codex-code-mode-host", "-e", "normal",
             "--prefix", "none", "--format", "{p}", "--offline"],
            cwd=CODEX_RS, capture_output=True, text=True, check=True,
        )
        # Guard the probe itself: the host must still link V8 under this format.
        self.assertTrue(any(line.startswith("v8 ") for line in host.stdout.splitlines()))


if __name__ == "__main__":
    unittest.main()
