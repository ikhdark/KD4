from __future__ import annotations

import errno
import hashlib
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
import types
import unittest
import zipfile
from pathlib import Path
from unittest import mock

import scripts.stage_npm_archives as archives
import scripts.stage_npm_packages as stage
from scripts.codex_package import layout
from scripts.codex_package.targets import PACKAGE_VARIANTS
from scripts.codex_package.targets import PackageInputs
from scripts.codex_package.targets import TARGET_SPECS


class CodexLauncherTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.node = shutil.which("node")
        if cls.node is None:
            raise unittest.SkipTest(
                "Node.js is required for launcher integration tests"
            )
        cls.platform_key = subprocess.check_output(
            [cls.node, "-p", "process.platform + '-' + process.arch"], text=True
        ).strip()

    def setUp(self) -> None:
        temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(temp_dir.cleanup)
        self.root = Path(temp_dir.name).resolve()
        self.launcher = self.root / "bin" / "codex.js"
        self.launcher.parent.mkdir()
        shutil.copyfile(
            Path(__file__).resolve().parents[1] / "codex-cli" / "bin" / "codex.js",
            self.launcher,
        )
        self.native_target = {
            "targetTriple": "test-native-target",
            "package": "@openai/codex-test-native",
            "binary": "codex.exe" if os.name == "nt" else "codex",
        }
        self.write_manifest({self.platform_key: self.native_target})

    def write_manifest(self, targets: dict) -> None:
        (self.root / "package.json").write_text(
            json.dumps(
                {
                    "type": "module",
                    "version": "1.2.3",
                    "optionalDependencies": {
                        self.native_target[
                            "package"
                        ]: "npm:@openai/codex@1.2.3-test-native"
                    },
                    "codexNativeTargets": targets,
                }
            ),
            encoding="utf-8",
        )

    def native_path(self, package_root: Path | None = None) -> Path:
        return (
            (package_root or self.root)
            / "vendor"
            / self.native_target["targetTriple"]
            / "bin"
            / self.native_target["binary"]
        )

    def run_launcher(self, *args: str, imported: bool = False, stdin: str = ""):
        command = [self.node]
        if imported:
            command.extend(
                [
                    "--input-type=module",
                    "-e",
                    f"await import({json.dumps(self.launcher.as_uri())})",
                ]
            )
        else:
            command.append(str(self.launcher))
        command.extend(args)
        env = {
            **os.environ,
            "npm_config_user_agent": "npm/10.0.0",
            "npm_execpath": "",
            "CODEX_MANAGED_BY_BUN": "stale",
            # Windows environment names are case-insensitive.
            "codex_managed_by_pnpm": "stale",
        }
        return subprocess.run(
            command,
            check=False,
            cwd=self.root,
            env=env,
            input=stdin,
            capture_output=True,
            text=True,
            encoding="utf-8",
            timeout=20,
        )

    def assert_startup_failure(self, result, message: str) -> None:
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertTrue(
            result.stderr.startswith("Codex could not start: "), result.stderr
        )
        self.assertIn(message, result.stderr)
        self.assertNotIn("\n    at ", result.stderr)

    def test_missing_optional_binary_has_actionable_error_without_stack(self) -> None:
        result = self.run_launcher()
        self.assert_startup_failure(
            result, "Missing optional dependency @openai/codex-test-native"
        )
        self.assertIn("Reinstall this KD4 package", result.stderr)

    def test_unsupported_platform_has_concise_error(self) -> None:
        self.write_manifest({})
        self.assert_startup_failure(self.run_launcher(), "Unsupported platform:")

    def test_installed_optional_package_reports_missing_executable(self) -> None:
        package_root = self.root / "node_modules" / "@openai" / "codex-test-native"
        package_root.mkdir(parents=True)
        (package_root / "package.json").write_text(
            json.dumps({"version": "1.2.3-test-native"}), encoding="utf-8"
        )

        result = self.run_launcher()

        self.assert_startup_failure(
            result, "is installed but its native executable is missing"
        )
        self.assertIn(str(self.native_path(package_root)), result.stderr)
        self.assertNotIn("Missing optional dependency", result.stderr)

    def test_malformed_native_target_has_actionable_metadata_error(self) -> None:
        malformed_targets = [None, "invalid"]
        for field in ("targetTriple", "package", "binary"):
            missing_field = dict(self.native_target)
            del missing_field[field]
            malformed_targets.append(missing_field)
            for value in (None, 12, "", " "):
                malformed_targets.append({**self.native_target, field: value})
        for target in malformed_targets:
            with self.subTest(target=target):
                self.write_manifest({self.platform_key: target})
                result = self.run_launcher()
                self.assert_startup_failure(result, "Invalid native target metadata")
                self.assertIn("Reinstall this KD4 package", result.stderr)
                self.assertNotIn("Unsupported platform", result.stderr)

    def test_optional_package_resolution_error_does_not_launch_bundled_binary(
        self,
    ) -> None:
        package_root = self.root / "node_modules" / "@openai" / "codex-test-native"
        package_root.mkdir(parents=True)
        (package_root / "package.json").write_text(
            json.dumps({"exports": {}}), encoding="utf-8"
        )
        binary = self.native_path()
        binary.parent.mkdir(parents=True)
        shutil.copy2(self.node, binary)

        result = self.run_launcher("-e", "console.log('unexpected bundled launch')")

        self.assert_startup_failure(result, '"exports"')
        self.assertIn(str(package_root / "package.json"), result.stderr)
        self.assertNotIn("Missing optional dependency", result.stderr)

    def test_mismatched_native_version_does_not_launch_either_binary(self) -> None:
        package_root = self.root / "node_modules" / "@openai" / "codex-test-native"
        for binary in (self.native_path(), self.native_path(package_root)):
            binary.parent.mkdir(parents=True)
            shutil.copy2(self.node, binary)
        for version in ("1.2.2-test-native", "1.2.4-test-native", "1.2.3", None):
            with self.subTest(version=version):
                (package_root / "package.json").write_text(
                    json.dumps({"version": version}), encoding="utf-8"
                )
                result = self.run_launcher("-e", "console.log('unexpected launch')")
                self.assert_startup_failure(result, "Native package version mismatch")
                self.assertIn("expected 1.2.3-test-native", result.stderr)
                self.assertIn("Reinstall this KD4 package", result.stderr)

    def test_upstream_native_publication_cannot_satisfy_release_alias(self) -> None:
        manifest = stage.load_build_module().build_codex_package_json("1.2.3")
        native_target = manifest["codexNativeTargets"].get(self.platform_key)
        if native_target is None:
            self.skipTest(f"no KD4 native target for {self.platform_key}")
        (self.root / "package.json").write_text(json.dumps(manifest), encoding="utf-8")
        package_root = self.root / "node_modules" / native_target["package"]
        binary = (
            package_root
            / "vendor"
            / native_target["targetTriple"]
            / "bin"
            / native_target["binary"]
        )
        binary.parent.mkdir(parents=True)
        shutil.copy2(self.node, binary)
        kd4_version = manifest["optionalDependencies"][native_target["package"]]
        # Upstream publishes its native payloads as @openai/codex@<version>-<platform>
        # with the same vendor layout, and KD4 reuses upstream release numbers.
        for version, launches in (
            (f"1.2.3-{self.platform_key}", False),
            (kd4_version.rsplit("@", 1)[1], True),
        ):
            with self.subTest(version=version):
                (package_root / "package.json").write_text(
                    json.dumps({"name": "@openai/codex", "version": version}),
                    encoding="utf-8",
                )
                result = self.run_launcher("-e", "console.log('launched')")
                if launches:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout.strip(), "launched")
                else:
                    self.assert_startup_failure(
                        result, "Native package version mismatch"
                    )

    @unittest.skipUnless(os.name == "nt", "Windows console Ctrl-C contract")
    def test_windows_ctrl_c_waits_for_child_and_mirrors_its_exit(self) -> None:
        binary = self.native_path()
        binary.parent.mkdir(parents=True)
        shutil.copy2(self.node, binary)
        # Console Ctrl-C already reaches the child; child.kill() would be
        # TerminateProcess and pre-empt the child's own interrupt handling.
        script = (
            "import childProcess from 'node:child_process'; "
            "import { syncBuiltinESMExports } from 'node:module'; "
            "const spawn = childProcess.spawn; "
            "childProcess.spawn = (...args) => { const child = spawn(...args); "
            "child.once('spawn', () => process.emit('SIGINT')); return child; }; "
            "syncBuiltinESMExports(); "
            f"process.argv = [process.execPath, {json.dumps(str(self.launcher))}, '-e', "
            "\"setTimeout(() => { console.log('child finished'); process.exit(5); }, 500)\"]; "
            f"await import({json.dumps(self.launcher.as_uri())});"
        )
        result = subprocess.run(
            [self.node, "--input-type=module", "-e", script],
            cwd=self.root,
            check=False,
            capture_output=True,
            text=True,
            timeout=20,
        )
        self.assertEqual(result.returncode, 5, result.stderr)
        self.assertEqual(result.stdout.strip(), "child finished")
        self.assertEqual(result.stderr, "")

    @unittest.skipUnless(os.name == "nt", "Windows exit status contract")
    def test_signal_terminated_child_preserves_numeric_exit_status(self) -> None:
        binary = self.native_path()
        binary.parent.mkdir(parents=True)
        binary.write_bytes(b"fixture")
        for signal, expected in (("SIGINT", 130), ("SIGTERM", 143), ("UNKNOWN", 1)):
            script = (
                "import childProcess from 'node:child_process'; "
                "import { EventEmitter } from 'node:events'; "
                "import { syncBuiltinESMExports } from 'node:module'; "
                "childProcess.spawn = () => { const child = new EventEmitter(); "
                f"setImmediate(() => child.emit('exit', null, {json.dumps(signal)})); "
                "return child; }; syncBuiltinESMExports(); "
                f"await import({json.dumps(self.launcher.as_uri())});"
            )
            result = subprocess.run(
                [self.node, "--input-type=module", "-e", script],
                cwd=self.root,
                check=False,
                capture_output=True,
                text=True,
                timeout=20,
            )
            self.assertEqual(result.returncode, expected, result.stderr)

    def test_spawn_failure_has_actionable_error_without_stack(self) -> None:
        binary = self.native_path()
        binary.mkdir(parents=True)
        result = self.run_launcher()
        self.assert_startup_failure(result, f"Unable to start {binary}")
        self.assertIn("Reinstall this KD4 package", result.stderr)

    def test_signal_forwarding_failure_does_not_wait_for_child_exit(self) -> None:
        binary = self.native_path()
        binary.parent.mkdir(parents=True)
        binary.write_bytes(b"fixture")
        cases = (
            ("return false", "the child process may still be running"),
            ("throw new Error('permission denied')", "permission denied"),
            ("setImmediate(() => child.emit('exit', 7, null)); return true", None),
        )
        for kill_body, error_detail in cases:
            with self.subTest(kill_body=kill_body):
                script = (
                    "import childProcess from 'node:child_process'; "
                    "import { EventEmitter } from 'node:events'; "
                    "import { syncBuiltinESMExports } from 'node:module'; "
                    "childProcess.spawn = () => { const child = new EventEmitter(); "
                    f"child.kill = (signal) => {{ console.log(signal); {kill_body}; }}; "
                    # A real child keeps Node alive; reproduce that until the
                    # launcher terminates rather than relying on unresolved TLA.
                    "setInterval(() => {}, 1000); "
                    "setImmediate(() => process.emit('SIGTERM')); return child; }; "
                    "syncBuiltinESMExports(); "
                    f"await import({json.dumps(self.launcher.as_uri())});"
                )
                result = subprocess.run(
                    [self.node, "--input-type=module", "-e", script],
                    cwd=self.root,
                    check=False,
                    capture_output=True,
                    text=True,
                    timeout=10,
                )
                self.assertEqual(result.stdout.strip(), "SIGTERM")
                if error_detail is None:
                    self.assertEqual(result.returncode, 7, result.stderr)
                    self.assertEqual(result.stderr, "")
                else:
                    self.assertEqual(result.returncode, 143 if os.name == "nt" else -15)
                    self.assertIn(
                        f"Unable to forward SIGTERM to Codex: {error_detail}.",
                        result.stderr,
                    )
                    self.assertNotIn("\n    at ", result.stderr)

    def test_optional_package_launch_forwards_args_environment_and_exit_code(
        self,
    ) -> None:
        package_root = self.root / "node_modules" / "@openai" / "codex-test-native"
        binary = self.native_path(package_root)
        binary.parent.mkdir(parents=True)
        (package_root / "package.json").write_text(
            json.dumps({"version": "1.2.3-test-native"}), encoding="utf-8"
        )
        shutil.copy2(self.node, binary)
        result = self.run_launcher(
            "-e",
            "console.log(JSON.stringify({args: process.argv.slice(1), "
            "root: process.env.CODEX_MANAGED_PACKAGE_ROOT, "
            "npm: process.env.CODEX_MANAGED_BY_NPM, "
            "bun: process.env.CODEX_MANAGED_BY_BUN ?? null, "
            "pnpm: process.env.CODEX_MANAGED_BY_PNPM ?? null})); process.exit(7)",
            "argument with spaces",
        )
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertEqual(result.stderr, "")
        self.assertEqual(
            json.loads(result.stdout),
            {
                "args": ["argument with spaces"],
                "root": str(self.root),
                "npm": "1",
                "bun": None,
                "pnpm": None,
            },
        )

    def test_package_manager_discovery_checks_shared_ancestors_once(self) -> None:
        binary = self.native_path()
        binary.parent.mkdir(parents=True)
        shutil.copy2(self.node, binary)
        for lexical_dir in (self.launcher.parent, self.root / "linked" / "bin"):
            for pnpm_owner in (None, lexical_dir):
                with self.subTest(lexical_dir=lexical_dir, pnpm_owner=pnpm_owner):
                    # Observe filesystem probes through the real launcher entrypoint.
                    # Simulate a pnpm link without requiring Windows symlink privileges.
                    script = (
                        "import fs from 'node:fs'; "
                        "import path from 'node:path'; "
                        "import { syncBuiltinESMExports } from 'node:module'; "
                        "const exists = fs.existsSync, realpath = fs.realpathSync; "
                        "const probes = []; "
                        f"const owner = {json.dumps(str(pnpm_owner) if pnpm_owner else None)}; "
                        "fs.existsSync = (p) => { "
                        "if (path.basename(p) !== '.modules.yaml') return exists(p); "
                        "probes.push(p); "
                        "return owner !== null && "
                        "p === path.join(owner, 'node_modules', '.modules.yaml'); }; "
                        "fs.realpathSync = (p, ...args) => { "
                        "if (owner !== null && "
                        "p === path.join(owner, 'node_modules', '@openai', 'codex')) "
                        f"return {json.dumps(str(self.root))}; "
                        "return realpath(p, ...args); }; "
                        "syncBuiltinESMExports(); "
                        f"process.argv = [process.execPath, {json.dumps(str(lexical_dir / 'codex.js'))}, "
                        "'-e', \"console.log(JSON.stringify({npm: process.env.CODEX_MANAGED_BY_NPM ?? null, "
                        'pnpm: process.env.CODEX_MANAGED_BY_PNPM ?? null}))"]; '
                        "process.on('exit', () => console.log(JSON.stringify(probes))); "
                        f"await import({json.dumps(self.launcher.as_uri())});"
                    )
                    result = subprocess.run(
                        [self.node, "--input-type=module", "-e", script],
                        cwd=self.root,
                        env={**os.environ, "npm_config_user_agent": "npm/10.0.0"},
                        check=False,
                        capture_output=True,
                        text=True,
                        timeout=20,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stderr, "")
                    child_env, probes = map(json.loads, result.stdout.splitlines())
                    self.assertEqual(
                        child_env,
                        {"npm": None, "pnpm": "1"}
                        if pnpm_owner
                        else {"npm": "1", "pnpm": None},
                    )
                    self.assertEqual(len(probes), len(set(probes)), probes)
                    if pnpm_owner is None:
                        directories = {
                            self.root,
                            *self.root.parents,
                            lexical_dir,
                            *lexical_dir.parents,
                        }
                        self.assertEqual(
                            set(probes),
                            {
                                str(p / "node_modules" / ".modules.yaml")
                                for p in directories
                            },
                        )

    def test_import_without_argv_entrypoint_launches_bundled_binary(self) -> None:
        binary = self.native_path()
        binary.parent.mkdir(parents=True)
        shutil.copy2(self.node, binary)
        result = self.run_launcher(
            imported=True, stdin="console.log('launched without argv[1]')"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "launched without argv[1]")
        self.assertEqual(result.stderr, "")


