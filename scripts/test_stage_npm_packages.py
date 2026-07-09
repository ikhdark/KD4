from __future__ import annotations

import io
import json
import sys
import tarfile
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock

import scripts.stage_npm_packages as stage


class StageNpmPackagesTests(unittest.TestCase):
    def setUp(self) -> None:
        if hasattr(stage, "_BUILD_MODULE"):
            delattr(stage, "_BUILD_MODULE")
        self.temp_dir = tempfile.TemporaryDirectory()
        self.root = Path(self.temp_dir.name)
        if hasattr(stage.list_workflow_artifacts, "cache_clear"):
            stage.list_workflow_artifacts.cache_clear()
        if hasattr(stage.load_build_module, "cache_clear"):
            stage.load_build_module.cache_clear()

    def tearDown(self) -> None:
        if hasattr(stage.list_workflow_artifacts, "cache_clear"):
            stage.list_workflow_artifacts.cache_clear()
        if hasattr(stage.load_build_module, "cache_clear"):
            stage.load_build_module.cache_clear()
        self.temp_dir.cleanup()

    def test_stage_models_use_slots(self) -> None:
        self.assertFalse(hasattr(stage.WorkflowArtifact("target", 1), "__dict__"))
        self.assertFalse(
            hasattr(stage.BinaryComponent("artifact", "dest", "binary"), "__dict__")
        )

    def test_build_package_metadata_is_loaded_lazily(self) -> None:
        fake_module = types.SimpleNamespace(
            PACKAGE_NATIVE_COMPONENTS={
                "codex": set(),
                "codex-linux-x64": {"codex-package"},
            },
            PACKAGE_EXPANSIONS={"codex": ["codex", "codex-linux-x64"]},
            CODEX_PLATFORM_PACKAGES={
                "codex-linux-x64": {
                    "npm_name": "@openai/codex-linux-x64",
                    "npm_tag": "linux-x64",
                    "target_triple": "x86_64-unknown-linux-musl",
                    "os": "linux",
                    "cpu": "x64",
                }
            },
            CODEX_PACKAGE_COMPONENT="codex-package",
            PACKAGE_TARGET_FILTERS={
                "codex-linux-x64": {"x86_64-unknown-linux-musl"},
            },
        )

        self.assertNotIn("_BUILD_MODULE", vars(stage))
        with mock.patch.object(stage, "load_build_module", return_value=fake_module):
            self.assertEqual(
                stage.native_components_for_package("codex-linux-x64"),
                ("codex-package",),
            )
            self.assertEqual(
                stage.expand_packages(["codex"]), ["codex", "codex-linux-x64"]
            )
            self.assertEqual(
                stage.native_targets_for_package("codex-linux-x64"),
                ("x86_64-unknown-linux-musl",),
            )
            self.assertEqual(
                stage.collect_native_component_sets(["codex-linux-x64"]),
                [(("codex-package",), ("x86_64-unknown-linux-musl",))],
            )
            self.assertEqual(
                stage.tarball_name_for_package("codex-linux-x64", "1.2.3"),
                "codex-npm-linux-x64-1.2.3.tgz",
            )

    def test_build_module_reads_platform_metadata_from_package_json(self) -> None:
        build = stage.load_build_module()

        self.assertIn("codex-linux-x64", build.CODEX_PLATFORM_PACKAGES)
        self.assertEqual(
            build.CODEX_PLATFORM_PACKAGES["codex-linux-x64"]["target_triple"],
            "x86_64-unknown-linux-musl",
        )
        self.assertEqual(
            build.PACKAGE_TARGET_FILTERS["codex-linux-x64"],
            {"x86_64-unknown-linux-musl"},
        )
        self.assertEqual(
            build.PACKAGE_NATIVE_COMPONENTS["codex-linux-x64"],
            ["codex-package"],
        )

        package_json = build.build_codex_package_json("1.2.3")
        self.assertEqual(
            package_json["optionalDependencies"]["@openai/codex-linux-x64"],
            "npm:@openai/codex@1.2.3-linux-x64",
        )

    def test_platform_package_manifest_is_minimal(self) -> None:
        build = stage.load_build_module()

        package_json = build.build_platform_package_json(
            "1.2.3-linux-x64",
            build.CODEX_PLATFORM_PACKAGES["codex-linux-x64"],
        )

        self.assertEqual(package_json["name"], "@openai/codex")
        self.assertEqual(package_json["version"], "1.2.3-linux-x64")
        self.assertEqual(package_json["os"], ["linux"])
        self.assertEqual(package_json["cpu"], ["x64"])
        self.assertEqual(package_json["files"], ["vendor"])
        self.assertNotIn("packageManager", package_json)

    def test_codex_sdk_staging_injects_matching_cli_dependency(self) -> None:
        build = stage.load_build_module()

        with mock.patch.object(build, "stage_codex_sdk_sources") as stage_sdk_sources:
            build.stage_sources(self.root, "1.2.3", "codex-sdk")

        stage_sdk_sources.assert_called_once_with(self.root)
        package_json = json.loads((self.root / "package.json").read_text())
        self.assertEqual(package_json["dependencies"]["@openai/codex"], "1.2.3")
        self.assertNotIn("prepare", package_json["scripts"])

    def test_copy_native_binaries_filters_target_and_requires_executable(self) -> None:
        build = stage.load_build_module()
        vendor_src = self.root / "vendor-src"
        selected_target = vendor_src / "x86_64-unknown-linux-musl"
        selected_bin = selected_target / "bin"
        selected_bin.mkdir(parents=True)
        (selected_bin / "codex").write_text("native", encoding="utf-8")

        skipped_target = vendor_src / "aarch64-unknown-linux-musl"
        skipped_bin = skipped_target / "bin"
        skipped_bin.mkdir(parents=True)
        (skipped_bin / "codex").write_text("native", encoding="utf-8")

        staging_dir = self.root / "staging"
        staging_dir.mkdir()
        build.copy_native_binaries(
            vendor_src,
            staging_dir,
            [build.CODEX_PACKAGE_COMPONENT],
            {"x86_64-unknown-linux-musl"},
        )

        self.assertTrue(
            (
                staging_dir / "vendor" / "x86_64-unknown-linux-musl" / "bin" / "codex"
            ).is_file()
        )
        self.assertFalse(
            (staging_dir / "vendor" / "aarch64-unknown-linux-musl").exists()
        )

        missing_src = self.root / "missing-src"
        (missing_src / "x86_64-unknown-linux-musl").mkdir(parents=True)
        with self.assertRaisesRegex(RuntimeError, "Missing Codex executable"):
            build.copy_native_binaries(
                missing_src,
                self.root / "missing-staging",
                [build.CODEX_PACKAGE_COMPONENT],
                {"x86_64-unknown-linux-musl"},
            )

    def test_parse_args_accepts_max_download_workers(self) -> None:
        argv = [
            "stage_npm_packages.py",
            "--release-version",
            "1.2.3",
            "--package",
            "codex",
            "--max-download-workers",
            "4",
            "--max-stage-workers",
            "2",
            "--cache-dir",
            str(self.root / "cache"),
            "--vendor-copy-mode",
            "hardlink",
            "--github-repo",
            "local/fork",
            "--workflow-name",
            ".github/workflows/local-release.yml",
        ]
        with mock.patch.object(sys, "argv", argv):
            args = stage.parse_args()

        self.assertEqual(args.max_download_workers, 4)
        self.assertEqual(args.max_stage_workers, 2)
        self.assertEqual(args.cache_dir, self.root / "cache")
        self.assertEqual(args.vendor_copy_mode, "hardlink")
        self.assertEqual(args.github_repo, "local/fork")
        self.assertEqual(args.workflow_name, ".github/workflows/local-release.yml")

    def test_parse_args_reads_github_repo_from_environment(self) -> None:
        argv = [
            "stage_npm_packages.py",
            "--release-version",
            "1.2.3",
            "--package",
            "codex",
        ]
        with (
            mock.patch.object(sys, "argv", argv),
            mock.patch.dict(stage.os.environ, {"CODEX_STAGE_GITHUB_REPO": "env/fork"}),
        ):
            args = stage.parse_args()

        self.assertEqual(args.github_repo, "env/fork")

    def test_resolve_github_repo_falls_back_to_current_gh_repo(self) -> None:
        with mock.patch.object(
            stage.subprocess,
            "check_output",
            return_value="local/fork\n",
        ) as check_output:
            self.assertEqual(stage.resolve_github_repo(None), "local/fork")

        self.assertIn("repo", check_output.call_args.args[0])

    def test_resolve_github_repo_falls_back_to_upstream_when_gh_unavailable(
        self,
    ) -> None:
        with mock.patch.object(
            stage.subprocess,
            "check_output",
            side_effect=FileNotFoundError,
        ):
            self.assertEqual(stage.resolve_github_repo(None), stage.DEFAULT_GITHUB_REPO)

    def test_github_repo_can_be_derived_from_workflow_url(self) -> None:
        self.assertEqual(
            stage.github_repo_from_workflow_url(
                "https://github.com/local/fork/actions/runs/12345"
            ),
            "local/fork",
        )

    def test_release_workflow_lookup_uses_selected_repo_and_workflow(self) -> None:
        with mock.patch.object(
            stage.subprocess,
            "check_output",
            return_value='{"url":"https://github.com/local/fork/actions/runs/123","headSha":"abc"}',
        ) as check_output:
            workflow = stage.resolve_release_workflow(
                "1.2.3", "local/fork", ".github/workflows/local.yml"
            )

        self.assertEqual(workflow["headSha"], "abc")
        command = check_output.call_args.args[0]
        self.assertIn("--repo", command)
        self.assertEqual(command[command.index("--repo") + 1], "local/fork")
        self.assertEqual(
            command[command.index("--workflow") + 1], ".github/workflows/local.yml"
        )

    def test_github_actions_download_default_uses_stable_limit(self) -> None:
        with mock.patch.dict(stage.os.environ, {"GITHUB_ACTIONS": "true"}):
            self.assertEqual(
                stage.download_worker_count_for(100),
                stage.DEFAULT_GHA_DOWNLOAD_WORKERS,
            )
            self.assertEqual(stage.download_worker_count_for(100, requested=4), 4)

    def test_list_workflow_artifacts_is_cached_per_repo_and_workflow(self) -> None:
        with mock.patch.object(
            stage.subprocess,
            "check_output",
            return_value="x86_64-unknown-linux-musl\t1024\n",
        ) as check_output:
            first = stage.list_workflow_artifacts("12345", "local/fork")
            second = stage.list_workflow_artifacts("12345", "local/fork")
            third = stage.list_workflow_artifacts("12345", "other/fork")

        self.assertEqual(first, second)
        self.assertEqual(first, third)
        self.assertEqual(first[0].name, "x86_64-unknown-linux-musl")
        self.assertEqual(check_output.call_count, 2)
        self.assertIn(
            "repos/local/fork/actions/runs/12345/artifacts",
            check_output.call_args_list[0].args[0],
        )
        self.assertIn(
            "repos/other/fork/actions/runs/12345/artifacts",
            check_output.call_args_list[1].args[0],
        )

    def test_select_target_artifacts_uses_requested_targets_only(self) -> None:
        artifacts = (
            stage.WorkflowArtifact("x86_64-pc-windows-msvc", 10),
            stage.WorkflowArtifact("x86_64-unknown-linux-musl", 20),
        )
        with mock.patch.object(
            stage,
            "list_workflow_artifacts",
            return_value=artifacts,
        ):
            selected = stage.select_target_artifacts(
                "12345",
                "local/fork",
                [stage.codex_package_component()],
                ["x86_64-pc-windows-msvc"],
            )

        self.assertEqual(selected, [artifacts[0]])

    def test_build_stage_command_uses_target_specific_vendor_src(self) -> None:
        key = (("codex-package",), ("x86_64-unknown-linux-musl",))
        vendor_src = self.root / "vendor-src"
        _pack_output, command = stage.build_stage_command(
            "codex-linux-x64",
            "1.2.3",
            self.root / "dist",
            self.root / "staging",
            {key: vendor_src},
        )

        self.assertIn("--vendor-src", command)
        self.assertEqual(command[command.index("--vendor-src") + 1], str(vendor_src))

    def test_download_artifacts_uses_complete_markers(self) -> None:
        artifacts = (
            stage.WorkflowArtifact("linux", 10),
            stage.WorkflowArtifact("macos", 20),
        )
        calls: list[str] = []

        def fake_check_call(cmd: list[str]) -> None:
            artifact_name = cmd[cmd.index("--name") + 1]
            artifact_dir = Path(cmd[cmd.index("--dir") + 1])
            self.assertEqual(cmd[cmd.index("--repo") + 1], "local/fork")
            artifact_dir.mkdir(parents=True, exist_ok=True)
            (artifact_dir / f"{artifact_name}.txt").write_text(
                artifact_name, encoding="utf-8"
            )
            calls.append(artifact_name)

        with mock.patch.object(stage.subprocess, "check_call", fake_check_call):
            stage.download_artifacts(
                "999", "local/fork", self.root / "artifacts", artifacts, 2
            )
            stage.download_artifacts(
                "999", "local/fork", self.root / "artifacts", artifacts, 2
            )

        self.assertCountEqual(calls, ["linux", "macos"])
        for artifact in artifacts:
            self.assertTrue(
                (self.root / "artifacts" / artifact.name / ".complete").is_file()
            )

    def test_codex_package_archive_extraction_is_reused(self) -> None:
        target = "x86_64-unknown-linux-musl"
        artifact_dir = self.root / "artifacts" / target
        artifact_dir.mkdir(parents=True)
        archive_path = artifact_dir / f"codex-package-{target}.tar.gz"
        payload = self.root / "payload.txt"
        payload.write_text("payload", encoding="utf-8")
        with tarfile.open(archive_path, "w:gz") as archive:
            archive.add(payload, arcname="payload.txt")

        real_tarfile_open = tarfile.open
        opened_archives: list[Path] = []

        def counting_open(*args, **kwargs):
            opened_archives.append(Path(args[0]))
            return real_tarfile_open(*args, **kwargs)

        with mock.patch.object(stage.tarfile, "open", counting_open):
            stage.install_codex_package_archives(
                self.root / "artifacts",
                self.root / "vendor-one",
                [target],
                self.root / "archive-cache",
            )
            stage.install_codex_package_archives(
                self.root / "artifacts",
                self.root / "vendor-two",
                [target],
                self.root / "archive-cache",
            )

        self.assertEqual(opened_archives.count(archive_path), 1)
        self.assertTrue((self.root / "vendor-one" / target / "payload.txt").is_file())
        self.assertTrue((self.root / "vendor-two" / target / "payload.txt").is_file())
        self.assertFalse((self.root / "vendor-one" / target / ".complete").exists())

    def test_existing_vendor_tree_survives_failed_archive_install(self) -> None:
        target = "x86_64-unknown-linux-musl"
        artifact_dir = self.root / "artifacts" / target
        artifact_dir.mkdir(parents=True)
        (artifact_dir / f"codex-package-{target}.tar.gz").write_text(
            "not a tarball", encoding="utf-8"
        )
        existing_payload = self.root / "vendor" / target / "payload.txt"
        existing_payload.parent.mkdir(parents=True)
        existing_payload.write_text("existing", encoding="utf-8")

        with self.assertRaises(tarfile.TarError):
            stage.install_single_codex_package_archive(
                self.root / "artifacts",
                self.root / "vendor",
                target,
            )

        self.assertEqual(existing_payload.read_text(encoding="utf-8"), "existing")
        self.assertEqual(list((self.root / "vendor").glob(f".{target}.*")), [])

    def test_extract_tar_data_rejects_unsafe_legacy_archive_members(self) -> None:
        archive_path = self.root / "unsafe.tar.gz"
        with tarfile.open(archive_path, "w:gz") as archive:
            data = b"unsafe"
            member = tarfile.TarInfo("../escape.txt")
            member.size = len(data)
            archive.addfile(member, io.BytesIO(data))

        with self.assertRaisesRegex(RuntimeError, "unsafe archive member path"):
            with tarfile.open(archive_path, "r:gz") as archive:
                stage.validate_tar_members_for_legacy_python(
                    archive,
                    self.root / "dest",
                )

    def test_extract_tar_data_rejects_legacy_archive_links(self) -> None:
        archive_path = self.root / "link.tar.gz"
        with tarfile.open(archive_path, "w:gz") as archive:
            member = tarfile.TarInfo("payload-link")
            member.type = tarfile.SYMTYPE
            member.linkname = "payload.txt"
            archive.addfile(member)

        with self.assertRaisesRegex(RuntimeError, "archive links require"):
            with tarfile.open(archive_path, "r:gz") as archive:
                stage.validate_tar_members_for_legacy_python(
                    archive,
                    self.root / "dest",
                )

    def test_extract_tar_data_uses_legacy_fallback_when_filter_is_unavailable(
        self,
    ) -> None:
        archive_path = self.root / "payload.tar.gz"
        with tarfile.open(archive_path, "w:gz") as archive:
            data = b"payload"
            member = tarfile.TarInfo("payload.txt")
            member.size = len(data)
            archive.addfile(member, io.BytesIO(data))

        original_extractall = tarfile.TarFile.extractall

        def legacy_extractall(
            self,
            path=".",
            members=None,
            *,
            numeric_owner=False,
            filter=None,
        ):
            if filter is not None:
                raise TypeError("unexpected keyword argument 'filter'")
            return original_extractall(
                self,
                path=path,
                members=members,
                numeric_owner=numeric_owner,
            )

        with mock.patch.object(tarfile.TarFile, "extractall", legacy_extractall):
            stage.extract_tar_data(archive_path, self.root / "dest")

        self.assertEqual(
            (self.root / "dest" / "payload.txt").read_text(encoding="utf-8"),
            "payload",
        )

    def test_cached_tree_materialization_skips_marker(self) -> None:
        cached_dir = self.root / "cached"
        nested_dir = cached_dir / "nested"
        nested_dir.mkdir(parents=True)
        (cached_dir / ".complete").write_text("done", encoding="utf-8")
        (nested_dir / "payload.txt").write_text("payload", encoding="utf-8")

        dest_dir = self.root / "dest"
        stage.materialize_cached_tree(cached_dir, dest_dir, "copy")

        self.assertTrue((dest_dir / "nested" / "payload.txt").is_file())
        self.assertFalse((dest_dir / ".complete").exists())

    def test_bounded_log_preserves_edges(self) -> None:
        text = "a" * 12 + "b" * 12

        result = stage.bounded_log(text, max_chars=10)

        self.assertTrue(result.startswith("aaaaa"))
        self.assertIn("[truncated 14 chars]", result)
        self.assertTrue(result.endswith("bbbbb"))

    def test_extract_zstd_archive_decompresses_in_destination_directory(self) -> None:
        archive_path = self.root / "cache" / "artifact.zst"
        archive_path.parent.mkdir()
        archive_path.write_text("archive", encoding="utf-8")
        dest = self.root / "out" / "codex"
        observed_output: list[Path] = []

        def fake_check_call(cmd: list[str]) -> None:
            output_path = Path(cmd[cmd.index("-o") + 1])
            observed_output.append(output_path)
            output_path.write_text("payload", encoding="utf-8")

        with mock.patch.object(stage.subprocess, "check_call", fake_check_call):
            stage.extract_zstd_archive(archive_path, dest)

        self.assertEqual(dest.read_text(encoding="utf-8"), "payload")
        self.assertEqual(observed_output[0].parent, dest.parent)
        self.assertFalse(observed_output[0].exists())

    def test_stage_packages_returns_results_in_package_order(self) -> None:
        calls: list[tuple[str, bool]] = []

        def fake_stage_package(
            package: str,
            release_version: str,
            output_dir: Path,
            runner_temp: Path,
            vendor_src_by_components: dict[tuple[str, ...], Path],
            keep_staging_dirs: bool,
            *,
            capture_output: bool,
        ) -> stage.StagePackageResult:
            calls.append((package, capture_output))
            return stage.StagePackageResult(
                package=package,
                pack_output=output_dir / f"{package}.tgz",
                log="",
            )

        with mock.patch.object(stage, "stage_package", fake_stage_package):
            results = stage.stage_packages(
                ["codex", "codex-linux-x64"],
                "1.2.3",
                self.root,
                self.root,
                {},
                False,
                2,
            )

        self.assertEqual(
            [result.package for result in results],
            ["codex", "codex-linux-x64"],
        )
        self.assertCountEqual(
            calls,
            [("codex", True), ("codex-linux-x64", True)],
        )


if __name__ == "__main__":
    unittest.main()
