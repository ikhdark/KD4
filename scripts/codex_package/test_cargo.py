#!/usr/bin/env python3

import os
import json
import struct
import subprocess
from contextlib import chdir, contextmanager
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
    @contextmanager
    def package_fixture(self):
        spec = TARGET_SPECS["x86_64-pc-windows-msvc"]
        variant = PACKAGE_VARIANTS["codex"]
        kwargs = dict(
            cargo="cargo",
            profile="release",
            entrypoint_bin=None,
            code_mode_host_bin=None,
            codex_command_runner_bin=None,
            codex_windows_sandbox_setup_bin=None,
            reuse_existing=True,
        )

        def compile(cmd, *, cwd, check, env):
            write_bins_for_cmd(cmd, env=env, spec=spec, profile="release")

        with (
            tempfile.TemporaryDirectory() as temp,
            mock.patch.object(cargo_module, "CODEX_RS_ROOT", Path(temp)),
            mock.patch.dict(os.environ, {}, clear=True),
            mock.patch.object(
                cargo_module,
                "source_tree_fingerprint",
                return_value=fixed_source_fingerprint(),
            ) as source,
            mock.patch.object(
                cargo_module,
                "build_recipe_fingerprint",
                wraps=cargo_module.build_recipe_fingerprint,
            ) as recipe,
            mock.patch.object(
                cargo_module.subprocess, "run", side_effect=compile
            ) as run,
        ):
            yield spec, variant, kwargs, run, source, recipe

    def test_compile_environment_changes_rebuild_through_package_entrypoint(self):
        for name in (
            "CODEX_BUILD_COMMIT",
            "CODEX_BUILD_DIRTY",
            "CODEX_BUILD_PROFILE",
            "CODEX_BUILD_TIMESTAMP",
            "CFLAGS",
            "CXXFLAGS",
            "CARGO_INCREMENTAL",
            "CFLAGS_x86_64_pc_windows_msvc",
            "BUILD_SCRIPT_CUSTOM_INPUT",
        ):
            with self.subTest(name=name), self.package_fixture() as fixture:
                spec, variant, kwargs, run, _, _ = fixture
                os.environ[name] = "first"
                outputs = build_source_binaries(spec, variant, **kwargs)
                self.assertTrue(outputs.entrypoint_bin.is_file())
                build_source_binaries(spec, variant, **kwargs)
                self.assertEqual(run.call_count, 1)
                os.environ[name] = "second"
                build_source_binaries(spec, variant, **kwargs)
                self.assertEqual(run.call_count, 2)
                env_name = name.upper() if os.name == "nt" else name
                self.assertEqual(run.call_args.kwargs["env"][env_name], "second")
                cmd = run.call_args.args[0]
                self.assertEqual(
                    [cmd[i + 1] for i, arg in enumerate(cmd) if arg == "--bin"],
                    [
                        "codex",
                        "codex-code-mode-host",
                        "codex-command-runner",
                        "codex-windows-sandbox-setup",
                    ],
                )

    def test_public_reuse_observes_inputs_once_and_build_observes_before_and_after(
        self,
    ):
        with self.package_fixture() as fixture:
            spec, variant, kwargs, run, source, recipe = fixture
            compile = run.side_effect

            def compile_after_observation(*args, **kw):
                self.assertEqual(source.call_count, 1)
                self.assertEqual(recipe.call_count, 1)
                compile(*args, **kw)

            run.side_effect = compile_after_observation
            outputs = build_source_binaries(spec, variant, **kwargs)
            self.assertTrue(outputs.entrypoint_bin.is_file())
            self.assertEqual(source.call_count, 2)
            self.assertEqual(recipe.call_count, 2)
            source.reset_mock()
            recipe.reset_mock()
            self.assertEqual(build_source_binaries(spec, variant, **kwargs), outputs)
            self.assertEqual(run.call_count, 1)
            self.assertEqual(source.call_count, 1)
            self.assertEqual(recipe.call_count, 1)
            # A prior stamp with no remaining outputs must not trigger extra
            # discovery for reuse before the required build observations.
            for path in vars(outputs).values():
                path.unlink()
            source.reset_mock()
            recipe.reset_mock()
            self.assertEqual(build_source_binaries(spec, variant, **kwargs), outputs)
            self.assertTrue(outputs.entrypoint_bin.is_file())
            self.assertEqual(run.call_count, 2)
            self.assertEqual(source.call_count, 2)
            self.assertEqual(recipe.call_count, 2)

    def test_helper_build_cannot_certify_a_changed_reused_entrypoint(self):
        with self.package_fixture() as fixture:
            spec, variant, kwargs, run, _, _ = fixture
            outputs = build_source_binaries(spec, variant, **kwargs)
            outputs.codex_command_runner_bin.unlink()
            compile = run.side_effect

            def mutate_entrypoint(*args, **kw):
                compile(*args, **kw)
                path = outputs.entrypoint_bin
                stat = path.stat()
                contents = bytearray(path.read_bytes())
                contents[-1] ^= 1
                path.write_bytes(contents)
                os.utime(path, ns=(stat.st_atime_ns, stat.st_mtime_ns))

            run.side_effect = mutate_entrypoint
            with self.assertRaisesRegex(RuntimeError, "reused output changed"):
                build_source_binaries(spec, variant, **kwargs)
            self.assertFalse(
                source_build_stamp_path(
                    cargo_package_target_dir(spec, "release")
                ).exists()
            )
            cmd = run.call_args.args[0]
            self.assertEqual(
                [cmd[i + 1] for i, arg in enumerate(cmd) if arg == "--bin"],
                ["codex-command-runner"],
            )
            run.side_effect = compile
            build_source_binaries(spec, variant, **kwargs)
            cmd = run.call_args.args[0]
            self.assertIn("codex", cmd)
            self.assertEqual(run.call_count, 3)

    def test_explicit_and_default_output_collisions_fail_before_cargo(self):
        for hardlink in (False, True):
            with self.subTest(hardlink=hardlink), self.package_fixture() as fixture:
                spec, variant, kwargs, run, _, _ = fixture
                target = cargo_package_target_dir(spec, "release")
                host = touch_file(
                    target / spec.target / "release" / "codex-code-mode-host.exe"
                )
                entry = host
                if hardlink:
                    entry = host.parent / "explicit.exe"
                    os.link(host, entry)
                kwargs["entrypoint_bin"] = entry
                before = entry.read_bytes()
                with self.assertRaisesRegex(RuntimeError, "distinct executables"):
                    build_source_binaries(spec, variant, **kwargs)
                run.assert_not_called()
                self.assertEqual(entry.read_bytes(), before)
                self.assertFalse(source_build_stamp_path(target).exists())

    def test_package_preserves_explicit_empty_wrapper_with_sccache_available(self):
        with self.package_fixture() as fixture:
            spec, variant, kwargs, run, _, _ = fixture
            os.environ["RUSTC_WRAPPER"] = ""
            with mock.patch.object(
                cargo_module.shutil, "which", return_value="sccache"
            ):
                outputs = build_source_binaries(spec, variant, **kwargs)
            self.assertTrue(outputs.entrypoint_bin.is_file())
            self.assertEqual(run.call_count, 1)
            self.assertEqual(run.call_args.kwargs["env"]["RUSTC_WRAPPER"], "")
            self.assertNotIn("SCCACHE_BASEDIR", run.call_args.kwargs["env"])

    def test_unavailable_recipe_evidence_runs_cargo_without_reuse_stamp(self):
        with self.package_fixture() as fixture:
            spec, variant, kwargs, run, _, _ = fixture
            build_source_binaries(spec, variant, **kwargs)
            with mock.patch.object(
                cargo_module, "command_identity", return_value={"status": "unavailable"}
            ):
                for _ in range(2):
                    outputs = build_source_binaries(spec, variant, **kwargs)
                    self.assertTrue(outputs.entrypoint_bin.is_file())
                    self.assertFalse(
                        source_build_stamp_path(
                            cargo_package_target_dir(spec, "release")
                        ).exists()
                    )
            self.assertEqual(run.call_count, 3)

    def test_recipe_change_during_build_fails_current_package(self):
        with self.package_fixture() as fixture:
            spec, variant, kwargs, run, _, _ = fixture
            config = cargo_module.CODEX_RS_ROOT / ".cargo" / "config.toml"
            config.parent.mkdir()
            config.write_text("[build]\njobs=1\n")
            compile = run.side_effect

            def change_config(*args, **kw):
                compile(*args, **kw)
                config.write_text("[build]\njobs=2\n")

            run.side_effect = change_config
            with self.assertRaisesRegex(RuntimeError, "inputs changed during build"):
                build_source_binaries(spec, variant, **kwargs)
            self.assertEqual(run.call_count, 1)
            self.assertFalse(
                source_build_stamp_path(
                    cargo_package_target_dir(spec, "release")
                ).exists()
            )

    def test_edit_during_build_cannot_stamp_old_outputs_as_current(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "repo"
            source_root = root / "codex-rs"
            source_root.mkdir(parents=True)
            source = source_root / "main.rs"
            source.write_text("old source", encoding="utf-8")
            (source_root / ".gitignore").write_text("target/\n", encoding="utf-8")
            for args in (
                ["init", "-q"],
                ["add", "."],
                [
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.com",
                    "commit",
                    "-qm",
                    "fixture",
                ],
            ):
                subprocess.run(
                    ["git", *args], cwd=root, check=True, capture_output=True
                )
            spec = TARGET_SPECS["x86_64-pc-windows-msvc"]
            variant = PACKAGE_VARIANTS["codex"]
            consumed = []

            def compile_source(*args, target_dir, **kwargs):
                consumed.append(source.read_text(encoding="utf-8"))
                output_dir = target_dir / spec.target / "release"
                for name in (
                    "codex",
                    "codex-code-mode-host",
                    "codex-command-runner",
                    "codex-windows-sandbox-setup",
                ):
                    path = output_dir / f"{name}.exe"
                    touch_file(path)
                    path.write_bytes(path.read_bytes() + consumed[-1].encode())
                source.write_text("new source", encoding="utf-8")

            with (
                mock.patch.object(cargo_module, "REPO_ROOT", root),
                mock.patch.object(cargo_module, "CODEX_RS_ROOT", source_root),
                mock.patch.dict(
                    os.environ, {"PATH": os.environ.get("PATH", "")}, clear=True
                ),
                mock.patch.object(
                    cargo_module, "run_cargo_build", side_effect=compile_source
                ) as build,
            ):
                kwargs = {
                    "cargo": "cargo",
                    "profile": "release",
                    "entrypoint_bin": None,
                    "code_mode_host_bin": None,
                    "codex_command_runner_bin": None,
                    "codex_windows_sandbox_setup_bin": None,
                    "reuse_existing": True,
                }
                with self.assertRaisesRegex(
                    RuntimeError, "inputs changed during build"
                ):
                    build_source_binaries(spec, variant, **kwargs)
                self.assertIsNone(
                    cargo_module.read_source_build_stamp(
                        cargo_package_target_dir(spec, "release")
                    )
                )
                outputs = build_source_binaries(spec, variant, **kwargs)
                build_source_binaries(spec, variant, **kwargs)
                self.assertEqual(build.call_count, 2)
                self.assertEqual(consumed, ["old source", "new source"])
                self.assertTrue(
                    outputs.entrypoint_bin.read_bytes().endswith(b"new source")
                )

    def test_external_cargo_config_changes_invalidate_recipe(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            cwd = root / "parent" / "repo" / "codex-rs"
            cwd.mkdir(parents=True)
            spec = TARGET_SPECS["x86_64-pc-windows-msvc"]
            for home_env in ({"CARGO_HOME": str(root / "cargo-home")}, {}):
                with (
                    self.subTest(env=home_env),
                    mock.patch.object(cargo_module, "CODEX_RS_ROOT", cwd),
                    mock.patch.object(Path, "home", return_value=root / "user"),
                ):
                    home = Path(home_env.get("CARGO_HOME", root / "user" / ".cargo"))
                    for config in (
                        home / "config.toml",
                        root / "parent" / ".cargo" / "config",
                    ):
                        config.parent.mkdir(parents=True, exist_ok=True)
                        config.write_text(
                            '[build]\nrustflags=["--cfg=first"]\n', encoding="utf-8"
                        )
                        first = cargo_module.build_recipe_fingerprint(
                            spec=spec, profile="release", build_env=home_env
                        )
                        config.write_text(
                            '[build]\nrustflags=["--cfg=second"]\n', encoding="utf-8"
                        )
                        second = cargo_module.build_recipe_fingerprint(
                            spec=spec, profile="release", build_env=home_env
                        )
                        self.assertNotEqual(first, second)

    def setUp(self) -> None:
        home = tempfile.TemporaryDirectory()
        self.addCleanup(home.cleanup)
        home_patch = mock.patch.object(Path, "home", return_value=Path(home.name))
        home_patch.start()
        self.addCleanup(home_patch.stop)
        command_identity = mock.patch.object(
            cargo_module,
            "command_identity",
            side_effect=lambda command, *_args, **_kwargs: {
                "path": command,
                "version": f"{command} test",
            },
        )
        command_identity.start()
        self.addCleanup(command_identity.stop)

    def test_windows_package_with_prebuilt_entrypoint_and_helpers_builds_nothing(
        self,
    ) -> None:
        self.assertEqual(
            source_binaries_for_target(
                TARGET_SPECS["x86_64-pc-windows-msvc"],
                PACKAGE_VARIANTS["codex"],
                build_entrypoint=False,
                build_code_mode_host=False,
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
                build_code_mode_host=False,
                build_codex_command_runner=True,
                build_codex_windows_sandbox_setup=True,
            ),
            ["codex-command-runner", "codex-windows-sandbox-setup"],
        )

    def test_missing_code_mode_host_is_built_for_every_variant(self) -> None:
        for variant in PACKAGE_VARIANTS.values():
            with self.subTest(variant=variant.name):
                self.assertEqual(
                    source_binaries_for_target(
                        TARGET_SPECS["aarch64-pc-windows-msvc"],
                        variant,
                        build_entrypoint=False,
                        build_code_mode_host=True,
                        build_codex_command_runner=False,
                        build_codex_windows_sandbox_setup=False,
                    ),
                    ["codex-code-mode-host"],
                )

    def test_build_uses_prebuilt_windows_helpers_without_running_cargo(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            entrypoint = touch_file(root / "codex.exe")
            code_mode_host = touch_file(root / "codex-code-mode-host.exe")
            command_runner = touch_file(root / "codex-command-runner.exe")
            sandbox_setup = touch_file(root / "codex-windows-sandbox-setup.exe")

            outputs = build_source_binaries(
                TARGET_SPECS["x86_64-pc-windows-msvc"],
                PACKAGE_VARIANTS["codex"],
                cargo=str(root / "cargo-that-should-not-run"),
                profile="release",
                entrypoint_bin=entrypoint,
                code_mode_host_bin=code_mode_host,
                codex_command_runner_bin=command_runner,
                codex_windows_sandbox_setup_bin=sandbox_setup,
            )

        self.assertEqual(outputs.entrypoint_bin, entrypoint)
        self.assertEqual(outputs.code_mode_host_bin, code_mode_host)
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
                                code_mode_host_bin=None,
                                codex_command_runner_bin=None,
                                codex_windows_sandbox_setup_bin=None,
                                release_version="1.2.3",
                            )

        self.assertGreaterEqual(len(calls), 1)
        for call in calls:
            self.assertEqual(call.cwd, codex_rs)
            self.assertTrue(call.check)
            self.assertIn("--locked", call.cmd)
            self.assertEqual(call.env["RUST_MIN_STACK"], "8388608")
            self.assertEqual(call.env["CODEX_RELEASE_VERSION"], "1.2.3")
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
            self.assertNotIn("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS", call.env)
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

    def test_prebuilt_pe_machine_must_match_requested_target(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            entrypoint = write_pe(root / "codex.exe", 0x8664)
            code_mode_host = write_pe(root / "codex-code-mode-host.exe", 0xAA64)
            command_runner = write_pe(root / "codex-command-runner.exe", 0x8664)
            sandbox_setup = write_pe(root / "codex-windows-sandbox-setup.exe", 0x8664)

            with self.assertRaisesRegex(RuntimeError, "code-mode-host target mismatch"):
                build_source_binaries(
                    TARGET_SPECS["x86_64-pc-windows-msvc"],
                    PACKAGE_VARIANTS["codex"],
                    cargo="cargo",
                    profile="release",
                    entrypoint_bin=entrypoint,
                    code_mode_host_bin=code_mode_host,
                    codex_command_runner_bin=command_runner,
                    codex_windows_sandbox_setup_bin=sandbox_setup,
                )

    def test_release_version_is_hashed_into_source_reuse_identity(self) -> None:
        with mock.patch.dict(os.environ, {}, clear=True):
            first = cargo_module.build_recipe_fingerprint(
                spec=TARGET_SPECS["x86_64-pc-windows-msvc"],
                profile="release",
                release_version="1.2.3",
            )
            second = cargo_module.build_recipe_fingerprint(
                spec=TARGET_SPECS["x86_64-pc-windows-msvc"],
                profile="release",
                release_version="1.2.4",
            )

        self.assertNotEqual(first, second)
        self.assertNotIn("1.2.3", json.dumps(first))

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
                code_mode_host_bin=touch_file(output_dir / "codex-code-mode-host.exe"),
                codex_command_runner_bin=touch_file(
                    output_dir / "codex-command-runner.exe"
                ),
                codex_windows_sandbox_setup_bin=touch_file(
                    output_dir / "codex-windows-sandbox-setup.exe"
                ),
            )
            source = fixed_source_fingerprint()

            with (
                mock.patch.object(cargo_module, "CODEX_RS_ROOT", codex_rs),
                mock.patch.dict(os.environ, {}, clear=True),
                mock.patch.object(
                    cargo_module,
                    "source_tree_fingerprint",
                    return_value=source,
                ),
            ):
                cargo_module.write_source_build_stamp(
                    target_dir,
                    spec=TARGET_SPECS["x86_64-pc-windows-msvc"],
                    profile="release",
                    variant=PACKAGE_VARIANTS["codex"],
                    outputs=outputs,
                    cargo="custom-cargo",
                )
                with mock.patch("subprocess.run") as run:
                    actual_outputs = build_source_binaries(
                        TARGET_SPECS["x86_64-pc-windows-msvc"],
                        PACKAGE_VARIANTS["codex"],
                        cargo="custom-cargo",
                        profile="release",
                        entrypoint_bin=None,
                        code_mode_host_bin=None,
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
                code_mode_host_bin=touch_file(output_dir / "codex-code-mode-host.exe"),
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
                                code_mode_host_bin=None,
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
            [
                "codex",
                "codex-code-mode-host",
                "codex-command-runner",
                "codex-windows-sandbox-setup",
            ],
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
                code_mode_host_bin=touch_file(output_dir / "codex-code-mode-host.exe"),
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
                                code_mode_host_bin=None,
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

    def test_reuse_skips_source_fingerprint_when_every_requested_output_misses(
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
                code_mode_host_bin=touch_file(output_dir / "codex-code-mode-host.exe"),
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
            [
                "codex",
                "codex-command-runner",
                "codex-windows-sandbox-setup",
            ],
        )

    def test_stamp_mismatch_skips_source_fingerprint_until_outputs_match(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            target_dir = root / "target"
            output_dir = target_dir / "x86_64-pc-windows-msvc" / "release"
            outputs = cargo_module.SourceBuildOutputs(
                entrypoint_bin=touch_file(output_dir / "codex.exe"),
                code_mode_host_bin=touch_file(output_dir / "codex-code-mode-host.exe"),
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

    def test_source_output_match_rejects_metadata_preserving_corruption(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = touch_file(Path(temp_dir) / "codex.exe")
            fingerprint = cargo_module.source_output_fingerprint(path)
            before = path.stat()
            path.write_bytes(b"x" * before.st_size)
            os.utime(path, ns=(before.st_atime_ns, before.st_mtime_ns))
            self.assertFalse(
                cargo_module.source_output_matches_fingerprint(path, fingerprint)
            )

    def test_source_build_stamp_write_preserves_existing_file_on_replace_failure(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            target_dir = Path(temp_dir) / "target"
            stamp_path = source_build_stamp_path(target_dir)
            stamp_path.parent.mkdir(parents=True)
            stamp_path.write_text("previous stamp\n", encoding="utf-8")
            outputs = cargo_module.SourceBuildOutputs(
                entrypoint_bin=touch_file(Path(temp_dir) / "codex.exe"),
                code_mode_host_bin=touch_file(
                    Path(temp_dir) / "codex-code-mode-host.exe"
                ),
                codex_command_runner_bin=None,
                codex_windows_sandbox_setup_bin=None,
            )

            with (
                mock.patch.object(
                    cargo_module,
                    "source_tree_fingerprint",
                    return_value=fixed_source_fingerprint(),
                ),
                mock.patch.object(
                    cargo_module.os,
                    "fsync",
                    wraps=os.fsync,
                ) as fsync,
                mock.patch.object(
                    cargo_module.os,
                    "replace",
                    side_effect=OSError("replace failed"),
                ) as replace,
                self.assertRaisesRegex(OSError, "replace failed"),
            ):
                cargo_module.write_source_build_stamp(
                    target_dir,
                    spec=TARGET_SPECS["x86_64-pc-windows-msvc"],
                    profile="release",
                    variant=PACKAGE_VARIANTS["codex"],
                    outputs=outputs,
                )

            temporary_path, replacement_path = replace.call_args.args
            self.assertEqual(Path(temporary_path).parent, stamp_path.parent)
            self.assertEqual(replacement_path, stamp_path)
            self.assertEqual(stamp_path.read_text(encoding="utf-8"), "previous stamp\n")
            self.assertFalse(Path(temporary_path).exists())
            fsync.assert_called_once()

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
            touch_file(output_dir / "codex-code-mode-host.exe")
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
                            code_mode_host_bin=None,
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

            with (
                mock.patch.object(cargo_module, "CODEX_RS_ROOT", codex_rs),
                mock.patch.object(
                    cargo_module,
                    "source_tree_fingerprint",
                    return_value={"status": "ok", "digest": "unchanged source"},
                ),
            ):
                with mock.patch.dict(os.environ, {}, clear=True):
                    with mock.patch("subprocess.run", side_effect=fake_run):
                        build_source_binaries(
                            TARGET_SPECS["x86_64-pc-windows-msvc"],
                            PACKAGE_VARIANTS["codex"],
                            cargo="cargo",
                            profile="release",
                            entrypoint_bin=None,
                            code_mode_host_bin=None,
                            codex_command_runner_bin=None,
                            codex_windows_sandbox_setup_bin=None,
                        )

            self.assertEqual(len(calls), 1)
            self.assertIn("codex", calls[0].cmd)
            self.assertIn("codex-code-mode-host", calls[0].cmd)
            self.assertIn("codex-command-runner", calls[0].cmd)
            self.assertIn("codex-windows-sandbox-setup", calls[0].cmd)
            self.assertTrue(
                source_build_stamp_path(
                    codex_rs / "target" / "package" / "x86_64-pc-windows-msvc-release"
                ).is_file()
            )

    def test_reused_entrypoint_builds_only_missing_windows_helpers(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            codex_rs = root / "codex-rs"
            entrypoint = touch_file(root / "prebuilt" / "codex.exe")
            code_mode_host = touch_file(root / "prebuilt" / "codex-code-mode-host.exe")
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
                    with mock.patch("subprocess.run", side_effect=fake_run):
                        build_source_binaries(
                            TARGET_SPECS["x86_64-pc-windows-msvc"],
                            PACKAGE_VARIANTS["codex"],
                            cargo="cargo",
                            profile="release",
                            entrypoint_bin=entrypoint,
                            code_mode_host_bin=code_mode_host,
                            codex_command_runner_bin=None,
                            codex_windows_sandbox_setup_bin=None,
                        )

        self.assertEqual(len(calls), 1)
        self.assertNotIn("codex", calls[0].cmd)
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
                                code_mode_host_bin=None,
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
                            "bins=codex,codex-code-mode-host,codex-command-runner,"
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
                                code_mode_host_bin=None,
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
                        code_mode_host_bin=None,
                        codex_command_runner_bin=None,
                        codex_windows_sandbox_setup_bin=None,
                    )

        run.assert_not_called()

    def test_override_at_default_path_does_not_gain_source_provenance(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            spec = TARGET_SPECS["x86_64-pc-windows-msvc"]
            variant = PACKAGE_VARIANTS["codex"]
            with (
                mock.patch.object(cargo_module, "CODEX_RS_ROOT", root),
                mock.patch.dict(os.environ, {}, clear=True),
                mock.patch.object(
                    cargo_module,
                    "source_tree_fingerprint",
                    return_value=fixed_source_fingerprint(),
                ),
            ):
                target = cargo_package_target_dir(spec, "release")
                entrypoint = touch_file(target / spec.target / "release" / "codex.exe")

                def compile(cmd, *, cwd, check, env):
                    write_bins_for_cmd(cmd, env=env, spec=spec, profile="release")

                with mock.patch.object(
                    cargo_module.subprocess, "run", side_effect=compile
                ) as run:
                    build_source_binaries(
                        spec,
                        variant,
                        cargo="cargo",
                        profile="release",
                        entrypoint_bin=entrypoint,
                        code_mode_host_bin=None,
                        codex_command_runner_bin=None,
                        codex_windows_sandbox_setup_bin=None,
                    )
                    stamp = cargo_module.read_source_build_stamp(target)
                    self.assertNotIn("entrypoint_bin", stamp["outputs"])
                    build_source_binaries(
                        spec,
                        variant,
                        cargo="cargo",
                        profile="release",
                        entrypoint_bin=None,
                        code_mode_host_bin=None,
                        codex_command_runner_bin=None,
                        codex_windows_sandbox_setup_bin=None,
                        reuse_existing=True,
                    )
                cmd = run.call_args.args[0]
                self.assertEqual(
                    [cmd[i + 1] for i, v in enumerate(cmd) if v == "--bin"], ["codex"]
                )
                self.assertEqual(run.call_count, 2)

    def test_effective_environment_changes_invalidate_reuse(self) -> None:
        spec = TARGET_SPECS["x86_64-pc-windows-msvc"]
        with mock.patch.dict(
            os.environ, {"CODEX_RELEASE_VERSION": "1.2.3"}, clear=True
        ):
            with mock.patch.object(
                cargo_module, "find_windows_lld_link", return_value="first-linker"
            ):
                first = cargo_module.build_recipe_fingerprint(
                    spec=spec, profile="release"
                )
            with mock.patch.object(
                cargo_module, "find_windows_lld_link", return_value="second-linker"
            ):
                second = cargo_module.build_recipe_fingerprint(
                    spec=spec, profile="release"
                )
                os.environ["CODEX_RELEASE_VERSION"] = "1.2.4"
                third = cargo_module.build_recipe_fingerprint(
                    spec=spec, profile="release"
                )
        self.assertNotEqual(first, second)
        self.assertNotEqual(second, third)

    def test_unavailable_tool_identity_cannot_authorize_reuse(self) -> None:
        spec = TARGET_SPECS["x86_64-pc-windows-msvc"]
        with mock.patch.object(
            cargo_module, "command_identity", return_value={"status": "unavailable"}
        ):
            recipe = cargo_module.build_recipe_fingerprint(spec=spec, profile="release")
            stamp = {
                "target": spec.target,
                "variant": "codex",
                "profile": "release",
                "build_recipe": recipe,
            }
            self.assertFalse(
                cargo_module.source_build_stamp_metadata_matches(
                    stamp,
                    spec=spec,
                    profile="release",
                    variant=PACKAGE_VARIANTS["codex"],
                )
            )

    def test_non_pe_override_fails_before_build(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            entry = Path(temp_dir) / "codex.exe"
            entry.write_bytes(b"not PE")
            with mock.patch.object(cargo_module, "run_cargo_build") as build:
                with self.assertRaisesRegex(RuntimeError, "Invalid PE"):
                    build_source_binaries(
                        TARGET_SPECS["x86_64-pc-windows-msvc"],
                        PACKAGE_VARIANTS["codex"],
                        cargo="cargo",
                        profile="release",
                        entrypoint_bin=entry,
                        code_mode_host_bin=None,
                        codex_command_runner_bin=None,
                        codex_windows_sandbox_setup_bin=None,
                    )
                build.assert_not_called()

    def test_known_output_mismatch_skips_hash(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = touch_file(Path(temp_dir) / "codex.exe")
            fingerprint = cargo_module.source_output_fingerprint(path)
            path.write_bytes(b"different size")
            with mock.patch.object(
                cargo_module,
                "source_output_fingerprint",
                side_effect=AssertionError("must not hash"),
            ):
                self.assertFalse(
                    cargo_module.source_output_matches_fingerprint(path, fingerprint)
                )


class SourceEvidenceTest(unittest.TestCase):
    @unittest.skipUnless(os.name == "nt", "Windows executable search semantics")
    def test_package_executes_the_cargo_selected_by_its_identity_probe(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            parent = root / "parent"
            build_root = root / "build"
            tools = build_root / "tools"
            parent.mkdir()
            tools.mkdir(parents=True)
            # The parent-directory executable must never run, even though
            # CreateProcess normally searches there before the child's PATH.
            (parent / "fixture-cargo.cmd").write_text(
                f'@echo off\necho wrong-tool>"{parent / "parent-ran"}"\nexit /b 91\n'
            )
            compiler = tools / "compiler.py"
            compiler.write_text(
                "import sys\n"
                "from pathlib import Path\n"
                "args = sys.argv[1:]\n"
                "if args[0] != 'build':\n"
                "    print('fixture cargo 1')\n"
                "else:\n"
                "    out = Path(args[args.index('--target-dir') + 1])\n"
                "    out /= args[args.index('--target') + 1]\n"
                "    out /= args[args.index('--profile') + 1]\n"
                "    out.mkdir(parents=True, exist_ok=True)\n"
                "    for i, arg in enumerate(args):\n"
                "        if arg == '--bin':\n"
                "            (out / (args[i + 1] + '.exe')).write_bytes(b'child-built')\n"
                "    with Path('build-calls').open('a') as log:\n"
                "        log.write('build\\n')\n",
                encoding="utf-8",
            )
            selected = tools / "fixture-cargo.cmd"
            selected.write_text(
                f'@echo off\n"{sys.executable}" "{compiler}" %*\n',
                encoding="utf-8",
            )
            spec = TARGET_SPECS["x86_64-pc-windows-msvc"]
            with (
                chdir(parent),
                mock.patch.object(cargo_module, "CODEX_RS_ROOT", build_root),
                mock.patch.dict(
                    os.environ,
                    {"PATH": "tools", "RUSTC": str(selected), "RUSTC_WRAPPER": ""},
                ),
                mock.patch.object(
                    cargo_module,
                    "source_tree_fingerprint",
                    return_value=fixed_source_fingerprint(),
                ),
            ):
                kwargs = dict(
                    cargo="fixture-cargo.cmd",
                    profile="release",
                    entrypoint_bin=None,
                    code_mode_host_bin=None,
                    codex_command_runner_bin=None,
                    codex_windows_sandbox_setup_bin=None,
                    reuse_existing=True,
                )
                outputs = build_source_binaries(
                    spec, PACKAGE_VARIANTS["codex"], **kwargs
                )
                self.assertEqual(
                    build_source_binaries(spec, PACKAGE_VARIANTS["codex"], **kwargs),
                    outputs,
                )
                stamp = cargo_module.read_source_build_stamp(
                    cargo_package_target_dir(spec, "release")
                )
            self.assertEqual(stamp["build_recipe"]["cargo"]["path"], str(selected))
            self.assertEqual((build_root / "build-calls").read_text(), "build\n")
            self.assertFalse((parent / "parent-ran").exists())
            for path in vars(outputs).values():
                self.assertEqual(path.read_bytes(), b"child-built")

    def test_tool_identity_resolves_supplied_path_relative_to_build_directory(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            parent_tool = touch_file(root / "parent-tools" / "fixture-tool.exe")
            build_root = root / "build"
            expected = touch_file(build_root / "tools" / "fixture-tool.exe")
            expected.chmod(0o755)
            parent_tool.chmod(0o755)
            with (
                chdir(parent_tool.parent),
                mock.patch.object(cargo_module, "CODEX_RS_ROOT", build_root),
                mock.patch.dict(os.environ, {"PATH": str(parent_tool.parent)}),
                mock.patch.object(
                    cargo_module.subprocess,
                    "run",
                    return_value=mock.Mock(stdout="child tool"),
                ) as run,
            ):
                for command in ("fixture-tool.exe", "./tools/fixture-tool.exe"):
                    with self.subTest(command=command):
                        env = {"PATH": "tools"}
                        identity = cargo_module.command_identity(
                            command, "--version", env=env
                        )
                        self.assertEqual(identity["path"], str(expected))
                        self.assertEqual(identity["version"], "child tool")
                        self.assertEqual(run.call_args.args[0][0], str(expected))
                        self.assertEqual(run.call_args.kwargs["env"], env)
                        self.assertEqual(run.call_args.kwargs["cwd"], build_root)

    def test_tool_identity_timeout_is_unavailable(self):
        with mock.patch.object(cargo_module, "DISCOVERY_TIMEOUT_SECONDS", 0.1):
            identity = cargo_module.command_identity(
                sys.executable, "-c", "import time; time.sleep(30)"
            )
        self.assertEqual(identity["status"], "unavailable")
        self.assertIn("timed out", identity["error"])

    def test_git_timeout_kills_and_reaps_probe_and_disables_source_evidence(self):
        process = mock.Mock()
        process.communicate.side_effect = [
            subprocess.TimeoutExpired("git", cargo_module.DISCOVERY_TIMEOUT_SECONDS),
            (b"", b""),
        ]
        with (
            mock.patch.object(cargo_module.shutil, "which", return_value="git"),
            mock.patch.object(cargo_module.subprocess, "Popen", return_value=process),
        ):
            self.assertEqual(
                cargo_module.source_tree_fingerprint(),
                {"status": "unavailable", "reason": "git-unavailable"},
            )
        process.kill.assert_called_once_with()
        self.assertEqual(process.communicate.call_count, 2)
        self.assertEqual(
            process.communicate.call_args_list[0].kwargs,
            {"timeout": cargo_module.DISCOVERY_TIMEOUT_SECONDS},
        )

    def test_tool_identity_uses_build_directory(self) -> None:
        with (
            mock.patch.object(
                cargo_module, "resolve_command", return_value=sys.executable
            ) as resolve,
            mock.patch.object(
                cargo_module.subprocess,
                "run",
                return_value=mock.Mock(stdout="cargo test"),
            ) as run,
        ):
            identity = cargo_module.command_identity("cargo", "--version")
        resolve.assert_called_once_with("cargo", env=None)
        self.assertEqual(identity["version"], "cargo test")
        self.assertEqual(run.call_args.args[0], [sys.executable, "--version"])
        self.assertEqual(run.call_args.kwargs["cwd"], cargo_module.CODEX_RS_ROOT)

    def test_unreadable_untracked_source_is_unavailable(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            subprocess.run(["git", "init", "--quiet", str(root)], check=True)
            subprocess.run(
                [
                    "git",
                    "-C",
                    str(root),
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "commit",
                    "--quiet",
                    "--allow-empty",
                    "-m",
                    "fixture",
                ],
                check=True,
            )
            source = root / "untracked.rs"
            source.write_text("fn main() {}")
            original_open = Path.open

            def open_file(path, *args, **kwargs):
                if path == source:
                    raise PermissionError("test unreadable source")
                return original_open(path, *args, **kwargs)

            with (
                mock.patch.object(cargo_module, "CODEX_RS_ROOT", root),
                mock.patch.object(Path, "open", open_file),
            ):
                self.assertEqual(
                    cargo_module.source_tree_fingerprint(),
                    {"status": "unavailable", "reason": "unreadable-source"},
                )


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
        "codex-code-mode-host": "codex-code-mode-host.exe",
        "codex-command-runner": "codex-command-runner.exe",
        "codex-windows-sandbox-setup": "codex-windows-sandbox-setup.exe",
    }
    for binary in bins:
        touch_file(output_dir / names[binary])


def touch_file(path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    return write_pe(path, 0x8664)


def write_pe(path: Path, machine: int) -> Path:
    contents = bytearray(128)
    contents[0:2] = b"MZ"
    struct.pack_into("<I", contents, 0x3C, 64)
    contents[64:68] = b"PE\0\0"
    struct.pack_into("<H", contents, 68, machine)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(contents)
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
        "untracked_contents_sha256": "untracked-contents",
    }


if __name__ == "__main__":
    unittest.main()
