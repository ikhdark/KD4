#!/usr/bin/env python3

import base64
from datetime import datetime, timezone
import json
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


class PublishLocalCodexSourceLayoutTest(unittest.TestCase):
    def test_hashing_helper_is_dot_sourced_without_duplicate_definitions(self) -> None:
        publish_script = SCRIPT.read_text(encoding="utf-8")
        hashing_helper = HASHING_HELPER.read_text(encoding="utf-8")

        self.assertIn(
            '. (Join-Path $PSScriptRoot "publish-local-codex.hashing.ps1")',
            publish_script,
        )
        self.assertIn("function Get-FileSha256Cached", hashing_helper)
        self.assertNotIn("function Get-FileSha256Cached", publish_script)
        self.assertNotIn("function Get-CachedFileSha256", publish_script)
        self.assertNotIn("function Remove-StaleFileSha256CacheEntries", publish_script)

    def test_publish_binary_proof_uses_direct_hashing_not_metadata_cache(self) -> None:
        publish_script = SCRIPT.read_text(encoding="utf-8")

        self.assertIn('$sourceSha256Mode = "hashed"', publish_script)
        self.assertIn("$sourceSha256 = Get-FileSha256 $SourceExe", publish_script)
        self.assertIn(
            "$targetBeforeSha256 = Get-FileSha256 $targetPath", publish_script
        )
        self.assertNotIn("Get-FileSha256Cached $SourceExe", publish_script)
        self.assertNotIn("Get-FileSha256Cached $targetPath", publish_script)

    @unittest.skipUnless(os.name == "nt", "PowerShell helper is Windows-only")
    def test_hashing_helper_initializes_cache_state_when_dot_sourced_directly(
        self,
    ) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")

        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            source = temp_path / "codex.exe"
            cache_dir = temp_path / "cache"
            source.write_bytes(b"codex")
            expected = hashlib.sha256(source.read_bytes()).hexdigest()

            result = subprocess.run(
                [
                    shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    (
                        # Mirror the production session: the publish script
                        # dot-sources this helper under StrictMode Latest.
                        "Set-StrictMode -Version Latest; "
                        "$ErrorActionPreference = 'Stop'; "
                        f". {ps_single_quote(HASHING_HELPER)}; "
                        f"Get-FileSha256Cached -Path {ps_single_quote(source)} "
                        f"-CacheDir {ps_single_quote(cache_dir)}"
                    ),
                ],
                text=True,
                capture_output=True,
                check=False,
                creationflags=CREATE_NO_WINDOW,
            )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertEqual(result.stdout.strip(), expected)

    def test_publish_script_uses_global_publish_mutex(self) -> None:
        publish_script = SCRIPT.read_text(encoding="utf-8")

        self.assertIn('"Global\\CodexLocalPublish"', publish_script)
        self.assertIn(".WaitOne([TimeSpan]::FromSeconds(30))", publish_script)
        self.assertIn(".ReleaseMutex()", publish_script)

    def test_publish_build_calls_shared_msvc_linker_setup(self) -> None:
        publish_script = SCRIPT.read_text(encoding="utf-8")

        self.assertIn(
            '. (Join-Path $PSScriptRoot "common-rust-env.ps1")',
            publish_script,
        )
        self.assertIn("Set-CodexRustMsvcLinkerEnvironment", publish_script)

    def test_just_publish_recipes_configure_desktop_local_cli(self) -> None:
        justfile = (SCRIPT.parent.parent / "justfile").read_text(encoding="utf-8")

        self.assertIn(
            'publish-local-codex.ps1" -DryRun -ConfigureDesktopLocalCli', justfile
        )
        self.assertIn(
            'publish-local-codex.ps1" -DryRun -SkipBuild -FailOnStaleSourceBuild -ConfigureDesktopLocalCli',
            justfile,
        )
        self.assertIn(
            'publish-local-codex.ps1" -AutoSkipBuild -ConfigureDesktopLocalCli',
            justfile,
        )
        self.assertIn("publish-local-codex-final *args:", justfile)
        self.assertIn(
            "-AutoSkipBuild -Profile release -RunDoctor -CloseRunningTargetTimeoutSeconds 30",
            justfile,
        )
        self.assertIn("publish-local-codex-final-dry-run *args:", justfile)
        self.assertIn(
            "-DryRun -AutoSkipBuild -Profile release -RunDoctor -CloseRunningTargetTimeoutSeconds 30",
            justfile,
        )
        self.assertIn("publish-local-codex-runtime-proof *args:", justfile)
        self.assertIn(
            "-DryRun -SkipBuild -RunDoctor -RuntimeProof -FailOnStaleSourceBuild",
            justfile,
        )
        self.assertIn("publish-local-codex-final-test-run *args:", justfile)
        self.assertIn(
            "-TestRun -AutoSkipBuild -Profile release -RunDoctor -CloseRunningTargetTimeoutSeconds 30",
            justfile,
        )
        self.assertIn("publish-local-codex-build-only *args:", justfile)
        self.assertIn('publish-local-codex.ps1" -BuildOnly', justfile)
        self.assertIn(
            "-ConfigureDesktopLocalCli -DesktopCliEnvironmentTarget User", justfile
        )

    def test_default_local_publish_target_is_not_openai_appdata_bin(self) -> None:
        publish_script = SCRIPT.read_text(encoding="utf-8")

        self.assertIn('Join-Path $env:USERPROFILE "Desktop\\LOCAL-KD"', publish_script)
        self.assertNotIn(
            'Join-Path $env:LOCALAPPDATA "OpenAI\\Codex\\bin\\codexKD-local"',
            publish_script,
        )

    def test_publish_doctor_allows_only_missing_auth_failure(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")

        command = rf"""
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile('{SCRIPT}', [ref]$tokens, [ref]$errors)
if ($errors.Count -ne 0) {{
    throw "Failed to parse publish script: $($errors[0].Message)"
}}
$functions = $ast.FindAll({{
    param($node)
    $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
        ($node.Name -eq 'ConvertFrom-DoctorOutput' -or
            $node.Name -eq 'Test-DoctorFailureAllowedForPublish')
}}, $true)
if (@($functions).Count -ne 2) {{
    throw 'Doctor publish classifier functions were not found.'
}}
foreach ($function in $functions) {{
    Invoke-Expression $function.Extent.Text
}}
$authOnly = '{{"checks":{{"auth.credentials":{{"status":"fail"}},"network.websocket_reachability":{{"status":"warning"}}}}}}'
$configFailure = '{{"checks":{{"auth.credentials":{{"status":"fail"}},"config.load":{{"status":"fail"}}}}}}'
[pscustomobject]@{{
    authOnly = Test-DoctorFailureAllowedForPublish -OutputLines @($authOnly)
    configFailure = Test-DoctorFailureAllowedForPublish -OutputLines @($configFailure)
}} | ConvertTo-Json -Compress
"""
        result = subprocess.run(
            [shell, "-NoProfile", "-Command", command],
            text=True,
            capture_output=True,
            check=False,
            timeout=RUN_TIMEOUT_SECONDS,
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        output = json.loads(result.stdout)
        self.assertTrue(output["authOnly"])
        self.assertFalse(output["configFailure"])

    def test_publish_run_doctor_uses_publish_classifier(self) -> None:
        publish_script = SCRIPT.read_text(encoding="utf-8")

        self.assertIn("function Invoke-DoctorForPublish", publish_script)
        self.assertIn("warning: auth.credentials missing", publish_script)
        self.assertEqual(
            publish_script.count("Invoke-DoctorForPublish -TargetPath $targetPath"),
            3,
        )

    def test_local_release_profile_is_minimal_release_inheritance(self) -> None:
        cargo_toml = (SCRIPT.parent.parent / "codex-rs" / "Cargo.toml").read_text(
            encoding="utf-8"
        )

        self.assertIn(
            '[profile.local-release]\ninherits = "release"\nlto = false',
            cargo_toml,
        )
        local_release_block = cargo_toml.split("[profile.local-release]", 1)[1].split(
            "[profile.",
            1,
        )[0]
        self.assertNotIn("incremental", local_release_block)
        self.assertNotIn("codegen-units", local_release_block)
        self.assertNotIn("debug", local_release_block)
        self.assertNotIn("strip", local_release_block)


@unittest.skipUnless(os.name == "nt", "publish-local-codex is Windows-only")
class PublishLocalCodexTest(unittest.TestCase):
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

    def hash_cache_path(self, path: Path) -> Path:
        encoded = base64.b64encode(str(path.resolve()).encode("utf-8")).decode("ascii")
        safe_name = "".join("_" if char in "+/=" else char for char in encoded)
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
        return subprocess.run(
            [
                self.shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                str(SCRIPT),
                *self.publish_args(args),
            ],
            text=True,
            capture_output=True,
            check=False,
            env=clean_env() if env is None else env,
            timeout=RUN_TIMEOUT_SECONDS,
        )

    def test_dry_run_reports_proof_without_writing_target(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-SourceExe",
                str(self.source_exe),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("DRY-RUN", result.stdout)
            self.assertIn("sourceSha256:", result.stdout)
            self.assertIn("targetPath:", result.stdout)
            self.assertFalse((install_dir / "codex.exe").exists())

    def test_dry_run_allows_missing_default_source_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"

            result = self.run_script(
                "-DryRun",
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("profile: release", result.stdout)
            self.assertIn("sourceMissing: true", result.stdout)
            self.assertIn("buildCommand: cargo --config", result.stdout)
            self.assertIn(" build --target-dir ", result.stdout)
            self.assertIn("--profile release", result.stdout)
            self.assertFalse((install_dir / "codex.exe").exists())

    def test_dry_run_reports_missing_windows_rusty_v8_archive(self) -> None:
        self.write_cargo_lock_with_v8()
        archive_url = self.rusty_v8_archive_url()
        cache_name = re.sub(r"[^A-Za-z0-9]", "_", archive_url)
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            user_profile = temp_path / "profile"
            user_profile.mkdir()
            env = self.publish_env_without_v8_archive(user_profile)

            result = self.run_script(
                "-DryRun",
                "-InstallDir",
                str(install_dir),
                env=env,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("v8ArchiveVersion: 149.2.0", result.stdout)
            self.assertIn(
                f"v8ArchiveUrl: {archive_url}",
                result.stdout,
            )
            self.assertIn(cache_name, result.stdout)
            self.assertIn("v8ArchiveStatus: missing", result.stdout)
            self.assertIn("v8ArchiveRemedy:", result.stdout)
            self.assertIn("-RustyV8Archive", result.stdout)
            self.assertIn("-AllowRustyV8Download", result.stdout)

    def test_publish_fails_before_cargo_when_windows_rusty_v8_archive_missing(
        self,
    ) -> None:
        self.write_cargo_lock_with_v8()
        target = self.expected_windows_rusty_v8_target()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            user_profile = temp_path / "profile"
            user_profile.mkdir()
            env = self.publish_env_without_v8_archive(user_profile)

            result = self.run_script(
                "-SourceExe",
                str(self.source_exe),
                "-InstallDir",
                str(install_dir),
                env=env,
            )

            combined = result.stdout + result.stderr
            self.assertNotEqual(result.returncode, 0, combined)
            self.assertIn("v8ArchiveStatus: missing", result.stdout)
            self.assertIn(
                f"Rusty V8 archive is missing for v149.2.0 ({target})",
                combined,
            )
            self.assertNotIn("cargo check failed", combined)

    def test_publish_seeds_windows_rusty_v8_cache_from_archive(self) -> None:
        self.write_cargo_lock_with_v8()
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            user_profile = temp_path / "profile"
            user_profile.mkdir()
            fake_codex = self.copy_valid_codex(
                temp_path / "fake-codex.exe",
                timestamp=FRESH_SOURCE_TIME,
                append_padding=True,
            )
            archive = temp_path / self.rusty_v8_archive_name()
            archive.write_bytes(b"fake rusty v8 archive")
            checksum = self.write_rusty_v8_checksum(archive)
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            fake_cargo = fake_bin / "cargo.cmd"
            fake_cargo.write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        "echo fake cargo %*",
                        "exit /b 0",
                    ]
                ),
                encoding="utf-8",
            )
            env = self.publish_env_without_v8_archive(user_profile)
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"

            result = self.run_script(
                "-RustyV8Archive",
                str(archive),
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
                env=env,
            )

            cache_path = self.rusty_v8_cache_path(user_profile)
            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn(f"v8ArchiveChecksum: {checksum}", result.stdout)
            self.assertIn("v8ArchiveChecksumStatus: ok", result.stdout)
            self.assertIn("v8ArchiveCacheAction: seeded from", result.stdout)
            self.assertIn("v8ArchiveStatus: cached", result.stdout)
            self.assertEqual(cache_path.read_bytes(), archive.read_bytes())

    def test_dry_run_disambiguates_cli_payload_from_desktop_app(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-SourceExe",
                str(self.source_exe),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn(
                "targetKind: local CLI/TUI payload used by Codex Desktop; "
                "launching it directly opens a terminal.",
                result.stdout,
            )
            self.assertIn("desktopAppExecutable:", result.stdout)
            self.assertIn(
                "desktopAppLaunchCommand: explorer.exe "
                "shell:AppsFolder\\OpenAI.Codex_2p2nqsd0c76g0!App",
                result.stdout,
            )

    def test_dry_run_reports_desktop_local_cli_routing(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"
            local_codex_home = Path(temp_dir) / "codex-home"
            env = clean_env()
            path_key = next((key for key in env if key.lower() == "path"), "Path")
            env["CODEX_CLI_PATH"] = str(Path(temp_dir) / "old-codex.exe")
            env[path_key] = f"{install_dir};{env.get(path_key, '')}"

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-ConfigureDesktopLocalCli",
                "-DesktopCliEnvironmentTarget",
                "Process",
                "-SourceExe",
                str(self.source_exe),
                "-InstallDir",
                str(install_dir),
                "-LocalCodexHome",
                str(local_codex_home),
                env=env,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assert_proof_value(result.stdout, "desktopLocalCliRouting", "enabled")
            self.assert_proof_value(
                result.stdout, "localCodexHome", str(local_codex_home)
            )
            self.assert_proof_value(result.stdout, "localCodexHomeScope", "Process")
            self.assert_proof_value(
                result.stdout, "localCodexHomeAction", "would create"
            )
            self.assert_proof_value(
                result.stdout,
                "localCodexSqliteHome",
                str(local_codex_home / "sqlite"),
            )
            self.assert_proof_value(
                result.stdout, "localCodexSqliteHomeScope", "Process"
            )
            self.assert_proof_value(
                result.stdout, "localCodexSqliteHomeAction", "would create"
            )
            self.assert_proof_value(
                result.stdout, "desktopCliPathEnvName", "CODEX_CLI_PATH"
            )
            self.assert_proof_value(
                result.stdout,
                "desktopCliPathEnvTarget",
                str(install_dir / "codex.exe"),
            )
            self.assert_proof_value(result.stdout, "desktopCliPathEnvScope", "Process")
            self.assert_proof_value(
                result.stdout, "desktopCliPathEnvAction", "would set"
            )
            self.assert_proof_value(
                result.stdout,
                "officialEnvCleanup",
                "CODEX_HOME unset, CODEX_CLI_PATH unset, CODEX_SQLITE_HOME unset",
            )
            self.assert_proof_value(
                result.stdout,
                "desktopUserPathLocalBinAction",
                "would remove 1 entry",
            )

    def test_dry_run_supports_user_scope_desktop_local_cli_routing(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"
            local_codex_home = Path(temp_dir) / "codex-home"

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-ConfigureDesktopLocalCli",
                "-DesktopCliEnvironmentTarget",
                "User",
                "-SourceExe",
                str(self.source_exe),
                "-InstallDir",
                str(install_dir),
                "-LocalCodexHome",
                str(local_codex_home),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assert_proof_value(result.stdout, "localCodexHomeScope", "User")
            self.assert_proof_value(
                result.stdout,
                "localCodexSqliteHome",
                str(local_codex_home / "sqlite"),
            )
            self.assert_proof_value(result.stdout, "localCodexSqliteHomeScope", "User")
            self.assert_proof_value(result.stdout, "desktopCliPathEnvScope", "User")
            self.assert_proof_value(
                result.stdout, "desktopCliPathEnvAction", "would set"
            )
            self.assert_proof_value(
                result.stdout, "desktopEnvironmentBroadcast", "would send"
            )

    def test_default_dry_run_reports_desktop_localexe_target(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            user_profile = Path(temp_dir) / "profile"
            expected_home = user_profile / "Desktop" / "LOCAL-KD"
            expected_target = expected_home / "codex.exe"
            stale_codex_home = user_profile / ".codex-test-home"
            env = clean_env()
            env["USERPROFILE"] = str(user_profile)
            env["CODEX_HOME"] = str(stale_codex_home)

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-ConfigureDesktopLocalCli",
                "-SourceExe",
                str(self.source_exe),
                env=env,
            )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assert_proof_value(result.stdout, "targetPath", str(expected_target))
        self.assert_proof_value(result.stdout, "localCodexHome", str(expected_home))
        self.assert_proof_value(result.stdout, "localCodexHomeScope", "Process")
        self.assert_proof_value(result.stdout, "localCodexHomeAction", "would create")
        self.assert_proof_value(
            result.stdout,
            "localCodexSqliteHome",
            str(expected_home / "sqlite"),
        )
        self.assert_proof_value(result.stdout, "localCodexSqliteHomeScope", "Process")
        self.assert_proof_value(
            result.stdout, "localCodexSqliteHomeAction", "would create"
        )
        self.assert_proof_value(
            result.stdout,
            "desktopCliPathEnvTarget",
            str(expected_target),
        )
        self.assert_proof_value(
            result.stdout,
            "officialEnvCleanup",
            "CODEX_HOME unset, CODEX_CLI_PATH unset, CODEX_SQLITE_HOME unset",
        )
        self.assertNotEqual(
            self.proof_value(result.stdout, "localCodexHome"),
            str(stale_codex_home),
        )
        self.assertNotIn(
            "AppData\\Local\\OpenAI\\Codex\\bin\\codexKD-local\\codex.exe",
            result.stdout,
        )

    def test_dry_run_reports_source_build_stamp_details(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            fake_codex = temp_path / "fake-codex.cmd"
            fake_codex.write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        "echo codex 9.9.9",
                        "echo commit: abc123def456",
                        "echo dirty: true",
                        "echo profile: release",
                        "echo built: 123s since unix epoch",
                    ]
                ),
                encoding="utf-8",
            )

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("sourceCommit: abc123def456", result.stdout)
            self.assertIn("sourceDirty: true", result.stdout)
            self.assertIn("sourceProfile: release", result.stdout)
            self.assertIn("sourceBuilt: 123s since unix epoch", result.stdout)

    def test_dry_run_reports_stale_target_when_source_tree_is_newer(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"
            install_dir.mkdir()
            target = install_dir / "codex.exe"
            target.write_bytes(self.source_exe_bytes)
            old_timestamp = 946684800
            os.utime(target, (old_timestamp, old_timestamp))

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-SourceExe",
                str(self.source_exe),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("sourceTreeNewestWriteUtc:", result.stdout)
            self.assertIn("targetBeforeLastWriteUtc:", result.stdout)
            self.assertIn("targetBeforeStale: True", result.stdout)
            self.assertIn(
                "targetBeforeStaleRemedy: Run just publish-local-codex and restart Codex Desktop.",
                result.stdout,
            )

    def test_dry_run_reports_stale_source_build_when_skip_build_would_noop(
        self,
    ) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            stale_timestamp = 946684800
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=stale_timestamp,
            )
            target = install_dir / "codex.exe"
            target.write_bytes(fake_codex.read_bytes())
            os.utime(target, (stale_timestamp, stale_timestamp))

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("sourceBuildStale: True", result.stdout)
            self.assert_publish_readiness(result.stdout, "blocked: source build stale")
            self.assertIn(
                "sourceBuildStaleRemedy: Run just publish-local-codex -Profile release -RunDoctor without -SkipBuild, then restart Codex Desktop.",
                result.stdout,
            )
            self.assertIn("binaryChanged: false", result.stdout)
            self.assertIn("replace: not run: source build stale", result.stdout)
            self.assertIn("restartRequired: unknown until rebuild", result.stdout)
            self.assertNotIn("replace: not run: target already current", result.stdout)

    def test_runtime_proof_reports_doctor_skip_before_stale_source_failure(
        self,
    ) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            stale_timestamp = 946684800
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=stale_timestamp,
            )

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-RunDoctor",
                "-RuntimeProof",
                "-FailOnStaleSourceBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("runtimeProof: requested", result.stdout)
            self.assertIn("sourceBuildStale: True", result.stdout)
            self.assert_publish_readiness(result.stdout, "blocked: source build stale")
            self.assertIn(
                f'doctorCommand: "{install_dir / "codex.exe"}" doctor --json (not run: target missing)',
                result.stdout,
            )
            self.assertIn("doctorStatus: skipped: target missing", result.stdout)
            self.assertIn("Dry-run source build is stale", result.stderr)

    def test_apply_blocks_skip_build_when_source_binary_is_stale(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=946684800,
            )
            target = install_dir / "codex.exe"
            target.write_bytes(b"previous-codex")

            result = self.run_script(
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(target.read_bytes(), b"previous-codex")
            self.assertFalse((install_dir / "backups").exists())
            self.assertIn("sourceBuildStale: True", result.stdout)
            self.assertIn("replace: blocked: source build stale", result.stdout)
            self.assertIn("restartRequired: unknown until rebuild", result.stdout)
            self.assertIn(
                "SkipBuild cannot publish the newest Codex app payload",
                result.stderr,
            )

    def test_apply_skips_replacement_when_target_hash_matches_source(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            source_timestamp = FRESH_SOURCE_TIME
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=source_timestamp,
            )
            target = install_dir / "codex.exe"
            target.write_bytes(fake_codex.read_bytes())
            os.utime(target, (source_timestamp, source_timestamp))

            result = self.run_script(
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertEqual(target.read_bytes(), fake_codex.read_bytes())
            self.assertFalse((install_dir / "backups").exists())
            self.assert_proof_value(result.stdout, "sourceBuildStale", "False")
            self.assert_proof_value(result.stdout, "sourceSha256Mode", "hashed")
            self.assert_proof_value(result.stdout, "binaryChanged", "false")
            self.assert_proof_value(
                result.stdout,
                "replace",
                "skipped: target already current",
            )
            self.assert_proof_value(result.stdout, "restartRequired", "false")
            self.assert_no_publish_temps(install_dir)

    def test_same_size_mtime_different_content_requires_replacement(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            source_timestamp = FRESH_SOURCE_TIME
            fake_codex = temp_path / "fake-codex.cmd"
            fake_codex.write_text("@echo off\r\necho codex A\r\n", encoding="utf-8")
            os.utime(fake_codex, (source_timestamp, source_timestamp))
            target = install_dir / "codex.exe"
            target.write_text("@echo off\r\necho codex B\r\n", encoding="utf-8")
            os.utime(target, (source_timestamp, source_timestamp))

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertNotEqual(
                self.proof_value(result.stdout, "sourceSha256"),
                self.proof_value(result.stdout, "targetBeforeSha256"),
            )
            self.assert_proof_value(result.stdout, "sourceSha256Mode", "hashed")
            self.assert_proof_value(result.stdout, "binaryChanged", "true")
            self.assert_proof_value(result.stdout, "replace", "not run")
            self.assert_proof_value(result.stdout, "restartRequired", "true")

    def test_auto_skip_build_uses_live_source_scan_before_stamp(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            source_timestamp = FIXTURE_TIME + 100
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=source_timestamp,
            )
            self.write_build_stamp("release", source_timestamp)
            self.touch_tracked_source(source_timestamp + 10)

            result = self.run_script(
                "-DryRun",
                "-AutoSkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("autoSkipBuild: false", result.stdout)
            self.assertIn(
                "autoSkipBuildReason: tracked source is newer than source build",
                result.stdout,
            )
            self.assertIn("buildCommand: cargo --config", result.stdout)
            self.assertIn(" build --target-dir ", result.stdout)
            self.assertIn("--profile release", result.stdout)
            self.assertIn("(not run)", result.stdout)
            self.assertNotIn("buildCommand: <skipped>", result.stdout)

    def test_auto_skip_build_ignores_unrelated_source_changes(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            source_timestamp = FIXTURE_TIME + 100
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=source_timestamp + 20,
            )
            self.write_build_stamp("release", source_timestamp)
            self.touch_unrelated_source(source_timestamp + 10)

            result = self.run_script(
                "-DryRun",
                "-AutoSkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("autoSkipBuild: true", result.stdout)
            self.assertIn(
                "autoSkipBuildReason: source build is current for tracked publish inputs",
                result.stdout,
            )
            self.assertIn("buildCommand: <skipped>", result.stdout)
            self.assertIn("sourceBuildStale: False", result.stdout)

    def test_auto_skip_build_does_not_skip_when_freshness_unknown(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=FRESH_SOURCE_TIME,
            )

            result = self.run_script(
                "-DryRun",
                "-AutoSkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("autoSkipBuild: false", result.stdout)
            self.assertIn(
                "autoSkipBuildReason: tracked source freshness unknown",
                result.stdout,
            )
            self.assertIn("buildCommand: cargo --config", result.stdout)
            self.assertIn(" build --target-dir ", result.stdout)
            self.assertIn("--profile release", result.stdout)
            self.assertIn("(not run)", result.stdout)
            self.assertNotIn("buildCommand: <skipped>", result.stdout)

    def test_print_built_codex_path_uses_profile_output_dir(self) -> None:
        result = self.run_script("-PrintBuiltCodexPath")
        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertEqual(
            Path(result.stdout.strip()),
            self.repo_root
            / "codex-rs"
            / "target"
            / "publish-release"
            / "release"
            / "codex.exe",
        )

        debug_result = self.run_script("-PrintBuiltCodexPath", "-Profile", "debug")
        self.assertEqual(
            debug_result.returncode,
            0,
            f"stdout:\n{debug_result.stdout}\nstderr:\n{debug_result.stderr}",
        )
        self.assertEqual(
            Path(debug_result.stdout.strip()),
            self.repo_root
            / "codex-rs"
            / "target"
            / "publish-debug"
            / "debug"
            / "codex.exe",
        )

        local_release_result = self.run_script(
            "-PrintBuiltCodexPath", "-Profile", "local-release"
        )
        self.assertEqual(
            local_release_result.returncode,
            0,
            f"stdout:\n{local_release_result.stdout}\nstderr:\n{local_release_result.stderr}",
        )
        self.assertEqual(
            Path(local_release_result.stdout.strip()),
            self.repo_root
            / "codex-rs"
            / "target"
            / "publish-local-release"
            / "local-release"
            / "codex.exe",
        )

    def test_dry_run_debug_profile_uses_cargo_dev_profile(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=FRESH_SOURCE_TIME,
            )

            result = self.run_script(
                "-DryRun",
                "-Profile",
                "debug",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("buildCommand: cargo build --target-dir", result.stdout)
            self.assertIn("--profile dev", result.stdout)
            self.assertNotIn("preflightCheckCommand:", result.stdout)

    def test_dry_run_release_reports_preflight_unless_skipped(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=FRESH_SOURCE_TIME,
            )

            result = self.run_script(
                "-DryRun",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("preflightCheckCommand: cargo --config", result.stdout)
            self.assertIn(" check --target-dir ", result.stdout)
            self.assertIn("buildCommand: cargo --config", result.stdout)
            self.assertIn(" build --target-dir ", result.stdout)
            self.assertIn("(not run)", result.stdout)

            skipped = self.run_script(
                "-DryRun",
                "-SkipPreflightCheck",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                skipped.returncode,
                0,
                f"stdout:\n{skipped.stdout}\nstderr:\n{skipped.stderr}",
            )
            self.assertNotIn("preflightCheckCommand:", skipped.stdout)
            self.assertIn("buildCommand: cargo --config", skipped.stdout)
            self.assertIn(" build --target-dir ", skipped.stdout)

    def test_source_hash_bypasses_cache_when_size_mtime_match(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            source_timestamp = FIXTURE_TIME + 300
            fake_codex = temp_path / "fake-codex.cmd"
            fake_codex.write_text("@echo off\r\necho codex A\r\n", encoding="utf-8")
            os.utime(fake_codex, (source_timestamp, source_timestamp))
            cache_path = self.hash_cache_path(fake_codex)
            cache_path.parent.mkdir(parents=True, exist_ok=True)
            cached_hash = "0" * 64
            cache_path.write_text(
                json.dumps(
                    {
                        "path": str(fake_codex.resolve()),
                        "length": fake_codex.stat().st_size,
                        "lastWriteUtc": datetime.fromtimestamp(
                            source_timestamp, timezone.utc
                        )
                        .isoformat()
                        .replace("+00:00", "Z"),
                        "sha256": cached_hash,
                    }
                ),
                encoding="utf-8",
            )
            fake_codex.write_text("@echo off\r\necho codex B\r\n", encoding="utf-8")
            os.utime(fake_codex, (source_timestamp, source_timestamp))
            expected = hashlib.sha256(fake_codex.read_bytes()).hexdigest()

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn(f"sourceSha256: {expected}", result.stdout)
            self.assertNotIn(f"sourceSha256: {cached_hash}", result.stdout)

    def test_target_hash_cache_cannot_hide_changed_target_content(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            timestamp = FIXTURE_TIME + 325
            fake_codex = temp_path / "fake-codex.cmd"
            fake_codex.write_text("@echo off\r\necho codex A\r\n", encoding="utf-8")
            os.utime(fake_codex, (timestamp, timestamp))
            target = install_dir / "codex.exe"
            target.write_text("@echo off\r\necho codex B\r\n", encoding="utf-8")
            os.utime(target, (timestamp, timestamp))
            stale_cached_target_hash = hashlib.sha256(
                fake_codex.read_bytes()
            ).hexdigest()
            cache_path = self.hash_cache_path(target)
            cache_path.parent.mkdir(parents=True, exist_ok=True)
            cache_path.write_text(
                json.dumps(
                    {
                        "path": str(target.resolve()),
                        "length": target.stat().st_size,
                        "lastWriteUtc": datetime.fromtimestamp(timestamp, timezone.utc)
                        .isoformat()
                        .replace("+00:00", "Z"),
                        "sha256": stale_cached_target_hash,
                    }
                ),
                encoding="utf-8",
            )

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertNotEqual(
                self.proof_value(result.stdout, "sourceSha256"),
                self.proof_value(result.stdout, "targetBeforeSha256"),
            )
            self.assertNotIn(
                f"targetBeforeSha256: {stale_cached_target_hash}", result.stdout
            )
            self.assert_proof_value(result.stdout, "binaryChanged", "true")

    def test_hash_cache_invalidates_when_mtime_changes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            original_timestamp = FIXTURE_TIME + 350
            updated_timestamp = FIXTURE_TIME + 360
            fake_codex = temp_path / "fake-codex.cmd"
            fake_codex.write_text("@echo off\r\necho codex old\r\n", encoding="utf-8")
            os.utime(fake_codex, (original_timestamp, original_timestamp))
            cache_path = self.hash_cache_path(fake_codex)
            cache_path.parent.mkdir(parents=True, exist_ok=True)
            cache_path.write_text(
                json.dumps(
                    {
                        "path": str(fake_codex.resolve()),
                        "length": fake_codex.stat().st_size,
                        "lastWriteUtc": datetime.fromtimestamp(
                            original_timestamp, timezone.utc
                        )
                        .isoformat()
                        .replace("+00:00", "Z"),
                        "sha256": "0" * 64,
                    }
                ),
                encoding="utf-8",
            )
            fake_codex.write_text("@echo off\r\necho codex new\r\n", encoding="utf-8")
            os.utime(fake_codex, (updated_timestamp, updated_timestamp))
            expected = hashlib.sha256(fake_codex.read_bytes()).hexdigest()

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn(f"sourceSha256: {expected}", result.stdout)

    def test_hash_cache_ignores_corrupted_json(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=FIXTURE_TIME + 400,
            )
            cache_path = self.hash_cache_path(fake_codex)
            cache_path.parent.mkdir(parents=True, exist_ok=True)
            cache_path.write_text("{not-json", encoding="utf-8")
            expected = hashlib.sha256(fake_codex.read_bytes()).hexdigest()

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn(f"sourceSha256: {expected}", result.stdout)

    def test_fast_proof_omits_desktop_appx_probe_for_noop(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            source_timestamp = FRESH_SOURCE_TIME
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=source_timestamp,
            )
            target = install_dir / "codex.exe"
            target.write_bytes(fake_codex.read_bytes())
            os.utime(target, (source_timestamp, source_timestamp))

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-FastProof",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("binaryChanged: false", result.stdout)
            self.assertIn(
                "desktopAppPackage: <skipped: fast proof no-op>", result.stdout
            )
            self.assertIn(
                "desktopAppExecutable: <skipped: fast proof no-op>", result.stdout
            )

    def test_noop_run_doctor_skips_doctor_by_default(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            source_timestamp = FRESH_SOURCE_TIME
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=source_timestamp,
            )
            target = install_dir / "codex.exe"
            target.write_bytes(fake_codex.read_bytes())
            os.utime(target, (source_timestamp, source_timestamp))

            result = self.run_script(
                "-SkipBuild",
                "-RunDoctor",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("replace: skipped: target already current", result.stdout)
            self.assertIn(
                "doctorCommand: <skipped: target already current>", result.stdout
            )
            self.assertNotIn("doctor --json", result.stdout)

    def test_actual_release_build_skips_preflight_and_uses_target_dir_argument(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            fake_codex = self.copy_valid_codex(
                temp_path / "fake-codex.exe",
                timestamp=FIXTURE_TIME + 450,
                append_padding=True,
            )
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            fake_cargo = fake_bin / "cargo.cmd"
            fake_cargo.write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        "echo fake cargo %*",
                        "echo cargoTargetDirEnv=%CARGO_TARGET_DIR%",
                        "exit /b 0",
                    ]
                ),
                encoding="utf-8",
            )
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            env["CARGO_TARGET_DIR"] = str(temp_path / "inherited-target")

            result = self.run_script(
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
                env=env,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("fake cargo --config ", result.stdout)
            self.assertIn(" build --target-dir ", result.stdout)
            self.assertIn("target\\publish-release", result.stdout)
            self.assertNotRegex(result.stdout, r"fake cargo .* check ")
            self.assertIn("cargoTargetDirEnv=", result.stdout)
            self.assertNotIn("inherited-target", result.stdout)
            self.assert_no_publish_temps(install_dir)

    def test_build_only_returns_after_build_stamp_and_proof(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            fake_cargo = fake_bin / "cargo.cmd"
            fake_cargo.write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        "echo fake cargo %*",
                        "exit /b 0",
                    ]
                ),
                encoding="utf-8",
            )
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"

            result = self.run_script(
                "-BuildOnly",
                "-RunDoctor",
                "-ConfigureDesktopLocalCli",
                "-RestartDesktop",
                "-InstallDir",
                str(install_dir),
                env=env,
            )

            stamp = (
                self.repo_root
                / "codex-rs"
                / "target"
                / "codex-local-publish-release.stamp"
            )
            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertTrue(stamp.exists())
            self.assertIn("action: build-only", result.stdout)
            self.assertIn("buildOnly: true", result.stdout)
            self.assertIn("builtCodexPath:", result.stdout)
            self.assertIn("buildStampPath:", result.stdout)
            self.assertIn("fake cargo --config ", result.stdout)
            self.assertIn(" build --target-dir ", result.stdout)
            self.assertNotIn("sourceSha256:", result.stdout)
            self.assertNotIn("targetPath:", result.stdout)
            self.assertNotIn("publishLock:", result.stdout)
            self.assertNotIn("desktopLocalCliRouting:", result.stdout)
            self.assertNotIn("doctorCommand:", result.stdout)
            self.assertFalse((install_dir / "codex.exe").exists())

    def test_test_run_executes_build_and_doctor_without_publishing(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            fake_codex = temp_path / "fake-codex.cmd"
            fake_codex.write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        'if "%1"=="doctor" (',
                        'echo {"checks":{"auth.credentials":{"status":"fail"},"local_publish.readiness":{"status":"warning","summary":"doctor is not running from the local publish target"},"desktop.runtime_chain":{"status":"ok","summary":"desktop runtime chain evidence collected"},"app_server.status":{"status":"ok","summary":"background server reachable"},"network.websocket_reachability":{"status":"warning"}}}',
                        "exit /b 1",
                        ")",
                        "echo codex 9.9.9",
                        "echo commit: test-commit",
                        "exit /b 0",
                    ]
                ),
                encoding="utf-8",
            )
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            fake_cargo = fake_bin / "cargo.cmd"
            fake_cargo.write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        "echo fake cargo %*",
                        "exit /b 0",
                    ]
                ),
                encoding="utf-8",
            )
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"

            result = self.run_script(
                "-TestRun",
                "-AutoSkipBuild",
                "-RunDoctor",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
                env=env,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("action: test-run", result.stdout)
            self.assertIn("testRun: true", result.stdout)
            self.assertIn("autoSkipBuild: true", result.stdout)
            self.assertIn(
                "autoSkipBuildReason: source build is current for tracked publish inputs",
                result.stdout,
            )
            self.assertIn("buildCommand: <skipped>", result.stdout)
            self.assertNotIn("fake cargo --config ", result.stdout)
            self.assertIn(f'doctorCommand: "{fake_codex}" doctor --json', result.stdout)
            self.assertIn(
                "doctorStatus: warning: auth.credentials missing", result.stdout
            )
            self.assertIn("doctorLocalPublishStatus: warning", result.stdout)
            self.assertIn(
                "doctorLocalPublishSummary: doctor is not running from the local publish target",
                result.stdout,
            )
            self.assertIn("doctorDesktopRuntimeStatus: ok", result.stdout)
            self.assertIn(
                "doctorDesktopRuntimeSummary: desktop runtime chain evidence collected",
                result.stdout,
            )
            self.assertIn("doctorAppServerStatus: ok", result.stdout)
            self.assertIn(
                "doctorAppServerSummary: background server reachable", result.stdout
            )
            self.assertIn("replace: not run: test run", result.stdout)
            self.assertIn("restartRequired: false", result.stdout)
            self.assertNotIn("targetPath:", result.stdout)
            self.assertNotIn("publishLock:", result.stdout)
            self.assertNotIn("desktopLocalCliRouting:", result.stdout)
            self.assertFalse((install_dir / "codex.exe").exists())

    def test_no_sccache_switch_disables_rustc_wrapper(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            fake_codex = self.copy_valid_codex(
                temp_path / "fake-codex.exe",
                timestamp=FIXTURE_TIME + 450,
                append_padding=True,
            )
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            fake_cargo = fake_bin / "cargo.cmd"
            fake_cargo.write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        "echo fake cargo %*",
                        'if "%1"=="--config" type "%2"',
                        "echo rustcWrapperEnv=%RUSTC_WRAPPER%",
                        "echo cargoBuildRustcWrapperEnv=%CARGO_BUILD_RUSTC_WRAPPER%",
                        "exit /b 0",
                    ]
                ),
                encoding="utf-8",
            )
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            env["RUSTC_WRAPPER"] = "sccache"
            env["CARGO_BUILD_RUSTC_WRAPPER"] = "sccache"

            result = self.run_script(
                "-NoSccache",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
                env=env,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("rustcWrapper: <none: disabled by -NoSccache>", result.stdout)
            self.assertIn(
                "cargoRustcWrapperConfig: <none: disabled by -NoSccache>",
                result.stdout,
            )
            self.assertIn("rustcWrapperEnv=", result.stdout)
            self.assertIn("cargoBuildRustcWrapperEnv=", result.stdout)
            self.assertNotIn("rustcWrapperEnv=sccache", result.stdout)
            self.assertNotIn("cargoBuildRustcWrapperEnv=sccache", result.stdout)
            self.assertIn("fake cargo --config ", result.stdout)
            self.assertIn(" build --target-dir ", result.stdout)
            self.assertIn("[build]", result.stdout)
            self.assertIn('rustc-wrapper = ""', result.stdout)
            self.assert_no_publish_temps(install_dir)

    def test_publish_build_sets_version_metadata_env(self) -> None:
        self.init_repo_fixture()
        expected_commit = self.run_git("rev-parse", "--short=12", "HEAD").stdout.strip()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            fake_codex = self.copy_valid_codex(
                temp_path / "fake-codex.exe",
                timestamp=FIXTURE_TIME + 450,
                append_padding=True,
            )
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            fake_cargo = fake_bin / "cargo.cmd"
            fake_cargo.write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        "echo fake cargo %*",
                        "echo metadata commit=%CODEX_BUILD_COMMIT%",
                        "echo metadata dirty=%CODEX_BUILD_DIRTY%",
                        "echo metadata profile=%CODEX_BUILD_PROFILE%",
                        "echo metadata timestamp=%CODEX_BUILD_TIMESTAMP%",
                        "exit /b 0",
                    ]
                ),
                encoding="utf-8",
            )
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"

            result = self.run_script(
                "-NoSccache",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
                env=env,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn(f"buildMetadataCommit: {expected_commit}", result.stdout)
            self.assertIn("buildMetadataDirty: false", result.stdout)
            self.assertIn("buildMetadataProfile: release", result.stdout)
            self.assertIn(f"metadata commit={expected_commit}", result.stdout)
            self.assertIn("metadata dirty=false", result.stdout)
            self.assertIn("metadata profile=release", result.stdout)
            self.assertNotIn("metadata timestamp=unknown", result.stdout)
            self.assert_no_publish_temps(install_dir)

    def test_apply_replaces_target_and_writes_backup(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            fake_codex = self.copy_valid_codex(
                temp_path / "fake-codex.exe",
                timestamp=FRESH_SOURCE_TIME,
                append_padding=True,
            )
            target = install_dir / "codex.exe"
            target.write_bytes(b"previous-codex")

            result = self.run_script(
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertEqual(target.read_bytes(), fake_codex.read_bytes())
            backups = sorted((install_dir / "backups").glob("codex-*.exe"))
            self.assertEqual(len(backups), 1)
            self.assertEqual(backups[0].read_bytes(), b"previous-codex")
            previous_sha256 = hashlib.sha256(b"previous-codex").hexdigest()
            self.assertIn("targetSha256:", result.stdout)
            self.assertIn(f"backupSha256: {previous_sha256}", result.stdout)
            self.assertIn("backupPath:", result.stdout)
            self.assertIn("postPublishVerify: version ok", result.stdout)
            self.assertRegex(
                result.stdout,
                r"targetBeforeVersion: <unavailable: [^\r\n]+>[\r\n]",
            )
            self.assert_no_publish_temps(install_dir)

    def test_apply_prunes_old_publish_backups(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            backup_dir = install_dir / "backups"
            backup_dir.mkdir(parents=True)
            for index in range(12):
                backup = backup_dir / f"codex-20000101T0000{index:02d}000Z.exe"
                backup.write_bytes(f"backup-{index}".encode("utf-8"))
                os.utime(backup, (946684800 + index, 946684800 + index))

            fake_codex = self.copy_valid_codex(
                temp_path / "fake-codex.exe",
                timestamp=FRESH_SOURCE_TIME,
                append_padding=True,
            )
            target = install_dir / "codex.exe"
            target.write_bytes(self.source_exe_bytes)

            result = self.run_script(
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            backups = sorted(backup_dir.glob("codex-*.exe"))
            self.assertLessEqual(len(backups), 10)
            self.assertIn("backupPruned:", result.stdout)
            self.assert_no_publish_temps(install_dir)

    def test_apply_rolls_back_when_published_binary_fails_version_check(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=FRESH_SOURCE_TIME,
            )
            previous = b"previous-codex"
            target = install_dir / "codex.exe"
            target.write_bytes(previous)

            result = self.run_script(
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(target.read_bytes(), previous)
            backups = sorted((install_dir / "backups").glob("codex-*.exe"))
            self.assertEqual(len(backups), 1)
            self.assertEqual(backups[0].read_bytes(), previous)
            self.assertIn("rollback: requested:", result.stdout)
            self.assertIn("rollbackResult: restored backup", result.stdout)
            self.assertIn(
                "Published Codex binary failed --version verification",
                result.stderr,
            )
            self.assert_no_publish_temps(install_dir)

    def test_failed_publish_can_rollback_when_backup_dir_is_over_limit(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            backup_dir = install_dir / "backups"
            backup_dir.mkdir(parents=True)
            for index in range(12):
                backup = backup_dir / f"codex-20990101T0000{index:02d}000Z.exe"
                backup.write_bytes(f"backup-{index}".encode("utf-8"))
                os.utime(backup, (4102444800 + index, 4102444800 + index))

            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=FRESH_SOURCE_TIME,
            )
            previous = self.source_exe_bytes
            target = install_dir / "codex.exe"
            target.write_bytes(previous)
            old_target_timestamp = 946684800
            os.utime(target, (old_target_timestamp, old_target_timestamp))

            result = self.run_script(
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(target.read_bytes(), previous)
            backups = sorted(backup_dir.glob("codex-*.exe"))
            self.assertTrue(
                any(backup.read_bytes() == previous for backup in backups),
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("rollback: requested:", result.stdout)
            self.assertIn("rollbackResult: restored backup", result.stdout)
            self.assertNotIn("backupPruned:", result.stdout)
            self.assert_no_publish_temps(install_dir)

    def test_apply_rolls_back_new_target_when_verification_fails(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=FIXTURE_TIME + 500,
            )
            target = install_dir / "codex.exe"

            result = self.run_script(
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(target.exists())
            self.assertIn("backupPath: <none: target missing>", result.stdout)
            self.assertIn("rollback: requested:", result.stdout)
            self.assertIn(
                "rollbackResult: removed newly published target", result.stdout
            )
            self.assert_no_publish_temps(install_dir)

    def test_apply_closes_running_target_before_replacing(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            target = install_dir / "codex.exe"
            target.write_bytes(self.source_exe_bytes)
            fake_codex = self.copy_valid_codex(
                temp_path / "fake-codex.exe",
                timestamp=FRESH_SOURCE_TIME,
                append_padding=True,
            )

            process = subprocess.Popen(
                [str(target), "/c", "ping -n 30 127.0.0.1 > nul"],
                creationflags=CREATE_NO_WINDOW,
            )
            try:
                result = self.run_script(
                    "-SkipBuild",
                    "-SourceExe",
                    str(fake_codex),
                    "-InstallDir",
                    str(install_dir),
                    "-CloseRunningTargetTimeoutSeconds",
                    "1",
                )

                self.assertEqual(
                    result.returncode,
                    0,
                    f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
                )
                self.assertIn("runningTargetProcesses: pid=", result.stdout)
                self.assertIn("closeRunningTarget: requested:", result.stdout)
                self.assertIn("closeRunningTargetResult: closed", result.stdout)
                self.assertIn("runningTargetProcessesAfterClose: <none>", result.stdout)
                self.assertIsNotNone(process.poll())
                self.assertEqual(
                    target.read_bytes(),
                    fake_codex.read_bytes(),
                )
                self.assert_no_publish_temps(install_dir)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)

    def test_apply_allow_running_target_skips_close_and_replaces(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            target = install_dir / "codex.exe"
            target.write_bytes(self.source_exe_bytes)
            fake_codex = self.copy_valid_codex(
                temp_path / "fake-codex.exe",
                timestamp=FIXTURE_TIME + 600,
                append_padding=True,
            )

            process = subprocess.Popen(
                [str(target), "/c", "ping -n 30 127.0.0.1 > nul"],
                creationflags=CREATE_NO_WINDOW,
            )
            try:
                result = self.run_script(
                    "-SkipBuild",
                    "-AllowRunningTarget",
                    "-SourceExe",
                    str(fake_codex),
                    "-InstallDir",
                    str(install_dir),
                )

                self.assertEqual(
                    result.returncode,
                    0,
                    f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
                )
                self.assertIn("runningTargetProcesses: pid=", result.stdout)
                self.assertIn(
                    "closeRunningTarget: skipped: -AllowRunningTarget",
                    result.stdout,
                )
                self.assertIsNone(process.poll())
                self.assertEqual(target.read_bytes(), fake_codex.read_bytes())
                self.assert_no_publish_temps(install_dir)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)


if __name__ == "__main__":
    unittest.main()
