#!/usr/bin/env python3
"""Runtime-path tests for the strict, manifest-driven Rust test runner.

Every discovered test starts ``scripts/rust_test_runner.py`` in a fresh Python
subprocess.  Tests which need deterministic Cargo behavior put an offline Cargo
fixture on ``PATH``; repository-contract tests use the installed Cargo metadata
command.  No test imports runner internals.
"""

from __future__ import annotations

import copy
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any, ClassVar

import tomllib

REPO_ROOT = Path(__file__).resolve().parents[1]
RUNNER = REPO_ROOT / "scripts" / "rust_test_runner.py"
REPOSITORY_MANIFEST = REPO_ROOT / "codex-rs" / ".config" / "kd4-rust-tests.toml"
RUST_MIN_STACK_BYTES = "8388608"
_FAKE_CARGO_LAUNCHER_DIR: tempfile.TemporaryDirectory[str] | None = None


MANIFEST_DATA: dict[str, Any] = {
    "version": 1,
    "helpers": {
        "codex": {"package": "codex-cli", "bin": "codex"},
        "codex-code-mode-host": {
            "package": "codex-code-mode-host",
            "bin": "codex-code-mode-host",
        },
        "test_stdio_server": {
            "package": "codex-rmcp-client",
            "bin": "test_stdio_server",
        },
        "codex-command-runner": {
            "package": "codex-windows-sandbox",
            "bin": "codex-command-runner",
            "platform": "windows",
        },
    },
    "targets": {
        "core_lib": {
            "package": "codex-core",
            "lib": True,
            "helpers": ["codex", "codex-command-runner"],
        },
        "core_all": {
            "package": "codex-core",
            "test": "all",
            "helpers": ["codex", "codex-code-mode-host", "test_stdio_server"],
        },
        "core_shard": {
            "package": "codex-core",
            "test": "core_shard",
            "helpers": ["codex"],
        },
        "core_shard_two": {
            "package": "codex-core",
            "test": "core_shard_two",
            "helpers": ["codex"],
        },
        "command_runner_bin": {
            "package": "codex-windows-sandbox",
            "bin": "codex-command-runner",
            "helpers": [],
        },
    },
    "gates": {
        "demo-gate": {
            "description": "Two targets so the helper union is observable.",
            "steps": [
                {
                    "target": "core_lib",
                    "filter": "test(alpha)",
                    "tests": ["mod::tests::alpha"],
                },
                {
                    "target": "core_all",
                    "filter": "test(beta)",
                    "tests": ["suite::mod::beta"],
                },
            ],
        },
    },
}


METADATA_PACKAGES: list[dict[str, Any]] = [
    {
        "name": "codex-core",
        "id": "path+file:///codex-core#0.0.0",
        "features": {"completion-proof-test-store": []},
        "targets": [
            {"name": "codex_core", "kind": ["lib"]},
            {"name": "all", "kind": ["test"]},
            {"name": "core_shard", "kind": ["test"]},
            {"name": "core_shard_two", "kind": ["test"]},
        ],
    },
    {
        "name": "codex-cli",
        "id": "path+file:///codex-cli#0.0.0",
        "targets": [{"name": "codex", "kind": ["bin"]}],
    },
    {
        "name": "codex-code-mode-host",
        "id": "path+file:///codex-code-mode-host#0.0.0",
        "targets": [{"name": "codex-code-mode-host", "kind": ["bin"]}],
    },
    {
        "name": "codex-rmcp-client",
        "id": "path+file:///codex-rmcp-client#0.0.0",
        "targets": [
            {"name": "codex_rmcp_client", "kind": ["lib"]},
            {"name": "test_stdio_server", "kind": ["bin"]},
        ],
    },
    {
        "name": "codex-windows-sandbox",
        "id": "path+file:///codex-windows-sandbox#0.0.0",
        "targets": [{"name": "codex-command-runner", "kind": ["bin"]}],
    },
]


FAKE_CARGO_SOURCE = r"""#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import sys
from pathlib import Path

state = json.loads(Path(os.environ["KD4_FAKE_CARGO_STATE"]).read_text(encoding="utf-8"))
args = sys.argv[1:]
interesting = {
    key: value
    for key, value in os.environ.items()
    if key.startswith("CARGO_BIN_EXE_")
    or key in {"INSTA_UPDATE", "NEXTEST_PROFILE", "RUST_MIN_STACK"}
}
entry = {"args": args, "cwd": os.getcwd(), "env": interesting}
with Path(os.environ["KD4_FAKE_CARGO_LOG"]).open("a", encoding="utf-8") as output:
    output.write(json.dumps(entry) + "\n")


def selector() -> str:
    for option in ("--test", "--bin"):
        if option in args:
            return args[args.index(option) + 1]
    return "--lib"


def target_identity() -> dict[str, str]:
    package_name = args[args.index("-p") + 1]
    package = next(
        item for item in state["metadata"]["packages"] if item["name"] == package_name
    )
    if "--lib" in args:
        target = next(
            item
            for item in package["targets"]
            if "lib" in item["kind"] or "proc-macro" in item["kind"]
        )
        kind = next(value for value in ("lib", "proc-macro") if value in target["kind"])
    else:
        kind = "test" if "--test" in args else "bin"
        binary_name = args[args.index(f"--{kind}") + 1]
        target = next(
            item
            for item in package["targets"]
            if item["name"] == binary_name and kind in item["kind"]
        )
    binary_name = target["name"]
    if kind in {"lib", "proc-macro"}:
        binary_id = package_name
    elif kind == "test":
        binary_id = f"{package_name}::{binary_name}"
    else:
        binary_id = f"{package_name}::bin/{binary_name}"
    return {
        "binary-id": binary_id,
        "package-name": package_name,
        "binary-name": binary_name,
        "kind": kind,
        "event-binary-alias": f"{package_name}::{binary_name}",
    }


def list_payload(tests: dict[str, bool], identity: dict[str, str]) -> str:
    return json.dumps(
        {
            "test-count": len(tests),
            "rust-suites": {
                identity["binary-id"]: {
                    "binary-id": identity["binary-id"],
                    "package-name": identity["package-name"],
                    "binary-name": identity["binary-name"],
                    "kind": identity["kind"],
                    "status": "listed",
                    "testcases": {
                        test_id: {
                            "kind": "test",
                            "ignored": ignored,
                            "filter-match": {"status": "matches"},
                        }
                        for test_id, ignored in tests.items()
                    },
                }
            },
        }
    )


def events(
    tests: dict[str, bool], event_binary_alias: str, failed: bool = False
) -> str:
    lines: list[str] = []
    for index, test_id in enumerate(tests):
        qualified_id = f"{event_binary_alias}${test_id}"
        lines.append(json.dumps({"type": "test", "event": "started", "name": qualified_id}))
        lines.append(
            json.dumps(
                {
                    "type": "test",
                    "event": "failed" if failed and index == 0 else "ok",
                    "name": qualified_id,
                }
            )
        )
    return "\n".join(lines)


if args[:1] == ["metadata"]:
    print(state.get("metadata_stdout", json.dumps(state["metadata"])))
    if state.get("metadata_stderr"):
        print(state["metadata_stderr"], file=sys.stderr)
    raise SystemExit(state.get("metadata_returncode", 0))

if args[:2] == ["nextest", "list"]:
    name = selector()
    raw = state.get("list_payloads", {}).get(name)
    print(
        raw
        if raw is not None
        else list_payload(
            state.get("listings", {}).get(name, state["default_listing"]),
            target_identity(),
        )
    )
    raise SystemExit(state.get("list_returncodes", {}).get(name, 0))

if args[:1] == ["build"]:
    binary = args[args.index("--bin") + 1]
    package = args[args.index("-p") + 1]
    executable = state.get("artifacts", {}).get(binary)
    if executable is None:
        print(json.dumps({"reason": "build-finished", "success": True}))
    else:
        package_id = next(item["id"] for item in state["metadata"]["packages"] if item["name"] == package)
        print(
            json.dumps(
                {
                    "reason": "compiler-artifact",
                    "package_id": package_id,
                    "target": {"name": binary, "kind": ["bin"]},
                    "executable": executable,
                }
            )
        )
    raise SystemExit(state.get("build_returncodes", {}).get(binary, 0))

if args[:2] == ["nextest", "run"]:
    name = selector()
    is_proof = "libtest-json-plus" in args
    configured = state.get("proof_runs", {}).get(name) if is_proof else None
    if configured is not None:
        if configured.get("stdout"):
            print(configured["stdout"])
        if configured.get("stderr"):
            print(configured["stderr"], file=sys.stderr)
        raise SystemExit(configured["returncode"])
    failed = name in state.get("failing_runs", [])
    if is_proof:
        print(
            events(
                state.get("listings", {}).get(name, state["default_listing"]),
                target_identity()["event-binary-alias"],
                failed,
            )
        )
        raise SystemExit(100 if failed else 0)
    if failed:
        print(f"failed {name}", file=sys.stderr)
        raise SystemExit(1)
    raise SystemExit(0)

print(f"unsupported fake cargo invocation: {args}", file=sys.stderr)
raise SystemExit(91)
"""


def _toml_value(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, str):
        return json.dumps(value)
    if isinstance(value, list):
        return "[" + ", ".join(_toml_value(item) for item in value) + "]"
    raise TypeError(f"unsupported TOML fixture value: {value!r}")


def _quoted_key(value: str) -> str:
    return json.dumps(value)


def render_manifest(data: dict[str, Any]) -> str:
    """Render the deliberately small manifest shape used by these CLI fixtures."""
    lines: list[str] = []
    for key, value in data.items():
        if key not in {"helpers", "targets", "gates"} and not isinstance(value, dict):
            lines.append(f"{key} = {_toml_value(value)}")

    for name, helper in data.get("helpers", {}).items():
        lines.extend(["", f"[helpers.{_quoted_key(name)}]"])
        lines.extend(f"{key} = {_toml_value(value)}" for key, value in helper.items())

    if data.get("targets") == {}:
        lines.extend(["", "[targets]"])
    for name, target in data.get("targets", {}).items():
        lines.extend(["", f"[targets.{_quoted_key(name)}]"])
        lines.extend(f"{key} = {_toml_value(value)}" for key, value in target.items())

    if data.get("gates") == {}:
        lines.extend(["", "[gates]"])
    for name, gate in data.get("gates", {}).items():
        scalar_items = [(key, value) for key, value in gate.items() if key != "steps"]
        if scalar_items:
            lines.extend(["", f"[gates.{_quoted_key(name)}]"])
            lines.extend(f"{key} = {_toml_value(value)}" for key, value in scalar_items)
        for step in gate.get("steps", []):
            lines.extend(["", f"[[gates.{_quoted_key(name)}.steps]]"])
            lines.extend(f"{key} = {_toml_value(value)}" for key, value in step.items())

    for key, value in data.items():
        if key in {"helpers", "targets", "gates"} or not isinstance(value, dict):
            continue
        lines.extend(["", f"[{key}]"])
        lines.extend(
            f"{item_key} = {_toml_value(item)}" for item_key, item in value.items()
        )
    return "\n".join(lines) + "\n"


