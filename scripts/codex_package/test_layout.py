#!/usr/bin/env python3

from pathlib import Path
import json
import hashlib
import struct
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from codex_package import layout
from codex_package.targets import PACKAGE_VARIANTS
from codex_package.targets import PackageInputs
from codex_package.targets import TARGET_SPECS


class CopyFileForStagingTest(unittest.TestCase):
    def setUp(self) -> None:
        # These fixtures exercise PE parsing; only launching a real Windows process is mocked.
        launch = mock.patch.object(
            layout.subprocess, "run", return_value=mock.Mock(stdout="codex 1.2.3")
        )
        launch.start()
        self.addCleanup(launch.stop)

    def test_reuse_package_dir_removes_all_residue(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            package_dir = Path(temp_dir) / "package"
            package_dir.mkdir()
            for relative_path in [
                Path("bin") / "old",
                Path("codex-resources") / "old",
                Path("codex-path") / "old",
            ]:
                path = package_dir / relative_path
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("stale", encoding="utf-8")
            (package_dir / "codex-package.json").write_text("{}", encoding="utf-8")
            keep = package_dir / "custom-cache" / "keep"
            keep.parent.mkdir()
            keep.write_text("keep", encoding="utf-8")

            layout.prepare_package_dir(package_dir, force=False, reuse=True)

            self.assertFalse((package_dir / "bin").exists())
            self.assertFalse((package_dir / "codex-resources").exists())
            self.assertFalse((package_dir / "codex-path").exists())
            self.assertFalse((package_dir / "codex-package.json").exists())
            self.assertFalse(keep.exists())

    def test_destination_preflight_does_not_remove_existing_output(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            package_dir = Path(temp_dir) / "package"
            package_dir.mkdir()
            existing = package_dir / "keep"
            existing.write_text("keep", encoding="utf-8")

            with self.assertRaisesRegex(RuntimeError, "not empty"):
                layout.validate_package_dir_destination(
                    package_dir,
                    force=False,
                    reuse=False,
                )

            self.assertEqual(existing.read_text(encoding="utf-8"), "keep")

    def test_remove_tree_uses_onerror_on_python_without_onexc(self) -> None:
        path = Path("package")

        with (
            mock.patch.object(layout, "rmtree_supports_onexc", return_value=False),
            mock.patch.object(layout.shutil, "rmtree") as rmtree,
            mock.patch.object(layout.os, "chmod") as chmod,
        ):
            layout.remove_tree_allow_readonly(path)
            rmtree.assert_called_once()
            self.assertEqual(rmtree.call_args.args, (path,))
            self.assertNotIn("onexc", rmtree.call_args.kwargs)
            onerror = rmtree.call_args.kwargs["onerror"]
            retry = mock.Mock()
            failed_path = Path("readonly")

            onerror(retry, failed_path, (PermissionError, PermissionError(), None))

            chmod.assert_called_once_with(failed_path, layout.stat.S_IWRITE)
            retry.assert_called_once_with(failed_path)

    def test_package_layout_stages_independent_runtime_binaries(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            package_dir.mkdir()
            inputs = PackageInputs(
                entrypoint_bin=root / "codex",
                code_mode_host_bin=root / "codex-code-mode-host",
                rg_bin=root / "rg",
                codex_command_runner_bin=root / "codex-command-runner",
                codex_windows_sandbox_setup_bin=root / "codex-windows-sandbox-setup",
            )
            binaries = {
                "bin/codex.exe": inputs.entrypoint_bin,
                "bin/codex-code-mode-host.exe": inputs.code_mode_host_bin,
                "codex-path/rg.exe": inputs.rg_bin,
                "codex-resources/codex-command-runner.exe": inputs.codex_command_runner_bin,
                "codex-resources/codex-windows-sandbox-setup.exe": inputs.codex_windows_sandbox_setup_bin,
            }
            for source in binaries.values():
                source.write_bytes(source.name.encode())

            layout.build_package_dir(
                package_dir,
                "1.2.3",
                PACKAGE_VARIANTS["codex"],
                TARGET_SPECS["x86_64-pc-windows-msvc"],
                inputs,
            )

            for relative_path, source in binaries.items():
                with self.subTest(binary=relative_path):
                    staged = package_dir / relative_path
                    original = source.name.encode()
                    self.assertEqual(staged.read_bytes(), original)
                    staged.write_bytes(b"edited staged binary")
                    self.assertEqual(source.read_bytes(), original)
                    source.write_bytes(b"rebuilt source binary")
                    self.assertEqual(staged.read_bytes(), b"edited staged binary")

    def test_package_validation_rejects_stale_version_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            package_dir.mkdir()
            for filename in [
                "codex.exe",
                "codex-code-mode-host.exe",
                "rg.exe",
                "codex-command-runner.exe",
                "codex-windows-sandbox-setup.exe",
            ]:
                path = root / filename
                write_pe(path)
                path.chmod(0o755)
            inputs = PackageInputs(
                entrypoint_bin=root / "codex.exe",
                code_mode_host_bin=root / "codex-code-mode-host.exe",
                rg_bin=root / "rg.exe",
                codex_command_runner_bin=root / "codex-command-runner.exe",
                codex_windows_sandbox_setup_bin=root
                / "codex-windows-sandbox-setup.exe",
            )
            spec = TARGET_SPECS["x86_64-pc-windows-msvc"]

            layout.build_package_dir(
                package_dir,
                "1.2.3",
                PACKAGE_VARIANTS["codex"],
                spec,
                inputs,
            )
            metadata_path = package_dir / "codex-package.json"
            metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
            metadata["version"] = "9.9.9"
            metadata_path.write_text(json.dumps(metadata), encoding="utf-8")

            with self.assertRaisesRegex(RuntimeError, "version"):
                layout.validate_package_dir(
                    package_dir,
                    PACKAGE_VARIANTS["codex"],
                    spec,
                    expected_version="1.2.3",
                )

            metadata.pop("version")
            metadata_path.write_text(json.dumps(metadata), encoding="utf-8")
            with self.assertRaisesRegex(RuntimeError, "non-empty string"):
                layout.validate_package_dir(
                    package_dir,
                    PACKAGE_VARIANTS["codex"],
                    spec,
                )

    def test_app_server_package_variant_uses_app_server_entrypoint(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            package_dir.mkdir()
            for filename in [
                "codex-app-server.exe",
                "codex-code-mode-host.exe",
                "rg.exe",
                "codex-command-runner.exe",
                "codex-windows-sandbox-setup.exe",
            ]:
                path = root / filename
                write_pe(path)
                path.chmod(0o755)
            inputs = PackageInputs(
                entrypoint_bin=root / "codex-app-server.exe",
                code_mode_host_bin=root / "codex-code-mode-host.exe",
                rg_bin=root / "rg.exe",
                codex_command_runner_bin=root / "codex-command-runner.exe",
                codex_windows_sandbox_setup_bin=root
                / "codex-windows-sandbox-setup.exe",
            )
            spec = TARGET_SPECS["x86_64-pc-windows-msvc"]

            layout.build_package_dir(
                package_dir,
                "1.2.3",
                PACKAGE_VARIANTS["codex-app-server"],
                spec,
                inputs,
            )

            metadata = json.loads(
                (package_dir / "codex-package.json").read_text(encoding="utf-8")
            )
            self.assertEqual(metadata["variant"], "codex-app-server")
            self.assertEqual(metadata["entrypoint"], "bin/codex-app-server.exe")
            self.assertTrue(
                (package_dir / "bin" / "codex-code-mode-host.exe").is_file()
            )
            layout.validate_package_dir(
                package_dir,
                PACKAGE_VARIANTS["codex-app-server"],
                spec,
                expected_version="1.2.3",
            )

    def test_windows_package_layout_writes_apply_patch_aliases(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            package_dir.mkdir()
            for filename in [
                "codex.exe",
                "codex-code-mode-host.exe",
                "rg.exe",
                "codex-command-runner.exe",
                "codex-windows-sandbox-setup.exe",
            ]:
                write_pe(root / filename)
            inputs = PackageInputs(
                entrypoint_bin=root / "codex.exe",
                code_mode_host_bin=root / "codex-code-mode-host.exe",
                rg_bin=root / "rg.exe",
                codex_command_runner_bin=root / "codex-command-runner.exe",
                codex_windows_sandbox_setup_bin=root
                / "codex-windows-sandbox-setup.exe",
            )
            spec = TARGET_SPECS["x86_64-pc-windows-msvc"]

            layout.build_package_dir(
                package_dir,
                "1.2.3",
                PACKAGE_VARIANTS["codex"],
                spec,
                inputs,
            )

            expected_script = (
                '@echo off\n"%~dp0..\\bin\\codex.exe" --codex-run-as-apply-patch %*\n'
            )
            for alias in ["apply_patch.bat", "applypatch.bat"]:
                self.assertEqual(
                    (package_dir / "codex-path" / alias).read_text(encoding="utf-8"),
                    expected_script,
                )
            layout.validate_package_dir(
                package_dir,
                PACKAGE_VARIANTS["codex"],
                spec,
            )

            (package_dir / "codex-path" / "applypatch.bat").unlink()
            with self.assertRaises(RuntimeError) as cm:
                layout.validate_package_dir(
                    package_dir,
                    PACKAGE_VARIANTS["codex"],
                    spec,
                )
            self.assertIn("applypatch.bat", str(cm.exception))

            (package_dir / "codex-path" / "applypatch.bat").write_text(
                "@echo off\nexit /b 1\n", encoding="utf-8"
            )
            with self.assertRaises(RuntimeError) as cm:
                layout.validate_package_dir(
                    package_dir,
                    PACKAGE_VARIANTS["codex"],
                    spec,
                )
            self.assertIn("Package file digest mismatch", str(cm.exception))

    def test_canonical_validation_and_staged_input_identity(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package = root / "package"
            package.mkdir()
            sources = [
                root / name
                for name in [
                    "entry.exe",
                    "host.exe",
                    "rg.exe",
                    "runner.exe",
                    "setup.exe",
                ]
            ]
            for source in sources:
                write_pe(source)
            inputs = PackageInputs(*sources)
            variant = PACKAGE_VARIANTS["codex"]
            spec = TARGET_SPECS["x86_64-pc-windows-msvc"]

            def build():
                layout.build_package_dir(
                    package,
                    "1.2.3",
                    variant,
                    spec,
                    inputs,
                    build_identity={"source": "test"},
                )

            def refresh_inventory():
                metadata = json.loads((package / "codex-package.json").read_text())
                metadata["files"] = layout.package_file_inventory(
                    package, variant=variant, spec=spec
                )
                metadata["bundleId"] = hashlib.sha256(
                    json.dumps(
                        metadata["files"], sort_keys=True, separators=(",", ":")
                    ).encode()
                ).hexdigest()
                layout.write_json(package / "codex-package.json", metadata)

            build()
            layout.validate_package_dir(package, variant, spec)
            metadata = json.loads((package / "codex-package.json").read_text())
            self.assertEqual(
                metadata["buildIdentity"]["inputs"]["entrypoint"],
                {
                    "size": 128,
                    "sha256": hashlib.sha256(sources[0].read_bytes()).hexdigest(),
                },
            )
            for relative in [
                "bin/codex.exe",
                "bin/codex-code-mode-host.exe",
                "codex-path/rg.exe",
                "codex-resources/codex-command-runner.exe",
                "codex-resources/codex-windows-sandbox-setup.exe",
            ]:
                with self.subTest(relative=relative):
                    (package / relative).unlink()
                    refresh_inventory()
                    with self.assertRaisesRegex(
                        RuntimeError, "Missing required package files"
                    ):
                        layout.validate_package_dir(package, variant, spec)
                    build()
                    (package / relative).write_bytes(b"not an executable")
                    refresh_inventory()
                    with self.assertRaisesRegex(RuntimeError, "Invalid PE executable"):
                        layout.validate_package_dir(package, variant, spec)
                    build()
            nested = package / "bin" / "codex-package.json"
            nested.write_text("nested metadata")
            with self.assertRaisesRegex(RuntimeError, "inventory mismatch"):
                layout.validate_package_dir(package, variant, spec)
            build()
            metadata = json.loads((package / "codex-package.json").read_text())
            self.assertIn(
                "bin/codex-package.json", [entry["path"] for entry in metadata["files"]]
            )
            layout.validate_package_dir(package, variant, spec)
            nested.write_text("tampered")
            with self.assertRaisesRegex(RuntimeError, "digest mismatch"):
                layout.validate_package_dir(package, variant, spec)


def write_pe(path: Path) -> None:
    contents = bytearray(128)
    contents[:2] = b"MZ"
    struct.pack_into("<I", contents, 0x3C, 64)
    contents[64:68] = b"PE\0\0"
    struct.pack_into("<H", contents, 68, 0x8664)
    path.write_bytes(contents)


if __name__ == "__main__":
    unittest.main()
