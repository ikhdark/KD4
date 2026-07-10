#!/usr/bin/env python3

from datetime import datetime, timezone
from pathlib import Path
import hashlib
import os
import re
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parent / "publish-local-codex.ps1"
HASHING_HELPER = Path(__file__).resolve().parent / "publish-local-codex.hashing.ps1"
CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)
RUN_TIMEOUT_SECONDS = 120
FIXTURE_TIME = 946684900
FRESH_SOURCE_TIME = FIXTURE_TIME + 10_000


def powershell() -> str | None:
    # Prefer Windows PowerShell 5.1: production invokes publish-local-codex.ps1
    # via `powershell -NoProfile -File ...` from the justfile, and 5.1 has
    # stricter native-stderr and StrictMode semantics than pwsh 7 — bugs in
    # that class are invisible when the tests run under pwsh.
    return shutil.which("powershell") or shutil.which("pwsh")


def ps_single_quote(value: str | Path) -> str:
    return "'" + str(value).replace("'", "''") + "'"


PUBLISH_ENV_VARS = (
    "CODEX_LOCAL_PUBLISH_DIR",
    "CODEX_HOME",
    "CODEX_SQLITE_HOME",
    "CODEX_CLI_PATH",
)


def clean_env() -> dict[str, str]:
    # A prior -ConfigureDesktopLocalCli publish persists these at User scope,
    # so the inherited environment can carry them; the script prefers
    # CODEX_LOCAL_PUBLISH_DIR over the test's temp USERPROFILE, which makes
    # assertions machine-state-dependent unless they are stripped.
    env = os.environ.copy()
    for name in PUBLISH_ENV_VARS:
        env.pop(name, None)
    return env


