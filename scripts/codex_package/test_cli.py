#!/usr/bin/env python3

from pathlib import Path
import json
import hashlib
import os
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
from contextvars import Context

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from codex_package import cargo as cargo_module
from codex_package import cli
from codex_package.cargo import SourceBuildOutputs
from codex_package.test_layout import write_pe


class CliEntrypointTest(unittest.TestCase):
    def test_help_from_any_cwd_with_or_without_safe_path(self) -> None:
        repo = Path(__file__).resolve().parents[2]
        script = repo / "scripts" / "build_codex_package.py"
        with tempfile.TemporaryDirectory() as directory:
            for cwd in (repo, Path(directory)):
                for safe_path in (False, True):
                    with self.subTest(cwd=cwd, safe_path=safe_path):
                        env = dict(os.environ)
                        env.pop("PYTHONPATH", None)
                        env.pop("PYTHONSAFEPATH", None)
                        if safe_path:
                            env["PYTHONSAFEPATH"] = "1"
                        result = subprocess.run(
                            [sys.executable, "-B", str(script), "--help"],
                            cwd=cwd,
                            env=env,
                            text=True,
                            capture_output=True,
                            timeout=15,
                            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
                        )
                        self.assertEqual(result.returncode, 0, result.stderr)
                        self.assertIn("--target", result.stdout)
                        self.assertIn("--package-dir", result.stdout)