def nextest_list_payload(
    tests: dict[str, bool],
    *,
    binary_id: str = "codex-core::all",
    package_name: str = "codex-core",
    binary_name: str = "all",
    kind: str = "test",
    status: str = "listed",
    suite_key: str | None = None,
) -> str:
    return json.dumps(
        {
            "test-count": len(tests),
            "rust-suites": {
                binary_id if suite_key is None else suite_key: {
                    "binary-id": binary_id,
                    "package-name": package_name,
                    "binary-name": binary_name,
                    "kind": kind,
                    "status": status,
                    "testcases": {
                        test_id: {
                            "kind": "test",
                            "ignored": ignored,
                            "filter-match": {"status": "matches"},
                        }
                        for test_id, ignored in tests.items()
                    },
                }
            },
        }
    )


def test_events(
    test_id: str,
    terminal: str | None,
    *,
    event_binary_alias: str = "codex-core::codex_core",
) -> str:
    qualified_id = f"{event_binary_alias}${test_id}"
    events = [{"type": "test", "event": "started", "name": qualified_id}]
    if terminal is not None:
        events.append({"type": "test", "event": terminal, "name": qualified_id})
    return "\n".join(json.dumps(event) for event in events)


def windows_fake_cargo_launcher() -> Path:
    """Build one tiny native launcher because CreateProcess cannot run .cmd files."""
    global _FAKE_CARGO_LAUNCHER_DIR
    if _FAKE_CARGO_LAUNCHER_DIR is not None:
        return Path(_FAKE_CARGO_LAUNCHER_DIR.name) / "cargo.exe"
    _FAKE_CARGO_LAUNCHER_DIR = tempfile.TemporaryDirectory(
        prefix="rust-runner-cargo-launcher-"
    )
    root = Path(_FAKE_CARGO_LAUNCHER_DIR.name)
    source = root / "launcher.rs"
    executable = root / "cargo.exe"
    source.write_text(
        """use std::env;
use std::process::{Command, exit};

fn main() {
    let python = env::var_os("KD4_FAKE_PYTHON").expect("KD4_FAKE_PYTHON");
    let script = env::var_os("KD4_FAKE_CARGO_SCRIPT").expect("KD4_FAKE_CARGO_SCRIPT");
    let status = Command::new(python)
        .arg(script)
        .args(env::args_os().skip(1))
        .status()
        .expect("start fake Cargo Python process");
    exit(status.code().unwrap_or(1));
}
""",
        encoding="utf-8",
        newline="\n",
    )
    subprocess.run(
        ["rustc", "--edition=2021", str(source), "-o", str(executable)],
        text=True,
        capture_output=True,
        check=True,
    )
    return executable


