#!/usr/bin/env python3

import io
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

from scripts import tool_versions
from scripts.build_tooling_test_support import REPO_ROOT
from scripts.build_tooling_test_support import load_format_module
from scripts.build_tooling_test_support import load_just_shell_module
from scripts.build_tooling_test_support import load_toml


class BuildToolingEnvironmentTest(unittest.TestCase):
    def run_supported_command(
        self,
        command: list[str],
        *,
        cwd: Path = REPO_ROOT,
        env: dict[str, str] | None = None,
        timeout: int = 30,
    ) -> subprocess.CompletedProcess[str]:
        result = subprocess.run(
            command,
            cwd=cwd,
            env=env,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            timeout=timeout,
        )
        self.assertEqual(
            result.returncode,
            0,
            f"command: {command!r}\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        return result

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

    def test_ci_does_not_override_rust_wrapper_or_linker(self) -> None:
        just_shell = load_just_shell_module()

        updates = just_shell.rust_tool_env(
            {"CI": "true"},
            which=lambda program: f"C:/tools/{program}.exe",
        )

        self.assertEqual(updates, {})

    def test_just_shell_defaults_python_cpu_count_when_unavailable(self) -> None:
        just_shell = load_just_shell_module()

        with mock.patch.object(just_shell.os, "cpu_count", return_value=None):
            updates = just_shell.python_cpu_env({})

        self.assertEqual(updates, {"PYTHON_CPU_COUNT": "1"})

    _fake_tool_temp = None
    _fake_tool_binary = None

    @classmethod
    def _compiled_fake_tool(cls) -> Path:
        if cls._fake_tool_binary is not None:
            return cls._fake_tool_binary

        cls._fake_tool_temp = tempfile.TemporaryDirectory(prefix="just-shell-tools-")
        build_dir = Path(cls._fake_tool_temp.name)
        source = build_dir / "fake_tool.rs"
        binary = build_dir / ("fake-tool.exe" if os.name == "nt" else "fake-tool")
        source.write_text(
            r'''use std::env;
use std::fmt::Write as FmtWrite;
use std::fs::{self, OpenOptions};
use std::io::Write as IoWrite;
use std::path::PathBuf;
use std::process::exit;
use std::thread;
use std::time::Duration;

fn hex(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value.as_bytes() {
        write!(&mut encoded, "{byte:02x}").unwrap();
    }
    encoded
}

fn record(role: &str, args: &[String]) {
    let Some(path) = env::var_os("KD4_TEST_TOOL_RECORD") else {
        return;
    };
    let mut line = hex(role);
    for arg in args {
        write!(&mut line, "\tA:{}", hex(arg)).unwrap();
    }
    for key in [
        "PATH",
        "VIRTUAL_ENV",
        "VIRTUAL_ENV_DISABLE_PROMPT",
        "PYTHONHOME",
        "PYTHON_CPU_COUNT",
        "CARGO_NET_GIT_FETCH_WITH_CLI",
        "RUSTC_WRAPPER",
        "SCCACHE_BASEDIR",
        "SCCACHE_CACHE_SIZE",
        "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER",
        "CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER",
    ] {
        if let Ok(value) = env::var(key) {
            write!(&mut line, "\tE:{key}:{}", hex(&value)).unwrap();
        }
    }
    line.push('\n');
    let mut output = OpenOptions::new()
        .create(true)
        .append(true)
        .open(PathBuf::from(path))
        .unwrap();
    output.write_all(line.as_bytes()).unwrap();
}

fn status_from_env(key: &str) -> i32 {
    env::var(key).ok().and_then(|value| value.parse().ok()).unwrap_or(0)
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let executable = env::current_exe().unwrap();
    let role = executable
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("fake-tool")
        .to_ascii_lowercase();
    record(&role, &args);

    if role.contains("pwsh") {
        if args.iter().any(|arg| arg == "-Command") {
            match env::var("KD4_TEST_PWSH_PROBE_MODE").as_deref() {
                Ok("old") => exit(1),
                Ok("timeout") => {
                    thread::sleep(Duration::from_secs(4));
                    if let Some(path) = env::var_os("KD4_TEST_PWSH_TIMEOUT_MARKER") {
                        fs::write(PathBuf::from(path), b"probe completed").unwrap();
                    }
                }
                _ => {}
            }
        }
        exit(status_from_env("KD4_TEST_PWSH_EXIT"));
    }

    if role.contains("sccache") {
        match args.first().map(String::as_str) {
            Some("--show-stats") => {
                let state_path = env::var_os("KD4_TEST_SCCACHE_STATE").map(PathBuf::from);
                let index = state_path
                    .as_ref()
                    .and_then(|path| fs::read_to_string(path).ok())
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                let sequence = env::var("KD4_TEST_SCCACHE_STATS")
                    .unwrap_or_else(|_| "80 GiB".to_string());
                let values: Vec<&str> = sequence.split('|').collect();
                let value = values
                    .get(index)
                    .or_else(|| values.last())
                    .copied()
                    .unwrap_or("80 GiB");
                if let Some(path) = state_path {
                    fs::write(path, (index + 1).to_string()).unwrap();
                }
                println!("Max cache size                       {value}");
                exit(status_from_env("KD4_TEST_SCCACHE_STATS_EXIT"));
            }
            Some("--start-server") => exit(status_from_env("KD4_TEST_SCCACHE_START_EXIT")),
            Some("--stop-server") => exit(status_from_env("KD4_TEST_SCCACHE_STOP_EXIT")),
            _ => exit(0),
        }
    }
}
''',
            encoding="utf-8",
            newline="\n",
        )
        result = subprocess.run(
            ["rustc", "--edition=2021", str(source), "-o", str(binary)],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            timeout=120,
        )
        if result.returncode != 0:
            raise AssertionError(
                "failed to compile just-shell fake tool:\n"
                f"{result.stdout}\n{result.stderr}"
            )
        cls._fake_tool_binary = binary
        return binary

    @staticmethod
    def _tool_filename(name: str) -> str:
        return f"{name}.exe" if os.name == "nt" else name

    @classmethod
    def _install_fake_tools(cls, bin_dir: Path, *names: str) -> dict[str, Path]:
        bin_dir.mkdir(parents=True, exist_ok=True)
        source = cls._compiled_fake_tool()
        installed = {}
        for name in names:
            destination = bin_dir / cls._tool_filename(name)
            destination.write_bytes(source.read_bytes())
            if os.name != "nt":
                destination.chmod(destination.stat().st_mode | 0o111)
            installed[name] = destination.resolve()
        return installed

    @staticmethod
    def _prepare_just_shell_repo(root: Path) -> tuple[Path, Path]:
        scripts_dir = root / "scripts"
        scripts_dir.mkdir(parents=True)
        (root / "codex-rs").mkdir()
        for name in ("just-shell.py", "rust_tool_env.py"):
            source = REPO_ROOT / "scripts" / name
            (scripts_dir / name).write_bytes(source.read_bytes())
        return (
            scripts_dir / "just-shell.py",
            root / "codex-rs" / "target" / "just-shell",
        )

    @staticmethod
    def _just_shell_env(bin_dir: Path, record_path: Path) -> dict[str, str]:
        retained = (
            "COMSPEC",
            "PATHEXT",
            "SYSTEMDRIVE",
            "SYSTEMROOT",
            "TEMP",
            "TMP",
            "WINDIR",
        )
        env = {key: os.environ[key] for key in retained if key in os.environ}
        env.update(
            {
                "KD4_TEST_TOOL_RECORD": str(record_path),
                "PATH": str(bin_dir),
                "PYTHONUTF8": "1",
            }
        )
        return env

    @staticmethod
    def _run_just_shell(
        script: Path,
        env: dict[str, str],
        command: str = "Write-Output ready",
        recipe: str = "integration-test",
        *args: str,
        python_args: tuple[str, ...] = (),
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                sys.executable,
                "-E",
                *python_args,
                str(script),
                command,
                recipe,
                *args,
            ],
            cwd=script.parents[1],
            env=env,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            timeout=15,
        )

    @staticmethod
    def _decode_tool_records(path: Path) -> list[dict[str, object]]:
        if not path.exists():
            return []

        def decode(value: str) -> str:
            return bytes.fromhex(value).decode("utf-8")

        records = []
        for line in path.read_text(encoding="utf-8").splitlines():
            fields = line.split("\t")
            record: dict[str, object] = {
                "role": decode(fields[0]),
                "args": [],
                "env": {},
            }
            for field in fields[1:]:
                kind, payload = field.split(":", 1)
                if kind == "A":
                    record["args"].append(decode(payload))
                else:
                    key, value = payload.split(":", 1)
                    record["env"][key] = decode(value)
            records.append(record)
        return records

    def _assert_just_shell_success(
        self, result: subprocess.CompletedProcess[str]
    ) -> None:
        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )

    def _actual_pwsh_record(
        self, records: list[dict[str, object]]
    ) -> dict[str, object]:
        matches = [
            record
            for record in records
            if "pwsh" in record["role"] and "-CommandWithArgs" in record["args"]
        ]
        self.assertEqual(matches, matches[-1:], records)
        return matches[0]

    def test_just_shell_cli_applies_rust_sccache_environment_and_respects_overrides(
        self,
    ) -> None:
        cases = (
            ("default", {}, "80G", True),
            ("cache-size-override", {"CODEX_SCCACHE_CACHE_SIZE": "100G"}, "100G", True),
            (
                "blank-cache-size-override",
                {"CODEX_SCCACHE_CACHE_SIZE": "   "},
                "80G",
                True,
            ),
            ("existing-sccache-wrapper", {"RUSTC_WRAPPER": "sccache"}, "80G", True),
            ("existing-wrapper", {"RUSTC_WRAPPER": "custom-wrapper"}, None, False),
        )
        for label, overrides, expected_size, expect_sccache in cases:
            with self.subTest(label=label), tempfile.TemporaryDirectory(
                prefix=f"just-shell-rust-{label}-"
            ) as temp_dir:
                root = Path(temp_dir)
                script, _cache_dir = self._prepare_just_shell_repo(root)
                bin_dir = root / "bin"
                tools = self._install_fake_tools(bin_dir, "pwsh", "sccache")
                record_path = root / "tool-records.tsv"
                env = self._just_shell_env(bin_dir, record_path)
                env.update(overrides)
                env["KD4_TEST_SCCACHE_STATS"] = (
                    f"{expected_size[:-1]} GiB" if expected_size else "80 GiB"
                )

                result = self._run_just_shell(
                    script, env, "cargo check {args}", "check", "--workspace"
                )

                self._assert_just_shell_success(result)
                records = self._decode_tool_records(record_path)
                actual = self._actual_pwsh_record(records)
                actual_env = actual["env"]
                self.assertEqual(actual["args"][-2:], ["check", "--workspace"])
                self.assertIn("@($args | Select-Object -Skip 1)", actual["args"][-3])
                self.assertEqual(actual_env["CARGO_NET_GIT_FETCH_WITH_CLI"], "true")
                if expect_sccache:
                    expected_wrapper = overrides.get(
                        "RUSTC_WRAPPER", str(tools["sccache"])
                    )
                    if os.path.isabs(expected_wrapper):
                        self.assertEqual(
                            os.path.normcase(actual_env["RUSTC_WRAPPER"]),
                            os.path.normcase(expected_wrapper),
                        )
                    else:
                        self.assertEqual(actual_env["RUSTC_WRAPPER"], expected_wrapper)
                    self.assertEqual(actual_env["SCCACHE_BASEDIR"], str(root.resolve()))
                    self.assertEqual(actual_env["SCCACHE_CACHE_SIZE"], expected_size)
                    self.assertTrue(
                        any(record["role"].startswith("sccache") for record in records),
                        records,
                    )
                else:
                    self.assertEqual(actual_env["RUSTC_WRAPPER"], "custom-wrapper")
                    self.assertNotIn("SCCACHE_BASEDIR", actual_env)
                    self.assertNotIn("SCCACHE_CACHE_SIZE", actual_env)
                    self.assertFalse(
                        any(record["role"].startswith("sccache") for record in records),
                        records,
                    )

    def test_just_shell_cli_manages_sccache_server_and_probe_cache(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="just-shell-sccache-restart-"
        ) as temp_dir:
            root = Path(temp_dir)
            script, cache_dir = self._prepare_just_shell_repo(root)
            bin_dir = root / "bin"
            self._install_fake_tools(bin_dir, "pwsh", "sccache")
            record_path = root / "tool-records.tsv"
            env = self._just_shell_env(bin_dir, record_path)
            env["KD4_TEST_SCCACHE_STATS"] = "10 GiB|80 GiB"
            env["KD4_TEST_SCCACHE_STATE"] = str(root / "sccache-state")

            first = self._run_just_shell(script, env, "cargo check")
            second = self._run_just_shell(script, env, "cargo check")

            self._assert_just_shell_success(first)
            self._assert_just_shell_success(second)
            records = self._decode_tool_records(record_path)
            sccache_args = [
                record["args"]
                for record in records
                if record["role"].startswith("sccache")
            ]
            self.assertEqual(
                sccache_args,
                [
                    ["--show-stats"],
                    ["--stop-server"],
                    ["--start-server"],
                    ["--show-stats"],
                ],
            )
            sccache_caches = list(cache_dir.glob("sccache*.probe"))
            self.assertEqual(len(sccache_caches), 1, list(cache_dir.iterdir()))
            self.assertEqual(sccache_caches[0].read_text(encoding="utf-8"), "ok")
            self.assertEqual(list(cache_dir.glob("*.tmp")), [])

        equivalent_sizes = (
            ("512M", "512 MiB"),
            ("1T", "1 TiB"),
            ("80GB", "80 GiB"),
            ("80GiB", "80 GiB"),
            ("1024G", "1 TiB"),
        )
        for configured, reported in equivalent_sizes:
            with (
                self.subTest(configured=configured, reported=reported),
                tempfile.TemporaryDirectory(
                    prefix="just-shell-sccache-bytes-"
                ) as temp_dir,
            ):
                root = Path(temp_dir)
                script, cache_dir = self._prepare_just_shell_repo(root)
                bin_dir = root / "bin"
                self._install_fake_tools(bin_dir, "pwsh", "sccache")
                record_path = root / "tool-records.tsv"
                env = self._just_shell_env(bin_dir, record_path)
                env["CODEX_SCCACHE_CACHE_SIZE"] = configured
                env["KD4_TEST_SCCACHE_STATS"] = reported

                first = self._run_just_shell(script, env, "cargo check")

                self._assert_just_shell_success(first)
                first_records = self._decode_tool_records(record_path)
                first_sccache_args = [
                    record["args"]
                    for record in first_records
                    if record["role"].startswith("sccache")
                ]
                self.assertEqual(first_sccache_args, [["--show-stats"]])
                sccache_caches = list(cache_dir.glob("sccache*.probe"))
                self.assertEqual(len(sccache_caches), 1, list(cache_dir.iterdir()))
                self.assertEqual(sccache_caches[0].read_text(encoding="utf-8"), "ok")
                self.assertEqual(list(cache_dir.glob("*.tmp")), [])

                record_path.unlink()
                second = self._run_just_shell(script, env, "cargo check")

                self._assert_just_shell_success(second)
                second_records = self._decode_tool_records(record_path)
                self.assertEqual(
                    [
                        record["args"]
                        for record in second_records
                        if record["role"].startswith("sccache")
                    ],
                    [],
                    second_records,
                )
                self.assertEqual(sccache_caches[0].read_text(encoding="utf-8"), "ok")
                self.assertEqual(list(cache_dir.glob("*.tmp")), [])

        with tempfile.TemporaryDirectory(
            prefix="just-shell-sccache-failed-"
        ) as temp_dir:
            root = Path(temp_dir)
            script, cache_dir = self._prepare_just_shell_repo(root)
            bin_dir = root / "bin"
            self._install_fake_tools(bin_dir, "pwsh", "sccache")
            record_path = root / "tool-records.tsv"
            env = self._just_shell_env(bin_dir, record_path)
            env["KD4_TEST_SCCACHE_STATS"] = "10 GiB"
            env["KD4_TEST_SCCACHE_START_EXIT"] = "9"

            first = self._run_just_shell(script, env, "cargo check")
            second = self._run_just_shell(script, env, "cargo check")

            self._assert_just_shell_success(first)
            self._assert_just_shell_success(second)
            records = self._decode_tool_records(record_path)
            sccache_args = [
                record["args"]
                for record in records
                if record["role"].startswith("sccache")
            ]
            self.assertEqual(
                sccache_args,
                [
                    ["--show-stats"],
                    ["--stop-server"],
                    ["--start-server"],
                    ["--show-stats"],
                    ["--stop-server"],
                    ["--start-server"],
                ],
            )
            self.assertEqual(list(cache_dir.glob("sccache*.probe")), [])
            self.assertEqual(list(cache_dir.glob("*.tmp")), [])

    def test_just_shell_cli_configures_python_environment(self) -> None:
        with tempfile.TemporaryDirectory(prefix="just-shell-python-venv-") as temp_dir:
            root = Path(temp_dir)
            script, _cache_dir = self._prepare_just_shell_repo(root)
            bin_dir = root / "bin"
            self._install_fake_tools(bin_dir, "pwsh")
            venv_bin = root / "scripts" / ".venv" / "Scripts"
            venv_bin.mkdir(parents=True)
            (venv_bin / "python.exe").write_bytes(b"python marker")
            record_path = root / "tool-records.tsv"
            env = self._just_shell_env(bin_dir, record_path)
            env["PATH"] = os.pathsep.join((str(venv_bin), str(bin_dir), str(venv_bin)))
            env["PYTHONHOME"] = str(root / "ignored-python-home")

            result = self._run_just_shell(
                script, env, python_args=("-X", "cpu_count=64")
            )

            self._assert_just_shell_success(result)
            actual = self._actual_pwsh_record(self._decode_tool_records(record_path))
            actual_env = actual["env"]
            path_parts = actual_env["PATH"].split(os.pathsep)
            self.assertEqual(path_parts[0], str(venv_bin))
            self.assertEqual(
                sum(
                    os.path.normcase(os.path.normpath(part))
                    == os.path.normcase(os.path.normpath(venv_bin))
                    for part in path_parts
                ),
                1,
            )
            self.assertEqual(actual_env["VIRTUAL_ENV"], str(root / "scripts" / ".venv"))
            self.assertEqual(actual_env["VIRTUAL_ENV_DISABLE_PROMPT"], "1")
            self.assertNotIn("PYTHONHOME", actual_env)
            self.assertEqual(actual_env["PYTHON_CPU_COUNT"], "30")

        with tempfile.TemporaryDirectory(
            prefix="just-shell-python-eight-cpus-"
        ) as temp_dir:
            root = Path(temp_dir)
            script, _cache_dir = self._prepare_just_shell_repo(root)
            bin_dir = root / "bin"
            self._install_fake_tools(bin_dir, "pwsh")
            record_path = root / "tool-records.tsv"
            env = self._just_shell_env(bin_dir, record_path)

            result = self._run_just_shell(
                script, env, python_args=("-X", "cpu_count=8")
            )

            self._assert_just_shell_success(result)
            actual_env = self._actual_pwsh_record(
                self._decode_tool_records(record_path)
            )["env"]
            self.assertEqual(actual_env["PYTHON_CPU_COUNT"], "8")

        with tempfile.TemporaryDirectory(
            prefix="just-shell-python-cpu-unavailable-"
        ) as temp_dir:
            root = Path(temp_dir)
            script, _cache_dir = self._prepare_just_shell_repo(root)
            bin_dir = root / "bin"
            self._install_fake_tools(bin_dir, "pwsh")
            record_path = root / "tool-records.tsv"
            env = self._just_shell_env(bin_dir, record_path)
            bootstrap = root / "run-just-shell-without-cpu-count.py"
            bootstrap.write_text(
                """import os
import runpy
import sys
from pathlib import Path
from unittest import mock

script = Path(sys.argv[1])
sys.argv = [str(script), *sys.argv[2:]]
sys.path.insert(0, str(script.parent))
with mock.patch.object(os, "cpu_count", return_value=None):
    runpy.run_path(str(script), run_name="__main__")
""",
                encoding="utf-8",
                newline="\n",
            )

            result = subprocess.run(
                [
                    sys.executable,
                    "-E",
                    str(bootstrap),
                    str(script),
                    "Write-Output ready",
                    "integration-test",
                ],
                cwd=root,
                env=env,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                check=False,
                timeout=15,
            )

            self._assert_just_shell_success(result)
            actual_env = self._actual_pwsh_record(
                self._decode_tool_records(record_path)
            )["env"]
            self.assertEqual(actual_env["PYTHON_CPU_COUNT"], "1")

        with tempfile.TemporaryDirectory(
            prefix="just-shell-python-existing-"
        ) as temp_dir:
            root = Path(temp_dir)
            script, _cache_dir = self._prepare_just_shell_repo(root)
            bin_dir = root / "bin"
            self._install_fake_tools(bin_dir, "pwsh")
            record_path = root / "tool-records.tsv"
            env = self._just_shell_env(bin_dir, record_path)
            env["VIRTUAL_ENV"] = str(root / "existing-venv")
            env["PYTHON_CPU_COUNT"] = "7"
            original_path = env["PATH"]

            result = self._run_just_shell(
                script, env, python_args=("-X", "cpu_count=64")
            )

            self._assert_just_shell_success(result)
            actual_env = self._actual_pwsh_record(
                self._decode_tool_records(record_path)
            )["env"]
            self.assertEqual(actual_env["VIRTUAL_ENV"], str(root / "existing-venv"))
            self.assertEqual(actual_env["PYTHON_CPU_COUNT"], "7")
            self.assertEqual(actual_env["PATH"], original_path)

        with tempfile.TemporaryDirectory(prefix="just-shell-python-ci-") as temp_dir:
            root = Path(temp_dir)
            script, _cache_dir = self._prepare_just_shell_repo(root)
            bin_dir = root / "bin"
            self._install_fake_tools(bin_dir, "pwsh")
            record_path = root / "tool-records.tsv"
            env = self._just_shell_env(bin_dir, record_path)
            env["CI"] = "true"

            result = self._run_just_shell(
                script, env, python_args=("-X", "cpu_count=64")
            )

            self._assert_just_shell_success(result)
            actual_env = self._actual_pwsh_record(
                self._decode_tool_records(record_path)
            )["env"]
            self.assertNotIn("PYTHON_CPU_COUNT", actual_env)

        for label, make_python_directory in (
            ("missing", False),
            ("directory-named-python", True),
        ):
            with self.subTest(label=label), tempfile.TemporaryDirectory(
                prefix=f"just-shell-python-{label}-"
            ) as temp_dir:
                root = Path(temp_dir)
                script, cache_dir = self._prepare_just_shell_repo(root)
                bin_dir = root / "bin"
                self._install_fake_tools(bin_dir, "pwsh")
                (root / "scripts" / "uv.lock").write_text("", encoding="utf-8")
                if make_python_directory:
                    (root / "scripts" / ".venv" / "Scripts" / "python.exe").mkdir(
                        parents=True
                    )
                record_path = root / "tool-records.tsv"
                env = self._just_shell_env(bin_dir, record_path)

                first = self._run_just_shell(script, env)
                second = self._run_just_shell(script, env)

                self._assert_just_shell_success(first)
                self._assert_just_shell_success(second)
                self.assertIn("scripts/.venv is missing", first.stderr)
                self.assertNotIn("scripts/.venv is missing", second.stderr)
                actuals = [
                    record
                    for record in self._decode_tool_records(record_path)
                    if "-CommandWithArgs" in record["args"]
                ]
                self.assertEqual(len(actuals), 2)
                self.assertTrue(
                    all("VIRTUAL_ENV" not in record["env"] for record in actuals)
                )
                self.assertEqual(
                    (cache_dir / "scripts-venv-missing.warn").read_text(
                        encoding="utf-8"
                    ),
                    "warned",
                )

    def test_just_shell_cli_renders_and_validates_powershell(self) -> None:
        with tempfile.TemporaryDirectory(prefix="just-shell-pwsh-render-") as temp_dir:
            root = Path(temp_dir)
            script, _cache_dir = self._prepare_just_shell_repo(root)
            bin_dir = root / "bin"
            self._install_fake_tools(bin_dir, "pwsh")
            record_path = root / "tool-records.tsv"
            env = self._just_shell_env(bin_dir, record_path)

            result = self._run_just_shell(
                script,
                env,
                "Write-Output {args}; Get-Item missing {stderr-null}",
                "render",
                "one",
                "two",
            )

            self._assert_just_shell_success(result)
            actual = self._actual_pwsh_record(self._decode_tool_records(record_path))
            self.assertEqual(
                actual["args"],
                [
                    "-NoLogo",
                    "-NoProfile",
                    "-CommandWithArgs",
                    "Write-Output @($args | Select-Object -Skip 1); "
                    "Get-Item missing 2>$null; exit $LASTEXITCODE",
                    "render",
                    "one",
                    "two",
                ],
            )

            invalid = self._run_just_shell(
                script, env, "Write-Output {stderr-null}; Write-Output later"
            )
            self.assertEqual(invalid.returncode, 1)
            self.assertIn("{stderr-null} must be the final token", invalid.stderr)

        with tempfile.TemporaryDirectory(prefix="just-shell-pwsh-old-") as temp_dir:
            root = Path(temp_dir)
            script, cache_dir = self._prepare_just_shell_repo(root)
            bin_dir = root / "bin"
            self._install_fake_tools(bin_dir, "pwsh")
            record_path = root / "tool-records.tsv"
            env = self._just_shell_env(bin_dir, record_path)
            env["KD4_TEST_PWSH_PROBE_MODE"] = "old"

            result = self._run_just_shell(script, env)

            self.assertEqual(result.returncode, 1)
            self.assertIn("PowerShell 7.4 or newer is required", result.stderr)
            records = self._decode_tool_records(record_path)
            self.assertEqual(len(records), 1, records)
            pwsh_caches = list(cache_dir.glob("pwsh*.probe"))
            self.assertEqual(len(pwsh_caches), 1)
            self.assertEqual(pwsh_caches[0].read_text(encoding="utf-8"), "fail")

        with tempfile.TemporaryDirectory(prefix="just-shell-pwsh-missing-") as temp_dir:
            root = Path(temp_dir)
            script, _cache_dir = self._prepare_just_shell_repo(root)
            bin_dir = root / "empty-bin"
            bin_dir.mkdir()
            result = self._run_just_shell(
                script, self._just_shell_env(bin_dir, root / "unused-records.tsv")
            )
            self.assertEqual(result.returncode, 1)
            self.assertIn("PowerShell ('pwsh') is required", result.stderr)

        with tempfile.TemporaryDirectory(prefix="just-shell-pwsh-broken-") as temp_dir:
            root = Path(temp_dir)
            script, cache_dir = self._prepare_just_shell_repo(root)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            broken = bin_dir / self._tool_filename("pwsh")
            broken.write_bytes(b"not an executable")
            record_path = root / "tool-records.tsv"
            env = self._just_shell_env(bin_dir, record_path)

            first = self._run_just_shell(script, env)
            second = self._run_just_shell(script, env)

            self.assertEqual(first.returncode, 1)
            self.assertEqual(second.returncode, 1)
            self.assertIn("Failed to launch PowerShell", first.stderr)
            self.assertIn("Failed to launch PowerShell", second.stderr)
            self.assertEqual(list(cache_dir.glob("pwsh*.probe")), [])
            self.assertEqual(list(cache_dir.glob("*.tmp")), [])

        with tempfile.TemporaryDirectory(prefix="just-shell-pwsh-timeout-") as temp_dir:
            root = Path(temp_dir)
            script, cache_dir = self._prepare_just_shell_repo(root)
            bin_dir = root / "bin"
            self._install_fake_tools(bin_dir, "pwsh")
            record_path = root / "tool-records.tsv"
            env = self._just_shell_env(bin_dir, record_path)
            env["KD4_TEST_PWSH_PROBE_MODE"] = "timeout"
            timeout_marker = root / "probe-finished-after-sleep"
            env["KD4_TEST_PWSH_TIMEOUT_MARKER"] = str(timeout_marker)

            started = time.monotonic()
            first = self._run_just_shell(script, env)
            second = self._run_just_shell(script, env)
            elapsed = time.monotonic() - started

            self._assert_just_shell_success(first)
            self._assert_just_shell_success(second)
            self.assertLess(elapsed, 6.5)
            time.sleep(2.25)
            self.assertFalse(timeout_marker.exists())
            records = self._decode_tool_records(record_path)
            probes = [record for record in records if "-Command" in record["args"]]
            actuals = [
                record for record in records if "-CommandWithArgs" in record["args"]
            ]
            self.assertEqual(len(probes), 2, records)
            self.assertEqual(len(actuals), 2, records)
            self.assertEqual(list(cache_dir.glob("pwsh*.probe")), [])
            self.assertEqual(list(cache_dir.glob("*.tmp")), [])

        with tempfile.TemporaryDirectory(
            prefix="just-shell-pwsh-concurrent-cache-"
        ) as temp_dir:
            root = Path(temp_dir)
            script, cache_dir = self._prepare_just_shell_repo(root)
            bin_dir = root / "bin"
            self._install_fake_tools(bin_dir, "pwsh")
            record_path = root / "tool-records.tsv"
            env = self._just_shell_env(bin_dir, record_path)
            command = [
                sys.executable,
                "-E",
                str(script),
                "Write-Output ready",
                "integration-test",
            ]

            processes = [
                subprocess.Popen(
                    command,
                    cwd=root,
                    env=env,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                    encoding="utf-8",
                    errors="replace",
                )
                for _ in range(2)
            ]
            concurrent_results = []
            for process in processes:
                stdout, stderr = process.communicate(timeout=15)
                concurrent_results.append(
                    subprocess.CompletedProcess(
                        command,
                        process.returncode,
                        stdout,
                        stderr,
                    )
                )

            for result in concurrent_results:
                self._assert_just_shell_success(result)
            cache_entries = list(cache_dir.glob("pwsh*.probe"))
            self.assertEqual(len(cache_entries), 1, list(cache_dir.iterdir()))
            self.assertEqual(cache_entries[0].read_text(encoding="utf-8"), "ok")
            self.assertEqual(list(cache_dir.glob("*.tmp")), [])

            record_path.write_text("", encoding="utf-8")
            reused = self._run_just_shell(script, env)

            self._assert_just_shell_success(reused)
            reused_records = self._decode_tool_records(record_path)
            self.assertEqual(
                len(
                    [
                        record
                        for record in reused_records
                        if "-CommandWithArgs" in record["args"]
                    ]
                ),
                1,
                reused_records,
            )
            self.assertEqual(
                [record for record in reused_records if "-Command" in record["args"]],
                [],
                reused_records,
            )
            self.assertEqual(cache_entries[0].read_text(encoding="utf-8"), "ok")
            self.assertEqual(list(cache_dir.glob("*.tmp")), [])

        with tempfile.TemporaryDirectory(
            prefix="just-shell-pwsh-identity-"
        ) as temp_dir:
            root = Path(temp_dir)
            script, cache_dir = self._prepare_just_shell_repo(root)
            bin_dir = root / "bin"
            tools = self._install_fake_tools(bin_dir, "pwsh")
            record_path = root / "tool-records.tsv"
            env = self._just_shell_env(bin_dir, record_path)

            first = self._run_just_shell(script, env)
            second = self._run_just_shell(script, env)
            stat = tools["pwsh"].stat()
            os.utime(
                tools["pwsh"],
                ns=(stat.st_atime_ns, stat.st_mtime_ns + 10_000_000),
            )
            third = self._run_just_shell(script, env)

            for result in (first, second, third):
                self._assert_just_shell_success(result)
            records = self._decode_tool_records(record_path)
            probes = [record for record in records if "-Command" in record["args"]]
            actuals = [
                record for record in records if "-CommandWithArgs" in record["args"]
            ]
            self.assertEqual(len(probes), 2, records)
            self.assertEqual(len(actuals), 3, records)
            pwsh_caches = list(cache_dir.glob("pwsh*.probe"))
            self.assertEqual(len(pwsh_caches), 2, list(cache_dir.iterdir()))
            self.assertTrue(
                all(path.read_text(encoding="utf-8") == "ok" for path in pwsh_caches)
            )
            self.assertEqual(list(cache_dir.glob("*.tmp")), [])

    def test_just_shell_cli_configures_windows_lld_link(self) -> None:
        with tempfile.TemporaryDirectory(prefix="just-shell-linker-path-") as temp_dir:
            root = Path(temp_dir)
            script, _cache_dir = self._prepare_just_shell_repo(root)
            bin_dir = root / "bin"
            tools = self._install_fake_tools(bin_dir, "pwsh", "lld-link")
            record_path = root / "tool-records.tsv"
            env = self._just_shell_env(bin_dir, record_path)

            result = self._run_just_shell(script, env, "cargo check")

            self._assert_just_shell_success(result)
            actual_env = self._actual_pwsh_record(
                self._decode_tool_records(record_path)
            )["env"]
            self.assertEqual(
                os.path.normcase(
                    actual_env["CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"]
                ),
                os.path.normcase(str(tools["lld-link"])),
            )
            self.assertEqual(
                os.path.normcase(
                    actual_env["CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER"]
                ),
                os.path.normcase(str(tools["lld-link"])),
            )

        with tempfile.TemporaryDirectory(prefix="just-shell-linker-scoop-") as temp_dir:
            root = Path(temp_dir)
            script, _cache_dir = self._prepare_just_shell_repo(root)
            bin_dir = root / "bin"
            self._install_fake_tools(bin_dir, "pwsh")
            scoop_linker = (
                root / "scoop" / "apps" / "llvm" / "current" / "bin" / "lld-link.exe"
            )
            scoop_linker.parent.mkdir(parents=True)
            scoop_linker.write_bytes(self._compiled_fake_tool().read_bytes())
            record_path = root / "tool-records.tsv"
            env = self._just_shell_env(bin_dir, record_path)
            env["SCOOP"] = str(root / "scoop")

            result = self._run_just_shell(script, env, "cargo check")

            self._assert_just_shell_success(result)
            actual_env = self._actual_pwsh_record(
                self._decode_tool_records(record_path)
            )["env"]
            self.assertEqual(
                actual_env["CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"],
                str(scoop_linker),
            )
            self.assertEqual(
                actual_env["CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER"],
                str(scoop_linker),
            )

        with tempfile.TemporaryDirectory(
            prefix="just-shell-linker-existing-"
        ) as temp_dir:
            root = Path(temp_dir)
            script, _cache_dir = self._prepare_just_shell_repo(root)
            bin_dir = root / "bin"
            tools = self._install_fake_tools(bin_dir, "pwsh", "lld-link")
            record_path = root / "tool-records.tsv"
            env = self._just_shell_env(bin_dir, record_path)
            env["CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"] = "custom-link.exe"

            result = self._run_just_shell(script, env, "cargo check")

            self._assert_just_shell_success(result)
            actual_env = self._actual_pwsh_record(
                self._decode_tool_records(record_path)
            )["env"]
            self.assertEqual(
                actual_env["CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"],
                "custom-link.exe",
            )
            self.assertEqual(
                os.path.normcase(
                    actual_env["CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER"]
                ),
                os.path.normcase(str(tools["lld-link"])),
            )

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

    def test_rust_cargo_config_lets_rustc_discover_msvc_linker(self) -> None:
        config = load_toml(REPO_ROOT / "codex-rs" / ".cargo" / "config.toml")
        targets = config["target"]

        for target_config in targets.values():
            self.assertNotIn("linker", target_config)

    def test_workspace_contracts_through_cargo_metadata(self) -> None:
        result = self.run_supported_command(
            [
                "cargo",
                "metadata",
                "--offline",
                "--locked",
                "--no-deps",
                "--format-version",
                "1",
                "--manifest-path",
                "codex-rs/Cargo.toml",
            ],
            timeout=120,
        )
        metadata = json.loads(result.stdout)
        self.assertEqual(
            Path(metadata["workspace_root"]).resolve(),
            (REPO_ROOT / "codex-rs").resolve(),
        )

        packages = {package["name"]: package for package in metadata["packages"]}

        def dependency(package_name: str, dependency_name: str) -> dict[str, object]:
            matches = [
                item
                for item in packages[package_name]["dependencies"]
                if item["name"] == dependency_name
            ]
            self.assertEqual(
                len(matches),
                1,
                f"{package_name} metadata dependency {dependency_name!r}",
            )
            return matches[0]

        reqwest = dependency("codex-http-client", "reqwest")
        self.assertFalse(reqwest["uses_default_features"])
        self.assertEqual(
            sorted(reqwest["features"]),
            [
                "blocking",
                "charset",
                "cookies",
                "http2",
                "json",
                "query",
                "rustls",
                "stream",
                "system-proxy",
            ],
        )
        sqlx = dependency("codex-state", "sqlx")
        self.assertFalse(sqlx["uses_default_features"])
        self.assertEqual(
            sorted(sqlx["features"]),
            [
                "chrono",
                "json",
                "macros",
                "migrate",
                "runtime-tokio",
                "sqlite-bundled",
                "uuid",
            ],
        )
        tokio_tungstenite = dependency("codex-api", "tokio-tungstenite")
        self.assertTrue(tokio_tungstenite["uses_default_features"])
        self.assertEqual(
            sorted(tokio_tungstenite["features"]),
            ["proxy", "rustls-tls-native-roots"],
        )
        tungstenite = dependency("codex-api", "tungstenite")
        self.assertTrue(tungstenite["uses_default_features"])
        self.assertEqual(
            sorted(tungstenite["features"]),
            ["deflate", "proxy"],
        )

        manifest = load_toml(REPO_ROOT / "codex-rs" / "Cargo.toml")
        profiles = manifest["profile"]
        self.assertEqual(profiles["dev"]["debug"], "limited")
        self.assertEqual(profiles["ci-test"]["debug"], "limited")
        self.assertEqual(profiles["release"]["lto"], "thin")
        self.assertEqual(profiles["release"]["debug"], "line-tables-only")
        self.assertEqual(profiles["release"]["split-debuginfo"], "off")
        self.assertEqual(profiles["release"]["strip"], "symbols")
        self.assertEqual(profiles["release"]["codegen-units"], 4)
        self.assertNotIn("local-test", profiles)
        self.assertNotIn("release-fast", profiles)

        def toml_scalar(value: object) -> str:
            if isinstance(value, bool):
                return str(value).lower()
            if isinstance(value, (int, str)):
                return json.dumps(value)
            raise AssertionError(f"unsupported fixture profile value: {value!r}")

        fixture_profile_lines: list[str] = []
        for profile_name in ("dev", "release", "ci-test"):
            fixture_profile_lines.append(f"[profile.{profile_name}]")
            fixture_profile_lines.extend(
                f"{key} = {toml_scalar(value)}"
                for key, value in profiles[profile_name].items()
            )
            fixture_profile_lines.append("")

        with tempfile.TemporaryDirectory(prefix="cargo-profile-contract-") as temp_dir:
            fixture_root = Path(temp_dir)
            (fixture_root / "src").mkdir()
            (fixture_root / "src" / "main.rs").write_text(
                'fn main() { println!("profile probe"); }\n',
                encoding="utf-8",
                newline="\n",
            )
            manifest_path = fixture_root / "Cargo.toml"
            manifest_path.write_text(
                "[package]\n"
                'name = "profile-probe"\n'
                'version = "0.0.0"\n'
                'edition = "2024"\n\n'
                "[workspace]\n"
                'resolver = "2"\n\n'
                + "\n".join(fixture_profile_lines),
                encoding="utf-8",
                newline="\n",
            )
            fixture_env = {
                key: value
                for key, value in os.environ.items()
                if key
                not in {
                    "CARGO_ENCODED_RUSTFLAGS",
                    "CARGO_INCREMENTAL",
                    "CARGO_TARGET_DIR",
                    "RUSTC_WRAPPER",
                    "RUSTFLAGS",
                }
            }
            fixture_env["CARGO_INCREMENTAL"] = "0"
            fixture_env["CARGO_TARGET_DIR"] = str(fixture_root / "target")

            profile_runs: dict[str, subprocess.CompletedProcess[str]] = {}
            for profile_name, cargo_action in (
                ("dev", "build"),
                ("release", "build"),
                ("ci-test", "test"),
            ):
                command = [
                    "cargo",
                    cargo_action,
                    "--offline",
                    "--manifest-path",
                    str(manifest_path),
                    "--profile",
                    profile_name,
                    "--message-format=json-render-diagnostics",
                    "-vv",
                ]
                if cargo_action == "test":
                    command.insert(2, "--no-run")
                profile_runs[profile_name] = self.run_supported_command(
                    command,
                    cwd=fixture_root,
                    env=fixture_env,
                    timeout=120,
                )

            artifacts: dict[str, dict[str, object]] = {}
            for profile_name, run in profile_runs.items():
                candidates = []
                for line in run.stdout.splitlines():
                    try:
                        event = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    if (
                        event.get("reason") == "compiler-artifact"
                        and event.get("target", {}).get("name") == "profile-probe"
                        and event.get("executable")
                    ):
                        candidates.append(event)
                expected_test = profile_name == "ci-test"
                candidates = [
                    event
                    for event in candidates
                    if event["profile"]["test"] is expected_test
                ]
                self.assertEqual(len(candidates), 1, (profile_name, candidates))
                artifacts[profile_name] = candidates[0]
                self.assertTrue(Path(candidates[0]["executable"]).is_file())

            self.assertEqual(
                artifacts["dev"]["profile"],
                {
                    "opt_level": "0",
                    "debuginfo": 1,
                    "debug_assertions": True,
                    "overflow_checks": True,
                    "test": False,
                },
            )
            self.assertEqual(
                artifacts["release"]["profile"],
                {
                    "opt_level": "3",
                    "debuginfo": "line-tables-only",
                    "debug_assertions": False,
                    "overflow_checks": False,
                    "test": False,
                },
            )
            self.assertEqual(
                artifacts["ci-test"]["profile"],
                {
                    "opt_level": "0",
                    "debuginfo": 1,
                    "debug_assertions": True,
                    "overflow_checks": True,
                    "test": True,
                },
            )

            release_trace = profile_runs["release"].stdout + profile_runs["release"].stderr
            for rustc_flag in (
                "codegen-units=4",
                "strip=symbols",
            ):
                self.assertIn(rustc_flag, release_trace)
            # Cargo omits the explicit off value when it is rustc's default.
            self.assertNotRegex(release_trace, r"split-debuginfo=(?:packed|unpacked)")
            self.assertRegex(release_trace, r"(?:lto=thin|linker-plugin-lto)")

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
        completion_proof_protocol_override = {
            "platform": "cfg(windows)",
            "filter": "(package(codex-app-server) & binary(all) & (test(=suite::v2::turn_start::blocked_completion_proof_reaches_app_server_as_failed_turn_without_launching_runner) | test(=suite::v2::turn_start::desktop_fresh_home_missing_canonical_command_blocks_changed_repository))) | (package(codex-mcp-server) & binary(all) & test(=suite::codex_tool::blocked_completion_proof_reaches_mcp_as_error_without_launching_runner))",
            "slow-timeout": {"period": "2m", "terminate-after": 2},
        }
        self.assertIn(
            completion_proof_protocol_override,
            nextest["profile"]["default"]["overrides"],
        )
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

    def test_format_recipes_through_just_cli(self) -> None:
        expected_argv = {
            "fmt": 'sys.argv = [script, "--fast-local"]',
            "fmt-full": "sys.argv = [script]",
            "fmt-check": 'sys.argv = [script, "--check"]',
            "fmt-check-fast": 'sys.argv = [script, "--check", "--fast-local"]',
        }
        for recipe, expected in expected_argv.items():
            with self.subTest(recipe=recipe):
                rendered = self.run_supported_command(["just", "--dry-run", recipe])
                output = rendered.stdout + rendered.stderr
                self.assertIn(expected, output)
                self.assertIn('runpy.run_path(script, run_name="__main__")', output)

        for recipe, expected_lines in {
            "validate-crate": [
                "just fmt-check-fast",
                "just test-fast -p codex-core",
            ],
            "validate-crate-full": [
                "just fmt-check",
                "just test-fast -p codex-core",
            ],
        }.items():
            with self.subTest(recipe=recipe):
                rendered = self.run_supported_command(
                    ["just", "--dry-run", recipe, "codex-core"]
                )
                self.assertEqual(
                    (rendered.stdout + rendered.stderr).splitlines(), expected_lines
                )

    def test_parallelism_and_windows_flags_through_just_cli(self) -> None:
        inherited_env = {
            key: value
            for key, value in os.environ.items()
            if key
            not in {
                "CARGO_BUILD_JOBS",
                "RUST_TEST_THREADS",
                "NEXTEST_TEST_THREADS",
            }
        }
        expressions = {
            "cargo_build_jobs": "CARGO_BUILD_JOBS",
            "rust_test_threads": "RUST_TEST_THREADS",
            "nextest_test_threads": "NEXTEST_TEST_THREADS",
        }
        parallelism = self.run_supported_command(
            ["just", "--evaluate", "rust_parallelism"], env=inherited_env
        ).stdout.strip()
        self.assertEqual(parallelism, "8")
        for expression, environment_name in expressions.items():
            with self.subTest(expression=expression, source="default"):
                evaluated = self.run_supported_command(
                    ["just", "--evaluate", expression], env=inherited_env
                )
                self.assertEqual(evaluated.stdout.strip(), parallelism)
            with self.subTest(expression=expression, source="environment"):
                evaluated = self.run_supported_command(
                    ["just", "--evaluate", expression],
                    env={**inherited_env, environment_name: "3"},
                )
                self.assertEqual(evaluated.stdout.strip(), "3")

        cargo_config = load_toml(REPO_ROOT / "codex-rs" / ".cargo" / "config.toml")
        self.assertEqual(cargo_config["build"]["jobs"], 8)
        self.assertEqual(
            cargo_config["env"]["RUST_TEST_THREADS"],
            {"value": "16", "force": False},
        )
        targets = cargo_config["target"]
        msvc_flags = targets['cfg(all(windows, target_env = "msvc"))']["rustflags"]
        arm64_flags = targets["aarch64-pc-windows-msvc"]["rustflags"]
        effective_arm64_flags = [*msvc_flags, *arm64_flags]
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
        for flag in (
            "link-arg=/STACK:8388608",
            "target-feature=+crt-static",
            "link-arg=/arm64hazardfree",
        ):
            self.assertEqual(effective_arm64_flags.count(flag), 1)

    def test_deps_audit_route_and_policy_through_just_cli(self) -> None:
        rendered = self.run_supported_command(["just", "--dry-run", "deps-audit"])
        self.assertEqual(
            (rendered.stdout + rendered.stderr).splitlines(), ["cargo audit"]
        )

        audit = load_toml(REPO_ROOT / "codex-rs" / ".cargo" / "audit.toml")
        deny = load_toml(REPO_ROOT / "codex-rs" / "deny.toml")
        audit_ignores = audit["advisories"]["ignore"]
        deny_ignores = [entry["id"] for entry in deny["advisories"]["ignore"]]
        self.assertEqual(len(audit_ignores), len(set(audit_ignores)))
        self.assertEqual(set(audit_ignores), set(deny_ignores))
        self.assertEqual(audit["output"]["deny"], ["yanked"])
        self.assertFalse(audit["output"]["quiet"])
        self.assertTrue(audit["output"]["show_tree"])
        self.assertFalse(
            (REPO_ROOT / ".github" / "workflows" / "cargo-audit.yml").exists()
        )

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

    def test_formatter_cli_runs_pinned_nightly_rustfmt(self) -> None:
        with tempfile.TemporaryDirectory(prefix="formatter-rustup-") as temp_dir:
            temp_path = Path(temp_dir)
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            record_path = temp_path / "rustup-invocations.jsonl"
            recorder_path = temp_path / "record_rustup.py"
            recorder_path.write_text(
                "import json\n"
                "import os\n"
                "import sys\n"
                "from pathlib import Path\n"
                "\n"
                'record_path = Path(os.environ["KD4_TEST_RUSTUP_RECORD"])\n'
                'with record_path.open("a", encoding="utf-8", newline="\\n") as stream:\n'
                '    payload = {"cwd": os.getcwd(), "args": sys.argv[1:]}\n'
                '    stream.write(json.dumps(payload) + "\\n")\n',
                encoding="utf-8",
                newline="\n",
            )

            if os.name == "nt":
                launcher_source = temp_path / "rustup_launcher.rs"
                launcher_path = fake_bin / "rustup.exe"
                launcher_source.write_text(
                    """use std::env;
use std::process::{exit, Command};

fn main() {
    let python = env::var_os("KD4_TEST_PYTHON").expect("KD4_TEST_PYTHON");
    let recorder =
        env::var_os("KD4_TEST_RUSTUP_RECORDER").expect("KD4_TEST_RUSTUP_RECORDER");
    let status = Command::new(python)
        .arg(recorder)
        .args(env::args_os().skip(1))
        .status()
        .expect("start fake rustup recorder");
    exit(status.code().unwrap_or(1));
}
""",
                    encoding="utf-8",
                    newline="\n",
                )
                compile_result = subprocess.run(
                    [
                        "rustc",
                        "--edition=2021",
                        str(launcher_source),
                        "-o",
                        str(launcher_path),
                    ],
                    cwd=REPO_ROOT,
                    capture_output=True,
                    text=True,
                    encoding="utf-8",
                    errors="replace",
                    check=False,
                    timeout=120,
                )
                self.assertEqual(
                    compile_result.returncode,
                    0,
                    (
                        f"stdout:\n{compile_result.stdout}\n"
                        f"stderr:\n{compile_result.stderr}"
                    ),
                )
            else:
                launcher_path = fake_bin / "rustup"
                launcher_path.write_text(
                    "#!/bin/sh\n"
                    'exec "$KD4_TEST_PYTHON" "$KD4_TEST_RUSTUP_RECORDER" "$@"\n',
                    encoding="utf-8",
                    newline="\n",
                )
                launcher_path.chmod(launcher_path.stat().st_mode | 0o111)

            env = os.environ.copy()
            env["KD4_TEST_PYTHON"] = str(Path(sys.executable).resolve())
            env["KD4_TEST_RUSTUP_RECORDER"] = str(recorder_path.resolve())
            env["KD4_TEST_RUSTUP_RECORD"] = str(record_path.resolve())
            env["PATH"] = str(fake_bin.resolve())
            result = subprocess.run(
                [
                    sys.executable,
                    "scripts/format.py",
                    "--check",
                    "--only",
                    "rust",
                ],
                cwd=REPO_ROOT,
                env=env,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                check=False,
                timeout=30,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertEqual(result.stderr, "")
            self.assertIn("Starting Rust formatter...", result.stdout)
            self.assertIn("==> Rust formatter finished", result.stdout)
            self.assertIn(
                f"$ rustup run {tool_versions.RUSTFMT_TOOLCHAIN} "
                "cargo fmt --check\n",
                result.stdout,
            )
            self.assertTrue(record_path.is_file(), result.stdout)
            invocations = [
                json.loads(line)
                for line in record_path.read_text(encoding="utf-8").splitlines()
                if line
            ]
            self.assertEqual(len(invocations), 1)
            invocation = invocations[0]
            self.assertEqual(
                invocation["args"],
                [
                    "run",
                    tool_versions.RUSTFMT_TOOLCHAIN,
                    "cargo",
                    "fmt",
                    "--check",
                ],
            )
            self.assertEqual(
                os.path.normcase(os.path.realpath(invocation["cwd"])),
                os.path.normcase(os.path.realpath(REPO_ROOT / "codex-rs")),
            )

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

    def test_formatter_cli_rejects_fast_local_prettier_selection(self) -> None:
        result = subprocess.run(
            [
                sys.executable,
                "scripts/format.py",
                "--check",
                "--fast-local",
                "--only",
                "prettier",
            ],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            timeout=30,
        )

        self.assertEqual(
            result.returncode,
            2,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertEqual(result.stdout, "")
        self.assertIn(
            "--fast-local excludes formatter groups selected by --only: prettier",
            result.stderr,
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