class CliPerformanceFlagsTest(unittest.TestCase):
    def setUp(self) -> None:
        fingerprint = mock.patch.object(
            cli, "source_tree_fingerprint", return_value={"status": "test"}
        )
        fingerprint.start()
        self.addCleanup(fingerprint.stop)

    def test_archive_compression_defaults_to_fast(self) -> None:
        with mock.patch("sys.argv", ["codex_package"]):
            args = cli.parse_args()

        self.assertEqual(args.archive_compression, "fast")

    def test_release_resource_flags_are_parsed(self) -> None:
        with mock.patch(
            "sys.argv",
            [
                "codex_package",
                "--code-mode-host-bin",
                "codex-code-mode-host",
            ],
        ):
            args = cli.parse_args()

        self.assertEqual(args.code_mode_host_bin, Path("codex-code-mode-host"))

    def test_skip_build_if_present_uses_existing_outputs_and_reuses_archive_entries(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            package_dir.mkdir()
            target_dir = root / "target" / "x86_64-pc-windows-msvc" / "debug"
            target_dir.mkdir(parents=True)
            for name in [
                "codex.exe",
                "codex-code-mode-host.exe",
                "codex-command-runner.exe",
                "codex-windows-sandbox-setup.exe",
                "rg.exe",
            ]:
                path = target_dir / name
                write_pe(path)
                path.chmod(0o755)
            archive_a = root / "a.zip"
            archive_b = root / "b.zip"
            archive_entries = [package_dir / "bin" / "codex.exe"]

            with (
                mock.patch.object(
                    cli,
                    "parse_args",
                    return_value=cli.argparse.Namespace(
                        target="x86_64-pc-windows-msvc",
                        variant="codex",
                        package_dir=package_dir,
                        archive_output=[archive_a, archive_b],
                        force=True,
                        cargo="cargo",
                        cargo_profile="release",
                        release_version="1.2.3",
                        entrypoint_bin=None,
                        code_mode_host_bin=None,
                        codex_command_runner_bin=None,
                        codex_windows_sandbox_setup_bin=None,
                        rg_bin=target_dir / "rg.exe",
                        skip_build_if_present=True,
                        skip_validate=False,
                        reuse_package_dir=True,
                        archive_compression="fast",
                        timings=True,
                    ),
                ),
                mock.patch.object(
                    cli, "cargo_profile_output_dir", return_value=target_dir
                ),
                mock.patch.object(
                    cli, "build_source_binaries"
                ) as build_source_binaries,
                mock.patch.object(
                    cli, "prepare_package_dir", side_effect=create_staged_package_dir
                ) as prepare_package_dir,
                mock.patch.object(cli, "build_package_dir") as build_package_dir,
                mock.patch.object(cli, "validate_package_dir") as validate_package_dir,
                mock.patch.object(
                    cli, "package_entries", return_value=archive_entries
                ) as entries,
                mock.patch.object(
                    cli,
                    "write_archive",
                    side_effect=create_staged_archive,
                ) as write_archive,
                mock.patch.object(cli, "read_workspace_version", return_value="1.2.3"),
                mock.patch.object(
                    cli, "resolve_rg_bin", return_value=target_dir / "rg.exe"
                ) as resolve_rg_bin,
                mock.patch.object(cli, "source_build_stamp_matches", return_value=True),
            ):
                rc = cli.main()

            self.assertEqual(rc, 0)
            build_source_binaries.assert_not_called()
            staged_package_dir = prepare_package_dir.call_args.args[0]
            prepare_package_dir.assert_called_once_with(
                staged_package_dir, force=True, reuse=True
            )
            build_package_dir.assert_called_once()
            validate_package_dir.assert_called_once_with(
                staged_package_dir,
                cli.PACKAGE_VARIANTS["codex"],
                cli.TARGET_SPECS["x86_64-pc-windows-msvc"],
                expected_version="1.2.3",
            )
            entries.assert_called_once_with(staged_package_dir)
            resolve_rg_bin.assert_called_once_with(
                cli.TARGET_SPECS["x86_64-pc-windows-msvc"],
                target_dir / "rg.exe",
            )
            self.assertEqual(write_archive.call_count, 1)
            for call in write_archive.call_args_list:
                self.assertEqual(call.kwargs["entries"], archive_entries)
                self.assertEqual(call.kwargs["compression"], "fast")
            self.assertEqual(archive_a.read_bytes(), b"archive")
            self.assertEqual(archive_b.read_bytes(), b"archive")
            self.assertTrue(package_dir.is_dir())
            self.assertFalse(staged_package_dir.exists())

    def test_without_skip_build_delegates_to_cargo_builder(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            out = root / "out"
            out.mkdir()
            outputs = SourceBuildOutputs(
                entrypoint_bin=out / "codex",
                code_mode_host_bin=out / "codex-code-mode-host",
                codex_command_runner_bin=None,
                codex_windows_sandbox_setup_bin=None,
            )
            write_pe(outputs.entrypoint_bin)
            outputs.entrypoint_bin.chmod(0o755)
            write_pe(outputs.code_mode_host_bin)
            outputs.code_mode_host_bin.chmod(0o755)
            rg = out / "rg"
            write_pe(rg)
            rg.chmod(0o755)

            with (
                mock.patch.object(
                    cli,
                    "parse_args",
                    return_value=cli.argparse.Namespace(
                        target="x86_64-pc-windows-msvc",
                        variant="codex",
                        package_dir=package_dir,
                        archive_output=[],
                        force=False,
                        cargo="cargo",
                        cargo_profile="debug",
                        entrypoint_bin=None,
                        code_mode_host_bin=None,
                        codex_command_runner_bin=None,
                        codex_windows_sandbox_setup_bin=None,
                        rg_bin=out / "rg",
                        skip_build_if_present=False,
                        skip_validate=True,
                        fast_validate=False,
                        reuse_package_dir=False,
                        archive_compression="default",
                        timings=False,
                    ),
                ),
                mock.patch.object(
                    cli, "build_source_binaries", return_value=outputs
                ) as build,
                mock.patch.object(
                    cli,
                    "prepare_package_dir",
                    side_effect=create_staged_package_dir,
                ),
                mock.patch.object(cli, "build_package_dir"),
                mock.patch.object(cli, "validate_package_dir") as validate,
                mock.patch.object(cli, "read_workspace_version", return_value="1.2.3"),
                mock.patch.object(cli, "resolve_rg_bin", return_value=out / "rg"),
            ):
                rc = cli.main()

            self.assertEqual(rc, 0)
            build.assert_called_once()
            self.assertIsNone(build.call_args.kwargs["codex_command_runner_bin"])
            self.assertIsNone(build.call_args.kwargs["codex_windows_sandbox_setup_bin"])
            validate.assert_not_called()

    def test_app_server_variant_is_forwarded_to_build_and_layout(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            out = root / "out"
            out.mkdir()
            outputs = SourceBuildOutputs(
                entrypoint_bin=out / "codex-app-server",
                code_mode_host_bin=out / "codex-code-mode-host",
                codex_command_runner_bin=None,
                codex_windows_sandbox_setup_bin=None,
            )
            write_pe(outputs.entrypoint_bin)
            outputs.entrypoint_bin.chmod(0o755)
            write_pe(outputs.code_mode_host_bin)
            outputs.code_mode_host_bin.chmod(0o755)
            rg = out / "rg"
            write_pe(rg)
            rg.chmod(0o755)

            with (
                mock.patch.object(
                    cli,
                    "parse_args",
                    return_value=cli.argparse.Namespace(
                        target="x86_64-pc-windows-msvc",
                        variant="codex-app-server",
                        package_dir=package_dir,
                        archive_output=[],
                        force=False,
                        cargo="cargo",
                        cargo_profile="debug",
                        entrypoint_bin=None,
                        code_mode_host_bin=None,
                        codex_command_runner_bin=None,
                        codex_windows_sandbox_setup_bin=None,
                        rg_bin=out / "rg",
                        skip_build_if_present=False,
                        skip_validate=True,
                        fast_validate=False,
                        reuse_package_dir=False,
                        archive_compression="default",
                        timings=False,
                    ),
                ),
                mock.patch.object(
                    cli, "build_source_binaries", return_value=outputs
                ) as build,
                mock.patch.object(
                    cli,
                    "prepare_package_dir",
                    side_effect=create_staged_package_dir,
                ),
                mock.patch.object(cli, "build_package_dir") as build_package_dir,
                mock.patch.object(cli, "read_workspace_version", return_value="1.2.3"),
                mock.patch.object(cli, "resolve_rg_bin", return_value=out / "rg"),
            ):
                rc = cli.main()

            self.assertEqual(rc, 0)
            variant = cli.PACKAGE_VARIANTS["codex-app-server"]
            self.assertIs(build.call_args.args[1], variant)
            self.assertIs(build_package_dir.call_args.args[2], variant)

    def test_reuse_and_force_rebuild_flags_are_forwarded_to_cargo_builder(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            out = root / "out"
            out.mkdir()
            outputs = SourceBuildOutputs(
                entrypoint_bin=out / "codex.exe",
                code_mode_host_bin=out / "codex-code-mode-host.exe",
                codex_command_runner_bin=out / "codex-command-runner.exe",
                codex_windows_sandbox_setup_bin=out / "codex-windows-sandbox-setup.exe",
            )
            for path in [
                outputs.entrypoint_bin,
                outputs.code_mode_host_bin,
                outputs.codex_command_runner_bin,
                outputs.codex_windows_sandbox_setup_bin,
                out / "rg.exe",
            ]:
                write_pe(path)

            with (
                mock.patch.object(
                    cli,
                    "parse_args",
                    return_value=cli.argparse.Namespace(
                        target="x86_64-pc-windows-msvc",
                        variant="codex",
                        package_dir=package_dir,
                        archive_output=[],
                        force=True,
                        cargo="cargo",
                        cargo_profile="release",
                        entrypoint_bin=None,
                        code_mode_host_bin=None,
                        codex_command_runner_bin=None,
                        codex_windows_sandbox_setup_bin=None,
                        rg_bin=out / "rg.exe",
                        reuse_source_builds=True,
                        skip_build_if_present=False,
                        force_source_rebuild=True,
                        skip_validate=True,
                        fast_validate=False,
                        reuse_package_dir=False,
                        archive_compression="fast",
                        timings=False,
                    ),
                ),
                mock.patch.object(
                    cli, "build_source_binaries", return_value=outputs
                ) as build,
                mock.patch.object(
                    cli,
                    "prepare_package_dir",
                    side_effect=create_staged_package_dir,
                ),
                mock.patch.object(cli, "build_package_dir"),
                mock.patch.object(cli, "read_workspace_version", return_value="1.2.3"),
                mock.patch.object(cli, "resolve_rg_bin", return_value=out / "rg.exe"),
            ):
                rc = cli.main()

            self.assertEqual(rc, 0)
            build.assert_called_once()
            self.assertTrue(build.call_args.kwargs["reuse_existing"])
            self.assertTrue(build.call_args.kwargs["force_rebuild"])


class CliPreflightTest(unittest.TestCase):
    def test_ripgrep_role_alias_fails_before_build_lease_or_inputs(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            rg = root / "rg.exe"
            write_pe(rg)
            alias = root / "alias.exe"
            alias.hardlink_to(rg)
            for name in SourceBuildOutputs.__dataclass_fields__:
                for path in (rg, alias):
                    with self.subTest(role=name, path=path):
                        args = request_args(
                            target="x86_64-pc-windows-msvc", variant="codex",
                            package_dir=root / "package", rg_bin=rg, **{name: path},
                        )
                        with (
                            mock.patch.object(cli, "parse_args", return_value=args),
                            mock.patch.object(cli, "package_build_lease") as lease,
                            mock.patch.object(cli, "resolve_package_inputs") as inputs,
                            self.assertRaisesRegex(RuntimeError, "distinct executables"),
                        ):
                            cli.main()
                        lease.assert_not_called()
                        inputs.assert_not_called()
                        self.assertFalse((root / "package").exists())

    def test_non_pe_ripgrep_fails_before_starting_cargo(self):
        with tempfile.TemporaryDirectory() as temp:
            rg = Path(temp) / "rg.exe"
            rg.write_text("not a Windows executable")
            with mock.patch.object(cli, "resolve_source_outputs") as source:
                with self.assertRaisesRegex(
                    RuntimeError, "Invalid PE executable for ripgrep"
                ):
                    cli.resolve_package_inputs(
                        request_args(rg_bin=rg),
                        cli.TARGET_SPECS["x86_64-pc-windows-msvc"],
                        cli.PACKAGE_VARIANTS["codex"],
                    )
                source.assert_not_called()

    def test_build_lease_protects_staging_but_not_archive_generation(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            spec = cli.TARGET_SPECS["x86_64-pc-windows-msvc"]
            files = [root / f"input-{i}.exe" for i in range(5)]
            for path in files:
                write_pe(path)
            inputs = cli.PackageInputs(*files)
            archive = root / "package.zip"
            args = request_args(
                target=spec.target,
                variant="codex",
                package_dir=root / "package",
                cargo="cargo",
                archive_output=[archive],
                rg_bin=files[2],
            )

            def acquire():
                with cargo_module.package_build_lease(spec, "release"):
                    pass

            def build(staging, *args, **kwargs):
                with self.assertRaisesRegex(RuntimeError, "already locked"):
                    Context().run(acquire)
                (staging / "bin").mkdir()
                (staging / "bin" / "snapshot").write_bytes(b"independent snapshot")

            def archive_snapshot(staging, archive_path, **kwargs):
                Context().run(acquire)
                self.assertEqual(
                    (staging / "bin" / "snapshot").read_bytes(), b"independent snapshot"
                )
                create_staged_archive(staging, archive_path)

            with (
                mock.patch.object(cli, "parse_args", return_value=args),
                mock.patch.object(
                    cargo_module,
                    "cargo_package_target_dir",
                    return_value=root / "target",
                ),
                mock.patch.object(
                    cli, "resolve_package_inputs", return_value=("1.2.3", inputs)
                ),
                mock.patch.object(
                    cli, "source_tree_fingerprint", return_value={"status": "test"}
                ),
                mock.patch.object(cli, "build_package_dir", side_effect=build),
                mock.patch.object(cli, "validate_package_dir"),
                mock.patch.object(cli, "write_archive", side_effect=archive_snapshot),
            ):
                self.assertEqual(cli.main(), 0)
            self.assertEqual(archive.read_bytes(), b"archive")

    def test_release_manifests_use_installer_asset_name_and_digest(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            release_dir = root / "release"
            package_dir.mkdir()
            archive_path = release_dir / "codex-package-x86_64-pc-windows-msvc.tar.gz"
            release_dir.mkdir()
            archive_path.write_bytes(b"archive")
            other_name = "codex-package-aarch64-pc-windows-msvc.tar.gz"
            (release_dir / "codex-package_SHA256SUMS").write_text(
                f"{'0' * 64}  {archive_path.name}\n{'b' * 64}  {other_name}\n",
                encoding="utf-8",
            )
            (package_dir / "codex-package.json").write_text(
                json.dumps(
                    {
                        "version": "1.2.3",
                        "target": "x86_64-pc-windows-msvc",
                        "bundleId": "a" * 64,
                        "buildIdentity": {"source": "test"},
                    }
                ),
                encoding="utf-8",
            )

            cli.write_release_manifests(release_dir, package_dir, [archive_path])

            checksums = (release_dir / "codex-package_SHA256SUMS").read_text()
            self.assertEqual(
                checksums,
                f"{'b' * 64}  {other_name}\n"
                f"{hashlib.sha256(b'archive').hexdigest()}  {archive_path.name}\n",
            )
            provenance = json.loads(
                (
                    release_dir / "codex-package_x86_64-pc-windows-msvc_PROVENANCE.json"
                ).read_text()
            )
            self.assertEqual(provenance["version"], "1.2.3")
            self.assertEqual(provenance["target"], "x86_64-pc-windows-msvc")
            self.assertEqual(provenance["bundleId"], "a" * 64)
            self.assertEqual(provenance["buildIdentity"], {"source": "test"})
            self.assertEqual(
                provenance["artifacts"],
                [
                    {
                        "name": archive_path.name,
                        "size": 7,
                        "sha256": hashlib.sha256(b"archive").hexdigest(),
                    }
                ],
            )

    def test_strict_release_semver(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            package = Path(temp_dir) / "package"
            for version in [
                "01.2.3",
                "1.02.3",
                "1.2.03",
                "1.2.3-alpha..1",
                "1.2.3-.",
                "1.2.3-01",
                "1.2.3+foo..bar",
            ]:
                with (
                    self.subTest(version=version),
                    self.assertRaisesRegex(RuntimeError, "semantic version"),
                ):
                    cli.validate_cli_request(
                        request_args(release_version=version),
                        cli.TARGET_SPECS["x86_64-pc-windows-msvc"],
                        package,
                    )
            for version in ["0.0.0", "1.2.3-alpha.1", "1.2.3-0", "1.2.3-01a+001.build"]:
                with self.subTest(version=version):
                    args = request_args(
                        target="x86_64-pc-windows-msvc",
                        variant="codex",
                        package_dir=package,
                        release_version=version,
                    )
                    with (
                        mock.patch.object(cli, "parse_args", return_value=args),
                        mock.patch.object(
                            cli,
                            "resolve_package_inputs",
                            side_effect=RuntimeError("valid request reached inputs"),
                        ) as inputs,
                    ):
                        with self.assertRaisesRegex(
                            RuntimeError, "valid request reached inputs"
                        ):
                            cli.main()
                        inputs.assert_called_once()

    def test_cheap_input_failures_do_not_start_cargo(self) -> None:
        spec = cli.TARGET_SPECS["x86_64-pc-windows-msvc"]
        with tempfile.TemporaryDirectory() as temp_dir:
            for failure in ["version", "ripgrep", "zstd"]:
                with self.subTest(failure=failure):
                    root = Path(temp_dir)
                    args = request_args(
                        target=spec.target,
                        variant="codex",
                        package_dir=root / "package",
                        rg_bin=None,
                        release_version=None if failure == "version" else "1.2.3",
                    )
                    if failure == "ripgrep":
                        args.rg_bin = root / "missing-rg.exe"
                    if failure == "zstd":
                        args.archive_output = [root / "out.tar.zst"]
                    with (
                        mock.patch.object(cli, "parse_args", return_value=args),
                        mock.patch.object(
                            cli,
                            "read_workspace_version",
                            side_effect=RuntimeError("invalid version"),
                        ),
                        mock.patch.object(
                            cli,
                            "resolve_zstd_command",
                            side_effect=RuntimeError("missing zstd"),
                        ),
                        mock.patch.object(cli, "resolve_source_outputs") as build,
                    ):
                        with self.assertRaisesRegex(
                            RuntimeError,
                            {
                                "version": "invalid version",
                                "ripgrep": "ripgrep executable does not exist",
                                "zstd": "missing zstd",
                            }[failure],
                        ):
                            cli.main()
                        build.assert_not_called()

    def test_failed_validation_preserves_previous_package_without_copying_it(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package = root / "package"
            package.mkdir()
            (package / "old").write_bytes(b"previous")
            args = request_args(
                target="x86_64-pc-windows-msvc",
                variant="codex",
                package_dir=package,
                reuse_package_dir=True,
            )

            def build(staging, *args, **kwargs):
                self.assertEqual(list(staging.iterdir()), [])
                (staging / "new").write_bytes(b"new")

            with (
                mock.patch.object(cli, "parse_args", return_value=args),
                mock.patch.object(
                    cli, "resolve_package_inputs", return_value=("1.2.3", mock.Mock())
                ),
                mock.patch.object(cli, "validate_package_input_roles"),
                mock.patch.object(
                    cli, "source_tree_fingerprint", return_value={"status": "test"}
                ),
                mock.patch.object(cli, "build_package_dir", side_effect=build),
                mock.patch.object(
                    cli,
                    "validate_package_dir",
                    side_effect=RuntimeError("validation failed"),
                ),
                self.assertRaisesRegex(RuntimeError, "validation failed"),
            ):
                cli.main()
            self.assertEqual((package / "old").read_bytes(), b"previous")
            self.assertEqual(
                [
                    path
                    for path in root.iterdir()
                    if not path.name.endswith(".publish.lock")
                ],
                [package],
            )

    def test_non_force_package_preserves_destination_created_during_staging(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            package = Path(temp_dir) / "package"
            with self.assertRaisesRegex(RuntimeError, "not empty"):
                with cli.staged_package_destination(
                    package, reuse_existing=False
                ) as staging:
                    staging.mkdir()
                    (staging / "new").write_bytes(b"new")
                    package.mkdir()
                    (package / "other").write_bytes(b"other writer")
            self.assertEqual((package / "other").read_bytes(), b"other writer")
            self.assertFalse((package / "new").exists())

    def test_second_archive_activation_failure_restores_previous_outputs(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            outputs = [root / "first.zip", root / "second.zip"]
            for path in outputs:
                path.write_bytes(path.name.encode())
            activate = cli.activate_archive

            def fail_second(staging, dest, *, force):
                if dest == outputs[1]:
                    raise OSError("second activation failed")
                activate(staging, dest, force=force)

            with (
                mock.patch.object(
                    cli, "write_archive", side_effect=create_staged_archive
                ),
                mock.patch.object(cli, "activate_archive", side_effect=fail_second),
                self.assertRaisesRegex(OSError, "second activation failed"),
            ):
                cli.write_archives_atomically(
                    root / "package",
                    outputs,
                    force=True,
                    entries=[],
                    compression="fast",
                )
            for path in outputs:
                self.assertEqual(path.read_bytes(), path.name.encode())
            self.assertEqual(set(root.iterdir()), set(outputs))

    def test_skip_build_rejects_ignored_source_override(self) -> None:
        args = request_args(
            skip_build_if_present=True,
            code_mode_host_bin=Path("codex-code-mode-host.exe"),
        )

        with self.assertRaisesRegex(RuntimeError, "--code-mode-host-bin"):
            cli.validate_cli_request(
                args,
                cli.TARGET_SPECS["x86_64-pc-windows-msvc"],
                Path("package"),
            )

    def test_duplicate_archive_output_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            output = root / "package.zip"
            args = request_args(archive_output=[output, root / "." / "package.zip"])

            with self.assertRaisesRegex(RuntimeError, "more than once"):
                cli.validate_cli_request(
                    args,
                    cli.TARGET_SPECS["x86_64-pc-windows-msvc"],
                    root / "package",
                )

    def test_main_rejects_invalid_archive_before_resolving_inputs(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            args = request_args(
                target="x86_64-pc-windows-msvc",
                variant="codex",
                package_dir=root / "package",
                archive_output=[root / "package.tar.gz"],
                archive_compression="none",
            )

            with (
                mock.patch.object(cli, "parse_args", return_value=args),
                mock.patch.object(
                    cli,
                    "resolve_package_inputs",
                    side_effect=AssertionError("inputs should not be resolved"),
                ),
                self.assertRaisesRegex(RuntimeError, "compression 'none'"),
            ):
                cli.main()


def create_staged_package_dir(path: Path, **_kwargs) -> None:
    path.mkdir(parents=True, exist_ok=True)


def create_staged_archive(_package_dir: Path, archive_path: Path, **_kwargs) -> None:
    archive_path.write_bytes(b"archive")


def request_args(**overrides) -> cli.argparse.Namespace:
    values = {
        "force": False,
        "reuse_package_dir": False,
        "archive_output": [],
        "archive_compression": "fast",
        "cargo_profile": "release",
        "release_version": "1.2.3",
        "codex_command_runner_bin": None,
        "codex_windows_sandbox_setup_bin": None,
        "skip_build_if_present": False,
        "reuse_source_builds": False,
        "force_source_rebuild": False,
        "entrypoint_bin": None,
        "code_mode_host_bin": None,
    }
    values.update(overrides)
    return cli.argparse.Namespace(**values)


if __name__ == "__main__":
    unittest.main()
