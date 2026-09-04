#!/usr/bin/env python3

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
SOURCE_WRAPPER = REPO_ROOT / "tools" / "argument-comment-lint" / "run.py"
PREBUILT_WRAPPER = (
    REPO_ROOT / "tools" / "argument-comment-lint" / "run-prebuilt-linter.py"
)
_WINDOWS_TOOL_LAUNCHER_DIR: tempfile.TemporaryDirectory[str] | None = None


def windows_tool_launcher() -> Path:
    """Build a native shim because CreateProcess cannot execute .cmd files."""
    global _WINDOWS_TOOL_LAUNCHER_DIR
    if _WINDOWS_TOOL_LAUNCHER_DIR is not None:
        return Path(_WINDOWS_TOOL_LAUNCHER_DIR.name) / "tool-launcher.exe"

    _WINDOWS_TOOL_LAUNCHER_DIR = tempfile.TemporaryDirectory(
        prefix="argument-comment-lint-launcher-"
    )
    root = Path(_WINDOWS_TOOL_LAUNCHER_DIR.name)
    source = root / "tool-launcher.rs"
    executable = root / "tool-launcher.exe"
    source.write_text(
        """\
use std::env;
use std::path::Path;
use std::process::{Command, exit};

fn main() {
    let python = env::var_os("KD4_FAKE_TOOL_PYTHON").expect("KD4_FAKE_TOOL_PYTHON");
    let script = env::var_os("KD4_FAKE_TOOL_SCRIPT").expect("KD4_FAKE_TOOL_SCRIPT");
    let current = env::current_exe().expect("current executable");
    let tool = Path::new(&current)
        .file_stem()
        .expect("tool filename");
    let status = Command::new(python)
        .arg(script)
        .arg(tool)
        .args(env::args_os().skip(1))
        .status()
        .expect("start fake tool process");
    exit(status.code().unwrap_or(1));
}
""",
        encoding="utf-8",
        newline="\n",
    )
    subprocess.run(
        ["rustc", "--edition=2021", str(source), "-o", str(executable)],
        text=True,
        capture_output=True,
        check=True,
    )
    return executable


class WrapperCommonTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tempdir.cleanup)
        self.root = Path(self.tempdir.name)
        self.bin_dir = self.root / "bin"
        self.bin_dir.mkdir()
        self.capture_path = self.root / "capture.json"
        tool_script = self.root / "fake_tool.py"
        tool_script.write_text(
            """\
import json
import os
import sys
from pathlib import Path

tool = sys.argv[1]
args = sys.argv[2:]
if tool == "rustup":
    if args == ["toolchain", "list"]:
        print("nightly-2025-09-18-x86_64-pc-windows-msvc (active)")
        raise SystemExit(0)
    if args == ["show", "home"]:
        print(r"C:\\fake-rustup-home")
        raise SystemExit(0)
    raise SystemExit(3)
if tool == "dotslash":
    print(os.environ["FAKE_DOTSLASH_ENTRYPOINT"])
    raise SystemExit(0)
if tool == "cargo-dylint":
    raise SystemExit(int(os.environ.get("FAKE_CARGO_DYLINT_EXIT", "0")))
if tool != "cargo":
    raise SystemExit(0)

Path(os.environ["FAKE_CAPTURE_PATH"]).write_text(
    json.dumps(
        {
            "argv": args,
            "DYLINT_RUSTFLAGS": os.environ.get("DYLINT_RUSTFLAGS"),
            "CARGO_INCREMENTAL": os.environ.get("CARGO_INCREMENTAL"),
        }
    ),
    encoding="utf-8",
)
raise SystemExit(int(os.environ.get("FAKE_CARGO_EXIT", "0")))
""",
            encoding="utf-8",
        )
        if os.name == "nt":
            launcher = windows_tool_launcher()
            for command in (
                "cargo",
                "cargo-dylint",
                "dylint-link",
                "rustup",
                "dotslash",
            ):
                shutil.copy2(launcher, self.bin_dir / f"{command}.exe")
        else:
            for command in (
                "cargo",
                "cargo-dylint",
                "dylint-link",
                "rustup",
                "dotslash",
            ):
                shim = self.bin_dir / command
                shim.write_text(
                    f"#!{sys.executable}\n"
                    f'exec(compile(open({str(tool_script)!r}, "rb").read(), '
                    f'{str(tool_script)!r}, "exec"))\n',
                    encoding="utf-8",
                    newline="\n",
                )
                shim.chmod(0o755)
        self.tool_script = tool_script

    def wrapper_env(self) -> dict[str, str]:
        env = os.environ.copy()
        env["PATH"] = f"{self.bin_dir}{os.pathsep}{env.get('PATH', '')}"
        env["FAKE_CAPTURE_PATH"] = str(self.capture_path)
        env["KD4_FAKE_TOOL_PYTHON"] = sys.executable
        env["KD4_FAKE_TOOL_SCRIPT"] = str(self.tool_script)
        env.pop("DYLINT_RUSTFLAGS", None)
        env.pop("CARGO_INCREMENTAL", None)
        return env

    def run_source_wrapper(
        self,
        *args: str,
        env: dict[str, str] | None = None,
        expected_returncode: int = 0,
    ) -> dict[str, object]:
        completed = subprocess.run(
            [sys.executable, str(SOURCE_WRAPPER), *args],
            cwd=REPO_ROOT,
            env=self.wrapper_env() if env is None else env,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
        self.assertEqual(
            completed.returncode,
            expected_returncode,
            msg=completed.stderr or completed.stdout,
        )
        self.assertTrue(self.capture_path.is_file(), completed.stderr)
        return json.loads(self.capture_path.read_text(encoding="utf-8"))

    def cargo_args(self, capture: dict[str, object]) -> list[str]:
        argv = capture["argv"]
        self.assertIsInstance(argv, list)
        return argv

    def test_source_wrapper_defaults_workspace_and_all_targets_through_process(
        self,
    ) -> None:
        capture = self.run_source_wrapper()

        self.assertEqual(
            self.cargo_args(capture),
            [
                "dylint",
                "--path",
                str(REPO_ROOT / "tools" / "argument-comment-lint"),
                "--all",
                "--manifest-path",
                str(REPO_ROOT / "codex-rs" / "Cargo.toml"),
                "--workspace",
                "--no-deps",
                "--",
                "--all-targets",
            ],
        )

    def test_source_wrapper_preserves_one_forwarded_cargo_separator(self) -> None:
        capture = self.run_source_wrapper("-p", "codex-core", "--", "--tests")

        self.assertEqual(
            self.cargo_args(capture),
            [
                "dylint",
                "--path",
                str(REPO_ROOT / "tools" / "argument-comment-lint"),
                "--all",
                "--manifest-path",
                str(REPO_ROOT / "codex-rs" / "Cargo.toml"),
                "--no-deps",
                "-p",
                "codex-core",
                "--",
                "--tests",
            ],
        )

    def test_source_wrapper_fix_does_not_add_all_targets(self) -> None:
        capture = self.run_source_wrapper("--fix", "-p", "codex-core")

        args = self.cargo_args(capture)
        self.assertEqual(args[-3:], ["--fix", "-p", "codex-core"])
        self.assertNotIn("--all-targets", args)

    def test_source_wrapper_preserves_explicit_manifest_and_workspace(self) -> None:
        custom_manifest = self.root / "custom" / "Cargo.toml"
        capture = self.run_source_wrapper(
            "--manifest-path",
            str(custom_manifest),
            "--workspace",
            "--no-deps",
            "--",
            "--bins",
        )

        args = self.cargo_args(capture)
        self.assertEqual(
            args[-6:],
            [
                "--manifest-path",
                str(custom_manifest),
                "--workspace",
                "--no-deps",
                "--",
                "--bins",
            ],
        )
        self.assertEqual(args.count("--manifest-path"), 1)

    def test_source_wrapper_explicit_manifest_does_not_force_workspace(self) -> None:
        custom_manifest = self.root / "custom" / "Cargo.toml"
        capture = self.run_source_wrapper("--manifest-path", str(custom_manifest))

        args = self.cargo_args(capture)
        self.assertEqual(
            args[-5:],
            [
                "--no-deps",
                "--manifest-path",
                str(custom_manifest),
                "--",
                "--all-targets",
            ],
        )
        self.assertNotIn("--workspace", args)

    def test_source_wrapper_process_receives_default_strict_lint_environment(
        self,
    ) -> None:
        capture = self.run_source_wrapper()

        self.assertEqual(
            capture["DYLINT_RUSTFLAGS"],
            "-D argument-comment-mismatch "
            "-D uncommented-anonymous-literal-argument "
            "-A unknown_lints",
        )
        self.assertEqual(capture["CARGO_INCREMENTAL"], "0")

    def test_source_and_prebuilt_entrypoints_exit_with_child_status(self) -> None:
        source_env = self.wrapper_env()
        source_env["FAKE_CARGO_EXIT"] = "23"
        self.run_source_wrapper(env=source_env, expected_returncode=23)

        package_root = self.root / "package"
        package_bin = package_root / "bin"
        package_lib = package_root / "lib"
        package_bin.mkdir(parents=True)
        package_lib.mkdir()
        package_entrypoint = package_bin / "argument-comment-lint.exe"
        package_entrypoint.write_bytes(b"entrypoint")
        cargo_dylint = package_bin / "cargo-dylint.exe"
        shim_name = "cargo-dylint.exe" if os.name == "nt" else "cargo-dylint"
        shutil.copy2(self.bin_dir / shim_name, cargo_dylint)
        (package_lib / "argument-comment-lint@nightly-2025-09-18-host.dll").write_bytes(
            b"lint-library"
        )
        prebuilt_env = self.wrapper_env()
        prebuilt_env["FAKE_DOTSLASH_ENTRYPOINT"] = str(package_entrypoint)
        prebuilt_env["FAKE_CARGO_DYLINT_EXIT"] = "19"
        prebuilt_env["CODEX_ARGUMENT_COMMENT_LINT_SKIP_RUSTUP_SHIMS"] = "1"

        completed = subprocess.run(
            [sys.executable, str(PREBUILT_WRAPPER)],
            cwd=REPO_ROOT,
            env=prebuilt_env,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )

        self.assertEqual(completed.returncode, 19, completed.stderr)


if __name__ == "__main__":
    unittest.main()
