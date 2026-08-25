#!/usr/bin/env python3
"""Guarded installed-provider to KD4 external-evidence smoke test.

This script intentionally uses only local provider checkouts, a loopback
Responses API server, disposable Git repositories, and disposable Codex homes.
It does not score benchmark cases or create investigation state.
"""

from __future__ import annotations

import argparse
import hashlib
import http.server
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

ARTIFACT_HEADER = b"KD4_EXTERNAL_EVIDENCE_CANONICAL_JSON_STRING_CHUNKS_V1\n"
SMOKE_MODEL = "gpt-5.2-codex"
KDS_CALLABLE_TOOL_ENV = "KDS_INTERNAL_MCP_CALLABLE_TOOL"
KDS_CALLABLE_TOOL_SENTINEL = "kd4-investigation-evidence-smoke-v1"
KDS_DIRECT_DIAGNOSTIC_ENV = "KDS_INTERNAL_MCP_DIRECT_DIAGNOSTIC"
RECEIPT_ID_RE = re.compile(r"^external-evidence-[1-9][0-9]*$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
WINDOWS_ABSOLUTE_PATH_RE = re.compile(
    r"(?i)(?<![A-Za-z0-9_])(?:[A-Z]:[\\/]|\\\\[^\\/\s]+[\\/][^\\/\s]+)"
    r"(?:[^\s\"'<>|]*)"
)
POSIX_ABSOLUTE_PATH_RE = re.compile(
    r"(?<![A-Za-z0-9_:])/(?:[^/\s\"'<>|]+/)*[^/\s\"'<>|]*"
)
WALL_TIME_OUTPUT_RE = re.compile(
    r"^Wall time: [0-9]+(?:\.[0-9]+)? seconds\r?\nOutput:(?:\r?\n)?",
)
FORBIDDEN_INVESTIGATION_FIELDS = {
    "coverage",
    "deferred_risks",
    "hypotheses",
    "refuter",
    "semantic_coverage",
    "stop_decision",
}
PLUGIN_MENTIONS = {
    "kds": "[@kds](plugin://kds@local-kds)",
    "repo-atlas": "[@repo-atlas](plugin://repo-atlas@repo-atlas-local)",
}


class SmokeFailure(RuntimeError):
    """A smoke-test invariant failed."""


class ToolNotAdvertised(SmokeFailure):
    """The requested provider tool has not yet been loaded into the turn."""


class Redactor:
    def __init__(self, paths: list[Path]) -> None:
        resolved: list[str] = []
        for path in paths:
            try:
                value = str(path.resolve())
            except OSError:
                value = str(path.absolute())
            resolved.extend(
                (value, value.replace("\\", "\\\\"), value.replace("\\", "/"))
            )
        self._paths = sorted(set(resolved), key=len, reverse=True)

    def text(self, value: object) -> str:
        text = str(value)
        for path in self._paths:
            text = text.replace(path, "<absolute-path>")
        text = WINDOWS_ABSOLUTE_PATH_RE.sub("<absolute-path>", text)
        text = POSIX_ABSOLUTE_PATH_RE.sub("<absolute-path>", text)
        return text


@dataclass(frozen=True)
class ToolInvocation:
    server_name: str
    tool_name: str
    call_id: str
    arguments: dict[str, Any]

    @property
    def namespace(self) -> str:
        return f"mcp__{self.server_name}"


@dataclass(frozen=True)
class CaseSpec:
    name: str
    provider: str
    producer: str
    expected_server_name: str
    expected_operation: str
    target: ToolInvocation
    calls: tuple[ToolInvocation, ...]
    expected_completeness: str
    expected_truncated: bool
    expected_approximate: bool
    expected_tool_success: bool
    expected_snapshot: str
    expected_model_fields: dict[str, Any]
    expected_content_markers: tuple[str, ...]
    environment: dict[str, str] = field(default_factory=dict)


@dataclass
class Scenario:
    case: CaseSpec
    outputs: dict[str, Any] = field(default_factory=dict)
    search_outputs: dict[str, dict[str, Any]] = field(default_factory=dict)
    searches_requested: set[str] = field(default_factory=set)
    requests: int = 0
    final_responses: int = 0
    failure: str | None = None
    lock: threading.Lock = field(default_factory=threading.Lock)

    def accept(self, body: dict[str, Any]) -> list[dict[str, Any]]:
        with self.lock:
            self.requests += 1
            try:
                self._capture_outputs(body)
                response_id = f"resp-{self.case.name}-{self.requests}"
                missing = next(
                    (
                        call
                        for call in self.case.calls
                        if call.call_id not in self.outputs
                    ),
                    None,
                )
                if missing is not None:
                    try:
                        namespace, name = resolve_wire_tool(body.get("tools"), missing)
                    except ToolNotAdvertised:
                        search_call_id = f"search-{missing.call_id}"
                        if search_call_id in self.search_outputs:
                            try:
                                namespace, name = resolve_wire_tool(
                                    self.search_outputs[search_call_id].get("tools"),
                                    missing,
                                )
                            except ToolNotAdvertised as exc:
                                raise SmokeFailure(
                                    f"tool search did not load "
                                    f"{missing.server_name}/{missing.tool_name}; "
                                    f"current request {advertised_tool_summary(body)}; "
                                    f"{plugin_request_summary(self.case.provider, body)}; "
                                    + advertised_tool_summary(
                                        self.search_outputs[search_call_id]
                                    )
                                ) from exc
                        else:
                            if not tool_search_is_advertised(body.get("tools")):
                                raise SmokeFailure(
                                    f"installed MCP tool "
                                    f"{missing.server_name}/{missing.tool_name} was not advertised "
                                    "and tool_search is unavailable; "
                                    + advertised_tool_summary(body)
                                )
                            self.searches_requested.add(search_call_id)
                            return [
                                response_created(response_id),
                                {
                                    "type": "response.output_item.done",
                                    "item": {
                                        "type": "tool_search_call",
                                        "call_id": search_call_id,
                                        "execution": "client",
                                        "arguments": {
                                            "query": tool_search_query(missing),
                                            "limit": 8,
                                        },
                                    },
                                },
                                response_completed(response_id),
                            ]
                    item: dict[str, Any] = {
                        "type": "function_call",
                        "call_id": missing.call_id,
                        "name": name,
                        "arguments": json.dumps(
                            missing.arguments,
                            ensure_ascii=False,
                            separators=(",", ":"),
                            sort_keys=True,
                        ),
                    }
                    if namespace is not None:
                        item["namespace"] = namespace
                    return [
                        response_created(response_id),
                        {"type": "response.output_item.done", "item": item},
                        response_completed(response_id),
                    ]

                self.final_responses += 1
                return [
                    response_created(response_id),
                    {
                        "type": "response.output_item.done",
                        "item": {
                            "type": "message",
                            "role": "assistant",
                            "id": f"message-{self.case.name}",
                            "content": [
                                {
                                    "type": "output_text",
                                    "text": f"{self.case.name} smoke complete",
                                }
                            ],
                        },
                    },
                    response_completed(response_id),
                ]
            except Exception as exc:
                self.failure = str(exc)
                raise

    def _capture_outputs(self, body: dict[str, Any]) -> None:
        inputs = body.get("input")
        if not isinstance(inputs, list):
            raise SmokeFailure("Responses request input is not an array")
        expected = {call.call_id for call in self.case.calls}
        for item in inputs:
            if not isinstance(item, dict):
                continue
            if item.get("type") != "function_call_output":
                if item.get("type") == "tool_search_output":
                    call_id = item.get("call_id")
                    if call_id in self.searches_requested:
                        self.search_outputs[str(call_id)] = item
                continue
            call_id = item.get("call_id")
            if call_id in expected and call_id not in self.outputs:
                if "output" not in item:
                    raise SmokeFailure(
                        f"tool output {call_id!r} omitted its output value"
                    )
                self.outputs[str(call_id)] = item["output"]

    def assert_finished(self) -> None:
        with self.lock:
            if self.failure is not None:
                raise SmokeFailure(
                    f"loopback Responses scenario failed: {self.failure}"
                )
            missing = [
                call.call_id
                for call in self.case.calls
                if call.call_id not in self.outputs
            ]
            if missing:
                raise SmokeFailure(
                    f"model did not return tool output for {len(missing)} call(s)"
                )
            if self.final_responses < 1:
                raise SmokeFailure("model session did not request its final response")


def response_created(response_id: str) -> dict[str, Any]:
    return {"type": "response.created", "response": {"id": response_id}}


def response_completed(response_id: str) -> dict[str, Any]:
    return {
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
    }


def resolve_wire_tool(
    tools: object,
    invocation: ToolInvocation,
) -> tuple[str | None, str]:
    if not isinstance(tools, list):
        raise ToolNotAdvertised("Responses request did not advertise a tool array")
    for tool in tools:
        if not isinstance(tool, dict):
            continue
        if tool.get("type") == "namespace" and tool.get("name") == invocation.namespace:
            children = tool.get("tools")
            if isinstance(children, list) and any(
                isinstance(child, dict) and child.get("name") == invocation.tool_name
                for child in children
            ):
                return invocation.namespace, invocation.tool_name

    flat_name = f"{invocation.namespace}__{invocation.tool_name}"
    if any(
        isinstance(tool, dict)
        and tool.get("type") == "function"
        and tool.get("name") == flat_name
        for tool in tools
    ):
        return None, flat_name
    raise ToolNotAdvertised(
        f"installed MCP tool {invocation.server_name}/{invocation.tool_name} is deferred"
    )


def tool_search_is_advertised(tools: object) -> bool:
    return isinstance(tools, list) and any(
        isinstance(tool, dict) and tool.get("type") == "tool_search" for tool in tools
    )


def tool_search_query(invocation: ToolInvocation) -> str:
    queries = {
        ("kds", "KDS"): "KDS compact noisy diagnostic command output",
        ("repo_atlas", "select_root"): "Repo Atlas select repository root",
        ("repo_atlas", "find_def"): "Repo Atlas find exact symbol definition",
        ("repo_atlas", "trace"): "Repo Atlas approximate symbol trace",
    }
    return queries.get(
        (invocation.server_name, invocation.tool_name),
        f"{invocation.server_name} {invocation.tool_name}",
    )


def advertised_tool_summary(body: dict[str, Any]) -> str:
    tools = body.get("tools")
    if not isinstance(tools, list):
        return (
            f"request keys={','.join(sorted(body))}; tools type={type(tools).__name__}"
        )
    identities: list[str] = []
    for tool in tools:
        if not isinstance(tool, dict):
            identities.append(type(tool).__name__)
            continue
        identities.append(f"{tool.get('type')}:{tool.get('name', '')}")
    return f"advertised tools={','.join(identities)}"


def plugin_request_summary(provider: str, body: dict[str, Any]) -> str:
    serialized_input = json.dumps(body.get("input"), ensure_ascii=True, sort_keys=True)
    plugin_uri = PLUGIN_MENTIONS[provider].split("(", 1)[1][:-1]
    plugin_skill = {
        "kds": "kds:kds",
        "repo-atlas": "repo-atlas:repo-atlas",
    }[provider]
    return (
        f"plugin_uri_present={plugin_uri in serialized_input}; "
        f"plugin_skill_present={plugin_skill in serialized_input}; "
        "explicit_plugin_guidance_present="
        f"{'Skills from this plugin' in serialized_input}; "
        "explicit_plugin_mcp_present="
        f"{'MCP servers from this plugin available in this session' in serialized_input}"
    )


class LoopbackResponsesServer(http.server.ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = False

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), LoopbackResponsesHandler)
        self._scenario_lock = threading.Lock()
        self._scenario: Scenario | None = None

    @property
    def base_url(self) -> str:
        host, port = self.server_address
        return f"http://{host}:{port}/v1"

    def activate(self, scenario: Scenario) -> None:
        with self._scenario_lock:
            if self._scenario is not None:
                raise SmokeFailure("a loopback Responses scenario is already active")
            self._scenario = scenario

    def current_scenario(self) -> Scenario:
        with self._scenario_lock:
            if self._scenario is None:
                raise SmokeFailure(
                    "loopback Responses request arrived without an active case"
                )
            return self._scenario

    def release(self, scenario: Scenario) -> None:
        with self._scenario_lock:
            if self._scenario is not scenario:
                raise SmokeFailure(
                    "loopback Responses scenario ownership changed unexpectedly"
                )
            self._scenario = None


class LoopbackResponsesHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server: LoopbackResponsesServer

    def do_POST(self) -> None:
        try:
            if self.path.split("?", 1)[0] != "/v1/responses":
                self.send_error(404)
                return
            encoding = self.headers.get("content-encoding", "identity").strip().lower()
            if encoding not in {"", "identity"}:
                raise SmokeFailure(f"unexpected request content encoding {encoding!r}")
            raw = self._read_request_body()
            body = json.loads(raw)
            if not isinstance(body, dict):
                raise SmokeFailure("Responses request body is not an object")
            events = self.server.current_scenario().accept(body)
            payload = encode_sse(events)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(payload)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(payload)
            self.wfile.flush()
        except Exception as exc:  # noqa: BLE001 - return a deterministic local failure
            scenario = None
            try:
                scenario = self.server.current_scenario()
            except SmokeFailure:
                pass
            if scenario is not None:
                with scenario.lock:
                    scenario.failure = str(exc)
            payload = json.dumps(
                {"error": {"message": "local smoke response failed"}},
                separators=(",", ":"),
            ).encode("utf-8")
            self.send_response(500)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(payload)

    def _read_request_body(self) -> bytes:
        transfer_encoding = self.headers.get("transfer-encoding", "").lower()
        if transfer_encoding == "chunked":
            chunks: list[bytes] = []
            while True:
                line = self.rfile.readline(128)
                if not line:
                    raise SmokeFailure("truncated chunked request")
                size_text = line.split(b";", 1)[0].strip()
                size = int(size_text, 16)
                if size == 0:
                    while self.rfile.readline(8192) not in {b"\r\n", b"\n", b""}:
                        pass
                    return b"".join(chunks)
                chunks.append(self.rfile.read(size))
                if self.rfile.read(2) != b"\r\n":
                    raise SmokeFailure("invalid chunk terminator")
        length_text = self.headers.get("content-length")
        if length_text is None:
            raise SmokeFailure("Responses request omitted Content-Length")
        length = int(length_text)
        if length < 0 or length > 32 * 1024 * 1024:
            raise SmokeFailure("Responses request length is outside the smoke bound")
        raw = self.rfile.read(length)
        if len(raw) != length:
            raise SmokeFailure("truncated Responses request body")
        return raw

    def log_message(self, _format: str, *_args: object) -> None:
        return