class PublishLocalCodexTestBase(unittest.TestCase):
    def setUp(self) -> None:
        shell = powershell()
        if shell is None:
            self.fail("PowerShell is not available")
        self.shell = shell
        comspec = os.environ.get("ComSpec")
        if not comspec:
            self.skipTest("ComSpec is not available")
        self.source_exe = Path(comspec)
        self.source_exe_bytes = self.source_exe.read_bytes()
        self.repo_temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.repo_temp.cleanup)
        self.repo_root = Path(self.repo_temp.name) / "repo"
        self.repo_root.mkdir()
        (self.repo_root / "codex-rs").mkdir()
        self.source_code_mode_host = (
            Path(self.repo_temp.name) / "fixture-code-mode-host.exe"
        )
        self.source_code_mode_host_bytes = b"codex-code-mode-host-test-fixture"
        self.source_code_mode_host.write_bytes(self.source_code_mode_host_bytes)
        self.repo_fixture_initialized = False

    def init_repo_fixture(self) -> None:
        if self.repo_fixture_initialized:
            return
        if shutil.which("git") is None:
            self.skipTest("git is not available")

        self.run_git("init", "--quiet")
        self.run_git("config", "user.email", "codex-test@example.com")
        self.run_git("config", "user.name", "Codex Test")
        (self.repo_root / "codex-rs").mkdir(exist_ok=True)
        tracked = self.repo_root / "codex-rs" / "tracked-source.rs"
        tracked.write_text("initial\n", encoding="utf-8")
        os.utime(tracked, (FIXTURE_TIME, FIXTURE_TIME))
        fixture_date = datetime.fromtimestamp(FIXTURE_TIME, timezone.utc).isoformat()
        self.run_git("add", "codex-rs/tracked-source.rs")
        self.run_git(
            "commit",
            "--quiet",
            "-m",
            "initial",
            env={
                **os.environ,
                "GIT_AUTHOR_DATE": fixture_date,
                "GIT_COMMITTER_DATE": fixture_date,
            },
        )
        self.repo_fixture_initialized = True

    def write_cargo_lock_with_v8(self, version: str = "149.2.0") -> Path:
        cargo_lock = self.repo_root / "codex-rs" / "Cargo.lock"
        cargo_lock.write_text(
            "\n".join(
                [
                    "[[package]]",
                    'name = "v8"',
                    f'version = "{version}"',
                    'source = "registry+https://github.com/rust-lang/crates.io-index"',
                    "",
                ]
            ),
            encoding="utf-8",
        )
        return cargo_lock

    def publish_env_without_v8_archive(self, user_profile: Path) -> dict[str, str]:
        env = clean_env()
        env["USERPROFILE"] = str(user_profile)
        for name in (
            "RUSTY_V8_ARCHIVE",
            "RUSTY_V8_MIRROR",
            "V8_FROM_SOURCE",
        ):
            env.pop(name, None)
        return env

    def expected_windows_rusty_v8_target(self) -> str:
        arch = (
            os.environ.get("PROCESSOR_ARCHITEW6432")
            or os.environ.get("PROCESSOR_ARCHITECTURE")
            or "AMD64"
        ).upper()
        if arch == "ARM64":
            return "aarch64-pc-windows-msvc"
        return "x86_64-pc-windows-msvc"

    def rusty_v8_archive_name(self) -> str:
        return f"rusty_v8_release_{self.expected_windows_rusty_v8_target()}.lib.gz"

    def rusty_v8_archive_url(self, version: str = "149.2.0") -> str:
        return (
            f"https://github.com/denoland/rusty_v8/releases/download/v{version}/"
            f"{self.rusty_v8_archive_name()}"
        )

    def rusty_v8_cache_path(self, user_profile: Path, version: str = "149.2.0") -> Path:
        cache_name = re.sub(r"[^A-Za-z0-9]", "_", self.rusty_v8_archive_url(version))
        return user_profile / ".cargo" / ".rusty_v8" / cache_name

    def write_rusty_v8_checksum(
        self, archive_path: Path, version: str = "149.2.0"
    ) -> str:
        checksum = hashlib.sha256(archive_path.read_bytes()).hexdigest()
        checksum_dir = self.repo_root / "third_party" / "v8"
        checksum_dir.mkdir(parents=True, exist_ok=True)
        (checksum_dir / f"rusty_v8_{version.replace('.', '_')}.sha256").write_text(
            f"{checksum}  {self.rusty_v8_archive_name()}\n",
            encoding="utf-8",
        )
        return checksum

    def run_git(
        self,
        *args: str,
        env: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        result = subprocess.run(
            ["git", "-C", str(self.repo_root), *args],
            text=True,
            capture_output=True,
            check=False,
            timeout=30,
            env=env,
        )
        if result.returncode != 0:
            self.fail(f"git {' '.join(args)} failed:\n{result.stderr}")
        return result

    def write_build_stamp(self, profile: str, timestamp: float) -> Path:
        stamp = (
            self.repo_root
            / "codex-rs"
            / "target"
            / f"codex-local-publish-{profile}.stamp"
        )
        stamp.parent.mkdir(parents=True, exist_ok=True)
        stamp_time = datetime.fromtimestamp(timestamp, timezone.utc)
        stamp.write_text(
            f"{stamp_time:%Y-%m-%dT%H:%M:%S}.{stamp_time.microsecond * 10:07d}Z",
            encoding="utf-8",
        )
        return stamp

    def touch_tracked_source(self, timestamp: float) -> Path:
        tracked = self.repo_root / "codex-rs" / "tracked-source.rs"
        tracked.write_text(f"changed at {timestamp}\n", encoding="utf-8")
        os.utime(tracked, (timestamp, timestamp))
        return tracked

    def touch_unrelated_source(self, timestamp: float) -> Path:
        docs = self.repo_root / "docs"
        docs.mkdir()
        unrelated = docs / "notes.md"
        unrelated.write_text(f"changed at {timestamp}\n", encoding="utf-8")
        os.utime(unrelated, (timestamp, timestamp))
        return unrelated

    def publish_args(self, args: tuple[str, ...]) -> tuple[str, ...]:
        if "-RepoRoot" in args:
            return args
        return ("-RepoRoot", str(self.repo_root), *args)

    def proof_value(self, text: str, name: str) -> str | None:
        prefix = f"{name}:"
        for line in text.splitlines():
            if line.startswith(prefix):
                return line[len(prefix) :].strip()
        return None

    def assert_proof_value(self, text: str, name: str, expected: str) -> None:
        self.assertEqual(
            self.proof_value(text, name),
            expected,
            f"proof value mismatch for {name!r}\nstdout:\n{text}",
        )

    def assert_publish_readiness(self, text: str, expected: str) -> None:
        self.assert_proof_value(text, "publishReadiness", expected)

    def assert_no_publish_temps(self, install_dir: Path) -> None:
        self.assertEqual(list(install_dir.glob(".codex-local-publish.*.tmp")), [])

    def install_matching_code_mode_host(
        self,
        install_dir: Path,
        *,
        timestamp: float | None = None,
    ) -> Path:
        target = install_dir / "codex-code-mode-host.exe"
        target.write_bytes(self.source_code_mode_host_bytes)
        if timestamp is not None:
            os.utime(target, (timestamp, timestamp))
        return target

    def hash_cache_path(self, path: Path) -> Path:
        safe_name = hashlib.sha256(str(path.resolve()).encode("utf-8")).hexdigest()
        return (
            self.repo_root
            / "codex-rs"
            / "target"
            / "codex-local-publish-hashes"
            / f"{safe_name}.sha256.json"
        )

    def write_fake_codex(
        self,
        path: Path,
        *,
        commit: str = "test-commit",
        timestamp: float | None = None,
    ) -> Path:
        path.write_text(
            "\r\n".join(
                [
                    "@echo off",
                    "echo codex 9.9.9",
                    f"echo commit: {commit}",
                ]
            ),
            encoding="utf-8",
        )
        if timestamp is not None:
            os.utime(path, (timestamp, timestamp))
        return path

    def copy_valid_codex(
        self,
        path: Path,
        *,
        timestamp: float | None = None,
        append_padding: bool = False,
    ) -> Path:
        # Use %ComSpec% as a tiny executable stand-in that satisfies the script's
        # version probe under redirected stdio.
        path.write_bytes(self.source_exe_bytes)
        if append_padding:
            with path.open("ab") as handle:
                handle.write(b"\r\ncodex-test-padding")
        if timestamp is not None:
            os.utime(path, (timestamp, timestamp))
        return path

    def run_script(
        self,
        *args: str,
        env: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        publish_args = list(self.publish_args(args))
        if "-SourceCodeModeHostExe" not in publish_args:
            publish_args.extend(
                ["-SourceCodeModeHostExe", str(self.source_code_mode_host)]
            )
        return subprocess.run(
            [
                self.shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                str(SCRIPT),
                *publish_args,
            ],
            text=True,
            capture_output=True,
            check=False,
            env=clean_env() if env is None else env,
            timeout=RUN_TIMEOUT_SECONDS,
        )


if __name__ == "__main__":
    unittest.main()
