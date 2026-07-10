#!/usr/bin/env python3
"""Run root package maintenance commands from one maintained target list."""

from __future__ import annotations

import argparse
import os
import subprocess
from pathlib import Path
from shutil import which
from typing import Sequence


REPO_ROOT = Path(__file__).resolve().parents[1]

PRETTIER_TARGETS = [
    "package.json",
    "knip.json",
    "pnpm-workspace.yaml",
    "eslint.config.mjs",
    "docs/*.md",
    ".github/workflows/*.yml",
    "codex-cli/**/*.js",
    "sdk/typescript/**/*.js",
    "sdk/typescript/**/*.ts",
]


def python_source_targets() -> list[str]:
    return sorted(
        path.relative_to(REPO_ROOT).as_posix()
        for path in (REPO_ROOT / "scripts").rglob("*.py")
        if "__pycache__" not in path.parts and ".venv" not in path.parts
    )


def python_unittest_targets() -> list[str]:
    return sorted(
        path.relative_to(REPO_ROOT).with_suffix("").as_posix().replace("/", ".")
        for path in (REPO_ROOT / "scripts").rglob("test_*.py")
        if "__pycache__" not in path.parts and ".venv" not in path.parts
    )


PYTHON_RUFF_TARGETS = python_source_targets()

PYTHON_UNITTEST_TARGETS = python_unittest_targets()

UV_RUN_SCRIPTS = ["uv", "run", "--frozen", "--project", "scripts"]

# Several script owners intentionally use aggregate test modules instead of a
# same-stem test file. Keep that routing explicit so changed PowerShell/shell
# helpers and shared Python utilities do not receive syntax-only validation.
SCRIPT_TEST_MODULES: dict[str, tuple[str, ...]] = {
    "scripts/app_server_schema_runtime_check.py": ("scripts.test_dev_environment",),
    "scripts/build_codex_package.py": ("scripts.test_stage_npm_packages",),
    "scripts/cargo-lane-trash-cleanup.ps1": ("scripts.test_cargo_lane",),
    "scripts/cargo-lane.ps1": ("scripts.test_cargo_lane",),
    "scripts/common-rust-env.ps1": ("scripts.test_build_tooling",),
    "scripts/config_schema_check.py": ("scripts.test_dev_environment",),
    "scripts/dev_env_doctor.py": ("scripts.test_dev_environment",),
    "scripts/format.py": ("scripts.test_build_tooling",),
    "scripts/git_doctor.py": ("scripts.test_dev_environment",),
    "scripts/invoke-rust-perf-env.ps1": ("scripts.test_build_tooling",),
    "scripts/just-shell.py": ("scripts.test_build_tooling",),
    "scripts/publish-local-codex-wsl.sh": (
        "scripts.test_dev_environment",
        "scripts.test_publish_local_codex",
    ),
    "scripts/publish-local-codex.hashing.ps1": ("scripts.test_publish_local_codex",),
    "scripts/publish-local-codex.ps1": ("scripts.test_publish_local_codex",),
    "scripts/root_maintenance.py": ("scripts.test_build_tooling",),
    "scripts/run-powershell-script.ps1": ("scripts.test_run_powershell_script",),
    "scripts/run_tui_with_exec_server.sh": ("scripts.test_run_tui_with_exec_server",),
    "scripts/rust_build_status.py": ("scripts.test_build_tooling",),
    "scripts/rust_packages.py": ("scripts.test_build_tooling",),
    "scripts/sccache-perf.ps1": ("scripts.test_build_tooling",),
    "scripts/stage_npm_packages.py": ("scripts.test_stage_npm_packages",),
    "scripts/start-codex-exec.sh": ("scripts.test_run_tui_with_exec_server",),
    "scripts/test-remote-env.sh": ("scripts.test_build_tooling",),
    "scripts/tool_versions.py": ("scripts.test_build_tooling",),
    "scripts/vscode_runtime_proof.py": ("scripts.test_dev_environment",),
}


def script_python_path(path_text: str) -> Path | None:
    path = Path(path_text)
    if path.is_absolute():
        try:
            path = path.relative_to(REPO_ROOT)
        except ValueError:
            return None
    first_part = path.parts[0] if path.parts else ""
    if os.name == "nt":
        # The filesystem is case-insensitive; a user-typed Scripts\foo.py must
        # not be silently dropped.
        first_part = first_part.lower()
    if first_part == "scripts" and path.suffix == ".py":
        return path
    return None


def python_lint_targets(changed: Sequence[str]) -> list[str]:
    selected = [
        path.as_posix()
        for path in (script_python_path(path_text) for path_text in changed)
        if path is not None and (REPO_ROOT / path).exists()
    ]
    if not selected:
        return PYTHON_RUFF_TARGETS
    return sorted(dict.fromkeys(selected))


