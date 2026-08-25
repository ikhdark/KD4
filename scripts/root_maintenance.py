#!/usr/bin/env python3
"""Run root package maintenance commands from one maintained target list."""

from __future__ import annotations

import argparse
import ast
from functools import cache
import hashlib
import json
import os
import re
import subprocess
import sys
from pathlib import Path
from shutil import which
from typing import Callable, Sequence


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPTS_ROOT = REPO_ROOT / "scripts"
SCRIPT_AUDIT_ROOTS = (
    REPO_ROOT / ".codex" / "environments",
    REPO_ROOT / ".codex" / "hooks",
    SCRIPTS_ROOT,
    REPO_ROOT / "codex-cli" / "scripts",
    REPO_ROOT / "codex-rs" / "app-server-test-client" / "scripts",
    REPO_ROOT / "codex-rs" / "config" / "scripts",
    REPO_ROOT / "codex-rs" / "scripts",
    REPO_ROOT / "codex-rs" / "skills" / "src" / "assets" / "samples",
    REPO_ROOT / "sdk" / "python" / "scripts",
    REPO_ROOT / "tools" / "argument-comment-lint",
)

# Tracked executable examples whose syntax belongs to an SDK/toolchain owner,
# not the repository maintenance-script audit. Keep exclusions path-specific
# and explain why they are not silently omitted from discovery.
SCRIPT_AUDIT_EXCLUSIONS: dict[str, str] = {
    "sdk/typescript/samples/basic_streaming.ts": (
        "TypeScript SDK sample; validated by the SDK toolchain"
    ),
    "sdk/typescript/samples/structured_output.ts": (
        "TypeScript SDK sample; validated by the SDK toolchain"
    ),
    "sdk/typescript/samples/structured_output_zod.ts": (
        "TypeScript SDK sample; validated by the SDK toolchain"
    ),
}