def encode_sse(events: list[dict[str, Any]]) -> bytes:
    parts: list[str] = []
    for event in events:
        event_type = event.get("type")
        if not isinstance(event_type, str):
            raise SmokeFailure("SSE event omitted its type")
        parts.append(f"event: {event_type}\n")
        parts.append(
            "data: "
            + json.dumps(event, ensure_ascii=False, separators=(",", ":"))
            + "\n\n"
        )
    return "".join(parts).encode("utf-8")


def run_command(
    argv: list[str],
    *,
    cwd: Path,
    env: dict[str, str],
    timeout: float,
    label: str,
    redactor: Redactor,
) -> subprocess.CompletedProcess[str]:
    creationflags = getattr(subprocess, "CREATE_NO_WINDOW", 0)
    try:
        completed = subprocess.run(
            argv,
            cwd=cwd,
            env=env,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout,
            check=False,
            creationflags=creationflags,
        )
    except subprocess.TimeoutExpired as exc:
        raise SmokeFailure(f"{label} timed out after {timeout:g} seconds") from exc
    except OSError as exc:
        raise SmokeFailure(f"{label} could not start: {redactor.text(exc)}") from exc
    if completed.returncode != 0:
        combined = (completed.stderr + "\n" + completed.stdout).strip().splitlines()
        tail = "\n".join(combined[-12:])
        raise SmokeFailure(
            f"{label} exited {completed.returncode}: {redactor.text(tail)}"
        )
    return completed


def git_output(
    provider_root: Path,
    args: list[str],
    *,
    env: dict[str, str],
    redactor: Redactor,
) -> bytes:
    creationflags = getattr(subprocess, "CREATE_NO_WINDOW", 0)
    try:
        completed = subprocess.run(
            ["git", "-C", str(provider_root), *args],
            env=env,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            timeout=60,
            check=False,
            creationflags=creationflags,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise SmokeFailure(
            f"provider Git inspection failed: {redactor.text(exc)}"
        ) from exc
    if completed.returncode != 0:
        raise SmokeFailure(
            "provider Git inspection failed: "
            + redactor.text(completed.stderr.decode("utf-8", errors="replace"))
        )
    return completed.stdout


def provider_source_paths(
    provider_root: Path,
    *,
    env: dict[str, str],
    redactor: Redactor,
) -> list[Path]:
    output = git_output(
        provider_root,
        ["ls-files", "--cached", "--others", "--exclude-standard", "-z"],
        env=env,
        redactor=redactor,
    )
    paths: list[Path] = []
    for raw in output.split(b"\0"):
        if not raw:
            continue
        relative = Path(os.fsdecode(raw))
        if relative.is_absolute() or ".." in relative.parts:
            raise SmokeFailure("provider Git manifest returned an unsafe source path")
        paths.append(relative)
    if not paths:
        raise SmokeFailure("provider checkout has no source files")
    return sorted(paths, key=lambda path: path.as_posix())


def is_reparse_or_link(path: Path) -> bool:
    metadata = path.lstat()
    attributes = getattr(metadata, "st_file_attributes", 0)
    reparse_flag = getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400)
    return path.is_symlink() or bool(attributes & reparse_flag)


def source_fingerprint(
    provider_root: Path,
    *,
    env: dict[str, str],
    redactor: Redactor,
) -> str:
    digest = hashlib.sha256()
    for relative in provider_source_paths(provider_root, env=env, redactor=redactor):
        raw = os.fsencode(relative)
        path = provider_root / relative
        digest.update(raw)
        digest.update(b"\0")
        if not path.exists():
            digest.update(b"<missing>")
            continue
        if is_reparse_or_link(path) or not path.is_file():
            raise SmokeFailure(
                "provider source fingerprint encountered a non-regular file"
            )
        with path.open("rb") as handle:
            while chunk := handle.read(1024 * 1024):
                digest.update(chunk)
        digest.update(b"\0")
    status = git_output(
        provider_root,
        ["status", "--porcelain=v2", "--untracked-files=all", "-z"],
        env=env,
        redactor=redactor,
    )
    digest.update(status)
    return digest.hexdigest()


def stage_provider(
    provider_root: Path,
    destination: Path,
    *,
    env: dict[str, str],
    redactor: Redactor,
) -> None:
    destination.mkdir(parents=True, exist_ok=False)
    for relative in provider_source_paths(provider_root, env=env, redactor=redactor):
        source = provider_root / relative
        if not source.exists():
            continue
        if is_reparse_or_link(source) or not source.is_file():
            raise SmokeFailure(
                "provider staging rejects links, reparses, and non-regular files"
            )
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)


def isolated_environment(home: Path, temp_root: Path) -> dict[str, str]:
    environment = os.environ.copy()
    scratch = home / "tmp"
    scratch.mkdir(parents=True, exist_ok=True)
    environment.update(
        {
            "ALL_PROXY": "http://127.0.0.1:9",
            "CODEX_HOME": str(home),
            "CODEX_SQLITE_HOME": str(home / "sqlite"),
            "GIT_CONFIG_NOSYSTEM": "1",
            "HOME": str(home),
            "HTTP_PROXY": "http://127.0.0.1:9",
            "HTTPS_PROXY": "http://127.0.0.1:9",
            "KDS_HOME": str(home / "kds-home"),
            "KD4_SMOKE_TEMP_ROOT": str(temp_root),
            "NO_COLOR": "1",
            "NO_PROXY": "127.0.0.1,localhost",
            "TEMP": str(scratch),
            "TMP": str(scratch),
            "USERPROFILE": str(home),
        }
    )
    for key in (
        "OPENAI_API_KEY",
        "AZURE_OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
    ):
        environment.pop(key, None)
    return environment


