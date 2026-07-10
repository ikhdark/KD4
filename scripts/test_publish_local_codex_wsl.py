#!/usr/bin/env python3

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
BRIDGE = REPO_ROOT / "scripts" / "publish-local-codex-wsl.sh"


def write_executable(path: Path, contents: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(contents, encoding="utf-8", newline="\n")
    path.chmod(0o755)
    return path


class WslPublishBridgeTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        candidates = [
            Path.home() / "scoop" / "apps" / "git" / "current" / "bin" / "bash.exe",
            Path(os.environ.get("ProgramFiles", "")) / "Git" / "bin" / "bash.exe",
        ]
        discovered = shutil.which("bash")
        if discovered is not None:
            candidates.append(Path(discovered))
        cls.bash = next((str(path) for path in candidates if path.is_file()), None)
        if cls.bash is None:
            raise unittest.SkipTest("bash is not available")

        probe = subprocess.run(
            [cls.bash, "-c", 'test -n "$BASH_VERSION"'],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            timeout=30,
        )
        if probe.returncode != 0:
            raise unittest.SkipTest("the available bash command did not start Bash")

    def prepare_fixture(self, fixture_root: Path) -> None:
        write_executable(
            fixture_root / "scripts" / BRIDGE.name,
            BRIDGE.read_text(encoding="utf-8"),
        )
        write_executable(
            fixture_root / "fake-bin" / "wslpath",
            """#!/usr/bin/env bash
set -euo pipefail
printf 'arg=%s\\n' "$@" >> "$WSLPATH_LOG"
if [[ "$#" -ne 2 || "$1" != "-w" ]]; then
  exit 91
fi
printf 'WIN<%s>\\n' "$2"
""",
        )
        write_executable(
            fixture_root / "fake-bin" / "powershell.exe",
            """#!/usr/bin/env bash
set -euo pipefail
printf 'env=%s\\n' "${CODEX_LOCAL_PUBLISH_DIR:-}" > "$POWERSHELL_LOG"
printf 'arg=%s\\n' "$@" >> "$POWERSHELL_LOG"
exit "${POWERSHELL_EXIT:-0}"
""",
        )

    def run_bridge(
        self,
        fixture_root: Path,
        *args: str,
        publish_dir: str | None = None,
        powershell_exit: int = 0,
    ) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env.pop("CODEX_LOCAL_PUBLISH_DIR", None)
        env["POWERSHELL_EXIT"] = str(powershell_exit)
        if publish_dir is not None:
            env["CODEX_LOCAL_PUBLISH_DIR"] = publish_dir
        return subprocess.run(
            [
                self.bash,
                "-c",
                """export PATH="$PWD/fake-bin:$PATH"
export POWERSHELL_LOG="$PWD/powershell.log"
export WSLPATH_LOG="$PWD/wslpath.log"
bash scripts/publish-local-codex-wsl.sh "$@"
""",
                "bash",
                *args,
            ],
            cwd=fixture_root,
            env=env,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            timeout=30,
        )

    def powershell_log(self, fixture_root: Path) -> list[str]:
        return (
            (fixture_root / "powershell.log").read_text(encoding="utf-8").splitlines()
        )

    def test_separate_case_insensitive_path_flags_and_arguments_are_preserved(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture_root = Path(temp_dir)
            self.prepare_fixture(fixture_root)

            result = self.run_bridge(
                fixture_root,
                "-rEpOrOoT",
                "/mnt/z/custom repo",
                "-sOuRcEcOdEmOdEhOsTeXe",
                "/mnt/c/build/host binary.exe",
                "-INSTALLDIR",
                "relative/output",
                "--unknown",
                "value with spaces",
                "literal*",
                "semi;colon",
                "",
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            lines = self.powershell_log(fixture_root)
            self.assertEqual(lines[0], "env=")
            self.assertEqual(
                lines[1:5],
                ["arg=-NoProfile", "arg=-ExecutionPolicy", "arg=Bypass", "arg=-File"],
            )
            self.assertRegex(
                lines[5], r"^arg=WIN<.*/scripts/publish-local-codex\.ps1>$"
            )
            self.assertEqual(
                lines[6:],
                [
                    "arg=-rEpOrOoT",
                    "arg=WIN</mnt/z/custom repo>",
                    "arg=-sOuRcEcOdEmOdEhOsTeXe",
                    "arg=WIN</mnt/c/build/host binary.exe>",
                    "arg=-INSTALLDIR",
                    "arg=relative/output",
                    "arg=--unknown",
                    "arg=value with spaces",
                    "arg=literal*",
                    "arg=semi;colon",
                    "arg=",
                ],
            )

    def test_inline_case_insensitive_paths_and_publish_env_are_translated(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture_root = Path(temp_dir)
            self.prepare_fixture(fixture_root)

            result = self.run_bridge(
                fixture_root,
                "-rEpOrOoT=/mnt/z/custom repo",
                "-sOuRcEcOdEmOdEhOsTeXe=/mnt/c/build/host binary.exe",
                "-lOcAlCoDeXhOmE=/home/user/local codex",
                publish_dir="/mnt/e/local kd",
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            lines = self.powershell_log(fixture_root)
            self.assertEqual(lines[0], "env=WIN</mnt/e/local kd>")
            self.assertEqual(
                lines[1:5],
                ["arg=-NoProfile", "arg=-ExecutionPolicy", "arg=Bypass", "arg=-File"],
            )
            self.assertRegex(
                lines[5], r"^arg=WIN<.*/scripts/publish-local-codex\.ps1>$"
            )
            self.assertEqual(
                lines[6:],
                [
                    "arg=-RepoRoot=WIN</mnt/z/custom repo>",
                    "arg=-SourceCodeModeHostExe=WIN</mnt/c/build/host binary.exe>",
                    "arg=-LocalCodexHome=WIN</home/user/local codex>",
                ],
            )

    def test_default_repo_root_is_injected_and_powershell_exit_is_propagated(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture_root = Path(temp_dir)
            self.prepare_fixture(fixture_root)

            result = self.run_bridge(
                fixture_root,
                "-NoBuild",
                "value with spaces",
                "literal*",
                powershell_exit=37,
            )

            self.assertEqual(result.returncode, 37, result.stderr)
            lines = self.powershell_log(fixture_root)
            self.assertEqual(
                lines[1:5],
                ["arg=-NoProfile", "arg=-ExecutionPolicy", "arg=Bypass", "arg=-File"],
            )
            self.assertRegex(
                lines[5], r"^arg=WIN<.*/scripts/publish-local-codex\.ps1>$"
            )
            self.assertEqual(lines[6], "arg=-RepoRoot")
            self.assertRegex(lines[7], r"^arg=WIN<.*>$")
            self.assertEqual(
                lines[8:],
                ["arg=-NoBuild", "arg=value with spaces", "arg=literal*"],
            )


if __name__ == "__main__":
    unittest.main()
