#!/usr/bin/env python3
from __future__ import annotations

import argparse
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


REPO_ROOT = Path(__file__).resolve().parents[1]


def load_module():
    path = REPO_ROOT / "scripts" / "verify_local.py"
    spec = importlib.util.spec_from_file_location("verify_local", path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def args(**overrides):
    defaults = dict(
        plan=True,
        fast=False,
        final=False,
        changed=[],
        staged=False,
        all_dirty=False,
        scope_start=None,
        scope=None,
        scope_add=[],
        scope_reset=False,
        related=False,
        related_tests=False,
        allow_workspace=False,
        isolated=False,
        regen=False,
        baseline=False,
        retry_flakes=False,
        no_cache=False,
        cache_readonly=False,
        json=False,
    )
    defaults.update(overrides)
    return argparse.Namespace(**defaults)


class VerifyLocalPlannerTest(unittest.TestCase):
    def setUp(self) -> None:
        self.v = load_module()
        self.core = self.v.PackageInfo(
            "codex-core",
            REPO_ROOT / "codex-rs" / "core",
            REPO_ROOT / "codex-rs" / "core" / "Cargo.toml",
            "core-id",
        )
        self.shell = self.v.PackageInfo(
            "codex-shell-command",
            REPO_ROOT / "codex-rs" / "shell-command",
            REPO_ROOT / "codex-rs" / "shell-command" / "Cargo.toml",
            "shell-id",
        )
        self.util = self.v.PackageInfo(
            "codex-util",
            REPO_ROOT / "codex-rs" / "utils" / "string",
            REPO_ROOT / "codex-rs" / "utils" / "string" / "Cargo.toml",
            "util-id",
        )
        self.app_server = self.v.PackageInfo(
            "codex-app-server",
            REPO_ROOT / "codex-rs" / "app-server",
            REPO_ROOT / "codex-rs" / "app-server" / "Cargo.toml",
            "app-server-id",
        )
        self.app_server_protocol = self.v.PackageInfo(
            "codex-app-server-protocol",
            REPO_ROOT / "codex-rs" / "app-server-protocol",
            REPO_ROOT / "codex-rs" / "app-server-protocol" / "Cargo.toml",
            "app-server-protocol-id",
        )
        packages = [
            self.core,
            self.shell,
            self.util,
            self.app_server,
            self.app_server_protocol,
        ]
        self.graph = self.v.CargoGraph(
            packages_by_name={p.name: p for p in packages},
            packages_by_id={p.package_id: p for p in packages},
            deps_by_name={
                "codex-core": {"codex-shell-command"},
                "codex-shell-command": {"codex-util"},
                "codex-util": set(),
                "codex-app-server": {"codex-app-server-protocol", "codex-core"},
                "codex-app-server-protocol": set(),
            },
            reverse_deps_by_name={
                "codex-shell-command": {"codex-core"},
                "codex-util": {"codex-shell-command"},
                "codex-core": {"codex-app-server"},
                "codex-app-server": set(),
                "codex-app-server-protocol": {"codex-app-server"},
            },
        )
        self.rule = self.v.SurfaceRule(
            id="command-safety-shape",
            paths=("codex-rs/core/src/tools/handlers/command_shape.rs",),
            owned_packages=("codex-core",),
            test_expr="test(command_shape)",
        )
        self.schema_rule = self.v.SurfaceRule(
            id="app-server-schema",
            paths=("codex-rs/app-server-protocol/src",),
            owned_packages=("codex-app-server", "codex-app-server-protocol"),
            validation_command=("just", "app-server-schema-check"),
            regen_command=("just", "app-server-schema-check-force"),
            skip_owner_tests=True,
        )

    def patch_package_lookup(self):
        def fake(path, _graph):
            path_text = path.as_posix()
            if path_text.startswith("codex-rs/core/"):
                return self.core
            if path_text.startswith("codex-rs/shell-command/"):
                return self.shell
            if path_text.startswith("codex-rs/utils/string/"):
                return self.util
            if path_text.startswith("codex-rs/app-server-protocol/"):
                return self.app_server_protocol
            if path_text.startswith("codex-rs/app-server/"):
                return self.app_server
            return None

        return mock.patch.object(self.v, "package_for_path", side_effect=fake)

    def test_git_retries_dubious_ownership_for_read_only_inspection(self) -> None:
        dubious = subprocess.CompletedProcess(
            ["git", "diff", "--name-only"],
            128,
            "",
            "fatal: detected dubious ownership in repository at 'C:/repo'\n"
            "To add an exception for this directory, call:\n\n"
            "\tgit config --global --add safe.directory C:/repo\n",
        )
        recovered = subprocess.CompletedProcess(
            [
                "git",
                "-c",
                f"safe.directory={self.v.REPO_ROOT}",
                "diff",
                "--name-only",
            ],
            0,
            "scripts/verify_local.py\n",
            "",
        )

        with mock.patch.object(
            self.v, "run_capture", side_effect=[dubious, recovered]
        ) as run_capture:
            output = self.v.git(["diff", "--name-only"], check=False)

        self.assertEqual(output, "scripts/verify_local.py\n")
        first, second = run_capture.call_args_list
        self.assertEqual(first.args[0], ["git", "diff", "--name-only"])
        self.assertEqual(
            second.args[0],
            [
                "git",
                "-c",
                f"safe.directory={self.v.REPO_ROOT}",
                "diff",
                "--name-only",
            ],
        )

    def test_git_name_list_preserves_newlines_and_spaces_in_paths(self) -> None:
        with mock.patch.object(
            self.v,
            "git",
            return_value="scripts/line\nbreak.py\0scripts/ trailing .py\0",
        ) as git:
            paths = self.v.git_name_list(
                ["diff", "--name-only", "--diff-filter=ACMRTD"]
            )

        self.assertEqual(
            [path.as_posix() for path in paths],
            ["scripts/line\nbreak.py", "scripts/ trailing .py"],
        )
        self.assertEqual(git.call_args.args[0][-1], "-z")

    def test_git_does_not_retry_dubious_ownership_for_non_inspection(self) -> None:
        dubious = subprocess.CompletedProcess(
            ["git", "branch", "new-branch"],
            128,
            "",
            "fatal: detected dubious ownership in repository at 'C:/repo'\n",
        )

        with mock.patch.object(
            self.v, "run_capture", return_value=dubious
        ) as run_capture:
            output = self.v.git(["branch", "new-branch"], check=False)

        self.assertEqual(output, "")
        run_capture.assert_called_once_with(
            ["git", "branch", "new-branch"], timeout=60, check=False
        )

    def test_powershell_safety_scope_groups_codex_core_once(self) -> None:
        changed = [Path("codex-rs/core/src/tools/handlers/command_shape.rs")]
        with self.patch_package_lookup():
            scope = self.v.build_scope(
                "powershell",
                "changed",
                changed,
                [*changed, Path("docs/foo.md")],
                self.graph,
                [self.rule],
            )
            plan = self.v.plan_commands(args(fast=True, plan=False), scope, [self.rule])

        self.assertEqual(scope.owned_packages, ["codex-core"])
        self.assertEqual(len([c for c in plan.commands if c.kind == "owner_test"]), 1)
        command = plan.commands[0].args
        self.assertIn("codex-core", command)
        self.assertIn("-E", command)
        skipped_text = "\n".join(item["reason"] for item in plan.skipped)
        self.assertIn("blocked unless --allow-workspace", skipped_text)
        self.assertTrue(scope.ignored_dirty_files)

    def test_fast_owner_source_scope_uses_compile_check(self) -> None:
        changed = [Path("codex-rs/shell-command/src/lib.rs")]
        with self.patch_package_lookup():
            scope = self.v.build_scope(
                "shell", "changed", changed, changed, self.graph, []
            )
            plan = self.v.plan_commands(args(fast=True, plan=False), scope, [])

        owner_checks = [
            command for command in plan.commands if command.kind == "owner_check"
        ]
        self.assertEqual(len(owner_checks), 1)
        self.assertEqual(
            owner_checks[0].args, ("just", "check-lane", "codex-shell-command")
        )

    def test_fast_owner_test_scope_keeps_package_tests(self) -> None:
        changed = [Path("codex-rs/shell-command/tests/smoke.rs")]
        with self.patch_package_lookup():
            scope = self.v.build_scope(
                "shell-tests", "changed", changed, changed, self.graph, []
            )
            plan = self.v.plan_commands(args(fast=True, plan=False), scope, [])

        owner_tests = [
            command for command in plan.commands if command.kind == "owner_test"
        ]
        self.assertEqual(len(owner_tests), 1)
        self.assertEqual(
            owner_tests[0].args, ("just", "test-fast", "-p", "codex-shell-command")
        )

    def test_multiple_dirty_groups_need_scope(self) -> None:
        dirty = [Path("codex-rs/core/src/lib.rs"), Path("scripts/foo.py")]
        with (
            self.patch_package_lookup(),
            mock.patch.object(self.v, "dirty_files", return_value=dirty),
            mock.patch.object(self.v, "staged_files", return_value=[]),
        ):
            scope, error = self.v.select_scope(args(), self.graph, [])

        self.assertEqual(error, "multiple dirty groups detected")
        self.assertIsNotNone(scope)
        self.assertEqual(scope.source, "dirty-groups")

    def test_staged_scope_beats_dirty_tree(self) -> None:
        staged = [Path("codex-rs/core/src/lib.rs")]
        dirty = [*staged, Path("scripts/foo.py")]
        with (
            self.patch_package_lookup(),
            mock.patch.object(self.v, "dirty_files", return_value=dirty),
            mock.patch.object(self.v, "staged_files", return_value=staged),
        ):
            scope, error = self.v.select_scope(args(), self.graph, [])

        self.assertIsNone(error)
        self.assertEqual(scope.source, "staged")
        self.assertEqual(scope.owned_packages, ["codex-core"])

    def test_explicit_all_dirty_beats_staged_default(self) -> None:
        staged = [Path("codex-rs/core/src/lib.rs")]
        dirty = [*staged, Path("scripts/foo.py")]
        with (
            self.patch_package_lookup(),
            mock.patch.object(self.v, "dirty_files", return_value=dirty),
            mock.patch.object(self.v, "staged_files", return_value=staged),
        ):
            scope, error = self.v.select_scope(args(all_dirty=True), self.graph, [])

        self.assertIsNone(error)
        # An explicit --all-dirty must not silently shrink to the staged set.
        self.assertEqual(scope.source, "all-dirty")

    def test_parse_last_json_value_skips_noisy_prefix(self) -> None:
        parsed = self.v.parse_last_json_value(
            'warning: ignored setting\n{"workspace_members": ["a"]}\n'
        )

        self.assertEqual(parsed, {"workspace_members": ["a"]})

    def test_parse_last_json_value_uses_last_json_line(self) -> None:
        parsed = self.v.parse_last_json_value(
            '{"older": true}\nprogress: done\n{"newer": true}\n'
        )

        self.assertEqual(parsed, {"newer": True})

    def test_plan_json_contract_is_versioned_and_mode_explicit(self) -> None:
        scope = self.v.Scope(
            scope_id="fixture",
            source="changed",
            active_files=[Path("kd4_features.toml")],
            owned_packages=[],
        )
        plan = self.v.Plan("plan", scope, [], [])

        payload = self.v.plan_to_json(plan, "PLANNED", [], [])

        self.assertEqual(payload["schema_version"], 1)
        self.assertEqual(payload["producer"], "kd4.verify_local")
        self.assertEqual(payload["mode"], "plan")
        self.assertEqual(payload["verdict"], "PLANNED")

    def test_error_json_uses_the_same_versioned_contract(self) -> None:
        payload = self.v.error_to_json(self.v.TOOLING_ERROR, "broken")

        self.assertEqual(
            payload,
            {
                "schema_version": 1,
                "producer": "kd4.verify_local",
                "verdict": self.v.TOOLING_ERROR,
                "error": "broken",
            },
        )

    def test_scope_add_includes_new_path(self) -> None:
        state = {"scope_id": "s", "owned_paths": ["codex-rs/core/src/lib.rs"]}
        with (
            self.patch_package_lookup(),
            mock.patch.object(self.v, "scope_state", return_value=state),
            mock.patch.object(self.v, "dirty_files", return_value=[]),
            mock.patch.object(self.v, "atomic_write_json") as write,
        ):
            scope, error = self.v.select_scope(
                args(scope_add=["codex-rs/core/src/new_tests.rs"]), self.graph, []
            )

        self.assertIsNone(error)
        self.assertIn(Path("codex-rs/core/src/new_tests.rs"), scope.active_files)
        write.assert_called_once()

    def test_stale_scope_refuses_fallback(self) -> None:
        state = {
            "scope_id": "s",
            "owned_paths": ["codex-rs/core/src/lib.rs"],
            "owned_packages": ["codex-core"],
        }
        with (
            self.patch_package_lookup(),
            mock.patch.object(self.v, "scope_state", return_value=state),
            mock.patch.object(
                self.v,
                "scope_stale_reasons",
                return_value=["none of the scoped files are currently dirty or staged"],
            ),
            mock.patch.object(self.v, "dirty_files", return_value=[]),
        ):
            scope, error = self.v.select_scope(args(scope="current"), self.graph, [])

        self.assertEqual(error, "sticky scope appears stale")
        self.assertEqual(
            scope.stale_reasons,
            ["none of the scoped files are currently dirty or staged"],
        )

    def test_transitive_workspace_dependency_included_for_hash_roots(self) -> None:
        roots = {p.name: p.root for p in [self.core, self.shell, self.util]}
        selected = set(self.v.selected_hash_roots(["codex-core"], self.graph))
        self.assertEqual(selected, set(roots.values()))

    def test_proof_inputs_include_all_verify_local_modules(self) -> None:
        proof_inputs = set(self.v.proof_input_files())

        self.assertTrue(
            {
                REPO_ROOT / "scripts" / "verify_local.py",
                REPO_ROOT / "scripts" / "verify_local_context.py",
                REPO_ROOT / "scripts" / "verify_local_execution.py",
            }.issubset(proof_inputs)
        )

    def test_non_owner_cache_key_uses_scoped_file_hash(self) -> None:
        scope = self.v.Scope(
            scope_id="script-scope",
            source="changed",
            active_files=[Path("scripts/verify_local.py")],
            owned_packages=[],
        )
        command = self.v.CommandSpec(
            id="hygiene:diff-check",
            kind="hygiene",
            args=("git", "diff", "--check"),
        )
        with mock.patch.object(
            self.v, "scoped_file_hash", return_value="scoped-hash"
        ) as scoped_hash:
            _key, payload = self.v.cache_key(command, scope, self.graph)

        scoped_hash.assert_called_once_with(scope.active_files)
        self.assertEqual(payload["input_hash"], "scoped-hash")
        self.assertEqual(payload["scope_id"], "script-scope")

    def test_surface_cache_key_combines_owner_and_hash_path_inputs(self) -> None:
        scope = self.v.Scope(
            scope_id="schema-scope",
            source="changed",
            active_files=[Path("codex-rs/app-server-protocol/src/protocol.rs")],
            owned_packages=[],
        )
        command = self.v.CommandSpec(
            id="surface:app-server-schema:validate",
            kind="surface_validation",
            args=("just", "app-server-schema-check"),
            owner_packages=("codex-app-server", "codex-app-server-protocol"),
            hash_paths=("codex-rs/app-server-protocol/src", "codex-rs/protocol/src"),
        )
        with (
            mock.patch.object(
                self.v, "working_tree_hash", return_value="owner-hash"
            ) as working_hash,
            mock.patch.object(
                self.v, "surface_paths_hash", return_value="surface-hash"
            ) as surface_hash,
        ):
            _key, payload = self.v.cache_key(command, scope, self.graph)

        working_hash.assert_called_once_with(command.owner_packages, self.graph)
        surface_hash.assert_called_once_with(command.hash_paths, scope.active_files)
        self.assertNotEqual(payload["input_hash"], "owner-hash")
        self.assertNotEqual(payload["input_hash"], "surface-hash")
        self.assertEqual(payload["scope_id"], "schema-scope")

    def test_surface_paths_hash_covers_inputs_outside_active_scope(self) -> None:
        listed = [Path("codex-rs/protocol/src/lib.rs")]
        active = [Path("codex-rs/app-server-protocol/src/protocol.rs")]
        with (
            mock.patch.object(
                self.v, "git_list_selected_files", return_value=listed
            ) as list_files,
            mock.patch.object(
                self.v, "scoped_file_hash", return_value="combined-hash"
            ) as scoped_hash,
        ):
            result = self.v.surface_paths_hash(("codex-rs/protocol/src",), active)

        self.assertEqual(result, "combined-hash")
        list_files.assert_called_once_with([self.v.REPO_ROOT / "codex-rs/protocol/src"])
        hashed = scoped_hash.call_args.args[0]
        self.assertEqual(
            sorted(hashed, key=lambda p: p.as_posix()),
            sorted(set(listed) | set(active), key=lambda p: p.as_posix()),
        )

    def test_owner_cache_key_omits_scope_identity(self) -> None:
        scope = self.v.Scope(
            scope_id="core-source",
            source="changed",
            active_files=[Path("codex-rs/core/src/lib.rs")],
            owned_packages=["codex-core"],
        )
        command = self.v.CommandSpec(
            id="owner-test:codex-core",
            kind="owner_test",
            args=("just", "test-fast", "-p", "codex-core"),
            owner_packages=("codex-core",),
        )
        with mock.patch.object(
            self.v, "working_tree_hash", return_value="owner-hash"
        ) as working_hash:
            _key, payload = self.v.cache_key(command, scope, self.graph)

        working_hash.assert_called_once_with(command.owner_packages, self.graph)
        self.assertNotIn("scope_id", payload)
        self.assertEqual(payload["input_hash"], "owner-hash")

    def test_hygiene_command_keeps_deleted_scope_paths(self) -> None:
        scope = self.v.Scope(
            scope_id="deleted-scope",
            source="changed",
            active_files=[Path("scripts/deleted.py")],
            owned_packages=[],
        )

        command = self.v.hygiene_command(scope)

        self.assertIsNotNone(command)
        # Diff against HEAD so staged-only changes are hygiene-checked too.
        self.assertEqual(
            command.args, ("git", "diff", "--check", "HEAD", "--", "scripts/deleted.py")
        )

    def test_script_scope_adds_focused_script_validation(self) -> None:
        changed = [Path("scripts/verify_local.py")]
        scope = self.v.build_scope(
            "script", "changed", changed, changed, self.graph, []
        )

        plan = self.v.plan_commands(args(fast=True, plan=False), scope, [])

        commands = {command.kind: command.args for command in plan.commands}
        self.assertEqual(
            commands["script_lint"],
            (
                self.v.python_executable(),
                "-m",
                "scripts.root_maintenance",
                "lint-python",
                "--changed",
                "scripts/verify_local.py",
            ),
        )
        self.assertEqual(
            commands["script_test"],
            (
                self.v.python_executable(),
                "-m",
                "scripts.root_maintenance",
                "test-python",
                "--changed",
                "scripts/verify_local.py",
            ),
        )

    def test_script_scope_adds_powershell_and_shell_syntax_validation(self) -> None:
        changed = [
            Path("scripts/publish-local-codex.ps1"),
            Path("scripts/start-codex-exec.sh"),
        ]
        scope = self.v.build_scope(
            "script-syntax", "changed", changed, changed, self.graph, []
        )

        plan = self.v.plan_commands(args(fast=True, plan=False), scope, [])

        commands = {command.id: command.args for command in plan.commands}
        self.assertEqual(
            commands["script-syntax:powershell:scripts-publish-local-codex-ps1"],
            (
                "pwsh",
                "-NoProfile",
                "-Command",
                self.v.POWERSHELL_PARSE_SCRIPT,
                "scripts/publish-local-codex.ps1",
            ),
        )
        self.assertEqual(
            commands["script-syntax:shell:scripts-start-codex-exec-sh"],
            (
                "bash",
                "-lc",
                self.v.bash_parse_script(Path("scripts/start-codex-exec.sh")),
            ),
        )
        self.assertEqual(
            commands["script-test:publish-local-codex+start-codex-exec"],
            (
                self.v.python_executable(),
                "-m",
                "scripts.root_maintenance",
                "test-python",
                "--changed",
                "scripts/publish-local-codex.ps1",
                "--changed",
                "scripts/start-codex-exec.sh",
            ),
        )

    def test_verify_local_rules_change_runs_router_tests(self) -> None:
        changed = [Path("scripts/verify_local_rules.toml")]
        scope = self.v.build_scope("rules", "changed", changed, changed, self.graph, [])

        plan = self.v.plan_commands(args(fast=True, plan=False), scope, [])

        commands = {command.kind: command.args for command in plan.commands}
        self.assertEqual(
            commands["script_test"],
            (
                self.v.python_executable(),
                "-m",
                "scripts.root_maintenance",
                "test-python",
                "--module",
                "scripts.test_verify_local",
            ),
        )

    def test_justfile_change_runs_router_tests(self) -> None:
        changed = [Path("justfile")]
        scope = self.v.build_scope(
            "justfile", "changed", changed, changed, self.graph, []
        )

        plan = self.v.plan_commands(args(fast=True, plan=False), scope, [])

        commands = {command.id: command.args for command in plan.commands}
        self.assertEqual(
            commands["script-test:verify_local_controls"],
            (
                self.v.python_executable(),
                "-m",
                "scripts.root_maintenance",
                "test-python",
                "--module",
                "scripts.test_verify_local",
            ),
        )
        self.assertEqual(commands["justfile:summary"], ("just", "--summary"))

    def test_final_script_scope_uses_python_formatter_not_rust_formatter(self) -> None:
        changed = [Path("scripts/verify_local.py")]
        scope = self.v.build_scope(
            "script", "changed", changed, changed, self.graph, []
        )

        plan = self.v.plan_commands(args(plan=False, final=True), scope, [])

        commands = {command.id: command.args for command in plan.commands}
        self.assertNotIn("formatter:fmt-check-fast", commands)
        self.assertEqual(
            commands["formatter:format-python"],
            (
                self.v.python_executable(),
                "-m",
                "scripts.root_maintenance",
                "format-python",
                "--changed",
                "scripts/verify_local.py",
            ),
        )

    def test_top_level_typescript_files_require_prettier(self) -> None:
        self.assertTrue(
            self.v.needs_prettier_formatter(Path("sdk/typescript/index.ts"))
        )
        self.assertTrue(self.v.needs_prettier_formatter(Path("codex-cli/index.js")))

    def test_main_preserves_explicit_empty_argv(self) -> None:
        with (
            mock.patch.object(
                self.v, "parse_args", side_effect=RuntimeError("stop after parse")
            ) as parse_args,
            self.assertRaisesRegex(RuntimeError, "stop after parse"),
        ):
            self.v.main([])

        parse_args.assert_called_once_with([])

    def test_root_maintenance_commands_resolve_bare_python_executable(self) -> None:
        with (
            mock.patch.object(self.v.sys, "executable", "python"),
            mock.patch.object(
                self.v,
                "which",
                return_value=r"C:\Python313\python.exe",
            ),
        ):
            self.assertEqual(
                self.v.root_maintenance_args("lint-python"),
                (
                    r"C:\Python313\python.exe",
                    "-m",
                    "scripts.root_maintenance",
                    "lint-python",
                ),
            )

    def test_baseline_ref_uses_head_for_dirty_scope(self) -> None:
        scope = self.v.Scope(
            scope_id="dirty-scope",
            source="changed",
            active_files=[Path("scripts/verify_local.py")],
            owned_packages=[],
        )
        with (
            mock.patch.object(
                self.v, "dirty_files", return_value=[Path("scripts/verify_local.py")]
            ),
            mock.patch.object(self.v, "current_head", return_value="HEADSHA"),
        ):
            self.assertEqual(self.v.baseline_ref_for_scope(scope), "HEADSHA")

    def test_baseline_ref_uses_parent_for_clean_committed_scope(self) -> None:
        scope = self.v.Scope(
            scope_id="clean-scope",
            source="changed",
            active_files=[Path("scripts/verify_local.py")],
            owned_packages=[],
        )
        with (
            mock.patch.object(self.v, "dirty_files", return_value=[]),
            mock.patch.object(self.v, "git", return_value="PARENTSHA\n"),
        ):
            self.assertEqual(self.v.baseline_ref_for_scope(scope), "PARENTSHA")

    def test_normalize_absolute_and_file_url_paths(self) -> None:
        expected = Path("scripts/verify_local.py")
        absolute = REPO_ROOT / expected

        self.assertEqual(self.v.normalize_path(str(absolute)), expected)
        self.assertEqual(self.v.normalize_path(absolute.as_uri()), expected)

    def test_related_flag_adds_adjacent_compile_check_only(self) -> None:
        changed = [Path("codex-rs/shell-command/src/lib.rs")]
        with self.patch_package_lookup():
            scope = self.v.build_scope(
                "shell", "changed", changed, changed, self.graph, []
            )
            plan = self.v.plan_commands(
                args(fast=True, plan=False, related=True), scope, []
            )

        adjacent = [
            command
            for command in plan.commands
            if command.id.startswith("adjacent-check:")
        ]
        self.assertEqual(
            [command.owner_packages for command in adjacent], [("codex-core",)]
        )
        self.assertTrue(
            all(command.args[:2] == ("just", "check-lane") for command in adjacent)
        )

    def test_related_tests_flag_adds_adjacent_test_lane(self) -> None:
        changed = [Path("codex-rs/shell-command/src/lib.rs")]
        with self.patch_package_lookup():
            scope = self.v.build_scope(
                "shell", "changed", changed, changed, self.graph, []
            )
            plan = self.v.plan_commands(
                args(fast=True, plan=False, related_tests=True), scope, []
            )

        adjacent_tests = [
            command
            for command in plan.commands
            if command.id.startswith("related-test:")
        ]
        self.assertEqual(
            [command.owner_packages for command in adjacent_tests], [("codex-core",)]
        )
        self.assertEqual(
            adjacent_tests[0].args, ("just", "test-lane-package", "codex-core")
        )
        skipped_text = "\n".join(item["reason"] for item in plan.skipped)
        self.assertNotIn("blocked unless --related-tests", skipped_text)

    def test_multiple_owner_packages_are_independent_commands(self) -> None:
        changed = [
            Path("codex-rs/core/src/lib.rs"),
            Path("codex-rs/shell-command/src/lib.rs"),
        ]
        with self.patch_package_lookup():
            scope = self.v.build_scope(
                "multi-owner", "changed", changed, changed, self.graph, []
            )
            plan = self.v.plan_commands(args(fast=False, plan=True), scope, [])

        owner_tests = [
            command for command in plan.commands if command.kind == "owner_test"
        ]
        self.assertEqual(
            [command.args for command in owner_tests],
            [
                ("just", "test-fast", "-p", "codex-core"),
                ("just", "test-fast", "-p", "codex-shell-command"),
            ],
        )

    def test_retry_flakes_is_opt_in(self) -> None:
        result = self.v.CommandResult(
            self.v.CommandSpec(
                id="owner-test:codex-core",
                kind="owner_test",
                args=("just", "test-fast", "-p", "codex-core"),
            ),
            self.v.FAILED,
            1,
            1.0,
            Path("missing.log"),
            "test failed",
        )

        self.assertFalse(self.v.should_retry_for_flake(result, args()))

    def _owner_test_result_with_log(self, log_text: str) -> object:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        log_path = Path(tmp.name) / "owner-test.log"
        log_path.write_text(log_text, encoding="utf-8")
        return self.v.CommandResult(
            self.v.CommandSpec(
                id="owner-test:codex-core",
                kind="owner_test",
                args=("just", "test-fast", "-p", "codex-core"),
            ),
            self.v.FAILED,
            1,
            1.0,
            log_path,
            "test failed",
        )

    def test_compile_error_is_not_retried_as_flake(self) -> None:
        # The command echo contains "test", which must not count as having
        # reached test execution.
        result = self._owner_test_result_with_log(
            "$ just test-fast -p codex-core\n\n"
            "error[E0308]: mismatched types\n"
            " --> core/src/lib.rs:1:1\n"
        )

        self.assertFalse(self.v.reached_test_execution(result))
        self.assertFalse(self.v.should_retry_for_flake(result, args(retry_flakes=True)))

    def test_real_test_failure_is_retryable_as_flake(self) -> None:
        result = self._owner_test_result_with_log(
            "$ just test-fast -p codex-core\n\n"
            "        FAIL [   0.335s] codex-core suite::flaky_case\n"
            "    Summary [   1.201s] 12 tests run: 11 passed, 1 failed\n"
        )

        self.assertTrue(self.v.reached_test_execution(result))
        self.assertTrue(self.v.should_retry_for_flake(result, args(retry_flakes=True)))

    def test_surface_validation_command_is_planned_for_matching_rule(self) -> None:
        changed = [Path("codex-rs/app-server-protocol/src/protocol.rs")]
        with self.patch_package_lookup():
            scope = self.v.build_scope(
                "schema", "changed", changed, changed, self.graph, [self.schema_rule]
            )
            plan = self.v.plan_commands(
                args(fast=True, plan=False), scope, [self.schema_rule]
            )

        self.assertEqual(scope.surface_rules, ["app-server-schema"])
        surface_commands = [
            command for command in plan.commands if command.kind == "surface_validation"
        ]
        self.assertEqual(surface_commands[0].args, ("just", "app-server-schema-check"))
        self.assertEqual(
            surface_commands[0].owner_packages, self.schema_rule.owned_packages
        )
        self.assertEqual(surface_commands[0].hash_paths, self.schema_rule.paths)
        self.assertFalse(
            [command for command in plan.commands if command.kind == "owner_test"]
        )
        skipped_text = "\n".join(item["reason"] for item in plan.skipped)
        self.assertIn("surface rule owns focused validation", skipped_text)

    def test_app_server_thread_scope_uses_thread_status_recipe(self) -> None:
        rule = self.v.SurfaceRule(
            id="app-server-thread-status",
            paths=("codex-rs/app-server/src/request_processors/thread_processor.rs",),
            owned_packages=("codex-app-server",),
            validation_command=("just", "app-server-thread-status-check"),
            skip_owner_tests=True,
        )
        changed = [
            Path("codex-rs/app-server/src/request_processors/thread_processor.rs")
        ]
        with self.patch_package_lookup():
            scope = self.v.build_scope(
                "thread", "changed", changed, changed, self.graph, [rule]
            )
            plan = self.v.plan_commands(args(fast=True, plan=False), scope, [rule])

        surface_commands = [
            command for command in plan.commands if command.kind == "surface_validation"
        ]
        self.assertEqual(
            surface_commands[0].args, ("just", "app-server-thread-status-check")
        )

    def test_regen_uses_surface_regeneration_command(self) -> None:
        changed = [Path("codex-rs/app-server-protocol/src/protocol.rs")]
        with self.patch_package_lookup():
            scope = self.v.build_scope(
                "schema", "changed", changed, changed, self.graph, [self.schema_rule]
            )
            plan = self.v.plan_commands(
                args(fast=True, plan=False, regen=True), scope, [self.schema_rule]
            )

        surface_commands = [
            command for command in plan.commands if command.kind == "surface_regen"
        ]
        self.assertEqual(
            surface_commands[0].args,
            ("just", "app-server-schema-check-force"),
        )

    def test_config_schema_rule_routes_generated_fixture_to_schema_check(self) -> None:
        rule = self.v.SurfaceRule(
            id="config-schema",
            paths=("codex-rs/core/config.schema.json",),
            owned_packages=("codex-core", "codex-config"),
            validation_command=("just", "config-schema-check"),
            regen_command=("just", "config-schema-check-force"),
            skip_owner_tests=True,
        )
        changed = [Path("codex-rs/core/config.schema.json")]
        with self.patch_package_lookup():
            scope = self.v.build_scope(
                "config-schema", "changed", changed, changed, self.graph, [rule]
            )
            plan = self.v.plan_commands(args(fast=True, plan=False), scope, [rule])

        self.assertEqual(scope.surface_rules, ["config-schema"])
        self.assertEqual(
            [
                command.args
                for command in plan.commands
                if command.kind == "surface_validation"
            ],
            [("just", "config-schema-check")],
        )

    def test_rules_file_contains_requested_local_workflow_surfaces(self) -> None:
        loaded = {rule.id: rule for rule in self.v.load_rules()}
        for surface_id in [
            "config-schema",
            "vscode-runtime-proof",
            "tui-large-widget-risk",
            "source-map",
            "dependency-cleanup",
            "sdk-typescript",
            "sdk-python",
            "codex-cli-wrapper",
        ]:
            self.assertIn(surface_id, loaded)
            self.assertIsNotNone(loaded[surface_id].validation_command)
        self.assertIn("SOURCEMAP.md", loaded["source-map"].paths)

    def test_rules_file_schema_surfaces_mirror_checker_inputs(self) -> None:
        loaded = {rule.id: rule for rule in self.v.load_rules()}
        self.assertIn("codex-rs/protocol/src", loaded["app-server-schema"].hash_paths)
        self.assertIn("codex-rs/features/src", loaded["config-schema"].hash_paths)


if __name__ == "__main__":
    unittest.main()
