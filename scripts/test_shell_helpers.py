#!/usr/bin/env python3

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]


def script_text(name: str) -> str:
    return (REPO_ROOT / "scripts" / name).read_text(encoding="utf-8")


def write_executable(path: Path, contents: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(contents, encoding="utf-8", newline="\n")
    path.chmod(0o755)
    return path


class ShellHelperScriptsTest(unittest.TestCase):
    def bash(self) -> str:
        candidates = [
            Path.home() / "scoop" / "apps" / "git" / "current" / "bin" / "bash.exe",
            Path(os.environ.get("ProgramFiles", "")) / "Git" / "bin" / "bash.exe",
        ]
        discovered = shutil.which("bash")
        if discovered is not None:
            candidates.append(Path(discovered))
        bash = next((str(path) for path in candidates if path.is_file()), None)
        if bash is None:
            self.skipTest("bash is not available")
        probe = subprocess.run(
            [bash, "-c", 'test -n "$BASH_VERSION"'],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            timeout=30,
        )
        if probe.returncode != 0:
            self.skipTest("the available bash command did not start Bash")
        return bash

    def copy_script(self, fixture_root: Path, name: str) -> Path:
        return write_executable(
            fixture_root / "scripts" / name,
            script_text(name),
        )

    def run_fixture(
        self,
        fixture_root: Path,
        command: str,
        *args: str,
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [self.bash(), "-c", command, "bash", *args],
            cwd=fixture_root,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            timeout=30,
        )

    def test_debug_codex_uses_existing_windows_binary_and_preserves_args(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture_root = Path(temp_dir)
            self.copy_script(fixture_root, "debug-codex.sh")
            write_executable(
                fixture_root / "codex-rs" / "target" / "debug" / "codex.exe",
                """#!/usr/bin/env bash
printf 'binary\\n' > "$CALL_LOG"
printf 'arg=%s\\n' "$@" >> "$CALL_LOG"
""",
            )
            write_executable(
                fixture_root / "fake-bin" / "cargo",
                """#!/usr/bin/env bash
printf 'cargo\\n' > "$CALL_LOG"
exit 99
""",
            )

            result = self.run_fixture(
                fixture_root,
                """export CALL_LOG="$PWD/calls.log"
export PATH="$PWD/fake-bin:$PATH"
export CODEX_DEBUG_USE_EXISTING_BINARY=1
bash scripts/debug-codex.sh "$@"
""",
                "value with spaces",
                "literal*",
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                (fixture_root / "calls.log").read_text(encoding="utf-8").splitlines(),
                ["binary", "arg=value with spaces", "arg=literal*"],
            )

    def test_debug_codex_cargo_fallback_preserves_exit_and_args(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture_root = Path(temp_dir)
            self.copy_script(fixture_root, "debug-codex.sh")
            (fixture_root / "codex-rs").mkdir()
            write_executable(
                fixture_root / "fake-bin" / "cargo",
                """#!/usr/bin/env bash
printf 'cargo-pwd=%s\\n' "$PWD" > "$CALL_LOG"
printf 'arg=%s\\n' "$@" >> "$CALL_LOG"
exit 37
""",
            )

            result = self.run_fixture(
                fixture_root,
                """export CALL_LOG="$PWD/calls.log"
export PATH="$PWD/fake-bin:$PATH"
unset CODEX_DEBUG_USE_EXISTING_BINARY
bash scripts/debug-codex.sh "$@"
""",
                "value with spaces",
                "literal*",
            )

            self.assertEqual(result.returncode, 37, result.stderr)
            lines = (
                (fixture_root / "calls.log").read_text(encoding="utf-8").splitlines()
            )
            self.assertTrue(lines[0].endswith("/codex-rs"), lines[0])
            self.assertEqual(
                lines[1:],
                [
                    "arg=run",
                    "arg=--quiet",
                    "arg=--bin",
                    "arg=codex",
                    "arg=--",
                    "arg=value with spaces",
                    "arg=literal*",
                ],
            )


if __name__ == "__main__":
    unittest.main()
