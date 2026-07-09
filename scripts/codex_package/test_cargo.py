#!/usr/bin/env python3

import os
import subprocess
from dataclasses import dataclass
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from codex_package import cargo as cargo_module
from codex_package.cargo import build_source_binaries
from codex_package.cargo import cargo_package_target_dir
from codex_package.cargo import source_binaries_for_target
from codex_package.cargo import source_build_stamp_path
from codex_package.targets import PACKAGE_VARIANTS
from codex_package.targets import TARGET_SPECS


class SourceBinariesForTargetTest(unittest.TestCase):
    def test_macos_package_with_prebuilt_entrypoint_builds_nothing(self) -> None:
        self.assertEqual(
            source_binaries_for_target(
                TARGET_SPECS["aarch64-apple-darwin"],
                PACKAGE_VARIANTS["codex"],
                build_entrypoint=False,
                build_bwrap=False,
                build_codex_command_runner=False,
                build_codex_windows_sandbox_setup=False,
            ),
            [],
        )

    def test_linux_package_with_prebuilt_entrypoint_and_bwrap_builds_nothing(
        self,
    ) -> None:
        self.assertEqual(
            source_binaries_for_target(
                TARGET_SPECS["x86_64-unknown-linux-musl"],
                PACKAGE_VARIANTS["codex"],
                build_entrypoint=False,
                build_bwrap=False,
                build_codex_command_runner=False,
                build_codex_windows_sandbox_setup=False,
            ),
            [],
        )

    def test_windows_package_with_prebuilt_entrypoint_and_helpers_builds_nothing(
        self,
    ) -> None:
        self.assertEqual(
            source_binaries_for_target(
                TARGET_SPECS["x86_64-pc-windows-msvc"],
                PACKAGE_VARIANTS["codex"],
                build_entrypoint=False,
                build_bwrap=False,
                build_codex_command_runner=False,
                build_codex_windows_sandbox_setup=False,
            ),
            [],
        )

    def test_missing_windows_helpers_are_built(self) -> None:
        self.assertEqual(
            source_binaries_for_target(
                TARGET_SPECS["x86_64-pc-windows-msvc"],
                PACKAGE_VARIANTS["codex"],
                build_entrypoint=False,
                build_bwrap=False,
                build_codex_command_runner=True,
                build_codex_windows_sandbox_setup=True,
            ),
            ["codex-command-runner", "codex-windows-sandbox-setup"],
        )

    def test_build_uses_prebuilt_windows_helpers_without_running_cargo(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            entrypoint = touch_file(root / "codex.exe")
            command_runner = touch_file(root / "codex-command-runner.exe")
            sandbox_setup = touch_file(root / "codex-windows-sandbox-setup.exe")

            outputs = build_source_binaries(
                TARGET_SPECS["x86_64-pc-windows-msvc"],
                PACKAGE_VARIANTS["codex"],
                cargo=str(root / "cargo-that-should-not-run"),
                profile="release",
                entrypoint_bin=entrypoint,
                bwrap_bin=None,
                codex_command_runner_bin=command_runner,
                codex_windows_sandbox_setup_bin=sandbox_setup,
            )

        self.assertEqual(outputs.entrypoint_bin, entrypoint)
        self.assertEqual(outputs.codex_command_runner_bin, command_runner)
        self.assertEqual(outputs.codex_windows_sandbox_setup_bin, sandbox_setup)

    def test_package_target_dir_defaults_outside_lane_gc_scope(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            codex_rs = Path(temp_dir) / "codex-rs"
            with mock.patch.object(cargo_module, "CODEX_RS_ROOT", codex_rs):
                with mock.patch.dict(os.environ, {}, clear=True):
                    target_dir = cargo_package_target_dir(
                        TARGET_SPECS["x86_64-pc-windows-msvc"],
                        "release",
                    )

        self.assertEqual(
            target_dir,
            codex_rs / "target" / "package" / "x86_64-pc-windows-msvc-release",
        )

    def test_package_target_dir_ignores_inherited_cargo_target_dir(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            codex_rs = Path(temp_dir) / "codex-rs"
            with mock.patch.object(cargo_module, "CODEX_RS_ROOT", codex_rs):
                with mock.patch.dict(
                    os.environ,
                    {"CARGO_TARGET_DIR": "target/lanes/test-lane"},
                    clear=True,
                ):
                    target_dir = cargo_package_target_dir(
                        TARGET_SPECS["x86_64-pc-windows-msvc"],
                        "release",
                    )

        self.assertEqual(
            target_dir,
            codex_rs / "target" / "package" / "x86_64-pc-windows-msvc-release",
        )

    def test_package_target_dir_honors_package_override(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            codex_rs = Path(temp_dir) / "codex-rs"
            with mock.patch.object(cargo_module, "CODEX_RS_ROOT", codex_rs):
                with mock.patch.dict(
                    os.environ,
                    {"CODEX_PACKAGE_TARGET_DIR": "target/custom-package"},
                    clear=True,
                ):
                    target_dir = cargo_package_target_dir(
                        TARGET_SPECS["x86_64-pc-windows-msvc"],
                        "release",
                    )

        self.assertEqual(target_dir, codex_rs / "target" / "custom-package")

    def test_package_build_sets_fast_env_defaults_and_sccache(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            codex_rs = root / "codex-rs"
            calls: list[SubprocessCall] = []

            def fake_run(cmd, *, cwd, check, env):
                calls.append(
                    SubprocessCall(cmd=list(cmd), cwd=Path(cwd), check=check, env=env)
                )
                write_bins_for_cmd(
                    cmd,
                    env=env,
                    spec=TARGET_SPECS["x86_64-pc-windows-msvc"],
                    profile="release",
                )

            with mock.patch.object(cargo_module, "CODEX_RS_ROOT", codex_rs):
                with mock.patch.dict(
                    os.environ,
                    {"CARGO_TARGET_DIR": "target/lanes/test-lane"},
                    clear=True,
                ):

                    def fake_which(program: str) -> str | None:
                        return {
                            "sccache": "C:/tools/sccache.exe",
                            "lld-link": "C:/LLVM/bin/lld-link.exe",
                        }.get(program)

                    with mock.patch("shutil.which", side_effect=fake_which):
                        with mock.patch("subprocess.run", side_effect=fake_run):
                            build_source_binaries(
                                TARGET_SPECS["x86_64-pc-windows-msvc"],
                                PACKAGE_VARIANTS["codex"],
                                cargo="cargo",
                                profile="release",
                                entrypoint_bin=None,
                                bwrap_bin=None,
                                codex_command_runner_bin=None,
                                codex_windows_sandbox_setup_bin=None,
                            )

        self.assertGreaterEqual(len(calls), 1)
        for call in calls:
            self.assertEqual(call.cwd, codex_rs)
            self.assertTrue(call.check)
            self.assertIn("--locked", call.cmd)
            self.assertEqual(call.env["RUST_MIN_STACK"], "8388608")
            self.assertEqual(call.env["RUSTC_WRAPPER"], "sccache")
            self.assertEqual(
                call.env["SCCACHE_BASEDIR"], str(cargo_module.REPO_ROOT.resolve())
            )
            self.assertEqual(call.env["SCCACHE_CACHE_SIZE"], "80G")
            self.assertNotIn("CARGO_TARGET_DIR", call.env)
            target_dir_arg = call.cmd[call.cmd.index("--target-dir") + 1]
            self.assertEqual(
                Path(target_dir_arg),
                codex_rs / "target" / "package" / "x86_64-pc-windows-msvc-release",
            )
            rustflags = call.env["CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS"]
            self.assertIn("link-arg=/STACK:8388608", rustflags)
            self.assertIn("target-feature=+crt-static", rustflags)
            self.assertEqual(
                call.env["CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"],
                "C:/LLVM/bin/lld-link.exe",
            )

    def test_package_dev_build_keeps_static_crt_out_of_rustflags(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            target_dir = Path(temp_dir) / "target"
            with mock.patch.dict(os.environ, {}, clear=True):
                env = cargo_module.cargo_build_env(
                    TARGET_SPECS["x86_64-pc-windows-msvc"],
                    "dev",
                    target_dir=target_dir,
                )

        self.assertNotIn("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS", env)

    def test_package_build_uses_scoop_lld_link_when_not_on_path(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            user_profile = Path(temp_dir) / "user"
            scoop_lld = (
                user_profile
                / "scoop"
                / "apps"
                / "llvm"
                / "current"
                / "bin"
                / "lld-link.exe"
            )
            scoop_lld.parent.mkdir(parents=True)
            scoop_lld.write_text("", encoding="utf-8")

            with (
                mock.patch.dict(
                    os.environ,
                    {"USERPROFILE": str(user_profile)},
                    clear=True,
                ),
                mock.patch.object(cargo_module.shutil, "which", return_value=None),
                mock.patch.object(
                    cargo_module,
                    "WINDOWS_LLVM_LLD_LINK_DEFAULT",
                    Path(temp_dir) / "missing-lld-link.exe",
                ),
            ):
                env = cargo_module.cargo_build_env(
                    TARGET_SPECS["x86_64-pc-windows-msvc"],
                    "release",
                    target_dir=Path(temp_dir) / "target",
                )

        self.assertEqual(
            env["CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"],
            str(scoop_lld),
        )

    def test_package_build_uses_explicit_scoop_lld_link_root(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            scoop_root = Path(temp_dir) / "custom-scoop"
            scoop_lld = (
                scoop_root / "apps" / "llvm" / "current" / "bin" / "lld-link.exe"
            )
            scoop_lld.parent.mkdir(parents=True)
            scoop_lld.write_text("", encoding="utf-8")

            with (
                mock.patch.dict(
                    os.environ,
                    {"SCOOP": str(scoop_root)},
                    clear=True,
                ),
                mock.patch.object(cargo_module.shutil, "which", return_value=None),
                mock.patch.object(
                    cargo_module,
                    "WINDOWS_LLVM_LLD_LINK_DEFAULT",
                    Path(temp_dir) / "missing-lld-link.exe",
                ),
            ):
                env = cargo_module.cargo_build_env(
                    TARGET_SPECS["x86_64-pc-windows-msvc"],
                    "release",
                    target_dir=Path(temp_dir) / "target",
                )

        self.assertEqual(
            env["CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"],
            str(scoop_lld),
        )

    def test_reuse_existing_source_outputs_with_matching_stamp_skips_cargo(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            codex_rs = root / "codex-rs"
            target_dir = (
                codex_rs / "target" / "package" / "x86_64-pc-windows-msvc-release"
            )
            output_dir = target_dir / "x86_64-pc-windows-msvc" / "release"
            outputs = cargo_module.SourceBuildOutputs(
                entrypoint_bin=touch_file(output_dir / "codex.exe"),
                bwrap_bin=None,
                codex_command_runner_bin=touch_file(
                    output_dir / "codex-command-runner.exe"
                ),
                codex_windows_sandbox_setup_bin=touch_file(
                    output_dir / "codex-windows-sandbox-setup.exe"
                ),
            )
            source = fixed_source_fingerprint()

            with mock.patch.object(cargo_module, "CODEX_RS_ROOT", codex_rs):
                with mock.patch.dict(os.environ, {}, clear=True):
                    with mock.patch.object(
                        cargo_module,
                        "source_tree_fingerprint",
                        return_value=source,
                    ):
                        cargo_module.write_source_build_stamp(
                            target_dir,
                            spec=TARGET_SPECS["x86_64-pc-windows-msvc"],
                            profile="release",
                            variant=PACKAGE_VARIANTS["codex"],
                            outputs=outputs,
                        )
                        with mock.patch("subprocess.run") as run:
                            actual_outputs = build_source_binaries(
                                TARGET_SPECS["x86_64-pc-windows-msvc"],
                                PACKAGE_VARIANTS["codex"],
                                cargo="cargo",
                                profile="release",
                                entrypoint_bin=None,
                                bwrap_bin=None,
                                codex_command_runner_bin=None,
                                codex_windows_sandbox_setup_bin=None,
                                reuse_existing=True,
                            )

        run.assert_not_called()
        self.assertEqual(actual_outputs.entrypoint_bin, output_dir / "codex.exe")

    def test_reuse_existing_source_outputs_with_mismatched_stamp_rebuilds(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            codex_rs = root / "codex-rs"
            target_dir = (
                codex_rs / "target" / "package" / "x86_64-pc-windows-msvc-release"
            )
            output_dir = target_dir / "x86_64-pc-windows-msvc" / "release"
            outputs = cargo_module.SourceBuildOutputs(
                entrypoint_bin=touch_file(output_dir / "codex.exe"),
                bwrap_bin=None,
                codex_command_runner_bin=touch_file(
                    output_dir / "codex-command-runner.exe"
                ),
                codex_windows_sandbox_setup_bin=touch_file(
                    output_dir / "codex-windows-sandbox-setup.exe"
                ),
            )
            calls: list[SubprocessCall] = []

            def fake_run(cmd, *, cwd, check, env):
                calls.append(
                    SubprocessCall(cmd=list(cmd), cwd=Path(cwd), check=check, env=env)
                )
                write_bins_for_cmd(
                    cmd,
                    env=env,
                    spec=TARGET_SPECS["x86_64-pc-windows-msvc"],
                    profile="release",
                )

            with mock.patch.object(cargo_module, "CODEX_RS_ROOT", codex_rs):
                with mock.patch.dict(os.environ, {}, clear=True):
                    with mock.patch.object(
                        cargo_module,
                        "source_tree_fingerprint",
                        return_value=fixed_source_fingerprint(
                            working_tree_sha256="old"
                        ),
                    ):
                        cargo_module.write_source_build_stamp(
                            target_dir,
                            spec=TARGET_SPECS["x86_64-pc-windows-msvc"],
                            profile="release",
                            variant=PACKAGE_VARIANTS["codex"],
                            outputs=outputs,
                        )
                    with mock.patch.object(
                        cargo_module,
                        "source_tree_fingerprint",
                        return_value=fixed_source_fingerprint(
                            working_tree_sha256="new"
                        ),
                    ):
                        with mock.patch("subprocess.run", side_effect=fake_run):
                            build_source_binaries(
                                TARGET_SPECS["x86_64-pc-windows-msvc"],
                                PACKAGE_VARIANTS["codex"],
                                cargo="cargo",
                                profile="release",
                                entrypoint_bin=None,
                                bwrap_bin=None,
                                codex_command_runner_bin=None,
                                codex_windows_sandbox_setup_bin=None,
                                reuse_existing=True,
                            )

        self.assertEqual(len(calls), 1)
        built_bins = [
            calls[0].cmd[index + 1]
            for index, value in enumerate(calls[0].cmd)
            if value == "--bin"
        ]
        self.assertEqual(
            built_bins,
            ["codex", "codex-command-runner", "codex-windows-sandbox-setup"],
        )

    def test_reuse_existing_source_outputs_with_missing_helper_rebuilds_helper_only(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            codex_rs = root / "codex-rs"
            target_dir = (
                codex_rs / "target" / "package" / "x86_64-pc-windows-msvc-release"
            )
            output_dir = target_dir / "x86_64-pc-windows-msvc" / "release"
            outputs = cargo_module.SourceBuildOutputs(
                entrypoint_bin=touch_file(output_dir / "codex.exe"),
                bwrap_bin=None,
                codex_command_runner_bin=touch_file(
                    output_dir / "codex-command-runner.exe"
                ),
                codex_windows_sandbox_setup_bin=touch_file(
                    output_dir / "codex-windows-sandbox-setup.exe"
                ),
            )
            source = fixed_source_fingerprint()
            calls: list[SubprocessCall] = []

            def fake_run(cmd, *, cwd, check, env):
                calls.append(
                    SubprocessCall(cmd=list(cmd), cwd=Path(cwd), check=check, env=env)
                )
                write_bins_for_cmd(
                    cmd,
                    env=env,
                    spec=TARGET_SPECS["x86_64-pc-windows-msvc"],
                    profile="release",
                )

            with mock.patch.object(cargo_module, "CODEX_RS_ROOT", codex_rs):
                with mock.patch.dict(os.environ, {}, clear=True):
                    with mock.patch.object(
                        cargo_module,
                        "source_tree_fingerprint",
                        return_value=source,
                    ):
                        cargo_module.write_source_build_stamp(
                            target_dir,
                            spec=TARGET_SPECS["x86_64-pc-windows-msvc"],
                            profile="release",
                            variant=PACKAGE_VARIANTS["codex"],
                            outputs=outputs,
                        )
                        outputs.codex_command_runner_bin.unlink()
                        with mock.patch("subprocess.run", side_effect=fake_run):
                            build_source_binaries(
                                TARGET_SPECS["x86_64-pc-windows-msvc"],
                                PACKAGE_VARIANTS["codex"],
                                cargo="cargo",
                                profile="release",
                                entrypoint_bin=None,
                                bwrap_bin=None,
                                codex_command_runner_bin=None,
                                codex_windows_sandbox_setup_bin=None,
                                reuse_existing=True,
                            )

        self.assertEqual(len(calls), 1)
        built_bins = [
            calls[0].cmd[index + 1]
            for index, value in enumerate(calls[0].cmd)
            if value == "--bin"
        ]
        self.assertEqual(built_bins, ["codex-command-runner"])

    def test_reuse_existing_source_outputs_skips_source_fingerprint_when_all_outputs_miss(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            codex_rs = root / "codex-rs"
            target_dir = (
                codex_rs / "target" / "package" / "x86_64-pc-windows-msvc-release"
            )
            output_dir = target_dir / "x86_64-pc-windows-msvc" / "release"
            outputs = cargo_module.SourceBuildOutputs(
                entrypoint_bin=touch_file(output_dir / "codex.exe"),
                bwrap_bin=None,
                codex_command_runner_bin=touch_file(
                    output_dir / "codex-command-runner.exe"
                ),
                codex_windows_sandbox_setup_bin=touch_file(
                    output_dir / "codex-windows-sandbox-setup.exe"
                ),
            )

            with mock.patch.object(cargo_module, "CODEX_RS_ROOT", codex_rs):
                with mock.patch.object(
                    cargo_module,
                    "source_tree_fingerprint",
                    return_value=fixed_source_fingerprint(),
                ):
                    cargo_module.write_source_build_stamp(
                        target_dir,
                        spec=TARGET_SPECS["x86_64-pc-windows-msvc"],
                        profile="release",
                        variant=PACKAGE_VARIANTS["codex"],
                        outputs=outputs,
                    )
                outputs.entrypoint_bin.unlink()
                outputs.codex_command_runner_bin.unlink()
                outputs.codex_windows_sandbox_setup_bin.unlink()
                with mock.patch.object(
                    cargo_module,
                    "source_tree_fingerprint",
                    side_effect=AssertionError("source fingerprint should be skipped"),
                ):
                    missing = cargo_module.binaries_missing_for_reuse(
                        [
                            "codex",
                            "codex-command-runner",
                            "codex-windows-sandbox-setup",
                        ],
                        outputs=outputs,
                        variant=PACKAGE_VARIANTS["codex"],
                        target_dir=target_dir,
                        spec=TARGET_SPECS["x86_64-pc-windows-msvc"],
                        profile="release",
                        reuse_existing=True,
                        force_rebuild=False,
                    )

        self.assertEqual(
            missing,
            ["codex", "codex-command-runner", "codex-windows-sandbox-setup"],
        )

    def test_stamp_mismatch_skips_source_fingerprint_until_outputs_match(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            target_dir = root / "target"
            output_dir = target_dir / "x86_64-pc-windows-msvc" / "release"
            outputs = cargo_module.SourceBuildOutputs(
                entrypoint_bin=touch_file(output_dir / "codex.exe"),
                bwrap_bin=None,
                codex_command_runner_bin=touch_file(
                    output_dir / "codex-command-runner.exe"
                ),
                codex_windows_sandbox_setup_bin=touch_file(
                    output_dir / "codex-windows-sandbox-setup.exe"
                ),
            )
            with mock.patch.object(
                cargo_module,
                "source_tree_fingerprint",
                return_value=fixed_source_fingerprint(),
            ):
                cargo_module.write_source_build_stamp(
                    target_dir,
                    spec=TARGET_SPECS["x86_64-pc-windows-msvc"],
                    profile="release",
                    variant=PACKAGE_VARIANTS["codex"],
                    outputs=outputs,
                )
            outputs.entrypoint_bin.unlink()
            with mock.patch.object(
                cargo_module,
                "source_tree_fingerprint",
                side_effect=AssertionError("source fingerprint should be skipped"),
            ):
                matched = cargo_module.source_build_stamp_matches(
                    target_dir,
                    spec=TARGET_SPECS["x86_64-pc-windows-msvc"],
                    profile="release",
                    variant=PACKAGE_VARIANTS["codex"],
                    outputs=outputs,
                )

        self.assertFalse(matched)

    def test_source_output_match_uses_stamp_metadata_without_rehashing(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = touch_file(Path(temp_dir) / "codex.exe")
            fingerprint = cargo_module.source_output_fingerprint(path)

            with mock.patch.object(
                cargo_module,
                "source_output_fingerprint",
                side_effect=AssertionError("output hash should be cached"),
            ):
                matched = cargo_module.source_output_matches_fingerprint(
                    path,
                    fingerprint,
                )

        self.assertTrue(matched)

    def test_force_rebuild_ignores_reusable_outputs(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            codex_rs = root / "codex-rs"
            output_dir = (
                codex_rs
                / "target"
                / "package"
                / "x86_64-pc-windows-msvc-release"
                / "x86_64-pc-windows-msvc"
                / "release"
            )
            touch_file(output_dir / "codex.exe")
            touch_file(output_dir / "codex-command-runner.exe")
            touch_file(output_dir / "codex-windows-sandbox-setup.exe")

            def fake_run(cmd, *, cwd, check, env):
                write_bins_for_cmd(
                    cmd,
                    env=env,
                    spec=TARGET_SPECS["x86_64-pc-windows-msvc"],
                    profile="release",
                )

            with mock.patch.object(cargo_module, "CODEX_RS_ROOT", codex_rs):
                with mock.patch.dict(os.environ, {}, clear=True):
                    with mock.patch("subprocess.run", side_effect=fake_run) as run:
                        build_source_binaries(
                            TARGET_SPECS["x86_64-pc-windows-msvc"],
                            PACKAGE_VARIANTS["codex"],
                            cargo="cargo",
                            profile="release",
                            entrypoint_bin=None,
                            bwrap_bin=None,
                            codex_command_runner_bin=None,
                            codex_windows_sandbox_setup_bin=None,
                            reuse_existing=True,
                            force_rebuild=True,
                        )

        self.assertGreater(run.call_count, 0)

    def test_entrypoint_and_windows_helpers_build_in_one_cargo_invocation(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            codex_rs = root / "codex-rs"
            calls: list[SubprocessCall] = []

            def fake_run(cmd, *, cwd, check, env):
                calls.append(
                    SubprocessCall(cmd=list(cmd), cwd=Path(cwd), check=check, env=env)
                )
                write_bins_for_cmd(
                    cmd,
                    env=env,
                    spec=TARGET_SPECS["x86_64-pc-windows-msvc"],
                    profile="release",
                )

            with mock.patch.object(cargo_module, "CODEX_RS_ROOT", codex_rs):
                with mock.patch.dict(os.environ, {}, clear=True):
                    with mock.patch.object(
                        cargo_module,
                        "resolve_codex_v8_cargo_env",
                        return_value={"CODEX_V8_ARCHIVE": "v8.tar.gz"},
                    ):
                        with mock.patch("subprocess.run", side_effect=fake_run):
                            build_source_binaries(
                                TARGET_SPECS["x86_64-pc-windows-msvc"],
                                PACKAGE_VARIANTS["codex"],
                                cargo="cargo",
                                profile="release",
                                entrypoint_bin=None,
                                bwrap_bin=None,
                                codex_command_runner_bin=None,
                                codex_windows_sandbox_setup_bin=None,
                            )

            self.assertEqual(len(calls), 1)
            self.assertIn("codex", calls[0].cmd)
            self.assertIn("CODEX_V8_ARCHIVE", calls[0].env)
            self.assertIn("codex-command-runner", calls[0].cmd)
            self.assertIn("codex-windows-sandbox-setup", calls[0].cmd)
            self.assertTrue(
                source_build_stamp_path(
                    codex_rs / "target" / "package" / "x86_64-pc-windows-msvc-release"
                ).is_file()
            )

    def test_reused_entrypoint_helpers_build_without_v8_env(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            codex_rs = root / "codex-rs"
            entrypoint = touch_file(root / "prebuilt" / "codex.exe")
            calls: list[SubprocessCall] = []

            def fake_run(cmd, *, cwd, check, env):
                calls.append(
                    SubprocessCall(cmd=list(cmd), cwd=Path(cwd), check=check, env=env)
                )
                write_bins_for_cmd(
                    cmd,
                    env=env,
                    spec=TARGET_SPECS["x86_64-pc-windows-msvc"],
                    profile="release",
                )

            with mock.patch.object(cargo_module, "CODEX_RS_ROOT", codex_rs):
                with mock.patch.dict(os.environ, {}, clear=True):
                    with mock.patch.object(
                        cargo_module,
                        "resolve_codex_v8_cargo_env",
                        return_value={"CODEX_V8_ARCHIVE": "v8.tar.gz"},
                    ):
                        with mock.patch("subprocess.run", side_effect=fake_run):
                            build_source_binaries(
                                TARGET_SPECS["x86_64-pc-windows-msvc"],
                                PACKAGE_VARIANTS["codex"],
                                cargo="cargo",
                                profile="release",
                                entrypoint_bin=entrypoint,
                                bwrap_bin=None,
                                codex_command_runner_bin=None,
                                codex_windows_sandbox_setup_bin=None,
                            )

        self.assertEqual(len(calls), 1)
        self.assertNotIn("codex", calls[0].cmd)
        self.assertNotIn("CODEX_V8_ARCHIVE", calls[0].env)
        self.assertIn("codex-command-runner", calls[0].cmd)
        self.assertIn("codex-windows-sandbox-setup", calls[0].cmd)

    def test_cargo_success_without_expected_binary_fails_validation(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            codex_rs = Path(temp_dir) / "codex-rs"

            with mock.patch.object(cargo_module, "CODEX_RS_ROOT", codex_rs):
                with mock.patch.dict(os.environ, {}, clear=True):
                    with mock.patch("subprocess.run", return_value=None):
                        with self.assertRaisesRegex(
                            RuntimeError,
                            "cargo build did not produce expected binary",
                        ):
                            build_source_binaries(
                                TARGET_SPECS["x86_64-pc-windows-msvc"],
                                PACKAGE_VARIANTS["codex"],
                                cargo="cargo",
                                profile="release",
                                entrypoint_bin=None,
                                bwrap_bin=None,
                                codex_command_runner_bin=None,
                                codex_windows_sandbox_setup_bin=None,
                            )

    def test_cargo_failure_names_build_context(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            codex_rs = Path(temp_dir) / "codex-rs"

            def fake_run(cmd, *, cwd, check, env):
                raise subprocess.CalledProcessError(101, cmd)

            with mock.patch.object(cargo_module, "CODEX_RS_ROOT", codex_rs):
                with mock.patch.dict(os.environ, {}, clear=True):
                    with mock.patch("subprocess.run", side_effect=fake_run):
                        with self.assertRaisesRegex(
                            RuntimeError,
                            "bins=codex,codex-command-runner,"
                            "codex-windows-sandbox-setup "
                            ".*target=x86_64-pc-windows-msvc "
                            ".*profile=release .*exit_code=101",
                        ):
                            build_source_binaries(
                                TARGET_SPECS["x86_64-pc-windows-msvc"],
                                PACKAGE_VARIANTS["codex"],
                                cargo="cargo",
                                profile="release",
                                entrypoint_bin=None,
                                bwrap_bin=None,
                                codex_command_runner_bin=None,
                                codex_windows_sandbox_setup_bin=None,
                            )

    def test_invalid_explicit_output_path_fails_before_cargo(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            missing = Path(temp_dir) / "missing-codex.exe"

            with mock.patch("subprocess.run") as run:
                with self.assertRaisesRegex(RuntimeError, "prebuilt entrypoint"):
                    build_source_binaries(
                        TARGET_SPECS["x86_64-pc-windows-msvc"],
                        PACKAGE_VARIANTS["codex"],
                        cargo="cargo",
                        profile="release",
                        entrypoint_bin=missing,
                        bwrap_bin=None,
                        codex_command_runner_bin=None,
                        codex_windows_sandbox_setup_bin=None,
                    )

        run.assert_not_called()


class SetSccacheEnvTest(unittest.TestCase):
    def test_cache_size_defaults_and_honors_override(self) -> None:
        env: dict[str, str] = {}
        cargo_module.set_sccache_env(env)
        self.assertEqual(env["SCCACHE_CACHE_SIZE"], "80G")

        env = {"CODEX_SCCACHE_CACHE_SIZE": "100G"}
        cargo_module.set_sccache_env(env)
        self.assertEqual(env["SCCACHE_CACHE_SIZE"], "100G")

        env = {"CODEX_SCCACHE_CACHE_SIZE": "   "}
        cargo_module.set_sccache_env(env)
        self.assertEqual(env["SCCACHE_CACHE_SIZE"], "80G")


@dataclass(frozen=True)
class SubprocessCall:
    cmd: list[str]
    cwd: Path
    check: bool
    env: dict[str, str]


def write_bins_for_cmd(
    cmd: list[str],
    *,
    env: dict[str, str],
    spec,
    profile: str,
) -> None:
    profile_dir = "release" if profile == "release" else profile
    output_dir = Path(cmd[cmd.index("--target-dir") + 1]) / spec.target / profile_dir
    bins = [cmd[index + 1] for index, value in enumerate(cmd) if value == "--bin"]
    names = {
        "codex": "codex.exe",
        "codex-command-runner": "codex-command-runner.exe",
        "codex-windows-sandbox-setup": "codex-windows-sandbox-setup.exe",
    }
    for binary in bins:
        touch_file(output_dir / names[binary])


def touch_file(path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("", encoding="utf-8")
    return path.resolve()


def fixed_source_fingerprint(
    *,
    working_tree_sha256: str = "dirty",
) -> dict[str, str]:
    return {
        "status": "ok",
        "git_head": "0123456789abcdef",
        "index_tree": "fedcba9876543210",
        "working_tree_sha256": working_tree_sha256,
        "untracked_names_sha256": "untracked",
    }


if __name__ == "__main__":
    unittest.main()
