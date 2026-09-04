from __future__ import annotations

import ctypes
import hashlib
import importlib.util
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
import tomllib
import unittest
import uuid
from pathlib import Path
from typing import Mapping
from unittest import mock

REPO_ROOT = Path(__file__).resolve().parents[1]
RUNNER = REPO_ROOT / "scripts" / "completion_proof.py"
MODULE_NAME = "_kd4_completion_proof_under_test"
MODULE_SPEC = importlib.util.spec_from_file_location(MODULE_NAME, RUNNER)
if MODULE_SPEC is None or MODULE_SPEC.loader is None:
    raise RuntimeError(f"cannot import completion-proof runner from {RUNNER}")
COMPLETION_PROOF = importlib.util.module_from_spec(MODULE_SPEC)
sys.modules[MODULE_NAME] = COMPLETION_PROOF
MODULE_SPEC.loader.exec_module(COMPLETION_PROOF)
CHILD_REPORT_PATH = REPO_ROOT / "scripts" / "child_validation_report.py"
CHILD_REPORT_MODULE_NAME = "_kd4_child_validation_report_under_test"
CHILD_REPORT_SPEC = importlib.util.spec_from_file_location(
    CHILD_REPORT_MODULE_NAME, CHILD_REPORT_PATH
)
if CHILD_REPORT_SPEC is None or CHILD_REPORT_SPEC.loader is None:
    raise RuntimeError(f"cannot import child-validation journal from {CHILD_REPORT_PATH}")
CHILD_REPORT = importlib.util.module_from_spec(CHILD_REPORT_SPEC)
sys.modules[CHILD_REPORT_MODULE_NAME] = CHILD_REPORT
CHILD_REPORT_SPEC.loader.exec_module(CHILD_REPORT)
RUST_GATE_VALIDATION_ID = "fixture.rust-gate"
RUST_GATE_NAME = "fixture-gate"
RUST_GATE_TEST_IDS = [
    "fixture_suite::first_runtime_path",
    "fixture_suite::second_runtime_path",
]
RUST_NEXTEST_NATIVE_ID = "fixture-package::fixture-test$fixture::ignored_runtime_path"
RUST_NEXTEST_BASELINE_ID = f"rust-nextest::{RUST_NEXTEST_NATIVE_ID}"


def _canonical_json(value: object) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")


def _inventory_hash(rows: list[dict[str, object]]) -> str:
    normalized = [
        {
            "baseline_id": str(row["baseline_id"]),
            "framework": str(row["framework"]),
            "native_id": str(row["native_id"]),
            "source": str(row["source"]),
            "ignored": bool(row.get("ignored", False)),
            "platforms": sorted(str(value) for value in row.get("platforms", [])),
        }
        for row in rows
    ]
    normalized.sort(key=lambda row: row["baseline_id"])
    return hashlib.sha256(
        _canonical_json({"schema_version": 1, "tests": normalized})
    ).hexdigest()


def _pid_is_running(pid: int) -> bool:
    if os.name == "nt":
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        handle = kernel32.OpenProcess(0x00100000, False, pid)
        if not handle:
            return False
        try:
            return kernel32.WaitForSingleObject(handle, 0) == 0x00000102
        finally:
            kernel32.CloseHandle(handle)
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def _wait_for_pid_exit(test: unittest.TestCase, pid: int) -> None:
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if not _pid_is_running(pid):
            return
        time.sleep(0.02)
    test.fail(f"validation descendant {pid} survived supervised cleanup")


