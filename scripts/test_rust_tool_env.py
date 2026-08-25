from __future__ import annotations

import tempfile
import subprocess
import sys
import unittest
from pathlib import Path

from scripts import rust_tool_env


class RustToolEnvTest(unittest.TestCase):
    def test_just_shell_loads_shared_policy_outside_the_repository(self) -> None:
        just_shell = Path(__file__).with_name("just-shell.py").resolve()
        with tempfile.TemporaryDirectory() as temp:
            completed = subprocess.run(
                [
                    sys.executable,
                    "-c",
                    "import runpy, sys; runpy.run_path(sys.argv[1], run_name='loaded')",
                    str(just_shell),
                ],
                cwd=temp,
                check=False,
                capture_output=True,
                text=True,
            )

        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_shared_sccache_policy_handles_wrappers_and_cache_override(self) -> None:
        self.assertTrue(rust_tool_env.is_sccache_wrapper("C:/tools/sccache.exe"))
        self.assertFalse(rust_tool_env.is_sccache_wrapper("cachepot"))
        self.assertEqual(rust_tool_env.sccache_cache_size({}), "80G")
        self.assertEqual(
            rust_tool_env.sccache_cache_size({"CODEX_SCCACHE_CACHE_SIZE": "100G"}),
            "100G",
        )

    def test_windows_linker_fallback_order_is_shared(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            scoop = root / "custom-scoop"
            user = root / "user"
            default = root / "program-files" / "lld-link.exe"
            expected = scoop / "apps" / "llvm" / "current" / "bin" / "lld-link.exe"
            expected.parent.mkdir(parents=True)
            expected.write_text("", encoding="utf-8")

            result = rust_tool_env.find_windows_lld_link(
                {"SCOOP": str(scoop), "USERPROFILE": str(user)},
                which=lambda _program: None,
                default_path=default,
            )

        self.assertEqual(result, str(expected))


if __name__ == "__main__":
    unittest.main()
