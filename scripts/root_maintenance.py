#!/usr/bin/env python3
"""Run root package maintenance commands from one maintained target list."""

from __future__ import annotations

import argparse
import ast
import hashlib
import json
import os
import shlex
import subprocess
from pathlib import Path
from shutil import which
from typing import Callable, Sequence


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPTS_ROOT = REPO_ROOT / "scripts"

SCRIPT_KIND_BY_SUFFIX = {
    ".py": "python",
    ".ps1": "powershell",
    ".sh": "shell",
    ".js": "javascript",
    ".mjs": "javascript",
}
SCRIPT_CANDIDATE_SUFFIXES = frozenset((*SCRIPT_KIND_BY_SUFFIX, ".bat", ".cmd", ".ts"))
SCRIPT_LINE_ADVISORY_THRESHOLD = 1_000
SCRIPT_AUDIT_PACKAGE_COMMAND = "python scripts/root_maintenance.py audit-scripts"
SCRIPT_AUDIT_JUST_RECIPE = "audit-scripts *args:"
SCRIPT_AUDIT_JUST_COMMAND = "scripts/root_maintenance.py audit-scripts {args}"
POWERSHELL_PARSE_ALL_SCRIPT = (
    "$failed = $false; "
    "foreach ($path in $paths) { "
    "$tokens = $null; $errors = $null; "
    "[System.Management.Automation.Language.Parser]::ParseFile("
    "(Resolve-Path -LiteralPath $path).Path, [ref]$tokens, [ref]$errors) | "
    "Out-Null; "
    "foreach ($error in $errors) { "
    "Write-Error ('{0}: {1}' -f $path, $error.Message); $failed = $true "
    "} "
    "}; "
    "if ($failed) { exit 1 }"
)

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


def script_kind_for_path(path: Path) -> str | None:
    suffix = path.suffix.lower()
    if suffix in SCRIPT_KIND_BY_SUFFIX:
        return SCRIPT_KIND_BY_SUFFIX[suffix]
    if suffix in SCRIPT_CANDIDATE_SUFFIXES:
        return f"unsupported:{suffix}"
    if suffix:
        return None
    try:
        with path.open("rb") as script_file:
            first_line = script_file.readline(256).decode("utf-8")
    except (OSError, UnicodeDecodeError):
        return None
    if first_line.rstrip("\r\n") == "#!/usr/bin/env dotslash":
        return "dotslash"
    if first_line.startswith("#!"):
        return "unsupported:shebang"
    return None


def script_source_targets() -> list[str]:
    return sorted(
        path.relative_to(REPO_ROOT).as_posix()
        for path in SCRIPTS_ROOT.rglob("*")
        if path.is_file()
        and "__pycache__" not in path.parts
        and ".venv" not in path.parts
        and script_kind_for_path(path) is not None
    )


def python_unittest_targets() -> list[str]:
    return sorted(
        path.relative_to(REPO_ROOT).with_suffix("").as_posix().replace("/", ".")
        for path in (REPO_ROOT / "scripts").rglob("test_*.py")
        if "__pycache__" not in path.parts and ".venv" not in path.parts
    )


PYTHON_RUFF_TARGETS = python_source_targets()

PYTHON_UNITTEST_TARGETS = python_unittest_targets()

SCRIPT_AUDIT_TARGETS = script_source_targets()

UV_RUN_SCRIPTS = ["uv", "run", "--frozen", "--project", "scripts"]