def git_changed_paths() -> list[str]:
    result = subprocess.run(
        [
            "git",
            # Keep non-ASCII filenames as raw UTF-8 instead of C-quoted octal
            # escapes that script_python_path can never match.
            "-c",
            "core.quotepath=off",
            "diff",
            "--name-only",
            "-z",
            "--diff-filter=ACMRTUXB",
            "HEAD",
            "--",
        ],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    if result.returncode != 0:
        return []
    delimiter = "\0" if "\0" in result.stdout else "\n"
    return [path for path in result.stdout.split(delimiter) if path]


def expand_changed_paths(changed: Sequence[str | None]) -> list[str]:
    expanded: list[str] = []
    needs_git = False
    for path in changed:
        if path is None:
            needs_git = True
        else:
            expanded.append(path)
    if needs_git:
        expanded.extend(git_changed_paths())
    return expanded


def test_modules_for_changed_path(path_text: str) -> tuple[str, ...]:
    raw_path = Path(path_text)
    if raw_path.is_absolute():
        try:
            raw_path = raw_path.relative_to(REPO_ROOT)
        except ValueError:
            return ()
    path_key = raw_path.as_posix()
    if os.name == "nt":
        path_key = path_key.lower()

    selected = list(SCRIPT_TEST_MODULES.get(path_key, ()))
    path = script_python_path(path_text)
    if path is None:
        return tuple(selected)
    module = path.with_suffix("").as_posix().replace("/", ".")
    if path.name.startswith("test_"):
        selected.append(module)
    else:
        test_module = ".".join((*path.parts[:-1], f"test_{path.stem}"))
        if test_module in PYTHON_UNITTEST_TARGETS:
            selected.append(test_module)
    return tuple(dict.fromkeys(selected))


def test_module_for_changed_path(path_text: str) -> str | None:
    modules = test_modules_for_changed_path(path_text)
    return modules[0] if modules else None


def python_test_targets(modules: Sequence[str], changed: Sequence[str]) -> list[str]:
    selected = list(modules)
    selected.extend(
        module for path in changed for module in test_modules_for_changed_path(path)
    )
    if not selected:
        return PYTHON_UNITTEST_TARGETS
    return sorted(dict.fromkeys(selected))


def run(command: Sequence[str]) -> int:
    executable = which(command[0]) or command[0]
    return subprocess.run([executable, *command[1:]], cwd=REPO_ROOT).returncode


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Run root package maintenance commands.",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    prettier = subparsers.add_parser("format-prettier")
    prettier.add_argument("--write", action="store_true")

    python_format = subparsers.add_parser("format-python")
    python_format.add_argument("--write", action="store_true")
    python_format.add_argument(
        "--changed",
        action="append",
        nargs="?",
        const=None,
        default=[],
        help="Format changed scripts/*.py paths. With no path, detect changed paths from git.",
    )

    python_lint = subparsers.add_parser("lint-python")
    python_lint.add_argument("--fix", action="store_true")
    python_lint.add_argument(
        "--changed",
        action="append",
        nargs="?",
        const=None,
        default=[],
        help="Lint changed scripts/*.py paths. With no path, detect changed paths from git.",
    )

    python_test = subparsers.add_parser("test-python")
    python_test.add_argument(
        "--module",
        action="append",
        default=[],
        help="Run a specific unittest module, such as scripts.test_verify_local.",
    )
    python_test.add_argument(
        "--changed",
        action="append",
        nargs="?",
        const=None,
        default=[],
        help="Run nearest script unittests for changed scripts/*.py paths. With no path, detect changed paths from git.",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)

    if args.command == "format-prettier":
        mode = "--write" if args.write else "--check"
        return run(["pnpm", "exec", "prettier", mode, *PRETTIER_TARGETS])

    if args.command == "format-python":
        command = [*UV_RUN_SCRIPTS, "ruff", "format"]
        if not args.write:
            command.append("--check")
        return run([*command, *python_lint_targets(expand_changed_paths(args.changed))])

    if args.command == "lint-python":
        command = [*UV_RUN_SCRIPTS, "ruff", "check"]
        if args.fix:
            command.append("--fix")
        return run([*command, *python_lint_targets(expand_changed_paths(args.changed))])

    if args.command == "test-python":
        return run(
            [
                *UV_RUN_SCRIPTS,
                "python",
                "-m",
                "unittest",
                *python_test_targets(args.module, expand_changed_paths(args.changed)),
                "-v",
            ]
        )

    raise AssertionError(f"unhandled command: {args.command}")


if __name__ == "__main__":
    raise SystemExit(main())