def install_provider(
    provider: str,
    staged_root: Path,
    codex: Path,
    home: Path,
    *,
    env: dict[str, str],
    redactor: Redactor,
) -> None:
    if provider == "kds":
        lifecycle = staged_root / "scripts" / "plugin_lifecycle.py"
        run_command(
            [
                sys.executable,
                "-B",
                str(lifecycle),
                "install",
                "--codex",
                str(codex),
                "--codex-home",
                str(home),
            ],
            cwd=staged_root,
            env=env,
            timeout=900,
            label="KDS local lifecycle install",
            redactor=redactor,
        )
        run_command(
            [
                sys.executable,
                "-B",
                str(lifecycle),
                "status",
                "--codex",
                str(codex),
                "--codex-home",
                str(home),
            ],
            cwd=staged_root,
            env=env,
            timeout=120,
            label="KDS local lifecycle status",
            redactor=redactor,
        )
        return

    if provider == "repo-atlas":
        marketplace = "repo-atlas-local"
        selector = "repo-atlas@repo-atlas-local"
    else:
        raise SmokeFailure(f"unknown provider {provider!r}")

    manifest = staged_root / ".agents" / "plugins" / "marketplace.json"
    value = json.loads(manifest.read_text(encoding="utf-8"))
    if value.get("name") != marketplace:
        raise SmokeFailure(f"{provider} staged marketplace identity changed")
    plugins = value.get("plugins")
    if not isinstance(plugins, list) or not plugins:
        raise SmokeFailure(f"{provider} staged marketplace has no plugins")
    if any(
        not isinstance(plugin, dict)
        or not isinstance(plugin.get("source"), dict)
        or plugin["source"].get("source") != "local"
        for plugin in plugins
    ):
        raise SmokeFailure(
            f"{provider} smoke lifecycle accepts only local plugin sources"
        )

    run_command(
        [
            str(codex),
            "plugin",
            "marketplace",
            "add",
            str(staged_root),
            "--json",
        ],
        cwd=staged_root,
        env=env,
        timeout=180,
        label=f"{provider} local marketplace add",
        redactor=redactor,
    )
    run_command(
        [str(codex), "plugin", "add", selector, "--json"],
        cwd=staged_root,
        env=env,
        timeout=300,
        label=f"{provider} local plugin install",
        redactor=redactor,
    )


def initialize_git_repo(
    path: Path,
    files: dict[str, str],
    *,
    env: dict[str, str],
    redactor: Redactor,
) -> None:
    path.mkdir(parents=True, exist_ok=False)
    for relative_text, content in files.items():
        relative = Path(relative_text)
        if relative.is_absolute() or ".." in relative.parts:
            raise SmokeFailure("fixture contains an unsafe path")
        target = path / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8", newline="\n")
    commands = (
        ["git", "init", "--quiet"],
        ["git", "config", "user.name", "KD4 Investigation Smoke"],
        ["git", "config", "user.email", "kd4-smoke.invalid"],
        ["git", "add", "--all"],
        ["git", "commit", "--quiet", "-m", "disposable smoke fixture"],
    )
    for index, command in enumerate(commands, start=1):
        run_command(
            command,
            cwd=path,
            env=env,
            timeout=60,
            label=f"disposable Git setup step {index}",
            redactor=redactor,
        )


def kds_fixture(noisy: bool) -> dict[str, str]:
    if not noisy:
        return {
            "test_diagnostic.py": (
                "import unittest\n\n"
                "class BoundedDiagnostic(unittest.TestCase):\n"
                "    def test_bounded_success(self):\n"
                "        self.assertEqual(2 + 2, 4)\n\n"
                "if __name__ == '__main__':\n"
                "    unittest.main()\n"
            )
        }
    return {
        "test_diagnostic.py": (
            "import os\n"
            "import unittest\n\n"
            "class NoisyDiagnostic(unittest.TestCase):\n"
            "    def test_deliberately_incomplete_failure(self):\n"
            "        for index in range(6000):\n"
            "            print(\n"
            "                f'NOISE-KDS-{index:05d} ' + ('safe diagnostic padding ' * 5),\n"
            "                flush=True,\n"
            "            )\n"
            "        os._exit(3)\n\n"
            "if __name__ == '__main__':\n"
            "    unittest.main()\n"
        )
    }


def repo_atlas_fixture() -> dict[str, str]:
    return {
        "package.json": '{"name":"repo-atlas-smoke","private":true,"type":"module"}\n',
        "src/index.ts": (
            "export function target(value: number): number {\n"
            "  return value + 1;\n"
            "}\n\n"
            "export function start(value: number): number {\n"
            "  return target(value);\n"
            "}\n\n"
            "export function unrelated(): number {\n"
            "  return 0;\n"
            "}\n"
        ),
        "tsconfig.json": (
            '{"compilerOptions":{"target":"ES2022","module":"ESNext","strict":true},'
            '"include":["src/**/*.ts"]}\n'
        ),
    }


