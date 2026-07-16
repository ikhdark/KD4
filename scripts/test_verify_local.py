from __future__ import annotations

import ast
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest


REPO_ROOT = Path(__file__).resolve().parents[1]
WRAPPER = REPO_ROOT / "scripts" / "verify_local.py"


class VerifyLocalCompatibilityTest(unittest.TestCase):
    def test_real_wrapper_matches_rust_cli_bytes_and_exit_code(self) -> None:
        binary = (
            REPO_ROOT
            / "codex-rs"
            / "target"
            / "debug"
            / ("codex-verify-local.exe" if os.name == "nt" else "codex-verify-local")
        )
        if not binary.is_file():
            self.skipTest("build codex-verify-local before running the parity check")
        arguments = [
            "--plan",
            "--json",
            "--changed=scripts/verify_local.py",
            f"--repository-root={REPO_ROOT}",
        ]
        env = os.environ.copy()
        env["CODEX_VERIFY_LOCAL_PYTHON"] = sys.executable
        direct = subprocess.run(
            [str(binary), *arguments],
            cwd=REPO_ROOT,
            env=env,
            capture_output=True,
            check=False,
        )
        env["CODEX_VERIFY_LOCAL_BIN"] = str(binary)
        wrapped = subprocess.run(
            [sys.executable, str(WRAPPER), *arguments],
            cwd=REPO_ROOT,
            env=env,
            capture_output=True,
            check=False,
        )
        self.assertEqual(wrapped.stdout, direct.stdout)
        self.assertEqual(wrapped.stderr, direct.stderr)
        self.assertEqual(wrapped.returncode, direct.returncode)

    def test_wrapper_forwards_stdout_stderr_and_exit_code_byte_for_byte(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fake = Path(temp_dir) / "fake_verifier.py"
            fake.write_text(
                textwrap.dedent(
                    """
                    import os
                    import sys

                    os.write(1, b'out\\x00\\xe2\\x98\\x83')
                    os.write(2, b'err\\r\\nraw')
                    raise SystemExit(5)
                    """
                ),
                encoding="utf-8",
            )
            env = os.environ.copy()
            env["CODEX_VERIFY_LOCAL_COMMAND"] = json.dumps(
                [sys.executable, str(fake)]
            )
            completed = subprocess.run(
                [sys.executable, str(WRAPPER), "--plan", "--json"],
                cwd=REPO_ROOT,
                env=env,
                capture_output=True,
                check=False,
            )
        self.assertEqual(completed.stdout, b"out\x00\xe2\x98\x83")
        self.assertEqual(completed.stderr, b"err\r\nraw")
        self.assertEqual(completed.returncode, 5)

    def test_wrapper_preserves_argument_boundaries(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fake = Path(temp_dir) / "echo_args.py"
            fake.write_text(
                "import json, sys; print(json.dumps(sys.argv[1:]))\n",
                encoding="utf-8",
            )
            env = os.environ.copy()
            env["CODEX_VERIFY_LOCAL_COMMAND"] = json.dumps(
                [sys.executable, str(fake)]
            )
            completed = subprocess.run(
                [
                    sys.executable,
                    str(WRAPPER),
                    "--changed=--allow-workspace",
                    "--changed=a path/with spaces.py",
                ],
                cwd=REPO_ROOT,
                env=env,
                capture_output=True,
                check=False,
                text=True,
            )
        self.assertEqual(completed.returncode, 0)
        self.assertEqual(
            json.loads(completed.stdout),
            ["--changed=--allow-workspace", "--changed=a path/with spaces.py"],
        )

    def test_python_files_contain_no_planning_or_finalization_policy(self) -> None:
        forbidden_names = {
            "plan_commands",
            "select_scope",
            "owner_commands",
            "surface_commands",
            "execute_plan",
            "execute_command",
            "plan_to_json",
            "result_to_json",
            "cache_key",
            "load_cargo_metadata",
            "dirty_files",
        }
        forbidden_literals = {
            "NEEDS_REGEN",
            "NEEDS_SCOPE",
            "INCONCLUSIVE",
            "VERIFIED (no proof needed)",
        }
        for path in sorted((REPO_ROOT / "scripts").glob("verify_local*.py")):
            source = path.read_text(encoding="utf-8")
            tree = ast.parse(source, filename=str(path))
            defined = {
                node.name
                for node in ast.walk(tree)
                if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef))
            }
            self.assertTrue(forbidden_names.isdisjoint(defined), path)
            for literal in forbidden_literals:
                self.assertNotIn(literal, source, path)


if __name__ == "__main__":
    unittest.main()
