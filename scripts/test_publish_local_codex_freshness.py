#!/usr/bin/env python3

from datetime import datetime, timezone
import json
from pathlib import Path
import hashlib
import os
import shutil
import subprocess
import tempfile
import unittest

from scripts.publish_local_codex_test_support import PublishLocalCodexTestBase


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


class PublishLocalCodexFreshnessTest(PublishLocalCodexTestBase):
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
            self.install_matching_code_mode_host(install_dir)

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

    def test_apply_repairs_missing_code_mode_host_without_replacing_codex(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            fake_codex = self.copy_valid_codex(
                temp_path / "fake-codex.exe",
                timestamp=FRESH_SOURCE_TIME,
            )
            target = install_dir / "codex.exe"
            target.write_bytes(fake_codex.read_bytes())
            os.utime(target, (FRESH_SOURCE_TIME, FRESH_SOURCE_TIME))
            code_mode_host_target = install_dir / "codex-code-mode-host.exe"

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
            self.assertEqual(
                code_mode_host_target.read_bytes(), self.source_code_mode_host_bytes
            )
            self.assert_proof_value(result.stdout, "codexBinaryChanged", "false")
            self.assert_proof_value(result.stdout, "codeModeHostBinaryChanged", "true")
            self.assert_proof_value(result.stdout, "binaryChanged", "true")
            self.assert_proof_value(
                result.stdout, "backupSha256", "<none: target already current>"
            )
            self.assert_proof_value(
                result.stdout,
                "codeModeHostBackupSha256",
                "<none: target missing>",
            )
            self.assert_proof_value(
                result.stdout, "codeModeHostPostPublishVerify", "sha256 ok"
            )
            self.assert_proof_value(result.stdout, "restartRequired", "true")
            self.assertFalse((install_dir / "backups").exists())
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

    def test_auto_skip_build_requires_code_mode_host_artifact(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=FRESH_SOURCE_TIME,
            )
            missing_host = temp_path / "missing-code-mode-host.exe"

            result = self.run_script(
                "-DryRun",
                "-AutoSkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-SourceCodeModeHostExe",
                str(missing_host),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("autoSkipBuild: false", result.stdout)
            self.assertIn("autoSkipBuildReason: source artifact missing", result.stdout)
            self.assertIn("buildCommand: cargo --config", result.stdout)
            self.assertIn("-p codex-cli -p codex-code-mode-host", result.stdout)

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
            self.install_matching_code_mode_host(install_dir)

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
            self.install_matching_code_mode_host(install_dir)

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


if __name__ == "__main__":
    unittest.main()