def make_case(
    provider: str,
    ordinal: int,
    repo: Path,
) -> tuple[CaseSpec, dict[str, str]]:
    if provider == "kds" and ordinal == 0:
        target = ToolInvocation(
            "kds",
            "KDS",
            "kds-bounded-call",
            {
                "command": ["python", "-m", "unittest", "-q", "test_diagnostic.py"],
                "cwd": str(repo),
            },
        )
        return (
            CaseSpec(
                name="kds-bounded",
                provider=provider,
                producer="kds",
                expected_server_name="kds",
                expected_operation="compact",
                target=target,
                calls=(target,),
                expected_completeness="complete",
                expected_truncated=False,
                expected_approximate=False,
                expected_tool_success=True,
                expected_snapshot="none",
                expected_model_fields={"exitCode": 0, "omittedBytes": 0},
                expected_content_markers=("KDS completed with exit code 0.",),
                environment={KDS_CALLABLE_TOOL_ENV: KDS_CALLABLE_TOOL_SENTINEL},
            ),
            kds_fixture(noisy=False),
        )
    if provider == "kds" and ordinal == 1:
        target = ToolInvocation(
            "kds",
            "KDS",
            "kds-noisy-call",
            {
                "command": ["python", "-m", "unittest", "-q", "test_diagnostic.py"],
                "cwd": str(repo),
            },
        )
        return (
            CaseSpec(
                name="kds-noisy-partial",
                provider=provider,
                producer="kds",
                expected_server_name="kds",
                expected_operation="compact",
                target=target,
                calls=(target,),
                expected_completeness="partial",
                expected_truncated=True,
                expected_approximate=False,
                expected_tool_success=False,
                expected_snapshot="none",
                expected_model_fields={},
                expected_content_markers=("NOISE-KDS-000",),
                environment={
                    KDS_CALLABLE_TOOL_ENV: KDS_CALLABLE_TOOL_SENTINEL,
                    KDS_DIRECT_DIAGNOSTIC_ENV: KDS_CALLABLE_TOOL_SENTINEL,
                },
            ),
            kds_fixture(noisy=True),
        )
    if provider == "repo-atlas" and ordinal in {0, 1}:
        select = ToolInvocation(
            "repo_atlas",
            "select_root",
            "repo-atlas-select-find" if ordinal == 0 else "repo-atlas-select-trace",
            {"root": str(repo)},
        )
        if ordinal == 0:
            target = ToolInvocation(
                "repo_atlas",
                "find_def",
                "repo-atlas-find-def-call",
                {"name": "target", "lang": "ts"},
            )
            case = CaseSpec(
                name="repo-atlas-find-def",
                provider=provider,
                producer="repo-atlas",
                expected_server_name="repo-atlas",
                expected_operation="find_def",
                target=target,
                calls=(select, target),
                expected_completeness="unknown",
                expected_truncated=False,
                expected_approximate=False,
                expected_tool_success=True,
                expected_snapshot="sha256",
                expected_model_fields={},
                expected_content_markers=("## find_def: target", "src/index.ts"),
            )
        else:
            target = ToolInvocation(
                "repo_atlas",
                "trace",
                "repo-atlas-trace-call",
                {"from": "start", "to": "target", "maxDepth": 3, "maxNodes": 20},
            )
            case = CaseSpec(
                name="repo-atlas-trace",
                provider=provider,
                producer="repo-atlas",
                expected_server_name="repo-atlas",
                expected_operation="trace",
                target=target,
                calls=(select, target),
                expected_completeness="partial",
                expected_truncated=False,
                expected_approximate=True,
                expected_tool_success=True,
                expected_snapshot="sha256",
                expected_model_fields={},
                expected_content_markers=("## trace: start", "approximate"),
            )
        return case, repo_atlas_fixture()
    raise SmokeFailure(f"unsupported provider case {provider}[{ordinal}]")


def model_provider_override(base_url: str) -> str:
    escaped = base_url.replace("\\", "\\\\").replace("'", "\\'")
    return (
        "model_providers.investigation-smoke={ "
        "name = 'Investigation smoke', "
        f"base_url = '{escaped}', "
        "wire_api = 'responses', "
        "request_max_retries = 0, "
        "stream_max_retries = 0, "
        "stream_idle_timeout_ms = 30000 "
        "}"
    )


def run_codex_case(
    case: CaseSpec,
    repo: Path,
    codex: Path,
    home: Path,
    server: LoopbackResponsesServer,
    *,
    base_env: dict[str, str],
    redactor: Redactor,
) -> tuple[str, Any]:
    scenario = Scenario(case)
    server.activate(scenario)
    environment = dict(base_env)
    environment.update(case.environment)
    prompt = (
        f"Use {PLUGIN_MENTIONS[case.provider]} and execute the supplied local smoke "
        "operation exactly. "
        "The test model controls the tool call; do not infer completion from it."
    )
    try:
        try:
            completed = run_command(
                [
                    str(codex),
                    "-c",
                    model_provider_override(server.base_url),
                    "-c",
                    "model_provider='investigation-smoke'",
                    "-c",
                    "enable_request_compression=false",
                    "--model",
                    SMOKE_MODEL,
                    "--dangerously-bypass-approvals-and-sandbox",
                    "--dangerously-bypass-hook-trust",
                    "--cd",
                    str(repo),
                    "exec",
                    "--json",
                    "--color",
                    "never",
                    prompt,
                ],
                cwd=repo,
                env=environment,
                timeout=300,
                label=f"{case.name} KD4 session",
                redactor=redactor,
            )
        except SmokeFailure as exc:
            if scenario.failure is not None:
                raise SmokeFailure(
                    f"{case.name} loopback Responses failure: {scenario.failure}; "
                    f"session: {redactor.text(exc)}"
                ) from exc
            raise
    finally:
        server.release(scenario)
    scenario.assert_finished()
    thread_id = parse_thread_id(completed.stdout)
    target_output = scenario.outputs.get(case.target.call_id)
    if target_output is None:
        raise SmokeFailure(f"{case.name} target output was not model-visible")
    return thread_id, target_output


def parse_thread_id(jsonl: str) -> str:
    thread_ids: list[str] = []
    for line in jsonl.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(event, dict) and event.get("type") == "thread.started":
            value = event.get("thread_id")
            if isinstance(value, str):
                thread_ids.append(value)
    if len(thread_ids) != 1:
        raise SmokeFailure(f"KD4 JSONL exposed {len(thread_ids)} thread IDs")
    try:
        parsed = uuid.UUID(thread_ids[0])
    except ValueError as exc:
        raise SmokeFailure("KD4 thread ID is not a UUID") from exc
    if str(parsed) != thread_ids[0].lower():
        raise SmokeFailure("KD4 thread ID is not canonical")
    return thread_ids[0]


def model_output_text(output: Any) -> str:
    if isinstance(output, str):
        return output
    if isinstance(output, list):
        texts = [
            item.get("text")
            for item in output
            if isinstance(item, dict)
            and item.get("type") == "input_text"
            and isinstance(item.get("text"), str)
        ]
        if texts:
            return "\n".join(texts)
    if isinstance(output, dict) and isinstance(output.get("content"), str):
        return output["content"]
    raise SmokeFailure("model-visible MCP output has an unsupported shape")


