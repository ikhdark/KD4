#!/usr/bin/env python3

from __future__ import annotations

import json
import math
import os
import queue
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import uuid
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from unittest.mock import patch


REPO_ROOT = Path(__file__).resolve().parents[1]
MANIFEST = REPO_ROOT / ".codex" / "hooks.json"
HELPER = REPO_ROOT / ".codex" / "hooks" / "task-continuity-entry.ps1"
SLOW_HELPER = REPO_ROOT / ".codex" / "hooks" / "task-continuity.ps1"
FAST_HELPERS = (
    REPO_ROOT / ".codex" / "hooks" / "task-continuity-fast-basic.ps1",
    REPO_ROOT / ".codex" / "hooks" / "task-continuity-fast-compact.ps1",
    REPO_ROOT / ".codex" / "hooks" / "task-continuity-fast-session.ps1",
)
INSTALLED_CODEX = Path(r"C:\Users\kuh\Desktop\LOCAL-KD\codex.exe")
RUN_TIMEOUT_SECONDS = 20
BENCHMARK_INVOCATIONS = 20
BENCHMARK_MEDIAN_LIMIT_MS = 750.0
MANIFEST_EVENTS = [
    "UserPromptSubmit",
    "PreCompact",
    "PostCompact",
    "SessionStart",
    "Stop",
]
PRODUCTION_COMMANDS = {
    event: (
        "powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass "
        f'-File "{HELPER}" -ExpectedEvent {event}'
    )
    for event in MANIFEST_EVENTS
}
DISCOVERY_EVENTS = {
    "UserPromptSubmit": "userPromptSubmit",
    "PreCompact": "preCompact",
    "PostCompact": "postCompact",
    "SessionStart": "sessionStart",
    "Stop": "stop",
}


def windows_powershell() -> str | None:
    return shutil.which("powershell.exe") or shutil.which("powershell")


def compact_json(value: object) -> str:
    return json.dumps(value, ensure_ascii=True, separators=(",", ":"))


def normalize_path(value: str | os.PathLike[str]) -> str:
    return os.path.normcase(os.path.abspath(os.fspath(value)))


def recovery_context(capsule: dict[str, Any]) -> str | None:
    if not any(
        capsule.get(name)
        for name in (
            "last_user_request",
            "last_assistant_result",
            "predecessor_thread_id",
        )
    ):
        return None
    lines = [
        "KD4 task continuity recovery (capsule v1; bounded and redacted).",
        f"Session: {capsule['session_id']}",
        f"Continuity epoch: {capsule['continuity_epoch']}",
    ]
    if capsule.get("predecessor_thread_id"):
        lines.append(f"Predecessor thread: {capsule['predecessor_thread_id']}")
    if capsule.get("task_label"):
        lines.append(f"Task label: {capsule['task_label']}")
    if capsule.get("last_user_request"):
        lines.extend(["Last user request:", capsule["last_user_request"]])
    if capsule.get("last_assistant_result"):
        lines.extend(["Last assistant result:", capsule["last_assistant_result"]])
    repository = capsule["repository"]
    if repository.get("root"):
        lines.extend(
            [
                f"Repository: {repository['root']} @ {repository['revision']}",
                f"Dirty summary: {repository['dirty_summary']}",
            ]
        )
    lines.append(f"Compaction state: {capsule['compaction']['phase']}")
    if capsule.get("transcript_path"):
        lines.append(f"Transcript: {capsule['transcript_path']}")
    lines.append(
        "Treat this as recovery context only; reconcile it with the current "
        "workspace and request before acting."
    )
    value = "\n".join(lines)
    return value if len(value) <= 8000 else value[:7997] + "..."


def exact_injection(capsule: dict[str, Any]) -> str:
    context = recovery_context(capsule)
    if context is None:
        return "{}"
    return compact_json(
        {
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": context,
            }
        }
    )


def invoke_helper(
    powershell: str,
    helper: Path,
    payload: dict[str, Any] | str,
    *,
    timeout: int = RUN_TIMEOUT_SECONDS,
    env: dict[str, str] | None = None,
) -> tuple[subprocess.CompletedProcess[str], float]:
    stdin = payload if isinstance(payload, str) else compact_json(payload)
    payload_event = (
        payload.get("hook_event_name") if isinstance(payload, dict) else None
    )
    expected_event = (
        payload_event if payload_event in MANIFEST_EVENTS else "UserPromptSubmit"
    )
    started = time.perf_counter()
    result = subprocess.run(
        [
            powershell,
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            str(helper),
            "-ExpectedEvent",
            expected_event,
        ],
        input=stdin,
        text=True,
        encoding="utf-8",
        capture_output=True,
        check=False,
        timeout=timeout,
        env=env,
    )
    return result, (time.perf_counter() - started) * 1000.0


