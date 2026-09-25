#!/usr/bin/env python3

from pathlib import Path
import json
import os
import subprocess
import tempfile
import unittest

from scripts.publish_local_codex_test_support import PublishLocalCodexTestBase
from scripts.publish_local_codex_test_support import clean_env
from scripts.publish_local_codex_test_support import ps_single_quote


SCRIPT = Path(__file__).resolve().parent / "publish-local-codex.ps1"


FIXTURE_TIME = 946684900
FRESH_SOURCE_TIME = FIXTURE_TIME + 10_000


class PublishLocalCodexFreshnessTest(PublishLocalCodexTestBase):
    def test_audit_publish_unknown_timestamps_fail_closed_as_stale(self) -> None:
        command = rf"""
. {ps_single_quote(SCRIPT)} -ImportOnly
[pscustomobject]@{{
    missingSource = Test-FileStaleAgainstSource -SourceNewestUtc $null -FileLastWriteUtc ([DateTime]::UtcNow)
    missingArtifact = Test-FileStaleAgainstSource -SourceNewestUtc ([DateTime]::UtcNow) -FileLastWriteUtc $null
    malformed = Test-FileStaleAgainstSource -SourceNewestUtc 'not-a-time' -FileLastWriteUtc ([DateTime]::UtcNow)
}} | ConvertTo-Json -Compress
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
            timeout=120,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads(result.stdout),
            {"missingSource": True, "missingArtifact": True, "malformed": True},
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
            self.install_matching_publish_helpers(install_dir)

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
            self.assertFalse((install_dir.parent / "publisher-backups").exists())
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
            self.assertEqual(
                {
                    path.name
                    for path in (install_dir.parent / "publisher-backups").iterdir()
                },
                {".codex-local-publish-backups"},
            )
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
            install_dir = Path(temp_dir) / "install"
            source_timestamp = FIXTURE_TIME + 100
            self.write_built_artifacts(timestamp=source_timestamp)
            self.write_build_stamp("local-release", source_timestamp)
            self.touch_tracked_source(source_timestamp + 10)

            result = self.run_script(
                "-DryRun",
                "-AutoSkipBuild",
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
                "autoSkipBuildReason: tracked publish inputs changed",
                result.stdout,
            )
            self.assertIn("buildCommand: cargo build --target-dir", result.stdout)
            self.assertIn(" build --target-dir ", result.stdout)
            self.assertIn("--profile local-release", result.stdout)
            self.assertIn("(not run)", result.stdout)
            self.assertNotIn("buildCommand: <skipped>", result.stdout)

    def test_auto_skip_build_detects_same_size_same_mtime_source_change(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"
            source_timestamp = FIXTURE_TIME + 100
            self.write_built_artifacts(timestamp=source_timestamp)
            self.write_build_stamp("local-release", source_timestamp)
            tracked = self.repo_root / "codex-rs" / "tracked-source.rs"
            original_stat = tracked.stat()
            original_size = original_stat.st_size
            tracked.write_text("changed\n", encoding="utf-8")
            os.utime(
                tracked,
                ns=(original_stat.st_atime_ns, original_stat.st_mtime_ns),
            )
            self.assertEqual(tracked.stat().st_size, original_size)
            self.assertEqual(tracked.stat().st_mtime_ns, original_stat.st_mtime_ns)

            result = self.run_script(
                "-DryRun",
                "-AutoSkipBuild",
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
                "autoSkipBuildReason: tracked publish inputs changed",
                result.stdout,
            )
            self.assertIn("buildCommand: cargo build --target-dir", result.stdout)
            self.assertNotIn("buildCommand: <skipped>", result.stdout)

    def test_auto_skip_build_scans_committed_publish_entrypoint(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"
            source_timestamp = FIXTURE_TIME + 100
            entrypoint = self.repo_root / "scripts" / "publish-local-codex.ps1"
            entrypoint.parent.mkdir(parents=True)
            entrypoint.write_text("# publish entrypoint\n", encoding="utf-8")
            self.run_git("add", "scripts/publish-local-codex.ps1")
            self.run_git("commit", "--quiet", "-m", "add publish entrypoint")
            os.utime(entrypoint, (source_timestamp + 10, source_timestamp + 10))
            self.assertEqual(self.run_git("status", "--porcelain").stdout, "")
            self.write_built_artifacts(timestamp=source_timestamp)
            self.write_build_stamp("local-release", source_timestamp)
            entrypoint.write_text("# changed publish entrypoint\n", encoding="utf-8")
            os.utime(entrypoint, (source_timestamp + 10, source_timestamp + 10))

            result = self.run_script(
                "-DryRun",
                "-AutoSkipBuild",
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
                "autoSkipBuildReason: tracked publish inputs changed",
                result.stdout,
            )
            self.assertIn("buildCommand: cargo build --target-dir", result.stdout)
            self.assertNotIn("buildCommand: <skipped>", result.stdout)

    def test_auto_skip_build_ignores_unrelated_source_changes(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"
            source_timestamp = FIXTURE_TIME + 100
            self.write_built_artifacts(timestamp=source_timestamp + 20)
            self.write_build_stamp("local-release", source_timestamp)
            self.touch_unrelated_source(source_timestamp + 10)

            result = self.run_script(
                "-DryRun",
                "-AutoSkipBuild",
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
                "autoSkipBuildReason: source artifacts and tracked publish inputs match build stamp",
                result.stdout,
            )
            self.assertIn("buildCommand: <skipped>", result.stdout)
            self.assertIn("sourceBuildStale: False", result.stdout)

    def test_auto_skip_build_uses_content_stamp_when_sidecar_mtime_is_old(
        self,
    ) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"
            source_timestamp = FIXTURE_TIME + 100
            sidecar_timestamp = FIXTURE_TIME - 100
            _, built_code_mode_host, _, _ = self.write_built_artifacts(
                timestamp=source_timestamp
            )
            os.utime(
                built_code_mode_host,
                (sidecar_timestamp, sidecar_timestamp),
            )
            self.write_build_stamp("local-release", source_timestamp)

            result = self.run_script(
                "-DryRun",
                "-AutoSkipBuild",
                "-FailOnStaleSourceBuild",
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assert_proof_value(result.stdout, "autoSkipBuild", "true")
            self.assert_proof_value(
                result.stdout,
                "autoSkipBuildReason",
                "source artifacts and tracked publish inputs match build stamp",
            )
            self.assert_proof_value(
                result.stdout,
                "sourceBuildFreshnessBasis",
                "content-bound build stamp",
            )
            self.assert_proof_value(
                result.stdout,
                "codeModeHostSourceBuildStale",
                "False",
            )
            self.assert_proof_value(result.stdout, "sourceBuildStale", "False")
            self.assertNotIn("sourceBuildStaleRemedy:", result.stdout)

    def test_explicit_skip_build_uses_content_stamp_when_sidecar_mtime_is_old(
        self,
    ) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            source_timestamp = FIXTURE_TIME + 100
            sidecar_timestamp = FIXTURE_TIME - 100
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=source_timestamp,
            )
            os.utime(
                self.source_code_mode_host,
                (sidecar_timestamp, sidecar_timestamp),
            )
            self.write_build_stamp("local-release", source_timestamp, fake_codex)

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-FailOnStaleSourceBuild",
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
            self.assert_proof_value(
                result.stdout,
                "sourceBuildStampValidation",
                "source artifacts and tracked publish inputs match build stamp",
            )
            self.assert_proof_value(
                result.stdout,
                "sourceBuildFreshnessBasis",
                "content-bound build stamp",
            )
            self.assert_proof_value(
                result.stdout,
                "codeModeHostSourceBuildStale",
                "False",
            )
            self.assert_proof_value(result.stdout, "sourceBuildStale", "False")
            self.assertNotIn("sourceBuildStaleRemedy:", result.stdout)

    def test_skip_build_rejects_stamped_artifacts_from_other_publish_inputs(
        self,
    ) -> None:
        # Recipe inputs such as RUSTFLAGS change without touching source mtimes;
        # newer artifact timestamps must not outvote the content-bound stamp.
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"
            install_dir.mkdir()
            target = install_dir / "codex.exe"
            target.write_bytes(b"previous-codex")
            self.write_built_artifacts(timestamp=FRESH_SOURCE_TIME)
            stamp_env = clean_env()
            stamp_env.pop("RUSTFLAGS", None)
            self.write_build_stamp("local-release", FIXTURE_TIME, env=stamp_env)

            # Default Cargo outputs without a manifest; run_script would inject
            # a digest-bound manifest for -SkipBuild.
            result = subprocess.run(
                [
                    self.shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    str(SCRIPT),
                    "-RepoRoot",
                    str(self.repo_root),
                    "-SkipBuild",
                    "-InstallDir",
                    str(install_dir),
                    "-BackupDir",
                    str(Path(temp_dir) / "backups"),
                ],
                text=True,
                capture_output=True,
                check=False,
                env={**stamp_env, "RUSTFLAGS": "-C target-cpu=native"},
                timeout=120,
            )

            self.assertNotEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assert_proof_value(
                result.stdout,
                "sourceBuildStampValidation",
                "tracked publish inputs changed",
            )
            self.assert_proof_value(
                result.stdout,
                "sourceBuildFreshnessBasis",
                "content-bound build stamp from different publish inputs",
            )
            self.assert_proof_value(result.stdout, "sourceBuildStale", "True")
            self.assert_proof_value(
                result.stdout, "replace", "blocked: source build stale"
            )
            # PowerShell hard-wraps errors at 120 columns; match within line one.
            self.assertIn(
                "their content-bound build stamp records different", result.stderr
            )
            self.assertEqual(target.read_bytes(), b"previous-codex")

    def test_auto_skip_revalidates_stamp_after_source_version_probe(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"
            source_timestamp = FIXTURE_TIME + 100
            tracked = self.repo_root / "codex-rs" / "tracked-source.rs"
            self.write_built_artifacts(timestamp=source_timestamp)
            self.write_build_stamp("local-release", source_timestamp)
            # Change a tracked input during the source version probe, after
            # auto-skip has already accepted the stamp.
            command = rf"""
