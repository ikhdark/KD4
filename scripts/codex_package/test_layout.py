#!/usr/bin/env python3

from pathlib import Path
import json
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
    def test_prefers_hardlink_when_requested(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            src = root / "src.exe"
            dest = root / "dest.exe"
            src.write_text("binary", encoding="utf-8")

            with (
                mock.patch.object(layout.os, "link") as link,
                mock.patch.object(layout.shutil, "copyfile") as copyfile,
            ):
                layout.copy_file_for_staging(src, dest, prefer_hardlink=True)

            link.assert_called_once_with(src, dest)
            copyfile.assert_not_called()

    def test_falls_back_to_copy_when_hardlink_fails(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            src = root / "src.exe"
            dest = root / "dest.exe"
            src.write_text("binary", encoding="utf-8")

            with mock.patch.object(layout.os, "link", side_effect=OSError):
                layout.copy_file_for_staging(src, dest, prefer_hardlink=True)

            self.assertEqual(dest.read_text(encoding="utf-8"), "binary")

    def test_reuse_package_dir_removes_managed_paths_only(self) -> None:
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
            self.assertTrue(keep.is_file())

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

    def test_package_layout_prefers_hardlink_for_ripgrep(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            package_dir.mkdir()
            inputs = PackageInputs(
                entrypoint_bin=root / "codex",
                code_mode_host_bin=root / "codex-code-mode-host",
                rg_bin=root / "rg",
                zsh_bin=None,
                bwrap_bin=None,
                codex_command_runner_bin=None,
                codex_windows_sandbox_setup_bin=None,
            )

            with mock.patch.object(layout, "copy_executable") as copy_executable:
                layout.build_package_dir(
                    package_dir,
                    "1.2.3",
                    PACKAGE_VARIANTS["codex"],
                    TARGET_SPECS["x86_64-apple-darwin"],
                    inputs,
                )

        rg_calls = [
            call
            for call in copy_executable.call_args_list
            if call.args[0] == inputs.rg_bin
        ]
        self.assertEqual(len(rg_calls), 1)
        self.assertIs(rg_calls[0].kwargs["prefer_hardlink"], True)

    def test_package_validation_rejects_stale_version_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            package_dir.mkdir()
            for filename in ["codex", "codex-code-mode-host", "rg"]:
                path = root / filename
                path.write_text(filename, encoding="utf-8")
                path.chmod(0o755)
            inputs = PackageInputs(
                entrypoint_bin=root / "codex",
                code_mode_host_bin=root / "codex-code-mode-host",
                rg_bin=root / "rg",
                zsh_bin=None,
                bwrap_bin=None,
                codex_command_runner_bin=None,
                codex_windows_sandbox_setup_bin=None,
            )
            spec = TARGET_SPECS["x86_64-apple-darwin"]

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
                    include_zsh=False,
                )

            metadata.pop("version")
            metadata_path.write_text(json.dumps(metadata), encoding="utf-8")
            with self.assertRaisesRegex(RuntimeError, "non-empty string"):
                layout.validate_package_dir(
                    package_dir,
                    PACKAGE_VARIANTS["codex"],
                    spec,
                    include_zsh=False,
                )

    def test_app_server_package_variant_uses_app_server_entrypoint(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            package_dir.mkdir()
            for filename in ["codex-app-server", "codex-code-mode-host", "rg"]:
                path = root / filename
                path.write_text(filename, encoding="utf-8")
                path.chmod(0o755)
            inputs = PackageInputs(
                entrypoint_bin=root / "codex-app-server",
                code_mode_host_bin=root / "codex-code-mode-host",
                rg_bin=root / "rg",
                zsh_bin=None,
                bwrap_bin=None,
                codex_command_runner_bin=None,
                codex_windows_sandbox_setup_bin=None,
            )
            spec = TARGET_SPECS["x86_64-apple-darwin"]

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
            self.assertEqual(metadata["entrypoint"], "bin/codex-app-server")
            self.assertTrue((package_dir / "bin" / "codex-code-mode-host").is_file())
            layout.validate_package_dir(
                package_dir,
                PACKAGE_VARIANTS["codex-app-server"],
                spec,
                expected_version="1.2.3",
                include_zsh=False,
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
                (root / filename).write_text(filename, encoding="utf-8")
            inputs = PackageInputs(
                entrypoint_bin=root / "codex.exe",
                code_mode_host_bin=root / "codex-code-mode-host.exe",
                rg_bin=root / "rg.exe",
                zsh_bin=None,
                bwrap_bin=None,
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

            expected_script = layout.windows_apply_patch_alias_text(
                layout.PureWindowsPath("..") / "bin" / "codex.exe"
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
                include_zsh=False,
            )

            (package_dir / "codex-path" / "applypatch.bat").unlink()
            with self.assertRaises(RuntimeError) as cm:
                layout.validate_package_dir(
                    package_dir,
                    PACKAGE_VARIANTS["codex"],
                    spec,
                    include_zsh=False,
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
                    include_zsh=False,
                )
            self.assertIn("Invalid package file contents", str(cm.exception))


if __name__ == "__main__":
    unittest.main()