class StageNpmPackagesTests(unittest.TestCase):
    def setUp(self) -> None:
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

    def test_build_module_derives_platform_metadata_from_canonical_targets(
        self,
    ) -> None:
        build = stage.load_build_module()
        self.assertEqual(
            stage.expand_packages(["codex"]),
            ["codex", "codex-win32-x64", "codex-win32-arm64"],
        )
        self.assertEqual(
            stage.collect_native_component_sets(["codex-win32-x64"]),
            [(("codex-package",), ("x86_64-pc-windows-msvc",))],
        )
        self.assertEqual(
            stage.tarball_name_for_package("codex-win32-x64", "1.2.3"),
            "codex-npm-win32-x64-1.2.3.tgz",
        )

        self.assertEqual(
            set(build.CODEX_PLATFORM_PACKAGES),
            {"codex-win32-x64", "codex-win32-arm64"},
        )
        self.assertEqual(
            build.CODEX_PLATFORM_PACKAGES["codex-win32-x64"]["target_triple"],
            "x86_64-pc-windows-msvc",
        )
        self.assertEqual(
            build.PACKAGE_TARGET_FILTERS["codex-win32-x64"],
            "x86_64-pc-windows-msvc",
        )
        self.assertEqual(
            build.PACKAGE_NATIVE_COMPONENTS["codex-win32-x64"],
            ["codex-package"],
        )

        package_json = build.build_codex_package_json("1.2.3")
        self.assertEqual(package_json["os"], ["win32"])
        self.assertEqual(
            package_json["optionalDependencies"]["@openai/codex-win32-x64"],
            "npm:@openai/codex@1.2.3-kd4-win32-x64",
        )
        self.assertEqual(
            package_json["codexNativeTargets"]["win32-x64"],
            {
                "targetTriple": "x86_64-pc-windows-msvc",
                "package": "@openai/codex-win32-x64",
                "binary": "codex.exe",
            },
        )

    def test_npm_pack_smoke_tests_the_moved_tarball(self) -> None:
        build = stage.load_build_module()
        staging_dir = self.root / "staging"
        staging_dir.mkdir()
        output_path = self.root / "out" / "codex.tgz"

        def fake_pack(command: list[str], **_: object) -> str:
            pack_dir = Path(command[command.index("--pack-destination") + 1])
            (pack_dir / "packed.tgz").write_bytes(b"fixture")
            return json.dumps([{"filename": "packed.tgz"}])

        with (
            mock.patch.object(build.subprocess, "check_output", side_effect=fake_pack),
            mock.patch.object(build, "smoke_test_npm_tarball") as smoke,
        ):
            actual = build.run_npm_pack(staging_dir, output_path)

        self.assertEqual(actual, output_path.resolve())
        self.assertEqual(output_path.read_bytes(), b"fixture")
        smoke.assert_called_once_with(output_path.resolve())

    def test_platform_package_manifest_is_minimal(self) -> None:
        build = stage.load_build_module()

        package_json = build.build_platform_package_json(
            "1.2.3-win32-x64",
            build.CODEX_PLATFORM_PACKAGES["codex-win32-x64"],
        )

        self.assertEqual(package_json["name"], "@openai/codex")
        self.assertEqual(package_json["version"], "1.2.3-win32-x64")
        self.assertEqual(package_json["os"], ["win32"])
        self.assertEqual(package_json["cpu"], ["x64"])
        self.assertEqual(package_json["files"], ["vendor", "LICENSE", "NOTICE"])
        self.assertNotIn("packageManager", package_json)

    def test_codex_staging_copies_user_facing_package_readme(self) -> None:
        build = stage.load_build_module()

        build.stage_sources(self.root, "1.2.3", "codex")

        self.assertEqual(
            (self.root / "README.md").read_text(encoding="utf-8"),
            (build.CODEX_CLI_ROOT / "README.npm.md").read_text(encoding="utf-8"),
        )
        staged_readme = (self.root / "README.md").read_text()
        self.assertNotIn("Repository source map", staged_readme)
        self.assertNotIn("github.com/openai/codex/releases", staged_readme)
        self.assertNotIn("chatgpt.com/codex/install.ps1", staged_readme)
        self.assertIn("github.com/ikhdark/KD4/releases", staged_readme)
        self.assertIn("raw.githubusercontent.com/ikhdark/KD4", staged_readme)

    def test_codex_sdk_staging_injects_matching_cli_dependency(self) -> None:
        build = stage.load_build_module()

        with mock.patch.object(build, "stage_codex_sdk_sources") as stage_sdk_sources:
            build.stage_sources(self.root, "1.2.3", "codex-sdk")

        stage_sdk_sources.assert_called_once_with(self.root)
        package_json = json.loads((self.root / "package.json").read_text())
        self.assertEqual(package_json["dependencies"]["@openai/codex"], "1.2.3")
        self.assertEqual(package_json["os"], ["win32"])
        self.assertNotIn("prepare", package_json["scripts"])

    def test_responses_proxy_staging_is_windows_only(self) -> None:
        build = stage.load_build_module()

        build.stage_sources(self.root, "1.2.3", "codex-responses-api-proxy")

        package_json = json.loads((self.root / "package.json").read_text())
        self.assertEqual(package_json["os"], ["win32"])

    def test_copy_native_binaries_filters_target_and_requires_executable(self) -> None:
        build = stage.load_build_module()
        vendor_src = self.root / "vendor-src"
        selected_target = vendor_src / "x86_64-pc-windows-msvc"
        self.write_canonical_package(selected_target)

        # Filtered targets are neither validated nor copied.
        skipped_bin = vendor_src / "aarch64-pc-windows-msvc" / "bin"
        skipped_bin.mkdir(parents=True)
        (skipped_bin / "codex.exe").write_text("native", encoding="utf-8")

        staging_dir = self.root / "staging"
        staging_dir.mkdir()
        with mock.patch.object(
            layout.subprocess, "run", return_value=mock.Mock(stdout="codex 1.2.3")
        ):
            build.copy_native_binaries(
                vendor_src,
                staging_dir,
                [build.CODEX_PACKAGE_COMPONENT],
                {"x86_64-pc-windows-msvc"},
                expected_version="1.2.3",
            )

        staged_target = staging_dir / "vendor" / "x86_64-pc-windows-msvc"
        self.assertEqual(relative_files(staged_target), relative_files(selected_target))
        self.assertIn("bin/codex.exe", relative_files(staged_target))
        self.assertFalse((staging_dir / "vendor" / "aarch64-pc-windows-msvc").exists())

        missing_src = self.root / "missing-src"
        (missing_src / "x86_64-pc-windows-msvc").mkdir(parents=True)
        with self.assertRaisesRegex(RuntimeError, "Missing package directory"):
            build.copy_native_binaries(
                missing_src,
                self.root / "missing-staging",
                [build.CODEX_PACKAGE_COMPONENT],
                {"x86_64-pc-windows-msvc"},
            )

    def test_copy_native_binaries_rejects_changed_declared_file(self) -> None:
        build = stage.load_build_module()
        target_dir = self.root / "vendor-src" / "x86_64-pc-windows-msvc"
        self.write_canonical_package(target_dir)
        (target_dir / "bin" / "codex.exe").write_bytes(b"tampered")

        with self.assertRaisesRegex(RuntimeError, "digest mismatch"):
            build.copy_native_binaries(
                self.root / "vendor-src",
                self.root / "staging",
                [build.CODEX_PACKAGE_COMPONENT],
            )

    def test_copy_native_binaries_rejects_package_built_for_another_target(
        self,
    ) -> None:
        build = stage.load_build_module()
        x64_package = self.root / "x86_64-pc-windows-msvc"
        self.write_canonical_package(x64_package)
        # Relabelling metadata keeps every inventory digest and the bundle id valid.
        relabelled = self.root / "vendor-src" / "aarch64-pc-windows-msvc"
        shutil.copytree(x64_package, relabelled)
        metadata_path = relabelled / "codex-package.json"
        metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
        metadata["target"] = relabelled.name
        metadata_path.write_text(json.dumps(metadata), encoding="utf-8")

        with self.assertRaisesRegex(RuntimeError, "target mismatch"):
            build.copy_native_binaries(
                self.root / "vendor-src",
                self.root / "staging",
                [build.CODEX_PACKAGE_COMPONENT],
                {relabelled.name},
            )
        self.assertFalse((self.root / "staging" / "vendor" / relabelled.name).exists())

    def test_copy_native_binaries_rejects_non_windows_targets(self) -> None:
        build = stage.load_build_module()
        vendor_src = self.root / "vendor-src"
        (vendor_src / "x86_64-unknown-linux-gnu").mkdir(parents=True)

        with self.assertRaisesRegex(
            RuntimeError, "Unsupported non-Windows native target"
        ):
            build.copy_native_binaries(
                vendor_src,
                self.root / "staging",
                [build.CODEX_PACKAGE_COMPONENT],
            )

    def write_canonical_package(self, target_dir: Path, version: str = "1.2.3") -> None:
        from scripts.codex_package.test_layout import write_pe

        inputs_dir = self.root / "inputs" / target_dir.name
        inputs_dir.mkdir(parents=True, exist_ok=True)
        inputs = []
        for name in ("codex", "host", "rg", "runner", "setup"):
            path = inputs_dir / f"{name}.exe"
            write_pe(path)
            inputs.append(path)
        target_dir.mkdir(parents=True)
        layout.build_package_dir(
            target_dir,
            version,
            PACKAGE_VARIANTS["codex"],
            TARGET_SPECS[target_dir.name],
            PackageInputs(*inputs),
            build_identity={"source": "test"},
        )

    def test_npm_smoke_install_uses_packed_name_outside_ancestor_projects(
        self,
    ) -> None:
        if shutil.which("npm") is None or shutil.which("node") is None:
            self.skipTest("npm and Node.js are required for the npm smoke install")
        build = stage.load_build_module()
        # A smoke install without its own prefix would land in this ancestor project.
        ancestor_modules = self.root / "node_modules"
        ancestor_modules.mkdir()
        temp_root = self.root / "temp"
        temp_root.mkdir()
        npm_env = {
            "NPM_CONFIG_CACHE": str(self.root / "npm-cache"),
            "NPM_CONFIG_AUDIT": "false",
            "NPM_CONFIG_FUND": "false",
            "NPM_CONFIG_UPDATE_NOTIFIER": "false",
        }
        for launcher, valid in (("export {};\n", True), ("export {\n", False)):
            with self.subTest(valid=valid):
                staging = self.root / f"staging-{valid}"
                (staging / "bin").mkdir(parents=True)
                (staging / "bin" / "proxy.js").write_text(launcher, encoding="utf-8")
                (staging / "package.json").write_text(
                    json.dumps(
                        {
                            "name": "@openai/codex-responses-api-proxy",
                            "version": "1.2.3",
                            "type": "module",
                            "bin": {"codex-responses-api-proxy": "bin/proxy.js"},
                            "files": ["bin"],
                        }
                    ),
                    encoding="utf-8",
                )
                output = self.root / f"out-{valid}" / "proxy.tgz"
                with (
                    mock.patch.object(tempfile, "tempdir", str(temp_root)),
                    mock.patch.dict(os.environ, npm_env),
                ):
                    if valid:
                        self.assertEqual(
                            build.run_npm_pack(staging, output), output.resolve()
                        )
                    else:
                        with self.assertRaises(subprocess.CalledProcessError):
                            build.run_npm_pack(staging, output)
                self.assertEqual(list(ancestor_modules.iterdir()), [])

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
            stage,
            "check_output_owned",
            return_value="local/fork\n",
        ) as check_output:
            self.assertEqual(stage.resolve_github_repo(None), "local/fork")

        self.assertIn("repo", check_output.call_args.args[0])

    def test_resolve_github_repo_falls_back_to_kd4_when_gh_unavailable(
        self,
    ) -> None:
        with mock.patch.object(
            stage,
            "check_output_owned",
            side_effect=FileNotFoundError,
        ):
            self.assertEqual(stage.resolve_github_repo(None), stage.DEFAULT_GITHUB_REPO)
        self.assertEqual(stage.DEFAULT_GITHUB_REPO, "ikhdark/KD4")

    def test_workflow_lookup_requires_an_explicit_locator(self) -> None:
        with self.assertRaisesRegex(ValueError, "--workflow-url is required"):
            stage.resolve_workflow_url("1.2.3", None, "ikhdark/KD4", None)

    def test_github_repo_can_be_derived_from_workflow_url(self) -> None:
        self.assertEqual(
            stage.github_repo_from_workflow_url(
                "https://github.com/local/fork/actions/runs/12345"
            ),
            "local/fork",
        )

    def test_attempt_qualified_workflow_url_is_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "workflow URL must match"):
            stage.workflow_id_from_url(
                "https://github.com/local/fork/actions/runs/12345/attempts/2"
            )

    def test_workflow_url_parser_rejects_non_https_and_unknown_suffixes(self) -> None:
        for workflow_url in (
            "http://github.com/local/fork/actions/runs/12345",
            "https://github.com/local/fork/actions/runs/12345/jobs/7",
            "https://github.com/local/fork/actions/runs/12345/attempts/latest",
        ):
            with (
                self.subTest(workflow_url=workflow_url),
                self.assertRaisesRegex(ValueError, "workflow URL must match"),
            ):
                stage.workflow_id_from_url(workflow_url)

    def test_invalid_workflow_url_does_not_fall_back_to_another_repo(self) -> None:
        args = types.SimpleNamespace(
            output_dir=self.root / "out",
            workflow_url="https://github.example/local/fork/actions/runs/12345",
            github_repo=None,
        )
        with (
            mock.patch.object(stage, "parse_args", return_value=args),
            mock.patch.object(stage, "resolve_github_repo") as resolve_repo,
            self.assertRaisesRegex(ValueError, "could not derive a GitHub repository"),
        ):
            stage.main()

        resolve_repo.assert_not_called()

    def test_workflow_url_repo_mismatch_is_rejected(self) -> None:
        args = types.SimpleNamespace(
            output_dir=self.root / "out",
            workflow_url="https://github.com/local/fork/actions/runs/12345",
            github_repo="other/fork",
        )
        with (
            mock.patch.object(stage, "parse_args", return_value=args),
            self.assertRaisesRegex(ValueError, "belongs to local/fork"),
        ):
            stage.main()

    def test_release_workflow_lookup_uses_selected_repo_and_workflow(self) -> None:
        with mock.patch.object(
            stage,
            "check_output_owned",
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

    def test_worker_counts_reject_non_positive_values(self) -> None:
        for requested in (0, -1):
            with (
                self.subTest(requested=requested),
                self.assertRaisesRegex(ValueError, "must be > 0"),
            ):
                stage.worker_count_for(4, requested=requested)

    def test_worker_cli_options_reject_zero(self) -> None:
        for option in ("--max-download-workers", "--max-stage-workers"):
            argv = [
                "stage_npm_packages.py",
                "--release-version",
                "1.2.3",
                "--package",
                "codex",
                option,
                "0",
            ]
            with (
                self.subTest(option=option),
                mock.patch.object(sys, "argv", argv),
                mock.patch.object(sys, "stderr", io.StringIO()),
                self.assertRaises(SystemExit),
            ):
                stage.parse_args()

    def test_list_workflow_artifacts_is_cached_per_repo_and_workflow(self) -> None:
        with mock.patch.object(
            stage,
            "check_output_owned",
            side_effect=[
                f"{artifact_id}\tx86_64-pc-windows-msvc\t1024\tsha256:{'a' * 64}\n"
                for artifact_id in (42, 43, 44)
            ],
        ) as check_output:
            first = stage.list_workflow_artifacts("12345", "local/fork")
            second = stage.list_workflow_artifacts("12345", "local/fork")
            third = stage.list_workflow_artifacts("12345", "other/fork")
            fourth = stage.list_workflow_artifacts("67890", "local/fork")
            self.assertEqual(
                stage.list_workflow_artifacts("67890", "local/fork"), fourth
            )

        self.assertEqual(first, second)
        self.assertEqual(third[0].artifact_id, 43)
        self.assertEqual(fourth[0].artifact_id, 44)
        self.assertEqual(first[0].name, "x86_64-pc-windows-msvc")
        self.assertEqual(first[0].artifact_id, 42)
        self.assertEqual(first[0].archive_sha256, "a" * 64)
        self.assertEqual(check_output.call_count, 3)
        self.assertIn(
            "repos/local/fork/actions/runs/67890/artifacts",
            check_output.call_args_list[2].args[0],
        )
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
            stage.WorkflowArtifact("x86_64-pc-windows-msvc", 10, 1, "a" * 64),
            stage.WorkflowArtifact("aarch64-pc-windows-msvc", 20, 2, "b" * 64),
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

    def test_duplicate_selected_artifact_names_are_rejected(self) -> None:
        artifacts = (
            stage.WorkflowArtifact("target", 10, 1, "a" * 64),
            stage.WorkflowArtifact("target", 10, 2, "b" * 64),
        )
        with (
            mock.patch.object(stage, "list_workflow_artifacts", return_value=artifacts),
            mock.patch.object(stage, "codex_package_component", return_value="package"),
            self.assertRaisesRegex(RuntimeError, "duplicate workflow artifact name"),
        ):
            stage.select_target_artifacts("123", "local/fork", ["package"], ["target"])

    def test_selected_artifact_requires_authoritative_digest(self) -> None:
        artifact = stage.WorkflowArtifact("target", 10, 1, None)
        with (
            mock.patch.object(
                stage, "list_workflow_artifacts", return_value=(artifact,)
            ),
            mock.patch.object(stage, "codex_package_component", return_value="package"),
            self.assertRaisesRegex(RuntimeError, "no authoritative sha256"),
        ):
            stage.select_target_artifacts("123", "local/fork", ["package"], ["target"])

    def test_build_stage_command_uses_target_specific_vendor_src(self) -> None:
        key = (("codex-package",), ("x86_64-pc-windows-msvc",))
        vendor_src = self.root / "vendor-src"
        _pack_output, command = stage.build_stage_command(
            "codex-win32-x64",
            "1.2.3",
            self.root / "dist",
            self.root / "staging",
            {key: vendor_src},
        )

        self.assertIn("--vendor-src", command)
        self.assertEqual(command[command.index("--vendor-src") + 1], str(vendor_src))

    def test_download_artifacts_uses_complete_markers(self) -> None:
        archives_by_id: dict[int, bytes] = {}
        artifacts_list: list[stage.WorkflowArtifact] = []
        for artifact_id, name in ((10, "windows-x64"), (20, "windows-arm64")):
            payload = io.BytesIO()
            with zipfile.ZipFile(payload, "w") as archive:
                archive.writestr(f"{name}.txt", name)
            archive_bytes = payload.getvalue()
            archives_by_id[artifact_id] = archive_bytes
            artifacts_list.append(
                stage.WorkflowArtifact(
                    name,
                    len(archive_bytes),
                    artifact_id,
                    hashlib.sha256(archive_bytes).hexdigest(),
                )
            )
        artifacts = tuple(artifacts_list)
        calls: list[str] = []

        def fake_run(cmd: list[str], **kwargs: object) -> mock.Mock:
            endpoint = cmd[-1]
            artifact_id = int(endpoint.split("/")[-2])
            output = kwargs["stdout"]
            output.write(archives_by_id[artifact_id])  # type: ignore[union-attr]
            calls.append(
                next(item.name for item in artifacts if item.artifact_id == artifact_id)
            )
            return mock.Mock(returncode=0)

        with mock.patch.object(stage, "run_owned", fake_run):
            stage.download_artifacts(
                "999", "local/fork", self.root / "artifacts", artifacts, 2
            )
            stage.download_artifacts(
                "999", "local/fork", self.root / "artifacts", artifacts, 2
            )

        self.assertCountEqual(calls, ["windows-x64", "windows-arm64"])
        for artifact in artifacts:
            self.assertTrue(
                (self.root / "artifacts" / artifact.name / ".complete").is_file()
            )

    def test_digest_mismatch_preserves_existing_artifact_cache(self) -> None:
        artifact_dir = self.root / "artifacts" / "windows-x64"
        artifact_dir.mkdir(parents=True)
        (artifact_dir / "existing.txt").write_text("existing", encoding="utf-8")
        old = stage.WorkflowArtifact("windows-x64", 1, 1, "a" * 64)
        stage.write_complete_marker(artifact_dir, old)
        payload = io.BytesIO()
        with zipfile.ZipFile(payload, "w") as archive:
            archive.writestr("new.txt", "new")
        archive_bytes = payload.getvalue()
        replacement = stage.WorkflowArtifact(
            "windows-x64", len(archive_bytes), 2, "0" * 64
        )

        def fake_run(_cmd: list[str], **kwargs: object) -> mock.Mock:
            kwargs["stdout"].write(archive_bytes)  # type: ignore[union-attr]
            return mock.Mock(returncode=0)

        with (
            mock.patch.object(stage, "run_owned", fake_run),
            self.assertRaisesRegex(RuntimeError, "sha256 mismatch"),
        ):
            stage.download_single_artifact(
                "999", "local/fork", self.root / "artifacts", replacement
            )
        self.assertEqual(
            (artifact_dir / "existing.txt").read_text(encoding="utf-8"), "existing"
        )
        self.assertTrue(stage.artifact_is_complete(artifact_dir, old))

    def test_changed_remote_artifact_id_invalidates_complete_marker(self) -> None:
        artifact_dir = self.root / "artifacts" / "windows-x64"
        artifact_dir.mkdir(parents=True)
        (artifact_dir / "payload.txt").write_text("payload", encoding="utf-8")
        old = stage.WorkflowArtifact("windows-x64", 10, 1, "a" * 64)
        stage.write_complete_marker(artifact_dir, old)
        replacement = stage.WorkflowArtifact("windows-x64", 10, 2, "a" * 64)
        with mock.patch.object(
            stage,
            "artifact_tree_digest",
            side_effect=AssertionError("obsolete cache scanned"),
        ):
            self.assertFalse(stage.artifact_is_complete(artifact_dir, replacement))

    def test_zip_rejects_windows_paths_and_aliases(self) -> None:
        for name in (
            "D:escaped.txt",
            "safe/./file.txt",
            "safe//file.txt",
            "safe/../file.txt",
            "safe/file.txt:stream",
            "safe/file.txt.",
            "NUL.txt",
            "SAFE/FILE.TXT",
        ):
            with self.subTest(name=name):
                archive_path = self.root / "input.zip"
                with zipfile.ZipFile(archive_path, "w") as archive:
                    archive.writestr("safe/file.txt", "original")
                    archive.writestr(name, "overwrite")
                with tempfile.TemporaryDirectory(dir=self.root) as destination:
                    with self.assertRaisesRegex(
                        RuntimeError, "unsafe workflow artifact path"
                    ):
                        stage.extract_artifact_zip(archive_path, Path(destination))
                    self.assertEqual(
                        (Path(destination) / "safe/file.txt").read_text(), "original"
                    )

    def test_cache_digests_frame_file_contents(self) -> None:
        for module, digest_function in (
            (stage, stage.artifact_tree_digest),
            (archives, archives.cache_tree_digest),
        ):
            with self.subTest(module=module.__name__):
                root = self.root / module.__name__
                root.mkdir()
                first, second = root / "a", root / "b"
                first.write_bytes(b"left")
                second.write_bytes(b"right")
                header = b"f" + (1).to_bytes(8, "big") + b"b"
                if module is archives:
                    header += (second.stat().st_mode & 0o777).to_bytes(4, "big")
                first.write_bytes(b"left" + header)
                before = digest_function(root)
                first.write_bytes(b"left")
                second.write_bytes(header + b"right")
                self.assertNotEqual(digest_function(root), before)

    def test_invalid_utf8_markers_are_cache_misses(self) -> None:
        marker = self.root / stage.COMPLETE_MARKER
        marker.write_bytes(b"\xff")
        self.assertFalse(
            stage.artifact_is_complete(
                self.root, stage.WorkflowArtifact("test", 1, 1, "a" * 64)
            )
        )
        self.assertFalse(
            archives.extracted_cache_is_complete(self.root, marker, "a" * 64)
        )

    def test_cross_device_package_activation_and_rollback(self) -> None:
        for fail_second in (False, True):
            with (
                self.subTest(fail_second=fail_second),
                tempfile.TemporaryDirectory(dir=self.root) as directory,
            ):
                root = Path(directory)
                staging, output = root / "staging", root / "output"
                staging.mkdir()
                output.mkdir()
                results = []
                for name in ("one.tgz", "two.tgz"):
                    (staging / name).write_bytes(b"new")
                    (output / name).write_bytes(b"old")
                    results.append(stage.StagePackageResult(name, staging / name, ""))
                real_replace = Path.replace

                def replace(source, destination):
                    if (source.parent == staging) != (destination.parent == staging):
                        raise OSError(errno.EXDEV, "cross device")
                    if (
                        fail_second
                        and source.suffix == ".tmp"
                        and destination.name == "two.tgz"
                    ):
                        raise OSError("activation failed")
                    return real_replace(source, destination)

                with mock.patch.object(Path, "replace", replace):
                    if fail_second:
                        with self.assertRaisesRegex(OSError, "activation failed"):
                            stage.commit_staged_packages(results, output)
                    else:
                        activated = stage.commit_staged_packages(results, output)
                        self.assertEqual(
                            [item.pack_output for item in activated],
                            [output / "one.tgz", output / "two.tgz"],
                        )
                self.assertEqual(
                    {
                        p.name: p.read_bytes()
                        for p in output.iterdir()
                        if p.name != ".npm-activation.lock"
                    },
                    {
                        name: b"old" if fail_second else b"new"
                        for name in ("one.tgz", "two.tgz")
                    },
                )

    def test_command_capture_bounds_large_output_and_reports_failures(self) -> None:
        for status in (0, 7):
            with self.subTest(status=status):
                command = [
                    sys.executable,
                    "-c",
                    f"import sys; print('BEGIN' + 'x' * 100000 + 'END'); sys.exit({status})",
                ]
                if status:
                    with self.assertRaisesRegex(RuntimeError, "exit code 7") as error:
                        stage.run_command_capture(command)
                    log = str(error.exception)
                else:
                    log = stage.run_command_capture(command)
                self.assertIn("BEGIN", log)
                self.assertIn("END", log)
                self.assertIn("truncated", log)
                self.assertLess(len(log), stage.MAX_CAPTURED_LOG_CHARS + 1000)

    def test_codex_package_archive_extraction_is_reused(self) -> None:
        target = "x86_64-pc-windows-msvc"
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
        target = "x86_64-pc-windows-msvc"
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

    def test_extract_tar_data_rejects_legacy_archive_special_files(self) -> None:
        archive_path = self.root / "fifo.tar.gz"
        with tarfile.open(archive_path, "w:gz") as archive:
            member = tarfile.TarInfo("payload-fifo")
            member.type = tarfile.FIFOTYPE
            archive.addfile(member)

        with self.assertRaisesRegex(RuntimeError, "archive special files require"):
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

    def test_format_bytes_returns_rendered_size(self) -> None:
        self.assertEqual(stage.format_bytes(1024), "1.0 KiB")

    def test_extract_zstd_archive_decompresses_in_destination_directory(self) -> None:
        archive_path = self.root / "cache" / "artifact.zst"
        archive_path.parent.mkdir()
        archive_path.write_text("archive", encoding="utf-8")
        dest = self.root / "out" / "codex"
        observed_output: list[Path] = []

        def fake_check_call(cmd: list[str], **kwargs) -> None:
            output_path = Path(cmd[cmd.index("-o") + 1])
            observed_output.append(output_path)
            output_path.write_text("payload", encoding="utf-8")

        with mock.patch("scripts.stage_npm_archives.run_owned", fake_check_call):
            stage.extract_zstd_archive(archive_path, dest)

        self.assertEqual(dest.read_text(encoding="utf-8"), "payload")
        self.assertEqual(observed_output[0].parent, dest.parent)
        self.assertFalse(observed_output[0].exists())

    def test_extract_zstd_archive_explains_missing_zstd(self) -> None:
        archive_path = self.root / "artifact.zst"
        archive_path.write_text("archive", encoding="utf-8")

        with (
            mock.patch.object(
                stage.subprocess, "check_call", side_effect=FileNotFoundError
            ),
            self.assertRaisesRegex(RuntimeError, "zstd is required"),
        ):
            stage.extract_zstd_archive(archive_path, self.root / "out" / "codex")

    def test_failed_binary_extract_preserves_existing_vendor_binary(self) -> None:
        target = "x86_64-pc-windows-msvc"
        component = stage.BinaryComponent("codex", "codex", "codex.exe")
        dest = self.root / "vendor" / target / "codex" / "codex.exe"
        dest.parent.mkdir(parents=True)
        dest.write_bytes(b"existing")
        artifact = (
            self.root
            / "artifacts"
            / target
            / stage.archive_name_for_target(component.artifact_prefix, target)
        )
        artifact.parent.mkdir(parents=True)
        artifact.write_bytes(b"invalid archive")

        with (
            mock.patch.object(
                stage,
                "binary_archive_path",
                return_value=self.root / "artifact.zst",
            ),
            mock.patch.object(
                stage, "extract_zstd_archive", side_effect=RuntimeError("bad archive")
            ),
            self.assertRaisesRegex(RuntimeError, "bad archive"),
        ):
            stage.install_single_binary(
                self.root / "artifacts",
                self.root / "vendor",
                target,
                component,
            )

        self.assertEqual(dest.read_bytes(), b"existing")

    def test_install_single_binary_rejects_non_windows_target(self) -> None:
        component = stage.BinaryComponent("codex", "codex", "codex.exe")

        with self.assertRaisesRegex(
            RuntimeError, "Unsupported non-Windows native target"
        ):
            stage.install_single_binary(
                self.root / "artifacts",
                self.root / "vendor",
                "x86_64-unknown-linux-gnu",
                component,
            )

    def test_kernel_lock_serializes_concurrent_holders(self) -> None:
        lock_path = self.root / "cache" / ".artifact.lock"
        second_acquired = threading.Event()
        contention_seen = threading.Event()
        errors: list[BaseException] = []
        acquire = archives._acquire_file_lock

        def observe_acquire(fd: int) -> None:
            try:
                acquire(fd)
            except OSError as error:
                if archives._lock_error_is_contention(error):
                    contention_seen.set()
                raise

        def acquire_second() -> None:
            try:
                with stage.exclusive_file_lock(lock_path, timeout_seconds=3):
                    second_acquired.set()
            except BaseException as error:
                errors.append(error)

        with mock.patch.object(
            archives, "_acquire_file_lock", side_effect=observe_acquire
        ):
            with stage.exclusive_file_lock(lock_path):
                worker = threading.Thread(target=acquire_second)
                worker.start()
                contended = contention_seen.wait(timeout=2)
                acquired_while_held = second_acquired.is_set()
            worker.join(timeout=5)
        self.assertFalse(worker.is_alive())
        self.assertEqual(errors, [])
        self.assertTrue(contended, "second holder must attempt the held kernel lock")
        self.assertFalse(acquired_while_held)
        self.assertTrue(second_acquired.is_set())
        self.assertTrue(lock_path.exists())

    def test_lock_retries_contention_then_succeeds(self) -> None:
        lock_path = self.root / "cache" / ".retry.lock"
        contention = OSError(errno.EAGAIN, "busy")
        with mock.patch.object(
            archives, "_acquire_file_lock", side_effect=[contention, None]
        ) as acquire:
            with archives.exclusive_file_lock(
                lock_path, timeout_seconds=1, poll_seconds=0.001
            ):
                pass
        self.assertEqual(acquire.call_count, 2)

    def test_lock_propagates_permanent_error_without_retry(self) -> None:
        lock_path = self.root / "cache" / ".unsupported.lock"
        permanent = OSError(errno.ENOTSUP, "unsupported")
        with (
            mock.patch.object(
                archives, "_acquire_file_lock", side_effect=permanent
            ) as acquire,
            self.assertRaisesRegex(OSError, "unsupported"),
        ):
            with archives.exclusive_file_lock(
                lock_path, timeout_seconds=1, poll_seconds=0.001
            ):
                self.fail("permanent lock failure must not enter the context")
        acquire.assert_called_once()

    def test_lock_times_out_on_persistent_contention(self) -> None:
        lock_path = self.root / "cache" / ".timeout.lock"
        with (
            mock.patch.object(
                archives,
                "_acquire_file_lock",
                side_effect=OSError(errno.EAGAIN, "busy"),
            ),
            self.assertRaisesRegex(TimeoutError, "timed out acquiring lock"),
        ):
            with archives.exclusive_file_lock(
                lock_path, timeout_seconds=0.01, poll_seconds=0.001
            ):
                self.fail("contended lock must time out")

    def test_lock_initialization_failure_closes_but_keeps_stable_lock_file(
        self,
    ) -> None:
        lock_path = self.root / "cache" / ".artifact.lock"

        with (
            mock.patch.object(stage.os, "write", side_effect=OSError("disk full")),
            mock.patch.object(archives.os, "close", wraps=archives.os.close) as close,
            self.assertRaisesRegex(OSError, "disk full"),
        ):
            with stage.exclusive_file_lock(lock_path):
                self.fail("lock should not have been acquired")

        self.assertTrue(lock_path.exists())
        close.assert_called_once()
        with self.assertRaises(OSError):
            os.fstat(close.call_args.args[0])

    def test_head_mismatch_fails_without_explicit_override(self) -> None:
        stderr = io.StringIO()
        with (
            mock.patch.object(
                stage,
                "check_output_owned",
                side_effect=["current-head\n", b""],
            ),
            mock.patch.object(sys, "stderr", stderr),
            self.assertRaisesRegex(RuntimeError, "source/native mismatch"),
        ):
            stage.ensure_source_matches_workflow("workflow-head", repo_root=self.root)

        with (
            mock.patch.object(
                stage,
                "check_output_owned",
                side_effect=["current-head\n", b"?? dirty\0"],
            ),
            mock.patch.object(sys, "stderr", stderr),
        ):
            stage.ensure_source_matches_workflow(
                "workflow-head", repo_root=self.root, allow_mismatch=True
            )
        self.assertIn("WARNING: allowing source/native mismatch", stderr.getvalue())

    def test_source_validation_ignores_only_owned_paths(self) -> None:
        owned = self.root / "dist"
        with mock.patch.object(
            stage,
            "check_output_owned",
            side_effect=["workflow-head\n", b"?? dist/package.tgz\0"],
        ):
            stage.ensure_source_matches_workflow(
                "workflow-head", repo_root=self.root, owned_paths=[owned]
            )

        with (
            mock.patch.object(
                stage,
                "check_output_owned",
                side_effect=[
                    "workflow-head\n",
                    b"?? dist/package.tgz\0?? unrelated.txt\0",
                ],
            ),
            self.assertRaisesRegex(RuntimeError, "uncommitted changes"),
        ):
            stage.ensure_source_matches_workflow(
                "workflow-head", repo_root=self.root, owned_paths=[owned]
            )

    def test_archive_cache_key_uses_content_not_stat_tuple(self) -> None:
        archive_path = self.root / "archive.tar.gz"
        cache_root = self.root / "cache"
        archive_path.write_bytes(b"AAAA")
        fixed_time = 946684800
        os.utime(archive_path, ns=(fixed_time * 1_000_000_000,) * 2)

        def fake_extract(source: Path, destination: Path) -> None:
            (destination / "payload.bin").write_bytes(source.read_bytes())

        with mock.patch.object(archives, "extract_tar_data", fake_extract):
            first = archives.cached_codex_package_archive(
                archive_path, "target", cache_root
            )
            archive_path.write_bytes(b"BBBB")
            os.utime(archive_path, ns=(fixed_time * 1_000_000_000,) * 2)
            second = archives.cached_codex_package_archive(
                archive_path, "target", cache_root
            )

        self.assertNotEqual(first, second)
        self.assertEqual((second / "payload.bin").read_bytes(), b"BBBB")

    def test_stage_packages_returns_results_in_package_order(self) -> None:
        calls: list[tuple[str, bool]] = []
        second_collected = threading.Event()
        completion_order: list[str] = []
        as_completed = stage.as_completed

        def observe_completion(futures):
            for future in as_completed(futures, timeout=5):
                completion_order.append(future.result().package)
                yield future
                second_collected.set()

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
            if package == "codex":
                self.assertTrue(second_collected.wait(timeout=3))
            return stage.StagePackageResult(
                package=package,
                pack_output=output_dir / f"{package}.tgz",
                log="",
            )

        with (
            mock.patch.object(stage, "stage_package", fake_stage_package),
            mock.patch.object(stage, "as_completed", observe_completion),
        ):
            results = stage.stage_packages(
                ["codex", "codex-win32-x64"],
                "1.2.3",
                self.root,
                self.root,
                {},
                False,
                2,
            )

        self.assertEqual(
            [result.package for result in results],
            ["codex", "codex-win32-x64"],
        )
        self.assertCountEqual(
            calls,
            [("codex", True), ("codex-win32-x64", True)],
        )
        self.assertEqual(completion_order, ["codex-win32-x64", "codex"])


def relative_files(root: Path) -> set[str]:
    return {
        path.relative_to(root).as_posix() for path in root.rglob("*") if path.is_file()
    }


if __name__ == "__main__":
    unittest.main()