class CompletionProofCliTest(unittest.TestCase):
    @classmethod
    def _native_fake_git_launcher(cls) -> Path:
        existing = getattr(cls, "_compiled_fake_git_launcher", None)
        if existing is not None:
            return existing
        temporary = tempfile.TemporaryDirectory(prefix="completion-proof-fake-git-")
        cls.addClassCleanup(temporary.cleanup)
        root = Path(temporary.name)
        source = root / "fake_git.rs"
        source.write_text(
            textwrap.dedent(
                r"""
                use std::env;
                use std::fs;
                use std::path::Path;
                use std::process::{Command, Stdio};
                use std::thread;
                use std::time::Duration;

                fn main() {
                    if env::var_os("KD4_FAKE_GIT_DESCENDANT").is_some() {
                        thread::sleep(Duration::from_secs(30));
                        return;
                    }

                    let arguments = env::args().skip(1).collect::<Vec<_>>().join(" ");
                    let selected = env::var("KD4_FAKE_GIT_HANG_MATCH")
                        .map(|value| arguments.contains(&value))
                        .unwrap_or(false);
                    let triggered = env::var_os("KD4_FAKE_GIT_HANG_TRIGGER")
                        .map(|value| Path::new(&value).exists())
                        .unwrap_or(true);
                    if selected && triggered {
                        let child = Command::new(env::current_exe().unwrap())
                            .env("KD4_FAKE_GIT_DESCENDANT", "1")
                            .stdin(Stdio::null())
                            .spawn()
                            .unwrap();
                        fs::write(
                            env::var_os("KD4_FAKE_GIT_CHILD_PID").unwrap(),
                            child.id().to_string(),
                        )
                        .unwrap();
                        thread::sleep(Duration::from_secs(30));
                        return;
                    }

                    if arguments.contains("rev-parse --verify HEAD^{tree}") {
                        println!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
                    } else if arguments.contains("rev-parse --verify HEAD") {
                        println!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
                    } else if arguments.contains("ls-files --others --ignored")
                        && env::var_os("KD4_FAKE_GIT_REPORT_IGNORED").is_some()
                    {
                        print!("test_ignored_marker.py\0");
                    }
                }
                """
            ).strip()
            + "\n",
            encoding="utf-8",
        )
        launcher = root / ("fake-git.exe" if os.name == "nt" else "fake-git")
        compiled = subprocess.run(
            ["rustc", str(source), "-o", str(launcher)],
            cwd=REPO_ROOT,
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
        )
        if compiled.returncode != 0:
            raise RuntimeError(
                "could not compile native fake Git launcher:\n"
                f"{compiled.stdout}\n{compiled.stderr}"
            )
        cls._compiled_fake_git_launcher = launcher
        return launcher

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="completion-proof-test-")
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.repository = self.base / "repository"
        self.repository.mkdir()
        self.validation_dir = self.repository / ".codex" / "validation"
        self.validation_dir.mkdir(parents=True)
        self.config = self.validation_dir / "completion-proof.toml"
        self.current_inventory = self.validation_dir / "current.json"
        self.frozen_inventory = self.validation_dir / "frozen.json"
        self.ledger = self.validation_dir / "ledger.json"
        self.marker = self.base / "executions.txt"
        self.validator = self.repository / "validator.py"
        self.deletable_input = self.repository / "tracked-input.txt"
        self.deletable_input.write_text("tracked input\n", encoding="utf-8")
        self.validator.write_text(
            textwrap.dedent(
                """\
                import pathlib
                import os
                import sys
                import time
                import uuid

                marker = pathlib.Path(sys.argv[1])
                leaked = sorted(
                    name
                    for name in os.environ
                    if name.casefold().startswith("codex_completion_proof_")
                )
                with marker.open("a", encoding="utf-8") as output:
                    output.write(str(uuid.uuid4()) + " " + repr(leaked) + "\\n")
                if leaked:
                    raise SystemExit(87)
                if sys.argv[2] == "wait":
                    (marker.parent / "validator-wait.pid").write_text(
                        str(os.getpid()), encoding="utf-8"
                    )
                    (marker.parent / "validator-ready").write_text(
                        "ready\\n", encoding="utf-8"
                    )
                    while not (marker.parent / "validator-release").exists():
                        time.sleep(0.02)
                    os.chdir(marker.parent.parent)
                    (marker.parent / "validator-finished").write_text(
                        "finished\\n", encoding="utf-8"
                    )
                raise SystemExit(1 if sys.argv[2] == "fail" else 0)
                """
            ),
            encoding="utf-8",
        )
        self.baseline_rows = [
            {
                "baseline_id": "fixture-command::old-behavior",
                "framework": "fixture-command",
                "native_id": "old-behavior",
                "source": "validator.py",
                "ignored": False,
                "platforms": [platform.system().casefold()],
            }
        ]
        self.current_rows = [
            {
                "baseline_id": "fixture-command::new-runtime-path",
                "framework": "fixture-command",
                "native_id": "new-runtime-path",
                "source": "validator.py",
                "ignored": False,
                "platforms": [platform.system().casefold()],
            }
        ]
        digest = _inventory_hash(self.baseline_rows)
        self._write_json(
            self.frozen_inventory,
            {
                "schema_version": 1,
                "baseline_commit": "fixture",
                "baseline_workspace_fingerprint": "fixture",
                "host_platform": platform.system().casefold(),
                "inventory_hash": digest,
                "tests": self.baseline_rows,
            },
        )
        self._write_json(
            self.ledger,
            {
                "schema_version": 1,
                "frozen_inventory_hash": digest,
                "rows": [
                    {
                        "baseline_id": "fixture-command::old-behavior",
                        "resolution": "replacement",
                        "replacement_ids": ["fixture-command::new-runtime-path"],
                        "preserved_behavior": "the fixture validation executes",
                        "product_path": "validator.py CLI",
                        "validation_id": "fixture.command",
                    }
                ],
                "overrides": [],
            },
        )
        self._write_json(
            self.current_inventory,
            {"schema_version": 1, "tests": self.current_rows},
        )
        self._write_config(mode="pass")
        self._git("init", "--quiet")
        self._git("config", "user.email", "completion-proof@example.invalid")
        self._git("config", "user.name", "Completion Proof Test")
        self._git("add", ".")
        self._git("commit", "--quiet", "-m", "fixture")

    def _fake_git_environment(
        self,
        *,
        hang_match: str,
        child_pid_file: Path,
        hang_trigger: Path | None = None,
        report_ignored: bool = False,
    ) -> dict[str, str]:
        fake_bin = self.base / f"fake-git-bin-{uuid.uuid4()}"
        fake_bin.mkdir()
        fake_git = fake_bin / ("git.exe" if os.name == "nt" else "git")
        shutil.copy2(self._native_fake_git_launcher(), fake_git)
        fake_git.chmod(fake_git.stat().st_mode | 0o111)
        env = self._base_env()
        env["PATH"] = str(fake_bin) + os.pathsep + env.get("PATH", "")
        env["KD4_FAKE_GIT_HANG_MATCH"] = hang_match
        env["KD4_FAKE_GIT_CHILD_PID"] = str(child_pid_file)
        if hang_trigger is not None:
            env["KD4_FAKE_GIT_HANG_TRIGGER"] = str(hang_trigger)
        if report_ignored:
            env["KD4_FAKE_GIT_REPORT_IGNORED"] = "1"
        return env

    def _run_fake_git_cli(
        self,
        *arguments: str,
        env: Mapping[str, str],
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            self._unittest_runner_command(
                "--config",
                str(self.config),
                *arguments,
                patch_source="GIT_PROCESS_TIMEOUT_SECONDS = 0.35",
            ),
            cwd=self.repository,
            env=env,
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            timeout=15,
            check=False,
        )

    def _write_json(self, path: Path, value: object) -> None:
        path.write_text(
            json.dumps(value, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )

    def _install_current_trusted_command_surfaces(self) -> None:
        for relative in (
            "justfile",
            "package.json",
            "sdk/typescript/package.json",
        ):
            destination = self.repository / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes((REPO_ROOT / relative).read_bytes())

    def test_workspace_status_parser_preserves_rename_endpoints_and_fails_closed(
        self,
    ) -> None:
        rename = (
            b"2 R. N... 100644 100644 100644 "
            + b"a" * 40
            + b" "
            + b"b" * 40
            + b" R100 docs/runtime.rs\0src/runtime.rs\0"
        )
        self.assertEqual(
            COMPLETION_PROOF._workspace_paths(rename),
            ["docs/runtime.rs", "src/runtime.rs"],
        )

        malformed_statuses = [
            b"x unknown\0",
            b"? unterminated.txt",
            b"2 R. N... 100644 100644 100644 a b R100 docs/runtime.rs\0",
            b"? ../outside.txt\0",
            b"? C:outside.txt\0",
            b"? .git/private-state\0",
            b"? \xff\0",
        ]
        for status in malformed_statuses:
            with (
                self.subTest(status=status),
                self.assertRaises(COMPLETION_PROOF.ProofError),
            ):
                COMPLETION_PROOF._workspace_paths(status)

    def _write_config(
        self,
        *,
        mode: str,
        validation_failure_exit_codes: list[int] | None = None,
        use_testing_inventory: bool = True,
    ) -> None:
        if validation_failure_exit_codes is None:
            validation_failure_exit_codes = [1]
        failure_codes = ", ".join(str(value) for value in validation_failure_exit_codes)
        frozen_inventory_hash = _inventory_hash(self.baseline_rows)
        command = [
            sys.executable,
            str(self.validator),
            str(self.marker),
            mode,
        ]
        command_toml = ", ".join(json.dumps(value) for value in command)
        testing_inventory = (
            'testing_current_inventory = ".codex/validation/current.json"\n'
            if use_testing_inventory
            else ""
        )
        self.config.write_text(
            textwrap.dedent(
                f"""\
                schema_version = 2
                policy_id = "fixture"
                frozen_inventory_hash = {json.dumps(frozen_inventory_hash)}
                canonical_command = "just completion-proof"
                focused_command = "just completion-focused {{validation_id}}"
                documentation_command = "just source-map-check"
                repository_root = {json.dumps(self.repository.as_posix())}
                frozen_inventory = ".codex/validation/frozen.json"
                replacement_ledger = ".codex/validation/ledger.json"
                {testing_inventory.rstrip()}
                host_platform = {json.dumps(platform.system().casefold())}

                [[validation]]
                id = "inventory.frozen-reconciliation"
                runner = "inventory-reconciliation"
                owned_paths = [".codex/validation/**"]
                consumed_paths = ["validator.py"]
                timeout_seconds = 30

                [[validation]]
                id = "fixture.command"
                runner = "typed-validation"
                validation_type = "fixture-command"
                command = [{command_toml}]
                validation_failure_exit_codes = [{failure_codes}]
                owned_paths = ["validator.py"]
                consumed_paths = ["validator.py"]
                timeout_seconds = 30
                """
            ),
            encoding="utf-8",
        )

    def _foreign_inventory_platform(self) -> str:
        return f"not-{platform.system().casefold()}"

    def _append_baseline_exception_rule(
        self,
        *,
        baseline_id: str,
        kind: str,
    ) -> None:
        with self.config.open("a", encoding="utf-8") as stream:
            stream.write(
                textwrap.dedent(
                    f"""\

                    [[baseline_exception]]
                    id_prefix = {json.dumps(baseline_id)}
                    kind = {json.dumps(kind)}
                    source = "completion-proof CLI fixture"
                    text = "the fixture exception remains explicitly quarantined"
                    """
                )
            )

    def _write_baseline_exception_fixture(
        self,
        *,
        kind: str,
        frozen_ignored: object,
        current_ignored: object,
        frozen_platforms: object,
        current_platforms: object,
    ) -> None:
        baseline_id = "fixture-command::old-behavior"
        self.baseline_rows = [
            {
                "baseline_id": baseline_id,
                "framework": "fixture-command",
                "native_id": "old-behavior",
                "source": "validator.py",
                "ignored": frozen_ignored,
                "platforms": frozen_platforms,
            }
        ]
        self.current_rows = [
            {
                "baseline_id": baseline_id,
                "framework": "fixture-command",
                "native_id": "old-behavior",
                "source": "validator.py",
                "ignored": current_ignored,
                "platforms": current_platforms,
            }
        ]
        digest = _inventory_hash(self.baseline_rows)
        self._write_json(
            self.frozen_inventory,
            {
                "schema_version": 1,
                "baseline_commit": "fixture",
                "baseline_workspace_fingerprint": "fixture",
                "host_platform": platform.system().casefold(),
                "inventory_hash": digest,
                "tests": self.baseline_rows,
            },
        )
        self._write_json(
            self.current_inventory,
            {"schema_version": 1, "tests": self.current_rows},
        )
        self._write_json(
            self.ledger,
            {
                "schema_version": 1,
                "frozen_inventory_hash": digest,
                "rows": [
                    {
                        "baseline_id": baseline_id,
                        "resolution": "exception",
                        "provenance": {
                            "kind": kind,
                            "source": "completion-proof CLI fixture",
                            "text": (
                                "the fixture exception remains explicitly quarantined"
                            ),
                        },
                    }
                ],
                "additions": [],
                "overrides": [],
            },
        )
        self._write_config(mode="pass")
        self._append_baseline_exception_rule(
            baseline_id=baseline_id,
            kind=kind,
        )

    def _write_rust_nextest_workspace_fixture(
        self, native_ids: list[str] | None = None
    ) -> None:
        (self.repository / "codex-rs").mkdir(exist_ok=True)
        native_ids = native_ids or [RUST_NEXTEST_NATIVE_ID]
        self.baseline_rows = [
            {
                "baseline_id": f"rust-nextest::{native_id}",
                "framework": "rust-nextest",
                "native_id": native_id,
                "source": "codex-rs/fixture.rs",
                "ignored": True,
                "platforms": [platform.system().casefold()],
            }
            for native_id in native_ids
        ]
        self.current_rows = [dict(row) for row in self.baseline_rows]
        digest = _inventory_hash(self.baseline_rows)
        self._write_json(
            self.frozen_inventory,
            {
                "schema_version": 1,
                "baseline_commit": "fixture",
                "baseline_workspace_fingerprint": "fixture",
                "host_platform": platform.system().casefold(),
                "inventory_hash": digest,
                "tests": self.baseline_rows,
            },
        )
        self._write_json(
            self.current_inventory,
            {"schema_version": 1, "tests": self.current_rows},
        )
        self._write_json(
            self.ledger,
            {
                "schema_version": 1,
                "frozen_inventory_hash": digest,
                "rows": [
                    {
                        "baseline_id": str(row["baseline_id"]),
                        "resolution": "exception",
                        "provenance": {
                            "kind": "protected",
                            "source": "completion-proof CLI fixture",
                            "text": "the ignored Rust case remains required and executable",
                        },
                    }
                    for row in self.baseline_rows
                ],
                "additions": [],
                "overrides": [],
            },
        )
        self.config.write_text(
            textwrap.dedent(
                f"""\
                schema_version = 2
                policy_id = "fixture"
                frozen_inventory_hash = {json.dumps(digest)}
                canonical_command = "just completion-proof"
                focused_command = "just completion-focused {{validation_id}}"
                documentation_command = "just source-map-check"
                repository_root = {json.dumps(self.repository.as_posix())}
                frozen_inventory = ".codex/validation/frozen.json"
                replacement_ledger = ".codex/validation/ledger.json"
                testing_current_inventory = ".codex/validation/current.json"
                host_platform = {json.dumps(platform.system().casefold())}

                [[validation]]
                id = "inventory.frozen-reconciliation"
                runner = "inventory-reconciliation"
                owned_paths = [".codex/validation/**"]
                consumed_paths = [".codex/validation/**"]
                timeout_seconds = 30

                [[validation]]
                id = "rust.nextest.workspace"
                runner = "rust-nextest"
                owned_paths = ["codex-rs/**"]
                consumed_paths = ["codex-rs/**"]
                timeout_seconds = 30
                """
            ),
            encoding="utf-8",
        )

    def _run_rust_nextest_events(
        self,
        events: str,
        *,
        child_returncode: int = 0,
        later_supervision_error: str | None = None,
    ) -> tuple[subprocess.CompletedProcess[str], dict[str, object], dict[str, object]]:
        fixture_id = uuid.uuid4().hex
        event_path = self.base / f"nextest-events-{fixture_id}.jsonl"
        log_path = self.base / f"nextest-launch-{fixture_id}.json"
        child_path = self.base / "nextest-child.py"
        event_path.write_text(events + "\n", encoding="utf-8")
        child_path.write_text(
            textwrap.dedent(
                f"""\
                import json
                import os
                import pathlib
                import sys

                log_path = pathlib.Path(sys.argv[1])
                event_path = pathlib.Path(sys.argv[2])
                log_path.write_text(
                    json.dumps({{"pid": os.getpid(), "argv": sys.argv[3:]}}) + "\\n",
                    encoding="utf-8",
                )
                sys.stdout.write(event_path.read_text(encoding="utf-8"))
                raise SystemExit({child_returncode})
                """
            ),
            encoding="utf-8",
        )
        patch_source = textwrap.dedent(
            f"""\
            import sys as _fixture_sys

            _fixture_original_run_process = run_process

            def _fixture_run_process(
                *, validation_id, execution_id, command, cwd, env, timeout_seconds
            ):
                if list(command[:3]) == ["cargo", "nextest", "run"]:
                    command = [
                        _fixture_sys.executable,
                        {str(child_path)!r},
                        {str(log_path)!r},
                        {str(event_path)!r},
                        *command,
                    ]
                result = _fixture_original_run_process(
                    validation_id=validation_id,
                    execution_id=execution_id,
                    command=command,
                    cwd=cwd,
                    env=env,
                    timeout_seconds=timeout_seconds,
                )
                if validation_id == "rust.nextest.workspace" and {later_supervision_error!r}:
                    result.returncode = None
                    result.child.exit_code = None
                    result.invocation_error = {later_supervision_error!r}
                return result

            run_process = _fixture_run_process
            """
        )
        result, report_path = self._run(patch_source=patch_source)
        report = self._load_report(report_path)
        launch = json.loads(log_path.read_text(encoding="utf-8"))
        return result, report, launch

    def _write_rust_doctest_workspace_fixture(self, native_id: str) -> None:
        source = "codex-rs/fixture/src/lib.rs"
        source_path = self.repository / source
        source_path.parent.mkdir(parents=True, exist_ok=True)
        source_path.write_text("//! completion-proof doctest fixture\n", encoding="utf-8")
        self.baseline_rows = [
            {
                "baseline_id": f"rust-doctest::{native_id}",
                "framework": "rust-doctest",
                "native_id": native_id,
                "source": source,
                "ignored": False,
                "platforms": [platform.system().casefold()],
            }
        ]
        self.current_rows = [dict(self.baseline_rows[0])]
        digest = _inventory_hash(self.baseline_rows)
        self._write_json(
            self.frozen_inventory,
            {
                "schema_version": 1,
                "baseline_commit": "fixture",
                "baseline_workspace_fingerprint": "fixture",
                "host_platform": platform.system().casefold(),
                "inventory_hash": digest,
                "tests": self.baseline_rows,
            },
        )
        self._write_json(
            self.current_inventory,
            {"schema_version": 1, "tests": self.current_rows},
        )
        self._write_json(
            self.ledger,
            {
                "schema_version": 1,
                "frozen_inventory_hash": digest,
                "rows": [
                    {
                        "baseline_id": self.baseline_rows[0]["baseline_id"],
                        "resolution": "exception",
                        "provenance": {
                            "kind": "protected",
                            "source": "completion-proof CLI fixture",
                            "text": "the doctest fixture remains required and executable",
                        },
                    }
                ],
                "additions": [],
                "overrides": [],
            },
        )
        self.config.write_text(
            textwrap.dedent(
                f"""\
                schema_version = 2
                policy_id = "fixture"
                frozen_inventory_hash = {json.dumps(digest)}
                canonical_command = "just completion-proof"
                focused_command = "just completion-focused {{validation_id}}"
                documentation_command = "just source-map-check"
                repository_root = {json.dumps(self.repository.as_posix())}
                frozen_inventory = ".codex/validation/frozen.json"
                replacement_ledger = ".codex/validation/ledger.json"
                testing_current_inventory = ".codex/validation/current.json"
                host_platform = {json.dumps(platform.system().casefold())}

                [[validation]]
                id = "inventory.frozen-reconciliation"
                runner = "inventory-reconciliation"
                owned_paths = [".codex/validation/**"]
                consumed_paths = [".codex/validation/**"]
                timeout_seconds = 30

                [[validation]]
                id = "rust.doctest.workspace"
                runner = "rust-doctest"
                owned_paths = ["codex-rs/**"]
                consumed_paths = ["codex-rs/**"]
                timeout_seconds = 30
                """
            ),
            encoding="utf-8",
        )

    def _run_rust_doctest_output(
        self,
        output: str,
        *,
        child_returncode: int,
        later_supervision_error: str,
    ) -> tuple[subprocess.CompletedProcess[str], dict[str, object], dict[str, object]]:
        fixture_id = uuid.uuid4().hex
        output_path = self.base / f"doctest-output-{fixture_id}.txt"
        log_path = self.base / f"doctest-launch-{fixture_id}.json"
        child_path = self.base / "doctest-child.py"
        output_path.write_text(output + "\n", encoding="utf-8")
        child_path.write_text(
            textwrap.dedent(
                f"""\
                import json
                import os
                import pathlib
                import sys

                log_path = pathlib.Path(sys.argv[1])
                output_path = pathlib.Path(sys.argv[2])
                log_path.write_text(
                    json.dumps({{"pid": os.getpid(), "argv": sys.argv[3:]}}) + "\\n",
                    encoding="utf-8",
                )
                sys.stdout.write(output_path.read_text(encoding="utf-8"))
                raise SystemExit({child_returncode})
                """
            ),
            encoding="utf-8",
        )
        patch_source = textwrap.dedent(
            f"""\
            import sys as _fixture_sys

            _fixture_original_run_process = run_process

            def _fixture_run_process(
                *, validation_id, execution_id, command, cwd, env, timeout_seconds
            ):
                if validation_id == "rust.doctest.workspace":
                    command = [
                        _fixture_sys.executable,
                        {str(child_path)!r},
                        {str(log_path)!r},
                        {str(output_path)!r},
                        *command,
                    ]
                result = _fixture_original_run_process(
                    validation_id=validation_id,
                    execution_id=execution_id,
                    command=command,
                    cwd=cwd,
                    env=env,
                    timeout_seconds=timeout_seconds,
                )
                if validation_id == "rust.doctest.workspace":
                    result.returncode = None
                    result.child.exit_code = None
                    result.invocation_error = {later_supervision_error!r}
                return result

            run_process = _fixture_run_process
            """
        )
        result, report_path = self._run(patch_source=patch_source)
        report = self._load_report(report_path)
        launch = json.loads(log_path.read_text(encoding="utf-8"))
        return result, report, launch

    def test_canonical_ignored_rust_test_requires_started_then_passed_execution(
        self,
    ) -> None:
        self._write_rust_nextest_workspace_fixture()
        events = "\n".join(
            [
                json.dumps(
                    {
                        "type": "test",
                        "event": "started",
                        "name": RUST_NEXTEST_NATIVE_ID,
                    }
                ),
                json.dumps(
                    {
                        "type": "test",
                        "event": "ok",
                        "name": RUST_NEXTEST_NATIVE_ID,
                    }
                ),
            ]
        )

        result, report, launch = self._run_rust_nextest_events(events)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["attempt_classification"], "confirmed_pass")
        validation = next(
            item
            for item in report["validations"]
            if item["id"] == "rust.nextest.workspace"
        )
        self.assertEqual(validation["classification"], "confirmed_pass")
        self.assertEqual(validation["intended_ids"], [RUST_NEXTEST_NATIVE_ID])
        self.assertEqual(validation["selected_ids"], [RUST_NEXTEST_NATIVE_ID])
        self.assertEqual(validation["executed_ids"], [RUST_NEXTEST_NATIVE_ID])
        self.assertEqual(
            validation["outcomes"],
            [{"id": RUST_NEXTEST_NATIVE_ID, "outcome": "passed"}],
        )
        child = next(
            item
            for item in report["child_processes"]
            if item["validation_id"] == "rust.nextest.workspace"
        )
        self.assertGreater(child["pid"], 0)
        self.assertEqual(launch["pid"], child["pid"])
        self.assertEqual(
            launch["argv"],
            [
                "cargo",
                "nextest",
                "run",
                "--workspace",
                "--profile",
                "completion-proof",
                "--no-fail-fast",
                "--run-ignored",
                "all",
                "--ignore-default-filter",
                "--retries",
                "0",
                "--no-tests=fail",
                "--message-format",
                "libtest-json-plus",
                "--message-format-version",
                "0.1",
            ],
        )

    def test_canonical_nextest_preserves_failure_before_supervision_error(self) -> None:
        self._write_rust_nextest_workspace_fixture()
        events = "\n".join(
            json.dumps(event)
            for event in (
                {
                    "type": "test",
                    "event": "started",
                    "name": RUST_NEXTEST_NATIVE_ID,
                },
                {
                    "type": "test",
                    "event": "failed",
                    "name": RUST_NEXTEST_NATIVE_ID,
                },
            )
        )

        result, report, launch = self._run_rust_nextest_events(
            events,
            child_returncode=100,
            later_supervision_error="fixture supervision failed after validation",
        )

        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(report["attempt_classification"], "confirmed_validation_failure")
        validation = next(
            item
            for item in report["validations"]
            if item["id"] == "rust.nextest.workspace"
        )
        self.assertEqual(validation["classification"], "confirmed_validation_failure")
        self.assertEqual(validation["executed_ids"], [RUST_NEXTEST_NATIVE_ID])
        self.assertEqual(validation["confirmed_failure_ids"], [RUST_NEXTEST_NATIVE_ID])
        self.assertIn("fixture supervision failed", validation["diagnostic"])
        child = next(
            item
            for item in report["child_processes"]
            if item["validation_id"] == "rust.nextest.workspace"
        )
        self.assertEqual(child["pid"], launch["pid"])
        self.assertIsNone(child["exit_code"])

    def test_canonical_nextest_rejects_failed_terminal_before_later_start(
        self,
    ) -> None:
        self._write_rust_nextest_workspace_fixture()
        events = "\n".join(
            json.dumps(event)
            for event in (
                {
                    "type": "test",
                    "event": "failed",
                    "name": RUST_NEXTEST_NATIVE_ID,
                },
                {
                    "type": "test",
                    "event": "started",
                    "name": RUST_NEXTEST_NATIVE_ID,
                },
            )
        )

        result, report, launch = self._run_rust_nextest_events(
            events,
            child_returncode=100,
        )

        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        validation = next(
            item
            for item in report["validations"]
            if item["id"] == "rust.nextest.workspace"
        )
        self.assertEqual(validation["classification"], "pre_result_error")
        self.assertIn("terminal result before its start", validation["diagnostic"])
        child = next(
            item
            for item in report["child_processes"]
            if item["validation_id"] == "rust.nextest.workspace"
        )
        self.assertEqual(child["pid"], launch["pid"])

    def test_canonical_nextest_preserves_failure_before_later_parse_fault(
        self,
    ) -> None:
        self._write_rust_nextest_workspace_fixture()
        events = "\n".join(
            (
                json.dumps(
                    {
                        "type": "test",
                        "event": "started",
                        "name": RUST_NEXTEST_NATIVE_ID,
                    }
                ),
                json.dumps(
                    {
                        "type": "test",
                        "event": "failed",
                        "name": RUST_NEXTEST_NATIVE_ID,
                    }
                ),
                "{later-malformed-json",
            )
        )

        result, report, launch = self._run_rust_nextest_events(
            events,
            child_returncode=100,
        )

        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(
            report["attempt_classification"],
            "confirmed_validation_failure",
        )
        validation = next(
            item
            for item in report["validations"]
            if item["id"] == "rust.nextest.workspace"
        )
        self.assertEqual(
            validation["classification"],
            "confirmed_validation_failure",
        )
        self.assertEqual(
            validation["confirmed_failure_ids"],
            [RUST_NEXTEST_NATIVE_ID],
        )
        self.assertIn("invalid nextest event JSON", validation["diagnostic"])
        child = next(
            item
            for item in report["child_processes"]
            if item["validation_id"] == "rust.nextest.workspace"
        )
        self.assertEqual(child["pid"], launch["pid"])

    def test_canonical_nextest_accepts_fresh_reverse_order_results(self) -> None:
        intended = [
            "fixture-package::fixture-test$fixture::first_runtime_path",
            "fixture-package::fixture-test$fixture::second_runtime_path",
        ]
        self._write_rust_nextest_workspace_fixture(intended)
        events = "\n".join(
            json.dumps(event)
            for event in (
                {"type": "test", "event": "started", "name": intended[1]},
                {"type": "test", "event": "started", "name": intended[0]},
                {"type": "test", "event": "ok", "name": intended[1]},
                {"type": "test", "event": "ok", "name": intended[0]},
            )
        )

        result, report, launch = self._run_rust_nextest_events(events)

        self.assertEqual(result.returncode, 0, result.stderr)
        validation = next(
            item
            for item in report["validations"]
            if item["id"] == "rust.nextest.workspace"
        )
        self.assertEqual(validation["classification"], "confirmed_pass")
        self.assertEqual(validation["intended_ids"], intended)
        self.assertEqual(validation["selected_ids"], intended)
        self.assertEqual(validation["executed_ids"], intended)
        self.assertEqual(validation["intended_count"], 2)
        self.assertEqual(validation["selected_count"], 2)
        self.assertEqual(validation["executed_count"], 2)
        self.assertEqual(
            validation["outcomes"],
            [{"id": test_id, "outcome": "passed"} for test_id in intended],
        )
        child = next(
            item
            for item in report["child_processes"]
            if item["validation_id"] == "rust.nextest.workspace"
        )
        self.assertGreater(child["pid"], 0)
        self.assertEqual(child["pid"], launch["pid"])
        self.assertEqual(child["execution_id"], validation["execution_id"])
        self.assertTrue(uuid.UUID(validation["execution_id"]))
        self._assert_file_identity(child["launch_target_identity"])

    def test_canonical_nextest_rejects_missing_and_unexpected_results(self) -> None:
        intended = [
            "fixture-package::fixture-test$fixture::first_runtime_path",
            "fixture-package::fixture-test$fixture::second_runtime_path",
        ]
        self._write_rust_nextest_workspace_fixture(intended)
        unexpected = "fixture-package::fixture-test$fixture::unexpected_runtime_path"
        cases = {
            "missing": (
                [
                    {"type": "test", "event": "started", "name": intended[0]},
                    {"type": "test", "event": "ok", "name": intended[0]},
                ],
                "omitted intended IDs",
            ),
            "unexpected": (
                [
                    *(
                        {"type": "test", "event": "started", "name": test_id}
                        for test_id in [*intended, unexpected]
                    ),
                    *(
                        {"type": "test", "event": "ok", "name": test_id}
                        for test_id in [*intended, unexpected]
                    ),
                ],
                "emitted unexpected IDs",
            ),
        }
        for name, (raw_events, diagnostic) in cases.items():
            with self.subTest(name=name):
                result, report, launch = self._run_rust_nextest_events(
                    "\n".join(json.dumps(event) for event in raw_events)
                )

                self.assertEqual(result.returncode, 2, result.stderr)
                validation = next(
                    item
                    for item in report["validations"]
                    if item["id"] == "rust.nextest.workspace"
                )
                self.assertEqual(validation["classification"], "pre_result_error")
                self.assertIn(diagnostic, validation["diagnostic"])
                child = next(
                    item
                    for item in report["child_processes"]
                    if item["validation_id"] == "rust.nextest.workspace"
                )
                self.assertEqual(child["pid"], launch["pid"])

    def test_canonical_ignored_rust_test_rejects_skip_and_ambiguous_event_streams(
        self,
    ) -> None:
        self._write_rust_nextest_workspace_fixture()
        cases = {
            "skipped": (
                [
                    {
                        "type": "test",
                        "event": "started",
                        "name": RUST_NEXTEST_NATIVE_ID,
                    },
                    {
                        "type": "test",
                        "event": "ignored",
                        "name": RUST_NEXTEST_NATIVE_ID,
                    },
                ],
                "",
                "skipped",
            ),
            "terminal-before-start": (
                [
                    {"type": "test", "event": "ok", "name": RUST_NEXTEST_NATIVE_ID},
                    {
                        "type": "test",
                        "event": "started",
                        "name": RUST_NEXTEST_NATIVE_ID,
                    },
                ],
                "terminal result before its start",
                "passed",
            ),
            "second-terminal": (
                [
                    {
                        "type": "test",
                        "event": "started",
                        "name": RUST_NEXTEST_NATIVE_ID,
                    },
                    {
                        "type": "test",
                        "event": "ignored",
                        "name": RUST_NEXTEST_NATIVE_ID,
                    },
                    {"type": "test", "event": "ok", "name": RUST_NEXTEST_NATIVE_ID},
                ],
                "duplicate terminal result",
                "skipped",
            ),
            "malformed-json": (
                [
                    "{not-json",
                    {
                        "type": "test",
                        "event": "started",
                        "name": RUST_NEXTEST_NATIVE_ID,
                    },
                    {"type": "test", "event": "ok", "name": RUST_NEXTEST_NATIVE_ID},
                ],
                "invalid nextest event JSON",
                "passed",
            ),
            "non-object-json": (
                [
                    [],
                    {
                        "type": "test",
                        "event": "started",
                        "name": RUST_NEXTEST_NATIVE_ID,
                    },
                    {"type": "test", "event": "ok", "name": RUST_NEXTEST_NATIVE_ID},
                ],
                "nextest event record is not an object",
                "passed",
            ),
            "missing-name": (
                [
                    {"type": "test", "event": "started"},
                    {
                        "type": "test",
                        "event": "started",
                        "name": RUST_NEXTEST_NATIVE_ID,
                    },
                    {"type": "test", "event": "ok", "name": RUST_NEXTEST_NATIVE_ID},
                ],
                "nextest test event omitted its name",
                "passed",
            ),
            "duplicate-start": (
                [
                    {
                        "type": "test",
                        "event": "started",
                        "name": RUST_NEXTEST_NATIVE_ID,
                    },
                    {
                        "type": "test",
                        "event": "started",
                        "name": RUST_NEXTEST_NATIVE_ID,
                    },
                    {"type": "test", "event": "ok", "name": RUST_NEXTEST_NATIVE_ID},
                ],
                "duplicate start",
                "passed",
            ),
        }
        for name, (raw_events, diagnostic, first_outcome) in cases.items():
            with self.subTest(name=name):
                events = "\n".join(
                    event if isinstance(event, str) else json.dumps(event)
                    for event in raw_events
                )
                result, report, launch = self._run_rust_nextest_events(events)

                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertNotIn("COMPLETION PROOF PASSED", result.stdout)
                self.assertEqual(report["attempt_classification"], "pre_result_error")
                validation = next(
                    item
                    for item in report["validations"]
                    if item["id"] == "rust.nextest.workspace"
                )
                self.assertEqual(validation["classification"], "pre_result_error")
                self.assertEqual(validation["executed_ids"], [RUST_NEXTEST_NATIVE_ID])
                self.assertEqual(
                    validation["outcomes"],
                    [{"id": RUST_NEXTEST_NATIVE_ID, "outcome": first_outcome}],
                )
                if diagnostic:
                    self.assertIn(diagnostic, validation["diagnostic"])
                child = next(
                    item
                    for item in report["child_processes"]
                    if item["validation_id"] == "rust.nextest.workspace"
                )
                self.assertGreater(child["pid"], 0)
                self.assertEqual(launch["pid"], child["pid"])

    def test_canonical_doctest_preserves_failure_before_supervision_error(self) -> None:
        native_id = "fixture::runtime_path (line 1)"
        self._write_rust_doctest_workspace_fixture(native_id)

        result, report, launch = self._run_rust_doctest_output(
            f"test {native_id} ... FAILED",
            child_returncode=101,
            later_supervision_error="fixture supervision failed after validation",
        )

        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(report["attempt_classification"], "confirmed_validation_failure")
        validation = next(
            item
            for item in report["validations"]
            if item["id"] == "rust.doctest.workspace"
        )
        self.assertEqual(validation["classification"], "confirmed_validation_failure")
        self.assertEqual(validation["executed_ids"], [native_id])
        self.assertEqual(validation["confirmed_failure_ids"], [native_id])
        self.assertIn("fixture supervision failed", validation["diagnostic"])
        child = next(
            item
            for item in report["child_processes"]
            if item["validation_id"] == "rust.doctest.workspace"
        )
        self.assertEqual(child["pid"], launch["pid"])
        self.assertIsNone(child["exit_code"])

    def _write_jest_workspace_fixture(self, native_ids: list[str]) -> None:
        source = "sdk/typescript/tests/order.test.ts"
        test_file = self.repository / source
        test_file.parent.mkdir(parents=True, exist_ok=True)
        test_file.write_text("// completion-proof Jest fixture\n", encoding="utf-8")
        self.baseline_rows = [
            {
                "baseline_id": f"javascript-jest::{native_id}",
                "framework": "javascript-jest",
                "native_id": native_id,
                "source": source,
                "ignored": False,
                "platforms": [platform.system().casefold()],
            }
            for native_id in native_ids
        ]
        self.current_rows = [dict(row) for row in self.baseline_rows]
        digest = _inventory_hash(self.baseline_rows)
        self._write_json(
            self.frozen_inventory,
            {
                "schema_version": 1,
                "baseline_commit": "fixture",
                "baseline_workspace_fingerprint": "fixture",
                "host_platform": platform.system().casefold(),
                "inventory_hash": digest,
                "tests": self.baseline_rows,
            },
        )
        self._write_json(
            self.current_inventory,
            {"schema_version": 1, "tests": self.current_rows},
        )
        self._write_json(
            self.ledger,
            {
                "schema_version": 1,
                "frozen_inventory_hash": digest,
                "rows": [
                    {
                        "baseline_id": str(row["baseline_id"]),
                        "resolution": "exception",
                        "provenance": {
                            "kind": "protected",
                            "source": "completion-proof CLI fixture",
                            "text": "the Jest fixture remains required and executable",
                        },
                    }
                    for row in self.baseline_rows
                ],
                "additions": [],
                "overrides": [],
            },
        )
        self.config.write_text(
            textwrap.dedent(
                f"""\
                schema_version = 2
                policy_id = "fixture"
                frozen_inventory_hash = {json.dumps(digest)}
                canonical_command = "just completion-proof"
                focused_command = "just completion-focused {{validation_id}}"
                documentation_command = "just source-map-check"
                repository_root = {json.dumps(self.repository.as_posix())}
                frozen_inventory = ".codex/validation/frozen.json"
                replacement_ledger = ".codex/validation/ledger.json"
                testing_current_inventory = ".codex/validation/current.json"
                host_platform = {json.dumps(platform.system().casefold())}

                [[validation]]
                id = "inventory.frozen-reconciliation"
                runner = "inventory-reconciliation"
                owned_paths = [".codex/validation/**"]
                consumed_paths = [".codex/validation/**"]
                timeout_seconds = 30

                [[validation]]
                id = "sdk.typescript.jest"
                runner = "javascript-jest"
                owned_paths = ["sdk/typescript/**"]
                consumed_paths = ["sdk/typescript/**"]
                timeout_seconds = 30
                """
            ),
            encoding="utf-8",
        )

    def _run_jest_assertions(
        self,
        assertions: list[dict[str, str]],
        *,
        journal_assertions: list[dict[str, str]] | None = None,
        journal_mode: str = "valid",
        write_final_report: bool = True,
        corrupt_final_report: bool = False,
        child_returncode: int = 0,
        later_supervision_error: str | None = None,
    ) -> tuple[subprocess.CompletedProcess[str], dict[str, object], dict[str, object]]:
        fixture_id = uuid.uuid4().hex
        fixture_path = self.base / f"jest-assertions-{fixture_id}.json"
        log_path = self.base / f"jest-launch-{fixture_id}.json"
        child_path = self.base / "jest-child.py"
        self._write_json(
            fixture_path,
            {
                "assertions": assertions,
                "journal_assertions": (
                    assertions if journal_assertions is None else journal_assertions
                ),
                "journal_mode": journal_mode,
                "write_final_report": write_final_report,
                "corrupt_final_report": corrupt_final_report,
                "child_returncode": child_returncode,
            },
        )
        child_path.write_text(
            textwrap.dedent(
                """\
                import json
                import os
                import pathlib
                import sys

                log_path = pathlib.Path(sys.argv[1])
                fixture_path = pathlib.Path(sys.argv[2])
                original_argv = sys.argv[3:]
                fixture = json.loads(fixture_path.read_text(encoding="utf-8"))
                assertions = fixture["assertions"]
                journal_assertions = fixture["journal_assertions"]
                output_path = pathlib.Path(
                    original_argv[original_argv.index("--outputFile") + 1]
                )
                journal_path = pathlib.Path(
                    os.environ["KD4_COMPLETION_PROOF_JEST_JOURNAL"]
                )
                nonce = os.environ["KD4_COMPLETION_PROOF_JEST_SELECTION_NONCE"]
                assertion_events = [
                    {
                        "event": "assertion",
                        "file": assertion["file"],
                        "fullName": assertion["fullName"],
                        "status": assertion["status"],
                    }
                    for assertion in journal_assertions
                ]
                run_start = {"event": "run_started", "selectionNonce": nonce}
                mode = fixture["journal_mode"]
                if mode == "valid":
                    journal = [run_start, *assertion_events]
                elif mode == "duplicate-run-start":
                    journal = [run_start, run_start, *assertion_events]
                elif mode == "assertion-before-run-start":
                    journal = [assertion_events[0], run_start, *assertion_events[1:]]
                elif mode == "wrong-nonce":
                    journal = [
                        {"event": "run_started", "selectionNonce": "wrong"},
                        *assertion_events,
                    ]
                elif mode == "invalid-json":
                    journal = [run_start, *assertion_events]
                else:
                    raise SystemExit(f"unknown journal mode: {mode}")
                prefix = "{not-json\\n" if mode == "invalid-json" else ""
                journal_path.write_text(
                    prefix + "".join(json.dumps(event) + "\\n" for event in journal),
                    encoding="utf-8",
                )
                if fixture["write_final_report"]:
                    output_path.write_text(
                        "{not-json\\n"
                        if fixture["corrupt_final_report"]
                        else json.dumps(
                            {
                                "testResults": [
                                    {
                                        "name": assertion["file"],
                                        "assertionResults": [
                                            {
                                                "fullName": assertion["fullName"],
                                                "status": assertion["status"],
                                            }
                                        ],
                                    }
                                    for assertion in assertions
                                ]
                            }
                        ) + "\\n",
                        encoding="utf-8",
                    )
                log_path.write_text(
                    json.dumps({"pid": os.getpid(), "argv": original_argv}) + "\\n",
                    encoding="utf-8",
                )
                raise SystemExit(fixture["child_returncode"])
                """
            ),
            encoding="utf-8",
        )
        patch_source = textwrap.dedent(
            f"""\
            import sys as _fixture_sys

            _fixture_original_run_process = run_process

            def _fixture_run_process(
                *, validation_id, execution_id, command, cwd, env, timeout_seconds
            ):
                if command and command[0] == "node":
                    command = [
                        _fixture_sys.executable,
                        {str(child_path)!r},
                        {str(log_path)!r},
                        {str(fixture_path)!r},
                        *command,
                    ]
                result = _fixture_original_run_process(
                    validation_id=validation_id,
                    execution_id=execution_id,
                    command=command,
                    cwd=cwd,
                    env=env,
                    timeout_seconds=timeout_seconds,
                )
                if validation_id == "sdk.typescript.jest" and {later_supervision_error!r}:
                    result.returncode = None
                    result.child.exit_code = None
                    result.invocation_error = {later_supervision_error!r}
                return result

            run_process = _fixture_run_process
            """
        )
        result, report_path = self._run(patch_source=patch_source)
        report = self._load_report(report_path)
        launch = json.loads(log_path.read_text(encoding="utf-8"))
        return result, report, launch

    def test_canonical_jest_accepts_fresh_reverse_order_results(self) -> None:
        source = "sdk/typescript/tests/order.test.ts"
        intended = [f"{source}::ordering first", f"{source}::ordering second"]
        self._write_jest_workspace_fixture(intended)
        test_file = str((self.repository / source).resolve())
        assertions = [
            {"file": test_file, "fullName": "ordering second", "status": "passed"},
            {"file": test_file, "fullName": "ordering first", "status": "passed"},
        ]

        result, report, launch = self._run_jest_assertions(assertions)

        self.assertEqual(result.returncode, 0, result.stderr)
        validation = next(
            item
            for item in report["validations"]
            if item["id"] == "sdk.typescript.jest"
        )
        self.assertEqual(validation["classification"], "confirmed_pass")
        self.assertEqual(validation["intended_ids"], intended)
        self.assertEqual(validation["selected_ids"], intended)
        self.assertEqual(validation["executed_ids"], intended)
        self.assertEqual(validation["intended_count"], 2)
        self.assertEqual(validation["selected_count"], 2)
        self.assertEqual(validation["executed_count"], 2)
        self.assertEqual(
            validation["outcomes"],
            [{"id": test_id, "outcome": "passed"} for test_id in intended],
        )
        child = next(
            item
            for item in report["child_processes"]
            if item["validation_id"] == "sdk.typescript.jest"
        )
        self.assertGreater(child["pid"], 0)
        self.assertEqual(child["pid"], launch["pid"])
        self.assertEqual(child["execution_id"], validation["execution_id"])
        self.assertTrue(uuid.UUID(validation["execution_id"]))
        self._assert_file_identity(child["launch_target_identity"])

    def test_canonical_jest_rejects_inexact_or_nonresult_evidence(self) -> None:
        source = "sdk/typescript/tests/order.test.ts"
        intended = [f"{source}::ordering first", f"{source}::ordering second"]
        self._write_jest_workspace_fixture(intended)
        test_file = str((self.repository / source).resolve())
        first = {"file": test_file, "fullName": "ordering first", "status": "passed"}
        second = {"file": test_file, "fullName": "ordering second", "status": "passed"}
        cases = {
            "duplicate": ([first, first, second], "duplicate assertion result"),
            "missing": ([first], "omitted intended IDs"),
            "unexpected": (
                [
                    first,
                    second,
                    {
                        "file": test_file,
                        "fullName": "ordering unexpected",
                        "status": "passed",
                    },
                ],
                "emitted unexpected IDs",
            ),
            "skipped": (
                [first, {**second, "status": "pending"}],
                "non-result outcome 'skipped'",
            ),
            "unknown": (
                [first, {**second, "status": "mystery"}],
                "non-result outcome 'unknown'",
            ),
        }
        for name, (assertions, diagnostic) in cases.items():
            with self.subTest(name=name):
                result, report, launch = self._run_jest_assertions(assertions)

                self.assertEqual(result.returncode, 2, result.stderr)
                validation = next(
                    item
                    for item in report["validations"]
                    if item["id"] == "sdk.typescript.jest"
                )
                self.assertEqual(validation["classification"], "pre_result_error")
                self.assertIn(diagnostic, validation["diagnostic"])
                child = next(
                    item
                    for item in report["child_processes"]
                    if item["validation_id"] == "sdk.typescript.jest"
                )
                self.assertEqual(child["pid"], launch["pid"])

    def test_canonical_jest_rejects_invalid_event_attestation(self) -> None:
        source = "sdk/typescript/tests/order.test.ts"
        intended = [f"{source}::ordering first", f"{source}::ordering second"]
        self._write_jest_workspace_fixture(intended)
        test_file = str((self.repository / source).resolve())
        assertions = [
            {"file": test_file, "fullName": "ordering first", "status": "passed"},
            {"file": test_file, "fullName": "ordering second", "status": "passed"},
        ]
        cases = {
            "duplicate-run-start": (
                {"journal_mode": "duplicate-run-start"},
                "duplicate run-start events",
            ),
            "assertion-before-run-start": (
                {"journal_mode": "assertion-before-run-start"},
                "assertion before its run-start event",
            ),
            "wrong-nonce": (
                {"journal_mode": "wrong-nonce"},
                "run-start nonce did not match",
            ),
            "invalid-json": (
                {"journal_mode": "invalid-json"},
                "journal contained invalid JSON",
            ),
            "missing-final-report": (
                {"write_final_report": False},
                "omitted or corrupted its final JSON report",
            ),
        }
        for name, (options, diagnostic) in cases.items():
            with self.subTest(name=name):
                result, report, launch = self._run_jest_assertions(
                    assertions, **options
                )

                self.assertEqual(result.returncode, 2, result.stderr)
                validation = next(
                    item
                    for item in report["validations"]
                    if item["id"] == "sdk.typescript.jest"
                )
                self.assertEqual(validation["classification"], "pre_result_error")
                self.assertIn(diagnostic, validation["diagnostic"])
                child = next(
                    item
                    for item in report["child_processes"]
                    if item["validation_id"] == "sdk.typescript.jest"
                )
                self.assertEqual(child["pid"], launch["pid"])

    def test_canonical_jest_rejects_journal_final_assertion_disagreement(self) -> None:
        source = "sdk/typescript/tests/order.test.ts"
        intended = [f"{source}::ordering first", f"{source}::ordering second"]
        self._write_jest_workspace_fixture(intended)
        test_file = str((self.repository / source).resolve())
        first = {"file": test_file, "fullName": "ordering first", "status": "passed"}
        second = {
            "file": test_file,
            "fullName": "ordering second",
            "status": "passed",
        }
        final_assertions = [first, second]
        cases = {
            "missing-journal-assertion": (
                [first],
                "Jest journal omitted intended IDs",
            ),
            "journal-outcome-differs": (
                [first, {**second, "status": "failed"}],
                "Jest journal and final report outcomes disagreed",
            ),
        }
        for name, (journal_assertions, diagnostic) in cases.items():
            with self.subTest(name=name):
                result, report, launch = self._run_jest_assertions(
                    final_assertions,
                    journal_assertions=journal_assertions,
                )

                self.assertEqual(result.returncode, 2, result.stderr)
                validation = next(
                    item
                    for item in report["validations"]
                    if item["id"] == "sdk.typescript.jest"
                )
                self.assertEqual(validation["classification"], "pre_result_error")
                self.assertIn(diagnostic, validation["diagnostic"])
                child = next(
                    item
                    for item in report["child_processes"]
                    if item["validation_id"] == "sdk.typescript.jest"
                )
                self.assertEqual(child["pid"], launch["pid"])

    def test_canonical_jest_preserves_nonce_bound_failure_without_final_json(
        self,
    ) -> None:
        source = "sdk/typescript/tests/order.test.ts"
        intended = [f"{source}::ordering fails"]
        self._write_jest_workspace_fixture(intended)
        failed_assertion = {
            "file": str((self.repository / source).resolve()),
            "fullName": "ordering fails",
            "status": "failed",
        }
        cases = {
            "missing-final-after-supervision-error": {
                "write_final_report": False,
                "later_supervision_error": "fixture supervision failed after validation",
            },
            "corrupt-final": {"corrupt_final_report": True},
        }
        for name, options in cases.items():
            with self.subTest(name=name):
                result, report, launch = self._run_jest_assertions(
                    [failed_assertion],
                    child_returncode=1,
                    **options,
                )

                self.assertEqual(result.returncode, 1, result.stderr)
                validation = next(
                    item
                    for item in report["validations"]
                    if item["id"] == "sdk.typescript.jest"
                )
                self.assertEqual(
                    validation["classification"],
                    "confirmed_validation_failure",
                )
                self.assertEqual(validation["executed_ids"], intended)
                self.assertEqual(validation["confirmed_failure_ids"], intended)
                self.assertIn(
                    "omitted or corrupted its final JSON report",
                    validation["diagnostic"],
                )
                child = next(
                    item
                    for item in report["child_processes"]
                    if item["validation_id"] == "sdk.typescript.jest"
                )
                self.assertEqual(child["pid"], launch["pid"])
                if options.get("later_supervision_error"):
                    self.assertIsNone(child["exit_code"])

    def _write_rust_gate_fixture(
        self, *, mode: str, gate_name: str = RUST_GATE_NAME
    ) -> None:
        scripts_dir = self.repository / "scripts"
        scripts_dir.mkdir(exist_ok=True)
        manifest_dir = self.repository / "codex-rs" / ".config"
        manifest_dir.mkdir(parents=True, exist_ok=True)
        intended = ", ".join(json.dumps(value) for value in RUST_GATE_TEST_IDS)
        companion_marker = self.base / "canonical-rust-gate-companion.txt"
        companion_command = ", ".join(
            json.dumps(value)
            for value in (
                sys.executable,
                str(self.validator),
                str(companion_marker),
                "pass",
            )
        )
        (manifest_dir / "kd4-rust-tests.toml").write_text(
            textwrap.dedent(
                f"""\
                version = 1

                [helpers]

                [targets.fixture]
                package = "fixture-package"
                test = "fixture-test"
                helpers = []

                [gates.{gate_name}]
                description = "strict fixture gate"

                [[gates.{gate_name}.steps]]
                target = "fixture"
                tests = [{intended}]
                """
            ),
            encoding="utf-8",
        )
        self._write_json(
            self.repository / "rust-gate-fixture.json",
            {
                "gate": gate_name,
                "marker": str(self.marker),
                "mode": mode,
                "tests": RUST_GATE_TEST_IDS,
            },
        )
        (scripts_dir / "rust_test_runner.py").write_text(
            textwrap.dedent(
                """\
                from __future__ import annotations

                import argparse
                import json
                import os
                import pathlib
                import sys
                import tomllib


                repository = pathlib.Path(__file__).resolve().parents[1]
                settings = json.loads(
                    (repository / "rust-gate-fixture.json").read_text(encoding="utf-8")
                )
                expected_gate = settings["gate"]
                expected_tests = settings["tests"]

                parser = argparse.ArgumentParser()
                parser.add_argument("command")
                parser.add_argument("--profile", required=True)
                parser.add_argument("gate")
                parser.add_argument("--proof-report", required=True, type=pathlib.Path)
                parser.add_argument("--proof-execution-id", required=True)
                args = parser.parse_args()
                if args.command != "run-gate-proof":
                    raise SystemExit("unexpected runner command")
                if args.profile != "completion-proof":
                    raise SystemExit("unexpected runner profile")
                if args.gate != expected_gate:
                    raise SystemExit("unexpected gate")
                leaked = sorted(
                    name
                    for name in os.environ
                    if name.casefold().startswith("codex_completion_proof_")
                )
                if leaked:
                    raise SystemExit(f"private completion-proof environment leaked: {leaked}")

                manifest = tomllib.loads(
                    (
                        repository
                        / "codex-rs"
                        / ".config"
                        / "kd4-rust-tests.toml"
                    ).read_text(encoding="utf-8")
                )
                if set(manifest) != {"version", "helpers", "targets", "gates"}:
                    raise SystemExit("unexpected manifest root")
                if manifest["version"] != 1 or manifest["helpers"] != {}:
                    raise SystemExit("unexpected manifest version or helpers")
                if set(manifest["targets"]) != {"fixture"}:
                    raise SystemExit("unexpected manifest targets")
                target = manifest["targets"]["fixture"]
                if target != {
                    "package": "fixture-package",
                    "test": "fixture-test",
                    "helpers": [],
                }:
                    raise SystemExit("unexpected fixture target")
                if set(manifest["gates"]) != {expected_gate}:
                    raise SystemExit("unexpected manifest gates")
                gate = manifest["gates"][expected_gate]
                if set(gate) != {"description", "steps"} or len(gate["steps"]) != 1:
                    raise SystemExit("unexpected gate shape")
                step = gate["steps"][0]
                if step != {"target": "fixture", "tests": expected_tests}:
                    raise SystemExit("unexpected gate selection")

                launch = {
                    "argv": sys.argv[1:],
                    "execution_id": args.proof_execution_id,
                    "gate": args.gate,
                    "profile": args.profile,
                    "proof_report": str(args.proof_report),
                    "windows_process_prerequisites_required": os.environ.get(
                        "CODEX_REQUIRE_WINDOWS_SANDBOX_PROCESS_TESTS"
                    ),
                }
                marker = pathlib.Path(settings["marker"])
                with marker.open("a", encoding="utf-8") as output:
                    output.write(json.dumps(launch, sort_keys=True) + "\\n")

                mode = settings["mode"]
                if mode == "process-prerequisites":
                    if os.environ.get(
                        "CODEX_REQUIRE_WINDOWS_SANDBOX_PROCESS_TESTS"
                    ) == "1":
                        classification = "confirmed_pass"
                        executed = list(expected_tests)
                        outcomes = [
                            {"id": test_id, "outcome": "passed"}
                            for test_id in expected_tests
                        ]
                        exit_code = 0
                        diagnostic = ""
                    else:
                        classification = "pre_result_error"
                        executed = []
                        outcomes = []
                        exit_code = 2
                        diagnostic = (
                            "Windows sandbox process prerequisites were not required"
                        )
                elif mode == "pass":
                    classification = "confirmed_pass"
                    executed = list(expected_tests)
                    outcomes = [
                        {"id": test_id, "outcome": "passed"}
                        for test_id in expected_tests
                    ]
                    exit_code = 0
                    diagnostic = ""
                elif mode == "missing-evidence":
                    classification = "confirmed_pass"
                    executed = []
                    outcomes = []
                    exit_code = 0
                    diagnostic = "runner exited successfully without terminal events"
                elif mode == "failure-then-infrastructure":
                    classification = "confirmed_validation_failure"
                    executed = [expected_tests[0]]
                    outcomes = [{"id": expected_tests[0], "outcome": "failed"}]
                    exit_code = 100
                    diagnostic = "later gate step ended in an infrastructure error"
                else:
                    raise SystemExit("unexpected fixture mode")

                report = {
                    "schema_version": 1,
                    "report_type": "RustNamedGateExecutionReportV1",
                    "gate": args.gate,
                    "execution_id": args.proof_execution_id,
                    "classification": classification,
                    "intended_ids": expected_tests,
                    "selected_ids": expected_tests,
                    "executed_ids": executed,
                    "outcomes": outcomes,
                    "diagnostic": diagnostic,
                }
                args.proof_report.write_text(
                    json.dumps(report, sort_keys=True) + "\\n",
                    encoding="utf-8",
                )
                raise SystemExit(exit_code)
                """
            ),
            encoding="utf-8",
        )

        frozen_inventory_hash = _inventory_hash(self.baseline_rows)
        self.config.write_text(
            textwrap.dedent(
                f"""\
                schema_version = 2
                policy_id = "fixture"
                frozen_inventory_hash = {json.dumps(frozen_inventory_hash)}
                canonical_command = "just completion-proof"
                focused_command = "just completion-focused {{validation_id}}"
                documentation_command = "just source-map-check"
                repository_root = {json.dumps(self.repository.as_posix())}
                frozen_inventory = ".codex/validation/frozen.json"
                replacement_ledger = ".codex/validation/ledger.json"
                testing_current_inventory = ".codex/validation/current.json"
                host_platform = {json.dumps(platform.system().casefold())}

                [[validation]]
                id = {json.dumps(RUST_GATE_VALIDATION_ID)}
                runner = "rust-gate"
                gate = {json.dumps(gate_name)}
                owned_paths = ["codex-rs/**"]
                consumed_paths = ["codex-rs/**", "scripts/rust_test_runner.py"]
                timeout_seconds = 30

                [[validation]]
                id = "fixture.command"
                runner = "typed-validation"
                validation_type = "fixture-command"
                command = [{companion_command}]
                validation_failure_exit_codes = [1]
                owned_paths = ["validator.py"]
                consumed_paths = ["validator.py"]
                timeout_seconds = 30
                """
            ),
            encoding="utf-8",
        )

    def _git(self, *args: str) -> subprocess.CompletedProcess[str]:
        result = subprocess.run(
            ["git", *args],
            cwd=self.repository,
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return result

    def _base_env(self) -> dict[str, str]:
        env = os.environ.copy()
        env["cOdEx_CoMpLeTiOn_PrOoF_TESTING"] = "must-not-leak"
        return env

    def _unittest_runner_command(
        self,
        *arguments: str,
        patch_source: str | None = None,
    ) -> list[str]:
        patch_statement = (
            f"exec({patch_source!r}, runner_globals); "
            if patch_source is not None
            else ""
        )
        return [
            sys.executable,
            "-c",
            (
                "import runpy,sys; module=runpy.run_path(sys.argv[1]); "
                "runner_globals=module['_unittest_main'].__globals__; "
                "runner_globals['_attest_runner_process']="
                "lambda runtime,identity: None; "
                + patch_statement
                + "raise SystemExit(module['_unittest_main'](sys.argv[2:]))"
            ),
            str(RUNNER),
            *arguments,
        ]

    def _fingerprint(self) -> str:
        result = subprocess.run(
            self._unittest_runner_command("--config", str(self.config), "fingerprint"),
            cwd=self.repository,
            env=self._base_env(),
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout.strip()

    def _run(
        self,
        *,
        report: Path | None = None,
        patch_source: str | None = None,
        start_fingerprint: str | None = None,
    ) -> tuple[subprocess.CompletedProcess[str], Path]:
        report = report or self.base / f"report-{uuid.uuid4()}.json"
        command, env = self._canonical_invocation(
            report,
            patch_source=patch_source,
            start_fingerprint=start_fingerprint,
        )
        result = subprocess.run(
            command,
            cwd=self.repository,
            env=env,
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
        )
        return result, report

    def _canonical_invocation(
        self,
        report: Path,
        *,
        patch_source: str | None = None,
        start_fingerprint: str | None = None,
    ) -> tuple[list[str], dict[str, str]]:
        env = self._base_env()
        env.update(
            {
                "CODEX_COMPLETION_PROOF_NONCE": uuid.uuid4().hex * 2,
                "CODEX_COMPLETION_PROOF_REPORT": str(report),
                "CODEX_COMPLETION_PROOF_ATTEMPT_ID": str(uuid.uuid4()),
                "CODEX_COMPLETION_PROOF_PARENT_PID": str(os.getpid()),
                "CODEX_COMPLETION_PROOF_REPOSITORY": str(self.repository.resolve()),
                "CODEX_COMPLETION_PROOF_START_FINGERPRINT": (
                    start_fingerprint or self._fingerprint()
                ),
                "CODEX_COMPLETION_PROOF_MUTATION_EPOCH": "7",
                "CODEX_COMPLETION_PROOF_POLICY_RUNNER_BUNDLE_SHA256": "a" * 64,
                "CODEX_COMPLETION_PROOF_RUNNER_ATTESTATION_ENDPOINT": (
                    "unit-test-endpoint"
                ),
            }
        )
        return (
            self._unittest_runner_command(
                "--config",
                str(self.config),
                "run",
                patch_source=patch_source,
            ),
            env,
        )

    def _start_run(self) -> tuple[subprocess.Popen[str], Path]:
        report = self.base / f"report-{uuid.uuid4()}.json"
        command, env = self._canonical_invocation(report)
        process = subprocess.Popen(
            command,
            cwd=self.repository,
            env=env,
            text=True,
            encoding="utf-8",
            errors="replace",
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        return process, report

    def _wait_for_validator(self, process: subprocess.Popen[str]) -> None:
        ready = self.base / "validator-ready"
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if ready.exists():
                return
            if process.poll() is not None:
                stdout, stderr = process.communicate()
                self.fail(
                    "canonical runner exited before its validation started: "
                    f"stdout={stdout!r}, stderr={stderr!r}"
                )
            time.sleep(0.02)
        process.kill()
        stdout, stderr = process.communicate()
        self.fail(
            "canonical validation did not start before timeout: "
            f"stdout={stdout!r}, stderr={stderr!r}"
        )

    def _run_focused(
        self,
        validation_id: str,
        *,
        report: Path | None = None,
        patch_source: str | None = None,
    ) -> tuple[subprocess.CompletedProcess[str], Path]:
        report = report or self.base / f"focused-report-{uuid.uuid4()}.json"
        env = self._base_env()
        env.update(
            {
                "CODEX_COMPLETION_PROOF_NONCE": uuid.uuid4().hex * 2,
                "CODEX_COMPLETION_PROOF_REPORT": str(report),
                "CODEX_COMPLETION_PROOF_ATTEMPT_ID": str(uuid.uuid4()),
                "CODEX_COMPLETION_PROOF_PARENT_PID": str(os.getpid()),
                "CODEX_COMPLETION_PROOF_REPOSITORY": str(self.repository.resolve()),
                "CODEX_COMPLETION_PROOF_START_FINGERPRINT": self._fingerprint(),
                "CODEX_COMPLETION_PROOF_MUTATION_EPOCH": "7",
                "CODEX_COMPLETION_PROOF_POLICY_RUNNER_BUNDLE_SHA256": "a" * 64,
                "CODEX_COMPLETION_PROOF_RUNNER_ATTESTATION_ENDPOINT": (
                    "unit-test-endpoint"
                ),
            }
        )
        result = subprocess.run(
            self._unittest_runner_command(
                "--config",
                str(self.config),
                "focused",
                validation_id,
                patch_source=patch_source,
            ),
            cwd=self.repository,
            env=env,
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
        )
        return result, report

    def _load_report(self, path: Path) -> dict[str, object]:
        value = json.loads(path.read_text(encoding="utf-8"))
        claimed_hash = value.pop("attempt_report_hash")
        self.assertEqual(
            claimed_hash, hashlib.sha256(_canonical_json(value)).hexdigest()
        )
        value["attempt_report_hash"] = claimed_hash
        return value

    def _assert_file_identity(self, identity: dict[str, object]) -> None:
        self.assertIsInstance(identity["requested"], str)
        self.assertTrue(identity["resolved_path"])
        self.assertEqual(len(identity["sha256_before"]), 64)
        self.assertEqual(identity["sha256_before"], identity["sha256_after"])

    def _assert_v2_process_identities(self, report: dict[str, object]) -> None:
        self.assertEqual(report["schema_version"], 2)
        self.assertEqual(report["policy_id"], "fixture")
        self.assertEqual(report["policy_runner_bundle_sha256"], "a" * 64)
        runner = report["runner_process_identity"]
        self.assertGreater(runner["pid"], 0)
        self.assertGreater(runner["parent_pid"], 0)
        self.assertEqual(runner["parent_pid"], report["observed_runner_parent_pid"])
        self.assertLessEqual(runner["started_at"], runner["ended_at"])
        self.assertEqual(len(runner["args_hash"]), 64)
        self._assert_file_identity(runner["executable_identity"])
        self._assert_file_identity(runner["entrypoint_identity"])
        for child in report["child_processes"]:
            if child["pid"] > 0:
                self._assert_file_identity(child["launch_target_identity"])

    def _load_single_rust_gate_launch(self) -> dict[str, object]:
        launches = [
            json.loads(line)
            for line in self.marker.read_text(encoding="utf-8").splitlines()
        ]
        self.assertEqual(len(launches), 1)
        launch = launches[0]
        argv = launch["argv"]
        self.assertEqual(
            argv[:5],
            [
                "run-gate-proof",
                "--profile",
                "completion-proof",
                RUST_GATE_NAME,
                "--proof-report",
            ],
        )
        self.assertEqual(argv[6], "--proof-execution-id")
        self.assertEqual(len(argv), 8)
        self.assertEqual(launch["gate"], RUST_GATE_NAME)
        self.assertEqual(launch["profile"], "completion-proof")
        self.assertEqual(launch["proof_report"], argv[5])
        self.assertEqual(launch["execution_id"], argv[7])
        return launch

    def test_fixture_entrypoint_records_fresh_execution_for_each_attempt(self) -> None:
        first_result, first_path = self._run()
        second_result, second_path = self._run()
        self.assertEqual(first_result.returncode, 0, first_result.stderr)
        self.assertEqual(second_result.returncode, 0, second_result.stderr)
        first = self._load_report(first_path)
        second = self._load_report(second_path)
        self.assertEqual(first["report_type"], "CompletionProofAttemptReportV2")
        self._assert_v2_process_identities(first)
        self._assert_v2_process_identities(second)
        self.assertEqual(first["attempt_classification"], "confirmed_pass")
        self.assertEqual(first["exact_command"], "just completion-proof")
        first_validation = next(
            item for item in first["validations"] if item["id"] == "fixture.command"
        )
        second_validation = next(
            item for item in second["validations"] if item["id"] == "fixture.command"
        )
        self.assertEqual(first_validation["runner"], "typed-validation")
        self.assertIsNone(first_validation["runner_selector"])
        self.assertEqual(first_validation["evidence_kind"], "typed_non_test")
        self.assertEqual(first_validation["validation_type"], "fixture-command")
        self.assertEqual(
            first_validation["input_contract_digest"],
            COMPLETION_PROOF._validation_input_contract_digest(
                {
                    "owned_paths": ["validator.py"],
                    "consumed_paths": ["validator.py"],
                }
            ),
        )
        self.assertEqual(first_validation["intended_ids"], ["fixture.command"])
        self.assertEqual(first_validation["selected_ids"], ["fixture.command"])
        self.assertEqual(first_validation["executed_ids"], ["fixture.command"])
        self.assertEqual(first_validation["intended_count"], 1)
        self.assertEqual(first_validation["selected_count"], 1)
        self.assertEqual(first_validation["executed_count"], 1)
        self.assertEqual(
            first_validation["outcomes"],
            [{"id": "fixture.command", "outcome": "passed"}],
        )
        self.assertNotEqual(
            first_validation["execution_id"], second_validation["execution_id"]
        )
        self.assertNotEqual(first["attempt_report_hash"], second["attempt_report_hash"])
        marker_ids = self.marker.read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(marker_ids), 2)
        self.assertEqual(len(set(marker_ids)), 2)
        confirmed = [
            (item["id"], item["execution_id"])
            for item in first["validations"]
            if item["classification"] == "confirmed_pass"
        ]
        child_evidence = [
            (item["validation_id"], item["execution_id"])
            for item in first["child_processes"]
            if item["pid"] is not None and item["exit_code"] == 0
        ]
        self.assertCountEqual(confirmed, child_evidence)
        self.assertEqual(len(first["validations"]), len(first["child_processes"]))

    def test_production_discovery_children_are_not_certification_children(self) -> None:
        self._write_config(mode="pass", use_testing_inventory=False)
        discovery_marker = self.base / "discovery-executed.txt"
        discovery_child_code = (
            "from pathlib import Path; import sys,uuid; "
            "Path(sys.argv[1]).write_text(str(uuid.uuid4()), encoding='utf-8')"
        )
        patch_source = textwrap.dedent(
            f"""\
            import sys as _fixture_sys
            import uuid as _fixture_uuid

            def _fixture_discover_inventory(repo_root, *, temp_dir):
                del temp_dir
                result = run_process(
                    validation_id="inventory.fixture-discovery",
                    execution_id=str(_fixture_uuid.uuid4()),
                    command=[
                        _fixture_sys.executable,
                        "-c",
                        {discovery_child_code!r},
                        {str(discovery_marker)!r},
                    ],
                    cwd=repo_root,
                    env=_network_disabled_env(),
                    timeout_seconds=30,
                )
                if result.invocation_error or result.returncode != 0:
                    raise ProofError("fixture discovery process failed")
                return {self.current_rows!r}, result.child

            discover_inventory = _fixture_discover_inventory
            """
        )

        result, report_path = self._run(patch_source=patch_source)

        self.assertEqual(result.returncode, 0, result.stderr)
        uuid.UUID(discovery_marker.read_text(encoding="utf-8"))
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "confirmed_pass")
        expected_children = [
            (validation["id"], validation["execution_id"])
            for validation in report["validations"]
        ]
        actual_children = [
            (child["validation_id"], child["execution_id"])
            for child in report["child_processes"]
        ]
        self.assertCountEqual(expected_children, actual_children)
        self.assertNotIn(
            "inventory.fixture-discovery",
            [child["validation_id"] for child in report["child_processes"]],
        )

    def test_claimed_wrapper_pass_requires_exact_nonzero_execution_evidence(
        self,
    ) -> None:
        native_ids = ["fixture.first", "fixture.second"]
        current_rows = [
            {
                "baseline_id": f"python-unittest::{native_id}",
                "framework": "python-unittest",
                "native_id": native_id,
                "source": "validator.py",
                "ignored": False,
                "platforms": [platform.system().casefold()],
            }
            for native_id in native_ids
        ]
        self._write_json(
            self.current_inventory,
            {"schema_version": 1, "tests": current_rows},
        )
        self._write_json(
            self.ledger,
            {
                "schema_version": 1,
                "frozen_inventory_hash": _inventory_hash(self.baseline_rows),
                "rows": [
                    {
                        "baseline_id": "fixture-command::old-behavior",
                        "resolution": "replacement",
                        "replacement_ids": [
                            str(row["baseline_id"]) for row in current_rows
                        ],
                        "preserved_behavior": "the fixture wrapper executes",
                        "product_path": "structured unittest wrapper",
                        "validation_id": "maintenance.root-unittest",
                    }
                ],
                "overrides": [],
            },
        )
        self.config.write_text(
            textwrap.dedent(
                f"""\
                schema_version = 2
                policy_id = "fixture"
                frozen_inventory_hash = {json.dumps(_inventory_hash(self.baseline_rows))}
                canonical_command = "just completion-proof"
                focused_command = "just completion-focused {{validation_id}}"
                documentation_command = "just source-map-check"
                repository_root = {json.dumps(self.repository.as_posix())}
                frozen_inventory = ".codex/validation/frozen.json"
                replacement_ledger = ".codex/validation/ledger.json"
                testing_current_inventory = ".codex/validation/current.json"
                host_platform = {json.dumps(platform.system().casefold())}

                [[validation]]
                id = "maintenance.root-unittest"
                runner = "python-unittest"
                owned_paths = ["validator.py"]
                consumed_paths = ["validator.py"]
                timeout_seconds = 30
                """
            ),
            encoding="utf-8",
        )
        wrapper = self.base / "claimed-pass-wrapper.py"
        cached_wrapper_report = self.base / "cached-wrapper-report.json"
        wrapper.write_text(
            textwrap.dedent(
                """\
                import json
                import pathlib
                import sys
                import time

                intended = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
                output = pathlib.Path(sys.argv[2])
                mode = sys.argv[3]
                marker = pathlib.Path(sys.argv[4])
                cache = pathlib.Path(sys.argv[5])
                flags = {
                    sys.argv[index]: sys.argv[index + 1]
                    for index in range(6, len(sys.argv) - 1, 2)
                }
                if mode == "replay":
                    report = json.loads(cache.read_text(encoding="utf-8"))
                    output.write_text(json.dumps(report), encoding="utf-8")
                    with marker.open("a", encoding="utf-8") as stream:
                        stream.write(mode + "\\n")
                    raise SystemExit(0)
                if mode in {
                    "save",
                    "mismatch",
                    "failure",
                    "failure-infra",
                    "started-out-of-order",
                    "terminal-out-of-order",
                }:
                    executed = intended
                elif mode == "failure-timeout":
                    executed = intended[:1]
                else:
                    executed = [] if mode == "zero" else intended[:1]
                started = list(executed)
                terminal = list(executed)
                if mode == "started-out-of-order":
                    started.reverse()
                if mode == "terminal-out-of-order":
                    terminal.reverse()
                    executed = list(terminal)
                report = {
                    "schema_version": 2,
                    "report_type": "CompletionProofStructuredTestReportV2",
                    "framework": "python-unittest",
                    "proof_attempt_id": flags["--proof-attempt-id"],
                    "proof_execution_id": flags["--proof-execution-id"],
                    "proof_receipt_nonce": flags["--proof-receipt-nonce"],
                    "proof_scope": flags["--proof-scope"],
                    "classification": (
                        "confirmed_validation_failure"
                        if mode in {"failure", "failure-infra", "failure-timeout"}
                        else "confirmed_pass"
                    ),
                    "selection_confirmed": True,
                    "intended_ids": intended,
                    "selected_ids": intended,
                    "started_ids": started,
                    "terminal_ids": terminal,
                    "executed_ids": executed,
                    "outcomes": [
                        {
                            "id": test_id,
                            "outcome": (
                                "failed"
                                if mode in {"failure", "failure-infra", "failure-timeout"}
                                and test_id == executed[0]
                                else "passed"
                            ),
                        }
                        for test_id in executed
                    ],
                }
                if mode == "mismatch":
                    report["proof_execution_id"] += "-wrong"
                if mode == "save":
                    cache.write_text(json.dumps(report), encoding="utf-8")
                with marker.open("a", encoding="utf-8") as stream:
                    stream.write(mode + "\\n")
                output.write_text(json.dumps(report), encoding="utf-8")
                if mode == "failure-timeout":
                    time.sleep(60)
                raise SystemExit(
                    2 if mode == "failure-infra" else 1
                    if mode == "failure" else 0
                )
                """
            ),
            encoding="utf-8",
        )

        for mode in ("zero", "partial"):
            with self.subTest(mode=mode):
                wrapper_command = [
                    sys.executable,
                    str(wrapper),
                    "{expected_file}",
                    "{report_file}",
                    mode,
                    str(self.marker),
                    str(cached_wrapper_report),
                ]
                patch_source = textwrap.dedent(
                    f"""\
                    _fixture_original_structured_wrapper = _run_structured_wrapper

                    def _fixture_structured_wrapper(**kwargs):
                        kwargs["command"] = {wrapper_command!r}
                        return _fixture_original_structured_wrapper(**kwargs)

                    _run_structured_wrapper = _fixture_structured_wrapper
                    """
                )
                result, report_path = self._run(patch_source=patch_source)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertNotIn("COMPLETION PROOF PASSED", result.stdout)
                report = self._load_report(report_path)
                self.assertEqual(report["attempt_classification"], "pre_result_error")
                self.assertEqual(len(report["validations"]), 1)
                validation = report["validations"][0]
                self.assertEqual(validation["id"], "maintenance.root-unittest")
                self.assertEqual(validation["classification"], "pre_result_error")
                self.assertEqual(validation["executed_ids"], [])

        self.assertEqual(
            self.marker.read_text(encoding="utf-8").splitlines(),
            ["zero", "partial"],
        )

        def run_wrapper(
            mode: str, *, focused: bool = False
        ) -> tuple[subprocess.CompletedProcess[str], dict[str, object]]:
            wrapper_command = [
                sys.executable,
                str(wrapper),
                "{expected_file}",
                "{report_file}",
                mode,
                str(self.marker),
                str(cached_wrapper_report),
            ]
            patch_source = textwrap.dedent(
                f"""\
                _fixture_original_structured_wrapper = _run_structured_wrapper

                def _fixture_structured_wrapper(**kwargs):
                    kwargs["command"] = {wrapper_command!r}
                    if {mode!r} == "failure-timeout":
                        kwargs["timeout_seconds"] = 1
                    return _fixture_original_structured_wrapper(**kwargs)

                _run_structured_wrapper = _fixture_structured_wrapper
                """
            )
            if focused:
                result, report_path = self._run_focused(
                    "maintenance.root-unittest",
                    patch_source=patch_source,
                )
            else:
                result, report_path = self._run(patch_source=patch_source)
            return result, self._load_report(report_path)

        mismatch_result, mismatch_report = run_wrapper("mismatch")
        self.assertEqual(mismatch_result.returncode, 2, mismatch_result.stderr)
        mismatch_validation = mismatch_report["validations"][0]
        self.assertEqual(mismatch_validation["classification"], "pre_result_error")
        self.assertEqual(mismatch_validation["executed_ids"], [])
        self.assertIn("invocation binding mismatch", mismatch_validation["diagnostic"])

        failure_result, failure_report = run_wrapper("failure")
        self.assertEqual(failure_result.returncode, 1, failure_result.stderr)
        failure_validation = failure_report["validations"][0]
        self.assertEqual(
            failure_validation["classification"],
            "confirmed_validation_failure",
        )
        self.assertEqual(failure_validation["confirmed_failure_ids"], [native_ids[0]])

        infra_result, infra_report = run_wrapper("failure-infra")
        self.assertEqual(infra_result.returncode, 1, infra_result.stderr)
        infra_validation = infra_report["validations"][0]
        self.assertEqual(
            infra_validation["classification"],
            "confirmed_validation_failure",
        )
        self.assertEqual(infra_validation["confirmed_failure_ids"], [native_ids[0]])

        for focused in (False, True):
            with self.subTest(mode="failure-timeout", focused=focused):
                timeout_result, timeout_report = run_wrapper(
                    "failure-timeout",
                    focused=focused,
                )
                self.assertEqual(timeout_result.returncode, 1, timeout_result.stderr)
                timeout_validation = timeout_report["validations"][0]
                self.assertEqual(
                    timeout_validation["classification"],
                    "confirmed_validation_failure",
                )
                self.assertEqual(
                    timeout_validation["confirmed_failure_ids"],
                    [native_ids[0]],
                )
                self.assertEqual(timeout_validation["executed_ids"], [native_ids[0]])
                self.assertIsNone(timeout_validation["exit_code"])
                self.assertIn("timed out", timeout_validation["diagnostic"])

        for mode in ("started-out-of-order", "terminal-out-of-order"):
            for focused in (False, True):
                with self.subTest(mode=mode, focused=focused):
                    order_result, order_report = run_wrapper(mode, focused=focused)
                    self.assertEqual(order_result.returncode, 2, order_result.stderr)
                    order_validation = order_report["validations"][0]
                    self.assertEqual(
                        order_validation["classification"], "pre_result_error"
                    )
                    self.assertEqual(order_validation["executed_ids"], [])
                    self.assertEqual(order_validation["confirmed_failure_ids"], [])
                    self.assertIn(
                        "lifecycle identity mismatch",
                        order_validation["diagnostic"],
                    )

        saved_result, saved_report = run_wrapper("save")
        self.assertEqual(saved_result.returncode, 0, saved_result.stderr)
        self.assertEqual(saved_report["attempt_classification"], "confirmed_pass")
        replay_result, replay_report = run_wrapper("replay")
        self.assertEqual(replay_result.returncode, 2, replay_result.stderr)
        replay_validation = replay_report["validations"][0]
        self.assertEqual(replay_validation["classification"], "pre_result_error")
        self.assertEqual(replay_validation["executed_ids"], [])

        focused_result, focused_report = run_wrapper("save", focused=True)
        self.assertEqual(focused_result.returncode, 0, focused_result.stderr)
        self.assertEqual(focused_report["attempt_classification"], "confirmed_pass")
        copied_focused_result, copied_focused_report = run_wrapper("replay")
        self.assertEqual(
            copied_focused_result.returncode,
            2,
            copied_focused_result.stderr,
        )
        copied_focused_validation = copied_focused_report["validations"][0]
        self.assertEqual(
            copied_focused_validation["classification"], "pre_result_error"
        )
        self.assertEqual(copied_focused_validation["executed_ids"], [])

    def test_structured_wrapper_zero_selection_never_launches_through_focused_cli(
        self,
    ) -> None:
        with self.config.open("a", encoding="utf-8") as stream:
            stream.write(
                textwrap.dedent(
                    """\

                    [[validation]]
                    id = "maintenance.root-unittest"
                    runner = "python-unittest"
                    owned_paths = ["validator.py"]
                    consumed_paths = ["validator.py"]
                    timeout_seconds = 30
                    """
                )
            )

        result, report_path = self._run_focused("maintenance.root-unittest")

        self.assertEqual(result.returncode, 2, result.stderr)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        validation = report["validations"][0]
        self.assertEqual(validation["classification"], "pre_result_error")
        self.assertEqual(validation["intended_count"], 0)
        self.assertEqual(validation["executed_count"], 0)
        self.assertEqual(report["child_processes"][0]["pid"], 0)
        self.assertFalse(self.marker.exists())

    def test_unittest_wrapper_reports_attempt_bound_start_and_terminal_ids(
        self,
    ) -> None:
        test_module = self.repository / "fixture_unit_test.py"
        test_module.write_text(
            textwrap.dedent(
                """\
                import unittest

                class RuntimePathTest(unittest.TestCase):
                    def test_runs(self):
                        self.assertTrue(True)
                """
            ),
            encoding="utf-8",
        )
        test_id = "fixture_unit_test.RuntimePathTest.test_runs"
        expected = self.base / "unittest-expected.json"
        output = self.base / "unittest-output.json"
        self._write_json(expected, [test_id])
        binding = {
            "proof_attempt_id": str(uuid.uuid4()),
            "proof_execution_id": str(uuid.uuid4()),
            "proof_receipt_nonce": uuid.uuid4().hex * 2,
            "proof_scope": "canonical",
        }
        wrapper = REPO_ROOT / "scripts" / "completion_proof_unittest.py"
        command = [
            sys.executable,
            "-c",
            (
                "import runpy,sys; module=runpy.run_path(sys.argv[1]); "
                "module['_run'].__globals__['_targets']=lambda:[sys.argv[2]]; "
                "raise SystemExit(module['main'](sys.argv[3:]))"
            ),
            str(wrapper),
            test_id,
            "run",
            "--expected-file",
            str(expected),
            "--output",
            str(output),
            "--proof-attempt-id",
            binding["proof_attempt_id"],
            "--proof-execution-id",
            binding["proof_execution_id"],
            "--proof-receipt-nonce",
            binding["proof_receipt_nonce"],
            "--proof-scope",
            binding["proof_scope"],
        ]

        result = subprocess.run(
            command,
            cwd=self.repository,
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(output.read_text(encoding="utf-8"))
        self.assertEqual(report["classification"], "confirmed_pass")
        self.assertEqual(report["started_ids"], [test_id])
        self.assertEqual(report["terminal_ids"], [test_id])
        self.assertEqual(report["executed_ids"], [test_id])
        for key, value in binding.items():
            self.assertEqual(report[key], value)

    def test_pytest_wrapper_reports_attempt_bound_start_and_terminal_ids(self) -> None:
        tests_dir = self.repository / "tests"
        tests_dir.mkdir()
        test_file = tests_dir / "test_runtime_path.py"
        test_file.write_text(
            "def test_runs():\n    assert True\n",
            encoding="utf-8",
        )
        test_id = "tests/test_runtime_path.py::test_runs"
        expected = self.base / "pytest-expected.json"
        output = self.base / "pytest-output.json"
        self._write_json(expected, [test_id])
        binding = {
            "proof_attempt_id": str(uuid.uuid4()),
            "proof_execution_id": str(uuid.uuid4()),
            "proof_receipt_nonce": uuid.uuid4().hex * 2,
            "proof_scope": "focused",
        }
        command = [
            "uv",
            "run",
            "--offline",
            "--frozen",
            "--project",
            str(REPO_ROOT / "sdk" / "python"),
            "python",
            str(REPO_ROOT / "scripts" / "completion_proof_pytest.py"),
            "run",
            "--expected-file",
            str(expected),
            "--output",
            str(output),
            "--proof-attempt-id",
            binding["proof_attempt_id"],
            "--proof-execution-id",
            binding["proof_execution_id"],
            "--proof-receipt-nonce",
            binding["proof_receipt_nonce"],
            "--proof-scope",
            binding["proof_scope"],
        ]

        result = subprocess.run(
            command,
            cwd=self.repository,
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(output.read_text(encoding="utf-8"))
        self.assertEqual(report["classification"], "confirmed_pass")
        self.assertEqual(report["started_ids"], [test_id])
        self.assertEqual(report["terminal_ids"], [test_id])
        self.assertEqual(report["executed_ids"], [test_id])
        for key, value in binding.items():
            self.assertEqual(report[key], value)

    def test_live_service_exception_requires_ignored_frozen_row_through_canonical_cli(
        self,
    ) -> None:
        foreign_platform = self._foreign_inventory_platform()
        self._write_baseline_exception_fixture(
            kind="live-service",
            frozen_ignored=False,
            current_ignored=True,
            frozen_platforms=[foreign_platform],
            current_platforms=[foreign_platform],
        )

        result, report_path = self._run()

        self.assertEqual(result.returncode, 2, result.stderr)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertIn(
            "live-service exception frozen inventory row must have ignored=true",
            report["fatal_error"],
        )
        self.assertEqual(report["child_processes"], [])
        self.assertFalse(self.marker.exists())

    def test_live_service_exception_requires_exact_true_in_current_row_through_canonical_cli(
        self,
    ) -> None:
        foreign_platform = self._foreign_inventory_platform()
        self._write_baseline_exception_fixture(
            kind="live-service",
            frozen_ignored=True,
            current_ignored="true",
            frozen_platforms=[foreign_platform],
            current_platforms=[foreign_platform],
        )

        result, report_path = self._run()

        self.assertEqual(result.returncode, 2, result.stderr)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertIn(
            "live-service exception current inventory row must have ignored=true",
            report["fatal_error"],
        )
        self.assertEqual(report["child_processes"], [])
        self.assertFalse(self.marker.exists())

    def test_valid_live_service_exception_reaches_canonical_cli(self) -> None:
        active_platform = platform.system().casefold()
        self._write_baseline_exception_fixture(
            kind="live-service",
            frozen_ignored=True,
            current_ignored=True,
            frozen_platforms=[active_platform],
            current_platforms=[active_platform],
        )

        result, report_path = self._run()

        self.assertEqual(result.returncode, 0, result.stderr)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "confirmed_pass")
        self.assertEqual(
            report["exceptions"],
            [
                {
                    "baseline_id": "fixture-command::old-behavior",
                    "kind": "live-service",
                    "source": "completion-proof CLI fixture",
                    "text": "the fixture exception remains explicitly quarantined",
                }
            ],
        )
        self.assertEqual(
            len(self.marker.read_text(encoding="utf-8").splitlines()),
            1,
        )

    def test_off_host_rejects_active_frozen_platform_case_insensitively(
        self,
    ) -> None:
        active_platform = platform.system().casefold()
        foreign_platform = self._foreign_inventory_platform()
        self._write_baseline_exception_fixture(
            kind="off-host",
            frozen_ignored=False,
            current_ignored=False,
            frozen_platforms=[active_platform.upper()],
            current_platforms=[foreign_platform],
        )

        result, report_path = self._run()

        self.assertEqual(result.returncode, 2, result.stderr)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertIn(
            "off-host exception frozen inventory row includes active platform "
            f"{active_platform!r}",
            report["fatal_error"],
        )
        self.assertEqual(report["child_processes"], [])
        self.assertFalse(self.marker.exists())

    def test_off_host_exception_rejects_non_string_current_platform_through_canonical_cli(
        self,
    ) -> None:
        foreign_platform = self._foreign_inventory_platform()
        self._write_baseline_exception_fixture(
            kind="off-host",
            frozen_ignored=False,
            current_ignored=False,
            frozen_platforms=[foreign_platform],
            current_platforms=[1],
        )

        result, report_path = self._run()

        self.assertEqual(result.returncode, 2, result.stderr)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertIn(
            "off-host exception current inventory row must have exact nonempty platform names",
            report["fatal_error"],
        )
        self.assertEqual(report["child_processes"], [])
        self.assertFalse(self.marker.exists())

    def test_platform_pending_exception_rejects_non_list_frozen_platforms_through_canonical_cli(
        self,
    ) -> None:
        foreign_platform = self._foreign_inventory_platform()
        self._write_baseline_exception_fixture(
            kind="platform-pending",
            frozen_ignored=False,
            current_ignored=False,
            frozen_platforms=foreign_platform,
            current_platforms=[foreign_platform],
        )

        result, report_path = self._run()

        self.assertEqual(result.returncode, 2, result.stderr)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertIn(
            "platform-pending exception frozen inventory row must have exact "
            "nonempty platform names",
            report["fatal_error"],
        )
        self.assertEqual(report["child_processes"], [])
        self.assertFalse(self.marker.exists())

    def test_platform_pending_exception_rejects_whitespace_current_platform_through_canonical_cli(
        self,
    ) -> None:
        foreign_platform = self._foreign_inventory_platform()
        self._write_baseline_exception_fixture(
            kind="platform-pending",
            frozen_ignored=False,
            current_ignored=False,
            frozen_platforms=[foreign_platform],
            current_platforms=[f" {foreign_platform} "],
        )

        result, report_path = self._run()

        self.assertEqual(result.returncode, 2, result.stderr)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertIn(
            "platform-pending exception current inventory row must have exact "
            "nonempty platform names",
            report["fatal_error"],
        )
        self.assertEqual(report["child_processes"], [])
        self.assertFalse(self.marker.exists())

    def test_platform_pending_accepts_exact_nonactive_platform_through_canonical_cli(
        self,
    ) -> None:
        foreign_platform = self._foreign_inventory_platform()
        self._write_baseline_exception_fixture(
            kind="platform-pending",
            frozen_ignored=False,
            current_ignored=False,
            frozen_platforms=[foreign_platform],
            current_platforms=[foreign_platform],
        )

        result, report_path = self._run()

        self.assertEqual(result.returncode, 0, result.stderr)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "confirmed_pass")
        self.assertEqual(
            report["exceptions"],
            [
                {
                    "baseline_id": "fixture-command::old-behavior",
                    "kind": "platform-pending",
                    "source": "completion-proof CLI fixture",
                    "text": "the fixture exception remains explicitly quarantined",
                }
            ],
        )
        self.assertEqual(
            len(self.marker.read_text(encoding="utf-8").splitlines()),
            1,
        )

    def test_live_service_exception_rejects_nonignored_current_row_through_focused_cli(
        self,
    ) -> None:
        self._write_rust_nextest_workspace_fixture()
        self.current_rows[0]["ignored"] = False
        self._write_json(
            self.current_inventory,
            {"schema_version": 1, "tests": self.current_rows},
        )
        self._append_baseline_exception_rule(
            baseline_id=RUST_NEXTEST_BASELINE_ID,
            kind="live-service",
        )

        result, report_path = self._run_focused("rust.nextest.workspace")

        self.assertEqual(result.returncode, 2, result.stderr)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertIn(
            "live-service exception discovered inventory row must have ignored=true",
            report["fatal_error"],
        )
        self.assertEqual(report["child_processes"], [])
        self.assertFalse(self.marker.exists())

    def test_platform_pending_exception_rejects_active_platform_through_focused_cli(
        self,
    ) -> None:
        self._write_rust_nextest_workspace_fixture()
        active_platform = platform.system().casefold()
        self.current_rows[0]["platforms"] = [active_platform.upper()]
        self._write_json(
            self.current_inventory,
            {"schema_version": 1, "tests": self.current_rows},
        )
        self._append_baseline_exception_rule(
            baseline_id=RUST_NEXTEST_BASELINE_ID,
            kind="platform-pending",
        )

        result, report_path = self._run_focused("rust.nextest.workspace")

        self.assertEqual(result.returncode, 2, result.stderr)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertIn(
            "platform-pending exception discovered inventory row includes active "
            f"platform {active_platform!r}",
            report["fatal_error"],
        )
        self.assertEqual(report["child_processes"], [])
        self.assertFalse(self.marker.exists())

    def test_focused_cli_records_one_fresh_non_certifying_pass(self) -> None:
        result, report_path = self._run_focused("fixture.command")
        self.assertEqual(result.returncode, 0, result.stderr)
        report = self._load_report(report_path)
        self.assertEqual(report["report_type"], "FocusedValidationAttemptReportV2")
        self._assert_v2_process_identities(report)
        self.assertEqual(
            report["exact_command"], "just completion-focused fixture.command"
        )
        self.assertEqual(report["focused_validation_id"], "fixture.command")
        self.assertEqual(report["attempt_classification"], "confirmed_pass")
        self.assertEqual(report["exceptions"], [])
        self.assertEqual(report["overrides"], [])
        self.assertEqual(len(report["validations"]), 1)
        validation = report["validations"][0]
        self.assertEqual(validation["id"], "fixture.command")
        self.assertEqual(validation["evidence_kind"], "typed_non_test")
        self.assertEqual(validation["validation_type"], "fixture-command")
        self.assertEqual(
            validation["input_contract_digest"],
            COMPLETION_PROOF._validation_input_contract_digest(
                {
                    "owned_paths": ["validator.py"],
                    "consumed_paths": ["validator.py"],
                }
            ),
        )
        self.assertEqual(validation["classification"], "confirmed_pass")
        self.assertEqual(validation["intended_ids"], ["fixture.command"])
        self.assertEqual(validation["selected_ids"], ["fixture.command"])
        self.assertEqual(validation["executed_ids"], ["fixture.command"])
        self.assertEqual(validation["intended_count"], 1)
        self.assertEqual(validation["selected_count"], 1)
        self.assertEqual(validation["executed_count"], 1)
        self.assertEqual(
            validation["outcomes"],
            [{"id": "fixture.command", "outcome": "passed"}],
        )
        self.assertEqual(len(report["child_processes"]), 1)
        child = report["child_processes"][0]
        self.assertEqual(child["validation_id"], "fixture.command")
        self.assertEqual(child["execution_id"], validation["execution_id"])
        self.assertGreater(child["pid"], 0)
        self.assertEqual(child["exit_code"], 0)
        self.assertNotEqual(report["report_type"], "CompletionProofAttemptReportV2")
        self.assertNotIn(
            "CompletionProofArtifactV1", report_path.read_text(encoding="utf-8")
        )

    def test_focused_cli_reports_declared_validation_failure(self) -> None:
        self._write_config(mode="fail")
        result, report_path = self._run_focused("fixture.command")
        self.assertEqual(result.returncode, 1)
        report = self._load_report(report_path)
        self.assertEqual(
            report["attempt_classification"], "confirmed_validation_failure"
        )
        validation = report["validations"][0]
        self.assertEqual(validation["classification"], "confirmed_validation_failure")
        self.assertEqual(validation["confirmed_failure_ids"], ["fixture.command"])
        self.assertEqual(validation["executed_ids"], ["fixture.command"])
        self.assertEqual(validation["executed_count"], 1)
        self.assertEqual(
            validation["outcomes"],
            [{"id": "fixture.command", "outcome": "failed"}],
        )
        self.assertEqual(report["child_processes"][0]["exit_code"], 1)

    def test_canonical_cli_rejects_untrusted_confirmed_child_evidence(self) -> None:
        mutations = {
            "unlaunched": "result.child.pid = 0",
            "fake-positive-pid": (
                "result.child.launch_target_identity = "
                "_unresolved_identity(result.child.executable)"
            ),
            "mismatched-validation": (
                "result.child.validation_id = 'fixture.other'"
            ),
            "mismatched-execution": (
                "result.child.execution_id = str(uuid.uuid4())"
            ),
        }
        for name, mutation in mutations.items():
            with self.subTest(name=name):
                patch_source = textwrap.dedent(
                    f"""\
                    _fixture_original_run_process = run_process

                    def _fixture_run_process(**kwargs):
                        result = _fixture_original_run_process(**kwargs)
                        if kwargs["validation_id"] == "fixture.command":
                            {mutation}
                        return result

                    run_process = _fixture_run_process
                    """
                )

                result, report_path = self._run(patch_source=patch_source)

                self.assertEqual(result.returncode, 2, result.stderr)
                report = self._load_report(report_path)
                self.assertEqual(
                    report["attempt_classification"],
                    "pre_result_error",
                )
                validation = next(
                    item
                    for item in report["validations"]
                    if item["id"] == "fixture.command"
                )
                self.assertEqual(validation["classification"], "pre_result_error")
                self.assertIn("child", validation["diagnostic"])

    def test_canonical_rust_gate_accepts_exact_nonzero_structured_pass(self) -> None:
        self._write_rust_gate_fixture(mode="pass")
        result, report_path = self._run()
        self.assertEqual(result.returncode, 0, result.stderr)
        report = self._load_report(report_path)
        self.assertEqual(report["report_type"], "CompletionProofAttemptReportV2")
        self.assertEqual(report["exact_command"], "just completion-proof")
        self.assertEqual(report["attempt_classification"], "confirmed_pass")
        self.assertEqual(len(report["validations"]), 2)
        validation = next(
            item
            for item in report["validations"]
            if item["id"] == RUST_GATE_VALIDATION_ID
        )
        self.assertEqual(validation["id"], RUST_GATE_VALIDATION_ID)
        self.assertEqual(validation["runner"], "rust-gate")
        self.assertEqual(validation["runner_selector"], RUST_GATE_NAME)
        self.assertEqual(validation["evidence_kind"], "structured_test")
        self.assertIsNone(validation["validation_type"])
        self.assertEqual(validation["classification"], "confirmed_pass")
        self.assertEqual(validation["intended_ids"], RUST_GATE_TEST_IDS)
        self.assertEqual(validation["selected_ids"], RUST_GATE_TEST_IDS)
        self.assertEqual(validation["executed_ids"], RUST_GATE_TEST_IDS)
        self.assertEqual(
            validation["outcomes"],
            [{"id": test_id, "outcome": "passed"} for test_id in RUST_GATE_TEST_IDS],
        )
        self.assertEqual(validation["intended_count"], 2)
        self.assertEqual(validation["selected_count"], 2)
        self.assertEqual(validation["executed_count"], 2)
        self.assertEqual(validation["confirmed_failure_ids"], [])
        launch = self._load_single_rust_gate_launch()
        self.assertEqual(launch["execution_id"], validation["execution_id"])
        self.assertEqual(len(report["child_processes"]), 2)
        child = next(
            item
            for item in report["child_processes"]
            if item["validation_id"] == RUST_GATE_VALIDATION_ID
        )
        self.assertEqual(child["validation_id"], RUST_GATE_VALIDATION_ID)
        self.assertEqual(child["execution_id"], validation["execution_id"])
        self.assertGreater(child["pid"], 0)
        self.assertEqual(child["exit_code"], 0)

    def test_focused_rust_gate_accepts_exact_nonzero_structured_pass(self) -> None:
        self._write_rust_gate_fixture(mode="pass")
        result, report_path = self._run_focused(RUST_GATE_VALIDATION_ID)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "confirmed_pass")
        self.assertEqual(report["focused_validation_id"], RUST_GATE_VALIDATION_ID)
        self.assertEqual(len(report["validations"]), 1)
        validation = report["validations"][0]
        self.assertEqual(validation["id"], RUST_GATE_VALIDATION_ID)
        self.assertEqual(validation["runner"], "rust-gate")
        self.assertEqual(validation["runner_selector"], RUST_GATE_NAME)
        self.assertEqual(validation["evidence_kind"], "structured_test")
        self.assertIsNone(validation["validation_type"])
        self.assertEqual(validation["classification"], "confirmed_pass")
        self.assertEqual(validation["intended_ids"], RUST_GATE_TEST_IDS)
        self.assertEqual(validation["selected_ids"], RUST_GATE_TEST_IDS)
        self.assertEqual(validation["executed_ids"], RUST_GATE_TEST_IDS)
        self.assertEqual(
            validation["outcomes"],
            [{"id": test_id, "outcome": "passed"} for test_id in RUST_GATE_TEST_IDS],
        )
        self.assertEqual(validation["intended_count"], 2)
        self.assertEqual(validation["selected_count"], 2)
        self.assertEqual(validation["executed_count"], 2)
        self.assertEqual(validation["confirmed_failure_ids"], [])
        launch = self._load_single_rust_gate_launch()
        self.assertEqual(launch["execution_id"], validation["execution_id"])
        self.assertEqual(len(report["child_processes"]), 1)
        child = report["child_processes"][0]
        self.assertEqual(child["validation_id"], RUST_GATE_VALIDATION_ID)
        self.assertEqual(child["execution_id"], validation["execution_id"])
        self.assertGreater(child["pid"], 0)
        self.assertEqual(child["exit_code"], 0)

    def test_windows_process_gate_requires_process_prerequisites(self) -> None:
        gate_name = "windows-process-coverage"
        self._write_rust_gate_fixture(
            mode="process-prerequisites",
            gate_name=gate_name,
        )

        result, report_path = self._run()

        self.assertEqual(result.returncode, 0, result.stderr)
        report = self._load_report(report_path)
        validation = next(
            item
            for item in report["validations"]
            if item["id"] == RUST_GATE_VALIDATION_ID
        )
        self.assertEqual(validation["runner_selector"], gate_name)
        self.assertEqual(validation["classification"], "confirmed_pass")
        launch = json.loads(self.marker.read_text(encoding="utf-8").splitlines()[0])
        self.assertEqual(launch["gate"], gate_name)
        self.assertEqual(
            launch["windows_process_prerequisites_required"],
            "1",
        )

        self.marker.unlink()
        patch_source = textwrap.dedent(
            """\
            _fixture_original_run_process = run_process

            def _fixture_run_process(**kwargs):
                if kwargs["validation_id"] == "fixture.rust-gate":
                    stripped = dict(kwargs["env"])
                    stripped.pop("CODEX_REQUIRE_WINDOWS_SANDBOX_PROCESS_TESTS", None)
                    kwargs["env"] = stripped
                return _fixture_original_run_process(**kwargs)

            run_process = _fixture_run_process
            """
        )
        result, report_path = self._run(patch_source=patch_source)

        self.assertEqual(result.returncode, 2)
        report = self._load_report(report_path)
        validation = next(
            item
            for item in report["validations"]
            if item["id"] == RUST_GATE_VALIDATION_ID
        )
        self.assertEqual(validation["classification"], "pre_result_error")
        self.assertEqual(validation["executed_count"], 0)
        self.assertIn(
            "Windows sandbox process prerequisites were not required",
            validation["diagnostic"],
        )

    def test_focused_rust_gate_rejects_success_without_terminal_evidence(self) -> None:
        self._write_rust_gate_fixture(mode="missing-evidence")
        result, report_path = self._run_focused(RUST_GATE_VALIDATION_ID)
        self.assertEqual(result.returncode, 2)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        validation = report["validations"][0]
        self.assertEqual(validation["id"], RUST_GATE_VALIDATION_ID)
        self.assertEqual(validation["classification"], "pre_result_error")
        self.assertEqual(validation["intended_ids"], RUST_GATE_TEST_IDS)
        self.assertEqual(validation["selected_count"], 0)
        self.assertEqual(validation["executed_count"], 0)
        self.assertEqual(validation["outcomes"], [])
        self.assertEqual(validation["confirmed_failure_ids"], [])
        self.assertIn(
            "runner exited successfully without terminal events",
            validation["diagnostic"],
        )
        self.assertIn(
            "named Rust gate report did not prove an exact fresh execution",
            validation["diagnostic"],
        )
        launch = self._load_single_rust_gate_launch()
        self.assertEqual(launch["execution_id"], validation["execution_id"])
        self.assertEqual(report["child_processes"][0]["exit_code"], 0)

    def test_focused_rust_gate_preserves_failure_before_later_infrastructure_error(
        self,
    ) -> None:
        self._write_rust_gate_fixture(mode="failure-then-infrastructure")
        result, report_path = self._run_focused(RUST_GATE_VALIDATION_ID)
        self.assertEqual(result.returncode, 1)
        report = self._load_report(report_path)
        self.assertEqual(
            report["attempt_classification"], "confirmed_validation_failure"
        )
        validation = report["validations"][0]
        self.assertEqual(validation["id"], RUST_GATE_VALIDATION_ID)
        self.assertEqual(validation["classification"], "confirmed_validation_failure")
        self.assertEqual(validation["intended_ids"], RUST_GATE_TEST_IDS)
        self.assertEqual(validation["selected_ids"], RUST_GATE_TEST_IDS)
        self.assertEqual(validation["executed_ids"], [RUST_GATE_TEST_IDS[0]])
        self.assertEqual(
            validation["outcomes"],
            [{"id": RUST_GATE_TEST_IDS[0], "outcome": "failed"}],
        )
        self.assertEqual(validation["confirmed_failure_ids"], [RUST_GATE_TEST_IDS[0]])
        self.assertIn(
            "later gate step ended in an infrastructure error",
            validation["diagnostic"],
        )
        launch = self._load_single_rust_gate_launch()
        self.assertEqual(launch["execution_id"], validation["execution_id"])
        self.assertEqual(report["child_processes"][0]["exit_code"], 100)

    def test_focused_rust_gate_preserves_failure_before_supervision_error(
        self,
    ) -> None:
        self._write_rust_gate_fixture(mode="failure-then-infrastructure")
        patch_source = textwrap.dedent(
            """\
            _original_run_process = run_process
            def _run_process_with_later_supervision_error(**kwargs):
                result = _original_run_process(**kwargs)
                return ProcessResult(
                    returncode=None,
                    stdout=result.stdout,
                    stderr=result.stderr,
                    child=result.child,
                    invocation_error="runner output drain failed after terminal evidence",
                )
            run_process = _run_process_with_later_supervision_error
            """
        )
        result, report_path = self._run_focused(
            RUST_GATE_VALIDATION_ID,
            patch_source=patch_source,
        )

        self.assertEqual(result.returncode, 1)
        report = self._load_report(report_path)
        validation = report["validations"][0]
        self.assertEqual(validation["classification"], "confirmed_validation_failure")
        self.assertEqual(validation["executed_ids"], [RUST_GATE_TEST_IDS[0]])
        self.assertEqual(validation["confirmed_failure_ids"], [RUST_GATE_TEST_IDS[0]])
        self.assertIn("output drain failed", validation["diagnostic"])

    def test_focused_rust_gate_never_accepts_pass_with_supervision_error(self) -> None:
        self._write_rust_gate_fixture(mode="pass")
        patch_source = textwrap.dedent(
            """\
            _original_run_process = run_process
            def _run_process_with_later_supervision_error(**kwargs):
                result = _original_run_process(**kwargs)
                return ProcessResult(
                    returncode=None,
                    stdout=result.stdout,
                    stderr=result.stderr,
                    child=result.child,
                    invocation_error="runner output drain failed after apparent success",
                )
            run_process = _run_process_with_later_supervision_error
            """
        )
        result, report_path = self._run_focused(
            RUST_GATE_VALIDATION_ID,
            patch_source=patch_source,
        )

        self.assertEqual(result.returncode, 2)
        report = self._load_report(report_path)
        validation = report["validations"][0]
        self.assertEqual(validation["classification"], "pre_result_error")
        self.assertEqual(validation["executed_count"], 0)
        self.assertIn("output drain failed", validation["diagnostic"])

    def test_git_supervision_errors_use_five_second_budget_and_fail_closed(
        self,
    ) -> None:
        supervision_errors = (
            "runner timed out after 5 seconds",
            "cannot launch runner: fixture",
            "runner stdout exceeded the 134217728-byte output limit",
            "cannot terminate runner Job Object: fixture",
        )
        for supervision_error in supervision_errors:
            with (
                self.subTest(supervision_error=supervision_error),
                mock.patch.object(
                    COMPLETION_PROOF,
                    "run_bounded_process",
                    return_value=(
                        COMPLETION_PROOF._bounded_process_module.BoundedProcessResult(
                            returncode=None,
                            stdout=b"",
                            stderr=b"",
                            pid=17,
                            supervision_error=supervision_error,
                        )
                    ),
                ) as run,
                self.assertRaises(COMPLETION_PROOF.ProofError) as raised,
            ):
                COMPLETION_PROOF._run_bounded_git_process(
                    self.repository,
                    ["status", "--porcelain=v2"],
                    env=os.environ,
                )
            self.assertIn("infrastructure error", str(raised.exception))
            self.assertIn(supervision_error, str(raised.exception))
            self.assertEqual(run.call_args.kwargs["timeout_seconds"], 5.0)
            self.assertEqual(
                run.call_args.kwargs["stdout_limit_bytes"],
                128 * 1024 * 1024,
            )
            self.assertEqual(
                run.call_args.kwargs["stderr_limit_bytes"],
                32 * 1024 * 1024,
            )

    def test_workspace_fingerprint_hanging_fake_git_is_bounded_and_cleans_descendant(
        self,
    ) -> None:
        child_pid_file = self.base / "head-git-child.pid"
        env = self._fake_git_environment(
            hang_match="rev-parse --verify HEAD",
            child_pid_file=child_pid_file,
        )

        started = time.monotonic()
        result = self._run_fake_git_cli("fingerprint", env=env)

        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertLess(time.monotonic() - started, 10)
        self.assertIn("infrastructure error", result.stderr)
        self.assertIn("timed out after 0.35 seconds", result.stderr)
        self.assertTrue(child_pid_file.is_file())
        _wait_for_pid_exit(self, int(child_pid_file.read_text(encoding="utf-8")))

    def test_git_helper_hanging_fake_git_is_bounded_and_preserves_failure(
        self,
    ) -> None:
        self._write_config(mode="fail")
        child_pid_file = self.base / "status-git-child.pid"
        env = self._fake_git_environment(
            hang_match="status --porcelain=v2",
            child_pid_file=child_pid_file,
            hang_trigger=self.marker,
        )
        fingerprint = self._run_fake_git_cli("fingerprint", env=env)
        self.assertEqual(fingerprint.returncode, 0, fingerprint.stderr)
        report_path = self.base / "git-timeout-after-failure-report.json"
        command, runtime_env = self._canonical_invocation(
            report_path,
            patch_source="GIT_PROCESS_TIMEOUT_SECONDS = 0.35",
            start_fingerprint=fingerprint.stdout.strip(),
        )
        for name in (
            "PATH",
            "KD4_FAKE_GIT_HANG_MATCH",
            "KD4_FAKE_GIT_HANG_TRIGGER",
            "KD4_FAKE_GIT_CHILD_PID",
        ):
            runtime_env[name] = env[name]

        started = time.monotonic()
        result = subprocess.run(
            command,
            cwd=self.repository,
            env=runtime_env,
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            timeout=15,
            check=False,
        )

        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertLess(time.monotonic() - started, 10)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        failure = next(
            item for item in report["validations"] if item["id"] == "fixture.command"
        )
        self.assertEqual(failure["classification"], "confirmed_validation_failure")
        self.assertEqual(failure["executed_ids"], ["fixture.command"])
        self.assertEqual(failure["confirmed_failure_ids"], ["fixture.command"])
        self.assertIn("timed out after 0.35 seconds", report["fatal_error"])
        self.assertTrue(child_pid_file.is_file())
        _wait_for_pid_exit(self, int(child_pid_file.read_text(encoding="utf-8")))

    def test_test_surface_git_process_hanging_fake_git_is_bounded_and_cleans_descendant(
        self,
    ) -> None:
        (self.repository / "test_ignored_marker.py").write_text(
            "def test_ignored_marker():\n    pass\n",
            encoding="utf-8",
        )
        child_pid_file = self.base / "surface-git-child.pid"
        env = self._fake_git_environment(
            hang_match="rev-parse --verify HEAD^{tree}",
            child_pid_file=child_pid_file,
            report_ignored=True,
        )

        started = time.monotonic()
        result = self._run_fake_git_cli("inventory-check", env=env)

        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertLess(time.monotonic() - started, 10)
        self.assertIn("infrastructure error", result.stderr)
        self.assertIn("timed out after 0.35 seconds", result.stderr)
        self.assertTrue(child_pid_file.is_file())
        _wait_for_pid_exit(self, int(child_pid_file.read_text(encoding="utf-8")))

    def test_focused_timeout_is_pre_result_and_kills_validation_tree(self) -> None:
        pid_file = self.base / "focused-timeout-child.pid"
        self.validator.write_text(
            textwrap.dedent(
                f"""\
                import pathlib
                import subprocess
                import sys
                import time

                child = subprocess.Popen(
                    [sys.executable, "-c", "import time; time.sleep(30)"]
                )
                pathlib.Path({str(pid_file)!r}).write_text(str(child.pid))
                time.sleep(30)
                """
            ),
            encoding="utf-8",
        )
        before, separator, after = self.config.read_text(encoding="utf-8").rpartition(
            "timeout_seconds = 30"
        )
        self.assertTrue(separator)
        self.config.write_text(
            before + "timeout_seconds = 1" + after,
            encoding="utf-8",
        )

        result, report_path = self._run_focused("fixture.command")

        self.assertEqual(result.returncode, 2, result.stderr)
        report = self._load_report(report_path)
        validation = report["validations"][0]
        self.assertEqual(validation["classification"], "pre_result_error")
        self.assertEqual(validation["executed_count"], 0)
        self.assertIn("timed out", validation["diagnostic"])
        self.assertIsNone(report["child_processes"][0]["exit_code"])
        _wait_for_pid_exit(self, int(pid_file.read_text(encoding="utf-8")))

    def test_focused_output_overflow_is_pre_result_and_kills_validation_tree(
        self,
    ) -> None:
        pid_file = self.base / "focused-overflow-child.pid"
        self.validator.write_text(
            textwrap.dedent(
                f"""\
                import pathlib
                import subprocess
                import sys
                import time

                child = subprocess.Popen(
                    [sys.executable, "-c", "import time; time.sleep(30)"]
                )
                pathlib.Path({str(pid_file)!r}).write_text(str(child.pid))
                sys.stdout.buffer.write(b"x" * 8192)
                sys.stdout.buffer.flush()
                time.sleep(30)
                """
            ),
            encoding="utf-8",
        )

        result, report_path = self._run_focused(
            "fixture.command",
            patch_source="PROCESS_STDOUT_LIMIT_BYTES = 1024",
        )

        self.assertEqual(result.returncode, 2, result.stderr)
        report = self._load_report(report_path)
        validation = report["validations"][0]
        self.assertEqual(validation["classification"], "pre_result_error")
        self.assertEqual(validation["executed_count"], 0)
        self.assertIn("stdout exceeded", validation["diagnostic"])
        self.assertIsNone(report["child_processes"][0]["exit_code"])
        _wait_for_pid_exit(self, int(pid_file.read_text(encoding="utf-8")))

    def test_canonical_validation_sweeps_descendant_holding_output_pipes(
        self,
    ) -> None:
        pid_file = self.base / "canonical-descendant.pid"
        self.validator.write_text(
            textwrap.dedent(
                f"""\
                import pathlib
                import subprocess
                import sys
                import uuid

                child = subprocess.Popen(
                    [sys.executable, "-c", "import time; time.sleep(30)"]
                )
                pathlib.Path({str(pid_file)!r}).write_text(str(child.pid))
                marker = pathlib.Path(sys.argv[1])
                with marker.open("a", encoding="utf-8") as output:
                    output.write(str(uuid.uuid4()) + " []\\n")
                """
            ),
            encoding="utf-8",
        )
        started = time.monotonic()

        result, report_path = self._run()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLess(time.monotonic() - started, 8)
        report = self._load_report(report_path)
        validation = next(
            item for item in report["validations"] if item["id"] == "fixture.command"
        )
        self.assertEqual(validation["classification"], "confirmed_pass")
        self.assertEqual(validation["executed_count"], 1)
        _wait_for_pid_exit(self, int(pid_file.read_text(encoding="utf-8")))

    def test_attestation_has_a_local_deadline_and_fixed_response_cap(self) -> None:
        identity = mock.Mock()
        identity.pid = os.getpid()
        identity.entrypoint.resolved_path = str(RUNNER)
        runtime = {
            "CODEX_COMPLETION_PROOF_RUNNER_ATTESTATION_ENDPOINT": "fixture-endpoint",
            "CODEX_COMPLETION_PROOF_ATTEMPT_ID": str(uuid.uuid4()),
            "CODEX_COMPLETION_PROOF_NONCE": "n" * 64,
        }

        class BlockingChannel:
            def __init__(self) -> None:
                self.closed = threading.Event()

            def __enter__(self):
                return self

            def __exit__(self, *_args):
                self.close()

            def write(self, _payload: bytes) -> None:
                return None

            def readline(self, _limit: int) -> bytes:
                self.closed.wait(10)
                return b""

            def close(self) -> None:
                self.closed.set()

        fake_os = mock.Mock()
        fake_os.name = "nt"
        started = time.monotonic()
        with (
            mock.patch.object(COMPLETION_PROOF, "os", fake_os),
            mock.patch("builtins.open", return_value=BlockingChannel()),
            mock.patch.object(
                COMPLETION_PROOF, "RUNNER_ATTESTATION_TIMEOUT_SECONDS", 0.1
            ),
            self.assertRaisesRegex(COMPLETION_PROOF.ProofError, "timed out"),
        ):
            COMPLETION_PROOF._attest_runner_process(runtime, identity)
        self.assertLess(time.monotonic() - started, 1)

        oversized = mock.MagicMock()
        oversized.__enter__.return_value = oversized
        oversized.readline.return_value = b"x" * 17
        with (
            mock.patch.object(COMPLETION_PROOF, "os", fake_os),
            mock.patch("builtins.open", return_value=oversized),
            self.assertRaisesRegex(COMPLETION_PROOF.ProofError, "exceeded its limit"),
        ):
            COMPLETION_PROOF._attest_runner_process(runtime, identity)

    def test_focused_cli_reports_untrusted_runner_exit_as_pre_result(self) -> None:
        self._write_config(
            mode="fail",
            validation_failure_exit_codes=[],
        )
        result, report_path = self._run_focused("fixture.command")
        self.assertEqual(result.returncode, 2)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        validation = report["validations"][0]
        self.assertEqual(validation["classification"], "pre_result_error")
        self.assertEqual(validation["executed_count"], 0)
        self.assertEqual(validation["confirmed_failure_ids"], [])
        self.assertEqual(report["child_processes"][0]["exit_code"], 1)

    def test_focused_cli_rejects_synthetic_typed_test_ids_without_execution(
        self,
    ) -> None:
        config_text = self.config.read_text(encoding="utf-8")
        self.config.write_text(
            config_text.replace(
                'validation_type = "fixture-command"',
                'validation_type = "fixture-command"\nintended_ids = ["forged.test"]',
            ),
            encoding="utf-8",
        )
        result, report_path = self._run_focused("fixture.command")
        self.assertEqual(result.returncode, 2)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertEqual(len(report["validations"]), 1)
        validation = report["validations"][0]
        self.assertEqual(validation["id"], "fixture.command")
        self.assertEqual(validation["classification"], "pre_result_error")
        self.assertEqual(validation["evidence_kind"], "infrastructure")
        self.assertEqual(report["child_processes"], [])
        self.assertIn("cannot declare synthetic intended_ids", report["fatal_error"])
        self.assertFalse(self.marker.exists())

    def test_focused_cli_rejects_unknown_exact_id_without_execution(self) -> None:
        result, report_path = self._run_focused("fixture.unknown")
        self.assertEqual(result.returncode, 2)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertEqual(report["focused_validation_id"], "fixture.unknown")
        self.assertEqual(len(report["validations"]), 1)
        validation = report["validations"][0]
        self.assertEqual(validation["id"], "fixture.unknown")
        self.assertEqual(validation["classification"], "pre_result_error")
        self.assertEqual(validation["executed_count"], 0)
        self.assertEqual(report["child_processes"], [])
        self.assertIn("unknown focused validation ID", report["fatal_error"])
        self.assertFalse(self.marker.exists())

    def test_focused_cli_rejects_configured_frozen_hash_mismatch(self) -> None:
        config_text = self.config.read_text(encoding="utf-8")
        self.config.write_text(
            config_text.replace(
                _inventory_hash(self.baseline_rows),
                "0" * 64,
                1,
            ),
            encoding="utf-8",
        )
        result, report_path = self._run_focused("fixture.command")
        self.assertEqual(result.returncode, 2)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertEqual(report["child_processes"], [])
        self.assertIn(
            "configured frozen_inventory_hash does not match",
            report["fatal_error"],
        )
        self.assertFalse(self.marker.exists())

    def test_executed_failure_is_confirmed_and_reported_through_canonical_cli(
        self,
    ) -> None:
        self._write_config(mode="fail")
        result, report_path = self._run()
        self.assertEqual(result.returncode, 1)
        report = self._load_report(report_path)
        self.assertEqual(
            report["attempt_classification"], "confirmed_validation_failure"
        )
        validation = next(
            item for item in report["validations"] if item["id"] == "fixture.command"
        )
        self.assertEqual(validation["classification"], "confirmed_validation_failure")
        self.assertEqual(validation["evidence_kind"], "typed_non_test")
        self.assertEqual(validation["validation_type"], "fixture-command")
        self.assertEqual(validation["confirmed_failure_ids"], ["fixture.command"])
        self.assertEqual(validation["executed_ids"], ["fixture.command"])
        self.assertEqual(validation["executed_count"], 1)

    def test_untrusted_nonzero_exit_is_a_pre_result_error(self) -> None:
        self._write_config(
            mode="fail",
            validation_failure_exit_codes=[],
        )
        result, report_path = self._run()
        self.assertEqual(result.returncode, 2)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        validation = next(
            item for item in report["validations"] if item["id"] == "fixture.command"
        )
        self.assertEqual(validation["classification"], "pre_result_error")
        self.assertEqual(validation["executed_count"], 0)
        self.assertEqual(validation["confirmed_failure_ids"], [])

    def test_existing_report_is_rejected_without_reusing_cached_content_through_canonical_cli(
        self,
    ) -> None:
        report_path = self.base / "copied-report.json"
        copied = '{"attempt_classification":"confirmed_pass"}\n'
        report_path.write_text(copied, encoding="utf-8")
        result, _ = self._run(report=report_path)
        self.assertEqual(result.returncode, 2)
        self.assertIn("copied/cached output is rejected", result.stderr)
        self.assertEqual(report_path.read_text(encoding="utf-8"), copied)
        self.assertFalse(self.marker.exists())

    def _assert_index_pre_result(
        self,
        result: subprocess.CompletedProcess[str],
        report_path: Path,
        expected_error: str,
    ) -> None:
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertNotIn("COMPLETION PROOF PASSED", result.stdout)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertIn(expected_error, report["fatal_error"])
        self.assertEqual(report["child_processes"], [])
        self.assertFalse(self.marker.exists())

    def test_canonical_rejects_skip_worktree_before_any_validation_child(self) -> None:
        start_fingerprint = self._fingerprint()
        self._git("update-index", "--skip-worktree", "tracked-input.txt")
        self.deletable_input.write_text("hidden mutation\n", encoding="utf-8")

        result, report_path = self._run(start_fingerprint=start_fingerprint)

        self._assert_index_pre_result(
            result,
            report_path,
            "invisible or unsupported entry tag 'S': tracked-input.txt",
        )

    def test_canonical_rejects_assume_unchanged_before_any_validation_child(
        self,
    ) -> None:
        start_fingerprint = self._fingerprint()
        self._git("update-index", "--assume-unchanged", "tracked-input.txt")
        self.deletable_input.write_text("hidden mutation\n", encoding="utf-8")

        result, report_path = self._run(start_fingerprint=start_fingerprint)

        self._assert_index_pre_result(
            result,
            report_path,
            "invisible or unsupported entry tag 'h': tracked-input.txt",
        )

    def test_canonical_rejects_true_sparse_index_before_any_validation_child(
        self,
    ) -> None:
        excluded = self.repository / "excluded"
        excluded.mkdir()
        (excluded / "rogue_test.py").write_text(
            "def test_hidden():\n    assert True\n",
            encoding="utf-8",
        )
        self._git("add", "excluded/rogue_test.py")
        self._git("commit", "--quiet", "-m", "add excluded test surface")
        start_fingerprint = self._fingerprint()
        self._git("sparse-checkout", "init", "--cone", "--sparse-index")
        self._git("sparse-checkout", "set", ".codex")

        result, report_path = self._run(start_fingerprint=start_fingerprint)

        self._assert_index_pre_result(
            result,
            report_path,
            "invisible or unsupported entry tag 'S': excluded/rogue_test.py",
        )

    def test_canonical_rejects_non_test_shaped_gitlink_before_any_validation_child(
        self,
    ) -> None:
        start_fingerprint = self._fingerprint()
        commit_id = self._git("rev-parse", "HEAD").stdout.strip()
        self._git(
            "update-index",
            "--add",
            "--cacheinfo",
            f"160000,{commit_id},vendor",
        )

        result, report_path = self._run(start_fingerprint=start_fingerprint)

        self._assert_index_pre_result(
            result,
            report_path,
            "cached index listing contains a gitlink: vendor",
        )

    def test_canonical_rejects_absent_test_shaped_symlink_before_any_validation_child(
        self,
    ) -> None:
        start_fingerprint = self._fingerprint()
        blob_source = self.base / "symlink-index-blob"
        blob_source.write_text("validator.py\n", encoding="utf-8")
        blob_id = self._git("hash-object", "-w", str(blob_source)).stdout.strip()
        self._git(
            "update-index",
            "--add",
            "--cacheinfo",
            f"120000,{blob_id},missing_test.py",
        )
        self.assertFalse((self.repository / "missing_test.py").exists())

        result, report_path = self._run(start_fingerprint=start_fingerprint)

        self._assert_index_pre_result(
            result,
            report_path,
            "tracked test-system marker is a symlink: missing_test.py",
        )

    def test_canonical_preserves_non_test_symlink_and_missing_regular_paths(
        self,
    ) -> None:
        blob_source = self.base / "ordinary-symlink-index-blob"
        blob_source.write_text("docs/target.md\n", encoding="utf-8")
        blob_id = self._git("hash-object", "-w", str(blob_source)).stdout.strip()
        self._git(
            "update-index",
            "--add",
            "--cacheinfo",
            f"120000,{blob_id},docs-current",
        )
        self.deletable_input.unlink()
        self.assertFalse((self.repository / "docs-current").exists())

        result, report_path = self._run()

        self.assertEqual(result.returncode, 0, result.stderr)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "confirmed_pass")
        self.assertTrue(self.marker.exists())

    def test_inventory_check_rejects_untracked_unknown_test_system(self) -> None:
        (self.repository / "rogue_test.go").write_text(
            "package rogue\n",
            encoding="utf-8",
        )
        result = subprocess.run(
            self._unittest_runner_command(
                "--config",
                str(self.config),
                "inventory-check",
            ),
            cwd=self.repository,
            env=self._base_env(),
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("test-system surface audit", result.stderr)
        self.assertIn("rogue_test.go", result.stderr)
        self.assertFalse(self.marker.exists())

    def test_inventory_check_rejects_test_marker_hidden_by_untracked_gitignore(
        self,
    ) -> None:
        (self.repository / ".gitignore").write_text(
            "rogue_test.go\n",
            encoding="utf-8",
        )
        (self.repository / "rogue_test.go").write_text(
            "package rogue\n",
            encoding="utf-8",
        )

        result = subprocess.run(
            self._unittest_runner_command(
                "--config",
                str(self.config),
                "inventory-check",
            ),
            cwd=self.repository,
            env=self._base_env(),
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
        )

        self.assertEqual(result.returncode, 2)
        self.assertIn("test-system surface audit", result.stderr)
        self.assertIn("rogue_test.go", result.stderr)
        self.assertFalse(self.marker.exists())

    def test_canonical_rejects_test_marker_hidden_by_broadened_tracked_gitignore(
        self,
    ) -> None:
        gitignore = self.repository / ".gitignore"
        gitignore.write_text("# committed baseline\n", encoding="utf-8")
        self._git("add", ".gitignore")
        self._git("commit", "--quiet", "-m", "track baseline ignore rules")
        gitignore.write_text(
            "# committed baseline\nrogue_test.go\n",
            encoding="utf-8",
        )
        (self.repository / "rogue_test.go").write_text(
            "package rogue\n",
            encoding="utf-8",
        )

        result, report_path = self._run()

        self.assertEqual(result.returncode, 2)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertIn("test-system surface audit", report["fatal_error"])
        self.assertIn("rogue_test.go", report["fatal_error"])
        self.assertEqual(report["child_processes"], [])
        self.assertFalse(self.marker.exists())

    def test_head_owned_ignored_test_marker_allows_canonical_runtime_path(
        self,
    ) -> None:
        self._git("config", "core.ignoreCase", "true")
        (self.repository / ".gitignore").write_text(
            "ROGUE_TEST.GO\n",
            encoding="utf-8",
        )
        self._git("add", ".gitignore")
        self._git("commit", "--quiet", "-m", "own ignored fixture marker")
        (self.repository / "rogue_test.go").write_text(
            "package rogue\n",
            encoding="utf-8",
        )

        result, report_path = self._run()

        self.assertEqual(result.returncode, 0, result.stderr)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "confirmed_pass")
        self.assertEqual(
            len(self.marker.read_text(encoding="utf-8").splitlines()),
            1,
        )

    def test_canonical_testing_inventory_cannot_bypass_test_system_surface_audit(
        self,
    ) -> None:
        (self.repository / "rogue_test.go").write_text(
            "package rogue\n",
            encoding="utf-8",
        )
        result, report_path = self._run()
        self.assertEqual(result.returncode, 2)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertIn("test-system surface audit", report["fatal_error"])
        self.assertIn("rogue_test.go", report["fatal_error"])
        self.assertEqual(report["child_processes"], [])
        self.assertFalse(self.marker.exists())

    def test_canonical_accepts_current_trusted_command_surfaces_and_runs_child(
        self,
    ) -> None:
        self._install_current_trusted_command_surfaces()

        result, report_path = self._run()

        self.assertEqual(result.returncode, 0, result.stderr)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "confirmed_pass")
        self.assertEqual(
            [child["validation_id"] for child in report["child_processes"]],
            ["inventory.frozen-reconciliation", "fixture.command"],
        )
        self.assertTrue(all(child["pid"] > 0 for child in report["child_processes"]))
        self.assertEqual(
            len(self.marker.read_text(encoding="utf-8").splitlines()),
            1,
        )

    def test_canonical_rejects_dependency_only_unsupported_framework_before_discovery(
        self,
    ) -> None:
        self._install_current_trusted_command_surfaces()
        package_path = self.repository / "package.json"
        package = json.loads(package_path.read_text(encoding="utf-8"))
        package.setdefault("devDependencies", {})["mocha"] = "1.0.0"
        self._write_json(package_path, package)

        result, report_path = self._run()

        self.assertEqual(result.returncode, 2)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertIn(
            "package.json [javascript-runner-dependency:mocha]",
            report["fatal_error"],
        )
        self.assertEqual(report["child_processes"], [])
        self.assertFalse(self.marker.exists())

    def test_canonical_rejects_added_just_test_recipe_before_child(self) -> None:
        self._install_current_trusted_command_surfaces()
        with (self.repository / "justfile").open("ab") as output:
            output.write(b"\nrogue-suite:\n    node --test rogue-suite.js\n")

        result, report_path = self._run()

        self.assertEqual(result.returncode, 2)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertIn(
            "justfile [just-command-manifest:lf-normalized-utf8-v1]",
            report["fatal_error"],
        )
        self.assertEqual(report["child_processes"], [])
        self.assertFalse(self.marker.exists())

    def test_canonical_rejects_added_package_test_script_before_child(self) -> None:
        self._install_current_trusted_command_surfaces()
        package_path = self.repository / "package.json"
        package = json.loads(package_path.read_text(encoding="utf-8"))
        package["scripts"]["ci:test"] = "node --test rogue-suite.js"
        self._write_json(package_path, package)

        result, report_path = self._run()

        self.assertEqual(result.returncode, 2)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertIn(
            "package.json [package-script-command:ci:test]",
            report["fatal_error"],
        )
        self.assertEqual(report["child_processes"], [])
        self.assertFalse(self.marker.exists())

    def test_canonical_rejects_changed_known_package_script_before_child(self) -> None:
        self._install_current_trusted_command_surfaces()
        package_path = self.repository / "package.json"
        package = json.loads(package_path.read_text(encoding="utf-8"))
        package["scripts"]["audit:scripts"] = "node --test rogue-suite.js"
        self._write_json(package_path, package)

        result, report_path = self._run()

        self.assertEqual(result.returncode, 2)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertIn(
            "package.json [package-script-command:audit:scripts]",
            report["fatal_error"],
        )
        self.assertEqual(report["child_processes"], [])
        self.assertFalse(self.marker.exists())

    def test_focused_reconciliation_testing_inventory_cannot_bypass_test_system_surface_audit(
        self,
    ) -> None:
        (self.repository / "rogue_test.go").write_text(
            "package rogue\n",
            encoding="utf-8",
        )
        result, report_path = self._run_focused("inventory.frozen-reconciliation")
        self.assertEqual(result.returncode, 2)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertIn("test-system surface audit", report["fatal_error"])
        self.assertIn("rogue_test.go", report["fatal_error"])
        self.assertEqual(report["child_processes"], [])
        self.assertFalse(self.marker.exists())

    def test_normal_focused_validation_does_not_run_repository_surface_audit(
        self,
    ) -> None:
        (self.repository / "rogue_test.go").write_text(
            "package rogue\n",
            encoding="utf-8",
        )
        result, report_path = self._run_focused("fixture.command")
        self.assertEqual(result.returncode, 0, result.stderr)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "confirmed_pass")
        self.assertEqual(
            len(self.marker.read_text(encoding="utf-8").splitlines()),
            1,
        )

    def test_unmapped_current_inventory_blocks_before_validation_through_canonical_cli(
        self,
    ) -> None:
        changed = [dict(self.current_rows[0], baseline_id="fixture-command::unmapped")]
        self._write_json(
            self.current_inventory,
            {"schema_version": 1, "tests": changed},
        )
        result, report_path = self._run()
        self.assertEqual(result.returncode, 2)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertIn("inventory reconciliation failed", report["fatal_error"])
        self.assertFalse(self.marker.exists())

    def test_replacement_cannot_reuse_another_frozen_baseline_identity(self) -> None:
        replacement_id = "fixture-command::still-baseline-behavior"
        remaining_baseline = {
            "baseline_id": replacement_id,
            "framework": "fixture-command",
            "native_id": "still-baseline-behavior",
            "source": "validator.py",
            "ignored": False,
            "platforms": [platform.system().casefold()],
        }
        self.baseline_rows = [*self.baseline_rows, remaining_baseline]
        self.current_rows = [remaining_baseline]
        digest = _inventory_hash(self.baseline_rows)
        self._write_json(
            self.frozen_inventory,
            {
                "schema_version": 1,
                "baseline_commit": "fixture",
                "baseline_workspace_fingerprint": "fixture",
                "host_platform": platform.system().casefold(),
                "inventory_hash": digest,
                "tests": self.baseline_rows,
            },
        )
        self._write_json(
            self.ledger,
            {
                "schema_version": 1,
                "frozen_inventory_hash": digest,
                "rows": [
                    {
                        "baseline_id": "fixture-command::old-behavior",
                        "resolution": "replacement",
                        "replacement_ids": [replacement_id],
                        "preserved_behavior": "the old fixture behavior remains covered",
                        "product_path": "validator.py CLI",
                        "validation_id": "fixture.command",
                    },
                    {
                        "baseline_id": replacement_id,
                        "resolution": "exception",
                        "provenance": {
                            "kind": "protected",
                            "source": "fixture repository instruction",
                            "text": "The remaining baseline fixture is protected.",
                        },
                    },
                ],
                "overrides": [],
            },
        )
        self._write_json(
            self.current_inventory,
            {"schema_version": 1, "tests": self.current_rows},
        )
        self._write_config(mode="pass")

        result, report_path = self._run()

        self.assertEqual(result.returncode, 2)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertIn(
            "replacement IDs must be disjoint from frozen baseline IDs",
            report["fatal_error"],
        )
        self.assertIn(replacement_id, report["fatal_error"])
        self.assertEqual(report["child_processes"], [])
        self.assertFalse(self.marker.exists())

    def test_explicit_post_freeze_addition_executes_through_canonical_cli(self) -> None:
        addition_id = "fixture-command::completion-gate-runtime-path"
        current_rows = [
            *self.current_rows,
            {
                "baseline_id": addition_id,
                "framework": "fixture-command",
                "native_id": "completion-gate-runtime-path",
                "source": "validator.py",
                "ignored": False,
                "platforms": [platform.system().casefold()],
            },
        ]
        self._write_json(
            self.current_inventory,
            {"schema_version": 1, "tests": current_rows},
        )
        ledger = json.loads(self.ledger.read_text(encoding="utf-8"))
        ledger["additions"] = [
            {
                "test_id": addition_id,
                "preserved_behavior": (
                    "the post-freeze completion policy remains covered by a real CLI run"
                ),
                "product_path": "completion_proof.py canonical CLI",
                "validation_id": "fixture.command",
                "provenance": {
                    "kind": "policy-addition",
                    "source": "locked completion-proof plan",
                    "text": "This test was added after the immutable baseline was frozen.",
                },
            }
        ]
        self._write_json(self.ledger, ledger)

        result, report_path = self._run()

        self.assertEqual(result.returncode, 0, result.stderr)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "confirmed_pass")
        inventory = next(
            item
            for item in report["validations"]
            if item["id"] == "inventory.frozen-reconciliation"
        )
        expected = sorted(["fixture-command::new-runtime-path", addition_id])
        self.assertEqual(inventory["intended_ids"], expected)
        self.assertEqual(inventory["selected_ids"], expected)
        self.assertEqual(inventory["executed_ids"], expected)
        self.assertEqual(
            inventory["input_contract_digest"],
            COMPLETION_PROOF._validation_input_contract_digest(
                {
                    "owned_paths": [".codex/validation/**"],
                    "consumed_paths": ["validator.py"],
                }
            ),
        )
        self.assertEqual(len(self.marker.read_text(encoding="utf-8").splitlines()), 1)

    def test_malformed_post_freeze_addition_blocks_before_validation(self) -> None:
        addition_id = "fixture-command::malformed-addition"
        self._write_json(
            self.current_inventory,
            {
                "schema_version": 1,
                "tests": [
                    *self.current_rows,
                    {
                        "baseline_id": addition_id,
                        "framework": "fixture-command",
                        "native_id": "malformed-addition",
                        "source": "validator.py",
                        "ignored": False,
                        "platforms": [platform.system().casefold()],
                    },
                ],
            },
        )
        ledger = json.loads(self.ledger.read_text(encoding="utf-8"))
        ledger["additions"] = [
            {
                "test_id": addition_id,
                "preserved_behavior": "the malformed addition must not count",
                "product_path": "completion_proof.py canonical CLI",
                "validation_id": "fixture.command",
                "provenance": {
                    "kind": "policy-addition",
                    "source": "locked completion-proof plan",
                    "text": "post-freeze policy test",
                    "forged": True,
                },
            }
        ]
        self._write_json(self.ledger, ledger)

        result, report_path = self._run()

        self.assertEqual(result.returncode, 2)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        self.assertIn("provenance fields mismatch", report["fatal_error"])
        self.assertFalse(self.marker.exists())

    def test_later_infrastructure_error_retains_confirmed_failure(self) -> None:
        self._write_config(mode="fail")
        with self.config.open("a", encoding="utf-8") as output:
            output.write(
                textwrap.dedent(
                    """

                    [[validation]]
                    id = "fixture.zzz-infrastructure"
                    runner = "typed-validation"
                    validation_type = "fixture-infrastructure"
                    command = ["definitely-missing-completion-proof-runner"]
                    validation_failure_exit_codes = []
                    owned_paths = ["validator.py"]
                    consumed_paths = ["validator.py"]
                    timeout_seconds = 30
                    """
                )
            )
        result, report_path = self._run()
        self.assertEqual(result.returncode, 2)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "pre_result_error")
        failure = next(
            item for item in report["validations"] if item["id"] == "fixture.command"
        )
        self.assertEqual(failure["classification"], "confirmed_validation_failure")
        self.assertEqual(failure["confirmed_failure_ids"], ["fixture.command"])
        self.assertEqual(failure["executed_ids"], ["fixture.command"])

    def test_killed_canonical_runner_preserves_last_complete_failure_checkpoint(
        self,
    ) -> None:
        self._write_config(mode="fail")
        wait_command = [
            sys.executable,
            str(self.validator),
            str(self.marker),
            "wait",
        ]
        command_toml = ", ".join(json.dumps(value) for value in wait_command)
        with self.config.open("a", encoding="utf-8") as output:
            output.write(
                textwrap.dedent(
                    f"""

                    [[validation]]
                    id = "fixture.zzz-wait"
                    runner = "typed-validation"
                    validation_type = "fixture-command"
                    command = [{command_toml}]
                    validation_failure_exit_codes = [1]
                    owned_paths = ["validator.py"]
                    consumed_paths = ["validator.py"]
                    timeout_seconds = 30
                    """
                )
            )

        owner, report_path = self._start_run()
        try:
            self._wait_for_validator(owner)
            owner.kill()
            owner.communicate(timeout=30)

            report = self._load_report(report_path)
            self.assertEqual(report["attempt_classification"], "pre_result_error")
            failure = next(
                item
                for item in report["validations"]
                if item["id"] == "fixture.command"
            )
            self.assertEqual(failure["classification"], "confirmed_validation_failure")
            self.assertEqual(failure["confirmed_failure_ids"], ["fixture.command"])
            self.assertEqual(failure["executed_ids"], ["fixture.command"])
            self.assertNotIn(
                "fixture.zzz-wait",
                [item["id"] for item in report["validations"]],
            )
        finally:
            (self.base / "validator-release").write_text("release\n", encoding="utf-8")
            if (self.base / "validator-ready").exists():
                finished = self.base / "validator-finished"
                validator_pid = int(
                    (self.base / "validator-wait.pid").read_text(encoding="utf-8")
                )
                deadline = time.monotonic() + 30
                while (
                    time.monotonic() < deadline
                    and not finished.exists()
                    and _pid_is_running(validator_pid)
                ):
                    time.sleep(0.02)
                self.assertTrue(
                    finished.exists() or not _pid_is_running(validator_pid),
                    "blocking validator neither released nor terminated",
                )
            if owner.poll() is None:
                owner.kill()
                owner.communicate()

    def test_stale_lock_file_does_not_block_a_new_canonical_attempt(self) -> None:
        repository_key = hashlib.sha256(
            str(self.repository.resolve()).casefold().encode("utf-8")
        ).hexdigest()[:20]
        lock = self.base / f".completion-proof-{repository_key}.lock"
        lock.write_text("existing-owner", encoding="utf-8")
        result, report_path = self._run()
        self.assertEqual(result.returncode, 0, result.stderr)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "confirmed_pass")
        owner = json.loads(lock.read_text(encoding="utf-8"))
        self.assertIsInstance(owner["attempt_id"], str)
        self.assertGreater(owner["pid"], 0)
        self.assertTrue(self.marker.exists())

    def test_live_canonical_owner_blocks_a_competing_attempt(self) -> None:
        self._write_config(mode="wait")
        owner, owner_report_path = self._start_run()
        try:
            self._wait_for_validator(owner)
            contender, contender_report_path = self._run()
            self.assertEqual(contender.returncode, 2)
            contender_report = self._load_report(contender_report_path)
            self.assertEqual(
                contender_report["attempt_classification"], "pre_result_error"
            )
            self.assertIn(
                "another completion-proof attempt owns",
                contender_report["fatal_error"],
            )

            (self.base / "validator-release").write_text("release\n", encoding="utf-8")
            stdout, stderr = owner.communicate(timeout=30)
            self.assertEqual(owner.returncode, 0, f"{stdout}\n{stderr}")
            owner_report = self._load_report(owner_report_path)
            self.assertEqual(owner_report["attempt_classification"], "confirmed_pass")
        finally:
            if owner.poll() is None:
                owner.kill()
                owner.communicate()

    def test_killed_canonical_runner_releases_lock_for_unchanged_retry(self) -> None:
        self._write_config(mode="wait")
        owner, _ = self._start_run()
        try:
            self._wait_for_validator(owner)
            owner.kill()
            owner.communicate(timeout=30)
            (self.base / "validator-release").write_text("release\n", encoding="utf-8")

            retry, retry_report_path = self._run()
            self.assertEqual(retry.returncode, 0, retry.stderr)
            retry_report = self._load_report(retry_report_path)
            self.assertEqual(retry_report["attempt_classification"], "confirmed_pass")
        finally:
            if owner.poll() is None:
                owner.kill()
                owner.communicate()

    def test_public_cli_environment_cannot_authorize_fixture_config(self) -> None:
        env = os.environ.copy()
        env["CODEX_COMPLETION_PROOF_TESTING"] = "1"
        env["CODEX_COMPLETION_PROOF_TEST_CONFIG"] = str(self.config)
        result = subprocess.run(
            [
                sys.executable,
                str(RUNNER),
                "--config",
                str(self.config),
                "fingerprint",
            ],
            cwd=self.repository,
            env=env,
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("alternate completion-proof config", result.stderr)

    def test_launch_identity_change_is_an_invocation_error(self) -> None:
        env = os.environ.copy()
        env["CoDeX_CoMpLeTiOn_PrOoF_TEST_SWITCH"] = "must-not-leak"
        child_code = (
            "import os,sys; "
            "sys.exit(91 if any(k.casefold().startswith("
            "'codex_completion_proof_') for k in os.environ) else 0)"
        )
        with mock.patch.object(
            COMPLETION_PROOF,
            "_sha256_file",
            side_effect=["1" * 64, "2" * 64],
        ):
            result = COMPLETION_PROOF.run_process(
                validation_id="fixture.identity",
                execution_id=str(uuid.uuid4()),
                command=[sys.executable, "-c", child_code],
                cwd=self.repository,
                env=env,
                timeout_seconds=30,
            )
        self.assertEqual(result.returncode, 0)
        self.assertIn("identity changed during execution", result.invocation_error)
        identity = result.child.launch_target_identity
        self.assertEqual(identity["sha256_before"], "1" * 64)
        self.assertEqual(identity["sha256_after"], "2" * 64)

    def test_fingerprint_subprocesses_strip_proof_environment_case_insensitively(
        self,
    ) -> None:
        with (
            mock.patch.dict(
                os.environ,
                {"CoDeX_CoMpLeTiOn_PrOoF_TEST_SWITCH": "must-not-leak"},
                clear=False,
            ),
            mock.patch.object(
                COMPLETION_PROOF,
                "run_bounded_process",
                return_value=(
                    COMPLETION_PROOF._bounded_process_module.BoundedProcessResult(
                        returncode=0,
                        stdout=b"",
                        stderr=b"",
                        pid=17,
                        supervision_error=None,
                    )
                ),
            ) as run,
        ):
            COMPLETION_PROOF.workspace_fingerprint(self.repository)
        self.assertGreaterEqual(run.call_count, 2)
        for invocation in run.call_args_list:
            child_env = invocation.kwargs["env"]
            leaked = [
                name
                for name in child_env
                if name.casefold().startswith("codex_completion_proof_")
            ]
            self.assertEqual(leaked, [])

    def test_deleted_tracked_input_changes_fingerprint_through_cli(self) -> None:
        before = self._fingerprint()
        self.deletable_input.unlink()
        after = self._fingerprint()
        self.assertNotEqual(before, after)

    def test_argument_comment_native_inventory_reaches_real_adapter(self) -> None:
        rows, child = COMPLETION_PROOF._native_adapter_inventory(
            REPO_ROOT,
            COMPLETION_PROOF._network_disabled_env(),
            "argument-comment-lint-native",
        )
        self.assertEqual(child.exit_code, 0)
        self.assertEqual(len(rows), 21)
        self.assertEqual(len({str(row["baseline_id"]) for row in rows}), 21)
        self.assertTrue(all(row["baseline_id"] == row["native_id"] for row in rows))
        self.assertTrue(
            all(row["framework"] == "argument-comment-lint-native" for row in rows)
        )
        self.assertTrue(
            all(row["platforms"] == ["darwin", "linux", "windows"] for row in rows)
        )
        self.assertTrue(all((REPO_ROOT / str(row["source"])).is_file() for row in rows))
        sources = {str(row["source"]) for row in rows}
        self.assertIn(
            "tools/argument-comment-lint/src/comment_parser.rs",
            sources,
        )
        self.assertIn(
            "tools/argument-comment-lint/src/bin/argument-comment-lint.rs",
            sources,
        )
        self.assertEqual(
            len(
                {
                    source
                    for source in sources
                    if source.startswith("tools/argument-comment-lint/ui/")
                }
            ),
            9,
        )

    def test_windows_sandbox_native_inventory_reaches_real_adapter(self) -> None:
        rows, child = COMPLETION_PROOF._native_adapter_inventory(
            REPO_ROOT,
            COMPLETION_PROOF._network_disabled_env(),
            "windows-sandbox-smoke",
        )
        self.assertEqual(child.exit_code, 0)
        self.assertEqual(len(rows), 46)
        self.assertEqual(len({str(row["baseline_id"]) for row in rows}), 46)
        self.assertTrue(all(row["baseline_id"] == row["native_id"] for row in rows))
        self.assertTrue(
            all(row["framework"] == "windows-sandbox-smoke" for row in rows)
        )
        self.assertTrue(all(row["platforms"] == ["windows"] for row in rows))
        self.assertTrue(all((REPO_ROOT / str(row["source"])).is_file() for row in rows))

    def _valid_argument_inventory_item(self) -> dict[str, object]:
        return {
            "id": (
                "argument-comment-lint::rust-lib::"
                "workspace_crate_filter_accepts_first_party_names_only"
            ),
            "kind": "rust-lib",
            "cargo_target": ["--lib"],
            "native_id": "workspace_crate_filter_accepts_first_party_names_only",
            "ui_case": None,
            "doctest_item": None,
            "doctest_ordinal": None,
        }

    def assert_argument_inventory_report_rejected(
        self, payload: dict[str, object]
    ) -> None:
        def fake_run_process(**kwargs):
            child = COMPLETION_PROOF._unlaunched_child(
                validation_id=kwargs["validation_id"],
                execution_id=kwargs["execution_id"],
                command=kwargs["command"],
            )
            child.pid = 126
            child.executable = sys.executable
            child.exit_code = 0
            return COMPLETION_PROOF.ProcessResult(
                returncode=0,
                stdout=json.dumps(payload, sort_keys=True),
                stderr="",
                child=child,
            )

        with (
            mock.patch.object(
                COMPLETION_PROOF,
                "run_process",
                side_effect=fake_run_process,
            ),
            self.assertRaises(COMPLETION_PROOF.ProofError),
        ):
            COMPLETION_PROOF._native_adapter_inventory(
                REPO_ROOT,
                COMPLETION_PROOF._network_disabled_env(),
                "argument-comment-lint-native",
            )

    def test_argument_inventory_empty_report_is_rejected(self) -> None:
        self.assert_argument_inventory_report_rejected(
            {
                "schema_version": 1,
                "report_type": "ArgumentCommentLintNativeTestInventoryV1",
                "count": 0,
                "tests": [],
            }
        )

    def test_argument_inventory_invalid_item_schema_is_rejected(self) -> None:
        item = self._valid_argument_inventory_item()
        item["cargo_target"] = "--lib"
        self.assert_argument_inventory_report_rejected(
            {
                "schema_version": 1,
                "report_type": "ArgumentCommentLintNativeTestInventoryV1",
                "count": 1,
                "tests": [item],
            }
        )

    def test_argument_inventory_count_mismatch_is_rejected(self) -> None:
        self.assert_argument_inventory_report_rejected(
            {
                "schema_version": 1,
                "report_type": "ArgumentCommentLintNativeTestInventoryV1",
                "count": 2,
                "tests": [self._valid_argument_inventory_item()],
            }
        )

    def test_argument_inventory_duplicate_semantic_ids_are_rejected(self) -> None:
        item = self._valid_argument_inventory_item()
        self.assert_argument_inventory_report_rejected(
            {
                "schema_version": 1,
                "report_type": "ArgumentCommentLintNativeTestInventoryV1",
                "count": 2,
                "tests": [item, dict(item)],
            }
        )

    def assert_native_adapter_dispatch(self, runner: str, flag: str) -> None:
        with tempfile.TemporaryDirectory(
            prefix="native-adapter-dispatch-"
        ) as temp_name:
            temp_dir = Path(temp_name)
            rows, _ = COMPLETION_PROOF._native_adapter_inventory(
                REPO_ROOT,
                COMPLETION_PROOF._network_disabled_env(),
                runner,
            )
            intended = [str(row["native_id"]) for row in rows]
            observed_commands: list[list[str]] = []
            adapter_env = COMPLETION_PROOF._network_disabled_env()
            codex_identity: dict[str, str] = {}
            codex_build: dict[str, object] = {}
            if runner == "windows-sandbox-smoke":
                cargo_target = temp_dir / "cargo-target"
                adapter_env["CARGO_TARGET_DIR"] = str(cargo_target)
                codex_binary = (
                    cargo_target
                    / "debug"
                    / ("codex.exe" if os.name == "nt" else "codex")
                )
                codex_binary.parent.mkdir(parents=True)
                codex_binary.write_bytes(b"current fork codex fixture\n")
                codex_hash = hashlib.sha256(codex_binary.read_bytes()).hexdigest()
                codex_identity = {
                    "requested": str(codex_binary.resolve()),
                    "resolved_path": str(codex_binary.resolve()),
                    "sha256_before": codex_hash,
                    "sha256_after": codex_hash,
                }
                codex_build = {
                    "command": [
                        shutil.which("cargo") or "cargo",
                        "build",
                        "--locked",
                        "-p",
                        "codex-cli",
                        "--bin",
                        "codex",
                    ],
                    "cwd": str((REPO_ROOT / "codex-rs").resolve()),
                    "exit_code": 0,
                }

            def fake_run_process(**kwargs):
                command = list(kwargs["command"])
                observed_commands.append(command)
                selected = [
                    command[index + 1]
                    for index, value in enumerate(command[:-1])
                    if value == flag
                ]
                self.assertEqual(selected, intended)
                if runner == "argument-comment-lint-native":
                    payload = {
                        "schema_version": 1,
                        "report_type": "ArgumentCommentLintNativeTestReportV1",
                        "intended_validation_ids": intended,
                        "selected_validation_ids": intended,
                        "actually_executed_validation_ids": intended,
                        "outcomes": [
                            {
                                "id": test_id,
                                "native_id": test_id,
                                "command": ["cargo", "test"],
                                "exit_code": 0,
                                "executed": True,
                                "classification": "confirmed_pass",
                                "stdout": "",
                                "stderr": "",
                            }
                            for test_id in intended
                        ],
                        "result": "confirmed_pass",
                    }
                    stdout = json.dumps(payload, sort_keys=True)
                else:
                    payload = {
                        "report_type": "WindowsSandboxSmokeCaseReportV1",
                        "schema_version": 1,
                        "validation_id": "windows-sandbox-smoke",
                        "host_platform": "windows",
                        "attempt_root": str(temp_dir / "attempt"),
                        "intended_case_ids": intended,
                        "selected_case_ids": intended,
                        "executed_case_ids": intended,
                        "selection_error": None,
                        "codex_executable_identity": codex_identity,
                        "codex_build": codex_build,
                        "counts": {
                            "intended": len(intended),
                            "selected": len(intended),
                            "executed": len(intended),
                            "passed": len(intended),
                            "failed": 0,
                            "pre_result_error": 0,
                        },
                        "results": [
                            {
                                "id": test_id,
                                "name": test_id,
                                "status": "passed",
                                "sandbox_launches": 1,
                                "detail": "",
                                "codex_executable_identity": codex_identity,
                                "codex_build": codex_build,
                            }
                            for test_id in intended
                        ],
                    }
                    report_path = Path(command[command.index("--report-json") + 1])
                    report_path.write_text(
                        json.dumps(payload, sort_keys=True),
                        encoding="utf-8",
                    )
                    stdout = ""
                child = COMPLETION_PROOF._unlaunched_child(
                    validation_id=kwargs["validation_id"],
                    execution_id=kwargs["execution_id"],
                    command=command,
                )
                child.pid = 123
                child.executable = sys.executable
                child.exit_code = 0
                return COMPLETION_PROOF.ProcessResult(
                    returncode=0,
                    stdout=stdout,
                    stderr="",
                    child=child,
                )

            with mock.patch.object(
                COMPLETION_PROOF,
                "run_process",
                side_effect=fake_run_process,
            ):
                report, child = COMPLETION_PROOF._run_native_adapter(
                    REPO_ROOT,
                    runner,
                    intended,
                    adapter_env,
                    temp_dir,
                    timeout_seconds=30,
                )

            self.assertEqual(len(observed_commands), 1)
            if runner == "windows-sandbox-smoke":
                command = observed_commands[0]
                self.assertEqual(
                    command[command.index("--codex-bin") + 1],
                    codex_identity["resolved_path"],
                )
                self.assertIn("--build-current-codex", command)
            self.assertEqual(child.exit_code, 0)
            self.assertEqual(report["classification"], "confirmed_pass")
            self.assertEqual(report["intended_ids"], intended)
            self.assertEqual(report["selected_ids"], intended)
            self.assertEqual(report["executed_ids"], intended)
            self.assertEqual(report["executed_count"], len(intended))
            self.assertEqual(len(report["outcomes"]), len(intended))
            for outcome in report["outcomes"]:
                self.assertEqual(outcome["outcome"], "passed")
                self.assertEqual(len(outcome["native_report_sha256"]), 64)
                identity = outcome["adapter_entrypoint_identity"]
                self.assertEqual(identity["sha256_before"], identity["sha256_after"])
                self.assertEqual(len(identity["sha256_before"]), 64)
                self.assertIsInstance(outcome["native_outcome"], dict)
                if runner == "windows-sandbox-smoke":
                    self.assertEqual(
                        outcome["native_outcome"]["codex_executable_identity"],
                        codex_identity,
                    )

            if runner == "windows-sandbox-smoke":
                codex_binary.write_bytes(b"different stale Codex binary\n")
                with mock.patch.object(
                    COMPLETION_PROOF,
                    "run_process",
                    side_effect=fake_run_process,
                ):
                    stale_report, _ = COMPLETION_PROOF._run_native_adapter(
                        REPO_ROOT,
                        runner,
                        intended,
                        adapter_env,
                        temp_dir,
                        timeout_seconds=30,
                    )
                self.assertEqual(
                    stale_report["classification"],
                    "pre_result_error",
                )
                self.assertIn(
                    "executable hash was invalid or unstable",
                    stale_report["diagnostic"],
                )

    def _write_failing_native_workspace_fixture(
        self,
        runner: str,
        native_id: str,
        launch_log: Path,
    ) -> None:
        adapter = COMPLETION_PROOF.NATIVE_ADAPTERS[runner]
        relative_path = str(adapter["relative_path"])
        adapter_path = self.repository / relative_path
        adapter_path.parent.mkdir(parents=True, exist_ok=True)
        if runner == "windows-sandbox-smoke":
            fixture_codex = (
                self.repository
                / "codex-rs"
                / "target"
                / "debug"
                / ("codex.exe" if os.name == "nt" else "codex")
            )
            fixture_codex.parent.mkdir(parents=True, exist_ok=True)
            fixture_codex.write_bytes(b"current fork failure fixture\n")
        script = textwrap.dedent(
            """\
            import json
            import hashlib
            import os
            import pathlib
            import shutil
            import sys

            runner = __RUNNER__
            launch_log = pathlib.Path(__LAUNCH_LOG__)
            args = sys.argv[1:]
            flag = "--test" if runner == "argument-comment-lint-native" else "--run-case"
            intended = [
                args[index + 1]
                for index, value in enumerate(args[:-1])
                if value == flag
            ]
            launch_log.write_text(
                json.dumps({"pid": os.getpid(), "argv": args}) + "\\n",
                encoding="utf-8",
            )
            if runner == "argument-comment-lint-native":
                payload = {
                    "schema_version": 1,
                    "report_type": "ArgumentCommentLintNativeTestReportV1",
                    "intended_validation_ids": intended,
                    "selected_validation_ids": intended,
                    "actually_executed_validation_ids": intended,
                    "outcomes": [
                        {
                            "id": test_id,
                            "native_id": test_id,
                            "command": ["cargo", "test"],
                            "exit_code": 1,
                            "executed": True,
                            "classification": "confirmed_validation_failure",
                        }
                        for test_id in intended
                    ],
                    "result": "confirmed_validation_failure",
                }
                print(json.dumps(payload, sort_keys=True))
            else:
                codex_binary = pathlib.Path(
                    args[args.index("--codex-bin") + 1]
                ).resolve()
                codex_hash = hashlib.sha256(codex_binary.read_bytes()).hexdigest()
                codex_identity = {
                    "requested": str(codex_binary),
                    "resolved_path": str(codex_binary),
                    "sha256_before": codex_hash,
                    "sha256_after": codex_hash,
                }
                codex_build = {
                    "command": [
                        shutil.which("cargo") or "cargo",
                        "build",
                        "--locked",
                        "-p",
                        "codex-cli",
                        "--bin",
                        "codex",
                    ],
                    "cwd": str(codex_binary.parents[2]),
                    "exit_code": 0,
                }
                payload = {
                    "report_type": "WindowsSandboxSmokeCaseReportV1",
                    "schema_version": 1,
                    "validation_id": "windows-sandbox-smoke",
                    "host_platform": "windows",
                    "attempt_root": args[args.index("--attempt-root") + 1],
                    "intended_case_ids": intended,
                    "selected_case_ids": intended,
                    "executed_case_ids": intended,
                    "selection_error": None,
                    "codex_executable_identity": codex_identity,
                    "codex_build": codex_build,
                    "counts": {
                        "intended": len(intended),
                        "selected": len(intended),
                        "executed": len(intended),
                        "passed": 0,
                        "failed": len(intended),
                        "pre_result_error": 0,
                    },
                    "results": [
                        {
                            "id": test_id,
                            "name": test_id,
                            "status": "failed",
                            "sandbox_launches": 1,
                            "detail": "fixture assertion failed",
                            "codex_executable_identity": codex_identity,
                            "codex_build": codex_build,
                        }
                        for test_id in intended
                    ],
                }
                pathlib.Path(args[args.index("--report-json") + 1]).write_text(
                    json.dumps(payload, sort_keys=True) + "\\n",
                    encoding="utf-8",
                )
            raise SystemExit(1)
            """
        )
        adapter_path.write_text(
            script.replace("__RUNNER__", repr(runner)).replace(
                "__LAUNCH_LOG__", repr(str(launch_log))
            ),
            encoding="utf-8",
        )
        baseline_id = f"{runner}::{native_id}"
        self.baseline_rows = [
            {
                "baseline_id": baseline_id,
                "framework": runner,
                "native_id": native_id,
                "source": relative_path,
                "ignored": False,
                "platforms": [platform.system().casefold()],
            }
        ]
        self.current_rows = [dict(self.baseline_rows[0])]
        digest = _inventory_hash(self.baseline_rows)
        self._write_json(
            self.frozen_inventory,
            {
                "schema_version": 1,
                "baseline_commit": "fixture",
                "baseline_workspace_fingerprint": "fixture",
                "host_platform": platform.system().casefold(),
                "inventory_hash": digest,
                "tests": self.baseline_rows,
            },
        )
        self._write_json(
            self.current_inventory,
            {"schema_version": 1, "tests": self.current_rows},
        )
        self._write_json(
            self.ledger,
            {
                "schema_version": 1,
                "frozen_inventory_hash": digest,
                "rows": [
                    {
                        "baseline_id": baseline_id,
                        "resolution": "exception",
                        "provenance": {
                            "kind": "protected",
                            "source": "completion-proof CLI fixture",
                            "text": "the native fixture remains required and executable",
                        },
                    }
                ],
                "additions": [],
                "overrides": [],
            },
        )
        self.config.write_text(
            textwrap.dedent(
                f"""\
                schema_version = 2
                policy_id = "fixture"
                frozen_inventory_hash = {json.dumps(digest)}
                canonical_command = "just completion-proof"
                focused_command = "just completion-focused {{validation_id}}"
                documentation_command = "just source-map-check"
                repository_root = {json.dumps(self.repository.as_posix())}
                frozen_inventory = ".codex/validation/frozen.json"
                replacement_ledger = ".codex/validation/ledger.json"
                testing_current_inventory = ".codex/validation/current.json"
                host_platform = {json.dumps(platform.system().casefold())}

                [[validation]]
                id = "inventory.frozen-reconciliation"
                runner = "inventory-reconciliation"
                owned_paths = [".codex/validation/**"]
                consumed_paths = [".codex/validation/**"]
                timeout_seconds = 30

                [[validation]]
                id = {json.dumps(str(adapter["validation_id"]))}
                runner = {json.dumps(runner)}
                owned_paths = [{json.dumps(relative_path)}]
                consumed_paths = [{json.dumps(relative_path)}]
                timeout_seconds = 30
                """
            ),
            encoding="utf-8",
        )

    def test_canonical_native_adapters_preserve_failure_before_supervision_error(
        self,
    ) -> None:
        cases = {
            "argument-comment-lint-native": "argument-comment-lint::rust-lib::fixture",
            "windows-sandbox-smoke": "fixture-process-tree",
        }
        for runner, native_id in cases.items():
            with self.subTest(runner=runner):
                launch_log = self.base / f"{runner}-launch.json"
                self._write_failing_native_workspace_fixture(
                    runner,
                    native_id,
                    launch_log,
                )
                validation_id = str(
                    COMPLETION_PROOF.NATIVE_ADAPTERS[runner]["validation_id"]
                )
                patch_source = textwrap.dedent(
                    f"""\
                    _fixture_original_run_process = run_process

                    def _fixture_run_process(**kwargs):
                        result = _fixture_original_run_process(**kwargs)
                        if kwargs["validation_id"] == {validation_id!r}:
                            result.returncode = None
                            result.child.exit_code = None
                            result.invocation_error = "fixture supervision failed after validation"
                        return result

                    run_process = _fixture_run_process
                    """
                )

                result, report_path = self._run(patch_source=patch_source)

                self.assertEqual(result.returncode, 1, result.stderr)
                report = self._load_report(report_path)
                validation = next(
                    item
                    for item in report["validations"]
                    if item["id"] == validation_id
                )
                self.assertEqual(
                    validation["classification"],
                    "confirmed_validation_failure",
                )
                self.assertEqual(validation["executed_ids"], [native_id])
                self.assertEqual(validation["confirmed_failure_ids"], [native_id])
                self.assertIn("later runner error", validation["diagnostic"])
                launch = json.loads(launch_log.read_text(encoding="utf-8"))
                child = next(
                    item
                    for item in report["child_processes"]
                    if item["validation_id"] == validation_id
                )
                self.assertEqual(child["pid"], launch["pid"])
                self.assertIsNone(child["exit_code"])

    def test_argument_native_dispatch_preserves_leaf_execution_and_hashes(self) -> None:
        self.assert_native_adapter_dispatch("argument-comment-lint-native", "--test")

    def test_windows_native_dispatch_preserves_leaf_execution_and_hashes(self) -> None:
        self.assert_native_adapter_dispatch("windows-sandbox-smoke", "--run-case")

    def _run_argument_adapter_payload(
        self,
        payload: dict[str, object],
        intended: list[str],
        *,
        returncode: int,
    ) -> dict[str, object]:
        with tempfile.TemporaryDirectory(
            prefix="native-adapter-classification-"
        ) as temp_name:
            temp_dir = Path(temp_name)

            def fake_run_process(**kwargs):
                child = COMPLETION_PROOF._unlaunched_child(
                    validation_id=kwargs["validation_id"],
                    execution_id=kwargs["execution_id"],
                    command=kwargs["command"],
                )
                child.pid = 124
                child.executable = sys.executable
                child.exit_code = returncode
                return COMPLETION_PROOF.ProcessResult(
                    returncode=returncode,
                    stdout=json.dumps(payload),
                    stderr="",
                    child=child,
                )

            with mock.patch.object(
                COMPLETION_PROOF,
                "run_process",
                side_effect=fake_run_process,
            ):
                report, _ = COMPLETION_PROOF._run_native_adapter(
                    REPO_ROOT,
                    "argument-comment-lint-native",
                    intended,
                    COMPLETION_PROOF._network_disabled_env(),
                    temp_dir,
                    timeout_seconds=30,
                )
            return report

    def test_native_adapter_zero_selection_is_pre_result(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="native-adapter-classification-"
        ) as temp_name:
            with mock.patch.object(COMPLETION_PROOF, "run_process") as run:
                report, _ = COMPLETION_PROOF._run_native_adapter(
                    REPO_ROOT,
                    "argument-comment-lint-native",
                    [],
                    COMPLETION_PROOF._network_disabled_env(),
                    Path(temp_name),
                    timeout_seconds=30,
                )
        run.assert_not_called()
        self.assertEqual(report["classification"], "pre_result_error")
        self.assertEqual(report["intended_count"], 0)

    def test_native_adapter_unknown_selection_is_pre_result(self) -> None:
        unknown_id = "argument-comment-lint::rust-lib::unknown"
        report = self._run_argument_adapter_payload(
            {
                "schema_version": 1,
                "report_type": "ArgumentCommentLintNativeTestReportV1",
                "intended_validation_ids": [unknown_id],
                "selected_validation_ids": [],
                "actually_executed_validation_ids": [],
                "outcomes": [],
                "result": "pre_result_error",
                "error": "unknown native test ID",
            },
            [unknown_id],
            returncode=2,
        )
        self.assertEqual(report["classification"], "pre_result_error")
        self.assertEqual(report["executed_ids"], [])

    def test_native_adapter_preserves_failure_before_later_pre_result(self) -> None:
        failed_id = "argument-comment-lint::rust-lib::first"
        pre_result_id = "argument-comment-lint::rust-lib::second"
        report = self._run_argument_adapter_payload(
            {
                "schema_version": 1,
                "report_type": "ArgumentCommentLintNativeTestReportV1",
                "intended_validation_ids": [failed_id, pre_result_id],
                "selected_validation_ids": [failed_id, pre_result_id],
                "actually_executed_validation_ids": [failed_id],
                "outcomes": [
                    {
                        "id": failed_id,
                        "native_id": "first",
                        "command": ["cargo", "test"],
                        "exit_code": 1,
                        "executed": True,
                        "classification": "confirmed_validation_failure",
                    },
                    {
                        "id": pre_result_id,
                        "native_id": None,
                        "command": [],
                        "exit_code": None,
                        "executed": False,
                        "classification": "pre_result_error",
                    },
                ],
                "result": "confirmed_validation_failure",
            },
            [failed_id, pre_result_id],
            returncode=1,
        )
        self.assertEqual(report["classification"], "confirmed_validation_failure")
        self.assertEqual(report["executed_ids"], [failed_id])
        self.assertEqual(report["confirmed_failure_ids"], [failed_id])
        self.assertEqual(report["outcomes"][0]["native_outcome"]["id"], failed_id)

    def test_clean_production_orchestration_dispatches_all_configured_validations(
        self,
    ) -> None:
        _, production_config = COMPLETION_PROOF.load_config(
            COMPLETION_PROOF.DEFAULT_CONFIG,
            allow_test_config=False,
        )
        configured = COMPLETION_PROOF.validation_configs(
            production_config,
            allow_test_config=False,
        )
        expected_ids = [str(item["id"]) for item in configured.values()]
        self.assertEqual(len(expected_ids), 28)
        self.assertEqual(len(expected_ids), len(set(expected_ids)))
        launched: list[tuple[str, str]] = []

        def confirmed_launch(
            validation_id: str,
            runner: str,
            *,
            runner_selector: str | None = None,
            validation_type: str | None = None,
        ):
            execution_id = str(uuid.uuid4())
            launched.append((validation_id, execution_id))
            process = COMPLETION_PROOF.run_process(
                validation_id=validation_id,
                execution_id=execution_id,
                command=[sys.executable, "-c", "pass"],
                cwd=self.repository,
                env=os.environ,
                timeout_seconds=30,
            )
            self.assertEqual(process.returncode, 0, process.stderr)
            return (
                COMPLETION_PROOF._validation_report(
                    validation_id=validation_id,
                    execution_id=execution_id,
                    runner=runner,
                    runner_selector=runner_selector,
                    evidence_kind=(
                        "typed_non_test"
                        if validation_type is not None
                        else "structured_test"
                    ),
                    validation_type=validation_type,
                    classification="confirmed_pass",
                    intended=[validation_id],
                    selected=[validation_id],
                    executed=[validation_id],
                    outcomes=[{"id": validation_id, "outcome": "passed"}],
                    exit_code=0,
                ),
                process.child,
            )

        def run_inventory(**kwargs):
            return confirmed_launch(
                str(kwargs["validation_config"]["id"]),
                "inventory-reconciliation",
            )

        def run_nextest(*_args, **_kwargs):
            return confirmed_launch("rust.nextest.workspace", "rust-nextest")

        def run_doctest(*_args, **_kwargs):
            return confirmed_launch("rust.doctest.workspace", "rust-doctest")

        def run_wrapper(**kwargs):
            return confirmed_launch(
                str(kwargs["validation_id"]),
                str(kwargs["framework"]),
            )

        def run_jest(*_args, **_kwargs):
            return confirmed_launch("sdk.typescript.jest", "javascript-jest")

        def run_native(_repo_root, runner, *_args, **_kwargs):
            return confirmed_launch(
                str(COMPLETION_PROOF.NATIVE_ADAPTERS[runner]["validation_id"]),
                str(runner),
            )

        def run_rust_gate(**kwargs):
            config = kwargs["config"]
            return confirmed_launch(
                str(config["id"]),
                "rust-gate",
                runner_selector=str(config["gate"]),
            )

        def run_typed(_repo_root, config, *_args, **_kwargs):
            return confirmed_launch(
                str(config["id"]),
                "typed-validation",
                validation_type=str(config["validation_type"]),
            )

        required_by_framework = {
            framework: [f"fixture::{framework}"]
            for framework in (
                "rust-nextest",
                "rust-doctest",
                "python-unittest",
                "python-pytest",
                "javascript-jest",
                "argument-comment-lint-native",
                "windows-sandbox-smoke",
            )
        }
        reconciliation = COMPLETION_PROOF.Reconciliation(
            required_by_framework=required_by_framework,
            exceptions=[],
            additions=[],
            overrides=[],
            current_ids=set(),
        )
        report_path = self.base / "production-dispatch-report.json"
        runtime = {
            "CODEX_COMPLETION_PROOF_NONCE": uuid.uuid4().hex * 2,
            "CODEX_COMPLETION_PROOF_REPORT": str(report_path),
            "CODEX_COMPLETION_PROOF_ATTEMPT_ID": str(uuid.uuid4()),
            "CODEX_COMPLETION_PROOF_PARENT_PID": str(os.getpid()),
            "CODEX_COMPLETION_PROOF_REPOSITORY": str(REPO_ROOT.resolve()),
            "CODEX_COMPLETION_PROOF_START_FINGERPRINT": "stable-production-fixture",
            "CODEX_COMPLETION_PROOF_MUTATION_EPOCH": "7",
            "CODEX_COMPLETION_PROOF_POLICY_RUNNER_BUNDLE_SHA256": "a" * 64,
            "CODEX_COMPLETION_PROOF_RUNNER_ATTESTATION_ENDPOINT": "fixture-endpoint",
        }
        discovery_child = COMPLETION_PROOF._unlaunched_child(
            validation_id="inventory.fixture-discovery",
            execution_id=str(uuid.uuid4()),
            command=[sys.executable, "fixture-discovery"],
        )

        with mock.patch.multiple(
            COMPLETION_PROOF,
            _runtime_inputs=mock.Mock(return_value=runtime),
            _attest_runner_process=mock.Mock(return_value=None),
            workspace_fingerprint=mock.Mock(return_value="stable-production-fixture"),
            _audit_test_system_surface=mock.Mock(return_value=None),
            _testing_inventory=mock.Mock(return_value=None),
            discover_inventory=mock.Mock(return_value=([], discovery_child)),
            load_frozen_inventory=mock.Mock(return_value=({}, [], "b" * 64)),
            reconcile_inventory=mock.Mock(return_value=reconciliation),
            _run_inventory_reconciliation=mock.Mock(side_effect=run_inventory),
            _run_rust_nextest=mock.Mock(side_effect=run_nextest),
            _run_rust_doctests=mock.Mock(side_effect=run_doctest),
            _run_structured_wrapper=mock.Mock(side_effect=run_wrapper),
            _run_jest=mock.Mock(side_effect=run_jest),
            _run_native_adapter=mock.Mock(side_effect=run_native),
            _run_rust_named_gate=mock.Mock(side_effect=run_rust_gate),
            _run_typed_validation=mock.Mock(side_effect=run_typed),
        ):
            returncode = COMPLETION_PROOF.main(["run"])

        self.assertEqual(returncode, 0)
        report = self._load_report(report_path)
        self.assertEqual(report["attempt_classification"], "confirmed_pass")
        self.assertCountEqual(
            [validation_id for validation_id, _ in launched],
            expected_ids,
        )
        self.assertEqual(len(launched), 28)
        self.assertIn(
            (
                "rust.named-gate.app-server-schema-protocol",
                "app-server-schema-protocol",
            ),
            [
                (str(item["id"]), str(item.get("runner_selector", "")))
                for item in report["validations"]
            ],
        )
        execution_ids = [execution_id for _, execution_id in launched]
        self.assertEqual(len(execution_ids), len(set(execution_ids)))
        for execution_id in execution_ids:
            self.assertTrue(uuid.UUID(execution_id))
        self.assertCountEqual(
            [item["validation_id"] for item in report["child_processes"]],
            expected_ids,
        )
        self.assertTrue(
            all(item["pid"] > 0 for item in report["child_processes"])
        )

    def test_confirmed_runner_families_require_authenticated_launched_children(
        self,
    ) -> None:
        runners = (
            "inventory-reconciliation",
            "rust-nextest",
            "rust-doctest",
            "python-unittest",
            "python-pytest",
            "javascript-jest",
            "argument-comment-lint-native",
            "windows-sandbox-smoke",
            "rust-gate",
            "typed-validation",
        )
        for runner in runners:
            with self.subTest(runner=runner):
                validation_id = f"fixture.{runner}"
                execution_id = str(uuid.uuid4())
                process = COMPLETION_PROOF.run_process(
                    validation_id=validation_id,
                    execution_id=execution_id,
                    command=[sys.executable, "-c", "pass"],
                    cwd=self.repository,
                    env=os.environ,
                    timeout_seconds=30,
                )
                self.assertEqual(process.returncode, 0, process.stderr)
                report = COMPLETION_PROOF._validation_report(
                    validation_id=validation_id,
                    execution_id=execution_id,
                    runner=runner,
                    runner_selector="fixture-gate" if runner == "rust-gate" else None,
                    evidence_kind=(
                        "typed_non_test"
                        if runner == "typed-validation"
                        else "inventory_reconciliation"
                        if runner == "inventory-reconciliation"
                        else "structured_test"
                    ),
                    validation_type=(
                        "fixture-validation" if runner == "typed-validation" else None
                    ),
                    classification="confirmed_pass",
                    intended=[validation_id],
                    selected=[validation_id],
                    executed=[validation_id],
                    outcomes=[{"id": validation_id, "outcome": "passed"}],
                    exit_code=0,
                )

                errors = COMPLETION_PROOF._enforce_child_evidence_contract(
                    [report],
                    [process.child],
                )

                self.assertEqual(errors, [])
                self.assertEqual(report["classification"], "confirmed_pass")

    def test_child_evidence_contract_rejects_forged_and_duplicate_identities(
        self,
    ) -> None:
        def confirmed_pair():
            validation_id = "fixture.javascript-jest"
            execution_id = str(uuid.uuid4())
            process = COMPLETION_PROOF.run_process(
                validation_id=validation_id,
                execution_id=execution_id,
                command=[sys.executable, "-c", "pass"],
                cwd=self.repository,
                env=os.environ,
                timeout_seconds=30,
            )
            self.assertEqual(process.returncode, 0, process.stderr)
            report = COMPLETION_PROOF._validation_report(
                validation_id=validation_id,
                execution_id=execution_id,
                runner="javascript-jest",
                classification="confirmed_validation_failure",
                intended=[validation_id],
                selected=[validation_id],
                executed=[validation_id],
                outcomes=[{"id": validation_id, "outcome": "failed"}],
                exit_code=1,
            )
            return report, process.child

        cases = (
            "unlaunched",
            "fake-positive-pid",
            "mismatched-validation",
            "mismatched-execution",
            "duplicate-child",
            "duplicate-validation",
            "duplicate-execution",
        )
        for case in cases:
            with self.subTest(case=case):
                report, child = confirmed_pair()
                validations = [report]
                children = [child]
                if case in {"unlaunched", "fake-positive-pid"}:
                    forged = COMPLETION_PROOF._unlaunched_child(
                        validation_id=str(report["id"]),
                        execution_id=str(report["execution_id"]),
                        command=[sys.executable, "-c", "pass"],
                    )
                    if case == "fake-positive-pid":
                        forged.pid = 424242
                        forged.executable = sys.executable
                    children = [forged]
                elif case == "mismatched-validation":
                    child.validation_id = "fixture.other"
                elif case == "mismatched-execution":
                    child.execution_id = str(uuid.uuid4())
                elif case == "duplicate-child":
                    children.append(child)
                elif case == "duplicate-validation":
                    duplicate = dict(report)
                    duplicate["execution_id"] = str(uuid.uuid4())
                    validations.append(duplicate)
                elif case == "duplicate-execution":
                    duplicate = dict(report)
                    duplicate["id"] = "fixture.other"
                    validations.append(duplicate)

                errors = COMPLETION_PROOF._enforce_child_evidence_contract(
                    validations,
                    children,
                )

                self.assertTrue(errors)
                self.assertEqual(report["classification"], "pre_result_error")
                self.assertEqual(report["confirmed_failure_ids"], [])

    def test_production_config_uses_exact_code_owned_typed_policy(self) -> None:
        _, config = COMPLETION_PROOF.load_config(
            COMPLETION_PROOF.DEFAULT_CONFIG,
            allow_test_config=False,
        )
        validations = COMPLETION_PROOF.validation_configs(
            config,
            allow_test_config=False,
        )
        typed = {
            item["id"]: item["validation_type"]
            for key, item in validations.items()
            if key.startswith("typed-validation:")
        }
        self.assertEqual(typed, COMPLETION_PROOF.KD4_TYPED_VALIDATIONS)
        rust_gates = {
            item["id"]: item["gate"]
            for key, item in validations.items()
            if key.startswith("rust-gate:")
        }
        self.assertEqual(rust_gates, COMPLETION_PROOF.KD4_RUST_GATES)
        with (REPO_ROOT / "codex-rs" / ".config" / "kd4-rust-tests.toml").open(
            "rb"
        ) as source:
            rust_manifest = tomllib.load(source)
        self.assertEqual(
            set(rust_gates.values()),
            set(COMPLETION_PROOF.KD4_RUST_GATES.values()),
        )
        self.assertEqual(
            set(COMPLETION_PROOF.KD4_RUST_GATES.values()),
            set(rust_manifest["gates"]),
        )
        self.assertEqual(
            validations["argument-comment-lint-native"]["id"],
            "tools.argument-comment-lint.native",
        )
        self.assertEqual(
            validations["windows-sandbox-smoke"]["id"],
            "windows.sandbox-smoke",
        )
        source_map = next(
            value
            for value in validations.values()
            if value["id"] == "maintenance.source-map"
        )
        self.assertEqual(
            source_map["owned_paths"],
            ["SOURCEMAP.md", "architecture_index.json", "source_owners.toml"],
        )
        self.assertEqual(
            source_map["consumed_paths"],
            [
                "scripts/source_map_check.py",
                "scripts/source_owners.py",
                "scripts/asciicheck.py",
                "scripts/readme_toc.py",
                "justfile",
            ],
        )
        self.assertEqual(source_map["path_set_paths"], ["**"])
        self.assertEqual(source_map["evidence_path_manifests"], ["source_owners.toml"])
        source_map_command, source_map_cwd, source_map_failures = (
            COMPLETION_PROOF._production_typed_validation_spec(
                REPO_ROOT,
                source_map["validation_type"],
            )
        )
        self.assertEqual(source_map_command, ["just", "source-map-check-only"])
        self.assertEqual(source_map_cwd, REPO_ROOT)
        self.assertEqual(source_map_failures, frozenset({1}))

        inventory = next(
            value
            for value in validations.values()
            if value["id"] == "inventory.frozen-reconciliation"
        )
        expected_inventory_inputs = {
            "justfile",
            "**/Cargo.toml",
            "codex-rs/**/*.rs",
            "tools/argument-comment-lint/**",
            "codex-rs/windows-sandbox-rs/sandbox_smoketests.py",
            "**/test_*.py",
            "**/*_test.py",
            "**/package.json",
            "**/pyproject.toml",
            "**/pytest.ini",
            "**/setup.cfg",
            "**/tox.ini",
            "**/*_test.go",
            "**/*.bats",
        }
        expected_inventory_inputs.update(
            f"**/{runner}.config.{suffix}"
            for runner in ("jest", "playwright", "vitest")
            for suffix in ("cjs", "cts", "js", "mjs", "mts", "ts")
        )
        expected_inventory_inputs.update(
            f"**/*.{kind}.{suffix}"
            for kind in ("test", "spec")
            for suffix in ("cjs", "cts", "js", "jsx", "mjs", "mts", "ts", "tsx")
        )
        self.assertEqual(set(inventory["consumed_paths"]), expected_inventory_inputs)

        expected_windows_paths = {
            "windows.sandbox-smoke": {
                "owned_paths": ["codex-rs/windows-sandbox-rs/**"],
                "consumed_paths": [
                    "codex-rs/windows-sandbox-rs/**",
                    "codex-rs/cli/**",
                    "codex-rs/core/**",
                ],
            },
            "rust.named-gate.windows-sandbox-core-exec": {
                "owned_paths": [
                    "codex-rs/core/**",
                    "codex-rs/windows-sandbox-rs/**",
                ],
                "consumed_paths": [
                    "codex-rs/core/**",
                    "codex-rs/windows-sandbox-rs/**",
                    "codex-rs/.config/kd4-rust-tests.toml",
                    "codex-rs/.config/nextest.toml",
                    "codex-rs/scripts/nextest_windows_stack.py",
                    "scripts/rust_test_runner.py",
                    "justfile",
                ],
            },
            "windows.process-coverage": {
                "owned_paths": [
                    "codex-rs/utils/pty/**",
                    "codex-rs/windows-sandbox-rs/**",
                    "codex-rs/core/**",
                ],
                "consumed_paths": [
                    "codex-rs/utils/pty/**",
                    "codex-rs/windows-sandbox-rs/**",
                    "codex-rs/core/**",
                    "codex-rs/.config/kd4-rust-tests.toml",
                    "codex-rs/.config/nextest.toml",
                    "codex-rs/scripts/nextest_windows_stack.py",
                    "scripts/rust_test_runner.py",
                    "justfile",
                ],
            },
        }
        for validation_id, expected_paths in expected_windows_paths.items():
            validation = next(
                value for value in validations.values() if value["id"] == validation_id
            )
            self.assertEqual(validation["owned_paths"], expected_paths["owned_paths"])
            self.assertEqual(
                validation["consumed_paths"], expected_paths["consumed_paths"]
            )

        real_windows_root = REPO_ROOT / "codex-rs" / "windows-sandbox-rs"
        phantom_windows_root = REPO_ROOT / "codex-rs" / "windows-sandbox"
        self.assertTrue(real_windows_root.is_dir())
        self.assertFalse(phantom_windows_root.exists())
        expected_named_rust_gate_inputs = {
            "rust.named-gate.adaptive-reasoning-contract": [
                "codex-rs/config/**",
                "codex-rs/core/**",
                "codex-rs/.config/kd4-rust-tests.toml",
                "codex-rs/.config/nextest.toml",
                "codex-rs/scripts/nextest_windows_stack.py",
                "scripts/rust_test_runner.py",
                "justfile",
            ],
            "rust.named-gate.config-schema-protocol": [
                "codex-rs/config/**",
                "codex-rs/core/**",
                "codex-rs/.config/kd4-rust-tests.toml",
                "codex-rs/.config/nextest.toml",
                "codex-rs/scripts/nextest_windows_stack.py",
                "scripts/rust_test_runner.py",
                "justfile",
            ],
            "rust.named-gate.core-helper-resolution": [
                "codex-rs/core/**",
                "codex-rs/utils/cargo-bin/**",
                "codex-rs/.config/kd4-rust-tests.toml",
                "codex-rs/.config/nextest.toml",
                "codex-rs/scripts/nextest_windows_stack.py",
                "scripts/rust_test_runner.py",
                "justfile",
            ],
            "rust.named-gate.core-stdio-helper-regressions": [
                "codex-rs/core/**",
                "codex-rs/rmcp-client/**",
                "codex-rs/.config/kd4-rust-tests.toml",
                "codex-rs/.config/nextest.toml",
                "codex-rs/scripts/nextest_windows_stack.py",
                "scripts/rust_test_runner.py",
                "justfile",
            ],
            "rust.named-gate.rmcp-streamable-http": [
                "codex-rs/rmcp-client/**",
                "codex-rs/.config/kd4-rust-tests.toml",
                "codex-rs/.config/nextest.toml",
                "codex-rs/scripts/nextest_windows_stack.py",
                "scripts/rust_test_runner.py",
                "justfile",
            ],
            "rust.named-gate.apply-patch-scenarios": [
                "codex-rs/apply-patch/**",
                "codex-rs/.config/kd4-rust-tests.toml",
                "codex-rs/.config/nextest.toml",
                "codex-rs/scripts/nextest_windows_stack.py",
                "scripts/rust_test_runner.py",
                "justfile",
            ],
            "rust.named-gate.tools-json-schema-policy-fixtures": [
                "codex-rs/tools/**",
                "codex-rs/.config/kd4-rust-tests.toml",
                "codex-rs/.config/nextest.toml",
                "codex-rs/scripts/nextest_windows_stack.py",
                "scripts/rust_test_runner.py",
                "justfile",
            ],
        }
        for validation_id, expected_consumed_paths in (
            expected_named_rust_gate_inputs.items()
        ):
            validation = next(
                value for value in validations.values() if value["id"] == validation_id
            )
            self.assertEqual(validation["consumed_paths"], expected_consumed_paths)
        expected_python_project_inputs = {
            "maintenance.root-unittest": [
                "scripts/**",
                "tools/**",
                "codex-cli/**",
                "justfile",
                "scripts/pyproject.toml",
                "scripts/uv.lock",
            ],
            "sdk.python.pytest": [
                "sdk/python/**",
                "scripts/completion_proof_pytest.py",
                "sdk/python/pyproject.toml",
                "sdk/python/uv.lock",
            ],
            "sdk.python.ruff": [
                "sdk/python/**",
                "sdk/python/pyproject.toml",
                "sdk/python/uv.lock",
            ],
            "maintenance.script-audit": [
                "scripts/**",
                "tools/**",
                "justfile",
                "scripts/pyproject.toml",
                "scripts/uv.lock",
            ],
        }
        for (
            validation_id,
            expected_consumed_paths,
        ) in expected_python_project_inputs.items():
            validation = next(
                value for value in validations.values() if value["id"] == validation_id
            )
            self.assertEqual(validation["consumed_paths"], expected_consumed_paths)
        expected_generated_and_wrapper_inputs = {
            "generated.config-schema": [
                "codex-rs/config/**",
                "codex-rs/core/**",
                "codex-rs/features/**",
                "codex-rs/protocol/**",
                "scripts/config_schema_check.py",
                "scripts/generated_output_lock.py",
                "codex-rs/.config/kd4-rust-tests.toml",
                "scripts/rust_test_runner.py",
                "codex-rs/.config/nextest.toml",
                "codex-rs/scripts/nextest_windows_stack.py",
                "justfile",
            ],
            "generated.app-server-schema": [
                "codex-rs/app-server-protocol/**",
                "codex-rs/protocol/**",
                "sdk/python/**",
                "scripts/app_server_schema_runtime_check.py",
                "scripts/generated_output_lock.py",
                "codex-rs/.config/kd4-rust-tests.toml",
                "scripts/rust_test_runner.py",
                "codex-rs/.config/nextest.toml",
                "codex-rs/scripts/nextest_windows_stack.py",
                "justfile",
            ],
            "generated.exec-server-relay-proto": [
                "codex-rs/exec-server/**",
                "codex-rs/Cargo.lock",
                "codex-rs/Cargo.toml",
            ],
            "wrapper.codex-cli": [
                "codex-cli/**",
                "codex-rs/responses-api-proxy/npm/**",
                "scripts/test_stage_npm_packages.py",
                "scripts/stage_npm_packages.py",
                "scripts/stage_npm_archives.py",
                "scripts/codex_package/targets.py",
                "justfile",
            ],
        }
        for validation_id, expected_consumed_paths in (
            expected_generated_and_wrapper_inputs.items()
        ):
            validation = next(
                value for value in validations.values() if value["id"] == validation_id
            )
            self.assertEqual(validation["consumed_paths"], expected_consumed_paths)
        config_proto = next(
            value
            for value in validations.values()
            if value["id"] == "generated.config-proto"
        )
        self.assertEqual(
            config_proto["owned_paths"],
            [("codex-rs/config/src/thread_config/proto/codex.thread_config.v1.rs")],
        )
        self.assertEqual(
            config_proto["consumed_paths"],
            [
                ".gitattributes",
                "codex-rs/Cargo.lock",
                "codex-rs/Cargo.toml",
                "codex-rs/config/Cargo.toml",
                "codex-rs/config/examples/generate-proto.rs",
                "codex-rs/config/scripts/generate-proto.ps1",
                (
                    "codex-rs/config/src/thread_config/proto/"
                    "codex.thread_config.v1.proto"
                ),
                "codex-rs/rustfmt.toml",
                "scripts/cargo-lane.ps1",
            ],
        )
        for item in typed:
            configured = next(
                value for value in validations.values() if value["id"] == item
            )
            self.assertNotIn("command", configured)
            self.assertNotIn("validation_failure_exit_codes", configured)


class ChildValidationJournalFoundationTest(unittest.TestCase):
    ACTION_ID = "fixture.child-validation"

    def _binding(self) -> object:
        return CHILD_REPORT.JournalInvocationBinding(
            proof_attempt_id=str(uuid.uuid4()),
            proof_execution_id=str(uuid.uuid4()),
            proof_receipt_nonce=uuid.uuid4().hex + uuid.uuid4().hex,
            proof_scope="focused",
            validation_id="fixture.child-validation-journal",
            validation_type="typed-child-validation",
            input_contract_digest=hashlib.sha256(b"fixture inputs").hexdigest(),
        )

    def _emit_subprocess(
        self,
        root: Path,
        *,
        binding: object,
        classification: str,
        later_infrastructure_error: bool = False,
        truncate_after_valid_prefix: bool = False,
        seal: bool = True,
        intended_ids: list[str] | None = None,
        selected_ids: list[str] | None = None,
    ) -> tuple[Path, object, int, int]:
        journal_path = root / "child-validation.ndjson"
        started_at = time.time_ns()
        intended = intended_ids or [self.ACTION_ID]
        selected = selected_ids or [self.ACTION_ID]
        script = textwrap.dedent(
            """
            import json
            import os
            import pathlib
            import sys

            sys.path.insert(0, os.environ["KD4_REPO_ROOT"])
            from scripts.child_validation_report import (
                ChildValidationJournalWriter,
                JournalInvocationBinding,
                ProcessIdentity,
            )

            binding = JournalInvocationBinding.from_mapping(
                json.loads(os.environ["KD4_BINDING"])
            )
            producer = ProcessIdentity.current(
                started_at_unix_ns=int(os.environ["KD4_STARTED_AT"]),
                arguments=sys.argv,
            )
            journal_path = pathlib.Path(os.environ["KD4_JOURNAL"])
            classification = os.environ["KD4_CLASSIFICATION"]
            intended_ids = json.loads(os.environ["KD4_INTENDED_IDS"])
            selected_ids = json.loads(os.environ["KD4_SELECTED_IDS"])
            with ChildValidationJournalWriter(
                journal_path,
                binding=binding,
                producer=producer,
                intended_ids=intended_ids,
                selected_ids=selected_ids,
            ) as writer:
                execution_id = writer.start_action(
                    "fixture.child-validation",
                    subjects=["fixture.subject"],
                    process=producer,
                )
                result_code = {
                    "confirmed_pass": "passed",
                    "confirmed_validation_failure": "failed",
                    "pre_result_error": "error",
                }[classification]
                writer.finish_action(
                    "fixture.child-validation",
                    action_execution_id=execution_id,
                    classification=classification,
                    actually_executed=classification != "pre_result_error",
                    result_code=result_code,
                    exit_code=0 if classification == "confirmed_pass" else 7,
                    diagnostic="fixture result",
                )
                if os.environ["KD4_LATER_INFRA"] == "1":
                    writer.infrastructure_error(
                        phase="fixture-cleanup",
                        diagnostic="later infrastructure error",
                    )
                if os.environ["KD4_SEAL"] == "1":
                    writer.seal()
            if os.environ["KD4_TRUNCATE"] == "1":
                with journal_path.open("ab") as handle:
                    handle.write(b'{"record_type"')
                    handle.flush()
                    os.fsync(handle.fileno())
            raise SystemExit(0 if classification == "confirmed_pass" else 7)
            """
        )
        environment = os.environ.copy()
        environment.update(
            {
                "KD4_REPO_ROOT": str(REPO_ROOT),
                "KD4_BINDING": json.dumps(binding.as_record_fields()),
                "KD4_STARTED_AT": str(started_at),
                "KD4_JOURNAL": str(journal_path),
                "KD4_CLASSIFICATION": classification,
                "KD4_INTENDED_IDS": json.dumps(intended),
                "KD4_SELECTED_IDS": json.dumps(selected),
                "KD4_LATER_INFRA": "1" if later_infrastructure_error else "0",
                "KD4_TRUNCATE": "1" if truncate_after_valid_prefix else "0",
                "KD4_SEAL": "1" if seal else "0",
            }
        )
        process = subprocess.Popen(
            [sys.executable, "-c", script],
            cwd=REPO_ROOT,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        stdout, stderr = process.communicate(timeout=15)
        ended_at = time.time_ns()
        self.assertEqual(stderr, "")
        expected_exit = 0 if classification == "confirmed_pass" else 7
        self.assertEqual(process.returncode, expected_exit, stdout)
        producer = CHILD_REPORT.ProcessIdentity(
            pid=process.pid,
            executable_path=str(Path(sys.executable).resolve()),
            executable_sha256=CHILD_REPORT.hash_file(Path(sys.executable).resolve()),
            argv_sha256=CHILD_REPORT.hash_arguments(["-c"]),
            started_at_unix_ns=started_at,
        )
        assert process.returncode is not None
        return journal_path, producer, ended_at, process.returncode

    def _parse(
        self,
        journal_path: Path,
        *,
        binding: object,
        producer: object,
        ended_at: int,
        outer_exit: int | None,
        expected_journal_path: Path | None = None,
        intended_ids: list[str] | None = None,
        selected_ids: list[str] | None = None,
        action_processes: Mapping[str, object] | None = None,
        outer_executable_sha256_after: str | None = None,
    ) -> object:
        return CHILD_REPORT.parse_child_validation_journal(
            journal_path,
            expected_journal_path=expected_journal_path or journal_path,
            expected_binding=binding,
            expected_intended_ids=intended_ids or [self.ACTION_ID],
            expected_selected_ids=selected_ids or [self.ACTION_ID],
            expected_producer=producer,
            expected_action_processes=action_processes
            or {self.ACTION_ID: producer},
            outer_ended_at_unix_ns=ended_at,
            outer_executable_sha256_after=(
                outer_executable_sha256_after or producer.executable_sha256
            ),
            outer_exit_code=outer_exit,
        )

    def _read_records(self, journal_path: Path) -> list[dict[str, object]]:
        return [
            json.loads(line)
            for line in journal_path.read_text(encoding="utf-8").splitlines()
        ]

    def _write_rechained(
        self, journal_path: Path, records: list[dict[str, object]]
    ) -> None:
        previous_hash: str | None = None
        encoded: list[bytes] = []
        for sequence, record in enumerate(records):
            record["sequence"] = sequence
            record["previous_record_sha256"] = previous_hash
            if record.get("record_type") == "seal":
                record["sealed_prefix_sha256"] = previous_hash
            record["record_sha256"] = CHILD_REPORT.record_sha256(record)
            previous_hash = str(record["record_sha256"])
            encoded.append(CHILD_REPORT.canonical_json(record))
        journal_path.write_bytes(b"\n".join(encoded) + b"\n")

    def test_subprocess_emits_and_parser_accepts_fresh_sealed_pass(self) -> None:
        with tempfile.TemporaryDirectory(prefix="child-validation-journal-") as temp:
            root = Path(temp)
            binding = self._binding()
            journal, producer, ended_at, outer_exit = self._emit_subprocess(
                root,
                binding=binding,
                classification="confirmed_pass",
            )
            verdict = self._parse(
                journal,
                binding=binding,
                producer=producer,
                ended_at=ended_at,
                outer_exit=outer_exit,
            )
            self.assertEqual(verdict.classification, "confirmed_pass")
            self.assertTrue(verdict.sealed)
            self.assertEqual(verdict.intended_ids, (self.ACTION_ID,))
            self.assertEqual(verdict.selected_ids, (self.ACTION_ID,))
            self.assertEqual(verdict.executed_ids, (self.ACTION_ID,))
            self.assertEqual(verdict.confirmed_failure_ids, ())
            self.assertEqual(verdict.valid_record_count, 4)
            self.assertRegex(verdict.journal_sha256 or "", r"^[0-9a-f]{64}$")

            binding_path = root / "binding.json"
            selection_path = root / "selection.json"
            producer_path = root / "producer.json"
            action_processes_path = root / "action-processes.json"
            binding_path.write_text(
                json.dumps(binding.as_record_fields()), encoding="utf-8"
            )
            selection_path.write_text(
                json.dumps(
                    {
                        "intended_ids": [self.ACTION_ID],
                        "selected_ids": [self.ACTION_ID],
                    }
                ),
                encoding="utf-8",
            )
            producer_path.write_text(
                json.dumps(producer.as_record_fields()), encoding="utf-8"
            )
            action_processes_path.write_text(
                json.dumps({self.ACTION_ID: producer.as_record_fields()}),
                encoding="utf-8",
            )
            cli = subprocess.run(
                [
                    sys.executable,
                    str(CHILD_REPORT_PATH),
                    "validate",
                    "--journal",
                    str(journal),
                    "--expected-journal",
                    str(journal),
                    "--binding",
                    str(binding_path),
                    "--selection",
                    str(selection_path),
                    "--producer",
                    str(producer_path),
                    "--action-processes",
                    str(action_processes_path),
                    "--outer-ended-at-unix-ns",
                    str(ended_at),
                    "--outer-executable-sha256-after",
                    producer.executable_sha256,
                    "--outer-exit-code",
                    str(outer_exit),
                ],
                cwd=REPO_ROOT,
                capture_output=True,
                text=True,
                timeout=15,
                check=False,
            )
            self.assertEqual(cli.returncode, 0, cli.stderr)
            cli_verdict = json.loads(cli.stdout)
            self.assertEqual(cli_verdict["classification"], "confirmed_pass")
            self.assertEqual(cli_verdict["valid_record_count"], 4)

    def test_failure_prefix_survives_later_infrastructure_error_and_truncation(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(prefix="child-validation-journal-") as temp:
            binding = self._binding()
            journal, producer, ended_at, outer_exit = self._emit_subprocess(
                Path(temp),
                binding=binding,
                classification="confirmed_validation_failure",
                later_infrastructure_error=True,
                truncate_after_valid_prefix=True,
                seal=False,
            )
            verdict = self._parse(
                journal,
                binding=binding,
                producer=producer,
                ended_at=ended_at,
                outer_exit=outer_exit,
            )
            self.assertEqual(verdict.classification, "confirmed_validation_failure")
            self.assertEqual(verdict.confirmed_failure_ids, (self.ACTION_ID,))
            self.assertEqual(verdict.valid_record_count, 4)
            self.assertFalse(verdict.sealed)
            self.assertIn("truncated", "\n".join(verdict.diagnostics))

    def test_binding_replay_and_outer_exit_contradictions_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory(prefix="child-validation-journal-") as temp:
            root = Path(temp)
            binding = self._binding()
            journal, producer, ended_at, outer_exit = self._emit_subprocess(
                root,
                binding=binding,
                classification="confirmed_validation_failure",
            )
            replacements = {
                "nonce": {"proof_receipt_nonce": uuid.uuid4().hex * 2},
                "attempt": {"proof_attempt_id": str(uuid.uuid4())},
                "execution": {"proof_execution_id": str(uuid.uuid4())},
                "scope": {"proof_scope": "canonical"},
                "validation": {"validation_id": "fixture.other-validation"},
                "type": {"validation_type": "fixture-other-type"},
                "inputs": {
                    "input_contract_digest": hashlib.sha256(
                        b"different fixture inputs"
                    ).hexdigest()
                },
            }
            for label, replacement in replacements.items():
                with self.subTest(binding=label):
                    replay_binding = CHILD_REPORT.JournalInvocationBinding(
                        **{
                            **binding.as_record_fields(),
                            **replacement,
                        }
                    )
                    verdict = self._parse(
                        journal,
                        binding=replay_binding,
                        producer=producer,
                        ended_at=ended_at,
                        outer_exit=outer_exit,
                    )
                    self.assertEqual(verdict.classification, "pre_result_error")
                    self.assertEqual(verdict.valid_record_count, 0)
                    self.assertEqual(verdict.confirmed_failure_ids, ())

            contradiction = self._parse(
                journal,
                binding=binding,
                producer=producer,
                ended_at=ended_at,
                outer_exit=0,
            )
            self.assertEqual(contradiction.classification, "pre_result_error")
            self.assertIn("contradicts", "\n".join(contradiction.diagnostics))

            pass_root = root / "pass"
            pass_root.mkdir()
            pass_binding = self._binding()
            pass_journal, pass_producer, pass_ended_at, _ = self._emit_subprocess(
                pass_root,
                binding=pass_binding,
                classification="confirmed_pass",
            )
            pass_contradiction = self._parse(
                pass_journal,
                binding=pass_binding,
                producer=pass_producer,
                ended_at=pass_ended_at,
                outer_exit=9,
            )
            self.assertEqual(pass_contradiction.classification, "pre_result_error")
            self.assertIn("contradicts", "\n".join(pass_contradiction.diagnostics))

            records = self._read_records(journal)
            records[2]["action_execution_id"] = str(uuid.uuid4())
            self._write_rechained(journal, records)
            action_mismatch = self._parse(
                journal,
                binding=binding,
                producer=producer,
                ended_at=ended_at,
                outer_exit=outer_exit,
            )
            self.assertEqual(action_mismatch.classification, "pre_result_error")
            self.assertEqual(action_mismatch.confirmed_failure_ids, ())
            self.assertIn("does not match", "\n".join(action_mismatch.diagnostics))

    def test_external_selection_and_action_process_bindings_fail_closed(self) -> None:
        second_action = "fixture.second-validation"
        with tempfile.TemporaryDirectory(prefix="child-validation-journal-") as temp:
            root = Path(temp)
            binding = self._binding()
            journal, producer, ended_at, outer_exit = self._emit_subprocess(
                root,
                binding=binding,
                classification="confirmed_validation_failure",
                intended_ids=[self.ACTION_ID, second_action],
                selected_ids=[self.ACTION_ID],
            )
            underselected = self._parse(
                journal,
                binding=binding,
                producer=producer,
                ended_at=ended_at,
                outer_exit=outer_exit,
                intended_ids=[self.ACTION_ID, second_action],
                selected_ids=[self.ACTION_ID, second_action],
                action_processes={
                    self.ACTION_ID: producer,
                    second_action: producer,
                },
            )
            self.assertEqual(underselected.classification, "pre_result_error")
            self.assertEqual(underselected.confirmed_failure_ids, ())
            self.assertIn(
                "external selection", "\n".join(underselected.diagnostics)
            )
            with self.assertRaisesRegex(
                CHILD_REPORT.JournalContractError,
                "must exactly match",
            ):
                self._parse(
                    journal,
                    binding=binding,
                    producer=producer,
                    ended_at=ended_at,
                    outer_exit=outer_exit,
                    intended_ids=[self.ACTION_ID, second_action],
                    selected_ids=[self.ACTION_ID],
                )

            pass_root = root / "pass"
            pass_root.mkdir()
            pass_binding = self._binding()
            pass_journal, pass_producer, pass_ended_at, pass_exit = (
                self._emit_subprocess(
                    pass_root,
                    binding=pass_binding,
                    classification="confirmed_pass",
                )
            )
            process_fields = pass_producer.as_record_fields()
            process_replacements = {
                "pid": {"pid": pass_producer.pid + 1},
                "path": {
                    "executable_path": str((root / "other-python.exe").resolve())
                },
                "executable": {"executable_sha256": "1" * 64},
                "argv": {"argv_sha256": "2" * 64},
                "timestamp": {
                    "started_at_unix_ns": pass_producer.started_at_unix_ns + 1
                },
            }
            for label, replacement in process_replacements.items():
                with self.subTest(action_process=label):
                    expected_action = CHILD_REPORT.ProcessIdentity(
                        **{**process_fields, **replacement}
                    )
                    verdict = self._parse(
                        pass_journal,
                        binding=pass_binding,
                        producer=pass_producer,
                        ended_at=pass_ended_at,
                        outer_exit=pass_exit,
                        action_processes={self.ACTION_ID: expected_action},
                    )
                    self.assertEqual(verdict.classification, "pre_result_error")
                    self.assertIn(
                        "action process identity mismatch",
                        "\n".join(verdict.diagnostics),
                    )

    def test_producer_identity_and_unsealed_after_hash_are_externally_bound(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(prefix="child-validation-journal-") as temp:
            root = Path(temp)
            binding = self._binding()
            journal, producer, ended_at, outer_exit = self._emit_subprocess(
                root,
                binding=binding,
                classification="confirmed_validation_failure",
                later_infrastructure_error=True,
                truncate_after_valid_prefix=True,
                seal=False,
            )
            changed_after = self._parse(
                journal,
                binding=binding,
                producer=producer,
                ended_at=ended_at,
                outer_exit=outer_exit,
                outer_executable_sha256_after="f" * 64,
            )
            self.assertEqual(changed_after.classification, "pre_result_error")
            self.assertEqual(changed_after.confirmed_failure_ids, ())
            self.assertIn(
                "producer executable changed",
                "\n".join(changed_after.diagnostics),
            )

            producer_fields = producer.as_record_fields()
            producer_replacements = {
                "pid": {"pid": producer.pid + 1},
                "path": {
                    "executable_path": str((root / "different.exe").resolve())
                },
                "executable": {"executable_sha256": "3" * 64},
                "argv": {"argv_sha256": "4" * 64},
                "timestamp": {"started_at_unix_ns": producer.started_at_unix_ns + 1},
            }
            for label, replacement in producer_replacements.items():
                with self.subTest(producer=label):
                    expected_producer = CHILD_REPORT.ProcessIdentity(
                        **{**producer_fields, **replacement}
                    )
                    verdict = self._parse(
                        journal,
                        binding=binding,
                        producer=expected_producer,
                        ended_at=ended_at,
                        outer_exit=outer_exit,
                        action_processes={self.ACTION_ID: producer},
                        outer_executable_sha256_after=(
                            expected_producer.executable_sha256
                        ),
                    )
                    self.assertEqual(verdict.classification, "pre_result_error")
                    self.assertEqual(verdict.confirmed_failure_ids, ())
                    self.assertIn(
                        "producer identity mismatch",
                        "\n".join(verdict.diagnostics),
                    )

    def test_stale_copied_preexisting_and_unknown_records_are_rejected(self) -> None:
        timestamp_cases = (
            "record-before-producer",
            "result-after-outer-end",
            "seal-after-outer-end",
        )
        for case in timestamp_cases:
            with self.subTest(timestamp=case), tempfile.TemporaryDirectory(
                prefix="child-validation-journal-"
            ) as temp:
                binding = self._binding()
                journal, producer, ended_at, outer_exit = self._emit_subprocess(
                    Path(temp),
                    binding=binding,
                    classification="confirmed_pass",
                )
                records = self._read_records(journal)
                if case == "record-before-producer":
                    records[0]["emitted_at_unix_ns"] = (
                        producer.started_at_unix_ns - 1
                    )
                elif case == "result-after-outer-end":
                    records[2]["ended_at_unix_ns"] = ended_at + 1
                else:
                    records[3]["producer_ended_at_unix_ns"] = ended_at + 1
                self._write_rechained(journal, records)
                verdict = self._parse(
                    journal,
                    binding=binding,
                    producer=producer,
                    ended_at=ended_at,
                    outer_exit=outer_exit,
                )
                self.assertEqual(verdict.classification, "pre_result_error")
                self.assertNotEqual(verdict.diagnostics, ())

        with tempfile.TemporaryDirectory(prefix="child-validation-journal-") as temp:
            root = Path(temp)
            binding = self._binding()
            journal, producer, ended_at, outer_exit = self._emit_subprocess(
                root,
                binding=binding,
                classification="confirmed_pass",
            )
            copied = root / "copied.ndjson"
            shutil.copyfile(journal, copied)
            with self.assertRaisesRegex(
                CHILD_REPORT.JournalContractError,
                "journal path does not match",
            ):
                self._parse(
                    copied,
                    expected_journal_path=journal,
                    binding=binding,
                    producer=producer,
                    ended_at=ended_at,
                    outer_exit=outer_exit,
                )
            fresh_binding = self._binding()
            replay = self._parse(
                copied,
                expected_journal_path=copied,
                binding=fresh_binding,
                producer=producer,
                ended_at=ended_at,
                outer_exit=outer_exit,
            )
            self.assertEqual(replay.classification, "pre_result_error")
            self.assertEqual(replay.valid_record_count, 0)

            preexisting = root / "preexisting.ndjson"
            preexisting.write_text("occupied", encoding="utf-8")
            with self.assertRaises(FileExistsError):
                CHILD_REPORT.ChildValidationJournalWriter(
                    preexisting,
                    binding=fresh_binding,
                    producer=producer,
                    intended_ids=[self.ACTION_ID],
                    selected_ids=[self.ACTION_ID],
                )

            records = self._read_records(journal)
            records[0]["unexpected_field"] = "not permitted"
            self._write_rechained(journal, records)
            unknown = self._parse(
                journal,
                binding=binding,
                producer=producer,
                ended_at=ended_at,
                outer_exit=outer_exit,
            )
            self.assertEqual(unknown.classification, "pre_result_error")
            self.assertIn("fields do not match", "\n".join(unknown.diagnostics))

    def test_runtime_and_json_schema_agree_on_records_when_available(self) -> None:
        with tempfile.TemporaryDirectory(prefix="child-validation-journal-") as temp:
            binding = self._binding()
            journal, producer, ended_at, outer_exit = self._emit_subprocess(
                Path(temp),
                binding=binding,
                classification="confirmed_pass",
            )
            verdict = self._parse(
                journal,
                binding=binding,
                producer=producer,
                ended_at=ended_at,
                outer_exit=outer_exit,
            )
            self.assertEqual(verdict.classification, "confirmed_pass")
            schema = json.loads(
                (
                    REPO_ROOT
                    / ".codex"
                    / "validation"
                    / "child-validation-journal-v1.schema.json"
                ).read_text(encoding="utf-8")
            )
            self.assertEqual(set(schema["required"]), set(CHILD_REPORT._COMMON_KEYS))
            schema_record_keys = {}
            for branch in schema["allOf"][0]["oneOf"]:
                record_type = branch["properties"]["record_type"]["const"]
                schema_record_keys[record_type] = set(branch["required"])
            self.assertEqual(
                schema_record_keys,
                {
                    record_type: set(keys)
                    for record_type, keys in CHILD_REPORT._RECORD_KEYS.items()
                },
            )
            path_pattern = schema["$defs"]["processIdentity"]["properties"][
                "executable_path"
            ]["pattern"]
            self.assertEqual(
                path_pattern, CHILD_REPORT._ABSOLUTE_EXECUTABLE_PATH.pattern
            )
            self.assertIsNotNone(
                CHILD_REPORT.re.compile(path_pattern).match(producer.executable_path)
            )
            relative_path = "relative/python.exe"
            self.assertIsNone(CHILD_REPORT.re.compile(path_pattern).match(relative_path))
            with self.assertRaisesRegex(
                CHILD_REPORT.JournalContractError, "not absolute"
            ):
                CHILD_REPORT.ProcessIdentity(
                    **{
                        **producer.as_record_fields(),
                        "executable_path": relative_path,
                    }
                )

            canonical_uuid_pattern = schema["$defs"]["canonicalUuid"]["pattern"]
            canonical_uuid = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
            self.assertIsNotNone(
                CHILD_REPORT.re.compile(canonical_uuid_pattern).fullmatch(canonical_uuid)
            )
            self.assertEqual(
                CHILD_REPORT._canonical_uuid(canonical_uuid, "proof_attempt_id"),
                canonical_uuid,
            )
            for noncanonical_uuid in (
                canonical_uuid.upper(),
                canonical_uuid.replace("-", ""),
            ):
                with self.subTest(noncanonical_uuid=noncanonical_uuid):
                    self.assertIsNone(
                        CHILD_REPORT.re.compile(canonical_uuid_pattern).fullmatch(
                            noncanonical_uuid
                        )
                    )
                    with self.assertRaisesRegex(
                        CHILD_REPORT.JournalContractError, "not a canonical UUID"
                    ):
                        CHILD_REPORT._canonical_uuid(
                            noncanonical_uuid, "proof_attempt_id"
                        )

            canonical_uuid_ref = {"$ref": "#/$defs/canonicalUuid"}
            self.assertEqual(schema["properties"]["proof_attempt_id"], canonical_uuid_ref)
            self.assertEqual(
                schema["properties"]["proof_execution_id"], canonical_uuid_ref
            )
            for branch in schema["allOf"][0]["oneOf"]:
                if branch["properties"]["record_type"]["const"] in {
                    "action_started",
                    "action_result",
                }:
                    self.assertEqual(
                        branch["properties"]["action_execution_id"],
                        canonical_uuid_ref,
                    )
            self.assertEqual(
                schema["$defs"]["outcome"]["properties"]["action_execution_id"],
                canonical_uuid_ref,
            )

            if importlib.util.find_spec("jsonschema") is not None:
                import jsonschema

                validator = jsonschema.Draft202012Validator(
                    schema,
                    format_checker=jsonschema.FormatChecker(),
                )
                records = self._read_records(journal)
                for record in records:
                    with self.subTest(record_type=record["record_type"]):
                        self.assertEqual(list(validator.iter_errors(record)), [])
                records[0]["unexpected_field"] = "not permitted"
                self.assertFalse(validator.is_valid(records[0]))

    def test_chain_duplicate_zero_selection_and_missing_seal_are_rejected(
        self,
    ) -> None:
        cases = (
            "broken-chain",
            "duplicate-start",
            "duplicate-terminal",
            "infrastructure-with-pass",
            "zero-selection",
            "missing-seal",
        )
        for case in cases:
            with self.subTest(case=case), tempfile.TemporaryDirectory(
                prefix="child-validation-journal-"
            ) as temp:
                binding = self._binding()
                journal, producer, ended_at, outer_exit = self._emit_subprocess(
                    Path(temp),
                    binding=binding,
                    classification="confirmed_pass",
                )
                records = self._read_records(journal)
                if case == "broken-chain":
                    records[1]["previous_record_sha256"] = "0" * 64
                    journal.write_bytes(
                        b"\n".join(CHILD_REPORT.canonical_json(item) for item in records)
                        + b"\n"
                    )
                elif case == "duplicate-start":
                    duplicate = dict(records[1])
                    duplicate["action_execution_id"] = str(uuid.uuid4())
                    records.insert(2, duplicate)
                    self._write_rechained(journal, records)
                elif case == "duplicate-terminal":
                    records.insert(3, dict(records[2]))
                    self._write_rechained(journal, records)
                elif case == "infrastructure-with-pass":
                    infrastructure = {
                        **{
                            key: records[0][key]
                            for key in (
                                "schema_version",
                                "report_type",
                                "proof_attempt_id",
                                "proof_execution_id",
                                "proof_receipt_nonce",
                                "proof_scope",
                                "validation_id",
                                "validation_type",
                                "input_contract_digest",
                                "emitted_at_unix_ns",
                            )
                        },
                        "record_type": "infrastructure_error",
                        "phase": "fixture-cleanup",
                        "diagnostic": "cleanup failed",
                    }
                    records.insert(3, infrastructure)
                    self._write_rechained(journal, records)
                elif case == "zero-selection":
                    records[0]["selected_ids"] = []
                    records[0]["selected_count"] = 0
                    self._write_rechained(journal, records)
                elif case == "missing-seal":
                    self._write_rechained(journal, records[:-1])
                verdict = self._parse(
                    journal,
                    binding=binding,
                    producer=producer,
                    ended_at=ended_at,
                    outer_exit=outer_exit,
                )
                self.assertEqual(verdict.classification, "pre_result_error")
                self.assertFalse(verdict.sealed)


if __name__ == "__main__":
    unittest.main()