$global:Mutated = $false
Set-PSBreakpoint -Command Write-VersionProofBlock -Action {{
    if (-not $global:Mutated) {{
        [IO.File]::WriteAllText({ps_single_quote(tracked)}, "changed-during-version-probe`n")
        $global:Mutated = $true
    }}
}} | Out-Null
& {ps_single_quote(SCRIPT)} -AutoSkipBuild -RepoRoot {ps_single_quote(self.repo_root)} `
    -InstallDir {ps_single_quote(install_dir)}
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
                env=clean_env(),
                timeout=120,
            )

            self.assertNotEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assert_proof_value(result.stdout, "autoSkipBuild", "true")
            self.assert_proof_value(
                result.stdout,
                "sourceBuildStampValidation",
                "tracked publish inputs changed",
            )
            self.assert_proof_value(
                result.stdout,
                "sourceBuildFreshnessBasis",
                "content-bound build stamp invalidated before publish",
            )
            self.assert_proof_value(result.stdout, "sourceBuildStale", "True")
            self.assert_proof_value(
                result.stdout,
                "replace",
                "blocked: source build stale",
            )
            self.assertIn(
                "content-bound build stamp no longer matches",
                result.stderr,
            )
            self.assertFalse((install_dir / "codex.exe").exists())

    def test_auto_skip_build_detects_same_size_same_mtime_artifact_change(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"
            source_timestamp = FIXTURE_TIME + 100
            built_codex, _, _, _ = self.write_built_artifacts(
                codex_bytes=b"codex artifact test-commit",
                timestamp=source_timestamp,
            )
            self.write_build_stamp("local-release", source_timestamp)
            original_stat = built_codex.stat()
            original_bytes = built_codex.read_bytes()
            changed_bytes = original_bytes.replace(b"test-commit", b"best-commit")
            self.assertNotEqual(changed_bytes, original_bytes)
            self.assertEqual(len(changed_bytes), len(original_bytes))
            built_codex.write_bytes(changed_bytes)
            os.utime(
                built_codex,
                ns=(original_stat.st_atime_ns, original_stat.st_mtime_ns),
            )

            result = self.run_script(
                "-DryRun",
                "-AutoSkipBuild",
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
                "autoSkipBuildReason: source artifact differs from stamped build",
                result.stdout,
            )
            self.assertIn("buildCommand: cargo build --target-dir", result.stdout)
            self.assertNotIn("buildCommand: <skipped>", result.stdout)

    def test_auto_skip_build_requires_code_mode_host_artifact(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"
            _, built_code_mode_host, _, _ = self.write_built_artifacts(
                timestamp=FRESH_SOURCE_TIME
            )
            built_code_mode_host.unlink()

            result = self.run_script(
                "-DryRun",
                "-AutoSkipBuild",
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
            self.assertIn("buildCommand: cargo build --target-dir", result.stdout)
            self.assertIn("-p codex-cli -p codex-code-mode-host", result.stdout)

    def test_auto_skip_build_does_not_skip_without_build_stamp(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"
            self.write_built_artifacts(timestamp=FRESH_SOURCE_TIME)

            result = self.run_script(
                "-DryRun",
                "-AutoSkipBuild",
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
                "autoSkipBuildReason: build stamp missing",
                result.stdout,
            )
            self.assertIn("buildCommand: cargo build --target-dir", result.stdout)
            self.assertIn(" build --target-dir ", result.stdout)
            self.assertIn("--profile local-release", result.stdout)
            self.assertIn("(not run)", result.stdout)
            self.assertNotIn("buildCommand: <skipped>", result.stdout)

    def test_auto_skip_build_rejects_legacy_timestamp_stamp(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"
            self.write_built_artifacts(timestamp=FIXTURE_TIME + 100)
            stamp = (
                self.repo_root
                / "codex-rs"
                / "target"
                / "codex-local-publish-local-release.stamp"
            )
            stamp.write_text("2000-01-01T00:00:00.0000000Z", encoding="utf-8")

            result = self.run_script(
                "-DryRun",
                "-AutoSkipBuild",
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
                "autoSkipBuildReason: build stamp legacy or invalid",
                result.stdout,
            )
            self.assertIn("buildCommand: cargo build --target-dir", result.stdout)
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
            / "publish-local-release"
            / "local-release"
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

        release_result = self.run_script("-PrintBuiltCodexPath", "-Profile", "release")
        self.assertEqual(
            release_result.returncode,
            0,
            f"stdout:\n{release_result.stdout}\nstderr:\n{release_result.stderr}",
        )
        self.assertEqual(
            Path(release_result.stdout.strip()),
            self.repo_root
            / "codex-rs"
            / "target"
            / "publish-release"
            / "release"
            / "codex.exe",
        )

    def test_dry_run_debug_profile_uses_cargo_dev_profile(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"

            result = self.run_script(
                "-DryRun",
                "-Profile",
                "debug",
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

    def test_dry_run_release_reports_only_artifact_producing_build(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"

            result = self.run_script(
                "-DryRun",
                "-Profile",
                "release",
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("--profile release", result.stdout)
            self.assertNotIn("preflightCheckCommand:", result.stdout)
            self.assertNotIn(" check --target-dir ", result.stdout)
            self.assertIn("buildCommand: cargo --config", result.stdout)
            self.assertIn(" build --target-dir ", result.stdout)
            self.assertIn("(not run)", result.stdout)

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
            self.install_matching_publish_helpers(install_dir)

            observed = {
                name: temp_path / (name + ".called")
                for name in ("Write-ProofLine", "Get-AppxPackage")
            }
            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-FastProof",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
                observe_commands=observed,
            )

            self.assertTrue(observed["Write-ProofLine"].exists(), result.stderr)
            self.assertFalse(observed["Get-AppxPackage"].exists())
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
            self.install_matching_publish_helpers(install_dir)

            observed = {
                name: temp_path / (name + ".called")
                for name in ("Write-ProofLine", "Invoke-DoctorForPublish")
            }
            result = self.run_script(
                "-SkipBuild",
                "-RunDoctor",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
                observe_commands=observed,
            )

            self.assertTrue(observed["Write-ProofLine"].exists(), result.stderr)
            self.assertFalse(observed["Invoke-DoctorForPublish"].exists())
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