def parse_model_structured_output(
    case: CaseSpec,
    output: Any,
    redactor: Redactor,
) -> dict[str, Any] | None:
    text = model_output_text(output)
    if not text.strip():
        raise SmokeFailure(f"{case.name} model-visible output is empty")
    match = WALL_TIME_OUTPUT_RE.match(text)
    if match is None:
        raise SmokeFailure(
            f"{case.name} model-visible output omitted its wall-time wrapper"
        )
    payload_text = text[match.end() :]
    try:
        value = json.loads(payload_text)
    except json.JSONDecodeError:
        if case.name != "kds-noisy-partial":
            raise SmokeFailure(
                f"{case.name} model-visible structured result is not JSON"
            )
        required_fragments = (
            '"evidenceMeta"',
            '"payloadCompleteness":"partial"',
            '"truncated":true',
        )
        if not all(fragment in payload_text for fragment in required_fragments):
            raise SmokeFailure(
                "KDS noisy model-visible truncation omitted required structured metadata"
            )
        return None
    if not isinstance(value, dict):
        raise SmokeFailure(
            f"{case.name} model-visible structured result is not an object"
        )
    evidence_meta = value.get("evidenceMeta")
    if not isinstance(evidence_meta, dict):
        raise SmokeFailure(f"{case.name} model-visible result omitted evidenceMeta")
    try:
        assert_metadata(case, evidence_meta)
    except SmokeFailure as exc:
        report = value.get("report")
        report_chars = len(report) if isinstance(report, str) else None
        raise SmokeFailure(
            f"{exc}; omittedBytes={value.get('omittedBytes')!r}; "
            f"report_chars={report_chars!r}; "
            f"limitations={evidence_meta.get('limitations')!r}"
        ) from exc
    for key, expected in case.expected_model_fields.items():
        if value.get(key) != expected:
            report = (
                redactor.text(value.get("report", ""))
                .replace("\r", " ")
                .replace("\n", " ")
            )
            raise SmokeFailure(
                f"{case.name} model-visible field {key!r} was {value.get(key)!r}, "
                f"expected {expected!r}; report={report[:300]!r}"
            )
    return value


def assert_metadata(case: CaseSpec, metadata: dict[str, Any]) -> None:
    expected = {
        "schemaVersion": 1,
        "producer": case.producer,
        "operation": case.expected_operation,
        "evidenceBearing": True,
        "payloadCompleteness": case.expected_completeness,
        "truncated": case.expected_truncated,
        "approximate": case.expected_approximate,
    }
    for key, value in expected.items():
        if metadata.get(key) != value:
            raise SmokeFailure(
                f"{case.name} evidenceMeta {key!r} was {metadata.get(key)!r}, "
                f"expected {value!r}"
            )
    limitations = metadata.get("limitations")
    if not isinstance(limitations, list) or not all(
        isinstance(item, str) and item.strip() for item in limitations
    ):
        raise SmokeFailure(f"{case.name} evidenceMeta limitations are invalid")
    snapshot = metadata.get("snapshot")
    if case.expected_snapshot == "none" and snapshot is not None:
        raise SmokeFailure(f"{case.name} unexpectedly claimed a provider snapshot")
    if case.expected_snapshot == "present" and not (
        isinstance(snapshot, str) and snapshot.strip()
    ):
        raise SmokeFailure(f"{case.name} omitted its provider snapshot")
    if case.expected_snapshot == "sha256" and not (
        isinstance(snapshot, str) and SHA256_RE.fullmatch(snapshot)
    ):
        raise SmokeFailure(f"{case.name} snapshot is not a content SHA-256")


def load_evidence_document(home: Path, thread_id: str) -> dict[str, Any]:
    path = home / "task-evidence" / f"{thread_id}.json"
    deadline = time.monotonic() + 10
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
            if isinstance(value, dict) and value.get("external_evidence"):
                return value
        except (OSError, json.JSONDecodeError) as exc:
            last_error = exc
        time.sleep(0.05)
    if last_error is not None:
        raise SmokeFailure(f"task-evidence reload failed: {last_error}") from last_error
    raise SmokeFailure("task-evidence reload found no external evidence receipt")


def canonical_json_bytes(value: Any) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=False,
        separators=(",", ":"),
        sort_keys=True,
    ).encode("utf-8")


def decode_artifact(home: Path, thread_id: str, artifact_id: str) -> bytes:
    try:
        parsed = uuid.UUID(artifact_id)
    except ValueError as exc:
        raise SmokeFailure("external evidence artifact ID is not a UUID") from exc
    if str(parsed) != artifact_id.lower():
        raise SmokeFailure("external evidence artifact ID is not canonical")
    path = home / "tool-output" / thread_id / f"{artifact_id}.log"
    raw = path.read_bytes()
    if not raw.startswith(ARTIFACT_HEADER):
        raise SmokeFailure("external evidence artifact header is invalid")
    chunks: list[str] = []
    for line in raw[len(ARTIFACT_HEADER) :].splitlines():
        if not line:
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError as exc:
            raise SmokeFailure(
                "external evidence artifact chunk is invalid JSON"
            ) from exc
        if not isinstance(value, str):
            raise SmokeFailure("external evidence artifact chunk is not a JSON string")
        chunks.append(value)
    if not chunks:
        raise SmokeFailure("external evidence artifact has no canonical payload chunks")
    return "".join(chunks).encode("utf-8")


