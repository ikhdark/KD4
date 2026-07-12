#!/usr/bin/env python3

from pathlib import Path
import os
import tempfile
import unittest

from scripts.publish_local_codex_test_support import PublishLocalCodexTestBase


FIXTURE_TIME = 946684900
FRESH_SOURCE_TIME = FIXTURE_TIME + 10_000


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
            self.write_build_stamp("release", source_timestamp, fake_codex)
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
                "autoSkipBuildReason: tracked publish inputs changed",
                result.stdout,
            )
            self.assertIn("buildCommand: cargo --config", result.stdout)
            self.assertIn(" build --target-dir ", result.stdout)
            self.assertIn("--profile release", result.stdout)
            self.assertIn("(not run)", result.stdout)
            self.assertNotIn("buildCommand: <skipped>", result.stdout)

    def test_auto_skip_build_detects_same_size_same_mtime_source_change(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            source_timestamp = FIXTURE_TIME + 100
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=source_timestamp,
            )
            self.write_build_stamp("release", source_timestamp, fake_codex)
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
                "autoSkipBuildReason: tracked publish inputs changed",
                result.stdout,
            )
            self.assertIn("buildCommand: cargo --config", result.stdout)
            self.assertNotIn("buildCommand: <skipped>", result.stdout)

    def test_auto_skip_build_scans_committed_split_publish_helpers(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            source_timestamp = FIXTURE_TIME + 100
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=source_timestamp,
            )
            helper = self.repo_root / "scripts" / "publish-local-codex.build.ps1"
            helper.parent.mkdir(parents=True)
            helper.write_text("# split publish helper\n", encoding="utf-8")
            self.run_git("add", "scripts/publish-local-codex.build.ps1")
            self.run_git("commit", "--quiet", "-m", "add split publish helper")
            os.utime(helper, (source_timestamp + 10, source_timestamp + 10))
            self.assertEqual(self.run_git("status", "--porcelain").stdout, "")
            self.write_build_stamp("release", source_timestamp, fake_codex)
            helper.write_text("# changed split publish helper\n", encoding="utf-8")
            os.utime(helper, (source_timestamp + 10, source_timestamp + 10))

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
                "autoSkipBuildReason: tracked publish inputs changed",
                result.stdout,
            )
            self.assertIn("buildCommand: cargo --config", result.stdout)
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
            self.write_build_stamp("release", source_timestamp, fake_codex)
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
            self.write_build_stamp("release", source_timestamp, fake_codex)

            result = self.run_script(
                "-DryRun",
                "-AutoSkipBuild",
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
            self.write_build_stamp("release", source_timestamp, fake_codex)

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

    def test_auto_skip_revalidates_stamp_after_source_version_probe(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            source_timestamp = FIXTURE_TIME + 100
            tracked = self.repo_root / "codex-rs" / "tracked-source.rs"
            fake_codex = temp_path / "fake-codex.cmd"
            fake_codex.write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        f'> "{tracked}" echo changed-during-version-probe',
                        "echo codex 9.9.9",
                        "echo commit: test-commit",
                    ]
                ),
                encoding="utf-8",
            )
            os.utime(fake_codex, (source_timestamp, source_timestamp))
            self.write_build_stamp("release", source_timestamp, fake_codex)

            result = self.run_script(
                "-AutoSkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
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
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            source_timestamp = FIXTURE_TIME + 100
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=source_timestamp,
            )
            self.write_build_stamp("release", source_timestamp, fake_codex)
            original_stat = fake_codex.stat()
            original_bytes = fake_codex.read_bytes()
            changed_bytes = original_bytes.replace(b"test-commit", b"best-commit")
            self.assertNotEqual(changed_bytes, original_bytes)
            self.assertEqual(len(changed_bytes), len(original_bytes))
            fake_codex.write_bytes(changed_bytes)
            os.utime(
                fake_codex,
                ns=(original_stat.st_atime_ns, original_stat.st_mtime_ns),
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
                "autoSkipBuildReason: source artifact differs from stamped build",
                result.stdout,
            )
            self.assertIn("buildCommand: cargo --config", result.stdout)
            self.assertNotIn("buildCommand: <skipped>", result.stdout)

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

    def test_auto_skip_build_does_not_skip_without_build_stamp(self) -> None:
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
                "autoSkipBuildReason: build stamp missing",
                result.stdout,
            )
            self.assertIn("buildCommand: cargo --config", result.stdout)
            self.assertIn(" build --target-dir ", result.stdout)
            self.assertIn("--profile release", result.stdout)
            self.assertIn("(not run)", result.stdout)
            self.assertNotIn("buildCommand: <skipped>", result.stdout)

    def test_auto_skip_build_rejects_legacy_timestamp_stamp(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            source_timestamp = FIXTURE_TIME + 100
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=source_timestamp,
            )
            stamp = (
                self.repo_root
                / "codex-rs"
                / "target"
                / "codex-local-publish-release.stamp"
            )
            stamp.parent.mkdir(parents=True)
            stamp.write_text("2000-01-01T00:00:00.0000000Z", encoding="utf-8")

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
                "autoSkipBuildReason: build stamp legacy or invalid",
                result.stdout,
            )
            self.assertIn("buildCommand: cargo --config", result.stdout)
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
