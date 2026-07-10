#!/usr/bin/env python3
"""Scope-locked local verification router for this checkout."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import fnmatch
import hashlib
import json
from pathlib import Path
from shutil import which
import subprocess
import sys
from typing import Any, Sequence


REPO_ROOT = Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.verify_local_context import (  # noqa: E402
    BASH_PARSE_SCRIPT_TEMPLATE as BASH_PARSE_SCRIPT_TEMPLATE,
    CACHE_PATH as CACHE_PATH,
    CODEX_RS as CODEX_RS,
    EXIT_CODES as EXIT_CODES,
    FAILED as FAILED,
    IGNORED_DIR_PARTS as IGNORED_DIR_PARTS,
    INCONCLUSIVE as INCONCLUSIVE,
    LEDGER_PATH as LEDGER_PATH,
    LOG_DIR as LOG_DIR,
    NEEDS_REGEN as NEEDS_REGEN,
    NEEDS_SCOPE as NEEDS_SCOPE,
    POWERSHELL_PARSE_SCRIPT as POWERSHELL_PARSE_SCRIPT,
    RULES_PATH as RULES_PATH,
    SCOPE_PATH as SCOPE_PATH,
    STATE_DIR as STATE_DIR,
    TIMEOUTS as TIMEOUTS,
    TOOLING_ERROR as TOOLING_ERROR,
    VERIFIED as VERIFIED,
    VERIFIED_NO_PROOF as VERIFIED_NO_PROOF,
    VERIFY_LOCAL_CONTROL_PATHS as VERIFY_LOCAL_CONTROL_PATHS,
    PackageInfo as PackageInfo,
    CargoGraph as CargoGraph,
    SurfaceRule as SurfaceRule,
    Scope as Scope,
    CommandSpec as CommandSpec,
    CommandResult as CommandResult,
    Plan as Plan,
    rel as rel,
    shell_join as shell_join,
    kill_process_tree as kill_process_tree,
    run_capture as run_capture,
    git_subcommand as git_subcommand,
    git_is_read_only_inspection as git_is_read_only_inspection,
    git_failed_for_dubious_ownership as git_failed_for_dubious_ownership,
    git_command as git_command,
    git_capture as git_capture,
    git as git,
    current_branch as current_branch,
    current_head as current_head,
    is_ancestor as is_ancestor,
    normalize_path as normalize_path,
    normalize_paths as normalize_paths,
    stable_unique as stable_unique,
    path_id as path_id,
    bash_parse_script as bash_parse_script,
    git_name_list as git_name_list,
    staged_files as staged_files,
    unstaged_files as unstaged_files,
    untracked_files as untracked_files,
    dirty_files as dirty_files,
    parse_last_json_value as parse_last_json_value,
    load_cargo_metadata as load_cargo_metadata,
    path_matches_rule_pattern as path_matches_rule_pattern,
    load_rules as load_rules,
    matching_rules as matching_rules,
    package_for_path as package_for_path,
    owner_packages as owner_packages,
    is_ignored_build_output as is_ignored_build_output,
    classify_dirty_group as classify_dirty_group,
    group_dirty_files as group_dirty_files,
    atomic_write_json as atomic_write_json,
    configure_runtime as _configure_context_runtime,
)
from scripts.verify_local_execution import (  # noqa: E402
    proof_input_files as proof_input_files,
    selected_hash_roots as selected_hash_roots,
    git_list_selected_files as git_list_selected_files,
    working_tree_hash as working_tree_hash,
    scoped_file_hash as scoped_file_hash,
    load_cache as load_cache,
    surface_paths_hash as surface_paths_hash,
    cache_key as cache_key,
    append_ledger as append_ledger,
    log_path_for as log_path_for,
    summarize_failure as summarize_failure,
    execute_command as execute_command,
    baseline_ref_for_scope as baseline_ref_for_scope,
    baseline_command_result as baseline_command_result,
    reached_test_execution as reached_test_execution,
    should_retry_for_flake as should_retry_for_flake,
    result_to_json as result_to_json,
    scope_to_json as scope_to_json,
    ledger_entry as ledger_entry,
    execute_plan as execute_plan,
    print_plan as print_plan,
    plan_to_json as plan_to_json,
    configure_runtime as _configure_execution_runtime,
)

_configure_context_runtime(sys.modules[__name__])
_configure_execution_runtime(sys.modules[__name__])


def scope_state() -> dict[str, Any] | None:
    if not SCOPE_PATH.exists():
        return None
    try:
        return json.loads(SCOPE_PATH.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None


def scope_name_for_paths(paths: Sequence[Path]) -> str:
    if not paths:
        return "empty"
    digest = hashlib.sha256(
        "\n".join(path.as_posix() for path in paths).encode("utf-8")
    ).hexdigest()[:10]
    first = paths[0].stem or paths[0].name or "scope"
    return f"{first}-{digest}"


def create_scope(
    name: str, paths: list[Path], graph: CargoGraph, rules: Sequence[SurfaceRule]
) -> dict[str, Any]:
    state = {
        "scope_id": name,
        "branch": current_branch(),
        "base_commit": current_head(),
        "owned_paths": [path.as_posix() for path in paths],
        "owned_packages": owner_packages(paths, graph, rules),
        "created_at": datetime.now(timezone.utc).isoformat(),
        "initial_dirty_paths": [path.as_posix() for path in dirty_files()],
    }
    atomic_write_json(SCOPE_PATH, state)
    return state


def scope_stale_reasons(state: dict[str, Any], graph: CargoGraph) -> list[str]:
    reasons: list[str] = []
    if state.get("branch") and state.get("branch") != current_branch():
        reasons.append("branch changed since scope creation")
    base = str(state.get("base_commit", ""))
    if base and not is_ancestor(base):
        reasons.append("base commit is no longer an ancestor of HEAD")
    owned_paths = [Path(path) for path in state.get("owned_paths", [])]
    dirty = {path.as_posix() for path in dirty_files()}
    if owned_paths and not any(path.as_posix() in dirty for path in owned_paths):
        reasons.append("none of the scoped files are currently dirty or staged")
    if len(dirty) >= 4:
        outside = [path for path in dirty if Path(path) not in owned_paths]
        if len(outside) > len(dirty) // 2:
            reasons.append("current dirty files are mostly outside the sticky scope")
    for package in state.get("owned_packages", []):
        if package not in graph.packages_by_name:
            reasons.append(f"owned package no longer exists: {package}")
    for path in owned_paths:
        if not (REPO_ROOT / path).exists() and path.as_posix() not in dirty:
            reasons.append(
                f"scope references deleted path without replacement: {path.as_posix()}"
            )
    return reasons


def build_scope(
    scope_id: str,
    source: str,
    active_files: list[Path],
    all_dirty: list[Path],
    graph: CargoGraph,
    rules: Sequence[SurfaceRule],
) -> Scope:
    active = stable_unique(active_files)
    packages = owner_packages(active, graph, rules)
    active_set = {path.as_posix() for path in active}
    ignored = [
        path
        for path in all_dirty
        if path.as_posix() not in active_set and not is_ignored_build_output(path)
    ]
    adjacent = sorted(graph.direct_reverse_deps(packages) - set(packages))
    return Scope(
        scope_id=scope_id,
        source=source,
        active_files=active,
        owned_packages=packages,
        ignored_dirty_files=ignored,
        adjacent_packages=adjacent,
        surface_rules=[rule.id for rule in matching_rules(active, rules)],
    )


def select_scope(
    args: argparse.Namespace, graph: CargoGraph, rules: Sequence[SurfaceRule]
) -> tuple[Scope | None, str | None]:
    all_dirty = dirty_files()
    if args.scope_reset:
        if SCOPE_PATH.exists():
            SCOPE_PATH.unlink()
        return Scope("scope-reset", "scope-reset", [], []), None
    if args.scope_start:
        changed = normalize_paths(args.changed or [])
        if not changed:
            return None, "--scope-start requires at least one --changed path"
        state = create_scope(args.scope_start, changed, graph, rules)
        return build_scope(
            state["scope_id"], "scope-start", changed, all_dirty, graph, rules
        ), None
    if args.scope_add:
        state = scope_state()
        if state is None:
            return None, "--scope-add requires an existing sticky scope"
        additions = normalize_paths(args.scope_add)
        owned = stable_unique(
            [Path(path) for path in state.get("owned_paths", [])] + additions
        )
        state["owned_paths"] = [path.as_posix() for path in owned]
        state["owned_packages"] = owner_packages(owned, graph, rules)
        atomic_write_json(SCOPE_PATH, state)
        return build_scope(
            str(state.get("scope_id", "current")),
            "scope-add",
            owned,
            all_dirty,
            graph,
            rules,
        ), None
    if args.changed:
        paths = normalize_paths(args.changed)
        return build_scope(
            scope_name_for_paths(paths), "changed", paths, all_dirty, graph, rules
        ), None
    if args.scope == "current":
        state = scope_state()
        if state is None:
            return None, "no sticky scope exists"
        paths = [Path(path) for path in state.get("owned_paths", [])]
        scope = build_scope(
            str(state.get("scope_id", "current")),
            "scope-current",
            paths,
            all_dirty,
            graph,
            rules,
        )
        scope.stale_reasons = scope_stale_reasons(state, graph)
        if scope.stale_reasons:
            return scope, "sticky scope appears stale"
        return scope, None
    staged = staged_files()
    if args.staged:
        if not staged:
            return None, "--staged selected but no staged files were found"
        return build_scope(
            scope_name_for_paths(staged), "staged", staged, all_dirty, graph, rules
        ), None
    if args.all_dirty:
        # An explicit --all-dirty must win over the implicit staged-first
        # default, otherwise staged files silently shrink the requested scope.
        return build_scope("all-dirty", "all-dirty", all_dirty, [], graph, rules), None
    if staged:
        return build_scope(
            scope_name_for_paths(staged), "staged", staged, all_dirty, graph, rules
        ), None
    groups = group_dirty_files(all_dirty, graph, rules)
    if len(groups) == 1:
        paths = next(iter(groups.values()))
        return build_scope(
            scope_name_for_paths(paths), "single-dirty-group", paths, [], graph, rules
        ), None
    if not groups:
        return Scope("empty", "empty", [], []), None
    return Scope(
        "needs-scope", "dirty-groups", [], [], dirty_groups=groups
    ), "multiple dirty groups detected"


def enabled_expansions(args: argparse.Namespace) -> list[str]:
    result: list[str] = []
    for name in [
        "related",
        "related_tests",
        "allow_workspace",
        "all_dirty",
        "isolated",
        "regen",
        "baseline",
        "no_cache",
        "cache_readonly",
    ]:
        if getattr(args, name, False):
            result.append("--" + name.replace("_", "-"))
    return result


def test_exprs_for_scope(scope: Scope, rules: Sequence[SurfaceRule]) -> list[str]:
    return [
        rule.test_expr
        for rule in rules
        if rule.id in scope.surface_rules and rule.test_expr
    ]


def active_scope_has_tests(scope: Scope) -> bool:
    return any(
        "_test" in path.stem or path.stem == "tests" or "/tests/" in path.as_posix()
        for path in scope.active_files
    )


def owner_commands(
    packages: list[str],
    args: argparse.Namespace,
    scope: Scope,
    rules: Sequence[SurfaceRule],
) -> list[CommandSpec]:
    if not packages:
        return []
    exprs = test_exprs_for_scope(scope, rules)
    if (
        args.fast
        and not args.isolated
        and not exprs
        and not active_scope_has_tests(scope)
    ):
        return [
            CommandSpec(
                id=f"owner-check:{package}",
                kind="owner_check",
                args=("just", "check-lane", package),
                timeout=TIMEOUTS["owner_test"],
                owner_packages=(package,),
                reason="fast owner package compile proof",
            )
            for package in packages
        ]
    if args.isolated:
        return [
            CommandSpec(
                id=f"owner-test:{package}",
                kind="owner_test",
                args=("just", "test-lane-package", package),
                timeout=TIMEOUTS["owner_test"],
                owner_packages=(package,),
                reason="isolated owner package proof",
            )
            for package in packages
        ]
    if len(packages) == 1 and exprs:
        command_args = ["just", "test-fast", "-p", packages[0]]
        command_args.extend(
            ["-E", " | ".join(f"({expr.strip()})" for expr in exprs if expr.strip())]
        )
        return [
            CommandSpec(
                id=f"owner-test:{packages[0]}",
                kind="owner_test",
                args=tuple(command_args),
                timeout=TIMEOUTS["owner_test"],
                owner_packages=(packages[0],),
                reason="owner package proof",
            )
        ]
    return [
        CommandSpec(
            id=f"owner-test:{package}",
            kind="owner_test",
            args=("just", "test-fast", "-p", package),
            timeout=TIMEOUTS["owner_test"],
            owner_packages=(package,),
            reason="owner package proof",
        )
        for package in packages
    ]


def hygiene_command(scope: Scope) -> CommandSpec | None:
    paths = list(scope.active_files)
    if not paths:
        return None
    return CommandSpec(
        id="hygiene:diff-check",
        kind="hygiene",
        # Diff against HEAD so staged changes are checked too: the default
        # scope source is staged files, and a plain worktree-vs-index diff is
        # empty exactly for those.
        args=(
            "git",
            "diff",
            "--check",
            "HEAD",
            "--",
            *(path.as_posix() for path in paths),
        ),
        timeout=TIMEOUTS["hygiene"],
        reason="scoped diff whitespace check",
    )


def fmt_check_command() -> CommandSpec:
    return CommandSpec(
        id="formatter:fmt-check-fast",
        kind="formatter",
        args=("just", "fmt-check-fast"),
        timeout=TIMEOUTS["formatter"],
        reason="check-only fast Rust/just formatter",
    )


def python_executable() -> str:
    executable = Path(sys.executable)
    if executable.is_absolute():
        return str(executable)
    return which(sys.executable) or sys.executable


def root_maintenance_args(*args: str) -> tuple[str, ...]:
    return (python_executable(), "-m", "scripts.root_maintenance", *args)


def script_paths(scope: Scope) -> list[Path]:
    return [
        path
        for path in scope.active_files
        if path.parts and path.parts[0] == "scripts" and path.suffix == ".py"
    ]


def powershell_script_paths(scope: Scope) -> list[Path]:
    return [
        path
        for path in scope.active_files
        if path.parts and path.parts[0] == "scripts" and path.suffix == ".ps1"
    ]


def shell_script_paths(scope: Scope) -> list[Path]:
    return [
        path
        for path in scope.active_files
        if path.parts and path.parts[0] == "scripts" and path.suffix == ".sh"
    ]


def verify_local_control_paths(scope: Scope) -> list[Path]:
    return [
        path
        for path in scope.active_files
        if path.as_posix() in VERIFY_LOCAL_CONTROL_PATHS
    ]


def justfile_check_command(scope: Scope) -> CommandSpec | None:
    if not any(path.as_posix() == "justfile" for path in scope.active_files):
        return None
    return CommandSpec(
        id="justfile:summary",
        kind="justfile_check",
        args=("just", "--summary"),
        timeout=TIMEOUTS["hygiene"],
        reason="parse justfile recipes",
    )


def script_validation_commands(scope: Scope) -> list[CommandSpec]:
    from scripts.root_maintenance import test_modules_for_changed_path

    paths = script_paths(scope)
    ps_paths = powershell_script_paths(scope)
    sh_paths = shell_script_paths(scope)
    control_paths = verify_local_control_paths(scope)
    if not paths and not ps_paths and not sh_paths and not control_paths:
        return []
    changed_args: list[str] = []
    for path in paths:
        changed_args.extend(["--changed", path.as_posix()])
    commands: list[CommandSpec] = []
    for path in ps_paths:
        commands.append(
            CommandSpec(
                id=f"script-syntax:powershell:{path_id(path)}",
                kind="script_syntax",
                args=(
                    "pwsh",
                    "-NoProfile",
                    "-Command",
                    POWERSHELL_PARSE_SCRIPT,
                    path.as_posix(),
                ),
                timeout=TIMEOUTS["script"],
                reason="PowerShell script parse check",
            )
        )
    for path in sh_paths:
        commands.append(
            CommandSpec(
                id=f"script-syntax:shell:{path_id(path)}",
                kind="script_syntax",
                args=("bash", "-lc", bash_parse_script(path)),
                timeout=TIMEOUTS["script"],
                reason="shell script parse check",
            )
        )
    if paths:
        commands.append(
            CommandSpec(
                id="script-lint:" + "+".join(path.stem for path in paths),
                kind="script_lint",
                args=root_maintenance_args(
                    "lint-python",
                    *changed_args,
                ),
                timeout=TIMEOUTS["script"],
                reason="scoped Python script lint check",
            )
        )
    test_paths = [
        path
        for path in (*paths, *ps_paths, *sh_paths)
        if test_modules_for_changed_path(path.as_posix())
    ]
    if not test_paths and control_paths:
        commands.append(
            CommandSpec(
                id="script-test:verify_local_controls",
                kind="script_test",
                args=root_maintenance_args(
                    "test-python",
                    "--module",
                    "scripts.test_verify_local",
                ),
                timeout=TIMEOUTS["script"],
                reason="nearest verify-local router tests",
            )
        )
        return commands
    if not test_paths:
        return commands
    test_changed_args: list[str] = []
    for path in test_paths:
        test_changed_args.extend(["--changed", path.as_posix()])
    commands.append(
        CommandSpec(
            id="script-test:" + "+".join(path.stem for path in test_paths),
            kind="script_test",
            args=root_maintenance_args(
                "test-python",
                *test_changed_args,
            ),
            timeout=TIMEOUTS["script"],
            reason="nearest Python script tests",
        )
    )
    return commands


def needs_python_formatter(path: Path) -> bool:
    return path.parts and path.parts[0] == "scripts" and path.suffix == ".py"


def needs_prettier_formatter(path: Path) -> bool:
    text = path.as_posix()
    return (
        text
        in {"package.json", "knip.json", "pnpm-workspace.yaml", "eslint.config.mjs"}
        or fnmatch.fnmatch(text, "docs/*.md")
        or fnmatch.fnmatch(text, ".github/workflows/*.yml")
        or (text.startswith("codex-cli/") and text.endswith(".js"))
        or (
            text.startswith("sdk/typescript/")
            and (text.endswith(".js") or text.endswith(".ts"))
        )
    )


def final_formatter_commands(scope: Scope) -> list[CommandSpec]:
    commands: list[CommandSpec] = []
    if any(
        path.as_posix().startswith("codex-rs/") or path.as_posix() == "justfile"
        for path in scope.active_files
    ):
        commands.append(fmt_check_command())
    if any(needs_python_formatter(path) for path in scope.active_files):
        changed_args: list[str] = []
        for path in script_paths(scope):
            changed_args.extend(["--changed", path.as_posix()])
        commands.append(
            CommandSpec(
                id="formatter:format-python",
                kind="formatter",
                args=root_maintenance_args(
                    "format-python",
                    *changed_args,
                ),
                timeout=TIMEOUTS["formatter"],
                reason="check-only Python script formatter",
            )
        )
    if any(needs_prettier_formatter(path) for path in scope.active_files):
        commands.append(
            CommandSpec(
                id="formatter:format-prettier",
                kind="formatter",
                args=root_maintenance_args("format-prettier"),
                timeout=TIMEOUTS["formatter"],
                reason="check-only Prettier formatter",
            )
        )
    return commands


def adjacent_commands(packages: list[str]) -> list[CommandSpec]:
    return [
        CommandSpec(
            id=f"adjacent-check:{package}",
            kind="owner_check",
            args=("just", "check-lane", package),
            timeout=TIMEOUTS["owner_check"],
            owner_packages=(package,),
            reason="explicit adjacent compile check",
        )
        for package in packages[:3]
    ]


def related_test_commands(packages: list[str]) -> list[CommandSpec]:
    return [
        CommandSpec(
            id=f"related-test:{package}",
            kind="related_test",
            args=("just", "test-lane-package", package),
            timeout=TIMEOUTS["owner_test"],
            owner_packages=(package,),
            reason="explicit related package test proof",
        )
        for package in packages[:3]
    ]


def surface_commands(
    args: argparse.Namespace, scope: Scope, rules: Sequence[SurfaceRule]
) -> list[CommandSpec]:
    commands: list[CommandSpec] = []
    for rule in rules:
        if rule.id not in scope.surface_rules:
            continue
        command_args = (
            rule.regen_command
            if args.regen and rule.regen_command
            else rule.validation_command
        )
        if command_args is None:
            continue
        regenerating = args.regen and rule.regen_command is not None
        commands.append(
            CommandSpec(
                id=f"surface:{rule.id}:{'regen' if regenerating else 'validate'}",
                kind="surface_regen" if regenerating else "surface_validation",
                args=command_args,
                timeout=TIMEOUTS["schema"],
                owner_packages=rule.owned_packages,
                hash_paths=tuple(dict.fromkeys(rule.paths + rule.hash_paths)),
                reason=f"{rule.id} surface {'regeneration and validation' if regenerating else 'validation'}",
            )
        )
    return commands


def plan_commands(
    args: argparse.Namespace, scope: Scope, rules: Sequence[SurfaceRule]
) -> Plan:
    mode = "final" if args.final else "fast" if args.fast else "plan"
    enabled = enabled_expansions(args)
    skipped: list[dict[str, str]] = []
    if scope.stale_reasons:
        return Plan(
            mode, scope, [], skipped, verdict=INCONCLUSIVE, enabled_expansions=enabled
        )
    if scope.source == "scope-reset":
        return Plan(
            mode,
            scope,
            [],
            skipped,
            verdict=VERIFIED_NO_PROOF,
            enabled_expansions=enabled,
        )
    if scope.source == "dirty-groups":
        return Plan(
            mode, scope, [], skipped, verdict=NEEDS_SCOPE, enabled_expansions=enabled
        )
    if not scope.active_files:
        return Plan(
            mode,
            scope,
            [],
            skipped,
            verdict=VERIFIED_NO_PROOF,
            enabled_expansions=enabled,
        )
    skipped.append(
        {"item": "workspace tests", "reason": "blocked unless --allow-workspace is set"}
    )
    if scope.adjacent_packages and not args.related_tests:
        skipped.append(
            {"item": "related tests", "reason": "blocked unless --related-tests is set"}
        )
    skipped.append(
        {"item": "just fmt", "reason": "mutating formatter is not verification"}
    )
    if scope.adjacent_packages:
        skipped.append(
            {
                "item": ", ".join(scope.adjacent_packages),
                "reason": "adjacent packages are compile-check only and require --related or --allow-workspace",
            }
        )
    commands = surface_commands(args, scope, rules)
    matched_rules = [rule for rule in rules if rule.id in scope.surface_rules]
    owner_packages = scope.owned_packages
    if any(rule.skip_owner_tests for rule in matched_rules):
        suppressed = sorted(
            {
                package
                for rule in matched_rules
                if rule.skip_owner_tests
                for package in rule.owned_packages
            }
        )
        owner_packages = [
            package for package in owner_packages if package not in suppressed
        ]
        if suppressed:
            skipped.append(
                {
                    "item": ", ".join(suppressed),
                    "reason": "surface rule owns focused validation",
                }
            )
    commands.extend(owner_commands(owner_packages, args, scope, rules))
    commands.extend(script_validation_commands(scope))
    if (args.related or args.allow_workspace) and scope.adjacent_packages:
        commands.extend(adjacent_commands(scope.adjacent_packages))
    if args.related_tests and scope.adjacent_packages:
        commands.extend(related_test_commands(scope.adjacent_packages))
    if mode == "final":
        commands = [*final_formatter_commands(scope), *commands]
    j = justfile_check_command(scope)
    if j is not None:
        commands.append(j)
    h = hygiene_command(scope)
    if h is not None:
        commands.append(h)
    return Plan(mode, scope, commands, skipped, enabled_expansions=enabled)


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Scope-locked local verification router"
    )
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument("--plan", action="store_true")
    modes.add_argument("--fast", action="store_true")
    modes.add_argument("--final", action="store_true")
    parser.add_argument("--changed", action="append", default=[])
    parser.add_argument("--staged", action="store_true")
    parser.add_argument("--all-dirty", action="store_true")
    parser.add_argument("--scope-start")
    parser.add_argument("--scope", choices=["current"])
    parser.add_argument("--scope-add", action="append", default=[])
    parser.add_argument("--scope-reset", action="store_true")
    parser.add_argument("--related", action="store_true")
    parser.add_argument("--related-tests", action="store_true")
    parser.add_argument("--allow-workspace", action="store_true")
    parser.add_argument("--isolated", action="store_true")
    parser.add_argument("--regen", action="store_true")
    parser.add_argument("--baseline", action="store_true")
    parser.add_argument("--retry-flakes", action="store_true")
    parser.add_argument("--no-cache", action="store_true")
    parser.add_argument("--cache-readonly", action="store_true")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    if not args.plan and not args.fast and not args.final:
        args.plan = True
    return args


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        graph = load_cargo_metadata()
        rules = load_rules()
        scope, scope_error = select_scope(args, graph, rules)
        if scope is None:
            if args.json:
                print(
                    json.dumps({"verdict": NEEDS_SCOPE, "error": scope_error}, indent=2)
                )
            else:
                print(f"{NEEDS_SCOPE}: {scope_error}")
            return EXIT_CODES[NEEDS_SCOPE]
        plan = plan_commands(args, scope, rules)
        if scope_error:
            plan.verdict = plan.verdict or (
                NEEDS_SCOPE if scope.source == "dirty-groups" else INCONCLUSIVE
            )
            plan.skipped.append({"item": "scope selection", "reason": scope_error})
        if args.plan:
            verdict = plan.verdict or "PLANNED"
            if args.json:
                print(
                    json.dumps(
                        plan_to_json(plan, verdict, [], []), indent=2, default=str
                    )
                )
            else:
                print_plan(plan, verdict=verdict)
            return 0 if verdict == "PLANNED" else EXIT_CODES.get(verdict, 0)
        verdict, results, cache_miss_reasons = execute_plan(plan, graph, args)
        if args.json:
            print(
                json.dumps(
                    plan_to_json(plan, verdict, results, cache_miss_reasons),
                    indent=2,
                    default=str,
                )
            )
        else:
            print_plan(
                plan,
                verdict=verdict,
                results=results,
                cache_miss_reasons=cache_miss_reasons,
            )
        return EXIT_CODES.get(verdict, 4)
    except subprocess.CalledProcessError as exc:
        message = exc.stderr or exc.output or str(exc)
        if args.json:
            print(json.dumps({"verdict": TOOLING_ERROR, "error": message}, indent=2))
        else:
            print(f"{TOOLING_ERROR}: {message}")
        return EXIT_CODES[TOOLING_ERROR]
    except Exception as exc:
        if args.json:
            print(json.dumps({"verdict": TOOLING_ERROR, "error": str(exc)}, indent=2))
        else:
            print(f"{TOOLING_ERROR}: {exc}")
        return EXIT_CODES[TOOLING_ERROR]


if __name__ == "__main__":
    raise SystemExit(main())
