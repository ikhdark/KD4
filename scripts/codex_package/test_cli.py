#!/usr/bin/env python3

from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from codex_package import cli
from codex_package.cargo import SourceBuildOutputs


class CliPerformanceFlagsTest(unittest.TestCase):
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
                "--zsh-manifest",
                "standalone-zsh",
            ],
        ):
            args = cli.parse_args()

        self.assertEqual(args.code_mode_host_bin, Path("codex-code-mode-host"))
        self.assertEqual(args.zsh_manifest, Path("standalone-zsh"))

    def test_zsh_manifest_is_forwarded_to_resolver(self) -> None:
        spec = cli.TARGET_SPECS["x86_64-unknown-linux-musl"]
        variant = cli.PACKAGE_VARIANTS["codex"]
        manifest = Path("standalone-zsh")
        outputs = SourceBuildOutputs(
            entrypoint_bin=Path("codex"),
            code_mode_host_bin=Path("codex-code-mode-host"),
            bwrap_bin=Path("bwrap"),
            codex_command_runner_bin=None,
            codex_windows_sandbox_setup_bin=None,
        )
        args = cli.argparse.Namespace(
            rg_bin=None,
            zsh_bin=None,
            zsh_manifest=manifest,
        )

        with (
            mock.patch.object(cli, "resolve_source_outputs", return_value=outputs),
            mock.patch.object(cli, "read_workspace_version", return_value="1.2.3"),
            mock.patch.object(cli, "resolve_rg_bin", return_value=Path("rg")),
            mock.patch.object(
                cli,
                "resolve_zsh_bin",
                return_value=Path("zsh"),
            ) as resolve_zsh,
        ):
            version, inputs = cli.resolve_package_inputs(args, spec, variant)

        self.assertEqual(version, "1.2.3")
        self.assertEqual(inputs.zsh_bin, Path("zsh"))
        resolve_zsh.assert_called_once_with(
            spec,
            None,
            manifest_path=manifest,
        )

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
                path.write_text("bin", encoding="utf-8")
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
                        cargo_profile="debug",
                        entrypoint_bin=None,
                        code_mode_host_bin=None,
                        bwrap_bin=None,
                        codex_command_runner_bin=None,
                        codex_windows_sandbox_setup_bin=None,
                        rg_bin=target_dir / "rg.exe",
                        zsh_bin=None,
                        zsh_manifest=None,
                        skip_build_if_present=True,
                        skip_validate=False,
                        fast_validate=True,
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
                mock.patch.object(cli, "prepare_package_dir") as prepare_package_dir,
                mock.patch.object(cli, "build_package_dir") as build_package_dir,
                mock.patch.object(cli, "validate_package_dir") as validate_package_dir,
                mock.patch.object(
                    cli, "package_entries", return_value=archive_entries
                ) as entries,
                mock.patch.object(cli, "write_archive") as write_archive,
                mock.patch.object(cli, "read_workspace_version", return_value="1.2.3"),
                mock.patch.object(
                    cli, "resolve_rg_bin", return_value=target_dir / "rg.exe"
                ) as resolve_rg_bin,
                mock.patch.object(cli, "resolve_zsh_bin", return_value=None),
            ):
                rc = cli.main()

            self.assertEqual(rc, 0)
            build_source_binaries.assert_not_called()
            prepare_package_dir.assert_called_once_with(
                package_dir, force=True, reuse=True
            )
            build_package_dir.assert_called_once()
            validate_package_dir.assert_called_once_with(
                package_dir,
                cli.PACKAGE_VARIANTS["codex"],
                cli.TARGET_SPECS["x86_64-pc-windows-msvc"],
                expected_version="1.2.3",
                include_zsh=False,
                fast=True,
            )
            entries.assert_called_once_with(package_dir)
            resolve_rg_bin.assert_called_once_with(
                cli.TARGET_SPECS["x86_64-pc-windows-msvc"],
                target_dir / "rg.exe",
            )
            self.assertEqual(write_archive.call_count, 2)
            for call in write_archive.call_args_list:
                self.assertEqual(call.kwargs["entries"], archive_entries)
                self.assertEqual(call.kwargs["compression"], "fast")

    def test_without_skip_build_delegates_to_cargo_builder(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            out = root / "out"
            out.mkdir()
            outputs = SourceBuildOutputs(
                entrypoint_bin=out / "codex",
                code_mode_host_bin=out / "codex-code-mode-host",
                bwrap_bin=None,
                codex_command_runner_bin=None,
                codex_windows_sandbox_setup_bin=None,
            )
            outputs.entrypoint_bin.write_text("bin", encoding="utf-8")
            outputs.entrypoint_bin.chmod(0o755)
            outputs.code_mode_host_bin.write_text("host", encoding="utf-8")
            outputs.code_mode_host_bin.chmod(0o755)

            with (
                mock.patch.object(
                    cli,
                    "parse_args",
                    return_value=cli.argparse.Namespace(
                        target="x86_64-apple-darwin",
                        variant="codex",
                        package_dir=package_dir,
                        archive_output=[],
                        force=False,
                        cargo="cargo",
                        cargo_profile="debug",
                        entrypoint_bin=None,
                        code_mode_host_bin=None,
                        bwrap_bin=None,
                        codex_command_runner_bin=None,
                        codex_windows_sandbox_setup_bin=None,
                        rg_bin=out / "rg",
                        zsh_bin=None,
                        zsh_manifest=None,
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
                mock.patch.object(cli, "prepare_package_dir"),
                mock.patch.object(cli, "build_package_dir"),
                mock.patch.object(cli, "validate_package_dir") as validate,
                mock.patch.object(cli, "read_workspace_version", return_value="1.2.3"),
                mock.patch.object(cli, "resolve_rg_bin", return_value=out / "rg"),
                mock.patch.object(cli, "resolve_zsh_bin", return_value=None),
            ):
                rc = cli.main()

            self.assertEqual(rc, 0)
            build.assert_called_once()
            self.assertIsNone(build.call_args.kwargs["bwrap_bin"])
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
                bwrap_bin=None,
                codex_command_runner_bin=None,
                codex_windows_sandbox_setup_bin=None,
            )
            outputs.entrypoint_bin.write_text("bin", encoding="utf-8")
            outputs.entrypoint_bin.chmod(0o755)
            outputs.code_mode_host_bin.write_text("host", encoding="utf-8")
            outputs.code_mode_host_bin.chmod(0o755)

            with (
                mock.patch.object(
                    cli,
                    "parse_args",
                    return_value=cli.argparse.Namespace(
                        target="x86_64-apple-darwin",
                        variant="codex-app-server",
                        package_dir=package_dir,
                        archive_output=[],
                        force=False,
                        cargo="cargo",
                        cargo_profile="debug",
                        entrypoint_bin=None,
                        code_mode_host_bin=None,
                        bwrap_bin=None,
                        codex_command_runner_bin=None,
                        codex_windows_sandbox_setup_bin=None,
                        rg_bin=out / "rg",
                        zsh_bin=None,
                        zsh_manifest=None,
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
                mock.patch.object(cli, "prepare_package_dir"),
                mock.patch.object(cli, "build_package_dir") as build_package_dir,
                mock.patch.object(cli, "read_workspace_version", return_value="1.2.3"),
                mock.patch.object(cli, "resolve_rg_bin", return_value=out / "rg"),
                mock.patch.object(cli, "resolve_zsh_bin", return_value=None),
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
                bwrap_bin=None,
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
                path.write_text("bin", encoding="utf-8")

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
                        bwrap_bin=None,
                        codex_command_runner_bin=None,
                        codex_windows_sandbox_setup_bin=None,
                        rg_bin=out / "rg.exe",
                        zsh_bin=None,
                        zsh_manifest=None,
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
                mock.patch.object(cli, "prepare_package_dir"),
                mock.patch.object(cli, "build_package_dir"),
                mock.patch.object(cli, "read_workspace_version", return_value="1.2.3"),
                mock.patch.object(cli, "resolve_rg_bin", return_value=out / "rg.exe"),
                mock.patch.object(cli, "resolve_zsh_bin", return_value=None),
            ):
                rc = cli.main()

            self.assertEqual(rc, 0)
            build.assert_called_once()
            self.assertTrue(build.call_args.kwargs["reuse_existing"])
            self.assertTrue(build.call_args.kwargs["force_rebuild"])


class CliPreflightTest(unittest.TestCase):
    def test_rejects_platform_incompatible_helper_override(self) -> None:
        args = request_args(bwrap_bin=Path("bwrap"))

        with self.assertRaisesRegex(RuntimeError, "only supported for Linux"):
            cli.validate_cli_request(
                args,
                cli.TARGET_SPECS["x86_64-apple-darwin"],
                Path("package"),
            )

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


def request_args(**overrides) -> cli.argparse.Namespace:
    values = {
        "force": False,
        "reuse_package_dir": False,
        "archive_output": [],
        "archive_compression": "fast",
        "zsh_bin": None,
        "zsh_manifest": None,
        "bwrap_bin": None,
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
