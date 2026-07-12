#!/usr/bin/env python3
"""Format repository sources or check that they are already formatted."""

import argparse
import shlex
import subprocess
import sys
from collections.abc import Callable
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.root_maintenance import PRETTIER_TARGETS  # noqa: E402
from scripts.tool_versions import RUSTFMT_TOOLCHAIN  # noqa: E402


@dataclass(frozen=True)
class Command:
    args: tuple[str, ...]
    cwd: Path = REPO_ROOT
    discard_stderr: bool = False


@dataclass(frozen=True)
class FormatterGroup:
    name: str
    commands: tuple[Command, ...]


@dataclass(frozen=True)
class FormatterResult:
    name: str
    output: str
    returncode: int


FormatterGroupFactory = tuple[str, Callable[[], FormatterGroup]]


def just_formatter_group(*, check: bool) -> FormatterGroup:
    args = ["just", "--unstable", "--fmt"]
    if check:
        args.append("--check")
    return FormatterGroup("Just", (Command(tuple(args)),))


def rust_formatter_group(*, check: bool) -> FormatterGroup:
    args = ["cargo", f"+{RUSTFMT_TOOLCHAIN}", "fmt"]
    if check:
        args.append("--check")
    command = Command(
        tuple(args),
        REPO_ROOT / "codex-rs",
    )
    return FormatterGroup("Rust", (command,))


def prettier_formatter_group(*, check: bool) -> FormatterGroup:
    mode = "--check" if check else "--write"
    return FormatterGroup(
        "Prettier",
        (Command(("pnpm", "exec", "prettier", mode, *PRETTIER_TARGETS)),),
    )


def python_sdk_formatter_group(*, check: bool) -> FormatterGroup:
    # Each `--project` retains its local dependency and Ruff configuration context.
    uv_run_args = [
        "uv",
        "run",
        "--frozen",
        "--project",
        "sdk/python",
        "--only-group",
        "format",
    ]
    format_args = [
        *uv_run_args,
        "ruff",
        "format",
    ]
    if check:
        format_args.append("--check")
        # `ruff check --diff` reports lint-driven rewrites without changing files.
        # It is the check-mode counterpart of `--fix --fix-only`, not a full lint gate.
        lint_args = ["ruff", "check", "--diff"]
    else:
        # Ruff's lint fixer and formatter are separate passes: the first applies
        # fixable lint rewrites, while the second formats source layout.
        lint_args = ["ruff", "check", "--fix", "--fix-only"]

    return FormatterGroup(
        "Python SDK",
        (
            Command((*uv_run_args, *lint_args, "sdk/python")),
            Command((*format_args, "sdk/python")),
        ),
    )


def python_scripts_formatter_group(*, check: bool) -> FormatterGroup:
    # The SDK and internal scripts intentionally use separate project roots so
    # uv and Ruff retain each project's configuration context.
    args = [
        "uv",
        "run",
        "--frozen",
        "--project",
        "scripts",
        "ruff",
        "format",
    ]
    if check:
        args.append("--check")
    args.append("scripts")
    return FormatterGroup("Python scripts", (Command(tuple(args)),))


def formatter_groups(
    *,
    check: bool,
    fast_local: bool = False,
    selected_groups: set[str] | None = None,
) -> tuple[FormatterGroup, ...]:
    factories: list[FormatterGroupFactory] = [
        ("just", lambda: just_formatter_group(check=check)),
        ("rust", lambda: rust_formatter_group(check=check)),
    ]
    if not fast_local:
        factories.extend(
            [
                ("prettier", lambda: prettier_formatter_group(check=check)),
                ("python sdk", lambda: python_sdk_formatter_group(check=check)),
                ("python scripts", lambda: python_scripts_formatter_group(check=check)),
            ]
        )
    if selected_groups is None:
        selected_factories = factories
    else:
        selected_factories = [
            factory for factory in factories if factory[0].lower() in selected_groups
        ]
    return tuple(create_group() for _name, create_group in selected_factories)


def run_formatter_group(group: FormatterGroup) -> FormatterResult:
    """Run one formatter group sequentially and return its buffered output."""
    output: list[str] = []
    for command in group.commands:
        output.append(f"$ {shlex.join(command.args)}\n")
        try:
            process = subprocess.run(
                command.args,
                cwd=command.cwd,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL
                if command.discard_stderr
                else subprocess.STDOUT,
                text=True,
                encoding="utf-8",
                errors="replace",
                check=False,
            )
        except OSError as error:
            output.append(f"{error}\n")
            return FormatterResult(group.name, "".join(output), 1)

        output.append(process.stdout)
        if process.stdout and not process.stdout.endswith("\n"):
            output.append("\n")
        if process.returncode != 0:
            return FormatterResult(group.name, "".join(output), process.returncode)

    return FormatterResult(group.name, "".join(output), 0)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--check",
        action="store_true",
        help="check formatting without modifying files",
    )
    parser.add_argument(
        "--fast-local",
        action="store_true",
        help="format only the high-frequency local edit surfaces: justfile and Rust",
    )
    parser.add_argument(
        "--only",
        action="append",
        choices=(
            "just",
            "rust",
            "prettier",
            "python sdk",
            "python scripts",
        ),
        help="run only the named formatter group; may be provided multiple times",
    )
    args = parser.parse_args()
    selected_groups = {name.lower() for name in args.only} if args.only else None
    groups = formatter_groups(
        check=args.check,
        fast_local=args.fast_local,
        selected_groups=selected_groups,
    )
    if not groups:
        print("No formatter groups selected.", file=sys.stderr)
        return 1

    failures: list[str] = []
    with ThreadPoolExecutor(max_workers=len(groups)) as executor:
        futures = {}
        for group in groups:
            print(f"Starting {group.name} formatter...", flush=True)
            futures[executor.submit(run_formatter_group, group)] = group.name
        for future in as_completed(futures):
            result = future.result()
            print(f"==> {result.name} formatter finished")
            print(result.output, end="")
            if result.returncode != 0:
                failures.append(result.name)

    if failures:
        print(f"Formatting failed: {', '.join(failures)}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