class HookSandbox:
    def __init__(self, powershell: str) -> None:
        self.powershell = powershell
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.helper = self.root / ".codex" / "hooks" / HELPER.name
        self.helper.parent.mkdir(parents=True)
        shutil.copy2(HELPER, self.helper)
        shutil.copy2(SLOW_HELPER, self.helper.parent / SLOW_HELPER.name)
        for fast_helper in FAST_HELPERS:
            shutil.copy2(fast_helper, self.helper.parent / fast_helper.name)
        self.state = (
            self.root / ".codex" / "harness" / "runs" / "task-continuity" / "v1"
        )
        self.transcript = self.root / "rollout.jsonl"
        self.transcript.write_text("", encoding="utf-8")

    def close(self) -> None:
        self.temporary.cleanup()

    def session_id(self) -> str:
        return str(uuid.uuid4())

    def capsule_path(self, session_id: str) -> Path:
        return self.state / f"{session_id.lower()}.json"

    def capsule(self, session_id: str) -> dict[str, Any]:
        return json.loads(self.capsule_path(session_id).read_text(encoding="utf-8"))

    def payload(
        self,
        event: str,
        session_id: str,
        **overrides: Any,
    ) -> dict[str, Any]:
        value: dict[str, Any] = {
            "session_id": session_id,
            "transcript_path": str(self.transcript),
            "cwd": str(self.root),
            "hook_event_name": event,
            "model": "continuity-test-model",
            "permission_mode": "never",
        }
        if event != "SessionStart":
            value["turn_id"] = "turn-1"
        if event == "UserPromptSubmit":
            value["prompt"] = "Continue the continuity test"
        elif event in {"PreCompact", "PostCompact"}:
            value["trigger"] = "manual"
        elif event == "SessionStart":
            value["source"] = "startup"
        elif event == "Stop":
            value["stop_hook_active"] = False
            value["last_assistant_message"] = "Continuity test complete"
        value.update(overrides)
        return value

    def invoke(
        self,
        payload: dict[str, Any] | str,
        *,
        env: dict[str, str] | None = None,
    ) -> tuple[subprocess.CompletedProcess[str], float]:
        result, elapsed_ms = invoke_helper(
            self.powershell,
            self.helper,
            payload,
            env=env,
        )
        if result.returncode != 0:
            raise AssertionError(
                f"helper returned {result.returncode}\n"
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
            )
        return result, elapsed_ms


def classify_hooks_list(
    response: dict[str, Any],
    manifest: Path = MANIFEST,
) -> tuple[dict[str, str], list[str], list[dict[str, Any]]]:
    data = response.get("result", {}).get("data", [])
    project_result = next(
        (
            item
            for item in data
            if normalize_path(item.get("cwd", "")) == normalize_path(REPO_ROOT)
        ),
        None,
    )
    if project_result is None:
        return ({name: "missing" for name in MANIFEST_EVENTS}, ["cwd missing"], [])

    expected_source = normalize_path(manifest)
    hooks = [
        hook
        for hook in project_result.get("hooks", [])
        if normalize_path(hook.get("sourcePath", "")) == expected_source
    ]
    diagnoses: dict[str, str] = {}
    issues = [
        *(f"warning: {value}" for value in project_result.get("warnings", [])),
        *(f"error: {value}" for value in project_result.get("errors", [])),
    ]
    for manifest_event, discovery_event in DISCOVERY_EVENTS.items():
        hook = next(
            (entry for entry in hooks if entry.get("eventName") == discovery_event),
            None,
        )
        if hook is None:
            diagnoses[manifest_event] = "missing"
            continue
        if not hook.get("enabled", False):
            diagnoses[manifest_event] = "disabled"
        else:
            trust_status = hook.get("trustStatus")
            diagnoses[manifest_event] = (
                trust_status
                if trust_status in {"untrusted", "modified", "trusted"}
                else str(trust_status or "untrusted")
            )
        if hook.get("handlerType") != "command":
            issues.append(f"{manifest_event}: handler type is not command")
        if hook.get("command") != PRODUCTION_COMMANDS[manifest_event]:
            issues.append(f"{manifest_event}: selected Windows command differs")
        if hook.get("timeoutSec") != 5:
            issues.append(f"{manifest_event}: timeout is not five seconds")
        if hook.get("source") != "project":
            issues.append(f"{manifest_event}: source is not project")
    return diagnoses, issues, hooks