def verify_receipt(
    case: CaseSpec,
    document: dict[str, Any],
    home: Path,
    thread_id: str,
    model_structured: dict[str, Any] | None,
) -> str:
    receipts = document.get("external_evidence")
    if not isinstance(receipts, list) or len(receipts) != 1:
        raise SmokeFailure(
            f"{case.name} expected one external evidence receipt, found "
            f"{len(receipts) if isinstance(receipts, list) else 0}"
        )
    receipt = receipts[0]
    if not isinstance(receipt, dict):
        raise SmokeFailure(f"{case.name} external evidence receipt is not an object")
    if not isinstance(receipt.get("id"), str) or not RECEIPT_ID_RE.fullmatch(
        receipt["id"]
    ):
        raise SmokeFailure(f"{case.name} external evidence receipt ID is invalid")
    expected = {
        "producer": case.producer,
        "producer_schema_version": 1,
        "server_name": case.expected_server_name,
        "tool_name": case.target.tool_name,
        "call_id": case.target.call_id,
        "payload_completeness": case.expected_completeness,
        "truncated": case.expected_truncated,
        "approximate": case.expected_approximate,
        "tool_success": case.expected_tool_success,
    }
    for key, value in expected.items():
        if receipt.get(key) != value:
            raise SmokeFailure(
                f"{case.name} receipt field {key!r} was {receipt.get(key)!r}, "
                f"expected {value!r}"
            )

    snapshot = receipt.get("provider_snapshot")
    if case.expected_snapshot == "none" and snapshot is not None:
        raise SmokeFailure(f"{case.name} receipt unexpectedly retained a snapshot")
    if case.expected_snapshot == "present" and not (
        isinstance(snapshot, str) and snapshot.strip()
    ):
        raise SmokeFailure(f"{case.name} receipt omitted its provider snapshot")
    if case.expected_snapshot == "sha256" and not (
        isinstance(snapshot, str) and SHA256_RE.fullmatch(snapshot)
    ):
        raise SmokeFailure(f"{case.name} receipt snapshot is not a content SHA-256")

    payload = receipt.get("payload")
    artifact_id = receipt.get("payload_artifact_id")
    if not isinstance(payload, dict):
        raise SmokeFailure(
            f"{case.name} receipt omitted its retained payload or summary"
        )
    if artifact_id is None:
        canonical = canonical_json_bytes(payload)
        retention = "inline"
    else:
        if not isinstance(artifact_id, str):
            raise SmokeFailure(f"{case.name} receipt artifact ID is invalid")
        artifact = payload.get("artifact")
        if not isinstance(artifact, dict):
            raise SmokeFailure(
                f"{case.name} externalized payload omitted its artifact summary"
            )
        if artifact.get("id") != artifact_id:
            raise SmokeFailure(
                f"{case.name} artifact summary ID does not match its receipt"
            )
        if artifact.get("encoding") != ARTIFACT_HEADER.decode("ascii").strip():
            raise SmokeFailure(f"{case.name} artifact summary encoding is invalid")
        summary = payload.get("evidenceMetaSummary")
        if not isinstance(summary, dict):
            raise SmokeFailure(
                f"{case.name} externalized payload omitted evidence metadata"
            )
        if (
            summary.get("producer") != case.producer
            or summary.get("schemaVersion") != 1
            or summary.get("payloadCompleteness") != case.expected_completeness
        ):
            raise SmokeFailure(f"{case.name} externalized payload summary changed")
        canonical = decode_artifact(home, thread_id, artifact_id)
        retention = "artifact"
        try:
            payload = json.loads(canonical)
        except json.JSONDecodeError as exc:
            raise SmokeFailure(
                f"{case.name} retained canonical payload is invalid JSON"
            ) from exc

    result_sha256 = receipt.get("result_sha256")
    if not isinstance(result_sha256, str) or not SHA256_RE.fullmatch(result_sha256):
        raise SmokeFailure(f"{case.name} receipt result hash is invalid")
    actual_sha256 = hashlib.sha256(canonical).hexdigest()
    if actual_sha256 != result_sha256:
        raise SmokeFailure(
            f"{case.name} retained payload hash does not match its receipt"
        )
    if canonical != canonical_json_bytes(payload):
        raise SmokeFailure(f"{case.name} artifact bytes are not canonical JSON")
    if not isinstance(payload, dict):
        raise SmokeFailure(f"{case.name} retained result payload is not an object")
    if set(payload) != {"content", "isError", "structuredContent"}:
        raise SmokeFailure(f"{case.name} retained result omitted a canonical MCP field")
    is_error = payload["isError"]
    if (
        case.expected_tool_success
        and is_error not in (None, False)
        or not case.expected_tool_success
        and is_error is not True
    ):
        raise SmokeFailure(f"{case.name} retained MCP isError semantics changed")

    structured = payload.get("structuredContent")
    if not isinstance(structured, dict):
        raise SmokeFailure(f"{case.name} retained result omitted structuredContent")
    metadata = structured.get("evidenceMeta")
    if not isinstance(metadata, dict):
        raise SmokeFailure(f"{case.name} retained result omitted evidenceMeta")
    assert_metadata(case, metadata)
    for key, expected_value in case.expected_model_fields.items():
        if structured.get(key) != expected_value:
            raise SmokeFailure(f"{case.name} retained structured field {key!r} changed")
    if (
        model_structured is not None
        and case.expected_completeness == "complete"
        and model_structured != structured
    ):
        raise SmokeFailure(
            f"{case.name} model-visible structured result differs from retained MCP data"
        )

    content = payload.get("content")
    if not isinstance(content, list):
        raise SmokeFailure(f"{case.name} retained MCP content is not an array")
    text = "\n".join(
        item.get("text", "")
        for item in content
        if isinstance(item, dict)
        and item.get("type") == "text"
        and isinstance(item.get("text"), str)
    )
    if not text:
        raise SmokeFailure(f"{case.name} retained provider text is empty")
    for marker in case.expected_content_markers:
        if marker not in text:
            raise SmokeFailure(
                f"{case.name} retained provider text lost marker {marker!r}"
            )
    return retention


def verify_evidence_only_state(document: dict[str, Any]) -> None:
    if document.get("completion") is not None:
        raise SmokeFailure("evidence-only session created completion state")
    if document.get("plan") != []:
        raise SmokeFailure("evidence-only session created a KD4 completion plan")
    present = FORBIDDEN_INVESTIGATION_FIELDS.intersection(document)
    if present:
        raise SmokeFailure(
            "provider created deferred investigation state: "
            + ", ".join(sorted(present))
        )


