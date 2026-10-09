#!/usr/bin/env python3

from pathlib import Path
import hashlib
import json
import os
import shutil
import subprocess
import tempfile
import unittest
from unittest import mock

from scripts import publish_local_codex_test_support as support
from scripts.publish_local_codex_test_support import PublishLocalCodexTestBase
from scripts.publish_local_codex_test_support import clean_env
from scripts.publish_local_codex_test_support import ps_single_quote


SCRIPT = Path(__file__).resolve().parent / "publish-local-codex.ps1"
RUN_TIMEOUT_SECONDS = 120
FIXTURE_TIME = 946684900
FRESH_SOURCE_TIME = FIXTURE_TIME + 10_000


class PublishLocalCodexApplyTest(PublishLocalCodexTestBase):
    def test_recovery_rejects_unowned_paths_without_changing_files(self) -> None:
        # A durable journal is input, not authority to delete arbitrary paths.
        # Both rollback and committed cleanup own only the install directory
        # named by the journal and its transaction-specific sibling directories.
        for phase in ("Applying", "Committed"):
            for field in ("InstallDir", "StageRoot", "RollbackRoot"):
                with self.subTest(phase=phase, field=field), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    install = root / "install"
                    stage = root / ".install.bundle.fixture"
                    rollback = root / ".install.rollback.fixture"
                    unrelated = root / "unrelated"
                    for path in (install, stage, rollback, unrelated):
                        path.mkdir()
                        (path / "keep.txt").write_bytes(path.name.encode())
                    journal = root / ".install.codex-local-publish.transaction.json"
                    metadata = {
                        "SchemaVersion": 1, "TransactionId": "fixture", "Phase": phase,
                        "InstallDir": str(install), "StageRoot": str(stage),
                        "RollbackRoot": str(rollback), "HadPreviousInstall": True,
                        "JournalPath": str(journal), "Entries": [],
                    }
                    metadata[field] = str(unrelated)
                    journal.write_text(json.dumps(metadata), encoding="utf-8")
                    before = {str(path.relative_to(root)): path.read_bytes()
                              for path in root.rglob("*") if path.is_file()}
                    result = subprocess.run(
                        [self.shell, "-NoProfile", "-Command",
                         f". {ps_single_quote(SCRIPT)} -ImportOnly; "
                         f"Recover-CodexRuntimeBundleTransaction -JournalPath {ps_single_quote(journal)}"],
                        env=clean_env(), capture_output=True, text=True,
                        check=False, timeout=RUN_TIMEOUT_SECONDS,
                    )
                    self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertIn("Cannot recover runtime bundle transaction", result.stderr)
                    self.assertEqual(
                        {str(path.relative_to(root)): path.read_bytes()
                         for path in root.rglob("*") if path.is_file()}, before,
                    )

    def test_unrelated_executable_is_rejected_before_install(self) -> None:
        install_dir = self.repo_root / "install"
        install_dir.mkdir()
        target = install_dir / "codex.exe"
        target.write_bytes(b"existing target")
        unrelated = self.repo_root / "unrelated.exe"
        shutil.copy2(os.environ["COMSPEC"], unrelated)
        result = self.run_script(
            "-SkipBuild", "-SourceExe", str(unrelated), "-InstallDir", str(install_dir)
        )
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(target.read_bytes(), b"existing target")
        self.assertEqual(list(install_dir.iterdir()), [target])
        self.assertFalse((install_dir.parent / "publisher-backups").exists())
        self.assertNotIn("publishLock:", result.stdout)
        self.assertNotIn("publishCommitted: true", result.stdout)
        self.assert_no_publish_temps(install_dir)

    def test_changed_publish_restart_failure_keeps_commit_and_returns_failure(
        self,
    ) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        start = source.index("function Restart-CodexDesktop {")
        end = source.index("\nfunction ", start + 1)
        isolated_script = self.repo_root / "publish-local-codex.ps1"
        # Only the external Desktop action is substituted. Run the real
        # transaction, commit, cleanup and terminal error handling unchanged.
        isolated_script.write_text(
            source[:start]
            + "function Restart-CodexDesktop { throw 'fixture restart failure' }\n"
            + source[end:],
            encoding="utf-8",
        )
        shutil.copy2(SCRIPT.with_name("common-rust-env.ps1"), self.repo_root)
        install_dir = self.repo_root / "install"
        install_dir.mkdir()
        target = install_dir / "codex.exe"
        target.write_bytes(b"old installed codex")
        with mock.patch.object(support, "SCRIPT", isolated_script):
            result = self.run_script(
                "-SkipBuild",
                "-SourceExe",
                str(self.source_exe),
                "-InstallDir",
                str(install_dir),
                "-RestartDesktop",
            )
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("publishCommitted: true", result.stdout)
        self.assertIn("restartFailed: true", result.stdout)
        self.assertIn("fixture restart failure", result.stderr)
        self.assertNotIn("rollback: requested", result.stdout)
        self.assertEqual(target.read_bytes(), self.source_exe_bytes)
        backups = list((install_dir.parent / "publisher-backups").glob("codex-*.exe"))
        self.assertEqual(len(backups), 1, backups)
        self.assertEqual(backups[0].read_bytes(), b"old installed codex")
        self.assert_no_publish_temps(install_dir)

    def test_cleanup_assertion_rejects_each_transaction_residue(self) -> None:
        install_dir = self.repo_root / "install"
        install_dir.mkdir()
        for name in (
            ".install.bundle.fixture",
            ".install.rollback.fixture",
            ".install.codex-local-publish.transaction.json",
            ".install.codex-local-publish.transaction.json.tmp",
        ):
            with self.subTest(residue=name):
                residue = install_dir.parent / name
                residue.touch()
                with self.assertRaises(AssertionError):
                    self.assert_no_publish_temps(install_dir)
                residue.unlink()
        self.assert_no_publish_temps(install_dir)

    def test_manifest_source_mutation_is_rejected_before_install(self) -> None:
        install = Path(self.repo_temp.name) / "install"
        manifest = self.write_source_bundle_manifest(self.source_exe)
        command = rf"""
$global:Mutated = $false
Set-PSBreakpoint -Command Get-CachedLocalPublishFileSha256 -Action {{
    if (-not $global:Mutated) {{
        [IO.File]::AppendAllText({ps_single_quote(self.source_code_mode_host)}, 'changed')
        $global:Mutated = $true
    }}
}} | Out-Null
& {ps_single_quote(SCRIPT)} -SkipBuild -RepoRoot {ps_single_quote(self.repo_root)} `
    -SourceBundleManifest {ps_single_quote(manifest)} -InstallDir {ps_single_quote(install)}
"""
        result = subprocess.run(
            [
                self.shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                command,
            ],
            env=clean_env(),
            capture_output=True,
            text=True,
            check=False,
            timeout=RUN_TIMEOUT_SECONDS,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "digest changed after manifest verification: code-mode-host", result.stderr
        )
        self.assertFalse((install / "codex.exe").exists())
        self.assertEqual(list(install.parent.glob(".install.bundle.*")), [])

    def test_audit_publish_stages_complete_bundle_before_visibility(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            target = install_dir / "codex.exe"
            target.write_bytes(b"old")
            source = temp_path / "source.exe"
            source.write_bytes(b"new")
            missing = temp_path / "missing.exe"
            journal = temp_path / ".install.codex-local-publish.transaction.json"
            command = rf"""
. {ps_single_quote(SCRIPT)} -ImportOnly
$entries = @(
    [pscustomobject]@{{ Name = 'codex'; SourcePath = {ps_single_quote(source)}; TargetPath = {ps_single_quote(target)}; BackupPath = {ps_single_quote(temp_path / "codex.bak")}; HadPreviousTarget = $true; Changed = $true }}
    [pscustomobject]@{{ Name = 'host'; SourcePath = {ps_single_quote(missing)}; TargetPath = {ps_single_quote(install_dir / "host.exe")}; BackupPath = {ps_single_quote(temp_path / "host.bak")}; HadPreviousTarget = $false; Changed = $true }}
)
try {{
    New-CodexRuntimeBundleTransaction -JournalPath {ps_single_quote(journal)} -InstallDir {ps_single_quote(install_dir)} -Entries $entries
    throw 'expected staging failure'
}}
catch {{
    if ($_.Exception.Message -ne {ps_single_quote(f"Cannot stage runtime bundle: source is missing: {missing}")}) {{ throw }}
    exit 0
}}
"""
            result = subprocess.run(
                [
                    self.shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    command,
                ],
                text=True,
                capture_output=True,
                check=False,
                timeout=RUN_TIMEOUT_SECONDS,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(target.read_bytes(), b"old")
            self.assertFalse(journal.exists())
            self.assertEqual(list(temp_path.glob(".install.bundle.*")), [])

    def test_audit_publish_recovers_journaled_bundle_as_one_rollback(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            target = install_dir / "codex.exe"
            backup = temp_path / "codex.bak"
            target.write_bytes(b"new")
            new_target = install_dir / "host.exe"
            new_target.write_bytes(b"new-host")
            backup.write_bytes(b"old")
            stage = temp_path / ".install.bundle.fixture"
            stage.mkdir()
            rollback = temp_path / ".install.rollback.fixture"
            rollback.mkdir()
            (rollback / "codex.exe").write_bytes(b"old")
            journal = temp_path / ".install.codex-local-publish.transaction.json"
            command = rf"""
. {ps_single_quote(SCRIPT)} -ImportOnly
$transaction = [pscustomobject]@{{
    SchemaVersion = 1
    TransactionId = 'fixture'
    Phase = 'Applying'
    JournalPath = {ps_single_quote(journal)}
    InstallDir = {ps_single_quote(install_dir)}
    StageRoot = {ps_single_quote(stage)}
    RollbackRoot = {ps_single_quote(rollback)}
    HadPreviousInstall = $true
    Entries = @([pscustomobject]@{{
        Name = 'codex'
        StagedPath = {ps_single_quote(stage / "codex.exe")}
        TargetPath = {ps_single_quote(target)}
        BackupPath = {ps_single_quote(backup)}
        HadPreviousTarget = $true
        Changed = $true
        State = 'Applying'
    }}, [pscustomobject]@{{
        Name = 'host'
        StagedPath = {ps_single_quote(stage / "host.exe")}
        TargetPath = {ps_single_quote(new_target)}
        BackupPath = $null
        HadPreviousTarget = $false
        Changed = $true
        State = 'Applied'
    }})
}}
Write-CodexPublishTransactionJournal -Transaction $transaction
"""
            result = subprocess.run(
                [
                    self.shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    command,
                ],
                text=True,
                capture_output=True,
                check=False,
                timeout=RUN_TIMEOUT_SECONDS,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            persisted = json.loads(journal.read_text(encoding="utf-8"))
            self.assertEqual(persisted["Phase"], "Applying")
            self.assertEqual(persisted["InstallDir"], str(install_dir))
            self.assertEqual(persisted["RollbackRoot"], str(rollback))
            # Recovery must work after the writer exits, with no ambient
            # $transaction variable left to substitute for the durable journal.
            result = subprocess.run(
                [
                    self.shell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command",
                    f". {ps_single_quote(SCRIPT)} -ImportOnly; "
                    f"if (-not (Recover-CodexRuntimeBundleTransaction -JournalPath {ps_single_quote(journal)})) "
                    "{ throw 'recovery did not run' }",
                ],
                text=True, capture_output=True, check=False,
                env=clean_env(), timeout=RUN_TIMEOUT_SECONDS,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(target.read_bytes(), b"old")
            self.assertFalse(journal.exists())
            self.assertFalse(stage.exists())
            self.assertFalse(new_target.exists())
            self.assertFalse(rollback.exists())

    def test_apply_replaces_target_and_writes_backup(self) -> None:
        for output_args in ((), ("-Concise",), ("-Concise", "-Verbose")):
            with self.subTest(output_args=output_args):
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
                        *output_args,
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
                        code_mode_host_target.read_bytes(),
                        self.source_code_mode_host_bytes,
                    )
                    sandbox_resources = install_dir / "codex-resources"
                    windows_sandbox_setup_target = (
                        sandbox_resources / "codex-windows-sandbox-setup.exe"
                    )
                    command_runner_target = (
                        sandbox_resources / "codex-command-runner.exe"
                    )
                    self.assertEqual(
                        windows_sandbox_setup_target.read_bytes(),
                        self.source_windows_sandbox_setup_bytes,
                    )
                    self.assertEqual(
                        command_runner_target.read_bytes(),
                        self.source_command_runner_bytes,
                    )
                    backup_dir = install_dir.parent / "publisher-backups"
                    backups = sorted(backup_dir.glob("codex-2*.exe"))
                    self.assertEqual(len(backups), 1)
                    self.assertEqual(backups[0].read_bytes(), b"previous-codex")
                    code_mode_host_backups = sorted(
                        backup_dir.glob("codex-code-mode-host-*.exe")
                    )
                    self.assertEqual(len(code_mode_host_backups), 1)
                    self.assertEqual(
                        code_mode_host_backups[0].read_bytes(), previous_code_mode_host
                    )
                    previous_sha256 = hashlib.sha256(b"previous-codex").hexdigest()
                    previous_code_mode_host_sha256 = hashlib.sha256(
                        previous_code_mode_host
                    ).hexdigest()
                    self.assertIn("publishCommitted: true", result.stdout)
                    if output_args == ("-Concise",):
                        self.assertNotIn("targetSha256:", result.stdout)
                        self.assertNotIn("backupPath:", result.stdout)
                        self.assertNotIn("desktopAppPackage:", result.stdout)
                        self.assertLessEqual(
                            len(result.stdout.splitlines()), 12, result.stdout
                        )
                    else:
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
                        self.assertIn(
                            "codexPostPublishVerify: sha256 ok", result.stdout
                        )
                        self.assertIn(
                            "codeModeHostPostPublishVerify: sha256 ok", result.stdout
                        )
                        self.assertIn(
                            "windowsSandboxSetupPostPublishVerify: sha256 ok",
                            result.stdout,
                        )
                        self.assertIn(
                            "commandRunnerPostPublishVerify: sha256 ok", result.stdout
                        )
                        self.assertRegex(
                            result.stdout,
                            r"targetBeforeVersion: <unavailable: [^\r\n]+>[\r\n]",
                        )
                    self.assert_no_publish_temps(install_dir)

    def test_apply_prunes_old_publish_backups(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            backup_dir = install_dir.parent / "publisher-backups"
            backup_dir.mkdir(parents=True)
            for index in range(12):
                backup = backup_dir / f"codex-20000101T0000{index:02d}000Z.exe"
                backup.write_bytes(f"backup-{index}".encode("utf-8"))
                # Retention follows the timestamp in the name, not mutable mtime.
                timestamp = 946684800 + (11 - index)
                os.utime(backup, (timestamp, timestamp))

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
            self.assertEqual(len(backups), 10)
            self.assertCountEqual(
                [backup.read_bytes() for backup in backups],
                [
                    self.source_exe_bytes,
                    *[f"backup-{index}".encode() for index in range(3, 12)],
                ],
            )
            self.assertIn("backupPruned:", result.stdout)
            self.assert_no_publish_temps(install_dir)

    def test_host_only_publish_does_not_reserve_nonexistent_codex_backup(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            backup_dir = install_dir.parent / "publisher-backups"
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
            self.assertCountEqual(
                [path.read_bytes() for path in backup_dir.glob("codex-2*.exe")],
                [f"backup-{index}".encode("utf-8") for index in range(10)],
            )
            self.assertEqual(target.read_bytes(), fake_codex.read_bytes())
            self.assertEqual(
                (install_dir / "codex-code-mode-host.exe").read_bytes(),
                self.source_code_mode_host_bytes,
            )
            host_backups = list(backup_dir.glob("codex-code-mode-host-*.exe"))
            self.assertEqual(len(host_backups), 1)
            self.assertEqual(host_backups[0].read_bytes(), b"old-host")
            self.assert_no_publish_temps(install_dir)
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
                "-Concise",
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertNotIn("publishCommitted: true", result.stdout)
            self.assertEqual(target.read_bytes(), previous)
            self.assertEqual(
                code_mode_host_target.read_bytes(), previous_code_mode_host
            )
            backup_dir = install_dir.parent / "publisher-backups"
            backups = sorted(backup_dir.glob("codex-2*.exe"))
            self.assertEqual(len(backups), 1)
            self.assertEqual(backups[0].read_bytes(), previous)
            code_mode_host_backups = sorted(
                backup_dir.glob("codex-code-mode-host-*.exe")
            )
            self.assertEqual(len(code_mode_host_backups), 1)
            self.assertEqual(
                code_mode_host_backups[0].read_bytes(), previous_code_mode_host
            )
            self.assertIn("rollback: requested:", result.stdout)
            self.assertIn("bundleTransactionRecovery: rolled back:", result.stdout)
            self.assertIn(
                "Published Codex binary failed --version verification",
                result.stderr,
            )
            self.assert_no_publish_temps(install_dir)

    def test_failed_publish_restores_process_desktop_routing(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=FRESH_SOURCE_TIME,
            )
            target = install_dir / "codex.exe"
            target.write_bytes(b"previous-codex")
            code_mode_host_target = install_dir / "codex-code-mode-host.exe"
            code_mode_host_target.write_bytes(b"previous-code-mode-host")
            local_home = temp_path / "local-home"
            sqlite_home = temp_path / "sqlite-home"

            publish_args = [
                "-RepoRoot",
                str(self.repo_root),
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-SourceCodeModeHostExe",
                str(self.source_code_mode_host),
                "-SourceWindowsSandboxSetupExe",
                str(self.source_windows_sandbox_setup),
                "-SourceCommandRunnerExe",
                str(self.source_command_runner),
                "-SourceBundleManifest",
                str(self.write_source_bundle_manifest(fake_codex)),
                "-InstallDir",
                str(install_dir),
                "-ConfigureDesktopLocalCli",
                "-DesktopCliEnvironmentTarget",
                "Process",
                "-LocalCodexHome",
                str(local_home),
                "-LocalCodexSqliteHome",
                str(sqlite_home),
            ]
            quoted_args = " ".join(
                arg if arg.startswith("-") else ps_single_quote(arg)
                for arg in publish_args
            )
            wrapper = (
                "$failure = $null; "
                f"try {{ & {ps_single_quote(SCRIPT)} {quoted_args} }} "
                "catch { $failure = $_.Exception.Message; "
                'Write-Output "publishFailure=$failure" }; '
                'Write-Output "routingAfterCli=$([Environment]::GetEnvironmentVariable('
                "'CODEX_CLI_PATH', 'Process'))\"; "
                'Write-Output "routingAfterHome=$([Environment]::GetEnvironmentVariable('
                "'CODEX_HOME', 'Process'))\"; "
                'Write-Output "routingAfterHomeIsNull=$($null -eq '
                "[Environment]::GetEnvironmentVariable('CODEX_HOME', 'Process'))\"; "
                'Write-Output "routingAfterSqlite=$([Environment]::GetEnvironmentVariable('
                "'CODEX_SQLITE_HOME', 'Process'))\"; "
                'Write-Output "routingAfterPath=$([Environment]::GetEnvironmentVariable('
                "'Path', 'Process'))\"; "
                "if ($null -eq $failure) { exit 9 }"
            )
            env = clean_env()
            original_cli = "C:\\prior\\codex.exe"
            original_sqlite = "C:\\prior\\sqlite"
            path_key = next(
                (key for key in env if key.casefold() == "path"),
                "Path",
            )
            original_path = f"{install_dir};{env[path_key]}"
            env.update(
                {
                    "CODEX_CLI_PATH": original_cli,
                    "CODEX_SQLITE_HOME": original_sqlite,
                }
            )
            env[path_key] = original_path

            result = subprocess.run(
                [
                    self.shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    wrapper,
                ],
                text=True,
                capture_output=True,
                check=False,
                env=env,
                timeout=RUN_TIMEOUT_SECONDS,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("publishFailure=", result.stdout)
            self.assertIn("desktopRoutingRollback: restored", result.stdout)
            self.assertIn(f"routingAfterCli={original_cli}", result.stdout)
            self.assertIn("routingAfterHome=", result.stdout)
            self.assertIn("routingAfterHomeIsNull=True", result.stdout)
            self.assertIn(f"routingAfterSqlite={original_sqlite}", result.stdout)
            self.assertIn(f"routingAfterPath={original_path}", result.stdout)
            self.assertEqual(target.read_bytes(), b"previous-codex")
            self.assertEqual(
                code_mode_host_target.read_bytes(),
                b"previous-code-mode-host",
            )

    def test_failed_publish_can_rollback_when_backup_dir_is_over_limit(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            backup_dir = install_dir.parent / "publisher-backups"
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
            self.assertIn("bundleTransactionRecovery: rolled back:", result.stdout)
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
            self.assertIn("bundleTransactionRecovery: rolled back:", result.stdout)
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

            with self.running_codex(target) as process:
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

            with self.running_codex(code_mode_host_target) as process:
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

            with self.running_codex(target) as process:
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
                # Windows cannot delete the old image while it is running.
                # The durable commit must survive until the next publisher
                # finishes cleanup, rather than rolling back the new bundle.
                journal = temp_path / ".install.codex-local-publish.transaction.json"
                self.assertEqual(json.loads(journal.read_text())["Phase"], "Committed")
                self.assertIn("deferred transaction cleanup", result.stdout)
            result = self.run_script(
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("binaryChanged: false", result.stdout)
            self.assertEqual(target.read_bytes(), fake_codex.read_bytes())
            self.assert_no_publish_temps(install_dir)


if __name__ == "__main__":
    unittest.main()
