#!/usr/bin/env python3

import io
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import tool_versions
from scripts.build_tooling_test_support import REPO_ROOT
from scripts.build_tooling_test_support import load_format_module
from scripts.build_tooling_test_support import load_just_shell_module
from scripts.build_tooling_test_support import load_toml


class BuildToolingEnvironmentTest(unittest.TestCase):
    def test_shared_support_loads_hyphenated_repo_script(self) -> None:
        just_shell = load_just_shell_module()

        self.assertEqual(
            Path(just_shell.__file__).resolve(),
            REPO_ROOT / "scripts" / "just-shell.py",
        )

    def test_just_shell_limits_rust_setup_to_rust_commands(self) -> None:
        just_shell = load_just_shell_module()

        self.assertFalse(just_shell.command_needs_rust_tooling("pnpm lint:markdown"))
        self.assertFalse(just_shell.command_needs_rust_tooling("python tool.py"))
        self.assertTrue(
            just_shell.command_needs_rust_tooling("cargo test -p codex-core")
        )
        self.assertTrue(
            just_shell.command_needs_rust_tooling(
                "python rust_build_status.py run-lane"
            )
        )

    def test_local_just_shell_sets_sccache_without_probe(self) -> None:
        just_shell = load_just_shell_module()

        updates = just_shell.rust_tool_env(
            {"PATH": "ignored"},
            which=lambda program: f"/tools/{program}" if program == "sccache" else None,
            repo_root=REPO_ROOT,
        )

        self.assertEqual(updates["RUSTC_WRAPPER"], "/tools/sccache")
        self.assertEqual(updates["SCCACHE_BASEDIR"], str(REPO_ROOT))
        self.assertEqual(updates["SCCACHE_CACHE_SIZE"], "80G")

    def test_local_just_shell_sets_sccache_env_for_existing_sccache_wrapper(
        self,
    ) -> None:
        just_shell = load_just_shell_module()

        updates = just_shell.rust_tool_env(
            {"RUSTC_WRAPPER": "sccache"},
            which=lambda program: None,
            repo_root=REPO_ROOT,
        )

        self.assertNotIn("RUSTC_WRAPPER", updates)
        self.assertEqual(updates["SCCACHE_BASEDIR"], str(REPO_ROOT))
        self.assertEqual(updates["SCCACHE_CACHE_SIZE"], "80G")

    def test_local_just_shell_honors_sccache_cache_size_override(self) -> None:
        just_shell = load_just_shell_module()

        def which(program: str) -> str | None:
            return f"/tools/{program}" if program == "sccache" else None

        updates = just_shell.rust_tool_env(
            {"CODEX_SCCACHE_CACHE_SIZE": "100G"},
            which=which,
            repo_root=REPO_ROOT,
        )
        self.assertEqual(updates["SCCACHE_CACHE_SIZE"], "100G")

        blank_updates = just_shell.rust_tool_env(
            {"CODEX_SCCACHE_CACHE_SIZE": "   "},
            which=which,
            repo_root=REPO_ROOT,
        )
        self.assertEqual(blank_updates["SCCACHE_CACHE_SIZE"], "80G")

    def test_local_just_shell_keeps_existing_rust_wrapper(self) -> None:
        just_shell = load_just_shell_module()

        updates = just_shell.rust_tool_env(
            {"RUSTC_WRAPPER": "existing-wrapper"},
            which=lambda program: f"/tools/{program}" if program == "sccache" else None,
        )

        self.assertNotIn("RUSTC_WRAPPER", updates)
        self.assertNotIn("SCCACHE_BASEDIR", updates)
        self.assertNotIn("SCCACHE_CACHE_SIZE", updates)

    def test_local_just_shell_restarts_stale_sccache_server_cache_size(self) -> None:
        just_shell = load_just_shell_module()
        calls = []
        stats_calls = 0

        def fake_run(command, **_kwargs):
            nonlocal stats_calls
            calls.append(command)
            stdout = ""
            if command[1:] == ["--show-stats"]:
                stats_calls += 1
                size = "10 GiB" if stats_calls == 1 else "80 GiB"
                stdout = f"Max cache size                       {size}\n"
            return subprocess.CompletedProcess(command, 0, stdout=stdout, stderr="")

        restarted = just_shell.ensure_sccache_server_env(
            {
                "RUSTC_WRAPPER": "/tools/sccache",
                "SCCACHE_CACHE_SIZE": "80G",
            },
            which=lambda program: f"/tools/{program}" if program == "sccache" else None,
            run=fake_run,
        )

        self.assertTrue(restarted)
        self.assertEqual(
            calls,
            [
                ["/tools/sccache", "--show-stats"],
                ["/tools/sccache", "--stop-server"],
                ["/tools/sccache", "--start-server"],
                ["/tools/sccache", "--show-stats"],
            ],
        )

    def test_local_just_shell_compares_sccache_sizes_by_bytes(self) -> None:
        just_shell = load_just_shell_module()

        expected_sizes = {
            "512M": "512 MiB",
            "1T": "1 TiB",
            "80GB": "80 GiB",
            "80GiB": "80 GiB",
            "1024G": "1 TiB",
        }
        for configured, reported in expected_sizes.items():
            with self.subTest(configured=configured):
                calls = []

                def fake_run(command, **_kwargs):
                    calls.append(command)
                    return subprocess.CompletedProcess(
                        command,
                        0,
                        stdout=f"Max cache size                       {reported}\n",
                        stderr="",
                    )

                restarted = just_shell.ensure_sccache_server_env(
                    {
                        "RUSTC_WRAPPER": "/tools/sccache",
                        "SCCACHE_CACHE_SIZE": configured,
                    },
                    which=lambda program: (
                        f"/tools/{program}" if program == "sccache" else None
                    ),
                    run=fake_run,
                )

                self.assertFalse(restarted)
                self.assertEqual(calls, [["/tools/sccache", "--show-stats"]])

    def test_local_just_shell_does_not_cache_failed_sccache_restart(self) -> None:
        just_shell = load_just_shell_module()
        calls = []

        def fake_run(command, **_kwargs):
            calls.append(command)
            if command[1:] == ["--show-stats"]:
                return subprocess.CompletedProcess(
                    command,
                    0,
                    stdout="Max cache size                       10 GiB\n",
                    stderr="",
                )
            return subprocess.CompletedProcess(
                command,
                9 if command[1:] == ["--start-server"] else 0,
                stdout="",
                stderr="",
            )

        with tempfile.TemporaryDirectory() as tmp:
            restarted = just_shell.ensure_sccache_server_env(
                {
                    "RUSTC_WRAPPER": "/tools/sccache",
                    "SCCACHE_CACHE_SIZE": "80G",
                },
                which=lambda program: (
                    f"/tools/{program}" if program == "sccache" else None
                ),
                run=fake_run,
                cache_dir=Path(tmp),
            )

        self.assertFalse(restarted)
        self.assertEqual(calls[-1], ["/tools/sccache", "--start-server"])

    def test_local_just_shell_caches_matching_sccache_server_cache_size(self) -> None:
        just_shell = load_just_shell_module()
        calls = []

        def fake_run(command, **_kwargs):
            calls.append(command)
            return subprocess.CompletedProcess(
                command,
                0,
                stdout="Max cache size                       80 GiB\n",
                stderr="",
            )

        with tempfile.TemporaryDirectory() as tmp:
            env = {
                "RUSTC_WRAPPER": "/tools/sccache",
                "SCCACHE_CACHE_SIZE": "80G",
            }
            first = just_shell.ensure_sccache_server_env(
                env,
                which=lambda program: (
                    f"/tools/{program}" if program == "sccache" else None
                ),
                run=fake_run,
                cache_dir=Path(tmp),
            )
            second = just_shell.ensure_sccache_server_env(
                env,
                which=lambda program: (
                    f"/tools/{program}" if program == "sccache" else None
                ),
                run=fake_run,
                cache_dir=Path(tmp),
            )

        self.assertFalse(first)
        self.assertFalse(second)
        self.assertEqual(calls, [["/tools/sccache", "--show-stats"]])

    def test_ci_does_not_override_rust_wrapper_or_linker(self) -> None:
        just_shell = load_just_shell_module()

        updates = just_shell.rust_tool_env(
            {"CI": "true"},
            which=lambda program: f"C:/tools/{program}.exe",
        )

        self.assertEqual(updates, {})

    def test_windows_local_just_shell_uses_lld_link_when_available(self) -> None:
        just_shell = load_just_shell_module()

        updates = just_shell.rust_tool_env(
            {},
            which=lambda program: (
                "C:/LLVM/bin/lld-link.exe" if program == "lld-link" else None
            ),
        )

        self.assertEqual(updates["CARGO_NET_GIT_FETCH_WITH_CLI"], "true")
        self.assertEqual(
            updates["CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"],
            "C:/LLVM/bin/lld-link.exe",
        )
        self.assertEqual(
            updates["CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER"],
            "C:/LLVM/bin/lld-link.exe",
        )

    def test_windows_local_just_shell_uses_scoop_lld_link_when_not_on_path(
        self,
    ) -> None:
        just_shell = load_just_shell_module()

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            lld_link = root / "apps" / "llvm" / "current" / "bin" / "lld-link.exe"
            lld_link.parent.mkdir(parents=True)
            lld_link.write_text("", encoding="utf-8")

            updates = just_shell.rust_tool_env(
                {"SCOOP": str(root)},
                which=lambda _program: None,
            )

        self.assertEqual(
            updates["CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"],
            str(lld_link),
        )

    def test_windows_local_just_shell_keeps_existing_msvc_linker(self) -> None:
        just_shell = load_just_shell_module()

        updates = just_shell.rust_tool_env(
            {"CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER": "link.exe"},
            which=lambda program: (
                "C:/LLVM/bin/lld-link.exe" if program == "lld-link" else None
            ),
        )

        self.assertEqual(updates["CARGO_NET_GIT_FETCH_WITH_CLI"], "true")
        self.assertNotIn("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER", updates)
        self.assertEqual(
            updates["CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER"],
            "C:/LLVM/bin/lld-link.exe",
        )

    def test_local_just_shell_prefers_scripts_venv_python_tools(self) -> None:
        just_shell = load_just_shell_module()

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            scripts_bin = root / "scripts" / ".venv" / "Scripts"
            scripts_bin.mkdir(parents=True)
            (scripts_bin / "python.exe").write_text("", encoding="utf-8")

            updates = just_shell.python_tool_env(
                {"PATH": "C:/Windows/System32"},
                repo_root=root,
            )

        self.assertEqual(updates["VIRTUAL_ENV"], str(root / "scripts" / ".venv"))
        self.assertEqual(updates["VIRTUAL_ENV_DISABLE_PROMPT"], "1")
        self.assertEqual(
            updates["PATH"],
            f"{scripts_bin}{just_shell.os.pathsep}C:/Windows/System32",
        )

    def test_local_just_shell_limits_python_to_thirty_logical_cpus(self) -> None:
        just_shell = load_just_shell_module()

        with mock.patch.object(just_shell.os, "cpu_count", return_value=8):
            self.assertEqual(
                just_shell.python_cpu_env({}),
                {"PYTHON_CPU_COUNT": "8"},
            )
        with mock.patch.object(just_shell.os, "cpu_count", return_value=64):
            self.assertEqual(
                just_shell.python_cpu_env({}),
                {"PYTHON_CPU_COUNT": "30"},
            )
        with mock.patch.object(just_shell.os, "cpu_count", return_value=None):
            self.assertEqual(
                just_shell.python_cpu_env({}),
                {"PYTHON_CPU_COUNT": "1"},
            )
        self.assertEqual(
            just_shell.python_cpu_env({"PYTHON_CPU_COUNT": "8"}),
            {},
        )
        self.assertEqual(just_shell.python_cpu_env({"CI": "true"}), {})

    def test_local_just_shell_keeps_existing_virtualenv(self) -> None:
        just_shell = load_just_shell_module()

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            scripts_bin = root / "scripts" / ".venv" / "Scripts"
            scripts_bin.mkdir(parents=True)
            (scripts_bin / "python.exe").write_text("", encoding="utf-8")

            updates = just_shell.python_tool_env(
                {
                    "PATH": "C:/Windows/System32",
                    "VIRTUAL_ENV": "C:/other/.venv",
                },
                repo_root=root,
            )

        self.assertEqual(updates, {})

    def test_local_just_shell_deduplicates_scripts_venv_path(self) -> None:
        just_shell = load_just_shell_module()

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            scripts_bin = root / "scripts" / ".venv" / "Scripts"
            scripts_bin.mkdir(parents=True)
            (scripts_bin / "python.exe").write_text("", encoding="utf-8")
            existing_path = f"{scripts_bin}{just_shell.os.pathsep}C:/Windows/System32"

            updates = just_shell.python_tool_env(
                {"PATH": existing_path},
                repo_root=root,
            )

        self.assertEqual(updates["PATH"], existing_path)

    def test_local_just_shell_warns_when_scripts_venv_is_missing(self) -> None:
        just_shell = load_just_shell_module()

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            (root / "scripts").mkdir()
            (root / "scripts" / "uv.lock").write_text("", encoding="utf-8")
            cache_dir = root / "cache"
            stderr = io.StringIO()

            updates = just_shell.python_tool_env(
                {},
                repo_root=root,
                cache_dir=cache_dir,
                stderr=stderr,
            )
            second_stderr = io.StringIO()
            just_shell.python_tool_env(
                {},
                repo_root=root,
                cache_dir=cache_dir,
                stderr=second_stderr,
            )

        self.assertEqual(updates, {})
        self.assertIn("scripts/.venv is missing", stderr.getvalue())
        self.assertEqual(second_stderr.getvalue(), "")

    def test_local_just_shell_rejects_directory_named_like_python(self) -> None:
        just_shell = load_just_shell_module()

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            python_dir = root / "scripts" / ".venv" / "Scripts" / "python.exe"
            python_dir.mkdir(parents=True)

            self.assertEqual(
                just_shell.python_tool_env({}, repo_root=root),
                {},
            )

    def test_local_just_shell_tool_probe_has_timeout(self) -> None:
        just_shell = load_just_shell_module()

        with mock.patch.object(
            just_shell.subprocess,
            "run",
            side_effect=just_shell.subprocess.TimeoutExpired(["tool"], 2),
        ) as run:
            self.assertFalse(just_shell.tool_runs(["tool"], timeout=2))

        self.assertEqual(run.call_args.kwargs["timeout"], 2)

    def test_local_just_shell_tool_probe_uses_persistent_cache(self) -> None:
        just_shell = load_just_shell_module()

        with tempfile.TemporaryDirectory() as temp_dir:
            cache_dir = Path(temp_dir)
            with mock.patch.object(just_shell, "tool_runs", return_value=True) as run:
                self.assertTrue(
                    just_shell.cached_tool_runs(
                        ["tool", "--version"], cache_dir=cache_dir
                    )
                )
            just_shell.TOOL_RUN_RESULTS.clear()
            with mock.patch.object(
                just_shell,
                "tool_runs",
                side_effect=AssertionError("probe should have been cached"),
            ):
                self.assertTrue(
                    just_shell.cached_tool_runs(
                        ["tool", "--version"], cache_dir=cache_dir
                    )
                )

        run.assert_called_once()

    def test_local_just_shell_inconclusive_tool_probe_proceeds_without_cache(
        self,
    ) -> None:
        just_shell = load_just_shell_module()

        with tempfile.TemporaryDirectory() as temp_dir:
            cache_dir = Path(temp_dir)
            command = ["tool", "--version"]
            with mock.patch.object(just_shell, "tool_runs", return_value=None) as run:
                self.assertTrue(
                    just_shell.cached_tool_runs(command, cache_dir=cache_dir)
                )
                self.assertTrue(
                    just_shell.cached_tool_runs(command, cache_dir=cache_dir)
                )

            self.assertFalse(
                just_shell.tool_run_cache_path(command, cache_dir).exists()
            )

        self.assertEqual(run.call_count, 2)

    def test_local_just_shell_writes_probe_cache_by_atomic_replace(self) -> None:
        just_shell = load_just_shell_module()

        with tempfile.TemporaryDirectory() as temp_dir:
            cache_dir = Path(temp_dir)
            command = ["tool", "--version"]
            original_replace = just_shell.os.replace
            with mock.patch.object(
                just_shell.os, "replace", wraps=original_replace
            ) as replace:
                just_shell.write_cached_tool_run(command, cache_dir, True)

            self.assertTrue(just_shell.read_cached_tool_run(command, cache_dir))
            replace.assert_called_once()
            self.assertEqual(list(cache_dir.glob("*.tmp")), [])

    def test_local_just_shell_tool_probe_cache_tracks_tool_identity(self) -> None:
        just_shell = load_just_shell_module()

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            tool = root / "tool.exe"
            tool.write_text("one", encoding="utf-8")
            first = just_shell.tool_run_cache_path([str(tool), "--version"], root)
            tool.write_text("two larger", encoding="utf-8")
            os.utime(tool, None)
            second = just_shell.tool_run_cache_path([str(tool), "--version"], root)

        self.assertNotEqual(first, second)

    def test_local_just_shell_stderr_null_must_be_terminal(self) -> None:
        just_shell = load_just_shell_module()

        self.assertEqual(
            just_shell.render_command(
                "tool {args} {stderr-null}", args="ARGS", stderr_null="ERR"
            ),
            "tool ARGS ERR",
        )
        with self.assertRaisesRegex(ValueError, "final token"):
            just_shell.render_command(
                "tool {stderr-null} | more", args="ARGS", stderr_null="ERR"
            )

    def test_local_just_shell_rejects_old_powershell(self) -> None:
        just_shell = load_just_shell_module()
        stderr = io.StringIO()

        result = just_shell.run_powershell(
            "Write-Output hi",
            "recipe",
            [],
            which=lambda program: "C:/pwsh.exe" if program == "pwsh.exe" else None,
            can_run=lambda command: False,
            stderr=stderr,
        )

        self.assertEqual(result, 1)
        self.assertIn("PowerShell 7.4", stderr.getvalue())

    def test_local_just_shell_reports_powershell_launch_failure(self) -> None:
        just_shell = load_just_shell_module()
        stderr = io.StringIO()

        with mock.patch.object(
            just_shell.subprocess, "run", side_effect=OSError("launch failed")
        ):
            result = just_shell.run_powershell(
                "Write-Output hi",
                "recipe",
                [],
                which=lambda _program: "C:/pwsh.exe",
                can_run=lambda _command: True,
                stderr=stderr,
            )

        self.assertEqual(result, 1)
        self.assertIn("Failed to launch PowerShell", stderr.getvalue())

    def test_rust_workspace_uses_upstream_default_members(self) -> None:
        manifest = load_toml(REPO_ROOT / "codex-rs" / "Cargo.toml")

        workspace = manifest["workspace"]
        removed_crates = {
            "ansi-escape",
            "collaboration-mode-templates",
            "core-api",
            "thread-manager-sample",
            "v8-poc",
        }

        self.assertNotIn("default-members", workspace)
        self.assertTrue(removed_crates.isdisjoint(workspace["members"]))
        for crate in removed_crates:
            self.assertFalse((REPO_ROOT / "codex-rs" / crate).exists())
        self.assertTrue(
            {
                "app-server/tests/common",
                "chatgpt",
                "core/tests/common",
                "mcp-server/tests/common",
                "message-history",
                "windows-sandbox-rs",
            }.issubset(workspace["members"])
        )

    def test_cli_removes_orphaned_wsl_path_normalization(self) -> None:
        main = (REPO_ROOT / "codex-rs" / "cli" / "src" / "main.rs").read_text(
            encoding="utf-8"
        )

        self.assertNotIn("wsl_paths", main)
        self.assertFalse(
            (REPO_ROOT / "codex-rs" / "cli" / "src" / "wsl_paths.rs").exists()
        )

    def test_rollout_state_integration_has_one_implementation_owner(self) -> None:
        rollout_src = REPO_ROOT / "codex-rs" / "rollout" / "src"
        rollout_lib = (rollout_src / "lib.rs").read_text(encoding="utf-8")
        state_docs = (REPO_ROOT / "codex-rs" / "state" / "src" / "lib.rs").read_text(
            encoding="utf-8"
        )

        self.assertTrue((rollout_src / "state_integration.rs").is_file())
        self.assertFalse((rollout_src / "state_db.rs").exists())
        self.assertTrue((rollout_src / "state_integration_tests.rs").is_file())
        self.assertFalse((rollout_src / "state_db_tests.rs").exists())
        self.assertIn("pub mod state_integration;", rollout_lib)
        self.assertNotIn("pub use state_integration as state_db;", rollout_lib)
        self.assertIn("codex-rollout::state_integration", state_docs)

    def test_memories_usage_metric_stays_with_usage_owner(self) -> None:
        memories_read_src = REPO_ROOT / "codex-rs" / "memories" / "read" / "src"
        usage = (memories_read_src / "usage.rs").read_text(encoding="utf-8")
        lib = (memories_read_src / "lib.rs").read_text(encoding="utf-8")

        self.assertFalse((memories_read_src / "metrics.rs").exists())
        self.assertNotIn("mod metrics;", lib)
        self.assertIn(
            'pub const MEMORIES_USAGE_METRIC: &str = "codex.memories.usage";',
            usage,
        )

    def test_windows_program_database_artifacts_are_ignored(self) -> None:
        ignore_rules = (
            (REPO_ROOT / ".gitignore").read_text(encoding="utf-8").splitlines()
        )

        self.assertIn("*.pdb", ignore_rules)

    def test_rust_workspace_excludes_retired_orphan_crates(self) -> None:
        manifest = load_toml(REPO_ROOT / "codex-rs" / "Cargo.toml")
        members = manifest["workspace"]["members"]

        retired_crates = ("execpolicy-legacy", "realtime-webrtc")
        for crate in retired_crates:
            self.assertNotIn(crate, members)
            self.assertFalse((REPO_ROOT / "codex-rs" / crate / "Cargo.toml").exists())

        execpolicy_readme = (
            REPO_ROOT / "codex-rs" / "execpolicy" / "README.md"
        ).read_text()
        self.assertNotIn("codex-execpolicy-legacy", execpolicy_readme)

    def test_rust_workspace_uses_upstream_profiles(self) -> None:
        manifest = load_toml(REPO_ROOT / "codex-rs" / "Cargo.toml")
        profiles = manifest["profile"]

        self.assertEqual(profiles["dev"]["debug"], "limited")
        self.assertEqual(profiles["ci-test"]["debug"], "limited")
        self.assertEqual(profiles["release"]["lto"], "thin")
        self.assertEqual(profiles["release"]["debug"], "line-tables-only")
        self.assertEqual(profiles["release"]["split-debuginfo"], "off")
        self.assertFalse(profiles["release"]["strip"])
        self.assertEqual(profiles["release"]["codegen-units"], 4)
        self.assertNotIn("local-test", profiles)
        self.assertNotIn("release-fast", profiles)

    def test_rust_cargo_config_lets_rustc_discover_msvc_linker(self) -> None:
        config = load_toml(REPO_ROOT / "codex-rs" / ".cargo" / "config.toml")
        targets = config["target"]

        for target_config in targets.values():
            self.assertNotIn("linker", target_config)

    def test_rust_workspace_uses_upstream_dependency_features(self) -> None:
        manifest = load_toml(REPO_ROOT / "codex-rs" / "Cargo.toml")
        workspace_deps = manifest["workspace"]["dependencies"]

        reqwest = workspace_deps["reqwest"]
        self.assertEqual(reqwest["features"], ["cookies"])
        self.assertNotIn("default-features", reqwest)

        sqlx = workspace_deps["sqlx"]
        self.assertFalse(sqlx["default-features"])
        self.assertEqual(
            sorted(sqlx["features"]),
            [
                "chrono",
                "json",
                "macros",
                "migrate",
                "runtime-tokio",
                "sqlite-bundled",
                "time",
                "tls-rustls",
                "uuid",
            ],
        )

        tokio_tungstenite = workspace_deps["tokio-tungstenite"]
        self.assertEqual(
            sorted(tokio_tungstenite["features"]),
            ["proxy", "rustls-tls-native-roots"],
        )

        tungstenite = workspace_deps["tungstenite"]
        self.assertEqual(sorted(tungstenite["features"]), ["deflate", "proxy"])

        codex_api = load_toml(REPO_ROOT / "codex-rs" / "codex-api" / "Cargo.toml")
        self.assertEqual(
            codex_api["dependencies"]["tokio-tungstenite"], {"workspace": True}
        )
        # Extension configuration is imported from tungstenite directly while
        # stream/message types continue to use tokio-tungstenite's re-export.
        self.assertEqual(codex_api["dependencies"]["tungstenite"], {"workspace": True})
        self.assertNotIn("features", codex_api)

    def test_sqlx_workspace_features_are_shared_by_sqlite_crates(self) -> None:
        state_manifest = load_toml(REPO_ROOT / "codex-rs" / "state" / "Cargo.toml")
        cli_manifest = load_toml(REPO_ROOT / "codex-rs" / "cli" / "Cargo.toml")

        state_sqlx = state_manifest["dependencies"]["sqlx"]
        self.assertEqual(state_sqlx, {"workspace": True})

        cli_dev_sqlx = cli_manifest["dev-dependencies"]["sqlx"]
        self.assertEqual(cli_dev_sqlx, {"workspace": True})

    def test_default_test_recipes_use_bounded_nextest_profiles(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        nextest = load_toml(REPO_ROOT / "codex-rs" / ".config" / "nextest.toml")

        self.assertIn(
            '$env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "local"; cargo nextest run --no-fail-fast',
            justfile,
        )
        self.assertEqual(nextest["profile"]["default"]["test-threads"], 4)
        local_profile = nextest["profile"]["local"]
        self.assertEqual(local_profile["inherits"], "default")
        fast_profile = nextest["profile"]["fast"]
        self.assertEqual(fast_profile["inherits"], "local")
        self.assertEqual(fast_profile["retries"], 0)
        local_app_server_override = {
            "filter": "package(codex-app-server) & kind(test)",
            "test-group": "app_server_integration_local",
        }
        self.assertIn(
            local_app_server_override,
            local_profile["overrides"],
        )
        self.assertIn(local_app_server_override, fast_profile["overrides"])
        self.assertIn('$env:NEXTEST_PROFILE = "fast"; cargo nextest run', justfile)
        no_sccache_recipe = justfile.split("test-fast-nosccache *args:", 1)[1].split(
            "\n\n", 1
        )[0]
        self.assertIn('$env:NEXTEST_PROFILE = "fast"', no_sccache_recipe)
        self.assertIn(
            '$env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "local"; cargo nextest run --no-fail-fast --timings=html,json',
            justfile,
        )
        self.assertNotIn("changed-validation", justfile)

    def test_default_fmt_recipe_uses_fast_local_with_full_escape_hatch(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        self.assertIn(
            "fmt:\n    {{ python }} ../scripts/format.py --fast-local",
            justfile,
        )
        self.assertIn("fmt-full:\n    {{ python }} ../scripts/format.py", justfile)
        self.assertIn(
            "fmt-check:\n    {{ python }} ../scripts/format.py --check",
            justfile,
        )
        self.assertIn(
            "validate-crate crate:\n    just fmt-check-fast\n    just test-fast -p {{ crate }}",
            justfile,
        )
        self.assertIn(
            "validate-crate-full crate:\n    just fmt-check\n    just test-fast -p {{ crate }}",
            justfile,
        )

    def test_cargo_config_caps_parallelism_and_nonduplicated_windows_flags(
        self,
    ) -> None:
        cargo_config = load_toml(REPO_ROOT / "codex-rs" / ".cargo" / "config.toml")
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        self.assertEqual(cargo_config["build"]["jobs"], 8)
        self.assertEqual(
            cargo_config["env"]["RUST_TEST_THREADS"],
            {"value": "16", "force": False},
        )
        self.assertIn('rust_parallelism := "16"', justfile)
        self.assertIn(
            'env_var_or_default("CARGO_BUILD_JOBS", rust_parallelism)',
            justfile,
        )
        self.assertIn(
            'env_var_or_default("RUST_TEST_THREADS", rust_parallelism)',
            justfile,
        )
        self.assertIn(
            'env_var_or_default("NEXTEST_TEST_THREADS", rust_parallelism)',
            justfile,
        )
        self.assertIn("export CARGO_BUILD_JOBS := cargo_build_jobs", justfile)
        self.assertIn("export RUST_TEST_THREADS := rust_test_threads", justfile)
        self.assertIn("export NEXTEST_TEST_THREADS := nextest_test_threads", justfile)

        targets = cargo_config["target"]
        msvc_flags = targets['cfg(all(windows, target_env = "msvc"))']["rustflags"]
        arm64_flags = targets["aarch64-pc-windows-msvc"]["rustflags"]
        self.assertEqual(
            msvc_flags,
            [
                "-C",
                "link-arg=/STACK:8388608",
                "-C",
                "target-feature=+crt-static",
            ],
        )
        self.assertEqual(arm64_flags, ["-C", "link-arg=/arm64hazardfree"])

        effective_arm64_flags = [*msvc_flags, *arm64_flags]
        self.assertEqual(effective_arm64_flags.count("link-arg=/STACK:8388608"), 1)
        self.assertEqual(effective_arm64_flags.count("target-feature=+crt-static"), 1)
        self.assertEqual(effective_arm64_flags.count("link-arg=/arm64hazardfree"), 1)

    def test_cargo_audit_policy_is_wired_and_synchronized(self) -> None:
        audit = load_toml(REPO_ROOT / "codex-rs" / ".cargo" / "audit.toml")
        deny = load_toml(REPO_ROOT / "codex-rs" / "deny.toml")
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        audit_ignores = audit["advisories"]["ignore"]
        deny_ignores = [entry["id"] for entry in deny["advisories"]["ignore"]]
        self.assertEqual(len(audit_ignores), len(set(audit_ignores)))
        self.assertEqual(set(audit_ignores), set(deny_ignores))
        self.assertEqual(audit["output"]["deny"], ["yanked"])
        self.assertFalse(audit["output"]["quiet"])
        self.assertFalse(audit["output"]["show_tree"])
        self.assertIn("deps-audit:\n    cargo audit", justfile)
        self.assertNotIn(".github/workflows/cargo-audit.yml", justfile)

    def test_rust_toolchain_manifest_stays_lean_for_local_bootstrap(self) -> None:
        toolchain = load_toml(REPO_ROOT / "codex-rs" / "rust-toolchain.toml")[
            "toolchain"
        ]

        self.assertEqual(toolchain["channel"], "1.95.0")
        self.assertEqual(toolchain["components"], ["clippy", "rustfmt", "rust-src"])
        self.assertNotIn("profile", toolchain)
        self.assertNotIn("targets", toolchain)

    def test_formatter_uses_pinned_nightly_rustfmt(self) -> None:
        format_script = load_format_module()

        groups = format_script.formatter_groups(
            check=True,
            selected_groups={"rust"},
        )

        self.assertEqual(len(groups), 1)
        command = groups[0].commands[0]
        self.assertEqual(
            command.args,
            (
                "rustup",
                "run",
                tool_versions.RUSTFMT_TOOLCHAIN,
                "cargo",
                "fmt",
                "--check",
            ),
        )
        self.assertEqual(command.cwd, REPO_ROOT / "codex-rs")
        self.assertFalse(hasattr(command, "discard_stderr"))

    def test_formatter_full_path_includes_prettier_targets(self) -> None:
        format_script = load_format_module()

        groups = format_script.formatter_groups(
            check=True,
            selected_groups={"prettier"},
        )

        self.assertEqual(len(groups), 1)
        command = groups[0].commands[0]
        self.assertEqual(command.args[:4], ("pnpm", "exec", "prettier", "--check"))
        self.assertNotIn("docs/*.md", command.args)
        self.assertIn("**/*.js", command.args)
        self.assertIn("sdk/typescript/**/*.ts", command.args)

    def test_formatter_only_constructs_selected_group_lazily(self) -> None:
        format_script = load_format_module()

        with mock.patch.object(
            format_script,
            "prettier_formatter_group",
            side_effect=AssertionError("prettier should not be constructed"),
        ):
            groups = format_script.formatter_groups(
                check=True,
                selected_groups={"python-scripts"},
            )

        self.assertEqual([group.name for group in groups], ["Python scripts"])

    def test_formatter_explains_fast_local_only_conflict(self) -> None:
        format_script = load_format_module()
        stderr = io.StringIO()

        with (
            mock.patch("sys.stderr", stderr),
            self.assertRaises(SystemExit) as raised,
        ):
            format_script.main(["--fast-local", "--only", "prettier"])

        self.assertEqual(raised.exception.code, 2)
        self.assertIn(
            "--fast-local excludes formatter groups selected by --only: prettier",
            stderr.getvalue(),
        )

    def test_ci_formatter_jobs_install_and_use_pinned_nightly_rustfmt(self) -> None:
        workflow_paths = [
            REPO_ROOT / ".github" / "workflows" / workflow_name
            for workflow_name in ("rust-ci.yml", "rust-ci-full.yml")
        ]
        if not any(path.exists() for path in workflow_paths):
            # Skip visibly instead of passing vacuously when the fork carries
            # no rust CI workflows at all.
            self.skipTest("no rust CI workflows are present in this fork")
        for workflow_path in workflow_paths:
            if not workflow_path.exists():
                continue

            workflow = workflow_path.read_text(encoding="utf-8")

            self.assertIn(
                f"rustup toolchain install {tool_versions.RUSTFMT_TOOLCHAIN}",
                workflow,
            )
            self.assertIn("--component rustfmt", workflow)
            self.assertIn(
                f"cargo +{tool_versions.RUSTFMT_TOOLCHAIN} fmt -- --config imports_granularity=Item --check",
                workflow,
            )
            self.assertNotIn(
                "run: cargo fmt -- --config imports_granularity=Item --check",
                workflow,
            )

    def test_windows_setup_uses_toolchain_manifest_for_rustup_options(self) -> None:
        setup_windows = (
            REPO_ROOT / "codex-rs" / "scripts" / "setup-windows.ps1"
        ).read_text(encoding="utf-8")

        self.assertIn("$toolchain = '1.95.0'", setup_windows)
        self.assertIn(
            "& rustup toolchain install $toolchain --profile minimal",
            setup_windows,
        )
        self.assertIn(
            "& rustup component add clippy rustfmt rust-src --toolchain $toolchain",
            setup_windows,
        )

    def test_publish_build_uses_static_crt_from_cargo_config_once(self) -> None:
        script_root = REPO_ROOT / "scripts"
        publish_entrypoint = (script_root / "publish-local-codex.ps1").read_text(
            encoding="utf-8"
        )
        cargo_config = (REPO_ROOT / "codex-rs" / ".cargo" / "config.toml").read_text(
            encoding="utf-8"
        )

        self.assertIn("function Invoke-CodexBuild", publish_entrypoint)
        self.assertIn("target-feature=+crt-static", cargo_config)
        self.assertIn("via codex-rs/.cargo/config.toml", publish_entrypoint)
        self.assertNotIn("CARGO_TARGET_*_RUSTFLAGS", publish_entrypoint)
        self.assertNotIn("$env:RUSTFLAGS", publish_entrypoint)

    def test_publish_commit_state_covers_changed_binary_restart_failures(self) -> None:
        publish_script = (REPO_ROOT / "scripts" / "publish-local-codex.ps1").read_text(
            encoding="utf-8"
        )

        state_start = publish_script.index("$publishCommitted = $false")
        outer_try = publish_script.index("try {", state_start)
        self.assertIn("$restartFailure = $null", publish_script[state_start:outer_try])

        commit_markers = [
            index
            for index in range(len(publish_script))
            if publish_script.startswith("$publishCommitted = $true", index)
        ]
        self.assertEqual(len(commit_markers), 2)
        changed_publish_commit = commit_markers[1]
        changed_publish_restart = publish_script.index(
            "if ($RestartDesktop) {", changed_publish_commit
        )
        backup_cleanup = publish_script.index(
            "$protectedBackupPath =", changed_publish_restart
        )
        self.assertLess(changed_publish_commit, changed_publish_restart)
        self.assertLess(changed_publish_restart, backup_cleanup)
        restart_block = publish_script[changed_publish_restart:backup_cleanup]
        self.assertIn("$restartFailure = $_.Exception", restart_block)
        self.assertIn('Write-ProofLine "restartFailed" "true"', restart_block)
        self.assertIn(
            "if ((-not $publishCommitted) -and $null -ne $desktopRoutingSnapshot)",
            publish_script,
        )

    def test_dependency_policy_dispatches_duplicate_check_recipe(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        recipe = justfile.split("deps-policy-check *args:", 1)[1].split("\n\n", 1)[0]

        self.assertIn("just deps-duplicates-check {args}", recipe)
        self.assertNotIn("just deps-duplicates {args}", recipe)

if __name__ == "__main__":
    unittest.main()
