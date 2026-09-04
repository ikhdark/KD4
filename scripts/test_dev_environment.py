#!/usr/bin/env python3

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path


class DevEnvironmentDoctorCliTest(unittest.TestCase):
    """Exercise development-tool checks through the supported CLI process."""

    maxDiff = None

    @classmethod
    def setUpClass(cls) -> None:
        super().setUpClass()
        cls._native_tool_dir = tempfile.TemporaryDirectory()
        native_root = Path(cls._native_tool_dir.name)
        source = native_root / "fake_tool.rs"
        source.write_text(
            r"""
use std::env;
use std::path::Path;
use std::process;

fn main() {
    let executable = env::current_exe().expect("resolve fake tool executable");
    let name = Path::new(&executable)
        .file_stem()
        .expect("fake tool has a file stem")
        .to_string_lossy()
        .to_ascii_uppercase()
        .replace('-', "_");
    let stdout_key = format!("FAKE_{name}_STDOUT");
    let stderr_key = format!("FAKE_{name}_STDERR");
    let exit_key = format!("FAKE_{name}_EXIT");
    if let Ok(value) = env::var(stdout_key) {
        if !value.is_empty() {
            println!("{value}");
        }
    }
    if let Ok(value) = env::var(stderr_key) {
        if !value.is_empty() {
            eprintln!("{value}");
        }
    }
    let exit_code = env::var(exit_key)
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(0);
    process::exit(exit_code);
}
""".lstrip(),
            encoding="utf-8",
        )
        cls._native_tool = native_root / "fake-tool.exe"
        compiled = subprocess.run(
            ["rustc", "--edition=2021", str(source), "-o", str(cls._native_tool)],
            text=True,
            capture_output=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            timeout=60,
        )
        if compiled.returncode != 0:
            cls._native_tool_dir.cleanup()
            raise AssertionError(
                "could not compile native development-tool fixture\n"
                f"stdout:\n{compiled.stdout}\nstderr:\n{compiled.stderr}"
            )

    @classmethod
    def tearDownClass(cls) -> None:
        cls._native_tool_dir.cleanup()
        super().tearDownClass()

    def _prepare_runtime(
        self,
        root: Path,
        *,
        package_manager: str = "pnpm@10.12.4",
    ) -> tuple[Path, Path, dict[str, str]]:
        scripts_dir = root / "scripts"
        scripts_dir.mkdir(parents=True)
        script = scripts_dir / "dev_env_doctor.py"
        shutil.copy2(Path(__file__).with_name("dev_env_doctor.py"), script)
        (root / "package.json").write_text(
            json.dumps({"packageManager": package_manager}),
            encoding="utf-8",
        )

        fake_bin = root / "fake-bin"
        fake_bin.mkdir()
        for name in ("git", "cargo", "just", "uv", "node", "pnpm", "pwsh"):
            shutil.copy2(self._native_tool, fake_bin / f"{name}.exe")

        env = os.environ.copy()
        env["PATH"] = str(fake_bin) + os.pathsep + env.get("PATH", "")
        env["PYTHONIOENCODING"] = "utf-8"
        env.update(
            {
                "FAKE_GIT_STDOUT": "git version 2.49.0",
                "FAKE_CARGO_STDOUT": "cargo 1.89.0",
                "FAKE_JUST_STDOUT": "just 1.40.0",
                "FAKE_UV_STDOUT": "uv 0.8.0",
                "FAKE_NODE_STDOUT": "v22.13.1",
                "FAKE_PNPM_STDOUT": package_manager.split("@", 1)[1].split("+", 1)[0],
                "FAKE_PWSH_STDOUT": "7.4.0",
            }
        )
        env.pop("PYTHONEXECUTABLE", None)
        env.pop("__PYVENV_LAUNCHER__", None)
        return script, fake_bin, env

    def _run_cli(
        self,
        script: Path,
        env: dict[str, str],
        *,
        no_fail: bool = False,
    ) -> subprocess.CompletedProcess[str]:
        arguments = [sys.executable, str(script), "--json"]
        if no_fail:
            arguments.append("--no-fail")
        return subprocess.run(
            arguments,
            cwd=script.parent.parent,
            env=env,
            text=True,
            capture_output=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            timeout=30,
        )

    def _checks(
        self, result: subprocess.CompletedProcess[str]
    ) -> dict[str, dict[str, object]]:
        self.assertTrue(result.stdout.strip(), f"stderr:\n{result.stderr}")
        payload = json.loads(result.stdout)
        return {check["name"]: check for check in payload["checks"]}

    def test_cli_reports_every_required_windows_workflow_tool(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            script, _fake_bin, env = self._prepare_runtime(Path(temp_dir))
            result = self._run_cli(script, env)

        self.assertEqual(result.returncode, 0, result.stderr)
        checks = self._checks(result)
        self.assertEqual(
            set(checks),
            {
                "python",
                "git",
                "cargo",
                "rustfmt",
                "clippy",
                "just",
                "cargo-nextest",
                "uv",
                "node",
                "pnpm",
                "pwsh",
            },
        )
        self.assertTrue(all(check["required"] for check in checks.values()))

    def test_cli_enforces_node_version_prefixes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            script, _fake_bin, env = self._prepare_runtime(Path(temp_dir))
            observed: list[tuple[str, bool]] = []
            for version in ("v22.13.1", "node 23.0.0", "not a version"):
                env["FAKE_NODE_STDOUT"] = version
                result = self._run_cli(script, env, no_fail=True)
                observed.append((version, bool(self._checks(result)["node"]["ok"])))

        self.assertEqual(
            observed,
            [("v22.13.1", True), ("node 23.0.0", True), ("not a version", False)],
        )

    def test_cli_accepts_integrity_suffixed_and_plain_package_manager_pins(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            script, _fake_bin, env = self._prepare_runtime(
                root,
                package_manager="pnpm@1.2.3+sha512.deadbeef",
            )
            suffixed = self._run_cli(script, env)
            (root / "package.json").write_text(
                '{"packageManager":"pnpm@4.5.6"}',
                encoding="utf-8",
            )
            env["FAKE_PNPM_STDOUT"] = "4.5.6"
            plain = self._run_cli(script, env)

        self.assertEqual(suffixed.returncode, 0, suffixed.stderr)
        self.assertEqual(plain.returncode, 0, plain.stderr)
        self.assertEqual(self._checks(suffixed)["pnpm"]["version"], "1.2.3")
        self.assertEqual(self._checks(plain)["pnpm"]["version"], "4.5.6")

    def test_cli_reports_malformed_package_json_without_traceback(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            script, _fake_bin, env = self._prepare_runtime(root)
            (root / "package.json").write_text(
                '{"packageManager":',
                encoding="utf-8",
            )
            result = self._run_cli(script, env)

        self.assertEqual(result.returncode, 1)
        self.assertIn("Development environment check failed:", result.stderr)
        self.assertIn(str(root / "package.json"), result.stderr)
        self.assertNotIn("Traceback", result.stderr)

    def test_cli_prefers_tool_stdout_over_stderr_warning(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            script, _fake_bin, env = self._prepare_runtime(Path(temp_dir))
            env["FAKE_PNPM_STDERR"] = "Corepack download warning"
            result = self._run_cli(script, env)

        self.assertEqual(result.returncode, 0, result.stderr)
        pnpm = self._checks(result)["pnpm"]
        self.assertTrue(pnpm["ok"])
        self.assertEqual(pnpm["version"], "10.12.4")

    def test_cli_uses_tool_stderr_when_stdout_is_empty(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            script, _fake_bin, env = self._prepare_runtime(Path(temp_dir))
            env["FAKE_GIT_STDOUT"] = ""
            env["FAKE_GIT_STDERR"] = "git version 2.49.1"
            result = self._run_cli(script, env)

        self.assertEqual(result.returncode, 0, result.stderr)
        git = self._checks(result)["git"]
        self.assertTrue(git["ok"])
        self.assertEqual(git["version"], "git version 2.49.1")

    def test_cli_enforces_python_floor_and_exact_pnpm_pin(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            script, fake_bin, env = self._prepare_runtime(root)
            fake_python = fake_bin / "python.exe"
            shutil.copy2(self._native_tool, fake_python)
            env["PYTHONEXECUTABLE"] = str(fake_python)
            env["FAKE_PYTHON_STDOUT"] = "Python 3.10.9"
            old_python = self._run_cli(script, env, no_fail=True)
            env["FAKE_PYTHON_STDOUT"] = "Python 3.11.9"
            env["FAKE_PNPM_STDOUT"] = "10.12.3"
            wrong_pnpm = self._run_cli(script, env, no_fail=True)

        old_python_checks = self._checks(old_python)
        wrong_pnpm_checks = self._checks(wrong_pnpm)
        self.assertFalse(old_python_checks["python"]["ok"])
        self.assertEqual(old_python_checks["python"]["version"], "Python 3.10.9")
        self.assertTrue(wrong_pnpm_checks["python"]["ok"])
        self.assertFalse(wrong_pnpm_checks["pnpm"]["ok"])
        self.assertEqual(wrong_pnpm_checks["pnpm"]["version"], "10.12.3")


class GitDoctorCliTest(unittest.TestCase):
    """Exercise Git diagnostics through the supported command process."""

    def _initialize_repository(self, root: Path) -> dict[str, str]:
        root.mkdir(parents=True, exist_ok=True)
        initialized = subprocess.run(
            ["git", "init", "--quiet", str(root)],
            check=False,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            timeout=30,
        )
        self.assertEqual(initialized.returncode, 0, initialized.stderr)
        env = os.environ.copy()
        env.update(
            {
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_CONFIG_GLOBAL": os.devnull,
                "GIT_DIR": str(root / ".git"),
                "GIT_WORK_TREE": str(root),
                "PYTHONIOENCODING": "utf-8",
            }
        )
        return env

    def _configure(
        self, root: Path, env: dict[str, str], name: str, value: str
    ) -> None:
        configured = subprocess.run(
            ["git", "-C", str(root), "config", name, value],
            env=env,
            check=False,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            timeout=30,
        )
        self.assertEqual(configured.returncode, 0, configured.stderr)

    def _run_cli(
        self,
        env: dict[str, str],
    ) -> tuple[subprocess.CompletedProcess[str], dict[str, object]]:
        script = Path(__file__).with_name("git_doctor.py").resolve()
        result = subprocess.run(
            [sys.executable, str(script), "--json", "--timeout", "5"],
            env=env,
            check=False,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return result, json.loads(result.stdout)

    def test_cli_reports_windows_path_kind_for_repository(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "repo"
            env = self._initialize_repository(root)
            _result, report = self._run_cli(env)

        self.assertEqual(report["path_kind"], "windows")
        self.assertEqual(Path(str(report["repo_root"])).resolve(), root.resolve())

    def test_cli_recommends_missing_repository_performance_settings(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "repo"
            env = self._initialize_repository(root)
            _result, report = self._run_cli(env)

        recommendations = "\n".join(report["recommendations"])
        self.assertIn("core.fsmonitor", recommendations)
        self.assertIn("core.untrackedCache", recommendations)

    def test_cli_accepts_fsmonitor_hook_path(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "repo"
            env = self._initialize_repository(root)
            hook = r"C:\Program Files\Git\query-watchman.exe"
            self._configure(root, env, "core.fsmonitor", hook)
            self._configure(root, env, "core.untrackedCache", "true")
            _result, report = self._run_cli(env)

        self.assertEqual(report["fsmonitor"], hook)
        self.assertFalse(
            any("core.fsmonitor" in item for item in report["recommendations"])
        )

    def test_cli_reports_only_known_unreadable_pytest_caches_as_local_state(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "repo"
            env = self._initialize_repository(root)
            self._configure(root, env, "core.fsmonitor", "true")
            self._configure(root, env, "core.untrackedCache", "true")
            known_cache = root / "sdk" / "python" / ".pytest_cache"
            known_cache.mkdir(parents=True)
            (root / ".pytest_cache").mkdir()
            (root / "other" / ".pytest_cache").mkdir(parents=True)

            injection = Path(temp_dir) / "python-injection"
            injection.mkdir()
            (injection / "sitecustomize.py").write_text(
                """
import os

_original_scandir = os.scandir
_target = os.path.normcase(os.path.abspath(os.environ["KD4_UNREADABLE_CACHE"]))


def _scandir(path):
    if os.path.normcase(os.path.abspath(path)) == _target:
        raise PermissionError(13, "fixture access denied", os.fspath(path))
    return _original_scandir(path)


os.scandir = _scandir
""".lstrip(),
                encoding="utf-8",
            )
            env["KD4_UNREADABLE_CACHE"] = str(known_cache)
            env["PYTHONPATH"] = str(injection)
            _result, report = self._run_cli(env)

        self.assertEqual(
            report["unreadable_pytest_caches"],
            ["sdk/python/.pytest_cache/"],
        )
        recommendations = "\n".join(report["recommendations"])
        self.assertIn("delete the cache directories", recommendations)
        self.assertIn("not source dirt", recommendations)
        self.assertNotIn("other/.pytest_cache", recommendations)

    def test_cli_preserves_non_ascii_repository_root_from_git(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "Jos\u00e9" / "repo"
            env = self._initialize_repository(root)
            _result, report = self._run_cli(env)

        self.assertEqual(Path(str(report["repo_root"])).resolve(), root.resolve())
        self.assertIn("Jos\u00e9", str(report["repo_root"]))


class VscodeRuntimeProofCliTest(unittest.TestCase):
    """Exercise runtime selection through the supported proof command."""

    def _prepare_cli(self, root: Path) -> tuple[Path, dict[str, str]]:
        scripts_dir = root / "scripts"
        scripts_dir.mkdir(parents=True)
        script = scripts_dir / "vscode_runtime_proof.py"
        shutil.copy2(Path(__file__).with_name("vscode_runtime_proof.py"), script)

        home = root / "home"
        home.mkdir()
        empty_bin = root / "empty-bin"
        empty_bin.mkdir()
        env = os.environ.copy()
        env["HOME"] = str(home)
        env["USERPROFILE"] = str(home)
        env["PATH"] = str(empty_bin)
        env["PYTHONIOENCODING"] = "utf-8"
        return script, env

    def _run_cli(
        self,
        script: Path,
        env: dict[str, str],
    ) -> tuple[subprocess.CompletedProcess[str], list[dict[str, object]]]:
        result = subprocess.run(
            [sys.executable, str(script), "--json", "--no-run-codex"],
            cwd=script.parent.parent,
            env=env,
            text=True,
            capture_output=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        payload = json.loads(result.stdout)
        return result, payload["probes"]

    def test_cli_uses_configured_local_publish_directory_for_desktop_target(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            script, env = self._prepare_cli(root)
            publish_dir = root / "local-publish"
            publish_dir.mkdir()
            target = publish_dir / "codex.exe"
            target.write_bytes(b"local fixture")
            env["CODEX_LOCAL_PUBLISH_DIR"] = str(publish_dir)
            _result, probes = self._run_cli(script, env)

        desktop = next(
            probe for probe in probes if probe["label"] == "desktop-local-target"
        )
        self.assertEqual(Path(str(desktop["path"])), target)
        self.assertTrue(desktop["exists"])
        self.assertIsNone(desktop["version"])

    def test_cli_discovers_extension_candidates_in_sorted_bounded_order(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            script, env = self._prepare_cli(root)
            home = Path(env["USERPROFILE"])
            extension_root = home / ".vscode" / "extensions"
            expected: list[Path] = []
            for index in reversed(range(10)):
                candidate = (
                    extension_root / f"openai.codex-{index:02d}" / "bin" / "codex.exe"
                )
                candidate.parent.mkdir(parents=True)
                candidate.write_bytes(b"extension fixture")
                expected.append(candidate)
            _result, probes = self._run_cli(script, env)

        candidates = [
            Path(str(probe["path"]))
            for probe in probes
            if probe["label"] == "vscode-extension-candidate"
        ]
        self.assertEqual(candidates, sorted(expected)[:8])
        self.assertTrue(all(probe["exists"] for probe in probes[2:]))


class ConfigSchemaCliTest(unittest.TestCase):
    """Exercise config-schema checks through the supported command process."""

    @classmethod
    def setUpClass(cls) -> None:
        super().setUpClass()
        cls._runner_dir = tempfile.TemporaryDirectory()
        runner_root = Path(cls._runner_dir.name)
        source = runner_root / "config_schema_fixture.rs"
        source.write_text(
            r"""
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process;

fn main() {
    let executable = env::current_exe().expect("resolve fixture executable");
    let tool = executable
        .file_stem()
        .expect("fixture executable has a name")
        .to_string_lossy()
        .to_ascii_lowercase();

    if let Ok(log_path) = env::var("KD4_SCHEMA_RUNNER_LOG") {
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .expect("open fixture log");
        write!(log, "{tool}").expect("write tool name");
        for argument in env::args().skip(1) {
            write!(log, "\u{1f}{argument}").expect("write tool argument");
        }
        writeln!(log).expect("finish fixture log line");
    }

    if tool == "cargo" {
        let schema = env::current_dir()
            .expect("resolve cargo working directory")
            .join("core")
            .join("config.schema.json");
        match env::var("KD4_SCHEMA_ACTION").as_deref() {
            Ok("add") | Ok("modify") => {
                fs::create_dir_all(schema.parent().expect("schema parent"))
                    .expect("create schema parent");
                fs::write(&schema, b"fixture schema changed\n").expect("write schema");
            }
            Ok("remove") => {
                if schema.exists() {
                    fs::remove_file(&schema).expect("remove schema");
                }
            }
            _ => {}
        }
    }

    let prefix = tool.to_ascii_uppercase().replace('-', "_");
    if let Ok(output) = env::var(format!("KD4_{prefix}_STDOUT")) {
        if !output.is_empty() {
            println!("{output}");
        }
    }
    if let Ok(output) = env::var(format!("KD4_{prefix}_STDERR")) {
        if !output.is_empty() {
            eprintln!("{output}");
        }
    }
    let code = env::var(format!("KD4_{prefix}_EXIT"))
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(0);
    process::exit(code);
}
""".lstrip(),
            encoding="utf-8",
        )
        cls._fixture_runner = runner_root / "config-schema-fixture.exe"
        compiled = subprocess.run(
            ["rustc", "--edition=2021", str(source), "-o", str(cls._fixture_runner)],
            text=True,
            capture_output=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            timeout=60,
        )
        if compiled.returncode != 0:
            cls._runner_dir.cleanup()
            raise AssertionError(
                "could not compile config-schema fixture runner\n"
                f"stdout:\n{compiled.stdout}\nstderr:\n{compiled.stderr}"
            )

    @classmethod
    def tearDownClass(cls) -> None:
        cls._runner_dir.cleanup()
        super().tearDownClass()

    def _prepare_cli(
        self,
        root: Path,
        *,
        tools: tuple[str, ...] = ("git", "cargo", "just"),
    ) -> tuple[Path, dict[str, str], Path]:
        scripts_dir = root / "scripts"
        scripts_dir.mkdir(parents=True)
        script = scripts_dir / "config_schema_check.py"
        shutil.copy2(Path(__file__).with_name("config_schema_check.py"), script)
        shutil.copy2(
            Path(__file__).with_name("generated_output_lock.py"),
            scripts_dir / "generated_output_lock.py",
        )
        schema = root / "codex-rs" / "core" / "config.schema.json"
        schema.parent.mkdir(parents=True)
        schema.write_text("fixture schema before\n", encoding="utf-8")
        (root / "justfile").write_text("# local fixture\n", encoding="utf-8")

        fake_bin = root / "fake-bin"
        fake_bin.mkdir()
        for tool in tools:
            shutil.copy2(self._fixture_runner, fake_bin / f"{tool}.exe")
        log = root / "runner.log"
        env = os.environ.copy()
        env["PATH"] = str(fake_bin)
        env["PYTHONIOENCODING"] = "utf-8"
        env["KD4_SCHEMA_RUNNER_LOG"] = str(log)
        env.pop("PYTHONPATH", None)
        return script, env, log

    def _run_cli(
        self,
        script: Path,
        env: dict[str, str],
        *arguments: str,
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(script), *arguments],
            cwd=script.parent.parent,
            env=env,
            text=True,
            capture_output=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            timeout=30,
        )

    def _logged_calls(self, log: Path) -> list[list[str]]:
        return [
            line.split("\x1f")
            for line in log.read_text(encoding="utf-8").splitlines()
            if line
        ]

    def test_cli_quotes_justfile_path_with_spaces(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "repository with spaces"
            script, env, _log = self._prepare_cli(root)
            result = self._run_cli(script, env, "--mode", "check")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"'{root / 'justfile'}'", result.stdout)

    def test_cli_detects_added_removed_and_modified_schema_output(self) -> None:
        observed: dict[str, str] = {}
        for action in ("add", "remove", "modify"):
            with (
                self.subTest(action=action),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir) / "repo"
                script, env, _log = self._prepare_cli(root)
                schema = root / "codex-rs" / "core" / "config.schema.json"
                if action == "add":
                    schema.unlink()
                env["KD4_SCHEMA_ACTION"] = action
                result = self._run_cli(
                    script,
                    env,
                    "--mode",
                    "force",
                    "--owner",
                    f"assignment:{action}",
                )
                observed[action] = result.stdout
                self.assertEqual(result.returncode, 0, result.stderr)

        for action, output in observed.items():
            self.assertIn(
                "Generated config schema outputs changed during regeneration",
                output,
                action,
            )
            self.assertIn("codex-rs/core/config.schema.json", output, action)

    def test_cli_sends_utf8_baseline_and_complete_schema_inputs_to_git(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "repo"
            script, env, log = self._prepare_cli(root)
            result = self._run_cli(
                script,
                env,
                "--mode",
                "check",
                "--baseline",
                "r\u00e9f\u00e9rence",
            )
            calls = self._logged_calls(log)

        self.assertEqual(result.returncode, 0, result.stderr)
        git_call = next(call for call in calls if call[0] == "git")
        self.assertEqual(
            git_call[:4], ["git", "diff", "--name-only", "r\u00e9f\u00e9rence"]
        )
        self.assertEqual(
            git_call[4:],
            [
                "--",
                "codex-rs/config/Cargo.toml",
                "codex-rs/config/src",
                "codex-rs/core/Cargo.toml",
                "codex-rs/core/src/config/schema.rs",
                "codex-rs/core/src/config/schema_tests.rs",
                "codex-rs/core/src/bin/config_schema.rs",
                "codex-rs/features/Cargo.toml",
                "codex-rs/features/src",
                "codex-rs/protocol/Cargo.toml",
                "codex-rs/protocol/src",
            ],
        )

    def test_cli_reports_missing_cargo_and_just_without_tracebacks(self) -> None:
        with tempfile.TemporaryDirectory() as cargo_temp:
            cargo_root = Path(cargo_temp) / "repo"
            cargo_script, cargo_env, _log = self._prepare_cli(
                cargo_root,
                tools=("git", "just"),
            )
            missing_cargo = self._run_cli(
                cargo_script,
                cargo_env,
                "--mode",
                "force",
                "--owner",
                "assignment:missing-cargo",
            )

        with tempfile.TemporaryDirectory() as just_temp:
            just_root = Path(just_temp) / "repo"
            just_script, just_env, _log = self._prepare_cli(
                just_root,
                tools=("git", "cargo"),
            )
            missing_just = self._run_cli(
                just_script,
                just_env,
                "--mode",
                "check",
            )

        self.assertEqual(missing_cargo.returncode, 127)
        self.assertIn("Could not run cargo:", missing_cargo.stderr)
        self.assertNotIn("Traceback", missing_cargo.stderr)
        self.assertEqual(missing_just.returncode, 127)
        self.assertIn("Could not run just:", missing_just.stderr)
        self.assertNotIn("Traceback", missing_just.stderr)

    def test_cli_reports_missing_git_and_continues_to_protocol_check(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "repo"
            script, env, log = self._prepare_cli(root, tools=("just",))
            result = self._run_cli(script, env, "--mode", "check")
            calls = self._logged_calls(log)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            "Could not compare config schema inputs with HEAD:",
            result.stderr,
        )
        self.assertNotIn("Traceback", result.stderr)
        self.assertIn(
            [
                "just",
                "--justfile",
                str(root / "justfile"),
                "config-schema-protocol-check",
            ],
            calls,
        )

    def test_cli_rejects_removed_auto_mode(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "repo"
            script, env, _log = self._prepare_cli(root, tools=())
            result = self._run_cli(script, env, "--mode", "auto")

        self.assertEqual(result.returncode, 2)
        self.assertIn("invalid choice: 'auto'", result.stderr)

    def test_force_cli_records_generation_owner_in_repository_lock(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "repo"
            script, env, _log = self._prepare_cli(root)
            result = self._run_cli(
                script,
                env,
                "--mode",
                "force",
                "--owner",
                "assignment:config-owner",
            )
            lock_payload = json.loads(
                (root / ".codex" / "locks" / "generated-output.lock").read_text(
                    encoding="utf-8"
                )
            )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(lock_payload["owner"], "assignment:config-owner")
        self.assertIsInstance(lock_payload["pid"], int)


class GeneratedOutputLockCliTest(unittest.TestCase):
    """Exercise generation locking through the config-schema command."""

    @classmethod
    def setUpClass(cls) -> None:
        super().setUpClass()
        cls._runner_dir = tempfile.TemporaryDirectory()
        runner_root = Path(cls._runner_dir.name)
        source = runner_root / "generation_fixture.rs"
        source.write_text(
            r"""
use std::env;
use std::fs;
use std::path::Path;
use std::process;
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    let executable = env::current_exe().expect("resolve fixture executable");
    let name = executable
        .file_stem()
        .expect("fixture executable has a name")
        .to_string_lossy()
        .to_ascii_lowercase();
    if name == "cargo" {
        if let Ok(signal) = env::var("KD4_LOCK_ACQUIRED_SIGNAL") {
            fs::write(signal, b"cargo started").expect("write acquisition signal");
        }
        if let Ok(release) = env::var("KD4_LOCK_RELEASE_SIGNAL") {
            let started = Instant::now();
            while !Path::new(&release).exists() {
                if started.elapsed() > Duration::from_secs(20) {
                    eprintln!("timed out waiting for release signal");
                    process::exit(97);
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
""".lstrip(),
            encoding="utf-8",
        )
        cls._fixture_runner = runner_root / "generation-fixture.exe"
        compiled = subprocess.run(
            ["rustc", "--edition=2021", str(source), "-o", str(cls._fixture_runner)],
            text=True,
            capture_output=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            timeout=60,
        )
        if compiled.returncode != 0:
            cls._runner_dir.cleanup()
            raise AssertionError(
                "could not compile generated-output fixture runner\n"
                f"stdout:\n{compiled.stdout}\nstderr:\n{compiled.stderr}"
            )

    @classmethod
    def tearDownClass(cls) -> None:
        cls._runner_dir.cleanup()
        super().tearDownClass()

    def _prepare_cli(self, root: Path) -> tuple[Path, dict[str, str]]:
        scripts_dir = root / "scripts"
        scripts_dir.mkdir(parents=True)
        script = scripts_dir / "config_schema_check.py"
        shutil.copy2(Path(__file__).with_name("config_schema_check.py"), script)
        shutil.copy2(
            Path(__file__).with_name("generated_output_lock.py"),
            scripts_dir / "generated_output_lock.py",
        )
        (root / "codex-rs").mkdir()
        (root / "justfile").write_text("# local fixture\n", encoding="utf-8")

        fake_bin = root / "fake-bin"
        fake_bin.mkdir()
        shutil.copy2(self._fixture_runner, fake_bin / "cargo.exe")
        shutil.copy2(self._fixture_runner, fake_bin / "just.exe")
        acquired = root / "cargo-started.signal"
        release = root / "cargo-release.signal"
        env = os.environ.copy()
        env["PATH"] = str(fake_bin)
        env["PYTHONIOENCODING"] = "utf-8"
        env["KD4_LOCK_ACQUIRED_SIGNAL"] = str(acquired)
        env["KD4_LOCK_RELEASE_SIGNAL"] = str(release)
        return script, env

    def _arguments(self, script: Path, owner: str) -> list[str]:
        return [
            sys.executable,
            str(script),
            "--mode",
            "force",
            "--owner",
            owner,
        ]

    def test_config_schema_cli_serializes_owners_and_recovers_after_release(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            script, env = self._prepare_cli(root)
            acquired = Path(env["KD4_LOCK_ACQUIRED_SIGNAL"])
            release = Path(env["KD4_LOCK_RELEASE_SIGNAL"])
            first = subprocess.Popen(
                self._arguments(script, "assignment:owner-a"),
                cwd=root,
                env=env,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                encoding="utf-8",
                errors="replace",
                creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            )
            try:
                deadline = time.monotonic() + 10
                while not acquired.exists() and time.monotonic() < deadline:
                    if first.poll() is not None:
                        break
                    time.sleep(0.01)
                self.assertTrue(acquired.exists(), "first generation never started")

                collision = subprocess.run(
                    self._arguments(script, "assignment:owner-b"),
                    cwd=root,
                    env=env,
                    text=True,
                    capture_output=True,
                    encoding="utf-8",
                    errors="replace",
                    check=False,
                    creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
                    timeout=30,
                )
            finally:
                release.write_text("release\n", encoding="utf-8")
                try:
                    first_stdout, first_stderr = first.communicate(timeout=30)
                except subprocess.TimeoutExpired:
                    first.kill()
                    first_stdout, first_stderr = first.communicate(timeout=10)

            self.assertEqual(first.returncode, 0, first_stderr)
            self.assertIn("Forcing config schema regeneration", first_stdout)
            self.assertEqual(collision.returncode, 2, collision.stdout)
            self.assertIn("generated outputs is already locked", collision.stderr)

            recovered_env = env.copy()
            recovered_env.pop("KD4_LOCK_ACQUIRED_SIGNAL")
            recovered_env.pop("KD4_LOCK_RELEASE_SIGNAL")
            recovered = subprocess.run(
                self._arguments(script, "assignment:owner-b"),
                cwd=root,
                env=recovered_env,
                text=True,
                capture_output=True,
                encoding="utf-8",
                errors="replace",
                check=False,
                creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
                timeout=30,
            )

        self.assertEqual(recovered.returncode, 0, recovered.stderr)
        self.assertIn("Forcing config schema regeneration", recovered.stdout)


class AppServerSchemaRuntimeCliTest(unittest.TestCase):
    """Exercise app-server schema checks through the supported CLI process."""

    @classmethod
    def setUpClass(cls) -> None:
        super().setUpClass()
        cls._runner_dir = tempfile.TemporaryDirectory()
        runner_root = Path(cls._runner_dir.name)
        source = runner_root / "app_server_schema_fixture.rs"
        source.write_text(
            r"""
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::process;

fn env_exit(name: &str) -> i32 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(0)
}

fn main() {
    let executable = env::current_exe().expect("resolve fixture executable");
    let tool = executable
        .file_stem()
        .expect("fixture executable has a name")
        .to_string_lossy()
        .to_ascii_lowercase();
    let arguments: Vec<String> = env::args().skip(1).collect();

    if let Ok(log_path) = env::var("KD4_APP_SCHEMA_RUNNER_LOG") {
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .expect("open fixture log");
        write!(log, "{tool}").expect("write tool name");
        for argument in &arguments {
            write!(log, "\u{1f}{argument}").expect("write tool argument");
        }
        writeln!(log).expect("finish fixture log line");
    }

    if tool == "git" {
        match arguments.first().map(String::as_str) {
            Some("diff") => {
                if let Ok(output) = env::var("KD4_APP_GIT_DIFF_STDOUT") {
                    if !output.is_empty() {
                        println!("{output}");
                    }
                }
                process::exit(env_exit("KD4_APP_GIT_DIFF_EXIT"));
            }
            Some("show") => {
                if let Ok(output) = env::var("KD4_APP_GIT_SHOW_STDOUT") {
                    if !output.is_empty() {
                        println!("{output}");
                    }
                }
                process::exit(env_exit("KD4_APP_GIT_SHOW_EXIT"));
            }
            _ => {}
        }
    }

    if tool == "cargo" && env::var("KD4_APP_SCHEMA_ACTION").as_deref() == Ok("add") {
        let output = env::current_dir()
            .expect("resolve cargo working directory")
            .join("app-server-protocol")
            .join("schema")
            .join("generated-fixture.json");
        fs::create_dir_all(output.parent().expect("generated output parent"))
            .expect("create generated output parent");
        fs::write(output, b"generated during fixture run\n")
            .expect("write generated fixture output");
    }

    let prefix = tool.to_ascii_uppercase().replace('-', "_");
    if let Ok(output) = env::var(format!("KD4_APP_{prefix}_STDOUT")) {
        if !output.is_empty() {
            println!("{output}");
        }
    }
    if let Ok(output) = env::var(format!("KD4_APP_{prefix}_STDERR")) {
        if !output.is_empty() {
            eprintln!("{output}");
        }
    }
    process::exit(env_exit(&format!("KD4_APP_{prefix}_EXIT")));
}
""".lstrip(),
            encoding="utf-8",
        )
        cls._fixture_runner = runner_root / "app-server-schema-fixture.exe"
        compiled = subprocess.run(
            ["rustc", "--edition=2021", str(source), "-o", str(cls._fixture_runner)],
            text=True,
            capture_output=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            timeout=60,
        )
        if compiled.returncode != 0:
            cls._runner_dir.cleanup()
            raise AssertionError(
                "could not compile app-server schema fixture runner\n"
                f"stdout:\n{compiled.stdout}\nstderr:\n{compiled.stderr}"
            )

    @classmethod
    def tearDownClass(cls) -> None:
        cls._runner_dir.cleanup()
        super().tearDownClass()

    def _prepare_cli(
        self,
        root: Path,
        *,
        baseline_schema: object | None = None,
        current_schema: object | None = None,
        tools: tuple[str, ...] = ("git", "cargo", "just", "uv"),
    ) -> tuple[Path, dict[str, str], Path]:
        if baseline_schema is None:
            baseline_schema = {"definitions": {"Request": {"type": "object"}}}
        if current_schema is None:
            current_schema = baseline_schema

        scripts_dir = root / "scripts"
        scripts_dir.mkdir(parents=True)
        script = scripts_dir / "app_server_schema_runtime_check.py"
        shutil.copy2(
            Path(__file__).with_name("app_server_schema_runtime_check.py"),
            script,
        )
        shutil.copy2(
            Path(__file__).with_name("generated_output_lock.py"),
            scripts_dir / "generated_output_lock.py",
        )
        schema = (
            root
            / "codex-rs"
            / "app-server-protocol"
            / "schema"
            / "json"
            / "codex_app_server_protocol.schemas.json"
        )
        schema.parent.mkdir(parents=True)
        schema.write_text(json.dumps(current_schema), encoding="utf-8")
        (root / "justfile").write_text("# local fixture\n", encoding="utf-8")
        (root / "sdk" / "python").mkdir(parents=True)

        fake_bin = root / "fake-bin"
        fake_bin.mkdir()
        for tool in tools:
            shutil.copy2(self._fixture_runner, fake_bin / f"{tool}.exe")
        log = root / "runner.log"
        env = os.environ.copy()
        env["PATH"] = str(fake_bin)
        env["PYTHONIOENCODING"] = "utf-8"
        env["KD4_APP_SCHEMA_RUNNER_LOG"] = str(log)
        env["KD4_APP_GIT_SHOW_STDOUT"] = json.dumps(baseline_schema)
        env.pop("PYTHONPATH", None)
        return script, env, log

    def _run_cli(
        self,
        script: Path,
        env: dict[str, str],
        *arguments: str,
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(script), *arguments],
            cwd=script.parent.parent,
            env=env,
            text=True,
            capture_output=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            timeout=30,
        )

    def _logged_calls(self, log: Path) -> list[list[str]]:
        if not log.exists():
            return []
        return [
            line.split("\x1f")
            for line in log.read_text(encoding="utf-8").splitlines()
            if line
        ]

    def test_cli_sends_utf8_baseline_and_complete_schema_inputs_to_git(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "repo"
            script, env, log = self._prepare_cli(root)
            result = self._run_cli(
                script,
                env,
                "--mode",
                "check",
                "--baseline",
                "r\u00e9f\u00e9rence",
            )
            calls = self._logged_calls(log)

        self.assertEqual(result.returncode, 0, result.stderr)
        diff_call = next(call for call in calls if call[:2] == ["git", "diff"])
        self.assertEqual(
            diff_call[:4], ["git", "diff", "--name-only", "r\u00e9f\u00e9rence"]
        )
        self.assertIn("codex-rs/app-server-protocol/src", diff_call)
        self.assertIn("codex-rs/protocol/src", diff_call)

    def test_cli_reports_missing_git_and_continues_to_protocol_check(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "repo"
            script, env, log = self._prepare_cli(root, tools=("just", "uv"))
            result = self._run_cli(script, env, "--mode", "check")
            calls = self._logged_calls(log)

        self.assertEqual(result.returncode, 2)
        self.assertIn(
            "Could not compare app-server schema inputs with HEAD:", result.stderr
        )
        self.assertIn("Could not read stable schema at HEAD^:", result.stderr)
        self.assertNotIn("Traceback", result.stderr)
        self.assertTrue(any(call[0] == "just" for call in calls))

    def test_cli_reports_missing_cargo_and_just_without_tracebacks(self) -> None:
        with tempfile.TemporaryDirectory() as cargo_temp:
            cargo_root = Path(cargo_temp) / "repo"
            cargo_script, cargo_env, _log = self._prepare_cli(
                cargo_root, tools=("git", "just", "uv")
            )
            missing_cargo = self._run_cli(
                cargo_script,
                cargo_env,
                "--mode",
                "force",
                "--owner",
                "assignment:missing-cargo",
            )

        with tempfile.TemporaryDirectory() as just_temp:
            just_root = Path(just_temp) / "repo"
            just_script, just_env, _log = self._prepare_cli(
                just_root, tools=("git", "cargo", "uv")
            )
            missing_just = self._run_cli(
                just_script,
                just_env,
                "--mode",
                "check",
            )

        self.assertEqual(missing_cargo.returncode, 127)
        self.assertIn("Could not run cargo:", missing_cargo.stderr)
        self.assertNotIn("Traceback", missing_cargo.stderr)
        self.assertEqual(missing_just.returncode, 127)
        self.assertIn("Could not run just:", missing_just.stderr)
        self.assertNotIn("Traceback", missing_just.stderr)

    def test_cli_quotes_justfile_path_with_spaces(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "repository with spaces"
            script, env, _log = self._prepare_cli(root)
            result = self._run_cli(script, env, "--mode", "check")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"'{root / 'justfile'}'", result.stdout)

    def test_force_cli_reports_changed_output_and_still_succeeds(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "repo"
            script, env, log = self._prepare_cli(root)
            env["KD4_APP_SCHEMA_ACTION"] = "add"
            result = self._run_cli(
                script,
                env,
                "--mode",
                "force",
                "--owner",
                "assignment:changed-output",
            )
            calls = self._logged_calls(log)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            "Generated app-server schema outputs changed during regeneration",
            result.stdout,
        )
        self.assertIn(
            "codex-rs/app-server-protocol/schema/generated-fixture.json",
            result.stdout,
        )
        self.assertEqual([call[0] for call in calls], ["cargo", "just", "git", "uv"])

    def test_force_cli_forwards_experimental_generator_arguments(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "repo"
            script, env, log = self._prepare_cli(root)
            result = self._run_cli(
                script,
                env,
                "--mode",
                "force",
                "--owner",
                "assignment:experimental",
                "--",
                "--experimental",
            )
            calls = self._logged_calls(log)

        self.assertEqual(result.returncode, 0, result.stderr)
        cargo_call = next(call for call in calls if call[0] == "cargo")
        self.assertEqual(cargo_call[-2:], ["--", "--experimental"])
        self.assertIn(
            "Skipping stable compatibility comparison for experimental schemas",
            result.stdout,
        )
        self.assertEqual([call[0] for call in calls], ["cargo", "just", "uv"])

    def test_cli_rejects_breaks_but_allows_additive_schema_changes(self) -> None:
        baseline = {
            "definitions": {
                "Request": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"],
                }
            }
        }
        additive = {
            "definitions": {
                "Request": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "tag": {"type": "string"},
                    },
                    "required": ["name"],
                },
                "Response": {"type": "object"},
            }
        }
        breaking = {
            "definitions": {
                "Request": {
                    "type": "object",
                    "properties": {},
                    "required": ["name", "tag"],
                }
            }
        }

        with tempfile.TemporaryDirectory() as additive_temp:
            additive_root = Path(additive_temp) / "repo"
            additive_script, additive_env, _log = self._prepare_cli(
                additive_root,
                baseline_schema=baseline,
                current_schema=additive,
            )
            additive_result = self._run_cli(
                additive_script, additive_env, "--mode", "check"
            )

        with tempfile.TemporaryDirectory() as breaking_temp:
            breaking_root = Path(breaking_temp) / "repo"
            breaking_script, breaking_env, breaking_log = self._prepare_cli(
                breaking_root,
                baseline_schema=baseline,
                current_schema=breaking,
            )
            breaking_result = self._run_cli(
                breaking_script, breaking_env, "--mode", "check"
            )
            breaking_calls = self._logged_calls(breaking_log)

        self.assertEqual(additive_result.returncode, 0, additive_result.stderr)
        self.assertIn("is compatible with HEAD^", additive_result.stdout)
        self.assertEqual(breaking_result.returncode, 1)
        self.assertIn(
            "$/definitions/Request/properties/name:removed",
            breaking_result.stderr,
        )
        self.assertIn("$/definitions/Request/required:changed", breaking_result.stderr)
        self.assertFalse(any(call[0] == "uv" for call in breaking_calls))

    def test_cli_allows_additions_inside_namespaced_definitions(self) -> None:
        baseline = {"definitions": {"v2": {"Existing": {"type": "object"}}}}
        current = {
            "definitions": {
                "v2": {
                    "Existing": {"type": "object"},
                    "Added": {"type": "object"},
                }
            }
        }
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "repo"
            script, env, log = self._prepare_cli(
                root,
                baseline_schema=baseline,
                current_schema=current,
            )
            result = self._run_cli(script, env, "--mode", "check")
            calls = self._logged_calls(log)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("is compatible with HEAD^", result.stdout)
        self.assertTrue(any(call[0] == "uv" for call in calls))

    def test_cli_allows_optional_properties_inside_existing_union_branch_and_rejects_new_union_alternative(
        self,
    ) -> None:
        baseline = {
            "definitions": {
                "v2": {
                    "ThreadItem": {
                        "oneOf": [
                            {
                                "type": "object",
                                "properties": {
                                    "type": {
                                        "type": "string",
                                        "enum": ["commandExecution"],
                                    },
                                    "id": {"type": "string"},
                                    "anyOf": {
                                        "type": "object",
                                        "properties": {
                                            "existing": {"type": "string"},
                                        },
                                        "required": ["existing"],
                                    },
                                },
                                "required": ["id", "type"],
                            }
                        ]
                    }
                }
            }
        }
        additive = json.loads(json.dumps(baseline))
        additive["definitions"]["v2"]["ThreadItem"]["oneOf"][0]["properties"][
            "anyOf"
        ]["properties"]["optionalNested"] = {"type": ["string", "null"]}
        expanded_union = json.loads(json.dumps(additive))
        expanded_union["definitions"]["v2"]["ThreadItem"]["oneOf"].append(
            {
                "type": "object",
                "properties": {
                    "type": {"type": "string", "enum": ["fileChange"]},
                    "id": {"type": "string"},
                },
                "required": ["id", "type"],
            }
        )

        with tempfile.TemporaryDirectory() as additive_temp:
            additive_root = Path(additive_temp) / "repo"
            additive_script, additive_env, additive_log = self._prepare_cli(
                additive_root,
                baseline_schema=baseline,
                current_schema=additive,
            )
            additive_result = self._run_cli(
                additive_script, additive_env, "--mode", "check"
            )
            additive_calls = self._logged_calls(additive_log)

        with tempfile.TemporaryDirectory() as expanded_temp:
            expanded_root = Path(expanded_temp) / "repo"
            expanded_script, expanded_env, expanded_log = self._prepare_cli(
                expanded_root,
                baseline_schema=baseline,
                current_schema=expanded_union,
            )
            expanded_result = self._run_cli(
                expanded_script, expanded_env, "--mode", "check"
            )
            expanded_calls = self._logged_calls(expanded_log)

        self.assertEqual(additive_result.returncode, 0, additive_result.stderr)
        self.assertIn("is compatible with HEAD^", additive_result.stdout)
        self.assertTrue(any(call[0] == "uv" for call in additive_calls))
        self.assertEqual(expanded_result.returncode, 1)
        self.assertIn(
            "$/definitions/v2/ThreadItem/oneOf:changed",
            expanded_result.stderr,
        )
        self.assertFalse(any(call[0] == "uv" for call in expanded_calls))

    def test_cli_composes_protocol_compatibility_and_python_consumer(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "repo"
            script, env, log = self._prepare_cli(root)
            result = self._run_cli(script, env, "--mode", "check")
            calls = self._logged_calls(log)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([call[0] for call in calls], ["git", "just", "git", "uv"])
        self.assertEqual(calls[2], ["git", "show", f"HEAD^:{self._stable_bundle()}"])
        self.assertIn("tests/test_contract_generation.py", calls[3])

    def _stable_bundle(self) -> str:
        return (
            "codex-rs/app-server-protocol/schema/json/"
            "codex_app_server_protocol.schemas.json"
        )

    def test_cli_stops_before_consumer_on_compatibility_failure(self) -> None:
        baseline = {"definitions": {"Request": {"type": "object"}}}
        current = {"definitions": {}}
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "repo"
            script, env, log = self._prepare_cli(
                root,
                baseline_schema=baseline,
                current_schema=current,
            )
            result = self._run_cli(script, env, "--mode", "check")
            calls = self._logged_calls(log)

        self.assertEqual(result.returncode, 1)
        self.assertIn("$/definitions/Request:removed", result.stderr)
        self.assertFalse(any(call[0] == "uv" for call in calls))

    def test_cli_rejects_removed_auto_mode(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "repo"
            script, env, _log = self._prepare_cli(root, tools=())
            result = self._run_cli(script, env, "--mode", "auto")

        self.assertEqual(result.returncode, 2)
        self.assertIn("invalid choice: 'auto'", result.stderr)


if __name__ == "__main__":
    unittest.main()
