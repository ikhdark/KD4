from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class RustToolEnvTest(unittest.TestCase):
    def run_just_shell(
        self,
        command: str,
        *,
        cwd: Path,
        env_updates: dict[str, str] | None = None,
        remove_env: tuple[str, ...] = (),
    ) -> subprocess.CompletedProcess[str]:
        just_shell = Path(__file__).with_name("just-shell.py").resolve()
        env = os.environ.copy()
        env["CODEXKD_DISABLE_SCRIPT_VENV"] = "1"
        for name in remove_env:
            env.pop(name, None)
        if env_updates:
            env.update(env_updates)
        return subprocess.run(
            [sys.executable, str(just_shell), command, "test-rust-tool-env"],
            cwd=cwd,
            env=env,
            check=False,
            capture_output=True,
            text=True,
        )

    def test_just_shell_cli_loads_shared_policy_outside_the_repository(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            completed = self.run_just_shell(
                "Write-Output 'loaded-from-outside'",
                cwd=Path(temp),
            )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stdout.strip(), "loaded-from-outside")

    def test_just_shell_cli_applies_shared_sccache_wrapper_and_cache_override(
        self,
    ) -> None:
        command = "Write-Output $env:SCCACHE_CACHE_SIZE; # cargo"
        with tempfile.TemporaryDirectory() as temp:
            cwd = Path(temp)
            default = self.run_just_shell(
                command,
                cwd=cwd,
                env_updates={"RUSTC_WRAPPER": "C:/tools/sccache.exe"},
                remove_env=("CI", "SCCACHE_CACHE_SIZE", "CODEX_SCCACHE_CACHE_SIZE"),
            )
            unrelated_wrapper = self.run_just_shell(
                command,
                cwd=cwd,
                env_updates={"RUSTC_WRAPPER": "cachepot"},
                remove_env=("CI", "SCCACHE_CACHE_SIZE", "CODEX_SCCACHE_CACHE_SIZE"),
            )
            overridden = self.run_just_shell(
                command,
                cwd=cwd,
                env_updates={
                    "RUSTC_WRAPPER": "C:/tools/sccache.exe",
                    "CODEX_SCCACHE_CACHE_SIZE": "100G",
                },
                remove_env=("CI", "SCCACHE_CACHE_SIZE"),
            )

        self.assertEqual(default.returncode, 0, default.stderr)
        self.assertEqual(default.stdout.strip(), "80G")
        self.assertEqual(unrelated_wrapper.returncode, 0, unrelated_wrapper.stderr)
        self.assertEqual(unrelated_wrapper.stdout.strip(), "")
        self.assertEqual(overridden.returncode, 0, overridden.stderr)
        self.assertEqual(overridden.stdout.strip(), "100G")

    def test_just_shell_cli_uses_shared_windows_linker_fallback_order(self) -> None:
        pwsh = shutil.which("pwsh.exe") or shutil.which("pwsh")
        self.assertIsNotNone(pwsh, "PowerShell is required by the just-shell runtime")
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            scoop = root / "custom-scoop"
            user = root / "user"
            expected = scoop / "apps" / "llvm" / "current" / "bin" / "lld-link.exe"
            expected.parent.mkdir(parents=True)
            expected.write_text("", encoding="utf-8")
            user_candidate = (
                user / "scoop" / "apps" / "llvm" / "current" / "bin" / "lld-link.exe"
            )
            user_candidate.parent.mkdir(parents=True)
            user_candidate.write_text("", encoding="utf-8")

            completed = self.run_just_shell(
                "Write-Output $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER; "
                "Write-Output $env:CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER; # cargo",
                cwd=root,
                env_updates={
                    "PATH": str(Path(pwsh).parent),
                    "SCOOP": str(scoop),
                    "USERPROFILE": str(user),
                },
                remove_env=(
                    "CI",
                    "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER",
                    "CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER",
                ),
            )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(
            completed.stdout.splitlines(),
            [str(expected), str(expected)],
        )


if __name__ == "__main__":
    unittest.main()
