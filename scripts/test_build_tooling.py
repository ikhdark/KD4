#!/usr/bin/env python3

import contextlib
import io
import json
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest
from unittest import mock

from scripts import rust_packages
from scripts import rust_build_status
from scripts import tool_versions


REPO_ROOT = Path(__file__).resolve().parents[1]
CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)


def powershell() -> str | None:
    # Prefer Windows PowerShell 5.1: the justfile invokes these scripts via
    # `powershell -NoProfile -File ...`, so tests should exercise the same
    # host (5.1 has stricter native-stderr and StrictMode semantics).
    return shutil.which("powershell") or shutil.which("pwsh")


def pwsh_only() -> str | None:
    # invoke-rust-perf-env.ps1 runs under pwsh 7.4+ in production (recipes
    # invoke it inline in the just-shell pwsh session), and its -NoSccache
    # proof depends on pwsh's empty-env-var semantics, so its tests must not
    # fall back to Windows PowerShell 5.1.
    return shutil.which("pwsh")


def ps_single_quote(value: str | Path) -> str:
    return "'" + str(value).replace("'", "''") + "'"


def load_just_shell_module():
    path = REPO_ROOT / "scripts" / "just-shell.py"
    spec = importlib.util.spec_from_file_location("just_shell", path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_format_module():
    path = REPO_ROOT / "scripts" / "format.py"
    spec = importlib.util.spec_from_file_location("format_script", path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_root_maintenance_module():
    path = REPO_ROOT / "scripts" / "root_maintenance.py"
    spec = importlib.util.spec_from_file_location("root_maintenance", path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_toml(path: Path):
    return tomllib.loads(path.read_text(encoding="utf-8"))


class BuildToolingTest(unittest.TestCase):
    def test_local_just_shell_sets_sccache_without_probe(self) -> None:
        just_shell = load_just_shell_module()
        can_run_calls = []

        updates = just_shell.rust_tool_env(
            {"PATH": "ignored"},
            os_name="posix",
            which=lambda program: f"/tools/{program}" if program == "sccache" else None,
            can_run=lambda command: can_run_calls.append(command) or False,
            repo_root=REPO_ROOT,
        )

        self.assertEqual(updates["RUSTC_WRAPPER"], "/tools/sccache")
        self.assertEqual(updates["SCCACHE_BASEDIR"], str(REPO_ROOT))
        self.assertEqual(updates["SCCACHE_CACHE_SIZE"], "80G")
        self.assertEqual(can_run_calls, [])

    def test_local_just_shell_sets_sccache_env_for_existing_sccache_wrapper(
        self,
    ) -> None:
        just_shell = load_just_shell_module()

        updates = just_shell.rust_tool_env(
            {"RUSTC_WRAPPER": "sccache"},
            os_name="posix",
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
            os_name="posix",
            which=which,
            repo_root=REPO_ROOT,
        )
        self.assertEqual(updates["SCCACHE_CACHE_SIZE"], "100G")

        blank_updates = just_shell.rust_tool_env(
            {"CODEX_SCCACHE_CACHE_SIZE": "   "},
            os_name="posix",
            which=which,
            repo_root=REPO_ROOT,
        )
        self.assertEqual(blank_updates["SCCACHE_CACHE_SIZE"], "80G")

    def test_local_just_shell_keeps_existing_rust_wrapper(self) -> None:
        just_shell = load_just_shell_module()

        updates = just_shell.rust_tool_env(
            {"RUSTC_WRAPPER": "existing-wrapper"},
            os_name="posix",
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
            os_name="nt",
            which=lambda program: f"C:/tools/{program}.exe",
        )

        self.assertEqual(updates, {})

    def test_windows_local_just_shell_uses_lld_link_when_available(self) -> None:
        just_shell = load_just_shell_module()
        can_run_calls = []

        updates = just_shell.rust_tool_env(
            {},
            os_name="nt",
            which=lambda program: (
                "C:/LLVM/bin/lld-link.exe" if program == "lld-link" else None
            ),
            can_run=lambda command: can_run_calls.append(command) or True,
        )

        self.assertEqual(updates["CARGO_NET_GIT_FETCH_WITH_CLI"], "true")
        self.assertEqual(
            updates["CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"],
            "C:/LLVM/bin/lld-link.exe",
        )
        self.assertNotIn("CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER", updates)
        self.assertEqual(can_run_calls, [])

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
                os_name="nt",
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
            os_name="nt",
            which=lambda program: (
                "C:/LLVM/bin/lld-link.exe" if program == "lld-link" else None
            ),
        )

        self.assertEqual(updates["CARGO_NET_GIT_FETCH_WITH_CLI"], "true")
        self.assertNotIn("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER", updates)

    def test_linux_local_just_shell_prefers_mold_linker_when_available(self) -> None:
        just_shell = load_just_shell_module()

        updates = just_shell.rust_tool_env(
            {},
            os_name="posix",
            which=lambda program: (
                f"/usr/bin/{program}"
                if program in {"clang", "mold", "ld.lld"}
                else None
            ),
            platform_name="linux",
        )

        self.assertEqual(
            updates["CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER"],
            "/usr/bin/clang",
        )
        self.assertEqual(
            updates["CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS"],
            "-C link-arg=-fuse-ld=mold",
        )

    def test_linux_local_just_shell_falls_back_to_lld_linker(self) -> None:
        just_shell = load_just_shell_module()

        updates = just_shell.rust_tool_env(
            {},
            os_name="posix",
            which=lambda program: (
                f"/usr/bin/{program}" if program in {"clang", "ld.lld"} else None
            ),
            platform_name="linux",
        )

        self.assertEqual(
            updates["CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS"],
            "-C link-arg=-fuse-ld=lld",
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
                os_name="nt",
                repo_root=root,
            )

        self.assertEqual(updates["VIRTUAL_ENV"], str(root / "scripts" / ".venv"))
        self.assertEqual(updates["VIRTUAL_ENV_DISABLE_PROMPT"], "1")
        self.assertEqual(
            updates["PATH"],
            f"{scripts_bin}{just_shell.os.pathsep}C:/Windows/System32",
        )

    def test_local_just_shell_uses_physical_core_count_for_python(self) -> None:
        just_shell = load_just_shell_module()

        self.assertEqual(
            just_shell.python_cpu_env({}),
            {"PYTHON_CPU_COUNT": "16"},
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
                os_name="nt",
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
                os_name="nt",
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
                os_name="nt",
                repo_root=root,
                cache_dir=cache_dir,
                stderr=stderr,
            )
            second_stderr = io.StringIO()
            just_shell.python_tool_env(
                {},
                os_name="nt",
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
                just_shell.python_tool_env({}, os_name="nt", repo_root=root),
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

    def test_local_just_shell_reports_missing_sh(self) -> None:
        just_shell = load_just_shell_module()
        stderr = io.StringIO()

        result = just_shell.run_sh(
            "echo hi", "recipe", [], which=lambda program: None, stderr=stderr
        )

        self.assertEqual(result, 1)
        self.assertIn("POSIX shell", stderr.getvalue())

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

        self.assertNotIn("default-members", workspace)
        self.assertIn("v8-poc", workspace["members"])

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

    def test_default_test_recipe_uses_upstream_nextest_defaults(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        nextest = (REPO_ROOT / "codex-rs" / ".config" / "nextest.toml").read_text(
            encoding="utf-8"
        )

        self.assertIn(
            "RUST_MIN_STACK={{ rust_min_stack }} NEXTEST_PROFILE=local cargo nextest run --no-fail-fast",
            justfile,
        )
        self.assertIn(
            '$env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "local"; cargo nextest run --no-fail-fast',
            justfile,
        )
        self.assertIn('[profile.fast]\ninherits = "local"', nextest)
        self.assertIn("retries = 0", nextest)
        self.assertIn(
            "RUST_MIN_STACK={{ rust_min_stack }} NEXTEST_PROFILE=fast cargo nextest run",
            justfile,
        )
        self.assertIn('$env:NEXTEST_PROFILE = "fast"; cargo nextest run', justfile)
        self.assertIn(
            "RUST_MIN_STACK={{ rust_min_stack }} NEXTEST_PROFILE=local cargo nextest run --no-fail-fast --timings=html,json",
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

    def test_windows_msvc_cargo_config_uses_upstream_static_crt(
        self,
    ) -> None:
        cargo_config = (REPO_ROOT / "codex-rs" / ".cargo" / "config.toml").read_text(
            encoding="utf-8"
        )

        self.assertIn("link-arg=/STACK:8388608", cargo_config)
        self.assertIn("target-feature=+crt-static", cargo_config)

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
            ("cargo", f"+{tool_versions.RUSTFMT_TOOLCHAIN}", "fmt", "--check"),
        )
        self.assertEqual(command.cwd, REPO_ROOT / "codex-rs")
        self.assertFalse(command.discard_stderr)

    def test_formatter_full_path_includes_prettier_targets(self) -> None:
        format_script = load_format_module()

        groups = format_script.formatter_groups(
            check=True,
            selected_groups={"prettier"},
        )

        self.assertEqual(len(groups), 1)
        command = groups[0].commands[0]
        self.assertEqual(command.args[:4], ("pnpm", "exec", "prettier", "--check"))
        self.assertIn("docs/*.md", command.args)
        self.assertIn("codex-cli/**/*.js", command.args)
        self.assertIn("sdk/typescript/**/*.ts", command.args)

    def test_formatter_only_constructs_selected_group_lazily(self) -> None:
        format_script = load_format_module()

        with mock.patch.object(
            format_script,
            "buildifier_formatter_group",
            side_effect=AssertionError("buildifier should not be constructed"),
        ):
            groups = format_script.formatter_groups(
                check=True,
                selected_groups={"python scripts"},
            )

        self.assertEqual([group.name for group in groups], ["Python scripts"])

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

    def test_publish_build_enables_static_crt_outside_cargo_config(self) -> None:
        publish_script = (REPO_ROOT / "scripts" / "publish-local-codex.ps1").read_text(
            encoding="utf-8"
        )

        self.assertIn("Enable-StaticMsvcRustFlagsForPublish", publish_script)
        self.assertIn("CARGO_TARGET_*_RUSTFLAGS", publish_script)
        self.assertIn("target-feature=+crt-static", publish_script)
        self.assertNotIn("$env:RUSTFLAGS", publish_script)

    def test_build_info_script_uses_upstream_git_metadata_fallbacks(
        self,
    ) -> None:
        text = (REPO_ROOT / "codex-rs" / "build_info.rs").read_text(encoding="utf-8")
        self.assertIn('args(["status", "--porcelain"])', text)
        self.assertIn("git_dirty(&workspace_root)", text)
        self.assertIn(
            'cargo:rerun-if-changed={}", git_dir.join("index").display()', text
        )
        self.assertNotIn("SystemTime::now", text)
        self.assertIn('workspace_root.join("build_info.rs").display()', text)

    def test_build_info_scripts_match_upstream_reset_shape(self) -> None:
        app_server_build = (
            REPO_ROOT / "codex-rs" / "app-server" / "build.rs"
        ).read_text(encoding="utf-8")
        cli_build = (REPO_ROOT / "codex-rs" / "cli" / "build.rs").read_text(
            encoding="utf-8"
        )

        self.assertIn('#[path = "../build_info.rs"]', app_server_build)
        self.assertIn("build_info::emit();", app_server_build)
        self.assertIn("cargo:rustc-link-arg=-ObjC", cli_build)
        self.assertNotIn("build_info::emit();", cli_build)

    def test_bazel_build_scripts_match_upstream_reset_shape(
        self,
    ) -> None:
        cli_build = (REPO_ROOT / "codex-rs" / "cli" / "BUILD.bazel").read_text(
            encoding="utf-8"
        )
        app_server_build = (
            REPO_ROOT / "codex-rs" / "app-server" / "BUILD.bazel"
        ).read_text(encoding="utf-8")

        self.assertIn("MACOS_WEBRTC_RUSTC_LINK_FLAGS", cli_build)
        self.assertIn("extra_binaries_non_windows", app_server_build)
        self.assertNotIn("build_script_srcs", cli_build)
        self.assertNotIn("build_script_srcs", app_server_build)

    def test_bwrap_build_script_tracks_resolved_source_dir(self) -> None:
        text = (REPO_ROOT / "codex-rs" / "bwrap" / "build.rs").read_text(
            encoding="utf-8"
        )

        self.assertIn(
            'println!("cargo:rerun-if-env-changed=CODEX_BWRAP_SOURCE_DIR");', text
        )
        self.assertIn("vendor_dir.join(source).display()", text)

    def test_skills_build_script_requires_bundled_samples(self) -> None:
        text = (REPO_ROOT / "codex-rs" / "skills" / "build.rs").read_text(
            encoding="utf-8"
        )

        self.assertIn('let samples_dir = Path::new("src/assets/samples");', text)
        self.assertIn("if !samples_dir.exists()", text)

    def test_repo_local_skill_tree_is_local_build_focused(self) -> None:
        required = {
            "kd4-crosscheck-and-finish",
            "kd4-harness",
        }
        skills_dir = REPO_ROOT / ".codex" / "skills"
        if not skills_dir.exists():
            self.skipTest("repo-local skills directory is not materialized")
        actual = {path.name for path in skills_dir.iterdir() if path.is_dir()}

        self.assertTrue(required.issubset(actual))

    def test_repo_local_skill_frontmatter_names_match_folders(self) -> None:
        skills_dir = REPO_ROOT / ".codex" / "skills"
        if not skills_dir.exists():
            self.skipTest("repo-local skills directory is not materialized")
        skill_dirs = [path for path in skills_dir.iterdir() if path.is_dir()]
        self.assertTrue(skill_dirs, "skills directory exists but contains no skills")
        frontmatter_names: list[str] = []
        for skill_dir in skill_dirs:
            skill_path = skill_dir / "SKILL.md"
            # A skill directory without SKILL.md is a broken skill, not an
            # ignorable one.
            self.assertTrue(
                skill_path.exists(),
                f"skill '{skill_dir.name}' is missing SKILL.md",
            )
            skill = skill_path.read_text(encoding="utf-8")

            name_lines = [
                line for line in skill.splitlines() if line.startswith("name: ")
            ]
            self.assertEqual(len(name_lines), 1, f"invalid skill name in {skill_path}")
            frontmatter_names.append(name_lines[0].removeprefix("name: ").strip())
        self.assertEqual(len(frontmatter_names), len(set(frontmatter_names)))

    def test_agents_skill_inventory_matches_local_build_tree(self) -> None:
        agents = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")
        normalized = " ".join(agents.split())
        skills_dir = REPO_ROOT / ".codex" / "skills"
        if not skills_dir.exists():
            # Skip visibly instead of silently dropping the central
            # inventory assertion.
            self.skipTest("repo-local skills directory is not materialized")
        skill_names = sorted(
            path.name for path in skills_dir.iterdir() if path.is_dir()
        )
        self.assertIn("kd4-crosscheck-and-finish", skill_names)
        self.assertIn("kd4-harness", skill_names)
        self.assertIn("`.codex/skills`", agents)
        for phrase in ("fork-local skills", "validation workflows"):
            self.assertIn(phrase, normalized)

    def test_agents_mentions_current_checkout_not_stale_codexkd_path(self) -> None:
        text = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")
        first_lines = "\n".join(text.splitlines()[:40])

        self.assertIn(r"C:\Users\kuh\Desktop\kd4", first_lines)
        self.assertNotIn(r"C:\Users\kuh\Desktop\codexKD`", text)

    def test_agents_desktop_boundary_is_top_level_guidance(self) -> None:
        text = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")

        # Assert the section exists as top-level (H2) guidance and carries the
        # rebuild contract, without pinning it to a line window that breaks
        # whenever earlier sections grow.
        self.assertIn("\n## Desktop app boundary\n", text)
        section = text.split("\n## Desktop app boundary\n", 1)[1]
        section = section.split("\n## ", 1)[0]
        self.assertIn("Source edits here do not hot-apply", section)
        self.assertIn("rebuilding and updating or replacing", section)

    def test_agents_validation_map_matches_current_layout(self) -> None:
        text = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")
        normalized = " ".join(text.split())

        self.assertIn("## Validation and local-build proof", text)
        self.assertIn("Rust crates", text)
        self.assertIn("Scripts", text)
        self.assertIn("Local publish", text)
        self.assertIn("do not hand-edit generated locks", normalized)

    def test_agents_scripts_policy_is_nested_and_discoverable(self) -> None:
        root_text = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")
        scripts_text = (REPO_ROOT / "scripts" / "AGENTS.md").read_text(encoding="utf-8")

        self.assertIn("`scripts/AGENTS.md`", root_text)
        self.assertIn("# Scripts Policy", scripts_text)
        self.assertIn("Root maintenance commands", scripts_text)
        self.assertIn("root_maintenance.py", scripts_text)

    def test_installers_require_standalone_metadata(self) -> None:
        shell_installer = (REPO_ROOT / "scripts" / "install" / "install.sh").read_text(
            encoding="utf-8"
        )
        powershell_installer = (
            REPO_ROOT / "scripts" / "install" / "install.ps1"
        ).read_text(encoding="utf-8")

        self.assertIn('INSTALL_METADATA_FILE="codex-install.env"', shell_installer)
        self.assertIn(
            '[ -f "$release_dir/$INSTALL_METADATA_FILE" ] ||', shell_installer
        )
        self.assertIn('"$BIN_PATH" --version >/dev/null', shell_installer)
        self.assertNotIn('visible_command_preverified="true"', shell_installer)

        self.assertIn(
            '$InstallMetadataFile = "codex-install.env"', powershell_installer
        )
        self.assertIn("function Write-InstallMetadata", powershell_installer)
        self.assertIn("function Get-InstallMetadataField", powershell_installer)
        self.assertIn(
            "Write-InstallMetadata -ReleaseDir $stagingDir", powershell_installer
        )
        self.assertIn(
            'Get-InstallMetadataField -ReleaseDir $ReleaseDir -Name "version"',
            powershell_installer,
        )

    def test_root_maintenance_covers_current_script_tooling_tests(self) -> None:
        root_maintenance = load_root_maintenance_module()

        expected_ruff_targets = sorted(
            path.relative_to(REPO_ROOT).as_posix()
            for path in (REPO_ROOT / "scripts").rglob("*.py")
            if "__pycache__" not in path.parts and ".venv" not in path.parts
        )
        expected_unittest_targets = sorted(
            path.relative_to(REPO_ROOT).with_suffix("").as_posix().replace("/", ".")
            for path in (REPO_ROOT / "scripts").rglob("test_*.py")
            if "__pycache__" not in path.parts and ".venv" not in path.parts
        )

        self.assertEqual(root_maintenance.PYTHON_RUFF_TARGETS, expected_ruff_targets)
        self.assertEqual(
            root_maintenance.PYTHON_UNITTEST_TARGETS, expected_unittest_targets
        )
        self.assertEqual(
            root_maintenance.python_test_targets(["scripts.test_verify_local"], []),
            ["scripts.test_verify_local"],
        )
        self.assertEqual(
            root_maintenance.python_test_targets([], ["scripts/verify_local.py"]),
            ["scripts.test_verify_local"],
        )
        with mock.patch.object(
            root_maintenance,
            "git_changed_paths",
            return_value=["scripts/verify_local.py", "docs/example.md"],
        ):
            self.assertEqual(
                root_maintenance.expand_changed_paths([None]),
                ["scripts/verify_local.py", "docs/example.md"],
            )
        with mock.patch.object(
            root_maintenance,
            "git_changed_paths",
            return_value=["scripts/verify_local.py"],
        ):
            self.assertEqual(
                root_maintenance.python_test_targets(
                    [], root_maintenance.expand_changed_paths([None])
                ),
                ["scripts.test_verify_local"],
            )
        self.assertEqual(
            root_maintenance.test_module_for_changed_path("docs/example.md"),
            None,
        )
        self.assertEqual(
            root_maintenance.test_modules_for_changed_path(
                "scripts/publish-local-codex.ps1"
            ),
            ("scripts.test_publish_local_codex",),
        )
        self.assertEqual(
            root_maintenance.test_modules_for_changed_path(
                "scripts/publish-local-codex-wsl.sh"
            ),
            (
                "scripts.test_dev_environment",
                "scripts.test_publish_local_codex",
            ),
        )

    def test_root_maintenance_git_paths_use_nul_delimiters(self) -> None:
        root_maintenance = load_root_maintenance_module()
        completed = subprocess.CompletedProcess(
            ["git"],
            0,
            stdout="scripts/line\nbreak.py\0scripts/ trailing .py\0",
            stderr="",
        )

        with mock.patch.object(
            root_maintenance.subprocess, "run", return_value=completed
        ) as run:
            paths = root_maintenance.git_changed_paths()

        self.assertEqual(
            paths,
            ["scripts/line\nbreak.py", "scripts/ trailing .py"],
        )
        self.assertIn("-z", run.call_args.args[0])

    def test_root_maintenance_uv_commands_use_frozen_lock(self) -> None:
        root_maintenance = load_root_maintenance_module()
        calls: list[tuple[str, ...]] = []

        def fake_run(command: list[str]) -> int:
            calls.append(tuple(command))
            return 0

        with mock.patch.object(root_maintenance, "run", side_effect=fake_run):
            self.assertEqual(
                root_maintenance.main(
                    ["format-python", "--changed", "scripts/verify_local.py"]
                ),
                0,
            )
            self.assertEqual(
                root_maintenance.main(
                    ["lint-python", "--changed", "scripts/verify_local.py"]
                ),
                0,
            )
            self.assertEqual(
                root_maintenance.main(
                    ["test-python", "--module", "scripts.test_verify_local"]
                ),
                0,
            )

        for command in calls:
            self.assertEqual(command[:4], ("uv", "run", "--frozen", "--project"))

    def test_codex_cli_launcher_parses_under_node(self) -> None:
        node = shutil.which("node")
        if node is None:
            self.skipTest("node is not available")

        result = subprocess.run(
            [node, "--check", str(REPO_ROOT / "codex-cli" / "bin" / "codex.js")],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
        )

        self.assertEqual(result.returncode, 0, result.stderr)

    def test_package_json_exposes_focused_script_test_alias(self) -> None:
        package = json.loads((REPO_ROOT / "package.json").read_text(encoding="utf-8"))

        self.assertEqual(
            package["scripts"]["test:scripts:target"],
            "python scripts/root_maintenance.py test-python --changed",
        )
        self.assertEqual(
            package["scripts"]["test:scripts:changed"],
            "python scripts/root_maintenance.py test-python --changed",
        )

    def test_rust_package_search_start_keeps_existing_dotted_directories(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            package_dir = repo_root / "codex-rs" / "crate.with.dot"
            package_dir.mkdir(parents=True)
            (package_dir / "Cargo.toml").write_text(
                '[package]\nname = "crate-with-dot"\n',
                encoding="utf-8",
            )

            self.assertEqual(
                rust_packages.package_search_start(package_dir), package_dir
            )
            self.assertEqual(
                rust_packages.nearest_package_root(package_dir, repo_root=repo_root),
                package_dir,
            )

    def test_rust_package_search_does_not_escape_repo_root(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            parent = Path(temp_dir)
            (parent / "Cargo.toml").write_text(
                '[package]\nname = "external"\n',
                encoding="utf-8",
            )
            repo_root = parent / "kd4"
            script = repo_root / "scripts" / "format.py"
            script.parent.mkdir(parents=True)
            script.write_text("", encoding="utf-8")

            self.assertIsNone(
                rust_packages.nearest_package_root(
                    script,
                    repo_root=repo_root,
                    assume_file=True,
                )
            )

    def test_formatter_group_decodes_command_output_as_utf8(self) -> None:
        format_script = load_format_module()
        group = format_script.FormatterGroup(
            "Test",
            (
                format_script.Command(
                    (
                        sys.executable,
                        "-c",
                        "import sys; sys.stdout.buffer.write(b'check \\xf0\\x9f\\x9b\\xa0 done')",
                    )
                ),
            ),
        )

        result = format_script.run_formatter_group(group)

        self.assertEqual(result.returncode, 0)
        self.assertIn("check \U0001f6e0 done", result.output)

    def test_agents_validation_tooling_does_not_prove_runtime_fix(self) -> None:
        text = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")
        normalized = " ".join(text.split())

        self.assertIn(
            "Tooling success alone does not prove a",
            text,
        )
        self.assertIn("focused failing test or approved final gate", normalized)

    def test_local_rust_loop_recipes_are_discoverable(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        for recipe in (
            "cargo-fetch:",
            "codex-fast *args:",
            "codex-lane *args:",
            "codex-stale-ok *args:",
            "fix-lane",
            "watch-lane package *args:",
            "coverage-lane package *args:",
            "rust-build-doctor:",
            "target-disk:",
            "target-prune *args:",
            "target-optimize *args:",
            "target-optimize-dry-run *args:",
            "build-dev-small package:",
            "run-dev-small package *args:",
            "local-release package:",
            "bazel-test-changed *targets:",
            "bench-workspace *args:",
            "test-lane-fast lane *args:",
            "test-windows-sandbox-processes *args:",
            "deps-duplicates-workspace *args:",
            "deps-policy-check *args:",
        ):
            self.assertIn(recipe, justfile)

    def test_local_setup_recipes_avoid_stale_or_unlocked_dependency_state(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        # `codex-fast` must actually be fast: reuse the built binary instead of
        # duplicating the plain `codex` recipe.
        self.assertIn("codex-fast *args:\n    just codex-stale-ok {args}", justfile)
        # Install/setup paths must not quietly re-resolve the lockfile.
        self.assertIn("cargo fetch --locked", justfile)
        self.assertNotIn("cargo fetch\n", justfile)

    def test_dependency_policy_gate_runs_offline_cargo_deny(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        rules = (REPO_ROOT / "scripts" / "verify_local_rules.toml").read_text(
            encoding="utf-8"
        )

        self.assertIn("cargo deny check bans sources licenses", justfile)
        self.assertIn("cargo tree -d --workspace --target all", justfile)
        self.assertIn('validation_command = ["just", "deps-policy-check"]', rules)

    def test_unix_lane_recipes_mirror_windows_focused_lanes(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        for snippet in (
            'shift; RUST_MIN_STACK={{ rust_min_stack }} NEXTEST_PROFILE=fast cargo nextest run --target-dir "target/lanes/{{ package }}" -p {{ package }} "$@"',
            'shift; cargo check --target-dir "target/lanes/{{ package }}" -p {{ package }} "$@"',
            'shift; cargo clippy --tests --target-dir "target/lanes/{{ package }}" -p {{ package }} "$@"',
            'cargo build --release --target-dir target/lanes/release "$@"',
        ):
            self.assertIn(snippet, justfile)

    def test_high_contention_just_recipes_use_cargo_lanes_on_windows(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        for command in (
            "cargo clippy --fix --tests --allow-dirty @forwarded_args",
            "cargo clippy --tests @forwarded_args",
            "fix-workspace *args:",
            "clippy-workspace *args:",
            "Pass a package/filter to 'just fix'",
            "Pass a package/filter to 'just clippy'",
            "cargo nextest run --no-run @forwarded_args",
            'cargo watch -x "check -p {{ package }}" @($args | Select-Object -Skip 2)',
            'cargo llvm-cov -p "{{ package }}" @($args | Select-Object -Skip 2)',
            "_core-test-helpers-mcp:",
            "_core-test-helpers-windows-sandbox:",
            '$text -match "(?i)rmcp|mcp|plugin|test_stdio_server"',
            '$text -match "(?i)windows_sandbox|windows-sandbox|sandbox|codex_command_runner"',
        ):
            self.assertIn(command, justfile)
        self.assertGreaterEqual(justfile.count("scripts\\cargo-lane.ps1"), 3)

    def test_perf_env_recipes_pass_structured_argv(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        perf_env = (REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1").read_text(
            encoding="utf-8"
        )

        self.assertIn("[string]$CargoTargetLane", perf_env)
        self.assertIn("[Parameter(ValueFromRemainingArguments = $true)]", perf_env)
        self.assertIn("[string[]]$ProgramArgs", perf_env)
        self.assertIn("& $program @arguments", perf_env)
        self.assertIn("-ProgramArgs $forwarded_args", justfile)
        self.assertIn("-ProgramArgs $command_args", justfile)
        self.assertIn('"--release"', justfile)
        self.assertIn(
            '& "{{ justfile_directory() }}\\scripts\\invoke-rust-perf-env.ps1"',
            justfile,
        )
        self.assertGreaterEqual(justfile.count("; exit $LASTEXITCODE"), 3)
        self.assertIn('-CargoTargetLane "perf-nextest-nosccache"', justfile)
        self.assertIn('-CargoTargetLane "release-cli"', justfile)
        self.assertNotIn(
            'pwsh -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\\scripts\\invoke-rust-perf-env.ps1"',
            justfile,
        )
        self.assertNotIn('-CommandLine (("cargo', justfile)
        self.assertNotIn("[string]$CommandLine", perf_env)
        self.assertNotIn("cmd.exe /d /s /c", perf_env)

    def test_remote_env_setup_quotes_container_paths_and_tracks_ownership(self) -> None:
        remote_env = (REPO_ROOT / "scripts" / "test-remote-env.sh").read_text(
            encoding="utf-8"
        )

        self.assertIn("cleanup_remote_env_setup_failure", remote_env)
        self.assertIn("CODEX_TEST_REMOTE_EXEC_SERVER_MANAGED", remote_env)
        self.assertIn('nohup "$remote_codex" exec-server', remote_env)
        self.assertNotIn("nohup ${remote_codex_path} exec-server", remote_env)

    @unittest.skipUnless(os.name == "nt", "invoke-rust-perf-env is Windows-only")
    def test_perf_env_no_sccache_leaves_incremental_and_uses_lane(self) -> None:
        shell = pwsh_only()
        if shell is None:
            self.skipTest("pwsh is not available")
        script = REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1"
        env = os.environ.copy()
        env["CARGO_INCREMENTAL"] = "keep"
        env["RUSTC_WRAPPER"] = "existing-wrapper"
        env["SCCACHE_BASEDIR"] = "stale"
        env["SCCACHE_CACHE_SIZE"] = "stale"

        result = subprocess.run(
            [
                shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                (
                    f"$programArgs = @({ps_single_quote(shell)}, '-NoProfile', "
                    "'-Command', 'exit 7'); "
                    f"& {ps_single_quote(script)} -NoSccache "
                    "-CargoTargetLane 'perf nextest/nosccache' "
                    f"-WorkingDirectory {ps_single_quote(REPO_ROOT)} "
                    "-ProgramArgs $programArgs; "
                    "exit $LASTEXITCODE"
                ),
            ],
            text=True,
            capture_output=True,
            check=False,
            env=env,
            creationflags=CREATE_NO_WINDOW,
        )

        self.assertEqual(
            result.returncode,
            7,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn("rustPerfEnv:", result.stdout)
        self.assertIn("cargoIncremental=keep", result.stdout)
        self.assertIn("rustcWrapper=<empty>", result.stdout)
        self.assertIn("sccacheBaseDir=<unset>", result.stdout)
        self.assertIn("cargoTargetDir=", result.stdout)
        self.assertIn("perf-nextest-nosccache", result.stdout)

    @unittest.skipUnless(os.name == "nt", "invoke-rust-perf-env is Windows-only")
    def test_perf_env_keeps_explicit_cargo_target_dir_argument(self) -> None:
        shell = pwsh_only()
        if shell is None:
            self.skipTest("pwsh is not available")
        script = REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1"

        with tempfile.TemporaryDirectory() as tempdir:
            temp_root = Path(tempdir)
            fake_bin = temp_root / "bin"
            fake_bin.mkdir()
            (fake_bin / "cargo.cmd").write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        "if defined CARGO_TARGET_DIR (echo targetenv=%CARGO_TARGET_DIR%) else echo targetenv=",
                        "echo cargo-args:%*",
                        "exit /b 0",
                        "",
                    ]
                ),
                encoding="utf-8",
            )
            explicit_target = temp_root / "explicit-target"
            env = os.environ.copy()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            env["CARGO_TARGET_DIR"] = "stale-target-env"

            result = subprocess.run(
                [
                    shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    (
                        "$programArgs = @('cargo', 'check', '--target-dir', "
                        f"{ps_single_quote(explicit_target)}); "
                        f"& {ps_single_quote(script)} "
                        "-CargoTargetLane 'perf explicit target' "
                        f"-WorkingDirectory {ps_single_quote(REPO_ROOT)} "
                        "-ProgramArgs $programArgs; "
                        "exit $LASTEXITCODE"
                    ),
                ],
                text=True,
                capture_output=True,
                check=False,
                env=env,
                creationflags=CREATE_NO_WINDOW,
            )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn("cargoTargetDir=<explicit command argument>", result.stdout)
        self.assertIn("targetenv=", result.stdout)
        self.assertIn(f"--target-dir {explicit_target}", result.stdout)
        self.assertNotIn("stale-target-env", result.stdout)
        self.assertNotIn("perf-explicit-target", result.stdout)

    @unittest.skipUnless(os.name == "nt", "invoke-rust-perf-env is Windows-only")
    def test_perf_env_rejects_dot_path_lane_names(self) -> None:
        shell = pwsh_only()
        if shell is None:
            self.skipTest("pwsh is not available")
        script = REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1"

        result = subprocess.run(
            [
                shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                (
                    f"$programArgs = @({ps_single_quote(shell)}, '-NoProfile', "
                    "'-Command', 'exit 0'); "
                    f"& {ps_single_quote(script)} -CargoTargetLane '..' "
                    f"-WorkingDirectory {ps_single_quote(REPO_ROOT)} "
                    "-ProgramArgs $programArgs"
                ),
            ],
            text=True,
            capture_output=True,
            check=False,
            creationflags=CREATE_NO_WINDOW,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Cargo target lane", result.stderr)

    @unittest.skipUnless(os.name == "nt", "invoke-rust-perf-env is Windows-only")
    def test_perf_env_keeps_same_length_cargo_watch_rewrite(self) -> None:
        shell = pwsh_only()
        if shell is None:
            self.skipTest("pwsh is not available")
        script = REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1"

        with tempfile.TemporaryDirectory() as tempdir:
            temp_root = Path(tempdir)
            fake_bin = temp_root / "bin"
            fake_bin.mkdir()
            (fake_bin / "cargo.cmd").write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        "if defined CARGO_TARGET_DIR (echo targetenv=%CARGO_TARGET_DIR%) else echo targetenv=",
                        "echo cargo-args:%*",
                        "exit /b 0",
                        "",
                    ]
                ),
                encoding="utf-8",
            )
            env = os.environ.copy()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            env["CARGO_TARGET_DIR"] = "stale-target-env"

            result = subprocess.run(
                [
                    shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    (
                        "$programArgs = @('cargo', 'watch', '-x', "
                        "'test -- --nocapture'); "
                        f"& {ps_single_quote(script)} "
                        "-CargoTargetLane 'perf watch' "
                        f"-WorkingDirectory {ps_single_quote(REPO_ROOT)} "
                        "-ProgramArgs $programArgs; exit $LASTEXITCODE"
                    ),
                ],
                text=True,
                capture_output=True,
                check=False,
                env=env,
                creationflags=CREATE_NO_WINDOW,
            )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn("targetenv=", result.stdout)
        self.assertNotIn("targetenv=stale-target-env", result.stdout)
        self.assertIn("--target-dir", result.stdout)
        self.assertIn(" -- --nocapture", result.stdout)

    @unittest.skipUnless(os.name == "nt", "invoke-rust-perf-env is Windows-only")
    def test_perf_env_non_native_success_does_not_use_stale_last_exit_code(
        self,
    ) -> None:
        shell = pwsh_only()
        if shell is None:
            self.skipTest("pwsh is not available")
        script = REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1"

        result = subprocess.run(
            [
                shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                (
                    "function Invoke-TestSuccess { 'ok' | Out-Null }; "
                    "$global:LASTEXITCODE = 99; "
                    f". {ps_single_quote(script)} -ProgramArgs @('Invoke-TestSuccess')"
                ),
            ],
            text=True,
            capture_output=True,
            check=False,
            creationflags=CREATE_NO_WINDOW,
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )

    @unittest.skipUnless(
        os.name == "nt", "common-rust-env sccache restart is Windows-only"
    )
    def test_common_rust_env_restarts_stale_sccache_server_cache_size(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")

        with tempfile.TemporaryDirectory() as tempdir:
            temp_root = Path(tempdir)
            fake_bin = temp_root / "bin"
            fake_bin.mkdir()
            calls = temp_root / "sccache-calls.txt"
            stats = temp_root / "sccache-stats.txt"
            stats.write_text(
                "Max cache size                       10 GiB\r\n",
                encoding="utf-8",
            )
            (fake_bin / "sccache.cmd").write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        'echo %*>>"%FAKE_SCCACHE_CALLS%"',
                        'if "%1"=="--show-stats" (',
                        '  type "%FAKE_SCCACHE_STATS%"',
                        "  exit /b 0",
                        ")",
                        'if "%1"=="--stop-server" exit /b 0',
                        'if "%1"=="--start-server" (',
                        '  >"%FAKE_SCCACHE_STATS%" echo Max cache size                       80 GiB',
                        "  exit /b 0",
                        ")",
                        "exit /b 0",
                        "",
                    ]
                ),
                encoding="utf-8",
            )

            env = os.environ.copy()
            env.pop("CODEX_SCCACHE_CACHE_SIZE", None)
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            env["FAKE_SCCACHE_CALLS"] = str(calls)
            env["FAKE_SCCACHE_STATS"] = str(stats)
            script = REPO_ROOT / "scripts" / "common-rust-env.ps1"

            result = subprocess.run(
                [
                    shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    (
                        # Mirror the production session: cargo-lane.ps1
                        # dot-sources this helper under StrictMode Latest
                        # with $ErrorActionPreference = "Stop".
                        "Set-StrictMode -Version Latest; "
                        "$ErrorActionPreference = 'Stop'; "
                        f". {ps_single_quote(script)}; "
                        f"Ensure-CodexRustSccacheServer -RepoRoot {ps_single_quote(REPO_ROOT)}; "
                        'Write-Output "cacheSize=$env:SCCACHE_CACHE_SIZE"'
                    ),
                ],
                text=True,
                capture_output=True,
                check=False,
                env=env,
                creationflags=CREATE_NO_WINDOW,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("cacheSize=80G", result.stdout)
            call_text = calls.read_text(encoding="utf-8")
            self.assertIn("--show-stats", call_text)
            self.assertIn("--stop-server", call_text)
            self.assertIn("--start-server", call_text)
            self.assertIn("80 GiB", stats.read_text(encoding="utf-8"))

    @unittest.skipUnless(os.name == "nt", "common-rust-env is Windows-only")
    def test_common_rust_env_cache_size_honors_override(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")
        script = REPO_ROOT / "scripts" / "common-rust-env.ps1"
        command = (
            "Set-StrictMode -Version Latest; "
            "$ErrorActionPreference = 'Stop'; "
            f". {ps_single_quote(script)}; "
            'Write-Output "cacheSize=$(Get-CodexRustSccacheCacheSize)"'
        )

        for override, expected in (
            (" 100G ", "cacheSize=100G"),
            ("   ", "cacheSize=80G"),
        ):
            env = os.environ.copy()
            env["CODEX_SCCACHE_CACHE_SIZE"] = override
            result = subprocess.run(
                [
                    shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    command,
                ],
                text=True,
                capture_output=True,
                check=False,
                env=env,
                creationflags=CREATE_NO_WINDOW,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn(expected, result.stdout)

    @unittest.skipUnless(os.name == "nt", "sccache-perf is Windows-only")
    def test_sccache_perf_restart_ignores_stop_failure_and_checks_start(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")

        with tempfile.TemporaryDirectory() as tempdir:
            temp_root = Path(tempdir)
            fake_bin = temp_root / "bin"
            fake_bin.mkdir()
            calls = temp_root / "sccache-calls.txt"
            (fake_bin / "sccache.cmd").write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        'echo %*>>"%FAKE_SCCACHE_CALLS%"',
                        'if "%1"=="--stop-server" exit /b 7',
                        'if "%1"=="--start-server" exit /b 0',
                        'if "%1"=="--show-stats" (',
                        "  echo Max cache size                       80 GiB",
                        "  exit /b 0",
                        ")",
                        "exit /b 0",
                        "",
                    ]
                ),
                encoding="utf-8",
            )
            env = os.environ.copy()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            env["FAKE_SCCACHE_CALLS"] = str(calls)
            script = REPO_ROOT / "scripts" / "sccache-perf.ps1"

            result = subprocess.run(
                [
                    shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    str(script),
                    "restart",
                ],
                text=True,
                capture_output=True,
                check=False,
                env=env,
                creationflags=CREATE_NO_WINDOW,
            )
            call_lines = (
                calls.read_text(encoding="utf-8").splitlines() if calls.exists() else []
            )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn("Max cache size", result.stdout)
        self.assertEqual(
            call_lines,
            ["--stop-server", "--start-server", "--show-stats"],
        )

    @unittest.skipUnless(os.name == "nt", "sccache-perf is Windows-only")
    def test_sccache_perf_reset_fails_when_zero_stats_fails(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")

        with tempfile.TemporaryDirectory() as tempdir:
            temp_root = Path(tempdir)
            fake_bin = temp_root / "bin"
            fake_bin.mkdir()
            calls = temp_root / "sccache-calls.txt"
            (fake_bin / "sccache.cmd").write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        'echo %*>>"%FAKE_SCCACHE_CALLS%"',
                        'if "%1"=="--show-stats" (',
                        "  echo Max cache size                       80 GiB",
                        "  exit /b 0",
                        ")",
                        'if "%1"=="--zero-stats" exit /b 9',
                        "exit /b 0",
                        "",
                    ]
                ),
                encoding="utf-8",
            )
            env = os.environ.copy()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            env["FAKE_SCCACHE_CALLS"] = str(calls)
            script = REPO_ROOT / "scripts" / "sccache-perf.ps1"

            result = subprocess.run(
                [
                    shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    str(script),
                    "reset",
                ],
                text=True,
                capture_output=True,
                check=False,
                env=env,
                creationflags=CREATE_NO_WINDOW,
            )
            call_lines = (
                calls.read_text(encoding="utf-8").splitlines() if calls.exists() else []
            )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("sccache --zero-stats failed with exit code 9", result.stderr)
        self.assertEqual(
            call_lines,
            ["--show-stats", "--zero-stats"],
        )

    def test_justfile_bench_and_bazel_fast_paths_are_explicit(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        self.assertIn("bench package bench_name *args:", justfile)
        self.assertIn("bench-workspace *args:", justfile)
        self.assertIn("Pass explicit Bazel test targets.", justfile)
        self.assertIn("target-optimize-dry-run *args:", justfile)
        self.assertIn("app-server-runtime-check:", justfile)
        self.assertIn("app-server-command-exec-check:", justfile)
        self.assertIn("app-server-process-exec-check:", justfile)
        self.assertIn("app-server-thread-status-check:", justfile)
        self.assertIn("app-server-schema-protocol-check:", justfile)
        self.assertIn("app-server-schema-check:", justfile)
        self.assertIn("app-server-schema-check-force:", justfile)
        self.assertIn("cargo nextest run -p codex-app-server-protocol -E", justfile)

    def test_agents_current_nested_instruction_layout_is_explicit(
        self,
    ) -> None:
        expected_agent_files = [
            ".codex/AGENTS.md",
            "AGENTS.md",
            "codex-rs/AGENTS.md",
            "codex-rs/core/AGENTS.md",
            "codex-rs/prompts/AGENTS.md",
            "codex-rs/protocol/AGENTS.md",
            "codex-rs/shell-command/AGENTS.md",
            "codex-rs/tui/src/bottom_pane/AGENTS.md",
            "scripts/AGENTS.md",
            "scripts/codex_package/AGENTS.md",
            "scripts/install/AGENTS.md",
        ]
        actual_agent_files = sorted(
            path.relative_to(REPO_ROOT).as_posix()
            for path in REPO_ROOT.rglob("AGENTS.md")
            if ".git" not in path.parts
        )
        root_text = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")
        normalized_root = " ".join(root_text.split())

        self.assertEqual(actual_agent_files, sorted(expected_agent_files))
        self.assertIn("further nested files apply only where present", normalized_root)
        self.assertIn(
            "Never rely on an instruction file that is absent", normalized_root
        )

    def test_rust_build_doctor_reports_cache_linker_and_contention(self) -> None:
        report = rust_build_status.build_doctor_report(
            repo_root=REPO_ROOT,
            processes=[
                rust_build_status.RustProcess(
                    pid=42,
                    name="cargo.exe",
                    command_line="cargo nextest run -p codex-core",
                ),
                rust_build_status.RustProcess(
                    pid=43,
                    name="rustc.exe",
                    command_line="rustc --out-dir codex-rs\\target\\lanes\\ui\\debug",
                ),
            ],
            tool_lookup=lambda name: (
                f"C:/tools/{name}.exe" if name == "sccache" else None
            ),
            env={},
        )

        self.assertIn("sccache: C:/tools/sccache.exe", report)
        self.assertIn(
            "MSVC linker config x86_64-pc-windows-msvc: (unset)",
            report,
        )
        self.assertIn(
            "MSVC linker config aarch64-pc-windows-msvc: (unset)",
            report,
        )
        self.assertIn("active Rust jobs: 2 total, 1 shared-target, 1 lane", report)
        self.assertIn(
            "shared-target jobs are active; prefer `just test-lane-fast <lane> ...`",
            report,
        )

    def test_windows_process_discovery_uses_cim_filter(self) -> None:
        with mock.patch.object(rust_build_status.subprocess, "run") as run:
            run.return_value.stdout = "[]"

            self.assertEqual(rust_build_status.active_rust_processes_windows(), [])

        command = run.call_args.args[0][-1]
        self.assertIn("Get-CimInstance Win32_Process -Filter", command)
        self.assertIn("Name = 'cargo.exe'", command)
        self.assertIn("Name = 'pwsh.exe'", command)
        self.assertNotIn("Where-Object", command)

    def test_posix_process_matching_ignores_cargo_substrings(self) -> None:
        self.assertFalse(
            rust_build_status.is_rust_process(
                rust_build_status.RustProcess(
                    pid=1,
                    name="editor",
                    command_line="editor /repo/codex-rs/Cargo.toml",
                )
            )
        )
        self.assertTrue(
            rust_build_status.is_rust_process(
                rust_build_status.RustProcess(
                    pid=2,
                    name="sh",
                    command_line="sh -c 'cargo test'",
                )
            )
        )

    def test_target_disk_report_warns_when_target_exceeds_budget(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            target = repo_root / "codex-rs" / "target" / "debug"
            target.mkdir(parents=True)
            (target / "artifact.bin").write_bytes(b"abcd")

            report = rust_build_status.target_disk_report(
                repo_root=repo_root,
                warn_bytes=3,
            )

        self.assertIn("target disk: 4 B", report)
        self.assertIn("target disk warning:", report)
        self.assertIn("just target-prune", report)

    def test_target_disk_report_flags_stray_cargo_target_dirs(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            target_root = repo_root / "codex-rs" / "target"
            stray_debug = target_root / "codex-core-registry-check" / "debug"
            protected = target_root / "dev-small"
            ambiguous = target_root / "schema-probe-plan"
            for cargo_dir in (stray_debug, protected):
                (cargo_dir / ".fingerprint").mkdir(parents=True)
                (cargo_dir / "deps").mkdir()
                (cargo_dir / "build").mkdir()
                (cargo_dir / "incremental").mkdir()
            ambiguous.mkdir()

            report = rust_build_status.target_disk_report(
                repo_root=repo_root,
                warn_bytes=100,
            )

        self.assertIn("stray cargo target dirs: codex-core-registry-check", report)
        self.assertIn("just cargo-lane <lane>", report)
        self.assertNotIn("dev-small", report)
        self.assertNotIn("schema-probe-plan", report)

    def test_prune_stray_target_dirs_removes_read_only_trees(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            stray_root = (
                repo_root / "codex-rs" / "target" / "codex-tools-responses-check"
            )
            stray_debug = stray_root / "debug"
            (stray_debug / ".fingerprint").mkdir(parents=True)
            (stray_debug / "deps").mkdir()
            (stray_debug / "build").mkdir()
            read_only_file = stray_debug / "deps" / "artifact.rlib"
            read_only_file.write_text("artifact", encoding="utf-8")
            read_only_file.chmod(0o400)

            removed = rust_build_status.prune_stray_cargo_target_dirs(
                repo_root=repo_root,
            )

        self.assertEqual(
            [path.name for path in removed], ["codex-tools-responses-check"]
        )
        self.assertFalse(stray_root.exists())

    def test_prune_stale_lanes_removes_only_inactive_lanes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            stale_lane = lane_root / "stale"
            active_lane = lane_root / "active"
            stale_lane.mkdir(parents=True)
            active_lane.mkdir(parents=True)
            (stale_lane / "artifact.txt").write_text("stale", encoding="utf-8")
            (active_lane / "artifact.txt").write_text("active", encoding="utf-8")

            removed = rust_build_status.prune_stale_lanes(
                repo_root=repo_root,
                processes=[
                    rust_build_status.RustProcess(
                        pid=7,
                        name="rustc.exe",
                        command_line=f"rustc --out-dir {active_lane}\\debug",
                    )
                ],
                keep_warm_per_base=0,
                max_age_days=None,
            )

            self.assertEqual([path.name for path in removed], ["stale"])
            self.assertFalse(stale_lane.exists())
            self.assertTrue(active_lane.exists())

    def test_locked_lane_is_active_and_not_pruned(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            locked = lane_root / "locked"
            stale = lane_root / "stale"
            locked.mkdir(parents=True)
            stale.mkdir(parents=True)

            with mock.patch.object(
                rust_build_status,
                "cargo_lock_is_busy",
                side_effect=lambda path: path.name == "locked",
            ):
                snapshot = rust_build_status.BuildStatusSnapshot.collect(
                    repo_root=repo_root,
                    processes=[],
                )
                removed = rust_build_status.prune_stale_lanes(
                    repo_root=repo_root,
                    snapshot=snapshot,
                    keep_warm_per_base=0,
                    max_age_days=None,
                )

            self.assertIn("locked", snapshot.active_lanes)
            self.assertEqual([path.name for path in removed], ["stale"])
            self.assertTrue(locked.exists())
            self.assertFalse(stale.exists())

    def test_unreadable_lock_files_are_treated_as_busy(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            lane = Path(temp_dir)
            with mock.patch.object(Path, "stat", side_effect=PermissionError("denied")):
                self.assertTrue(rust_build_status.cargo_lock_is_busy(lane))
                self.assertTrue(rust_build_status.lane_active_lock_is_held(lane))

    def test_prune_rechecks_lane_lock_before_delete(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            lane = lane_root / "late-busy"
            lane.mkdir(parents=True)
            snapshot = rust_build_status.BuildStatusSnapshot.collect(
                repo_root=repo_root,
                processes=[],
            )

            with mock.patch.object(
                rust_build_status,
                "cargo_lock_is_busy",
                return_value=True,
            ):
                removed = rust_build_status.prune_stale_lanes(
                    repo_root=repo_root,
                    snapshot=snapshot,
                    keep_warm_per_base=0,
                    max_age_days=None,
                )

            self.assertEqual(removed, [])
            self.assertTrue(lane.exists())

    def test_prune_rechecks_active_reservation_before_delete(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane = repo_root / "codex-rs" / "target" / "lanes" / "late-reserved"
            lane.mkdir(parents=True)
            snapshot = rust_build_status.BuildStatusSnapshot.collect(
                repo_root=repo_root,
                processes=[],
            )

            with mock.patch.object(
                rust_build_status, "lane_active_lock_is_held", return_value=True
            ):
                removed = rust_build_status.prune_stale_lanes(
                    repo_root=repo_root,
                    snapshot=snapshot,
                    keep_warm_per_base=0,
                    max_age_days=None,
                )

            self.assertEqual(removed, [])
            self.assertTrue(lane.exists())

    def test_prune_skips_path_that_becomes_indirect(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane = repo_root / "codex-rs" / "target" / "lanes" / "racy"
            lane.mkdir(parents=True)

            with (
                mock.patch.object(
                    rust_build_status, "prunable_lane_dirs", return_value=[lane]
                ),
                mock.patch.object(
                    rust_build_status,
                    "is_indirect_directory",
                    side_effect=[False, True],
                ),
                mock.patch.object(
                    rust_build_status, "cargo_lock_is_busy", return_value=False
                ),
                mock.patch.object(
                    rust_build_status,
                    "lane_active_lock_is_held",
                    return_value=False,
                ),
            ):
                removed = rust_build_status.prune_stale_lanes(
                    repo_root=repo_root,
                    keep_warm_per_base=0,
                    max_age_days=None,
                )

            self.assertEqual(removed, [])
            self.assertTrue(lane.exists())

    def test_prune_strays_skips_indirect_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            stray = repo_root / "codex-rs" / "target" / "stray"
            stray.mkdir(parents=True)

            with (
                mock.patch.object(
                    rust_build_status, "stray_cargo_target_dirs", return_value=[stray]
                ),
                mock.patch.object(
                    rust_build_status, "is_indirect_directory", return_value=True
                ),
            ):
                removed = rust_build_status.prune_stray_cargo_target_dirs(
                    repo_root=repo_root
                )

            self.assertEqual(removed, [])
            self.assertTrue(stray.exists())

    def test_prune_stale_lanes_keeps_two_newest_warm_lanes_per_base(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            newest = lane_root / "codex-core"
            middle = lane_root / "codex-core-2"
            oldest = lane_root / "codex-core-3"
            for lane in (newest, middle, oldest):
                lane.mkdir(parents=True)
                (lane / "artifact.txt").write_text(lane.name, encoding="utf-8")

            removed = rust_build_status.prune_stale_lanes(
                repo_root=repo_root,
                processes=[],
                keep_warm_per_base=2,
            )

            self.assertEqual([path.name for path in removed], ["codex-core-3"])
            self.assertTrue(newest.exists())
            self.assertTrue(middle.exists())
            self.assertFalse(oldest.exists())

    def test_prune_stale_lanes_removes_timestamped_lanes_even_with_warm_budget(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            stable = lane_root / "codex-core"
            timestamped = lane_root / "codex-core-20260608183755"
            stable.mkdir(parents=True)
            timestamped.mkdir(parents=True)

            removed = rust_build_status.prune_stale_lanes(
                repo_root=repo_root,
                processes=[],
                keep_warm_per_base=2,
            )

            self.assertEqual([path.name for path in removed], [timestamped.name])
            self.assertTrue(stable.exists())
            self.assertFalse(timestamped.exists())

    def test_prune_stale_lanes_removes_lanes_over_age_budget(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            old = lane_root / "old"
            fresh = lane_root / "fresh"
            old.mkdir(parents=True)
            fresh.mkdir(parents=True)
            old_time = 1_700_000_000
            fresh_time = 1_700_086_400
            for lane in (old, fresh):
                (lane / "artifact.txt").write_text(lane.name, encoding="utf-8")
            old.touch()
            fresh.touch()

            removed = rust_build_status.prune_stale_lanes(
                repo_root=repo_root,
                processes=[],
                keep_warm_per_base=2,
                max_age_days=1,
                now_timestamp=fresh_time + 1,
                lane_mtime=lambda path: old_time if path.name == "old" else fresh_time,
            )

            self.assertEqual([path.name for path in removed], ["old"])
            self.assertFalse(old.exists())
            self.assertTrue(fresh.exists())

    def test_prune_stale_lanes_applies_warm_budget_before_size_scan(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            protected = lane_root / "codex-core"
            pruned_by_warm_budget = lane_root / "codex-core-2"
            protected.mkdir(parents=True)
            pruned_by_warm_budget.mkdir(parents=True)
            size_calls: list[str] = []

            def lane_size(path: Path) -> tuple[int, int]:
                size_calls.append(path.name)
                return 0, 0

            removed = rust_build_status.prune_stale_lanes(
                repo_root=repo_root,
                processes=[],
                keep_warm_per_base=1,
                max_lane_bytes=1,
                lane_size=lane_size,
            )

            self.assertEqual([path.name for path in removed], ["codex-core-2"])
            self.assertEqual(size_calls, ["codex-core"])

    def test_prune_report_can_skip_disk_scan(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            (lane_root / "stale").mkdir(parents=True)

            report = rust_build_status.prune_stale_lanes_report(
                repo_root=repo_root,
                processes=[],
                dry_run=True,
                keep_warm_per_base=0,
                max_age_days=None,
                include_disk_report=False,
            )

        self.assertIn("would prune:", report)
        self.assertNotIn("target root:", report)

    def test_lane_size_workers_are_capped(self) -> None:
        self.assertEqual(rust_build_status.bounded_size_workers(99, 10), 4)
        self.assertEqual(rust_build_status.bounded_size_workers(2, 1), 1)

    def test_prune_cli_rejects_destructive_negative_budgets(self) -> None:
        for option, value in (
            ("--keep-warm-per-base", "-1"),
            ("--max-age-days", "-1"),
            ("--max-lane-gib", "-1"),
            ("--max-lane-bytes", "-1"),
            ("--size-workers", "0"),
        ):
            with (
                self.subTest(option=option),
                contextlib.redirect_stderr(io.StringIO()),
                self.assertRaises(SystemExit),
            ):
                rust_build_status.main(["prune", option, value])

    def test_lane_regexes_use_shared_tooling_patterns(self) -> None:
        self.assertEqual(
            rust_build_status.LANE_RE.pattern,
            tool_versions.LANE_PATH_PATTERN,
        )
        self.assertEqual(
            rust_build_status.JUST_LANE_RE.pattern,
            tool_versions.JUST_LANE_PATTERN,
        )

    def test_new_local_lane_recipes_are_detected_from_just_commands(self) -> None:
        cargo_lane_text = (REPO_ROOT / "scripts" / "cargo-lane.ps1").read_text(
            encoding="utf-8"
        )
        self.assertIn("watch-lane", cargo_lane_text)
        self.assertIn("coverage-lane", cargo_lane_text)

        for command in (
            "just watch-lane codex-core",
            "just coverage-lane codex-core",
        ):
            self.assertEqual(
                rust_build_status.lane_name_for_process(
                    rust_build_status.RustProcess(
                        pid=99,
                        name="just.exe",
                        command_line=command,
                    )
                ),
                "codex-core",
            )

    def test_lane_report_marks_active_lanes_and_emits_safe_prune_suggestions(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            stale_lane = lane_root / "stale"
            prunable_lane = lane_root / "stale-2"
            active_lane = lane_root / "active"
            stale_lane.mkdir(parents=True)
            prunable_lane.mkdir(parents=True)
            active_lane.mkdir(parents=True)
            (stale_lane / "artifact.txt").write_text("stale", encoding="utf-8")

            report = rust_build_status.lane_report(
                repo_root=repo_root,
                processes=[
                    rust_build_status.RustProcess(
                        pid=7,
                        name="rustc.exe",
                        command_line=f"rustc --out-dir {active_lane}\\debug",
                    )
                ],
            )

        self.assertIn("active: active", report)
        self.assertIn("stale: stale", report)
        self.assertIn("warm-protected: stale", report)
        self.assertIn("prunable:", report)
        self.assertIn("stale-2", report)
        self.assertIn("safe prune suggestions:", report)
        self.assertIn("Remove-Item -Recurse -LiteralPath", report)
        self.assertNotIn("active\\debug", report)


if __name__ == "__main__":
    unittest.main()