def app_server_hooks_list(codex: Path) -> tuple[dict[str, Any], dict[str, Any], str]:
    process = subprocess.Popen(
        [str(codex), "app-server", "--stdio"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        bufsize=1,
    )
    assert process.stdin is not None
    assert process.stdout is not None
    assert process.stderr is not None
    responses: queue.Queue[dict[str, Any]] = queue.Queue()
    stderr_lines: list[str] = []

    def read_stdout() -> None:
        for line in process.stdout:
            try:
                responses.put(json.loads(line))
            except json.JSONDecodeError:
                continue

    def read_stderr() -> None:
        stderr_lines.extend(process.stderr.readlines())

    threading.Thread(target=read_stdout, daemon=True).start()
    threading.Thread(target=read_stderr, daemon=True).start()

    def send(message: dict[str, Any]) -> None:
        process.stdin.write(compact_json(message) + "\n")
        process.stdin.flush()

    def wait_for(request_id: int) -> dict[str, Any]:
        deadline = time.monotonic() + RUN_TIMEOUT_SECONDS
        while time.monotonic() < deadline:
            try:
                message = responses.get(timeout=0.2)
            except queue.Empty:
                if process.poll() is not None:
                    break
                continue
            if message.get("id") == request_id:
                return message
        raise RuntimeError(f"app-server did not answer request {request_id}")

    try:
        send(
            {
                "method": "initialize",
                "id": 1,
                "params": {
                    "clientInfo": {
                        "name": "kd4_task_continuity_doctor",
                        "title": "KD4 Task Continuity Doctor",
                        "version": "1.0.0",
                    }
                },
            }
        )
        initialize = wait_for(1)
        send({"method": "initialized"})
        send(
            {
                "method": "hooks/list",
                "id": 2,
                "params": {"cwds": [str(REPO_ROOT)]},
            }
        )
        hooks = wait_for(2)
        return initialize, hooks, "".join(stderr_lines)
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


class LoopbackResponsesServer:
    def __init__(self) -> None:
        self.requests: list[bytes] = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_POST(self) -> None:  # noqa: N802 - stdlib callback name
                length = int(self.headers.get("Content-Length", "0"))
                body = self.rfile.read(length)
                owner.requests.append(body)
                response_id = f"resp-continuity-{len(owner.requests)}"
                events = [
                    {
                        "type": "response.created",
                        "response": {"id": response_id},
                    },
                    {
                        "type": "response.output_item.done",
                        "item": {
                            "type": "message",
                            "role": "assistant",
                            "content": [
                                {
                                    "type": "output_text",
                                    "text": "Continuity doctor response",
                                }
                            ],
                        },
                    },
                    {
                        "type": "response.completed",
                        "response": {
                            "id": response_id,
                            "usage": {
                                "input_tokens": 0,
                                "input_tokens_details": None,
                                "output_tokens": 0,
                                "output_tokens_details": None,
                                "total_tokens": 0,
                            },
                        },
                    },
                ]
                payload = "".join(
                    f"event: {event['type']}\ndata: {compact_json(event)}\n\n"
                    for event in events
                ).encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(payload)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(payload)
                self.wfile.flush()

            def log_message(self, _format: str, *_args: object) -> None:
                return

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    @property
    def base_url(self) -> str:
        host, port = self.server.server_address
        return f"http://{host}:{port}/v1"

    def __enter__(self) -> LoopbackResponsesServer:
        self.thread.start()
        return self

    def __exit__(self, *_args: object) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def run_installed_marker_smoke(codex: Path) -> dict[str, Any]:
    cleanup_paths: list[Path] = []
    primary_error: BaseException | None = None
    try:
        return _run_installed_marker_smoke(codex, cleanup_paths)
    except BaseException as error:
        primary_error = error
        raise
    finally:
        cleanup_errors: list[str] = []
        for path in reversed(cleanup_paths):
            try:
                path.unlink(missing_ok=True)
            except OSError as error:
                cleanup_errors.append(f"{path}: {error}")
        if cleanup_errors:
            message = "marker smoke cleanup failed: " + "; ".join(cleanup_errors)
            if primary_error is None:
                raise RuntimeError(message)
            print(message, file=sys.stderr)


def _run_installed_marker_smoke(
    codex: Path, cleanup_paths: list[Path]
) -> dict[str, Any]:
    marker = f"KD4_SESSIONSTART_MARKER_{uuid.uuid4().hex}"
    capsule_path: Path | None = None
    marker_report: dict[str, Any] | None = None
    with LoopbackResponsesServer() as server:
        config = [
            "-c",
            'model_provider="continuity_doctor"',
            "-c",
            'model_providers.continuity_doctor.name="Continuity Doctor"',
            "-c",
            f'model_providers.continuity_doctor.base_url="{server.base_url}"',
            "-c",
            'model_providers.continuity_doctor.wire_api="responses"',
            "-c",
            "model_providers.continuity_doctor.requires_openai_auth=false",
            "-c",
            "features.enable_request_compression=false",
            "-m",
            "continuity-doctor-model",
        ]
        first = subprocess.run(
            [
                str(codex),
                "exec",
                "--json",
                "--skip-git-repo-check",
                *config,
                "Run the KD4 continuity doctor seed turn.",
            ],
            cwd=REPO_ROOT,
            text=True,
            encoding="utf-8",
            capture_output=True,
            check=False,
            timeout=60,
        )
        if first.returncode != 0:
            raise RuntimeError(
                f"doctor seed task failed ({first.returncode}): {first.stderr}"
            )
        events = []
        for line in first.stdout.splitlines():
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                continue
        started = next(
            (event for event in events if event.get("type") == "thread.started"),
            None,
        )
        if started is None:
            raise RuntimeError("doctor seed task did not report a thread id")
        thread_id = str(started["thread_id"])
        capsule_path = (
            REPO_ROOT
            / ".codex"
            / "harness"
            / "runs"
            / "task-continuity"
            / "v1"
            / f"{thread_id}.json"
        )
        cleanup_paths.append(capsule_path)
        if not capsule_path.is_file():
            raise RuntimeError("trusted seed task did not create a continuity capsule")
        if any(marker.encode("utf-8") in body for body in server.requests):
            raise RuntimeError(
                "unique marker unexpectedly appeared before capsule injection"
            )

        capsule = json.loads(capsule_path.read_text(encoding="utf-8"))
        capsule["task_label"] = "SessionStart marker smoke"
        capsule["last_user_request"] = marker
        capsule["last_assistant_result"] = None
        temporary = capsule_path.with_name(
            f".{capsule_path.name}.{uuid.uuid4().hex}.tmp"
        )
        cleanup_paths.append(temporary)
        temporary.write_text(compact_json(capsule), encoding="utf-8")
        os.replace(temporary, capsule_path)
        request_count = len(server.requests)

        resumed = subprocess.run(
            [
                str(codex),
                "exec",
                "resume",
                "--json",
                "--skip-git-repo-check",
                *config,
                thread_id,
                "Continue the KD4 continuity doctor.",
            ],
            cwd=REPO_ROOT,
            text=True,
            encoding="utf-8",
            capture_output=True,
            check=False,
            timeout=60,
        )
        if resumed.returncode != 0:
            raise RuntimeError(
                f"doctor resume task failed ({resumed.returncode}): {resumed.stderr}"
            )
        captured = server.requests[request_count:]
        if not captured:
            raise RuntimeError("doctor resume produced no captured model request")
        if not any(marker.encode("utf-8") in body for body in captured):
            raise RuntimeError(
                "SessionStart marker did not reach a captured resumed model request"
            )
        marker_report = {
            "thread_id": thread_id,
            "marker": marker,
            "captured_requests": len(captured),
        }
    if marker_report is None:
        raise RuntimeError("marker smoke ended without a report")
    return marker_report


def run_doctor() -> int:
    if not INSTALLED_CODEX.is_file():
        print(f"installed binary is missing: {INSTALLED_CODEX}", file=sys.stderr)
        return 2
    try:
        initialize, response, server_stderr = app_server_hooks_list(INSTALLED_CODEX)
    except Exception as error:  # noqa: BLE001 - doctor must classify support failures
        print(f"hooks/list is unavailable: {error}", file=sys.stderr)
        return 2

    features = (
        initialize.get("result", {})
        .get("serverCapabilities", {})
        .get("enabledFeatures", [])
    )
    if "hooks" not in features or "error" in response:
        print(
            "installed binary does not expose the required hooks contract",
            file=sys.stderr,
        )
        return 2

    diagnoses, issues, hooks = classify_hooks_list(response)
    report = {
        "binary": str(INSTALLED_CODEX),
        "manifest": str(MANIFEST),
        "diagnoses": diagnoses,
        "hooks": [
            {
                "eventName": hook.get("eventName"),
                "enabled": hook.get("enabled"),
                "trustStatus": hook.get("trustStatus"),
                "key": hook.get("key"),
                "currentHash": hook.get("currentHash"),
            }
            for hook in hooks
        ],
        "issues": issues,
    }
    print(json.dumps(report, indent=2))
    if server_stderr.strip():
        print("app-server diagnostics were emitted on stderr", file=sys.stderr)
    if issues or any(status != "trusted" for status in diagnoses.values()):
        print(
            "Task-continuity hooks are not operationally trusted. Review and trust "
            "them through /hooks; the doctor never auto-trusts or bypasses trust.",
            file=sys.stderr,
        )
        return 2

    try:
        marker_report = run_installed_marker_smoke(INSTALLED_CODEX)
    except Exception as error:  # noqa: BLE001 - report exact installed-runtime blocker
        print(f"trusted marker smoke failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps({"marker_smoke": marker_report}, indent=2))
    return 0


def run_benchmark() -> int:
    shell = windows_powershell()
    if shell is None:
        print("Windows PowerShell is unavailable", file=sys.stderr)
        return 2
    sandbox = HookSandbox(shell)
    try:
        git = shutil.which("git.exe") or shutil.which("git")
        if git is None:
            print("Git is unavailable", file=sys.stderr)
            return 2
        for args in (
            ["init"],
            ["config", "user.email", "continuity@example.invalid"],
            ["config", "user.name", "Continuity Benchmark"],
        ):
            subprocess.run(
                [git, *args],
                cwd=sandbox.root,
                check=True,
                text=True,
                capture_output=True,
            )
        tracked = sandbox.root / "tracked.txt"
        tracked.write_text("tracked\n", encoding="utf-8")
        subprocess.run([git, "add", tracked.name], cwd=sandbox.root, check=True)
        subprocess.run(
            [git, "commit", "-m", "benchmark fixture"],
            cwd=sandbox.root,
            check=True,
            text=True,
            capture_output=True,
        )

        measured: dict[str, tuple[dict[str, Any], str]] = {}
        for event in MANIFEST_EVENTS:
            session_id = sandbox.session_id()
            if event == "SessionStart":
                seed, _ = sandbox.invoke(
                    sandbox.payload("UserPromptSubmit", session_id)
                )
                if seed.stdout != "{}":
                    raise RuntimeError(
                        "SessionStart benchmark seed emitted unexpected stdout"
                    )
                payload = sandbox.payload(
                    "SessionStart",
                    session_id,
                    source="resume",
                )
                warm, _ = sandbox.invoke(payload)
                expected = exact_injection(sandbox.capsule(session_id))
                if warm.stdout != expected:
                    raise RuntimeError(
                        "SessionStart benchmark warmup output was not exact"
                    )
            else:
                payload = sandbox.payload(event, session_id)
                warm, _ = sandbox.invoke(payload)
                if warm.stdout != "{}":
                    raise RuntimeError(f"{event} benchmark warmup output was not exact")
                expected = "{}"
            measured[event] = (payload, expected)

        report: dict[str, dict[str, float]] = {}
        failures: list[str] = []
        for event, (payload, expected) in measured.items():
            timings: list[float] = []
            for _ in range(BENCHMARK_INVOCATIONS):
                result, elapsed_ms = sandbox.invoke(payload)
                if result.stdout != expected:
                    raise RuntimeError(f"{event} benchmark stdout was not byte-exact")
                timings.append(elapsed_ms)
            ordered = sorted(timings)
            median_ms = statistics.median(ordered)
            p95_ms = ordered[math.ceil(0.95 * len(ordered)) - 1]
            report[event] = {
                "invocations": float(BENCHMARK_INVOCATIONS),
                "median_ms": round(median_ms, 3),
                "p95_ms": round(p95_ms, 3),
            }
            if median_ms >= BENCHMARK_MEDIAN_LIMIT_MS:
                failures.append(event)
        print(json.dumps(report, indent=2))
        if not failures:
            return 0
        if failures == ["UserPromptSubmit"]:
            print(
                "UserPromptSubmit alone missed the 750 ms median target; apply the "
                "documented manifest fallback and rely on Stop plus SessionStart.",
                file=sys.stderr,
            )
            return 3
        print(
            "Events outside the permitted UserPromptSubmit fallback missed the latency "
            f"target: {', '.join(failures)}",
            file=sys.stderr,
        )
        return 1
    finally:
        sandbox.close()


class MarkerSmokeCleanupTest(unittest.TestCase):
    def test_cleanup_runs_after_success(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            capsule = Path(temp_dir) / "capsule.json"
            temporary = Path(temp_dir) / ".capsule.json.tmp"
            capsule.write_text("{}", encoding="utf-8")
            temporary.write_text("{}", encoding="utf-8")

            def succeed(_codex: Path, cleanup_paths: list[Path]) -> dict[str, Any]:
                cleanup_paths.extend([capsule, temporary])
                return {"marker": "ok"}

            with patch.object(
                sys.modules[__name__],
                "_run_installed_marker_smoke",
                side_effect=succeed,
            ):
                self.assertEqual(
                    run_installed_marker_smoke(Path("codex")), {"marker": "ok"}
                )
            self.assertFalse(capsule.exists())
            self.assertFalse(temporary.exists())

    def test_cleanup_preserves_primary_failure(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            capsule = Path(temp_dir) / "capsule.json"
            temporary = Path(temp_dir) / ".capsule.json.tmp"
            capsule.write_text("{}", encoding="utf-8")
            temporary.write_text("{}", encoding="utf-8")

            def fail(_codex: Path, cleanup_paths: list[Path]) -> dict[str, Any]:
                cleanup_paths.extend([capsule, temporary])
                raise ValueError("seed failed")

            with patch.object(
                sys.modules[__name__], "_run_installed_marker_smoke", side_effect=fail
            ):
                with self.assertRaisesRegex(ValueError, "seed failed"):
                    run_installed_marker_smoke(Path("codex"))
            self.assertFalse(capsule.exists())
            self.assertFalse(temporary.exists())


class TaskContinuityHookTest(unittest.TestCase):
    def setUp(self) -> None:
        shell = windows_powershell()
        if shell is None:
            self.skipTest("Windows PowerShell is not available")
        self.shell = shell
        self.sandbox = HookSandbox(shell)

    def tearDown(self) -> None:
        self.sandbox.close()

    def assert_empty(self, result: subprocess.CompletedProcess[str]) -> None:
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "{}")

    def invoke_empty(
        self,
        payload: dict[str, Any] | str,
        *,
        env: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        result, _ = self.sandbox.invoke(payload, env=env)
        self.assert_empty(result)
        return result

    def invoke_injection(
        self,
        payload: dict[str, Any],
        session_id: str,
    ) -> subprocess.CompletedProcess[str]:
        result, _ = self.sandbox.invoke(payload)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(
            result.stdout, exact_injection(self.sandbox.capsule(session_id))
        )
        return result

    def test_manifest_has_exact_handlers_flags_paths_and_timeouts(self) -> None:
        manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
        self.assertEqual(list(manifest), ["hooks"])
        self.assertEqual(list(manifest["hooks"]), MANIFEST_EVENTS)
        for event in MANIFEST_EVENTS:
            groups = manifest["hooks"][event]
            self.assertEqual(len(groups), 1)
            self.assertEqual(set(groups[0]), {"hooks"})
            self.assertEqual(len(groups[0]["hooks"]), 1)
            handler = groups[0]["hooks"][0]
            self.assertEqual(
                set(handler), {"type", "command", "commandWindows", "timeout"}
            )
            self.assertEqual(handler["type"], "command")
            self.assertEqual(handler["command"], PRODUCTION_COMMANDS[event])
            self.assertEqual(handler["commandWindows"], PRODUCTION_COMMANDS[event])
            self.assertEqual(handler["timeout"], 5)
            self.assertNotIn("async", handler)
            self.assertIn(
                "-NoLogo -NoProfile -NonInteractive", handler["commandWindows"]
            )
            self.assertIn(f'-File "{HELPER}"', handler["commandWindows"])
        self.assertTrue(HELPER.is_file())
        self.assertTrue(SLOW_HELPER.is_file())
        self.assertTrue(all(helper.is_file() for helper in FAST_HELPERS))

    def test_helper_parses_as_windows_powershell(self) -> None:
        command = """
$tokens = $null
$errors = $null
[System.Management.Automation.Language.Parser]::ParseFile($env:KD4_CONTINUITY_SCRIPT, [ref]$tokens, [ref]$errors) | Out-Null
if ($errors.Count -ne 0) {
    $errors | ForEach-Object { Write-Error $_.Message }
    exit 1
}
"""
        for helper in (HELPER, SLOW_HELPER, *FAST_HELPERS):
            result = subprocess.run(
                [
                    self.shell,
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    command,
                ],
                text=True,
                encoding="utf-8",
                capture_output=True,
                check=False,
                timeout=RUN_TIMEOUT_SECONDS,
                env={**os.environ, "KD4_CONTINUITY_SCRIPT": str(helper)},
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "")

    def test_every_event_uses_exact_stdout(self) -> None:
        session_id = self.sandbox.session_id()
        self.invoke_empty(self.sandbox.payload("SessionStart", session_id))
        self.invoke_empty(self.sandbox.payload("UserPromptSubmit", session_id))
        self.invoke_empty(self.sandbox.payload("PreCompact", session_id))
        self.invoke_empty(self.sandbox.payload("PostCompact", session_id))
        self.invoke_empty(self.sandbox.payload("Stop", session_id))
        self.invoke_injection(
            self.sandbox.payload("SessionStart", session_id, source="resume"),
            session_id,
        )

    def test_resume_output_is_exact_bounded_and_redacted(self) -> None:
        session_id = self.sandbox.session_id()
        prompt = (
            "Resume the exact-output task with token=super-secret-value and "
            "Bearer abcdefghijklmnop"
        )
        self.invoke_empty(
            self.sandbox.payload("UserPromptSubmit", session_id, prompt=prompt)
        )
        result = self.invoke_injection(
            self.sandbox.payload("SessionStart", session_id, source="resume"),
            session_id,
        )
        self.assertNotIn("super-secret-value", result.stdout)
        self.assertNotIn("abcdefghijklmnop", result.stdout)
        self.assertIn("[REDACTED]", result.stdout)
        self.assertLessEqual(
            len(recovery_context(self.sandbox.capsule(session_id)) or ""), 8000
        )

    def test_schema_bounds_and_secret_redaction(self) -> None:
        session_id = self.sandbox.session_id()
        prompt = "sk-proj-abcdefghijklmno " + ("x" * 5000)
        self.invoke_empty(
            self.sandbox.payload("UserPromptSubmit", session_id, prompt=prompt)
        )
        self.invoke_empty(
            self.sandbox.payload(
                "Stop",
                session_id,
                last_assistant_message="password=hunter2 result",
            )
        )
        capsule = self.sandbox.capsule(session_id)
        self.assertEqual(
            set(capsule),
            {
                "schema_version",
                "session_id",
                "continuity_epoch",
                "predecessor_thread_id",
                "created_at",
                "updated_at",
                "last_event",
                "last_turn_id",
                "working_directory",
                "transcript_path",
                "task_label",
                "last_user_request",
                "last_assistant_result",
                "repository",
                "compaction",
                "material_digest",
            },
        )
        self.assertEqual(capsule["schema_version"], 1)
        self.assertIsNone(capsule["predecessor_thread_id"])
        self.assertLessEqual(len(capsule["last_user_request"]), 4000)
        self.assertLessEqual(len(capsule["last_assistant_result"]), 4000)
        self.assertLessEqual(len(capsule["task_label"]), 80)
        self.assertNotIn("abcdefghijklmno", compact_json(capsule))
        self.assertNotIn("hunter2", compact_json(capsule))
        self.assertEqual(len(capsule["material_digest"]), 64)

    def test_all_handled_error_fixtures_emit_exact_empty_object(self) -> None:
        fixtures: list[tuple[str, dict[str, Any] | str]] = [
            ("empty input", ""),
            ("malformed JSON", "{"),
            ("non-object JSON", "[]"),
            (
                "unsupported event",
                self.sandbox.payload(
                    "Stop", self.sandbox.session_id(), hook_event_name="Unknown"
                ),
            ),
            (
                "unsafe session id",
                self.sandbox.payload("Stop", "../../escape"),
            ),
            (
                "redaction failure",
                self.sandbox.payload(
                    "UserPromptSubmit",
                    self.sandbox.session_id(),
                    prompt="bad\x00secret",
                ),
            ),
        ]
        for name, payload in fixtures:
            with self.subTest(name=name):
                result = self.invoke_empty(payload)
                self.assertTrue(result.stderr.startswith("task-continuity:"))

    def test_subagent_payload_early_exits_without_state_or_diagnostics(self) -> None:
        session_id = self.sandbox.session_id()
        payload = {
            "agent_id": "agent-1",
            "hook_event_name": "UserPromptSubmit",
        }
        result = self.invoke_empty(payload)
        self.assertEqual(result.stderr, "")
        self.assertFalse(self.sandbox.capsule_path(session_id).exists())
        self.assertFalse(self.sandbox.state.exists())

    def test_missing_transcript_git_failure_and_non_git_directory_fail_open(
        self,
    ) -> None:
        session_id = self.sandbox.session_id()
        missing = self.sandbox.root / "missing-rollout.jsonl"
        result = self.invoke_empty(
            self.sandbox.payload(
                "SessionStart",
                session_id,
                transcript_path=str(missing),
                source="startup",
            )
        )
        self.assertIn("transcript", result.stderr)
        self.assertIn("Git state unavailable", result.stderr)
        capsule = self.sandbox.capsule(session_id)
        self.assertEqual(
            capsule["repository"],
            {"root": None, "revision": None, "dirty_summary": None},
        )

        missing_cwd_id = self.sandbox.session_id()
        result = self.invoke_empty(
            self.sandbox.payload(
                "PreCompact",
                missing_cwd_id,
                cwd=str(self.sandbox.root / "does-not-exist"),
            )
        )
        self.assertIn("Git state unavailable", result.stderr)

    def test_git_state_is_captured_when_available(self) -> None:
        git = shutil.which("git.exe") or shutil.which("git")
        if git is None:
            self.skipTest("Git is not available")
        for args in (
            ["init"],
            ["config", "user.email", "continuity@example.invalid"],
            ["config", "user.name", "Continuity Test"],
        ):
            subprocess.run(
                [git, *args],
                cwd=self.sandbox.root,
                check=True,
                text=True,
                capture_output=True,
            )
        tracked = self.sandbox.root / "tracked.txt"
        tracked.write_text("tracked\n", encoding="utf-8")
        subprocess.run([git, "add", "tracked.txt"], cwd=self.sandbox.root, check=True)
        subprocess.run(
            [git, "commit", "-m", "fixture"],
            cwd=self.sandbox.root,
            check=True,
            text=True,
            capture_output=True,
        )
        session_id = self.sandbox.session_id()
        result = self.invoke_empty(self.sandbox.payload("PreCompact", session_id))
        self.assertNotIn("Git state unavailable", result.stderr)
        repository = self.sandbox.capsule(session_id)["repository"]
        self.assertEqual(
            normalize_path(repository["root"]), normalize_path(self.sandbox.root)
        )
        self.assertGreaterEqual(len(repository["revision"]), 40)
        self.assertIsInstance(repository["dirty_summary"], str)

    def test_corrupt_capsule_and_write_failure_emit_exact_empty_object(self) -> None:
        corrupt_id = self.sandbox.session_id()
        self.sandbox.state.mkdir(parents=True)
        self.sandbox.capsule_path(corrupt_id).write_text("{not-json", encoding="utf-8")
        result = self.invoke_empty(self.sandbox.payload("UserPromptSubmit", corrupt_id))
        self.assertIn("task-continuity:", result.stderr)
        self.assertEqual(
            self.sandbox.capsule_path(corrupt_id).read_text(encoding="utf-8"),
            "{not-json",
        )

        second = HookSandbox(self.shell)
        try:
            harness_path = second.root / ".codex" / "harness"
            harness_path.write_text("blocks directory creation", encoding="utf-8")
            session_id = second.session_id()
            result, _ = second.invoke(second.payload("UserPromptSubmit", session_id))
            self.assert_empty(result)
            self.assertTrue(result.stderr.startswith("task-continuity:"))
            self.assertFalse(second.capsule_path(session_id).exists())
        finally:
            second.close()

    def test_resume_fork_new_and_clear_identity_rules(self) -> None:
        predecessor = self.sandbox.session_id()
        self.invoke_empty(
            self.sandbox.payload(
                "UserPromptSubmit", predecessor, prompt="Preserve predecessor context"
            )
        )
        self.invoke_empty(
            self.sandbox.payload(
                "Stop", predecessor, last_assistant_message="Predecessor result"
            )
        )
        predecessor_path = self.sandbox.capsule_path(predecessor)

        self.invoke_injection(
            self.sandbox.payload("SessionStart", predecessor, source="resume"),
            predecessor,
        )
        predecessor_bytes = predecessor_path.read_bytes()
        predecessor_mtime = predecessor_path.stat().st_mtime_ns

        child = self.sandbox.session_id()
        self.sandbox.transcript.write_text(
            compact_json(
                {
                    "timestamp": "2026-08-01T00:00:00Z",
                    "type": "session_meta",
                    "payload": {
                        "session_id": child,
                        "id": child,
                        "forked_from_id": predecessor,
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )
        self.invoke_injection(
            self.sandbox.payload("SessionStart", child, source="startup"),
            child,
        )
        child_capsule = self.sandbox.capsule(child)
        self.assertEqual(child_capsule["predecessor_thread_id"], predecessor)
        self.assertEqual(
            child_capsule["last_user_request"], "Preserve predecessor context"
        )
        self.assertEqual(child_capsule["last_assistant_result"], "Predecessor result")
        self.assertEqual(predecessor_path.read_bytes(), predecessor_bytes)
        self.assertEqual(predecessor_path.stat().st_mtime_ns, predecessor_mtime)

        new_session = self.sandbox.session_id()
        self.sandbox.transcript.write_text("", encoding="utf-8")
        self.invoke_empty(
            self.sandbox.payload("SessionStart", new_session, source="startup")
        )
        new_capsule = self.sandbox.capsule(new_session)
        self.assertIsNone(new_capsule["predecessor_thread_id"])
        self.assertIsNone(new_capsule["last_user_request"])

        epoch_before = self.sandbox.capsule(child)["continuity_epoch"]
        self.invoke_empty(self.sandbox.payload("SessionStart", child, source="clear"))
        cleared = self.sandbox.capsule(child)
        self.assertEqual(cleared["continuity_epoch"], epoch_before + 1)
        self.assertIsNone(cleared["last_user_request"])
        self.assertIsNone(cleared["last_assistant_result"])
        self.assertIsNone(cleared["predecessor_thread_id"])
        self.invoke_empty(self.sandbox.payload("SessionStart", child, source="resume"))

    def test_pre_and_post_compaction_restore_ordering(self) -> None:
        session_id = self.sandbox.session_id()
        self.invoke_empty(self.sandbox.payload("UserPromptSubmit", session_id))
        self.invoke_empty(self.sandbox.payload("PreCompact", session_id))
        self.assertEqual(self.sandbox.capsule(session_id)["compaction"]["phase"], "pre")
        result = self.invoke_empty(
            self.sandbox.payload("SessionStart", session_id, source="compact")
        )
        self.assertIn("not post-compaction", result.stderr)
        self.invoke_empty(self.sandbox.payload("PostCompact", session_id))
        self.assertEqual(
            self.sandbox.capsule(session_id)["compaction"]["phase"], "post"
        )
        self.invoke_injection(
            self.sandbox.payload("SessionStart", session_id, source="compact"),
            session_id,
        )

    def test_unchanged_stop_preserves_bytes_and_modification_time(self) -> None:
        session_id = self.sandbox.session_id()
        self.invoke_empty(self.sandbox.payload("UserPromptSubmit", session_id))
        stop = self.sandbox.payload(
            "Stop", session_id, last_assistant_message="Stable result"
        )
        self.invoke_empty(stop)
        capsule_path = self.sandbox.capsule_path(session_id)
        before_bytes = capsule_path.read_bytes()
        before_mtime = capsule_path.stat().st_mtime_ns
        stop["turn_id"] = "turn-2"
        self.invoke_empty(stop)
        self.assertEqual(capsule_path.read_bytes(), before_bytes)
        self.assertEqual(capsule_path.stat().st_mtime_ns, before_mtime)

    def test_concurrent_sessions_use_atomic_replacement_without_temp_files(
        self,
    ) -> None:
        session_ids = [self.sandbox.session_id() for _ in range(8)]

        def invoke(session_id: str) -> subprocess.CompletedProcess[str]:
            result, _ = self.sandbox.invoke(
                self.sandbox.payload("UserPromptSubmit", session_id)
            )
            return result

        with ThreadPoolExecutor(max_workers=8) as executor:
            results = list(executor.map(invoke, session_ids))
        for result in results:
            self.assert_empty(result)
        for session_id in session_ids:
            self.assertEqual(self.sandbox.capsule(session_id)["session_id"], session_id)
        leftovers = [
            path.name
            for path in self.sandbox.state.iterdir()
            if path.suffix in {".tmp", ".bak"}
        ]
        self.assertEqual(leftovers, [])

        first = session_ids[0]
        self.invoke_empty(self.sandbox.payload("PostCompact", first, trigger="auto"))
        json.loads(self.sandbox.capsule_path(first).read_text(encoding="utf-8"))
        leftovers = [
            path.name
            for path in self.sandbox.state.iterdir()
            if path.suffix in {".tmp", ".bak"}
        ]
        self.assertEqual(leftovers, [])

    def test_retention_runs_only_on_session_start_and_enforces_age_and_count(
        self,
    ) -> None:
        self.sandbox.state.mkdir(parents=True)
        old_ids = [self.sandbox.session_id(), self.sandbox.session_id()]
        for session_id in old_ids:
            path = self.sandbox.capsule_path(session_id)
            path.write_text("{}", encoding="utf-8")
            old = time.time() - (31 * 24 * 60 * 60)
            os.utime(path, (old, old))
        for _ in range(103):
            self.sandbox.capsule_path(self.sandbox.session_id()).write_text(
                "{}", encoding="utf-8"
            )

        current = self.sandbox.session_id()
        self.invoke_empty(self.sandbox.payload("UserPromptSubmit", current))
        self.assertTrue(
            all(self.sandbox.capsule_path(value).exists() for value in old_ids)
        )
        self.invoke_empty(
            self.sandbox.payload("SessionStart", current, source="startup")
        )
        self.assertTrue(
            all(not self.sandbox.capsule_path(value).exists() for value in old_ids)
        )
        inactive = [
            path
            for path in self.sandbox.state.glob("*.json")
            if path.name != f"{current}.json"
        ]
        self.assertLessEqual(len(inactive), 100)
        self.assertTrue(self.sandbox.capsule_path(current).exists())

    def test_hooks_list_diagnoses_states_without_using_order(self) -> None:
        source = str(MANIFEST)
        entries = [
            {
                "eventName": DISCOVERY_EVENTS["UserPromptSubmit"],
                "sourcePath": source,
                "handlerType": "command",
                "command": PRODUCTION_COMMANDS["UserPromptSubmit"],
                "timeoutSec": 5,
                "source": "project",
                "enabled": False,
                "trustStatus": "trusted",
                "displayOrder": 99,
            },
            {
                "eventName": DISCOVERY_EVENTS["PreCompact"],
                "sourcePath": source,
                "handlerType": "command",
                "command": PRODUCTION_COMMANDS["PreCompact"],
                "timeoutSec": 5,
                "source": "project",
                "enabled": True,
                "trustStatus": "untrusted",
                "displayOrder": 1,
            },
            {
                "eventName": DISCOVERY_EVENTS["PostCompact"],
                "sourcePath": source,
                "handlerType": "command",
                "command": PRODUCTION_COMMANDS["PostCompact"],
                "timeoutSec": 5,
                "source": "project",
                "enabled": True,
                "trustStatus": "modified",
                "displayOrder": 0,
            },
            {
                "eventName": DISCOVERY_EVENTS["SessionStart"],
                "sourcePath": source,
                "handlerType": "command",
                "command": PRODUCTION_COMMANDS["SessionStart"],
                "timeoutSec": 5,
                "source": "project",
                "enabled": True,
                "trustStatus": "trusted",
                "displayOrder": -1,
            },
        ]
        response = {
            "result": {
                "data": [
                    {
                        "cwd": str(REPO_ROOT),
                        "hooks": list(reversed(entries)),
                        "warnings": [],
                        "errors": [],
                    }
                ]
            }
        }
        diagnoses, issues, _ = classify_hooks_list(response)
        self.assertEqual(
            diagnoses,
            {
                "UserPromptSubmit": "disabled",
                "PreCompact": "untrusted",
                "PostCompact": "modified",
                "SessionStart": "trusted",
                "Stop": "missing",
            },
        )
        self.assertEqual(issues, [])


def main() -> int:
    if len(sys.argv) > 1 and sys.argv[1] == "--doctor":
        return run_doctor()
    if len(sys.argv) > 1 and sys.argv[1] == "--benchmark":
        return run_benchmark()
    unittest.main()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