SCRIPT_KIND_BY_SUFFIX = {
    ".py": "python",
    ".ps1": "powershell",
    ".js": "javascript",
    ".mjs": "javascript",
}
SCRIPT_CANDIDATE_SUFFIXES = frozenset((*SCRIPT_KIND_BY_SUFFIX, ".bat", ".cmd", ".ts"))
SCRIPT_LINE_ADVISORY_THRESHOLD = 1_000
SCRIPT_AUDIT_PACKAGE_COMMAND = (
    "node scripts/run-python.js scripts/root_maintenance.py audit-scripts"
)
SCRIPT_AUDIT_JUST_RECIPE = "audit-scripts *args:"
SCRIPT_AUDIT_JUST_COMMAND = (
    '"{{ justfile_directory() }}/scripts/root_maintenance.py" audit-scripts {args}'
)
POWERSHELL_PARSE_ALL_SCRIPT = (
    "$failed = $false; "
    "foreach ($path in $paths) { "
    "$tokens = $null; $errors = $null; "
    "[System.Management.Automation.Language.Parser]::ParseFile("
    "(Resolve-Path -LiteralPath $path).Path, [ref]$tokens, [ref]$errors) | "
    "Out-Null; "
    "foreach ($parseError in $errors) { "
    "Write-Error ('{0}: {1}' -f $path, $parseError.Message); $failed = $true "
    "} "
    "}; "
    "if ($failed) { exit 1 }"
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


def tracked_script_entrypoints() -> tuple[Path, ...]:
    """Return tracked executable scripts outside the owned script roots."""
    result = subprocess.run(
        ["git", "ls-files", "--stage", "-z"],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    if result.returncode != 0:
        return ()
    entrypoints: list[Path] = []
    for record in result.stdout.split("\0"):
        if not record:
            continue
        metadata, separator, target = record.partition("\t")
        if not separator:
            continue
        path = REPO_ROOT / target
        if any(
            root == path.parent or root in path.parents for root in SCRIPT_AUDIT_ROOTS
        ):
            continue
        if target in SCRIPT_AUDIT_EXCLUSIONS:
            continue
        mode = metadata.split(" ", 1)[0]
        try:
            with path.open("rb") as script_file:
                first_line = script_file.readline(256)
        except OSError:
            continue
        if mode == "100755" or first_line.startswith(b"#!"):
            entrypoints.append(path)
    return tuple(entrypoints)


@cache
def script_inventory() -> tuple[
    tuple[str, ...], tuple[str, ...], tuple[tuple[str, str], ...]
]:
    python_sources: list[str] = []
    unittest_targets: list[str] = []
    script_kinds: list[tuple[str, str]] = []
    owned_paths = (path for root in SCRIPT_AUDIT_ROOTS for path in root.rglob("*"))
    for path in dict.fromkeys((*owned_paths, *tracked_script_entrypoints())):
        if not path.is_file() or "__pycache__" in path.parts or ".venv" in path.parts:
            continue
        target = path.relative_to(REPO_ROOT).as_posix()
        if path.suffix.lower() == ".py":
            python_sources.append(target)
            if path.name.lower().startswith("test_"):
                if SCRIPTS_ROOT in path.parents:
                    unittest_targets.append(
                        path.relative_to(REPO_ROOT)
                        .with_suffix("")
                        .as_posix()
                        .replace("/", ".")
                    )
                else:
                    # unittest accepts repository-relative file paths. Keep
                    # tests below non-package script roots discoverable even
                    # when a path segment cannot be a Python identifier.
                    unittest_targets.append(target)
        kind = script_kind_for_path(path)
        if kind is not None:
            script_kinds.append((target, kind))
    return (
        tuple(sorted(python_sources)),
        tuple(sorted(unittest_targets)),
        tuple(sorted(script_kinds)),
    )


def replace_just_interpolations(source: str) -> str:
    """Replace just expressions while preserving the surrounding PowerShell."""
    rendered: list[str] = []
    cursor = 0
    while True:
        start = source.find("{{", cursor)
        if start < 0:
            rendered.append(source[cursor:])
            return "".join(rendered)
        rendered.append(source[cursor:start])
        depth = 0
        index = start + 2
        while index < len(source):
            if source[index] == "{":
                depth += 1
            elif source[index] == "}":
                if depth:
                    depth -= 1
                elif index + 1 < len(source) and source[index + 1] == "}":
                    rendered.append("KD4_JUST_VALUE")
                    cursor = index + 2
                    break
            index += 1
        else:
            rendered.append(source[start:])
            return "".join(rendered)


def just_recipe_sources(
    text: str | None = None, *, script_interpreter: str | None
) -> tuple[tuple[str, str], ...]:
    """Extract recipe bodies handled by one interpreter."""
    if text is None:
        text = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
    sources: list[tuple[str, str]] = []
    recipe_start: int | None = None
    recipe_lines: list[str] = []
    pending_script_interpreter: str | None = None
    active_script_interpreter: str | None = None

    def flush_recipe() -> None:
        nonlocal recipe_start, recipe_lines
        if recipe_start is not None and recipe_lines:
            if script_interpreter is None and not recipe_lines[0].startswith("#!"):
                # Ordinary just recipes execute each body line in a fresh
                # shell. Parse those lines independently; only script/shebang
                # recipes are one multi-line source unit.
                sources.extend(
                    (f"justfile:{recipe_start + offset}", source)
                    for offset, source in enumerate(recipe_lines)
                )
            else:
                sources.append((f"justfile:{recipe_start}", "\n".join(recipe_lines)))
        recipe_start = None
        recipe_lines = []

    for line_number, line in enumerate(text.splitlines(), start=1):
        if not line.startswith("    "):
            flush_recipe()
            stripped = line.strip()
            script_match = re.fullmatch(r'\[script\("([^"\r\n]+)"\)\]', stripped)
            if script_match is not None:
                pending_script_interpreter = script_match.group(1)
            elif stripped.startswith("[") or not stripped or stripped.startswith("#"):
                pass
            elif re.match(r"^[A-Za-z_][A-Za-z0-9_-]*(?:\s[^:]*)?:", stripped):
                active_script_interpreter = pending_script_interpreter
                pending_script_interpreter = None
            else:
                pending_script_interpreter = None
                active_script_interpreter = None
            continue
        if active_script_interpreter != script_interpreter:
            continue
        source = line[4:]
        if recipe_start is None:
            recipe_start = line_number
        if (script_interpreter is None or not recipe_lines) and source.startswith("@"):
            source = source[1:]
        source = replace_just_interpolations(source)
        source = re.sub(r"\{[A-Za-z_][A-Za-z0-9_]*\}", "$null", source)
        recipe_lines.append(source)
    flush_recipe()
    return tuple(sources)


def just_powershell_sources(text: str | None = None) -> tuple[tuple[str, str], ...]:
    """Extract recipe lines executed by the default PowerShell adapter."""
    return just_recipe_sources(text, script_interpreter=None)


def just_python_sources(text: str | None = None) -> tuple[tuple[str, str], ...]:
    """Extract recipe bodies executed directly by Python script recipes."""
    return just_recipe_sources(text, script_interpreter="python")


def python_source_targets() -> list[str]:
    return list(script_inventory()[0])


def script_source_targets() -> list[str]:
    return [target for target, _kind in script_inventory()[2]]


def python_unittest_targets() -> list[str]:
    return list(script_inventory()[1])


def script_kind_map() -> dict[str, str]:
    return dict(script_inventory()[2])


UV_RUN_SCRIPTS = ["uv", "run", "--frozen", "--project", "scripts"]

# Several script owners intentionally use aggregate test modules instead of a
# same-stem test file. Keep that routing explicit so changed PowerShell
# helpers and shared Python utilities do not receive syntax-only validation.
SCRIPT_TEST_MODULES: dict[str, tuple[str, ...]] = {
    ".codex/hooks.json": ("scripts.test_task_continuity_hook",),
    ".codex/hooks/task-continuity-entry.ps1": ("scripts.test_task_continuity_hook",),
    ".codex/hooks/task-continuity-fast-basic.ps1": (
        "scripts.test_task_continuity_hook",
    ),
    ".codex/hooks/task-continuity-fast-compact.ps1": (
        "scripts.test_task_continuity_hook",
    ),
    ".codex/hooks/task-continuity-fast-session.ps1": (
        "scripts.test_task_continuity_hook",
    ),
    ".codex/hooks/task-continuity.ps1": ("scripts.test_task_continuity_hook",),
    "scripts/app_server_schema_runtime_check.py": ("scripts.test_dev_environment",),
    "scripts/build_codex_package.py": ("scripts.test_stage_npm_packages",),
    "scripts/cargo-lane-trash-cleanup.ps1": ("scripts.test_cargo_lane",),
    "scripts/cargo-lane.ps1": ("scripts.test_cargo_lane",),
    "scripts/common-rust-env.ps1": ("scripts.test_build_tooling_performance",),
    "scripts/codex_package/rg": (
        "scripts.codex_package.test_dotslash",
        "scripts.codex_package.test_ripgrep",
    ),
    "scripts/config_schema_check.py": ("scripts.test_dev_environment",),
    "scripts/generated_output_lock.py": ("scripts.test_dev_environment",),
    "scripts/dev_env_doctor.py": ("scripts.test_dev_environment",),
    "scripts/format.py": ("scripts.test_build_tooling",),
    "scripts/git_doctor.py": ("scripts.test_dev_environment",),
    "scripts/invoke-rust-perf-env.ps1": ("scripts.test_build_tooling_performance",),
    "scripts/install/install.ps1": ("scripts.test_build_tooling_policy",),
    "scripts/investigation_eval/score_results.py": (
        "scripts.investigation_eval.test_investigation_eval",
    ),
    "scripts/investigation_eval/validate_cases.py": (
        "scripts.investigation_eval.test_investigation_eval",
    ),
    "scripts/just-shell.py": ("scripts.test_build_tooling",),
    "scripts/kd4_model_attempt_analysis.py": ("scripts.test_kd4_perf_snapshot",),
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
    "scripts/rust_build_status.py": ("scripts.test_build_tooling_storage",),
    "scripts/rust_build_status_support.py": ("scripts.test_build_tooling_storage",),
    "scripts/rust_packages.py": ("scripts.test_build_tooling_policy",),
    "scripts/sccache-perf.ps1": ("scripts.test_build_tooling_performance",),
    "scripts/stage_npm_packages.py": ("scripts.test_stage_npm_packages",),
    "scripts/stage_npm_archives.py": ("scripts.test_stage_npm_packages",),
    "scripts/tool_versions.py": ("scripts.test_build_tooling_storage",),
    "scripts/vscode_runtime_proof.py": ("scripts.test_dev_environment",),
}


def repository_relative_path(path_text: str) -> Path | None:
    path = Path(path_text)
    if path.is_absolute():
        try:
            path = path.relative_to(REPO_ROOT)
        except ValueError:
            return None
    path = Path(*(part.lower() for part in path.parts))
    return path


def script_python_path(path_text: str) -> Path | None:
    path = repository_relative_path(path_text)
    if path is None:
        return None
    first_part = path.parts[0] if path.parts else ""
    if first_part == "scripts" and path.suffix.lower() == ".py":
        return path
    return None


def python_lint_targets(changed: Sequence[str]) -> list[str]:
    if not changed:
        return python_source_targets()
    selected = [
        path.as_posix()
        for path in (script_python_path(path_text) for path_text in changed)
        if path is not None and (REPO_ROOT / path).exists()
    ]
    return sorted(dict.fromkeys(selected))


class ChangedPathDiscoveryError(RuntimeError):
    """Raised when an implicit --changed selection cannot be determined safely."""


def git_changed_paths() -> list[str]:
    commands = (
        [
            "git",
            # Keep non-ASCII filenames as raw UTF-8 instead of C-quoted octal
            # escapes that script_python_path can never match.
            "-c",
            "core.quotepath=off",
            "diff",
            "--name-only",
            "-z",
            # Deleted scripts still own regression routes. Keep their paths so
            # changed-only validation can select the maintained mapping even
            # though the source no longer exists on disk.
            "--diff-filter=ACDMRTUXB",
            "HEAD",
            "--",
        ],
        [
            "git",
            "-c",
            "core.quotepath=off",
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
        ],
    )
    paths: list[str] = []
    for command in commands:
        try:
            result = subprocess.run(
                command,
                cwd=REPO_ROOT,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                check=False,
            )
        except OSError as error:
            raise ChangedPathDiscoveryError(
                f"could not inspect changed paths with git: {error}"
            ) from error
        if result.returncode != 0:
            detail = result.stderr.strip()
            suffix = f": {detail}" if detail else ""
            raise ChangedPathDiscoveryError(
                f"could not inspect changed paths with git{suffix}"
            )
        delimiter = "\0" if "\0" in result.stdout else "\n"
        paths.extend(path for path in result.stdout.split(delimiter) if path)
    return list(dict.fromkeys(paths))


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


def resolved_changed_paths(changed: Sequence[str | None]) -> list[str] | None:
    try:
        return expand_changed_paths(changed)
    except ChangedPathDiscoveryError as error:
        print(f"Changed-path discovery failed: {error}", file=sys.stderr)
        return None


def test_modules_for_changed_path(path_text: str) -> tuple[str, ...]:
    raw_path = repository_relative_path(path_text)
    if raw_path is None:
        return ()
    path_key = raw_path.as_posix()

    selected = list(SCRIPT_TEST_MODULES.get(path_key, ()))
    path = script_python_path(path_text)
    if path is None:
        return tuple(selected)
    module = path.with_suffix("").as_posix().replace("/", ".")
    if path.name.lower().startswith("test_"):
        selected.append(module)
    else:
        test_module = ".".join((*path.parts[:-1], f"test_{path.stem}"))
        if test_module in python_unittest_targets():
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
    if not selected and not changed:
        return python_unittest_targets()
    return sorted(dict.fromkeys(selected))


def changed_production_script_requires_test(path_text: str) -> bool:
    """Return whether a changed path is an owned production script surface."""
    path = repository_relative_path(path_text)
    if path is None:
        return False
    path_key = path.as_posix()
    if path_key in SCRIPT_TEST_MODULES:
        return True
    absolute = REPO_ROOT / path
    if not any(
        root == absolute.parent or root in absolute.parents
        for root in SCRIPT_AUDIT_ROOTS
    ):
        return False
    return (
        path.suffix.lower() in SCRIPT_CANDIDATE_SUFFIXES
        and not path.name.lower().startswith("test_")
        and path.name != "__init__.py"
    )


def script_audit_test_targets() -> list[str]:
    return python_unittest_targets()


def script_audit_context_issues() -> list[str]:
    issues: list[str] = []
    required_paths = (
        REPO_ROOT / "AGENTS.md",
        SCRIPTS_ROOT / "AGENTS.md",
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
        try:
            justfile_text = justfile_path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as error:
            issues.append(f"justfile is unreadable: {error}")
        else:
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
        try:
            just_summary = subprocess.run(
                [just, "--summary"],
                cwd=REPO_ROOT,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                check=False,
            )
        except OSError as error:
            issues.append(f"justfile parse failed: {error}")
        else:
            if just_summary.returncode != 0:
                issues.append(f"justfile parse failed: {just_summary.stderr.strip()}")
            elif "audit-scripts" not in just_summary.stdout.split():
                issues.append("justfile summary does not expose audit-scripts")

    unittest_targets = set(python_unittest_targets())
    for source, modules in SCRIPT_TEST_MODULES.items():
        if not (REPO_ROOT / source).is_file():
            issues.append(f"stale script test route: {source}")
        for module in modules:
            if module not in unittest_targets:
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


def script_audit_findings(
    *, kind_by_target: dict[str, str] | None = None
) -> tuple[list[str], list[str]]:
    errors: list[str] = []
    advisories: list[str] = []
    content_hashes: dict[str, list[str]] = {}
    kind_by_target = script_kind_map() if kind_by_target is None else kind_by_target

    for target in script_source_targets():
        path = REPO_ROOT / target
        kind = kind_by_target.get(target)
        if kind is None:
            errors.append(f"script disappeared from inventory: {target}")
            continue
        if kind.startswith("unsupported:"):
            errors.append(
                f"unsupported script type `{kind.removeprefix('unsupported:')}`: {target}"
            )
            continue

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
    kind_by_target: dict[str, str] | None = None,
) -> tuple[list[tuple[str, tuple[str, ...]]], list[str]]:
    commands: list[tuple[str, tuple[str, ...]]] = []
    missing_tools: list[str] = []
    unit_test_command: tuple[str, tuple[str, ...]] | None = None
    python_targets = python_source_targets()
    unittest_targets = python_unittest_targets()
    kind_by_target = script_kind_map() if kind_by_target is None else kind_by_target

    uv = resolve_tool("uv")
    if uv is None:
        missing_tools.append("uv")
    else:
        uv_prefix = (uv, "run", "--frozen", "--project", "scripts")
        commands.append(
            (
                "Python format",
                (*uv_prefix, "ruff", "format", "--check", *python_targets),
            )
        )
        commands.append(("Python lint", (*uv_prefix, "ruff", "check", *python_targets)))
        if include_tests:
            selected_tests = (
                unittest_targets if test_targets is None else list(test_targets)
            )
            if selected_tests:
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
        target for target, kind in kind_by_target.items() if kind == "powershell"
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
            just_sources_json = json.dumps(just_powershell_sources()).replace("'", "''")
            just_parse_script = (
                f"$sources = ConvertFrom-Json '{just_sources_json}'; "
                "$failed = $false; foreach ($item in $sources) { "
                "$tokens = $null; $errors = $null; "
                "[System.Management.Automation.Language.Parser]::ParseInput("
                "$item[1], $item[0], [ref]$tokens, [ref]$errors) | Out-Null; "
                "foreach ($parseError in $errors) { "
                "Write-Error ('{0}: {1}' -f $item[0], $parseError.Message); "
                "$failed = $true } }; if ($failed) { exit 1 }"
            )
            commands.append(
                (
                    "justfile PowerShell syntax",
                    (powershell, "-NoProfile", "-Command", just_parse_script),
                )
            )

    just_python = just_python_sources()
    if just_python:
        sources_json = json.dumps(just_python)
        python_parse_script = (
            "import json,sys; "
            "sources=json.loads(sys.argv[1]); "
            "[compile(source, name, 'exec') for name, source in sources]"
        )
        commands.append(
            (
                "justfile Python syntax",
                (sys.executable, "-c", python_parse_script, sources_json),
            )
        )

    javascript_targets = [
        target for target, kind in kind_by_target.items() if kind == "javascript"
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
    try:
        result = subprocess.run(
            ["git", "status", "--short", "--branch"],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
    except OSError as error:
        return f"git context unavailable: {error}"
    if result.returncode != 0:
        return "git context unavailable"
    lines = result.stdout.splitlines()
    branch = lines[0].removeprefix("## ") if lines else "unknown branch"
    return f"{branch}; {max(0, len(lines) - 1)} changed path(s)"


def run_script_audit(*, include_tests: bool, strict: bool) -> int:
    audit_targets = script_source_targets()
    kind_by_target = script_kind_map()
    inventory: dict[str, int] = {}
    for target in audit_targets:
        kind = kind_by_target.get(target, "unknown")
        inventory[kind] = inventory.get(kind, 0) + 1

    print(f"Script audit context: {git_context_label()}", flush=True)
    print(
        f"Inventory: {len(audit_targets)} script artifact(s) "
        + ", ".join(f"{kind}={count}" for kind, count in sorted(inventory.items())),
        flush=True,
    )
    if not include_tests:
        print("Mode: quick (full script unit tests skipped)", flush=True)

    errors = script_audit_context_issues()
    hygiene_errors, advisories = script_audit_findings(kind_by_target=kind_by_target)
    errors.extend(hygiene_errors)
    test_targets = script_audit_test_targets() if include_tests else []
    commands, missing_tools = script_audit_commands(
        include_tests=include_tests,
        test_targets=test_targets,
        kind_by_target=kind_by_target,
    )
    errors.extend(f"required audit tool is missing: {tool}" for tool in missing_tools)

    for issue in errors:
        print(f"[FAIL] {issue}", flush=True)
    for advisory in advisories:
        print(f"[ADVISORY] {advisory}", flush=True)
    failed_commands: list[str] = []
    passed_commands = 0
    for label, command in commands:
        print(f"[RUN] {label}", flush=True)
        try:
            returncode = run(command)
        except OSError as exc:
            print(f"[FAIL] {label}: {exc}", flush=True)
            failed_commands.append(label)
            continue
        if returncode == 0:
            print(f"[PASS] {label}", flush=True)
            passed_commands += 1
        else:
            print(f"[FAIL] {label}: exit {returncode}", flush=True)
            failed_commands.append(label)

    strict_failure = strict and bool(advisories)
    if strict_failure:
        print(
            "[FAIL] --strict promoted optimization advisories to failures", flush=True
        )
    if errors or failed_commands or strict_failure:
        print(
            "SCRIPT AUDIT FAILED: "
            f"{len(errors)} internal/context failure(s), "
            f"{len(failed_commands)} command failure(s), "
            f"{len(advisories)} advisory item(s).",
            flush=True,
        )
        return 1

    print(
        "SCRIPT AUDIT PASSED: "
        f"{len(audit_targets)} script artifact(s), "
        f"{passed_commands} command group(s), "
        f"{len(advisories)} advisory item(s).",
        flush=True,
    )
    return 0


def run(command: Sequence[str]) -> int:
    executable = which(command[0]) or command[0]
    try:
        return subprocess.run([executable, *command[1:]], cwd=REPO_ROOT).returncode
    except OSError as error:
        print(f"Could not run {command[0]}: {error}", file=sys.stderr)
        return 127


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Run root package maintenance commands.",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

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
        help="Run a specific unittest module, such as scripts.test_build_tooling_policy.",
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

    if args.command == "lint-python":
        command = [*UV_RUN_SCRIPTS, "ruff", "check"]
        if args.fix:
            command.append("--fix")
        changed_paths = resolved_changed_paths(args.changed)
        if changed_paths is None:
            return 2
        targets = (
            python_lint_targets(changed_paths)
            if changed_paths or not args.changed
            else []
        )
        if not targets:
            print("No matching changed Python files to lint.")
            return 0
        return run([*command, *targets])

    if args.command == "test-python":
        changed_paths = resolved_changed_paths(args.changed)
        if changed_paths is None:
            return 2
        targets = (
            python_test_targets(args.module, changed_paths)
            if changed_paths or not args.changed or args.module
            else []
        )
        if not targets:
            print("No matching changed Python test modules to run.")
            if args.changed and any(
                changed_production_script_requires_test(path) for path in changed_paths
            ):
                print(
                    "Changed production script validation is unverified because no "
                    "focused test route was selected.",
                    file=sys.stderr,
                )
                return 2
            return 0
        return run(
            [
                *UV_RUN_SCRIPTS,
                "python",
                "-m",
                "unittest",
                *targets,
                "-v",
            ]
        )

    if args.command == "audit-scripts":
        return run_script_audit(
            include_tests=not args.quick,
            strict=args.strict,
        )

    raise AssertionError(f"unhandled command: {args.command}")


if __name__ == "__main__":
    raise SystemExit(main())
