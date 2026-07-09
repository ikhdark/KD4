#!/usr/bin/env python3

from pathlib import Path
import stat
import sys
import unittest
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from codex_package import targets
from codex_package.targets import BINARY_TARGETS
from codex_package.targets import PACKAGE_VARIANTS
from codex_package.targets import RELEASE_TARGETS
from codex_package.targets import SUPPORTED_TARGETS
from codex_package.targets import SUPPORTED_VARIANTS
from codex_package.targets import TARGET_SPECS
from codex_package.targets import default_target
from codex_package.targets import is_executable
from codex_package.targets import normalize_machine
from codex_package.targets import resolve_input_path


class TargetMetadataTest(unittest.TestCase):
    def test_target_spec_is_slotted_and_precomputes_binary_names(self) -> None:
        spec = TARGET_SPECS["x86_64-pc-windows-msvc"]

        self.assertFalse(hasattr(spec, "__dict__"))
        self.assertEqual(spec.exe_suffix, ".exe")
        self.assertEqual(spec.rg_name, "rg.exe")

    def test_supported_choices_are_sorted_tuples(self) -> None:
        self.assertEqual(SUPPORTED_TARGETS, tuple(sorted(TARGET_SPECS)))
        self.assertEqual(SUPPORTED_VARIANTS, tuple(sorted(PACKAGE_VARIANTS)))
        self.assertEqual(set(BINARY_TARGETS), set(RELEASE_TARGETS))

    def test_release_targets_match_standalone_installers(self) -> None:
        install_sh = (
            targets.REPO_ROOT / "scripts" / "install" / "install.sh"
        ).read_text(encoding="utf-8")
        install_ps1 = (
            targets.REPO_ROOT / "scripts" / "install" / "install.ps1"
        ).read_text(encoding="utf-8")

        for release in RELEASE_TARGETS.values():
            installer = install_ps1 if release.host_system == "windows" else install_sh
            with self.subTest(target=release.target):
                self.assertIn(release.target, installer)
                self.assertIn(release.npm_tag, installer)
                self.assertIn(release.platform_label, installer)

    def test_entrypoint_name_uses_precomputed_variant_target_names(self) -> None:
        variant = PACKAGE_VARIANTS["codex"]

        self.assertEqual(
            variant.entrypoint_name(TARGET_SPECS["x86_64-pc-windows-msvc"]),
            "codex.exe",
        )
        self.assertEqual(
            variant.entrypoint_name(TARGET_SPECS["aarch64-apple-darwin"]),
            "codex",
        )

    def test_default_target_is_cached_for_process_lifetime(self) -> None:
        calls = {"system": 0, "machine": 0}

        def fake_system() -> str:
            calls["system"] += 1
            return "Windows"

        def fake_machine() -> str:
            calls["machine"] += 1
            return "AMD64"

        default_target.cache_clear()
        with (
            patch.object(targets.platform, "system", fake_system),
            patch.object(targets.platform, "machine", fake_machine),
        ):
            self.assertEqual(default_target(), "x86_64-pc-windows-msvc")
            self.assertEqual(default_target(), "x86_64-pc-windows-msvc")

        self.assertEqual(calls, {"system": 1, "machine": 1})
        default_target.cache_clear()

    def test_normalize_machine_uses_alias_table(self) -> None:
        self.assertEqual(normalize_machine("AMD64"), "x86_64")
        self.assertEqual(normalize_machine("arm64"), "aarch64")
        self.assertEqual(normalize_machine("mips64"), "mips64")


class ResolveInputPathTest(unittest.TestCase):
    def test_missing_path_does_not_resolve(self) -> None:
        class MissingPath:
            def is_file(self) -> bool:
                return False

            def resolve(self) -> Path:
                raise AssertionError("resolve should not run for missing inputs")

            def __str__(self) -> str:
                return "missing-tool"

        with self.assertRaisesRegex(RuntimeError, "does not exist: missing-tool"):
            resolve_input_path(MissingPath(), "test tool", "--test-tool")

    def test_canonicalization_can_be_skipped_for_explicit_inputs(self) -> None:
        class ExistingExe:
            suffix = ".exe"

            def is_file(self) -> bool:
                return True

            def resolve(self) -> Path:
                raise AssertionError("resolve should not run")

            def stat(self) -> SimpleNamespace:
                return SimpleNamespace(st_mode=stat.S_IFREG)

        path = ExistingExe()
        with patch.object(targets.platform, "system", lambda: "Windows"):
            self.assertIs(
                resolve_input_path(
                    path,
                    "test tool",
                    "--test-tool",
                    canonicalize=False,
                ),
                path,
            )

    def test_windows_exe_is_executable_without_unix_mode_bits(self) -> None:
        fake_path = SimpleNamespace(
            suffix=".exe",
            is_file=lambda: True,
            stat=lambda: SimpleNamespace(st_mode=stat.S_IFREG),
        )

        with patch.object(targets.platform, "system", lambda: "Windows"):
            self.assertTrue(is_executable(fake_path))

    def test_windows_host_accepts_extensionless_cross_target_binary(self) -> None:
        # A prebuilt Linux/mac binary staged from a Windows host has no
        # extension and no meaningful execute bits; existence must suffice.
        fake_path = SimpleNamespace(
            suffix="",
            is_file=lambda: True,
            stat=lambda: SimpleNamespace(st_mode=stat.S_IFREG),
        )

        with patch.object(targets.platform, "system", lambda: "Windows"):
            self.assertTrue(is_executable(fake_path))


if __name__ == "__main__":
    unittest.main()
