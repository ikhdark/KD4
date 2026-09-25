#!/usr/bin/env python3

from __future__ import annotations

import shlex
import shutil
import subprocess
import sys
from collections.abc import MutableMapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Never

STRICT_LINTS = [
    "argument-comment-mismatch",
    "uncommented-anonymous-literal-argument",
]
NOISE_LINT = "unknown_lints"
TOOLCHAIN_CHANNEL = "nightly-2025-09-18"

_TARGET_SELECTION_ARGS = {
    "--all-targets",
    "--lib",
    "--bins",
    "--tests",
    "--examples",
    "--benches",
    "--doc",
}
_TARGET_SELECTION_PREFIXES = ("--bin=", "--test=", "--example=", "--bench=")
_TARGET_SELECTION_WITH_VALUE = {"--bin", "--test", "--example", "--bench"}
_PACKAGE_SELECTION_PREFIXES = ("--package=", "-p")


@dataclass
class ParsedWrapperArgs:
    lint_args: list[str]
    cargo_args: list[str]
    has_manifest_path: bool = False
    has_package_selection: bool = False
    has_no_deps: bool = False
    has_library_selection: bool = False
    has_cargo_target_selection: bool = False
    has_fix: bool = False


def repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def parse_wrapper_args(argv: Sequence[str]) -> ParsedWrapperArgs:
    parsed = ParsedWrapperArgs(lint_args=[], cargo_args=[])
    after_separator = False
    expect_value: str | None = None

    for arg in argv:
        if after_separator:
            parsed.cargo_args.append(arg)
            if (
                arg in _TARGET_SELECTION_ARGS
                or arg in _TARGET_SELECTION_WITH_VALUE
                or arg.startswith(_TARGET_SELECTION_PREFIXES)
            ):
                parsed.has_cargo_target_selection = True
            elif arg == "--package" or arg.startswith(_PACKAGE_SELECTION_PREFIXES):
                parsed.has_package_selection = True
            continue

        if arg == "--":
            after_separator = True
            continue

        parsed.lint_args.append(arg)

        if expect_value is not None:
            if expect_value == "manifest_path":
                parsed.has_manifest_path = True
            elif expect_value == "package_selection":
                parsed.has_package_selection = True
            elif expect_value == "library_selection":
                parsed.has_library_selection = True
            expect_value = None
            continue

        if arg == "--manifest-path":
            expect_value = "manifest_path"
        elif arg.startswith("--manifest-path="):
            parsed.has_manifest_path = True
        elif arg in {"-p", "--package"}:
            expect_value = "package_selection"
        elif arg.startswith(_PACKAGE_SELECTION_PREFIXES):
            parsed.has_package_selection = True
        elif arg == "--fix":
            parsed.has_fix = True
        elif arg == "--workspace":
            parsed.has_package_selection = True
        elif arg == "--no-deps":
            parsed.has_no_deps = True
        elif arg in {"--lib", "--lib-path"}:
            expect_value = "library_selection"
        elif arg.startswith("--lib=") or arg.startswith("--lib-path="):
            parsed.has_library_selection = True

    return parsed


def build_final_args(parsed: ParsedWrapperArgs, manifest_path: Path) -> list[str]:
    final_args: list[str] = []
    cargo_args = list(parsed.cargo_args)

    if not parsed.has_manifest_path:
        final_args.extend(["--manifest-path", str(manifest_path)])
    if not parsed.has_package_selection and not parsed.has_manifest_path:
        final_args.append("--workspace")
    if not parsed.has_no_deps:
        final_args.append("--no-deps")
    if not parsed.has_fix and not parsed.has_cargo_target_selection:
        cargo_args.append("--all-targets")
    # The pinned lint nightly is older than the rust-version some dependencies
    # declare; the code still checks, so do not let Cargo refuse the run.
    if "--ignore-rust-version" not in cargo_args:
        cargo_args.append("--ignore-rust-version")
    final_args.extend(parsed.lint_args)
    if cargo_args:
        final_args.extend(["--", *cargo_args])
    return final_args


def append_env_flag(env: MutableMapping[str, str], key: str, flag: str) -> None:
    value = env.get(key)
    if value is None or value == "":
        env[key] = flag
        return
    if flag not in value:
        env[key] = f"{value} {flag}"


def set_default_lint_env(env: MutableMapping[str, str]) -> None:
    for strict_lint in STRICT_LINTS:
        # rustc applies the last level given for a lint, so appending `-D` would
        # override an explicit ad hoc level such as `-A <lint>`.
        if strict_lint not in env.get("DYLINT_RUSTFLAGS", "").replace("_", "-"):
            append_env_flag(env, "DYLINT_RUSTFLAGS", f"-D {strict_lint}")
    append_env_flag(env, "DYLINT_RUSTFLAGS", f"-A {NOISE_LINT}")
    if not env.get("CARGO_INCREMENTAL"):
        env["CARGO_INCREMENTAL"] = "0"


def die(message: str) -> Never:
    print(message, file=sys.stderr)
    raise SystemExit(1)


def require_command(name: str, install_message: str | None = None) -> str:
    executable = shutil.which(name)
    if executable is None:
        if install_message is None:
            die(f"{name} is required but was not found on PATH.")
        die(install_message)
    return executable


def run_capture(
    args: Sequence[str], env: MutableMapping[str, str] | None = None
) -> str:
    try:
        completed = subprocess.run(
            list(args),
            capture_output=True,
            check=True,
            env=None if env is None else dict(env),
            text=True,
        )
    except subprocess.CalledProcessError as error:
        command = shlex.join(str(part) for part in error.cmd)
        stderr = error.stderr.strip()
        stdout = error.stdout.strip()
        output = stderr or stdout
        if output:
            die(f"{command} failed:\n{output}")
        die(f"{command} failed with exit code {error.returncode}")
    return completed.stdout.strip()


def ensure_source_prerequisites(env: MutableMapping[str, str]) -> None:
    require_command(
        "cargo-dylint",
        "argument-comment-lint source wrapper requires cargo-dylint and dylint-link.\n"
        "Install them with:\n"
        "  cargo install cargo-dylint dylint-link",
    )
    require_command(
        "dylint-link",
        "argument-comment-lint source wrapper requires cargo-dylint and dylint-link.\n"
        "Install them with:\n"
        "  cargo install cargo-dylint dylint-link",
    )
    require_command(
        "rustup",
        "argument-comment-lint source wrapper requires rustup.\n"
        f"Install the {TOOLCHAIN_CHANNEL} toolchain with:\n"
        f"  rustup toolchain install {TOOLCHAIN_CHANNEL} \\\n"
        "    --component llvm-tools-preview \\\n"
        "    --component rustc-dev \\\n"
        "    --component rust-src",
    )
    toolchains = run_capture(["rustup", "toolchain", "list"], env=env)
    if not any(line.startswith(TOOLCHAIN_CHANNEL) for line in toolchains.splitlines()):
        die(
            "argument-comment-lint source wrapper requires the "
            f"{TOOLCHAIN_CHANNEL} toolchain with rustc-dev support.\n"
            "Install it with:\n"
            f"  rustup toolchain install {TOOLCHAIN_CHANNEL} \\\n"
            "    --component llvm-tools-preview \\\n"
            "    --component rustc-dev \\\n"
            "    --component rust-src"
        )


def exec_command(command: Sequence[str], env: MutableMapping[str, str]) -> Never:
    try:
        completed = subprocess.run(list(command), env=dict(env), check=False)
    except FileNotFoundError:
        die(f"{command[0]} is required but was not found on PATH.")
    raise SystemExit(completed.returncode)