# Several script owners intentionally use aggregate test modules instead of a
# same-stem test file. Keep that routing explicit so changed PowerShell/shell
# helpers and shared Python utilities do not receive syntax-only validation.
SCRIPT_TEST_MODULES: dict[str, tuple[str, ...]] = {
    "scripts/app_server_schema_runtime_check.py": ("scripts.test_dev_environment",),
    "scripts/build_codex_package.py": ("scripts.test_stage_npm_packages",),
    "scripts/cargo-lane-trash-cleanup.ps1": ("scripts.test_cargo_lane",),
    "scripts/cargo-lane.ps1": ("scripts.test_cargo_lane",),
    "scripts/check-module-bazel-lock.sh": ("scripts.test_shell_helpers",),
    "scripts/common-rust-env.ps1": ("scripts.test_build_tooling_performance",),
    "scripts/codex_package/codex-zsh": (
        "scripts.codex_package.test_dotslash",
        "scripts.codex_package.test_zsh",
    ),
    "scripts/codex_package/rg": (
        "scripts.codex_package.test_dotslash",
        "scripts.codex_package.test_ripgrep",
    ),
    "scripts/config_schema_check.py": ("scripts.test_dev_environment",),
    "scripts/debug-codex.sh": ("scripts.test_shell_helpers",),
    "scripts/dev_env_doctor.py": ("scripts.test_dev_environment",),
    "scripts/format.py": ("scripts.test_build_tooling",),
    "scripts/git_doctor.py": ("scripts.test_dev_environment",),
    "scripts/invoke-rust-perf-env.ps1": ("scripts.test_build_tooling_performance",),
    "scripts/install/install.ps1": ("scripts.test_build_tooling_policy",),
    "scripts/install/build_install_sh.py": ("scripts.install.test_install_sh",),
    "scripts/install/install.sh": (
        "scripts.install.test_install_sh",
        "scripts.test_build_tooling_policy",
    ),
    "scripts/install/install_release.sh": ("scripts.install.test_install_sh",),
    "scripts/just-shell.py": ("scripts.test_build_tooling",),
    "scripts/list-bazel-clippy-targets.sh": ("scripts.test_shell_helpers",),
    "scripts/list-bazel-release-targets.sh": ("scripts.test_shell_helpers",),
    "scripts/publish-local-codex-wsl.sh": (
        "scripts.test_dev_environment",
        "scripts.test_publish_local_codex",
        "scripts.test_publish_local_codex_apply",
        "scripts.test_publish_local_codex_build",
        "scripts.test_publish_local_codex_dry_run",
        "scripts.test_publish_local_codex_freshness",
    ),
    "scripts/publish-local-codex.hashing.ps1": (
        "scripts.test_publish_local_codex",
        "scripts.test_publish_local_codex_apply",
        "scripts.test_publish_local_codex_build",
        "scripts.test_publish_local_codex_dry_run",
        "scripts.test_publish_local_codex_freshness",
    ),
    "scripts/publish-local-codex.apply.ps1": (
        "scripts.test_publish_local_codex",
        "scripts.test_publish_local_codex_apply",
        "scripts.test_publish_local_codex_build",
        "scripts.test_publish_local_codex_dry_run",
        "scripts.test_publish_local_codex_freshness",
    ),
    "scripts/publish-local-codex.build.ps1": (
        "scripts.test_publish_local_codex",
        "scripts.test_publish_local_codex_apply",
        "scripts.test_publish_local_codex_build",
        "scripts.test_publish_local_codex_dry_run",
        "scripts.test_publish_local_codex_freshness",
    ),
    "scripts/publish-local-codex.desktop.ps1": (
        "scripts.test_publish_local_codex",
        "scripts.test_publish_local_codex_apply",
        "scripts.test_publish_local_codex_build",
        "scripts.test_publish_local_codex_dry_run",
        "scripts.test_publish_local_codex_freshness",
    ),
    "scripts/publish-local-codex.proof.ps1": (
        "scripts.test_publish_local_codex",
        "scripts.test_publish_local_codex_apply",
        "scripts.test_publish_local_codex_build",
        "scripts.test_publish_local_codex_dry_run",
        "scripts.test_publish_local_codex_freshness",
    ),
    "scripts/publish-local-codex.ps1": (
        "scripts.test_publish_local_codex",
        "scripts.test_publish_local_codex_apply",
        "scripts.test_publish_local_codex_build",
        "scripts.test_publish_local_codex_dry_run",
        "scripts.test_publish_local_codex_freshness",
    ),
    "scripts/publish_local_codex_test_support.py": (
        "scripts.test_publish_local_codex_apply",
        "scripts.test_publish_local_codex_build",
        "scripts.test_publish_local_codex_dry_run",
        "scripts.test_publish_local_codex_freshness",
    ),
    "scripts/root_maintenance.py": ("scripts.test_build_tooling_policy",),
    "scripts/run-powershell-script.ps1": ("scripts.test_run_powershell_script",),
    "scripts/run_tui_with_exec_server.sh": ("scripts.test_run_tui_with_exec_server",),
    "scripts/rust_build_status.py": ("scripts.test_build_tooling_storage",),
    "scripts/rust_build_status_support.py": ("scripts.test_build_tooling_storage",),
    "scripts/rust_packages.py": ("scripts.test_build_tooling_policy",),
    "scripts/sccache-perf.ps1": ("scripts.test_build_tooling_performance",),
    "scripts/stage_npm_packages.py": ("scripts.test_stage_npm_packages",),
    "scripts/stage_npm_archives.py": ("scripts.test_stage_npm_packages",),
    "scripts/start-codex-exec.sh": ("scripts.test_run_tui_with_exec_server",),
    "scripts/test-remote-env.sh": ("scripts.test_build_tooling_policy",),
    "scripts/tool_versions.py": ("scripts.test_build_tooling_storage",),
    "scripts/vscode_runtime_proof.py": ("scripts.test_dev_environment",),
    "scripts/verify_local_context.py": ("scripts.test_verify_local",),
    "scripts/verify_local_execution.py": ("scripts.test_verify_local",),
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


def script_audit_test_targets(
    *,
    platform: str | None = None,
    native_sh_available: bool | None = None,
) -> tuple[list[str], list[str]]:
    platform = os.name if platform is None else platform
    native_sh_available = (
        Path("/bin/sh").is_file()
        if native_sh_available is None
        else native_sh_available
    )
    targets = list(PYTHON_UNITTEST_TARGETS)
    skipped: list[str] = []
    install_test = "scripts.install.test_install_sh"
    if platform == "nt" and not native_sh_available and install_test in targets:
        targets.remove(install_test)
        skipped.append(f"{install_test}: native /bin/sh is unavailable on Windows")
    return targets, skipped


def script_audit_context_issues() -> list[str]:
    issues: list[str] = []
    required_paths = (
        REPO_ROOT / "AGENTS.md",
        SCRIPTS_ROOT / "AGENTS.md",
        SCRIPTS_ROOT / "README.md",
        SCRIPTS_ROOT / "pyproject.toml",
        SCRIPTS_ROOT / "uv.lock",
        REPO_ROOT / "package.json",
        REPO_ROOT / "justfile",
    )
    for path in required_paths:
        if not path.is_file():
            issues.append(
                f"missing project-context file: {path.relative_to(REPO_ROOT)}"
            )

    package_path = REPO_ROOT / "package.json"
    if package_path.is_file():
        try:
            package = json.loads(package_path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
            issues.append(f"package.json is unreadable: {exc}")
        else:
            actual = package.get("scripts", {}).get("audit:scripts")
            if actual != SCRIPT_AUDIT_PACKAGE_COMMAND:
                issues.append(
                    "package.json audit:scripts must route to "
                    f"`{SCRIPT_AUDIT_PACKAGE_COMMAND}`"
                )

    justfile_path = REPO_ROOT / "justfile"
    if justfile_path.is_file():
        justfile_text = justfile_path.read_text(encoding="utf-8")
        if SCRIPT_AUDIT_JUST_RECIPE not in justfile_text:
            issues.append("justfile is missing the audit-scripts recipe")
        if SCRIPT_AUDIT_JUST_COMMAND not in justfile_text:
            issues.append(
                "justfile audit-scripts recipe does not route to root_maintenance"
            )

    just = which("just")
    if just is None:
        issues.append("required audit tool is missing: just")
    else:
        just_summary = subprocess.run(
            [just, "--summary"],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
        if just_summary.returncode != 0:
            issues.append(f"justfile parse failed: {just_summary.stderr.strip()}")
        elif "audit-scripts" not in just_summary.stdout.split():
            issues.append("justfile summary does not expose audit-scripts")

    readme_path = SCRIPTS_ROOT / "README.md"
    if readme_path.is_file() and "audit-scripts" not in readme_path.read_text(
        encoding="utf-8"
    ):
        issues.append("scripts/README.md does not document audit-scripts")

    for source, modules in SCRIPT_TEST_MODULES.items():
        if not (REPO_ROOT / source).is_file():
            issues.append(f"stale script test route: {source}")
        for module in modules:
            if module not in PYTHON_UNITTEST_TARGETS:
                issues.append(f"missing script test module route: {source} -> {module}")

    try:
        result = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
    except OSError as exc:
        issues.append(f"git project-context check failed: {exc}")
    else:
        if result.returncode != 0:
            issues.append(f"git project-context check failed: {result.stderr.strip()}")
        elif Path(result.stdout.strip()).resolve() != REPO_ROOT.resolve():
            issues.append("root_maintenance.py is not running in its owning repository")
    return issues


def script_audit_findings() -> tuple[list[str], list[str]]:
    errors: list[str] = []
    advisories: list[str] = []
    content_hashes: dict[str, list[str]] = {}

    for target in SCRIPT_AUDIT_TARGETS:
        path = REPO_ROOT / target
        kind = script_kind_for_path(path)
        if kind is None:
            errors.append(f"script disappeared from inventory: {target}")
            continue
        if kind.startswith("unsupported:"):
            errors.append(
                f"unsupported script type `{kind.removeprefix('unsupported:')}`: {target}"
            )

        try:
            data = path.read_bytes()
        except OSError as exc:
            errors.append(f"cannot read {target}: {exc}")
            continue
        content_hashes.setdefault(hashlib.sha256(data).hexdigest(), []).append(target)
        if b"\0" in data:
            errors.append(f"NUL byte found in {target}")
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError as exc:
            errors.append(f"{target} is not UTF-8 at byte {exc.start}")
            continue

        lines = text.splitlines()
        trailing_lines = [
            line_number
            for line_number, line in enumerate(lines, start=1)
            if line.rstrip(" \t") != line
        ]
        if trailing_lines:
            sample = ", ".join(str(line) for line in trailing_lines[:5])
            errors.append(f"trailing whitespace in {target} at line(s) {sample}")

        if kind == "python":
            try:
                ast.parse(text, filename=target)
            except SyntaxError as exc:
                errors.append(
                    f"Python syntax error in {target}:{exc.lineno}: {exc.msg}"
                )
        elif kind == "shell" and not text.startswith("#!"):
            errors.append(f"shell script is missing a shebang: {target}")
        elif kind == "dotslash":
            _, separator, manifest_text = text.partition("\n")
            if not separator:
                errors.append(f"DotSlash manifest has no JSON body: {target}")
            else:
                try:
                    manifest = json.loads(manifest_text)
                except json.JSONDecodeError as exc:
                    errors.append(f"invalid DotSlash JSON in {target}: {exc}")
                else:
                    if not isinstance(manifest, dict):
                        errors.append(f"DotSlash manifest must be an object: {target}")
                    elif not isinstance(manifest.get("name"), str) or not isinstance(
                        manifest.get("platforms"), dict
                    ):
                        errors.append(
                            f"DotSlash manifest needs string name and object platforms: {target}"
                        )

        if len(lines) >= SCRIPT_LINE_ADVISORY_THRESHOLD:
            advisories.append(
                f"large script candidate: {target} ({len(lines)} lines, {len(data)} bytes)"
            )

        if (
            kind not in {"unsupported:shebang"}
            and not path.name.startswith("test_")
            and path.name != "__init__.py"
            and not test_modules_for_changed_path(target)
        ):
            advisories.append(
                f"syntax/lint-only script has no focused test route: {target}"
            )

    for duplicate_targets in content_hashes.values():
        if len(duplicate_targets) > 1:
            advisories.append(
                "duplicate script content: " + ", ".join(duplicate_targets)
            )
    return errors, advisories


def script_audit_commands(
    *,
    include_tests: bool,
    test_targets: Sequence[str] | None = None,
    resolve_tool: Callable[[str], str | None] = which,
) -> tuple[list[tuple[str, tuple[str, ...]]], list[str]]:
    commands: list[tuple[str, tuple[str, ...]]] = []
    missing_tools: list[str] = []
    unit_test_command: tuple[str, tuple[str, ...]] | None = None

    uv = resolve_tool("uv")
    if uv is None:
        missing_tools.append("uv")
    else:
        uv_prefix = (uv, "run", "--frozen", "--project", "scripts")
        commands.append(
            (
                "Python format",
                (*uv_prefix, "ruff", "format", "--check", *PYTHON_RUFF_TARGETS),
            )
        )
        commands.append(
            ("Python lint", (*uv_prefix, "ruff", "check", *PYTHON_RUFF_TARGETS))
        )
        if include_tests:
            selected_tests = (
                PYTHON_UNITTEST_TARGETS if test_targets is None else list(test_targets)
            )
            unit_test_command = (
                "script unit tests",
                (
                    *uv_prefix,
                    "python",
                    "-m",
                    "unittest",
                    *selected_tests,
                    "-v",
                ),
            )

    powershell_targets = [
        target
        for target in SCRIPT_AUDIT_TARGETS
        if script_kind_for_path(REPO_ROOT / target) == "powershell"
    ]
    if powershell_targets:
        powershell = resolve_tool("pwsh") or resolve_tool("powershell")
        if powershell is None:
            missing_tools.append("pwsh or powershell")
        else:
            paths_json = json.dumps(powershell_targets).replace("'", "''")
            parse_script = (
                f"$paths = ConvertFrom-Json '{paths_json}'; "
                f"{POWERSHELL_PARSE_ALL_SCRIPT}"
            )
            commands.append(
                (
                    "PowerShell syntax",
                    (
                        powershell,
                        "-NoProfile",
                        "-Command",
                        parse_script,
                    ),
                )
            )

    shell_targets = [
        target
        for target in SCRIPT_AUDIT_TARGETS
        if script_kind_for_path(REPO_ROOT / target) == "shell"
    ]
    if shell_targets:
        bash = resolve_tool("bash")
        if bash is None:
            missing_tools.append("bash")
        else:
            parse_script = "set -o pipefail; " + " && ".join(
                f"sed 's/\\r$//' {shlex.quote(target)} | bash -n"
                for target in shell_targets
            )
            commands.append(
                (
                    "shell syntax",
                    (
                        bash,
                        "-lc",
                        parse_script,
                    ),
                )
            )

    javascript_targets = [
        target
        for target in SCRIPT_AUDIT_TARGETS
        if script_kind_for_path(REPO_ROOT / target) == "javascript"
    ]
    if javascript_targets:
        node = resolve_tool("node")
        if node is None:
            missing_tools.append("node")
        else:
            commands.extend(
                (f"JavaScript syntax: {target}", (node, "--check", target))
                for target in javascript_targets
            )

    if unit_test_command is not None:
        commands.append(unit_test_command)

    return commands, missing_tools


def git_context_label() -> str:
    result = subprocess.run(
        ["git", "status", "--short", "--branch"],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    if result.returncode != 0:
        return "git context unavailable"
    lines = result.stdout.splitlines()
    branch = lines[0].removeprefix("## ") if lines else "unknown branch"
    return f"{branch}; {max(0, len(lines) - 1)} changed path(s)"


def run_script_audit(*, include_tests: bool, strict: bool) -> int:
    inventory: dict[str, int] = {}
    for target in SCRIPT_AUDIT_TARGETS:
        kind = script_kind_for_path(REPO_ROOT / target) or "unknown"
        inventory[kind] = inventory.get(kind, 0) + 1

    print(f"Script audit context: {git_context_label()}")
    print(
        f"Inventory: {len(SCRIPT_AUDIT_TARGETS)} script artifact(s) "
        + ", ".join(f"{kind}={count}" for kind, count in sorted(inventory.items()))
    )
    if not include_tests:
        print("Mode: quick (full script unit tests skipped)")

    errors = script_audit_context_issues()
    hygiene_errors, advisories = script_audit_findings()
    errors.extend(hygiene_errors)
    test_targets, skipped_tests = (
        script_audit_test_targets() if include_tests else ([], [])
    )
    commands, missing_tools = script_audit_commands(
        include_tests=include_tests,
        test_targets=test_targets,
    )
    errors.extend(f"required audit tool is missing: {tool}" for tool in missing_tools)

    for issue in errors:
        print(f"[FAIL] {issue}")
    for advisory in advisories:
        print(f"[ADVISORY] {advisory}")
    for skipped_test in skipped_tests:
        print(f"[SKIP] {skipped_test}")

    failed_commands: list[str] = []
    passed_commands = 0
    for label, command in commands:
        print(f"[RUN] {label}", flush=True)
        try:
            returncode = run(command)
        except OSError as exc:
            print(f"[FAIL] {label}: {exc}")
            failed_commands.append(label)
            continue
        if returncode == 0:
            print(f"[PASS] {label}")
            passed_commands += 1
        else:
            print(f"[FAIL] {label}: exit {returncode}")
            failed_commands.append(label)

    strict_failure = strict and bool(advisories)
    if strict_failure:
        print("[FAIL] --strict promoted optimization advisories to failures")
    if errors or failed_commands or strict_failure:
        print(
            "SCRIPT AUDIT FAILED: "
            f"{len(errors)} internal/context failure(s), "
            f"{len(failed_commands)} command failure(s), "
            f"{len(advisories)} advisory item(s)."
        )
        return 1

    print(
        "SCRIPT AUDIT PASSED: "
        f"{len(SCRIPT_AUDIT_TARGETS)} script artifact(s), "
        f"{passed_commands} command group(s), "
        f"{len(advisories)} advisory item(s), "
        f"{len(skipped_tests)} platform test skip(s)."
    )
    return 0


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

    script_audit = subparsers.add_parser(
        "audit-scripts",
        help="Check every script artifact against current repository context.",
    )
    script_audit.add_argument(
        "--quick",
        action="store_true",
        help="Run inventory, context, syntax, format, lint, and hygiene checks without the full test suite.",
    )
    script_audit.add_argument(
        "--strict",
        action="store_true",
        help="Treat optimization advisories such as large or syntax-only scripts as failures.",
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

    if args.command == "audit-scripts":
        return run_script_audit(include_tests=not args.quick, strict=args.strict)

    raise AssertionError(f"unhandled command: {args.command}")


if __name__ == "__main__":
    raise SystemExit(main())