class CliRunnerTestCase(unittest.TestCase):
    """Shared subprocess fixture; tearDown prevents helper-only regressions."""

    def setUp(self) -> None:
        self._temporary = tempfile.TemporaryDirectory(prefix="rust-runner-cli-")
        self.addCleanup(self._temporary.cleanup)
        self.temp_dir = Path(self._temporary.name)
        self.manifest_path = self.temp_dir / "manifest.toml"
        self.state_path = self.temp_dir / "cargo-state.json"
        self.log_path = self.temp_dir / "cargo-log.jsonl"
        self.bin_dir = self.temp_dir / "bin"
        self.bin_dir.mkdir()
        self.fake_cargo = self.temp_dir / "fake_cargo.py"
        self.fake_cargo.write_text(FAKE_CARGO_SOURCE, encoding="utf-8", newline="\n")
        self.fake_cargo.chmod(0o755)
        if os.name == "nt":
            shutil.copy2(windows_fake_cargo_launcher(), self.bin_dir / "cargo.exe")
        else:
            shim = self.bin_dir / "cargo"
            shim.write_text(
                f'#!{sys.executable}\nexec(compile(open({str(self.fake_cargo)!r}, "rb").read(), {str(self.fake_cargo)!r}, "exec"))\n',
                encoding="utf-8",
                newline="\n",
            )
            shim.chmod(0o755)
        self.cli_invocations = 0
        self.last_calls: list[dict[str, Any]] = []
        self.target_dir = self.temp_dir / "lane-target"
        self.artifacts: dict[str, str] = {}
        for name in (
            "codex",
            "codex-code-mode-host",
            "test_stdio_server",
            "codex-command-runner",
        ):
            artifact = self.temp_dir / "artifacts" / f"{name}.exe"
            artifact.parent.mkdir(exist_ok=True)
            artifact.write_bytes(b"")
            self.artifacts[name] = str(artifact)

    def tearDown(self) -> None:
        self.assertGreater(
            self.cli_invocations,
            0,
            "each runner test must execute scripts/rust_test_runner.py as a subprocess",
        )

    def default_state(self) -> dict[str, Any]:
        return {
            "metadata": {
                "target_directory": str(self.temp_dir / "metadata-target"),
                "packages": copy.deepcopy(METADATA_PACKAGES),
            },
            "artifacts": dict(self.artifacts),
            "listings": {
                "--lib": {"mod::tests::alpha": False},
                "all": {"suite::mod::beta": False},
                "core_shard": {"mod::tests::alpha": False},
                "core_shard_two": {"suite::mod::beta": False},
            },
            "default_listing": {"mod::tests::alpha": False},
        }

    def invoke(
        self,
        args: list[str],
        *,
        manifest_data: dict[str, Any] | None = None,
        manifest_path: Path | None = None,
        state: dict[str, Any] | None = None,
        env_overrides: dict[str, str] | None = None,
        runner: Path = RUNNER,
        use_fake_cargo: bool = True,
    ) -> subprocess.CompletedProcess[str]:
        self.cli_invocations += 1
        active_manifest = manifest_path or self.manifest_path
        if manifest_data is not None or manifest_path is None:
            active_manifest.write_text(
                render_manifest(manifest_data or copy.deepcopy(MANIFEST_DATA)),
                encoding="utf-8",
                newline="\n",
            )
        command = [
            sys.executable,
            str(runner),
            "--manifest",
            str(active_manifest),
            *args,
        ]
        env = dict(os.environ)
        env.pop("CODEX_CARGO_LANE_TARGET_DIR", None)
        env.pop("CARGO_TARGET_DIR", None)
        env.pop("NEXTEST_PROFILE", None)
        env.pop("RUST_MIN_STACK", None)
        if use_fake_cargo:
            active_state = self.default_state()
            if state:
                active_state.update(copy.deepcopy(state))
            self.state_path.write_text(
                json.dumps(active_state), encoding="utf-8", newline="\n"
            )
            self.log_path.unlink(missing_ok=True)
            env["KD4_FAKE_CARGO_STATE"] = str(self.state_path)
            env["KD4_FAKE_CARGO_LOG"] = str(self.log_path)
            env["KD4_FAKE_PYTHON"] = sys.executable
            env["KD4_FAKE_CARGO_SCRIPT"] = str(self.fake_cargo)
            env["PATH"] = str(self.bin_dir) + os.pathsep + env.get("PATH", "")
        if env_overrides:
            env.update(env_overrides)
        result = subprocess.run(
            command,
            cwd=REPO_ROOT,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )
        self.last_calls = []
        if use_fake_cargo and self.log_path.exists():
            self.last_calls = [
                json.loads(line)
                for line in self.log_path.read_text(encoding="utf-8").splitlines()
                if line
            ]
        return result

    def calls(self, prefix: list[str]) -> list[dict[str, Any]]:
        return [
            call for call in self.last_calls if call["args"][: len(prefix)] == prefix
        ]

    def assert_error(
        self,
        args: list[str],
        message: str,
        *,
        manifest_data: dict[str, Any] | None = None,
        state: dict[str, Any] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        result = self.invoke(args, manifest_data=manifest_data, state=state)
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn(message, result.stderr)
        return result

    def metadata_with(self, edit: Any) -> dict[str, Any]:
        metadata = copy.deepcopy(self.default_state()["metadata"])
        edit(metadata)
        return metadata

    def proof(
        self,
        *,
        execution_id: str,
        state: dict[str, Any] | None = None,
        manifest_data: dict[str, Any] | None = None,
    ) -> tuple[subprocess.CompletedProcess[str], dict[str, Any]]:
        report = self.temp_dir / f"{execution_id}.json"
        result = self.invoke(
            [
                "run-gate-proof",
                "demo-gate",
                "--proof-report",
                str(report),
                "--proof-execution-id",
                execution_id,
            ],
            state=state,
            manifest_data=manifest_data,
        )
        self.assertTrue(report.is_file(), result.stdout + result.stderr)
        return result, json.loads(report.read_text(encoding="utf-8"))

    def isolated_runner(self) -> tuple[Path, Path]:
        root = self.temp_dir / "isolated"
        scripts = root / "scripts"
        scripts.mkdir(parents=True)
        runner = scripts / "rust_test_runner.py"
        shutil.copy2(RUNNER, runner)
        codex_rs = root / "codex-rs"
        codex_rs.mkdir()
        return runner, codex_rs


class ManifestSchemaTest(CliRunnerTestCase):
    def test_empty_targets_table_is_rejected_through_cli(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"] = {}
        self.assert_error(
            ["check-manifest"],
            "manifest.targets must be a non-empty table",
            manifest_data=data,
        )
        self.assertEqual(self.last_calls, [])

    def test_empty_gates_table_is_rejected_through_cli(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["gates"] = {}
        self.assert_error(
            ["check-manifest"],
            "manifest.gates must be a non-empty table",
            manifest_data=data,
        )
        self.assertEqual(self.last_calls, [])

    def test_unknown_top_level_key_is_rejected_through_cli(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["profiles"] = {}
        self.assert_error(
            ["check-manifest"], "unknown keys: profiles", manifest_data=data
        )

    def test_unknown_helper_key_is_rejected_through_cli(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["helpers"]["codex"]["binary"] = "codex"
        self.assert_error(
            ["check-manifest"],
            "helpers.codex contains unknown keys",
            manifest_data=data,
        )

    def test_unknown_target_key_is_rejected_through_cli(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"]["core_lib"]["profile"] = "fast"
        self.assert_error(
            ["check-manifest"], "unknown keys: profile", manifest_data=data
        )

    def test_unknown_gate_step_key_is_rejected_through_cli(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["gates"]["demo-gate"]["steps"][0]["retries"] = 0
        self.assert_error(
            ["check-manifest"], "unknown keys: retries", manifest_data=data
        )

    def test_version_must_match_the_supported_schema_through_cli(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["version"] = 2
        self.assert_error(
            ["check-manifest"], "manifest.version must be 1", manifest_data=data
        )

    def test_target_must_declare_exactly_one_selector_through_cli(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"]["core_lib"]["test"] = "all"
        self.assert_error(
            ["check-manifest"], "exactly one of lib, test, or bin", manifest_data=data
        )
        data = copy.deepcopy(MANIFEST_DATA)
        del data["targets"]["core_all"]["test"]
        self.assert_error(
            ["check-manifest"], "exactly one of lib, test, or bin", manifest_data=data
        )

    def test_target_helper_reference_must_exist_through_cli(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"]["core_lib"]["helpers"] = ["not-a-helper"]
        self.assert_error(
            ["check-manifest"], "unknown helper 'not-a-helper'", manifest_data=data
        )

    def test_gate_step_target_reference_must_exist_through_cli(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["gates"]["demo-gate"]["steps"][0]["target"] = "core_missing"
        self.assert_error(
            ["check-manifest"], "unknown target 'core_missing'", manifest_data=data
        )

    def test_gate_step_requires_expected_test_ids_through_cli(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["gates"]["demo-gate"]["steps"][0]["tests"] = []
        self.assert_error(["check-manifest"], "must not be empty", manifest_data=data)

    def test_duplicate_expected_test_ids_are_rejected_through_cli(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["gates"]["demo-gate"]["steps"][0]["tests"] = ["a::b", "a::b"]
        self.assert_error(
            ["check-manifest"], "contains duplicates: a::b", manifest_data=data
        )

    def test_helper_platform_must_be_known_through_cli(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["helpers"]["codex"]["platform"] = "solaris"
        self.assert_error(
            ["check-manifest"], "must be windows, linux, or macos", manifest_data=data
        )

    def test_feature_declarations_must_be_exact_unique_and_trusted_through_cli(self) -> None:
        cases = (
            ([""], "must be a non-empty string"),
            (["codex-core"], "must be exactly package/feature"),
            (
                [
                    "codex-core/completion-proof-test-store",
                    "codex-core/completion-proof-test-store",
                ],
                "contains duplicates",
            ),
            (["codex-core/arbitrary-test-hook"], "contains untrusted feature"),
        )
        for features, message in cases:
            with self.subTest(features=features):
                data = copy.deepcopy(MANIFEST_DATA)
                data["targets"]["core_all"]["features"] = features
                self.assert_error(
                    ["check-manifest"], message, manifest_data=data
                )


class MetadataValidationTest(CliRunnerTestCase):
    def test_helper_binary_must_exist_in_cargo_metadata_through_cli(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["helpers"]["codex"]["bin"] = "codex-renamed"
        self.assert_error(
            ["check-manifest"], "declares missing binary", manifest_data=data
        )

    def test_test_target_must_exist_in_cargo_metadata_through_cli(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"]["core_all"]["test"] = "gone"
        self.assert_error(
            ["check-manifest"], "declares missing test target", manifest_data=data
        )

    def test_bin_target_must_exist_in_cargo_metadata_through_cli(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"]["command_runner_bin"]["bin"] = "gone"
        self.assert_error(
            ["check-manifest"], "declares missing bin target", manifest_data=data
        )

    def test_unknown_package_is_rejected_through_cli(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"]["core_all"]["package"] = "codex-nope"
        self.assert_error(
            ["check-manifest"], "unknown Cargo package 'codex-nope'", manifest_data=data
        )

    def test_trusted_feature_must_exist_in_cargo_metadata_through_cli(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"]["core_all"]["features"] = [
            "codex-core/completion-proof-test-store"
        ]
        metadata = self.metadata_with(
            lambda value: value["packages"][0].__setitem__("features", {})
        )
        self.assert_error(
            ["check-manifest"],
            "declares unknown Cargo feature",
            manifest_data=data,
            state={"metadata": metadata},
        )


class NamedSelectionTest(CliRunnerTestCase):
    def test_unknown_target_name_fails_through_cli(self) -> None:
        self.assert_error(
            ["run-target", "nope"], "unknown named Rust test target 'nope'"
        )

    def test_unknown_plan_name_fails_through_cli(self) -> None:
        self.assert_error(["plan", "nope"], "unknown named Rust test target or gate")

    def test_selection_is_taken_from_the_manifest_through_cli(self) -> None:
        expected = {
            "core_all": ["-p", "codex-core", "--test", "all"],
            "core_lib": ["-p", "codex-core", "--lib"],
            "command_runner_bin": [
                "-p",
                "codex-windows-sandbox",
                "--bin",
                "codex-command-runner",
            ],
        }
        for name, selection in expected.items():
            result = self.invoke(["plan", name])
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout)["selection"], selection)

    def test_trusted_features_reach_plan_list_run_and_helper_build_through_cli(self) -> None:
        feature = "codex-core/completion-proof-test-store"
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"]["core_all"]["features"] = [feature]
        data["helpers"]["codex"]["features"] = [feature]

        result = self.invoke(["plan", "core_all"], manifest_data=data)
        self.assertEqual(result.returncode, 0, result.stderr)
        plan = json.loads(result.stdout)
        self.assertEqual(plan["features"], [feature])
        self.assertEqual(plan["helper_features"]["codex"], [feature])
        for command_name in ("list", "run"):
            self.assertIn("--features", plan[command_name])
            self.assertIn(feature, plan[command_name])
        codex_build = next(
            command for command in plan["builds"] if "codex-cli" in command
        )
        self.assertIn("--features", codex_build)
        self.assertIn(feature, codex_build)

        result = self.invoke(["list-targets"], manifest_data=data)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"target\tcore_all\tfeatures={feature}", result.stdout)


class FilteringArgumentPolicyTest(CliRunnerTestCase):
    def test_package_and_target_overrides_are_rejected_through_cli(self) -> None:
        for argv in (
            ["-p", "codex-tui"],
            ["--package=codex-tui"],
            ["--workspace"],
            ["--test", "all"],
            ["--test=all"],
            ["--lib"],
            ["--all-targets"],
            ["--manifest-path", "Cargo.toml"],
            ["--target-dir", "target"],
            ["--target-dir=target"],
        ):
            with self.subTest(argv=argv):
                self.assert_error(
                    ["run-target", "core_all", *argv], "cannot override a named target"
                )

    def test_no_tests_override_is_rejected_through_cli(self) -> None:
        for argv in (["--no-tests"], ["--no-tests=pass"], ["--no-tests=fail"]):
            with self.subTest(argv=argv):
                self.assert_error(
                    ["run-target", "core_all", *argv], "--no-tests is runner-owned"
                )

    def test_filtering_and_ignored_options_are_permitted_through_cli(self) -> None:
        argv = [
            "-E",
            "test(alpha)",
            "--run-ignored",
            "only",
            "suite::live_cli",
            "--",
            "--exact",
            "--skip",
            "slow",
        ]
        result = self.invoke(["run-target", "core_all", *argv])
        self.assertEqual(result.returncode, 0, result.stderr)
        command = self.calls(["nextest", "run"])[0]["args"]
        self.assertEqual(command[-len(argv) :], argv)

    def test_run_ignored_value_is_validated_through_cli(self) -> None:
        self.assert_error(
            ["run-target", "core_all", "--run-ignored", "sometimes"],
            "must be default, only, or all",
        )


class GenericRecipeGuardTest(CliRunnerTestCase):
    def test_every_codex_core_package_spelling_is_rejected_through_cli(self) -> None:
        for argv in (
            ["-p", "codex-core"],
            ["--package", "codex-core"],
            ["--package=codex-core"],
            ["-pcodex-core"],
            ["--no-fail-fast", "-p", "codex-core", "-E", "test(x)"],
        ):
            with self.subTest(argv=argv):
                result = self.invoke(["_guard-generic", "--", *argv])
                self.assertEqual(result.returncode, 2)
                self.assertIn("cannot select codex-core", result.stderr)
                self.assertEqual(self.last_calls, [])

    def test_other_packages_are_allowed_through_cli(self) -> None:
        for argv in (["-p", "codex-tui"], ["--package=codex-app-server"]):
            result = self.invoke(["_guard-generic", "--", *argv])
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(self.last_calls, [])

    def test_guard_is_token_aware_through_cli(self) -> None:
        for argv in (["-E", "package(codex-core)"], ["--", "-p", "codex-core"]):
            result = self.invoke(["_guard-generic", "--", *argv])
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(self.last_calls, [])


class NextestListParsingTest(CliRunnerTestCase):
    def test_ignored_state_is_preserved_through_cli(self) -> None:
        listings = {
            "all": {"a::b": False, "a::c": True},
            "core_shard": {"a::b": False, "a::c": True},
        }
        result = self.invoke(
            ["parity", "core_all", "core_shard"], state={"listings": listings}
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_invalid_json_is_rejected_through_cli(self) -> None:
        self.assert_error(
            ["run-target", "core_all"],
            "invalid JSON",
            state={"list_payloads": {"all": "not json"}},
        )

    def test_declared_count_mismatch_is_rejected_through_cli(self) -> None:
        payload = json.loads(nextest_list_payload({"a::b": False}))
        payload["test-count"] = 7
        self.assert_error(
            ["run-target", "core_all"],
            "does not match parsed count",
            state={"list_payloads": {"all": json.dumps(payload)}},
        )

    def test_missing_declared_count_is_rejected_through_cli(self) -> None:
        payload = json.loads(nextest_list_payload({"a::b": False}))
        del payload["test-count"]
        self.assert_error(
            ["run-target", "core_all"],
            "test-count is required",
            state={"list_payloads": {"all": json.dumps(payload)}},
        )

    def test_boolean_declared_count_is_rejected_through_cli(self) -> None:
        payload = json.loads(nextest_list_payload({"a::b": False}))
        payload["test-count"] = True
        self.assert_error(
            ["run-target", "core_all"],
            "test-count must be an integer",
            state={"list_payloads": {"all": json.dumps(payload)}},
        )

    def test_string_declared_count_is_rejected_through_cli(self) -> None:
        payload = json.loads(nextest_list_payload({"a::b": False}))
        payload["test-count"] = "1"
        self.assert_error(
            ["run-target", "core_all"],
            "test-count must be an integer",
            state={"list_payloads": {"all": json.dumps(payload)}},
        )

    def test_negative_declared_count_is_rejected_through_cli(self) -> None:
        payload = json.loads(nextest_list_payload({"a::b": False}))
        payload["test-count"] = -1
        self.assert_error(
            ["run-target", "core_all"],
            "test-count must be nonnegative",
            state={"list_payloads": {"all": json.dumps(payload)}},
        )

    def test_empty_rust_suite_identity_is_rejected_through_cli(self) -> None:
        payload = json.loads(nextest_list_payload({"a::b": False}, binary_id=""))
        self.assert_error(
            ["run-target", "core_all"],
            "nextest rust-suite ID must be a non-empty string",
            state={"list_payloads": {"all": json.dumps(payload)}},
        )

    def test_rust_suite_identity_with_separator_is_rejected_through_cli(self) -> None:
        payload = nextest_list_payload(
            {"a::b": False}, binary_id="codex-core$forged"
        )
        self.assert_error(
            ["run-target", "core_all"],
            "rust-suite ID 'codex-core$forged' must not contain '$'",
            state={"list_payloads": {"all": payload}},
        )

    def test_missing_suite_binary_id_is_rejected_through_cli(self) -> None:
        payload = json.loads(nextest_list_payload({"a::b": False}))
        del payload["rust-suites"]["codex-core::all"]["binary-id"]
        self.assert_error(
            ["run-target", "core_all"],
            "binary-id must be a non-empty string",
            state={"list_payloads": {"all": json.dumps(payload)}},
        )

    def test_missing_suite_package_name_is_rejected_through_cli(self) -> None:
        payload = json.loads(nextest_list_payload({"a::b": False}))
        del payload["rust-suites"]["codex-core::all"]["package-name"]
        self.assert_error(
            ["run-target", "core_all"],
            "package-name must be a non-empty string",
            state={"list_payloads": {"all": json.dumps(payload)}},
        )

    def test_missing_suite_binary_name_is_rejected_through_cli(self) -> None:
        payload = json.loads(nextest_list_payload({"a::b": False}))
        del payload["rust-suites"]["codex-core::all"]["binary-name"]
        self.assert_error(
            ["run-target", "core_all"],
            "binary-name must be a non-empty string",
            state={"list_payloads": {"all": json.dumps(payload)}},
        )

    def test_missing_suite_kind_is_rejected_through_cli(self) -> None:
        payload = json.loads(nextest_list_payload({"a::b": False}))
        del payload["rust-suites"]["codex-core::all"]["kind"]
        self.assert_error(
            ["run-target", "core_all"],
            "kind must be a non-empty string",
            state={"list_payloads": {"all": json.dumps(payload)}},
        )

    def test_missing_suite_status_is_rejected_through_cli(self) -> None:
        payload = json.loads(nextest_list_payload({"a::b": False}))
        del payload["rust-suites"]["codex-core::all"]["status"]
        self.assert_error(
            ["run-target", "core_all"],
            "status must be a non-empty string",
            state={"list_payloads": {"all": json.dumps(payload)}},
        )

    def test_unknown_suite_status_is_rejected_through_cli(self) -> None:
        payload = nextest_list_payload({"a::b": False}, status="cached")
        self.assert_error(
            ["run-target", "core_all"],
            "status must be 'listed', found 'cached'",
            state={"list_payloads": {"all": payload}},
        )

    def test_unknown_suite_kind_is_rejected_through_cli(self) -> None:
        payload = nextest_list_payload({"a::b": False}, kind="bench")
        self.assert_error(
            ["run-target", "core_all"],
            "unsupported nextest Rust suite kind 'bench'",
            state={"list_payloads": {"all": payload}},
        )

    def test_suite_map_key_must_equal_binary_id_through_cli(self) -> None:
        payload = nextest_list_payload(
            {"a::b": False}, suite_key="codex-core::lookalike"
        )
        self.assert_error(
            ["run-target", "core_all"],
            "map key 'codex-core::lookalike' does not match binary-id 'codex-core::all'",
            state={"list_payloads": {"all": payload}},
        )

    def test_suite_binary_id_must_match_stable_derivation_through_cli(self) -> None:
        payload = nextest_list_payload(
            {"a::b": False},
            binary_id="codex-core::lookalike",
            binary_name="all",
        )
        self.assert_error(
            ["run-target", "core_all"],
            "does not match the stable test identity 'codex-core::all'",
            state={"list_payloads": {"all": payload}},
        )

    def test_suite_package_must_match_requested_target_through_cli(self) -> None:
        payload = nextest_list_payload(
            {"a::b": False},
            binary_id="other-package::all",
            package_name="other-package",
        )
        self.assert_error(
            ["run-target", "core_all"],
            "expected package='codex-core'",
            state={"list_payloads": {"all": payload}},
        )

    def test_suite_binary_must_match_requested_target_through_cli(self) -> None:
        payload = nextest_list_payload(
            {"a::b": False},
            binary_id="codex-core::other",
            binary_name="other",
        )
        self.assert_error(
            ["run-target", "core_all"],
            "expected package='codex-core', binary='all'",
            state={"list_payloads": {"all": payload}},
        )

    def test_suite_kind_must_match_cargo_metadata_through_cli(self) -> None:
        payload = nextest_list_payload(
            {"a::b": False},
            binary_id="codex-core",
            binary_name="codex_core",
            kind="proc-macro",
        )
        self.assert_error(
            ["run-target", "core_lib"],
            "kind='lib'",
            state={"list_payloads": {"--lib": payload}},
        )

    def test_identity_metadata_with_separator_is_rejected_through_cli(self) -> None:
        payload = nextest_list_payload(
            {"a::b": False},
            binary_id="codex-core::all",
            package_name="codex$core",
        )
        self.assert_error(
            ["run-target", "core_all"],
            "package-name 'codex$core' must not contain '$'",
            state={"list_payloads": {"all": payload}},
        )

    def test_semantic_identity_with_separator_is_rejected_through_cli(self) -> None:
        payload = nextest_list_payload({"a$forged": False})
        self.assert_error(
            ["run-target", "core_all"],
            "test ID 'a$forged' must not contain '$'",
            state={"list_payloads": {"all": payload}},
        )

    def test_retry_shaped_list_identity_is_rejected_through_cli(self) -> None:
        payload = nextest_list_payload({"a::b#2": False})
        self.assert_error(
            ["run-target", "core_all"],
            "retry-attempt suffix",
            state={"list_payloads": {"all": payload}},
        )

    def test_stress_shaped_list_identity_is_rejected_through_cli(self) -> None:
        payload = nextest_list_payload({"a::b@stress-1": False})
        self.assert_error(
            ["run-target", "core_all"],
            "stress-attempt suffix",
            state={"list_payloads": {"all": payload}},
        )

    def test_repeated_semantic_id_across_binaries_is_rejected_through_cli(
        self,
    ) -> None:
        testcase = {
            "kind": "test",
            "ignored": False,
            "filter-match": {"status": "matches"},
        }
        payload = {
            "test-count": 2,
            "rust-suites": {
                "codex-core::one": {
                    "binary-id": "codex-core::one",
                    "package-name": "codex-core",
                    "binary-name": "one",
                    "kind": "test",
                    "status": "listed",
                    "testcases": {"same::test": testcase},
                },
                "codex-core::two": {
                    "binary-id": "codex-core::two",
                    "package-name": "codex-core",
                    "binary-name": "two",
                    "kind": "test",
                    "status": "listed",
                    "testcases": {"same::test": testcase},
                },
            },
        }
        self.assert_error(
            ["run-target", "core_all"],
            "nextest listed duplicate test ID 'same::test'",
            state={"list_payloads": {"all": json.dumps(payload)}},
        )

    def test_event_alias_ambiguity_across_suite_kinds_is_rejected_through_cli(
        self,
    ) -> None:
        payload = {
            "test-count": 2,
            "rust-suites": {
                "codex-core::same": json.loads(
                    nextest_list_payload(
                        {"test::case": False},
                        binary_id="codex-core::same",
                        binary_name="same",
                    )
                )["rust-suites"]["codex-core::same"],
                "codex-core::bin/same": json.loads(
                    nextest_list_payload(
                        {"bin::case": False},
                        binary_id="codex-core::bin/same",
                        binary_name="same",
                        kind="bin",
                    )
                )["rust-suites"]["codex-core::bin/same"],
            },
        }
        self.assert_error(
            ["run-target", "core_all"],
            "event binary alias 'codex-core::same' is ambiguous",
            state={"list_payloads": {"all": json.dumps(payload)}},
        )

    def test_missing_ignored_state_is_rejected_through_cli(self) -> None:
        payload = json.loads(nextest_list_payload({"a::b": False}))
        del payload["rust-suites"]["codex-core::all"]["testcases"]["a::b"][
            "ignored"
        ]
        self.assert_error(
            ["run-target", "core_all"],
            "ignored state for 'a::b' is required",
            state={"list_payloads": {"all": json.dumps(payload)}},
        )

    def test_non_boolean_ignored_state_is_rejected_through_cli(self) -> None:
        payload = json.loads(nextest_list_payload({"a::b": False}))
        payload["rust-suites"]["codex-core::all"]["testcases"]["a::b"][
            "ignored"
        ] = 0
        self.assert_error(
            ["run-target", "core_all"],
            "ignored state for 'a::b' must be boolean",
            state={"list_payloads": {"all": json.dumps(payload)}},
        )

    def test_missing_filter_match_is_rejected_through_cli(self) -> None:
        payload = json.loads(nextest_list_payload({"a::b": False}))
        del payload["rust-suites"]["codex-core::all"]["testcases"]["a::b"][
            "filter-match"
        ]
        self.assert_error(
            ["run-target", "core_all"],
            "filter-match is required",
            state={"list_payloads": {"all": json.dumps(payload)}},
        )

    def test_missing_filter_match_status_is_rejected_through_cli(self) -> None:
        payload = json.loads(nextest_list_payload({"a::b": False}))
        payload["rust-suites"]["codex-core::all"]["testcases"]["a::b"][
            "filter-match"
        ] = {}
        self.assert_error(
            ["run-target", "core_all"],
            "filter-match.status must be a non-empty string",
            state={"list_payloads": {"all": json.dumps(payload)}},
        )

    def test_unknown_filter_match_status_is_rejected_through_cli(self) -> None:
        payload = json.loads(nextest_list_payload({"a::b": False}))
        payload["rust-suites"]["codex-core::all"]["testcases"]["a::b"][
            "filter-match"
        ] = {"status": "cached"}
        self.assert_error(
            ["run-target", "core_all"],
            "filter-match.status has unsupported value 'cached'",
            state={"list_payloads": {"all": json.dumps(payload)}},
        )

    def test_only_filter_matches_are_returned_as_selected_through_cli(self) -> None:
        payload = json.loads(
            nextest_list_payload({"a::selected": False, "a::other": True})
        )
        cases = payload["rust-suites"]["codex-core::all"]["testcases"]
        cases["a::selected"]["filter-match"] = {"status": "matches"}
        cases["a::other"]["filter-match"] = {"status": "mismatch", "reason": "string"}
        result = self.invoke(
            ["run-target", "core_all", "-E", "test(selected)"],
            state={"list_payloads": {"all": json.dumps(payload)}},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.calls(["nextest", "run"])), 1)


class NextestEventParsingTest(CliRunnerTestCase):
    def test_only_explicit_started_and_terminal_events_count_through_cli(self) -> None:
        lib = "\n".join(
            [
                json.dumps({"type": "suite", "event": "started"}),
                test_events("mod::tests::alpha", "ok"),
            ]
        )
        result, report = self.proof(
            execution_id="explicit-events",
            state={"proof_runs": {"--lib": {"returncode": 0, "stdout": lib}}},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            report["executed_ids"], ["mod::tests::alpha", "suite::mod::beta"]
        )
        self.assertEqual(
            [item["outcome"] for item in report["outcomes"]], ["passed", "passed"]
        )

    def test_ignored_start_noise_does_not_count_or_fail_proof_through_cli(
        self,
    ) -> None:
        ghost = "codex-core::codex_core$agent::role::tests::filtered_out"
        ignored = "codex-core::codex_core$unified_exec::tests::ignored_noise"
        lib = "\n".join(
            [
                json.dumps({"type": "test", "event": "started", "name": ghost}),
                json.dumps({"type": "test", "event": "started", "name": ignored}),
                json.dumps({"type": "test", "event": "ignored", "name": ignored}),
                test_events("mod::tests::alpha", "ok"),
            ]
        )
        result, report = self.proof(
            execution_id="ignored-start-noise",
            state={"proof_runs": {"--lib": {"returncode": 0, "stdout": lib}}},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["classification"], "confirmed_pass")
        self.assertEqual(
            report["executed_ids"], ["mod::tests::alpha", "suite::mod::beta"]
        )
        self.assertEqual(
            [item["outcome"] for item in report["outcomes"]], ["passed", "passed"]
        )

    def test_missing_terminal_result_stays_unknown_through_cli(self) -> None:
        result, report = self.proof(
            execution_id="missing-terminal-parser",
            state={
                "proof_runs": {
                    "--lib": {
                        "returncode": 0,
                        "stdout": test_events("mod::tests::alpha", None),
                    }
                }
            },
        )
        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["classification"], "pre_result_error")
        self.assertNotIn("mod::tests::alpha", report["executed_ids"])

    def test_terminal_without_start_is_rejected_through_cli(self) -> None:
        payload = json.dumps(
            {
                "type": "test",
                "event": "ok",
                "name": "codex-core::codex_core$a::orphan",
            }
        )
        result, report = self.proof(
            execution_id="orphan-terminal",
            state={"proof_runs": {"--lib": {"returncode": 0, "stdout": payload}}},
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn(
            "terminal result before its start for codex-core::codex_core$a::orphan",
            report["diagnostic"],
        )

    def test_duplicate_terminal_is_rejected_without_overwriting_first_through_cli(
        self,
    ) -> None:
        payload = "\n".join(
            [
                test_events("mod::tests::alpha", "ok"),
                json.dumps(
                    {
                        "type": "test",
                        "event": "failed",
                        "name": "codex-core::codex_core$mod::tests::alpha",
                    }
                ),
            ]
        )
        result, report = self.proof(
            execution_id="duplicate-terminal",
            state={"proof_runs": {"--lib": {"returncode": 0, "stdout": payload}}},
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn(
            "duplicate terminal result for codex-core::codex_core$mod::tests::alpha",
            report["diagnostic"],
        )
        self.assertIn(
            {"id": "mod::tests::alpha", "outcome": "passed"}, report["outcomes"]
        )

    def test_duplicate_start_is_rejected_through_cli(self) -> None:
        qualified_id = "codex-core::codex_core$mod::tests::alpha"
        payload = "\n".join(
            json.dumps({"type": "test", "event": event, "name": qualified_id})
            for event in ("started", "started", "ok")
        )
        result, report = self.proof(
            execution_id="duplicate-start",
            state={"proof_runs": {"--lib": {"returncode": 0, "stdout": payload}}},
        )
        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["classification"], "pre_result_error")
        self.assertIn(f"duplicate start for {qualified_id}", report["diagnostic"])

    def test_bare_event_identity_is_rejected_through_cli(self) -> None:
        payload = "\n".join(
            [
                test_events("mod::tests::alpha", "ok"),
                json.dumps(
                    {"type": "test", "event": "started", "name": "bare::test"}
                ),
            ]
        )
        result, report = self.proof(
            execution_id="bare-event-name",
            state={"proof_runs": {"--lib": {"returncode": 0, "stdout": payload}}},
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn(
            "must be exactly <package-name>::<binary-name>$<semantic-test-id>",
            report["diagnostic"],
        )

    def test_malformed_event_identity_is_rejected_through_cli(self) -> None:
        payload = "\n".join(
            [
                test_events("mod::tests::alpha", "ok"),
                json.dumps(
                    {
                        "type": "test",
                        "event": "started",
                        "name": "$semantic",
                    }
                ),
            ]
        )
        result, report = self.proof(
            execution_id="malformed-event-name",
            state={"proof_runs": {"--lib": {"returncode": 0, "stdout": payload}}},
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn(
            "must be exactly <package-name>::<binary-name>$<semantic-test-id>",
            report["diagnostic"],
        )

    def test_terminal_before_later_start_is_pre_result_through_cli(self) -> None:
        payload = "\n".join(
            [
                json.dumps(
                    {
                        "type": "test",
                        "event": "ok",
                        "name": "codex-core::codex_core$mod::tests::alpha",
                    }
                ),
                json.dumps(
                    {
                        "type": "test",
                        "event": "started",
                        "name": "codex-core::codex_core$mod::tests::alpha",
                    }
                ),
            ]
        )
        result, report = self.proof(
            execution_id="terminal-before-later-start",
            state={"proof_runs": {"--lib": {"returncode": 0, "stdout": payload}}},
        )
        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["classification"], "pre_result_error")
        self.assertIn(
            "terminal result before its start for "
            "codex-core::codex_core$mod::tests::alpha",
            report["diagnostic"],
        )

    def test_suite_binary_id_cannot_substitute_for_libtest_alias_through_cli(
        self,
    ) -> None:
        payload = test_events(
            "mod::tests::alpha",
            "ok",
            event_binary_alias="codex-core",
        )
        result, report = self.proof(
            execution_id="suite-id-is-not-event-alias",
            state={"proof_runs": {"--lib": {"returncode": 0, "stdout": payload}}},
        )
        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["classification"], "pre_result_error")
        self.assertNotIn("mod::tests::alpha", report["executed_ids"])
        self.assertIn(
            "<package-name>::<binary-name>$<semantic-test-id>",
            report["diagnostic"],
        )

    def test_error_event_is_not_a_confirmed_failure_through_cli(self) -> None:
        result, report = self.proof(
            execution_id="unsupported-error-event",
            state={
                "proof_runs": {
                    "--lib": {
                        "returncode": 101,
                        "stdout": test_events("mod::tests::alpha", "error"),
                    }
                }
            },
        )
        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["classification"], "pre_result_error")
        self.assertIn("unsupported test event 'error'", report["diagnostic"])

    def test_timed_out_event_is_not_a_confirmed_failure_through_cli(self) -> None:
        result, report = self.proof(
            execution_id="unsupported-timed-out-event",
            state={
                "proof_runs": {
                    "--lib": {
                        "returncode": 101,
                        "stdout": test_events("mod::tests::alpha", "timed_out"),
                    }
                }
            },
        )
        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["classification"], "pre_result_error")
        self.assertIn("unsupported test event 'timed_out'", report["diagnostic"])

    def test_skipped_event_is_not_a_confirmed_failure_through_cli(self) -> None:
        result, report = self.proof(
            execution_id="unsupported-skipped-event",
            state={
                "proof_runs": {
                    "--lib": {
                        "returncode": 101,
                        "stdout": test_events("mod::tests::alpha", "skipped"),
                    }
                }
            },
        )
        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["classification"], "pre_result_error")
        self.assertIn("unsupported test event 'skipped'", report["diagnostic"])

    def test_intended_ignored_event_is_pre_result_through_cli(self) -> None:
        result, report = self.proof(
            execution_id="intended-ignored-event",
            state={
                "proof_runs": {
                    "--lib": {
                        "returncode": 0,
                        "stdout": test_events("mod::tests::alpha", "ignored"),
                    }
                }
            },
        )
        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["classification"], "pre_result_error")
        self.assertNotIn("mod::tests::alpha", report["executed_ids"])
        self.assertIn("invalid terminal outcomes", report["diagnostic"])

    def test_retry_shaped_event_identity_is_rejected_through_cli(self) -> None:
        result, report = self.proof(
            execution_id="retry-shaped-event",
            state={
                "proof_runs": {
                    "--lib": {
                        "returncode": 0,
                        "stdout": test_events("mod::tests::alpha#2", "ok"),
                    }
                }
            },
        )
        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["classification"], "pre_result_error")
        self.assertIn("retry-attempt suffix", report["diagnostic"])

    def test_stress_shaped_event_identity_is_rejected_through_cli(self) -> None:
        result, report = self.proof(
            execution_id="stress-shaped-event",
            state={
                "proof_runs": {
                    "--lib": {
                        "returncode": 0,
                        "stdout": test_events("mod::tests::alpha@stress-1", "ok"),
                    }
                }
            },
        )
        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["classification"], "pre_result_error")
        self.assertIn("stress-attempt suffix", report["diagnostic"])


class HelperUnionTest(CliRunnerTestCase):
    def test_target_helpers_are_exactly_the_declared_set_through_cli(self) -> None:
        result = self.invoke(["plan", "core_all"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads(result.stdout)["helpers"],
            ["codex", "codex-code-mode-host", "test_stdio_server"],
        )

    def test_gate_helpers_are_the_deduplicated_union_of_its_steps_through_cli(
        self,
    ) -> None:
        result = self.invoke(["plan", "demo-gate"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads(result.stdout)["helpers"],
            [
                "codex",
                "codex-command-runner",
                "codex-code-mode-host",
                "test_stdio_server",
            ],
        )

    def test_platform_scoped_helpers_are_dropped_off_platform_through_cli(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["helpers"]["codex-command-runner"]["platform"] = (
            "linux" if os.name == "nt" else "windows"
        )
        result = self.invoke(["plan", "core_lib"], manifest_data=data)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["helpers"], ["codex"])


class TargetDirectoryPropagationTest(CliRunnerTestCase):
    def test_every_cargo_command_targets_the_active_lane_through_cli(self) -> None:
        result = self.invoke(["--target-dir", str(self.target_dir), "plan", "core_all"])
        self.assertEqual(result.returncode, 0, result.stderr)
        plan = json.loads(result.stdout)
        self.assertEqual(plan["target_dir"], str(self.target_dir.resolve()))
        for command in [plan["list"], plan["run"], *plan["builds"]]:
            self.assertEqual(
                command[command.index("--target-dir") + 1],
                str(self.target_dir.resolve()),
            )

    def test_metadata_target_directory_is_the_default_through_cli(self) -> None:
        result = self.invoke(["plan", "core_all"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads(result.stdout)["target_dir"],
            str((self.temp_dir / "metadata-target").resolve()),
        )

    def test_relative_codex_rs_target_dir_is_rejected_before_cargo_through_cli(
        self,
    ) -> None:
        runner, codex_rs = self.isolated_runner()
        leaf = "target-private-state-test"
        variants = [
            f"codex-rs/{leaf}",
            f"./codex-rs/{leaf}",
            f"CoDeX-Rs/{leaf}",
            f"other/../codex-rs/{leaf}",
        ]
        if os.altsep is not None:
            variants.append(f"codex-rs{os.altsep}{leaf}")
        for target_dir in variants:
            for args in (
                ["--target-dir", target_dir, "run-target", "core_all"],
                ["run-target", "--target-dir", target_dir, "core_all"],
            ):
                with self.subTest(target_dir=target_dir, args=args):
                    result = self.invoke(args, runner=runner)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn(
                        "use an absolute path or a workspace-relative target-* path",
                        result.stderr,
                    )
                    self.assertEqual(self.last_calls, [])
                    self.assertFalse((codex_rs / "codex-rs" / leaf).exists())

    def test_effective_environment_target_dir_is_validated_before_cargo_through_cli(
        self,
    ) -> None:
        runner, codex_rs = self.isolated_runner()
        leaf = "target-private-state-env"
        for variable in ("CODEX_CARGO_LANE_TARGET_DIR", "CARGO_TARGET_DIR"):
            with self.subTest(variable=variable):
                result = self.invoke(
                    ["run-target", "core_all"],
                    runner=runner,
                    env_overrides={variable: f"codex-rs/{leaf}"},
                )
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn(
                    "use an absolute path or a workspace-relative target-* path",
                    result.stderr,
                )
                self.assertEqual(self.last_calls, [])
                self.assertFalse((codex_rs / "codex-rs" / leaf).exists())

    def test_cli_target_dir_preserves_precedence_over_environment_through_cli(
        self,
    ) -> None:
        runner, codex_rs = self.isolated_runner()
        result = self.invoke(
            ["--target-dir", "target-cli", "plan", "core_all"],
            runner=runner,
            env_overrides={
                "CODEX_CARGO_LANE_TARGET_DIR": "codex-rs/target-invalid-lane",
                "CARGO_TARGET_DIR": "codex-rs/target-invalid-cargo",
            },
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads(result.stdout)["target_dir"],
            str((codex_rs / "target-cli").resolve()),
        )

    def test_safe_relative_target_dir_is_anchored_once_through_cli(self) -> None:
        runner, codex_rs = self.isolated_runner()
        result = self.invoke(
            ["run-target", "--target-dir", "target-relative", "core_all"],
            runner=runner,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        for call in self.last_calls:
            if call["args"][:1] == ["metadata"]:
                continue
            target_index = call["args"].index("--target-dir")
            self.assertEqual(
                call["args"][target_index + 1],
                str((codex_rs / "target-relative").resolve()),
            )


class RunTargetTest(CliRunnerTestCase):
    def test_run_forces_no_tests_fail_through_cli(self) -> None:
        result = self.invoke(["run-target", "core_all"])
        self.assertEqual(result.returncode, 0, result.stderr)
        command = self.calls(["nextest", "run"])[0]["args"]
        self.assertIn("--no-tests=fail", command)

    def test_local_run_can_preserve_no_fail_fast_behavior_through_cli(self) -> None:
        result = self.invoke(["run-target", "core_all", "--no-fail-fast"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--no-fail-fast", self.calls(["nextest", "run"])[0]["args"])

    def test_zero_selected_tests_fails_before_anything_is_built_through_cli(
        self,
    ) -> None:
        result = self.assert_error(
            ["run-target", "core_all"],
            "selected zero tests",
            state={"listings": {"all": {}}},
        )
        self.assertEqual(result.returncode, 2)
        self.assertEqual(self.calls(["build"]), [])
        self.assertEqual(self.calls(["nextest", "run"]), [])

    def test_helper_environment_exports_dashed_and_underscored_aliases_through_cli(
        self,
    ) -> None:
        result = self.invoke(["run-target", "core_all"])
        self.assertEqual(result.returncode, 0, result.stderr)
        env = self.calls(["nextest", "run"])[0]["env"]
        env = {key.casefold(): value for key, value in env.items()}
        expected = str(Path(self.artifacts["codex-code-mode-host"]).resolve())
        self.assertEqual(env["cargo_bin_exe_codex-code-mode-host"], expected)
        self.assertEqual(env["cargo_bin_exe_codex_code_mode_host"], expected)
        self.assertEqual(
            env["cargo_bin_exe_test_stdio_server"],
            str(Path(self.artifacts["test_stdio_server"]).resolve()),
        )

    def test_only_declared_helpers_are_built_through_cli(self) -> None:
        result = self.invoke(["run-target", "core_shard"])
        self.assertEqual(result.returncode, 0, result.stderr)
        built = [
            call["args"][call["args"].index("--bin") + 1]
            for call in self.calls(["build"])
        ]
        self.assertEqual(built, ["codex"])

    def test_missing_helper_artifact_fails_through_cli(self) -> None:
        artifacts = dict(self.artifacts)
        del artifacts["codex-code-mode-host"]
        self.assert_error(
            ["run-target", "core_all"],
            "did not produce exactly one executable",
            state={"artifacts": artifacts},
        )

    def test_caller_cannot_widen_the_named_selection_through_cli(self) -> None:
        self.assert_error(
            ["run-target", "core_all", "-p", "codex-tui"],
            "cannot override a named target",
        )
        self.assertEqual(self.calls(["build"]), [])
        self.assertEqual(self.calls(["nextest", "list"]), [])
        self.assertEqual(self.calls(["nextest", "run"]), [])


class RunGateTest(CliRunnerTestCase):
    @staticmethod
    def matching_listings() -> dict[str, dict[str, bool]]:
        return {
            "--lib": {"mod::tests::alpha": False},
            "all": {"suite::mod::beta": False},
        }

    def test_matching_test_ids_run_every_step_through_cli(self) -> None:
        result = self.invoke(
            ["run-gate", "demo-gate"], state={"listings": self.matching_listings()}
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.calls(["nextest", "run"])), 2)

    def test_missing_expected_test_id_fails_the_gate_through_cli(self) -> None:
        listings = self.matching_listings()
        listings["all"] = {"suite::mod::unrelated": False}
        self.assert_error(
            ["run-gate", "demo-gate"], "wrong test-ID set", state={"listings": listings}
        )
        self.assertEqual(self.calls(["nextest", "run"]), [])

    def test_unexpected_test_id_fails_the_gate_through_cli(self) -> None:
        listings = self.matching_listings()
        listings["all"] = {"suite::mod::beta": False, "suite::mod::extra": False}
        result = self.assert_error(
            ["run-gate", "demo-gate"],
            "unexpected=['suite::mod::extra']",
            state={"listings": listings},
        )
        self.assertEqual(result.returncode, 2)

    def test_gate_verifies_every_step_before_running_any_through_cli(self) -> None:
        listings = self.matching_listings()
        listings["all"] = {"suite::mod::beta": False, "suite::mod::extra": False}
        self.assert_error(
            ["run-gate", "demo-gate"], "wrong test-ID set", state={"listings": listings}
        )
        self.assertEqual(self.calls(["nextest", "run"]), [])
        self.assertEqual(self.calls(["build"]), [])


class RunGateProofTest(CliRunnerTestCase):
    @staticmethod
    def one_step_manifest(target: str, test_id: str) -> dict[str, Any]:
        data = copy.deepcopy(MANIFEST_DATA)
        data["gates"]["demo-gate"]["steps"] = [
            {"target": target, "tests": [test_id]}
        ]
        return data

    def test_fresh_structured_events_prove_every_gate_test_executed_through_cli(
        self,
    ) -> None:
        result, report = self.proof(execution_id="fresh-execution")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["classification"], "confirmed_pass")
        self.assertEqual(
            report["intended_ids"], ["mod::tests::alpha", "suite::mod::beta"]
        )
        self.assertEqual(report["selected_ids"], report["intended_ids"])
        self.assertEqual(report["executed_ids"], report["intended_ids"])
        self.assertEqual(
            report["outcomes"],
            [
                {"id": "mod::tests::alpha", "outcome": "passed"},
                {"id": "suite::mod::beta", "outcome": "passed"},
            ],
        )
        for call in self.calls(["nextest", "run"]):
            command = call["args"]
            self.assertIn("--no-fail-fast", command)
            self.assertEqual(command[command.index("--retries") + 1], "0")
            self.assertEqual(
                command[command.index("--message-format") + 1], "libtest-json-plus"
            )

    def test_library_suite_uses_its_distinct_event_alias_through_cli(self) -> None:
        test_id = "mod::tests::alpha"
        result, report = self.proof(
            execution_id="library-event-alias",
            manifest_data=self.one_step_manifest("core_lib", test_id),
            state={"listings": {"--lib": {test_id: False}}},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["classification"], "confirmed_pass")
        self.assertEqual(report["executed_ids"], [test_id])
        self.assertIn("--lib", self.calls(["nextest", "run"])[0]["args"])

    def test_integration_suite_uses_authoritative_identity_through_cli(self) -> None:
        test_id = "suite::mod::beta"
        result, report = self.proof(
            execution_id="integration-event-alias",
            manifest_data=self.one_step_manifest("core_all", test_id),
            state={"listings": {"all": {test_id: False}}},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["classification"], "confirmed_pass")
        self.assertEqual(report["executed_ids"], [test_id])
        command = self.calls(["nextest", "run"])[0]["args"]
        self.assertEqual(command[command.index("--test") + 1], "all")

    def test_binary_suite_uses_its_distinct_event_alias_through_cli(self) -> None:
        test_id = "command::tests::runs"
        result, report = self.proof(
            execution_id="binary-event-alias",
            manifest_data=self.one_step_manifest("command_runner_bin", test_id),
            state={"listings": {"codex-command-runner": {test_id: False}}},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["classification"], "confirmed_pass")
        self.assertEqual(report["executed_ids"], [test_id])
        command = self.calls(["nextest", "run"])[0]["args"]
        self.assertEqual(
            command[command.index("--bin") + 1], "codex-command-runner"
        )

    def test_proc_macro_suite_requires_matching_cargo_metadata_through_cli(
        self,
    ) -> None:
        test_id = "macro::tests::expands"
        metadata = copy.deepcopy(self.default_state()["metadata"])
        codex_core = next(
            package
            for package in metadata["packages"]
            if package["name"] == "codex-core"
        )
        codex_core["targets"][0]["kind"] = ["proc-macro"]
        result, report = self.proof(
            execution_id="proc-macro-event-alias",
            manifest_data=self.one_step_manifest("core_lib", test_id),
            state={
                "metadata": metadata,
                "listings": {"--lib": {test_id: False}},
            },
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["classification"], "confirmed_pass")
        self.assertEqual(report["executed_ids"], [test_id])

    def test_success_exit_without_terminal_result_is_pre_result_through_cli(
        self,
    ) -> None:
        result, report = self.proof(
            execution_id="missing-terminal",
            state={
                "proof_runs": {
                    "all": {
                        "returncode": 0,
                        "stdout": test_events(
                            "suite::mod::beta",
                            None,
                            event_binary_alias="codex-core::all",
                        ),
                    }
                }
            },
        )
        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["classification"], "pre_result_error")
        self.assertNotIn("suite::mod::beta", report["executed_ids"])
        self.assertNotIn(
            {"id": "suite::mod::beta", "outcome": "unknown"}, report["outcomes"]
        )
        self.assertIn("terminal mismatch", report["diagnostic"])

    def test_unselected_ignored_harness_events_do_not_count_as_execution_through_cli(
        self,
    ) -> None:
        ignored = "ignored::fixture"
        qualified_ignored = f"codex-core::codex_core${ignored}"
        payload = "\n".join(
            [
                test_events("mod::tests::alpha", "ok"),
                json.dumps(
                    {"type": "test", "event": "started", "name": qualified_ignored}
                ),
                json.dumps(
                    {"type": "test", "event": "ignored", "name": qualified_ignored}
                ),
            ]
        )
        result, report = self.proof(
            execution_id="ignored-harness",
            state={"proof_runs": {"--lib": {"returncode": 0, "stdout": payload}}},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["classification"], "confirmed_pass")
        self.assertNotIn(ignored, report["executed_ids"])
        self.assertNotIn(ignored, [item["id"] for item in report["outcomes"]])

    def test_unselected_unterminated_start_is_ignored_through_cli(self) -> None:
        payload = "\n".join(
            [
                test_events("mod::tests::alpha", "ok"),
                test_events("unselected::fixture", None),
            ]
        )
        result, report = self.proof(
            execution_id="unselected-unterminated-start",
            state={"proof_runs": {"--lib": {"returncode": 0, "stdout": payload}}},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["classification"], "confirmed_pass")
        self.assertEqual(
            report["executed_ids"], ["mod::tests::alpha", "suite::mod::beta"]
        )
        self.assertNotIn(
            "unselected::fixture", [item["id"] for item in report["outcomes"]]
        )

    def test_cross_binary_lookalike_cannot_prove_execution_through_cli(self) -> None:
        result, report = self.proof(
            execution_id="cross-binary-spoof",
            state={
                "proof_runs": {
                    "--lib": {
                        "returncode": 0,
                        "stdout": test_events(
                            "mod::tests::alpha",
                            "ok",
                            event_binary_alias="codex-core::different-binary",
                        ),
                    }
                }
            },
        )
        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["classification"], "pre_result_error")
        self.assertNotIn("mod::tests::alpha", report["executed_ids"])
        self.assertIn("cross_binary_lookalikes", report["diagnostic"])

    def test_parallel_event_order_is_reported_in_manifest_order_through_cli(
        self,
    ) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["gates"]["demo-gate"]["steps"] = [
            {
                "target": "core_lib",
                "tests": ["mod::tests::alpha", "mod::tests::omega"],
            }
        ]
        payload = "\n".join(
            [
                test_events("mod::tests::omega", "ok"),
                test_events("mod::tests::alpha", "ok"),
            ]
        )
        result, report = self.proof(
            execution_id="parallel-event-order",
            manifest_data=data,
            state={
                "listings": {
                    "--lib": {
                        "mod::tests::alpha": False,
                        "mod::tests::omega": False,
                    }
                },
                "proof_runs": {
                    "--lib": {"returncode": 0, "stdout": payload}
                },
            },
        )
        expected = ["mod::tests::alpha", "mod::tests::omega"]
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["executed_ids"], expected)
        self.assertEqual(
            report["outcomes"],
            [{"id": test_id, "outcome": "passed"} for test_id in expected],
        )

    def test_exit_101_after_confirmed_failure_is_confirmed_through_cli(self) -> None:
        result, report = self.proof(
            execution_id="confirmed-failure-exit-101",
            state={
                "proof_runs": {
                    "--lib": {
                        "returncode": 101,
                        "stdout": test_events("mod::tests::alpha", "failed"),
                        "stderr": "runner crashed",
                    }
                }
            },
        )
        self.assertEqual(result.returncode, 100)
        self.assertEqual(report["classification"], "confirmed_validation_failure")
        self.assertIn(
            {"id": "mod::tests::alpha", "outcome": "failed"}, report["outcomes"]
        )

    def test_exit_zero_with_failed_terminal_is_pre_result_through_cli(self) -> None:
        result, report = self.proof(
            execution_id="failed-terminal-exit-zero",
            state={
                "proof_runs": {
                    "--lib": {
                        "returncode": 0,
                        "stdout": test_events("mod::tests::alpha", "failed"),
                    }
                }
            },
        )
        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["classification"], "pre_result_error")
        self.assertIn("failed terminal with exit 0", report["diagnostic"])

    def test_later_terminal_cannot_launder_first_failure_through_cli(self) -> None:
        qualified_id = "codex-core::codex_core$mod::tests::alpha"
        payload = "\n".join(
            [
                test_events("mod::tests::alpha", "failed"),
                json.dumps(
                    {"type": "test", "event": "ok", "name": qualified_id}
                ),
            ]
        )
        result, report = self.proof(
            execution_id="immutable-first-terminal",
            state={
                "proof_runs": {
                    "--lib": {
                        "returncode": 101,
                        "stdout": payload,
                        "stderr": "runner failed after reporting the test",
                    }
                }
            },
        )
        self.assertEqual(result.returncode, 100)
        self.assertEqual(report["classification"], "confirmed_validation_failure")
        self.assertIn(
            {"id": "mod::tests::alpha", "outcome": "failed"}, report["outcomes"]
        )
        self.assertNotIn(
            {"id": "mod::tests::alpha", "outcome": "passed"}, report["outcomes"]
        )
        self.assertIn(f"duplicate terminal result for {qualified_id}", report["diagnostic"])

    def test_confirmed_failure_survives_a_later_infrastructure_error_through_cli(
        self,
    ) -> None:
        result, report = self.proof(
            execution_id="failure-then-error",
            state={
                "proof_runs": {
                    "--lib": {
                        "returncode": 100,
                        "stdout": test_events("mod::tests::alpha", "failed"),
                        "stderr": "test failed",
                    },
                    "all": {
                        "returncode": 101,
                        "stdout": "",
                        "stderr": "runner crashed",
                    },
                }
            },
        )
        self.assertEqual(result.returncode, 100)
        self.assertEqual(report["classification"], "confirmed_validation_failure")
        self.assertIn(
            {"id": "mod::tests::alpha", "outcome": "failed"}, report["outcomes"]
        )
        self.assertNotIn("suite::mod::beta", report["executed_ids"])


class ParityTest(CliRunnerTestCase):
    def parity_state(
        self,
        listings: dict[str, dict[str, bool]],
        *,
        failing_runs: list[str] | None = None,
    ) -> dict[str, Any]:
        return {"listings": listings, "failing_runs": failing_runs or []}

    def test_identical_inventories_pass_and_run_both_sides_through_cli(self) -> None:
        listings = {
            "all": {"suite::a::one": False, "suite::b::two": True},
            "core_shard": {"suite::a::one": False, "suite::b::two": True},
        }
        result = self.invoke(
            ["parity", "core_all", "core_shard"], state=self.parity_state(listings)
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        for call in self.calls(["nextest", "list"]):
            self.assertIn("--ignore-default-filter", call["args"])
            self.assertEqual(
                call["args"][call["args"].index("--run-ignored") + 1], "all"
            )
        run_calls = self.calls(["nextest", "run"])
        self.assertEqual(len(run_calls), 2)
        for call in run_calls:
            self.assertIn("--no-fail-fast", call["args"])
            self.assertEqual(call["args"][call["args"].index("--retries") + 1], "0")
            self.assertEqual(
                call["args"][call["args"].index("--run-ignored") + 1], "default"
            )
            self.assertEqual(call["env"]["INSTA_UPDATE"], "always")

    def test_snapshot_content_change_fails_after_behavior_runs_through_cli(
        self,
    ) -> None:
        runner, codex_rs = self.isolated_runner()
        snapshots = codex_rs / "core" / "tests" / "suite" / "snapshots"
        snapshots.mkdir(parents=True)
        (snapshots / "all__suite__a__one.snap").write_text("legacy", encoding="utf-8")
        (snapshots / "core_shard__suite__a__one.snap").write_text(
            "replacement", encoding="utf-8"
        )
        listings = {
            "all": {"suite::a::one": False},
            "core_shard": {"suite::a::one": False},
        }
        result = self.invoke(
            ["parity", "core_all", "core_shard"],
            state=self.parity_state(listings),
            runner=runner,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("content_changes", result.stderr)
        self.assertEqual(len(self.calls(["nextest", "run"])), 2)

    def test_behavior_failures_are_reported_after_every_target_runs_through_cli(
        self,
    ) -> None:
        listings = {
            "all": {"suite::a::one": False, "suite::b::two": False},
            "core_shard": {"suite::a::one": False},
            "core_shard_two": {"suite::b::two": False},
        }
        result = self.invoke(
            ["parity", "core_all", "core_shard", "core_shard_two"],
            state=self.parity_state(listings, failing_runs=["all", "core_shard_two"]),
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("every target", result.stderr)
        self.assertIn("core_all:", result.stderr)
        self.assertIn("core_shard_two:", result.stderr)
        self.assertEqual(len(self.calls(["nextest", "run"])), 3)

    def test_missing_or_duplicate_snapshot_counterpart_fails_through_cli(self) -> None:
        runner, codex_rs = self.isolated_runner()
        snapshots = codex_rs / "core" / "tests" / "suite" / "snapshots"
        snapshots.mkdir(parents=True)
        for name in (
            "all__suite__a__one.snap",
            "all__suite__b__two.snap",
            "core_shard__suite__a__one.snap",
            "core_shard_two__suite__a__one.snap",
        ):
            (snapshots / name).write_text("same", encoding="utf-8")
        listings = {
            "all": {"suite::a::one": False, "suite::b::two": False},
            "core_shard": {"suite::a::one": False},
            "core_shard_two": {"suite::b::two": False},
        }
        result = self.invoke(
            ["parity", "core_all", "core_shard", "core_shard_two"],
            state=self.parity_state(listings),
            runner=runner,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("missing=['suite__b__two.snap']", result.stderr)
        self.assertIn("duplicates=['suite__a__one.snap']", result.stderr)

    def test_missing_test_fails_parity_through_cli(self) -> None:
        listings = {
            "all": {"suite::a::one": False, "suite::b::two": False},
            "core_shard": {"suite::a::one": False},
        }
        result = self.invoke(
            ["parity", "core_all", "core_shard"], state=self.parity_state(listings)
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("missing=['suite::b::two']", result.stderr)
        self.assertEqual(self.calls(["nextest", "run"]), [])

    def test_ignored_state_change_fails_parity_through_cli(self) -> None:
        listings = {
            "all": {"suite::a::one": True},
            "core_shard": {"suite::a::one": False},
        }
        result = self.invoke(
            ["parity", "core_all", "core_shard"], state=self.parity_state(listings)
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("ignored_state_changes", result.stderr)

    def test_legacy_target_cannot_also_be_a_replacement_through_cli(self) -> None:
        self.assert_error(
            ["parity", "core_all", "core_all"], "cannot also be a replacement"
        )


class RepositoryManifestTest(CliRunnerTestCase):
    """The checked-in manifest must satisfy the installed Cargo workspace."""

    def repository_cli(self, args: list[str]) -> subprocess.CompletedProcess[str]:
        return self.invoke(
            args,
            manifest_path=REPOSITORY_MANIFEST,
            use_fake_cargo=False,
        )

    def checked_manifest(self) -> dict[str, Any]:
        result = self.repository_cli(["check-manifest"])
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        with REPOSITORY_MANIFEST.open("rb") as source:
            return tomllib.load(source)

    def test_manifest_parses_strictly_through_cli(self) -> None:
        manifest = self.checked_manifest()
        result = self.repository_cli(["list-targets"])
        self.assertEqual(result.returncode, 0, result.stderr)
        listed = {
            (kind, name)
            for line in result.stdout.splitlines()
            for kind, name, features in [line.split("\t")]
            if features.startswith("features=")
        }
        self.assertEqual(manifest["version"], 1)
        self.assertIn(("target", "core_lib"), listed)
        self.assertNotIn(("target", "core_all"), listed)
        for name in (
            "core_cli_workspace",
            "core_code_mode_mcp",
            "core_exec_permissions",
            "core_thread_state",
            "core_transport_telemetry",
            "core_agents_review",
            "core_model_prompt_runtime",
            "core_windows",
        ):
            self.assertIn(("target", name), listed)

    def test_repository_manifest_has_nonempty_targets_and_gates_through_cli(
        self,
    ) -> None:
        manifest = self.checked_manifest()
        result = self.repository_cli(["list-targets"])
        self.assertEqual(result.returncode, 0, result.stderr)
        listed_targets = [
            line for line in result.stdout.splitlines() if line.startswith("target\t")
        ]
        listed_gates = [
            line for line in result.stdout.splitlines() if line.startswith("gate\t")
        ]
        self.assertGreater(len(listed_targets), 0)
        self.assertGreater(len(listed_gates), 0)
        self.assertEqual(len(listed_targets), len(manifest["targets"]))
        self.assertEqual(len(listed_gates), len(manifest["gates"]))

    def test_app_server_schema_gate_binds_exact_fixture_ids_through_cli(self) -> None:
        manifest = self.checked_manifest()
        gate = manifest["gates"]["app-server-schema-protocol"]
        self.assertEqual(
            gate["steps"],
            [
                {
                    "target": "app_server_protocol_schema",
                    "filter": "test(typescript_schema_fixtures_match_generated) | test(json_schema_fixtures_match_generated)",
                    "tests": [
                        "json_schema_fixtures_match_generated",
                        "typescript_schema_fixtures_match_generated",
                    ],
                }
            ],
        )

        planned = self.repository_cli(["plan", "app-server-schema-protocol"])
        self.assertEqual(planned.returncode, 0, planned.stdout + planned.stderr)
        plan = json.loads(planned.stdout)
        self.assertEqual(plan["kind"], "gate")
        self.assertEqual(plan["steps"][0]["tests"], gate["steps"][0]["tests"])
        self.assertIn("--no-tests=fail", plan["steps"][0]["run"])

        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        recipe = justfile.split("app-server-schema-protocol-check:", 1)[1].split(
            "\n\n", 1
        )[0]
        self.assertIn("just core-gate app-server-schema-protocol", recipe)
        self.assertNotIn("cargo nextest", recipe)

    def test_app_server_schema_real_selection_rejects_zero_matches_through_cli(
        self,
    ) -> None:
        result = self.repository_cli(
            [
                "run-target",
                "app_server_protocol_schema",
                "-E",
                "test(kd4_deliberate_zero_selection_probe)",
            ]
        )
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn(
            "named target 'app_server_protocol_schema' selected zero tests",
            result.stderr,
        )

    def test_every_declared_test_target_has_a_source_file_through_cli(self) -> None:
        manifest = self.checked_manifest()
        tests_dir = REPO_ROOT / "codex-rs" / "core" / "tests"
        for name, target in manifest["targets"].items():
            if target["package"] != "codex-core" or "test" not in target:
                continue
            result = self.repository_cli(["plan", name])
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue((tests_dir / f"{target['test']}.rs").is_file())

    def test_every_gate_step_names_a_declared_target_through_cli(self) -> None:
        manifest = self.checked_manifest()
        listed = self.repository_cli(["list-targets"])
        self.assertEqual(listed.returncode, 0, listed.stderr)
        declared = {
            name
            for line in listed.stdout.splitlines()
            for kind, name, features in [line.split("\t")]
            if kind == "target" and features.startswith("features=")
        }
        for gate in manifest["gates"].values():
            for step in gate["steps"]:
                self.assertIn(step["target"], declared)

    def test_shard_modules_and_helpers_match_migration_contract_through_cli(
        self,
    ) -> None:
        self.checked_manifest()
        expected_modules = {
            "core_cli_workspace": [
                "agents_md",
                "cli_stream",
                "completion_proof_gate",
                "deprecation_notice",
                "live_cli",
                "remote_env",
                "user_shell_cmd",
            ],
            "core_code_mode_mcp": [
                "code_mode",
                "code_mode_elicitation",
                "mcp_auth_elicitation",
                "mcp_auth_refresh",
                "mcp_refresh_cleanup",
                "mcp_tool_exposure",
                "rmcp_client",
            ],
            "core_exec_permissions": [
                "apply_patch_cli",
                "approvals",
                "exec_policy",
                "extension_sandbox",
                "permissions_messages",
                "request_permissions",
                "safety_check_downgrade",
                "shell_command",
                "shell_snapshot",
                "unified_exec",
                "unified_exec_process_events",
            ],
            "core_thread_state": [
                "compact",
                "compact_remote",
                "compact_resume_fork",
                "fork_thread",
                "pending_input",
                "resume",
                "resume_warning",
                "rollout_list_find",
                "sqlite_state",
                "stream_error_allows_next_turn",
                "stream_no_completed",
                "turn_state",
                "window_headers",
            ],
            "core_transport_telemetry": [
                "client",
                "client_websockets",
                "external_auth",
                "otel",
                "responses_api_proxy_headers",
                "responses_lite",
                "websocket_fallback",
            ],
            "core_agents_review": [
                "agent_execution",
                "agent_jobs",
                "agent_websocket",
                "auto_review",
                "codex_delegate",
                "collaboration_instructions",
                "investigation_evidence_schema",
                "multi_agent_mode",
                "request_user_input",
                "review",
                "subagent_notifications",
            ],
            "core_model_prompt_runtime": [
                "additional_context",
                "current_time_reminder",
                "image_rollout",
                "model_overrides",
                "model_runtime_selectors",
                "model_switching",
                "model_visible_layout",
                "models_cache_ttl",
                "override_updates",
                "personality",
                "prompt_caching",
                "prompt_debug_tests",
                "quota_exceeded",
                "safety_buffering",
                "web_search",
            ],
            "core_windows": ["hooks_windows", "windows_sandbox"],
        }
        expected_helpers = {
            name: ["codex", "codex-code-mode-host"] for name in expected_modules
        }
        expected_helpers["core_cli_workspace"] += [
            "codex-windows-sandbox-setup",
            "codex-command-runner",
        ]
        expected_helpers["core_code_mode_mcp"] += [
            "test_stdio_server",
            "test_streamable_http_server",
        ]
        expected_helpers["core_thread_state"].append("test_stdio_server")
        expected_helpers["core_exec_permissions"] += [
            "codex-windows-sandbox-setup",
            "codex-command-runner",
        ]
        expected_helpers["core_windows"] += [
            "codex-windows-sandbox-setup",
            "codex-command-runner",
        ]
        tests_dir = REPO_ROOT / "codex-rs" / "core" / "tests"
        for target_name, modules in expected_modules.items():
            result = self.repository_cli(["plan", target_name])
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                json.loads(result.stdout)["helpers"], expected_helpers[target_name]
            )
            source = (tests_dir / f"{target_name}.rs").read_text(encoding="utf-8")
            declared = [
                line.strip().removeprefix("mod ").removesuffix(";")
                for line in source.splitlines()
                if line.startswith("    mod ")
            ]
            self.assertEqual(declared, modules)
            self.assertIn('include!("suite/prelude.rs");', source)


class RunEnvironmentTest(CliRunnerTestCase):
    def test_profile_is_exported_to_every_cargo_command_through_cli(self) -> None:
        result = self.invoke(["run-target", "--profile", "fast", "core_shard"])
        self.assertEqual(result.returncode, 0, result.stderr)
        for call in self.last_calls:
            if call["args"][:1] != ["metadata"]:
                self.assertEqual(call["env"]["NEXTEST_PROFILE"], "fast")

    def test_stack_size_matches_the_windows_test_binary_contract_through_cli(
        self,
    ) -> None:
        result = self.invoke(["run-target", "core_shard"])
        self.assertEqual(result.returncode, 0, result.stderr)
        for call in self.last_calls:
            if call["args"][:1] != ["metadata"]:
                self.assertEqual(call["env"]["RUST_MIN_STACK"], RUST_MIN_STACK_BYTES)

    def test_inherited_profile_is_preserved_when_none_is_requested_through_cli(
        self,
    ) -> None:
        result = self.invoke(
            ["run-target", "core_shard"],
            env_overrides={"NEXTEST_PROFILE": "local", "RUST_MIN_STACK": "42"},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        run_env = self.calls(["nextest", "run"])[0]["env"]
        self.assertEqual(run_env["NEXTEST_PROFILE"], "local")
        self.assertEqual(run_env["RUST_MIN_STACK"], "42")


class CommandLineTest(CliRunnerTestCase):
    def test_guard_accepts_both_recipe_spellings_through_cli(self) -> None:
        for command in ("_guard-generic", "guard-args"):
            allowed = self.invoke([command, "--", "-p", "codex-tui"])
            self.assertEqual(allowed.returncode, 0, allowed.stderr)
            denied = self.invoke([command, "--", "-p", "codex-core"])
            self.assertEqual(denied.returncode, 2)
            self.assertIn("just core-test", denied.stderr)

    def test_guard_names_the_calling_recipe_through_cli(self) -> None:
        result = self.invoke(
            ["guard-args", "--recipe", "just test-fast", "--", "-p", "codex-core"]
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("just test-fast cannot select codex-core", result.stderr)

    def test_guard_does_not_read_the_manifest_through_cli(self) -> None:
        missing = self.temp_dir / "does-not-exist.toml"
        result = self.invoke(
            ["guard-args", "--", "-p", "codex-tui"],
            manifest_path=missing,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(missing.exists())
        self.assertEqual(self.last_calls, [])


class JustfileContractTest(CliRunnerTestCase):
    """Run each justfile runner command shape through the actual CLI parser."""

    PLACEHOLDERS: ClassVar[dict[str, str]] = {
        "{{ target }}": "core_shard",
        "{{ gate }}": "demo-gate",
        "{{ name }}": "core_shard",
        "{{ legacy }}": "core_all",
        "{{ package }}": "codex-tui",
        "$target_dir": "target",
        "@forwarded_args": "core_shard",
    }

    @staticmethod
    def tokenize(argv: str) -> list[str]:
        tokens: list[str] = []
        current: list[str] = []
        quote: str | None = None
        for char in argv:
            if quote is not None:
                if char == quote:
                    quote = None
                else:
                    current.append(char)
            elif char in "\"'":
                quote = char
            elif char.isspace():
                if current:
                    tokens.append("".join(current))
                    current = []
            else:
                current.append(char)
        if current:
            tokens.append("".join(current))
        return tokens

    def invocations(self) -> list[list[str]]:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        found: list[list[str]] = []
        for line in justfile.splitlines():
            _, separator, rest = line.partition("rust_test_runner.py")
            if not separator:
                continue
            argv = rest.lstrip('"').split(";", 1)[0]
            tokens = [
                self.PLACEHOLDERS.get(token, token) for token in self.tokenize(argv)
            ]
            found.append([token for token in tokens if token])
        return found

    def test_every_justfile_invocation_parses_through_cli(self) -> None:
        invocations = self.invocations()
        self.assertGreaterEqual(len(invocations), 10)
        for argv in invocations:
            state: dict[str, Any] | None = None
            if "parity" in argv:
                state = {
                    "listings": {
                        "all": {"same::test": False},
                        "core_shard": {"same::test": False},
                    }
                }
            result = self.invoke(argv, state=state)
            self.assertNotIn("usage: rust_test_runner.py", result.stderr)
            self.assertNotIn("unrecognized arguments", result.stderr)

    def test_named_selections_in_the_justfile_exist_in_the_manifest_through_cli(
        self,
    ) -> None:
        result = self.invoke(["list-targets"])
        self.assertEqual(result.returncode, 0, result.stderr)
        listed = {
            (kind, name)
            for line in result.stdout.splitlines()
            for kind, name, features in [line.split("\t")]
            if features.startswith("features=")
        }
        for argv in self.invocations():
            if "run-target" in argv or "plan" in argv:
                name = (
                    argv[argv.index("run-target") + 1]
                    if "run-target" in argv
                    else argv[argv.index("plan") + 1]
                )
                self.assertIn(("target", name), listed)
            elif "run-gate" in argv:
                self.assertIn(("gate", argv[argv.index("run-gate") + 1]), listed)
            elif "parity" in argv:
                index = argv.index("parity")
                self.assertIn(("target", argv[index + 1]), listed)
                for name in argv[index + 2 :]:
                    self.assertIn(("target", name), listed)


if __name__ == "__main__":
    unittest.main()
