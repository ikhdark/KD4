#!/usr/bin/env python3

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


class PublishLocalCodexApplyTest(PublishLocalCodexTestBase):
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
            code_mode_host_target = install_dir / "codex-code-mode-host.exe"
            previous_code_mode_host = b"previous-code-mode-host"
            code_mode_host_target.write_bytes(previous_code_mode_host)

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
            backups = sorted((install_dir / "backups").glob("codex-2*.exe"))
            self.assertEqual(len(backups), 1)
            self.assertEqual(backups[0].read_bytes(), b"previous-codex")
            code_mode_host_backups = sorted(
                (install_dir / "backups").glob("codex-code-mode-host-*.exe")
            )
            self.assertEqual(len(code_mode_host_backups), 1)
            self.assertEqual(
                code_mode_host_backups[0].read_bytes(), previous_code_mode_host
            )
            previous_sha256 = hashlib.sha256(b"previous-codex").hexdigest()
            previous_code_mode_host_sha256 = hashlib.sha256(
                previous_code_mode_host
            ).hexdigest()
            self.assertIn("targetSha256:", result.stdout)
            self.assertIn(f"backupSha256: {previous_sha256}", result.stdout)
            self.assertIn("codeModeHostTargetSha256:", result.stdout)
            self.assertIn(
                f"codeModeHostBackupSha256: {previous_code_mode_host_sha256}",
                result.stdout,
            )
            self.assertIn("backupPath:", result.stdout)
            self.assertIn("codeModeHostBackupPath:", result.stdout)
            self.assertIn("postPublishVerify: version ok", result.stdout)
            self.assertIn("codexPostPublishVerify: sha256 ok", result.stdout)
            self.assertIn("codeModeHostPostPublishVerify: sha256 ok", result.stdout)
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

    def test_host_only_publish_does_not_reserve_nonexistent_codex_backup(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            backup_dir = install_dir / "backups"
            backup_dir.mkdir(parents=True)
            for index in range(10):
                (backup_dir / f"codex-20000101T0000{index:02d}000Z.exe").write_bytes(
                    f"backup-{index}".encode("utf-8")
                )

            fake_codex = self.copy_valid_codex(
                temp_path / "fake-codex.exe",
                timestamp=FRESH_SOURCE_TIME,
            )
            target = install_dir / "codex.exe"
            target.write_bytes(fake_codex.read_bytes())
            os.utime(target, (FRESH_SOURCE_TIME, FRESH_SOURCE_TIME))
            (install_dir / "codex-code-mode-host.exe").write_bytes(b"old-host")

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
            self.assertEqual(len(list(backup_dir.glob("codex-2*.exe"))), 10)
            self.assert_proof_value(result.stdout, "codexBinaryChanged", "false")
            self.assert_proof_value(result.stdout, "codeModeHostBinaryChanged", "true")

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
            previous_code_mode_host = b"previous-code-mode-host"
            code_mode_host_target = install_dir / "codex-code-mode-host.exe"
            code_mode_host_target.write_bytes(previous_code_mode_host)

            result = self.run_script(
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(target.read_bytes(), previous)
            self.assertEqual(
                code_mode_host_target.read_bytes(), previous_code_mode_host
            )
            backups = sorted((install_dir / "backups").glob("codex-2*.exe"))
            self.assertEqual(len(backups), 1)
            self.assertEqual(backups[0].read_bytes(), previous)
            code_mode_host_backups = sorted(
                (install_dir / "backups").glob("codex-code-mode-host-*.exe")
            )
            self.assertEqual(len(code_mode_host_backups), 1)
            self.assertEqual(
                code_mode_host_backups[0].read_bytes(), previous_code_mode_host
            )
            self.assertIn("rollback: requested:", result.stdout)
            self.assertIn("rollbackResult: restored backup", result.stdout)
            self.assertIn("codeModeHostRollbackResult: restored backup", result.stdout)
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
            self.assertFalse((install_dir / "codex-code-mode-host.exe").exists())
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
            self.assertFalse((install_dir / "codex-code-mode-host.exe").exists())
            self.assertIn("backupPath: <none: target missing>", result.stdout)
            self.assertIn("rollback: requested:", result.stdout)
            self.assertIn(
                "rollbackResult: removed newly published target", result.stdout
            )
            self.assertIn(
                "codeModeHostRollbackResult: removed newly published target",
                result.stdout,
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

    def test_apply_closes_running_code_mode_host_before_replacing(self) -> None:
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
            code_mode_host_target.write_bytes(self.source_exe_bytes)

            process = subprocess.Popen(
                [str(code_mode_host_target), "/c", "ping -n 30 127.0.0.1 > nul"],
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
                self.assertEqual(target.read_bytes(), fake_codex.read_bytes())
                self.assertEqual(
                    code_mode_host_target.read_bytes(),
                    self.source_code_mode_host_bytes,
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