def find_temp_processes(temp_root: Path, env: dict[str, str]) -> list[dict[str, Any]]:
    shell = shutil.which("pwsh", path=env.get("PATH")) or shutil.which(
        "powershell", path=env.get("PATH")
    )
    if shell is None:
        raise SmokeFailure("PowerShell is required to verify plugin-process cleanup")
    script = (
        "$needle = $env:KD4_SMOKE_TEMP_ROOT\n"
        "$items = Get-CimInstance Win32_Process | Where-Object {\n"
        "  $_.ProcessId -ne $PID -and $_.CommandLine -and "
        "$_.CommandLine.IndexOf($needle, [StringComparison]::OrdinalIgnoreCase) -ge 0\n"
        "} | ForEach-Object { [pscustomobject]@{ pid = [int]$_.ProcessId; name = $_.Name } }\n"
        "@($items) | ConvertTo-Json -Compress\n"
    )
    query_env = dict(env)
    query_env["KD4_SMOKE_TEMP_ROOT"] = str(temp_root)
    creationflags = getattr(subprocess, "CREATE_NO_WINDOW", 0)
    try:
        completed = subprocess.run(
            [shell, "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", script],
            env=query_env,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=30,
            check=False,
            creationflags=creationflags,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise SmokeFailure("plugin-process cleanup inspection failed") from exc
    if completed.returncode != 0:
        raise SmokeFailure("plugin-process cleanup inspection failed")
    value = json.loads(completed.stdout or "[]")
    if isinstance(value, dict):
        value = [value]
    if not isinstance(value, list):
        raise SmokeFailure("plugin-process cleanup inspection returned invalid data")
    return [
        item
        for item in value
        if isinstance(item, dict)
        and isinstance(item.get("pid"), int)
        and item["pid"] > 0
    ]


def cleanup_temp_processes(temp_root: Path, env: dict[str, str]) -> int:
    processes = find_temp_processes(temp_root, env)
    if not processes:
        return 0
    shell = shutil.which("pwsh", path=env.get("PATH")) or shutil.which(
        "powershell", path=env.get("PATH")
    )
    if shell is None:
        raise SmokeFailure("PowerShell is unavailable for plugin-process cleanup")
    process_ids = sorted({str(item["pid"]) for item in processes})
    script = (
        "$ids = @(" + ",".join(process_ids) + ")\n"
        "foreach ($id in $ids) { Stop-Process -Id $id -Force -ErrorAction SilentlyContinue }\n"
    )
    creationflags = getattr(subprocess, "CREATE_NO_WINDOW", 0)
    subprocess.run(
        [shell, "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", script],
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=30,
        check=False,
        creationflags=creationflags,
    )
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if not find_temp_processes(temp_root, env):
            return len(processes)
        time.sleep(0.1)
    raise SmokeFailure("temporary plugin processes survived forced cleanup")


def assert_no_temp_processes(temp_root: Path, env: dict[str, str]) -> None:
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if not find_temp_processes(temp_root, env):
            return
        time.sleep(0.1)
    count = cleanup_temp_processes(temp_root, env)
    raise SmokeFailure(
        f"{count} temporary plugin process(es) outlived their KD4 session"
    )


def provider_paths(args: argparse.Namespace) -> dict[str, Path]:
    return {
        "kds": args.kds.resolve(),
        "repo-atlas": args.repo_atlas.resolve(),
    }


def validate_inputs(
    codex: Path,
    providers: dict[str, Path],
    redactor: Redactor,
) -> None:
    if not codex.is_file():
        raise SmokeFailure("KD4 binary does not exist")
    for name, root in providers.items():
        if not root.is_dir() or not (root / ".git").exists():
            raise SmokeFailure(f"{name} provider checkout is unavailable")
    for program in ("git", "cargo", "node"):
        if shutil.which(program) is None:
            raise SmokeFailure(f"required local executable {program!r} is unavailable")
    try:
        version = subprocess.run(
            [str(codex), "--version"],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=15,
            check=False,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise SmokeFailure(
            f"KD4 binary could not be inspected: {redactor.text(exc)}"
        ) from exc
    if version.returncode != 0:
        raise SmokeFailure("KD4 binary version inspection failed")


def parse_args() -> argparse.Namespace:
    repository = Path(__file__).resolve().parents[1]
    desktop = repository.parent
    parser = argparse.ArgumentParser(
        description=(
            "Exercise installed KDS and Repo Atlas MCP results through "
            "KD4 evidence-only persistence. Requires the explicit --run guard."
        )
    )
    parser.add_argument(
        "--run",
        action="store_true",
        help="acknowledge local plugin installation into disposable Codex homes",
    )
    parser.add_argument(
        "--codex",
        type=Path,
        default=repository / "codex-rs" / "target" / "debug" / "codex.exe",
        help="current KD4 binary to exercise",
    )
    parser.add_argument(
        "--kds",
        type=Path,
        default=desktop / "kds-main",
        help="local KDS checkout",
    )
    parser.add_argument(
        "--repo-atlas",
        type=Path,
        default=desktop / "repo-atlas",
        help="local Repo Atlas checkout",
    )
    args = parser.parse_args()
    if not args.run:
        parser.error(
            "--run is required; the smoke test installs local plugins into temp homes"
        )
    return args


def main() -> int:
    args = parse_args()
    codex = args.codex.resolve()
    providers = provider_paths(args)
    initial_redactor = Redactor([codex, *providers.values()])
    validate_inputs(codex, providers, initial_redactor)

    temp_directory = tempfile.TemporaryDirectory(prefix="kd4-investigation-evidence-")
    temp_root = Path(temp_directory.name).resolve()
    redactor = Redactor([codex, temp_root, *providers.values()])
    server = LoopbackResponsesServer()
    server_thread = threading.Thread(
        target=server.serve_forever,
        name="kd4-investigation-loopback",
        daemon=True,
    )
    server_thread.start()
    cleanup_env: dict[str, str] | None = None
    passed = 0
    try:
        common_env = isolated_environment(temp_root / "control-home", temp_root)
        cleanup_env = common_env
        for provider, source_root in providers.items():
            source_before_staging = source_fingerprint(
                source_root,
                env=common_env,
                redactor=redactor,
            )
            staged_root = temp_root / "providers" / provider
            stage_provider(
                source_root,
                staged_root,
                env=common_env,
                redactor=redactor,
            )
            source_after_staging = source_fingerprint(
                source_root,
                env=common_env,
                redactor=redactor,
            )
            if source_after_staging != source_before_staging:
                raise SmokeFailure(
                    f"{provider} provider checkout changed during staging"
                )
            home = temp_root / "homes" / provider
            home.mkdir(parents=True, exist_ok=False)
            (home / "config.toml").write_text(
                "[features]\nplugins = true\n",
                encoding="utf-8",
                newline="\n",
            )
            environment = isolated_environment(home, temp_root)
            cleanup_env = environment
            install_provider(
                provider,
                staged_root,
                codex,
                home,
                env=environment,
                redactor=redactor,
            )

            for ordinal in range(2):
                repo = temp_root / "repositories" / f"{provider}-{ordinal}"
                case, files = make_case(provider, ordinal, repo)
                initialize_git_repo(
                    repo,
                    files,
                    env=environment,
                    redactor=redactor,
                )
                thread_id, model_output = run_codex_case(
                    case,
                    repo,
                    codex,
                    home,
                    server,
                    base_env=environment,
                    redactor=redactor,
                )
                model_structured = parse_model_structured_output(
                    case, model_output, redactor
                )
                document = load_evidence_document(home, thread_id)
                retention = verify_receipt(
                    case,
                    document,
                    home,
                    thread_id,
                    model_structured,
                )
                verify_evidence_only_state(document)
                assert_no_temp_processes(temp_root, environment)
                print(
                    f"PASS {case.name}: {case.expected_completeness}, "
                    f"{'approximate' if case.expected_approximate else 'exact'}, {retention}"
                )
                passed += 1

        print(f"PASS integrated provider evidence smoke: {passed}/4 cases")
        return 0
    except Exception as exc:  # noqa: BLE001 - one redacted, nonzero smoke report
        print(
            f"FAIL integrated provider evidence smoke: {redactor.text(exc)}",
            file=sys.stderr,
        )
        return 1
    finally:
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=5)
        if cleanup_env is not None:
            try:
                cleanup_temp_processes(temp_root, cleanup_env)
            except Exception as exc:  # noqa: BLE001 - cleanup must not expose paths
                print(
                    f"FAIL temporary process cleanup: {redactor.text(exc)}",
                    file=sys.stderr,
                )
        temp_directory.cleanup()


if __name__ == "__main__":
    raise SystemExit(main())
