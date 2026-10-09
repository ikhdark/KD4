"""Harbor adapter: Windows Codex app-server, Linux sandbox terminal tools.

No Codex binary, credentials, host filesystem, or model client enters the sandbox.
This is a custom tool harness, not the built-in Harbor Codex agent.
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import os
import shlex
import shutil
import subprocess
import tempfile
from pathlib import Path, PurePosixPath
from uuid import uuid4

from harbor.agents.base import BaseAgent
from harbor.agents.options import AgentOptions
from harbor.models.agent.context import AgentContext
from pydantic import Field

VERSION = "1.0.0"
TOOL = {
    "type": "function",
    "name": "sandbox_terminal",
    "description": (
        "Run bash in the Linux benchmark sandbox (never on the Windows host). "
        "Use shell commands to inspect, edit files, and test. Each call is a fresh "
        "shell; use cwd or cd explicitly. No interactive stdin. Commands are "
        "serialized and killed at timeout. Large output is saved in the sandbox; "
        "read it in smaller chunks with another call."
    ),
    "inputSchema": {
        "type": "object",
        "properties": {
            "command": {"type": "string"},
            "cwd": {"type": "string", "description": "Absolute Linux directory."},
            "timeout_sec": {"type": "integer", "minimum": 1, "maximum": 300},
        },
        "required": ["command"],
        "additionalProperties": False,
    },
}
SAFE_CONFIG = {
    "web_search": "disabled",
    "features.apps": False,
    "features.plugins": False,
    "features.tool_suggest": False,
    "features.hooks": False,
    "features.multi_agent": False,
    "features.multi_agent_v2": False,
    "features.shell_tool": False,
    "features.shell_snapshot": False,
}


def file_sha256(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def validate_binary(path: Path) -> Path:
    path = path.expanduser().resolve(strict=True)
    with path.open("rb") as stream:
        magic = stream.read(2)
    if path.suffix.lower() != ".exe" or magic != b"MZ":
        raise ValueError("codex_binary must be an explicit Windows .exe, not a PATH shim or Linux binary")
    if not path.with_name("codex-code-mode-host.exe").is_file():
        raise ValueError("The matching codex-code-mode-host.exe must be beside codex_binary")
    return path


def write_json(path: Path, value) -> None:
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


class WindowsCodexOptions(AgentOptions):
    codex_binary: str
    auth_home: str
    reasoning_effort: str = "high"
    capture_patch: bool = False
    budget_sec: int | None = Field(default=None, ge=10, le=86400)


class SandboxTerminal:
    def __init__(self, environment, logs_dir: Path, cwd: str):
        self.environment = environment
        self.logs_dir = logs_dir
        self.cwd = cwd
        self.control = f"/tmp/codex-benchmark-{uuid4().hex}"
        self.active: set[str] = set()

    async def setup(self):
        result = await self.environment.exec(
            command=f"command -v bash && command -v timeout && command -v setsid && mkdir -m 700 {self.control}",
            timeout_sec=30,
        )
        if result.return_code != 0:
            raise RuntimeError("Sandbox requires bash, GNU timeout, and setsid")

    async def execute(self, name: str, arguments: dict) -> dict:
        if name != TOOL["name"]:
            raise ValueError(f"Unknown tool: {name}")
        if not isinstance(arguments, dict) or set(arguments) - {"command", "cwd", "timeout_sec"}:
            raise ValueError("Invalid terminal arguments")
        command = arguments.get("command")
        cwd = arguments.get("cwd", self.cwd)
        seconds = arguments.get("timeout_sec", 60)
        if not isinstance(command, str) or not command or "\0" in command:
            raise ValueError("command must be nonempty text without NUL")
        if not isinstance(cwd, str) or not cwd.startswith("/") or "\0" in cwd:
            raise ValueError("cwd must be an absolute Linux path")
        if type(seconds) is not int or not 1 <= seconds <= 300:
            raise ValueError("timeout_sec must be an integer between 1 and 300")
        token = uuid4().hex
        pid = f"{self.control}/{token}.pid"
        output = f"{self.control}/{token}.output"
        # Keep a process group handle so host cancellation can stop the remote
        # command before patch capture. Never retry an uncertain execution.
        wrapped = (
            f"if test -f {pid}.cancel; then touch {pid}.done; exit 125; fi; "
            f"setsid timeout -k 5s {seconds}s bash -lc {shlex.quote(command)} >{output} 2>&1 & "
            f"child=$!; printf '%s' \"$child\" >{pid}; "
            f"if test -f {pid}.cancel; then kill -TERM -- -\"$child\" 2>/dev/null; kill -TERM \"$child\" 2>/dev/null; fi; "
            'wait "$child"; rc=$?; '
            'kill -KILL -- -"$child" 2>/dev/null || true; '
            f"rm -f {pid}; touch {pid}.done; head -c 24000 {output}; "
            f"printf '\\n[full output: {output}; bytes: '; wc -c <{output}; printf ']\\n'; exit \"$rc\""
        )
        self.active.add(pid)
        result = await self.environment.exec(command=wrapped, cwd=cwd, timeout_sec=seconds + 15)
        self.active.remove(pid)
        record = {
            "command": command, "cwd": cwd, "timeout_sec": seconds,
            "stdout": result.stdout or "", "stderr": result.stderr or "",
            "return_code": result.return_code, "output_path": output,
        }
        write_json(self.logs_dir / f"terminal-{token}.json", record)
        return record

    async def stop(self):
        for pid in tuple(self.active):
            # pid is generated by this object, never supplied by the model.
            command = (
                f"touch {pid}.cancel; n=0; while ! test -f {pid}.done; do "
                f"if test -f {pid}; then p=$(cat {pid}); "
                'case "$p" in ""|*[!0-9]*) exit 1;; esac; '
                'test "$p" -gt 1 || exit 1; '
                'kill -TERM -- -"$p" 2>/dev/null || true; sleep 1; '
                'kill -KILL -- -"$p" 2>/dev/null || true; '
                'kill -TERM "$p" 2>/dev/null || true; fi; '
                'n=$((n+1)); test "$n" -lt 5 || exit 1; sleep 1; done'
            )
            result = await self.environment.exec(command=command, timeout_sec=15)
            if result.return_code != 0:
                raise RuntimeError("Unable to confirm remote command cleanup; do not grade this trial")
            self.active.remove(pid)


class AppServer:
    """One stdio reader; server tool calls are intentionally serialized."""

    def __init__(self, process, terminal: SandboxTerminal, log, context: AgentContext):
        self.process, self.terminal, self.log, self.context = process, terminal, log, context
        self.sequence = 0
        self.completed = None
        self.seen_calls: set[str] = set()

    async def send(self, value):
        self.process.stdin.write((json.dumps(value, ensure_ascii=False) + "\n").encode())
        await self.process.stdin.drain()

    async def receive(self):
        line = await self.process.stdout.readline()
        if not line:
            raise RuntimeError(f"Codex app-server closed stdout (exit={self.process.returncode}); see app-server.stderr.log")
        message = json.loads(line)
        self.log.write(line.decode("utf-8"))
        self.log.flush()
        method = message.get("method")
        params = message.get("params", {})
        if method and "id" in message:
            if method != "item/tool/call":
                await self.send({"id": message["id"], "error": {"code": -32601, "message": "Interactive requests are disabled in benchmarks"}})
                raise RuntimeError(f"Unexpected app-server request: {method}")
            call_id = params["callId"]
            if call_id in self.seen_calls:
                raise RuntimeError("Duplicate tool call ID; refusing to execute twice")
            self.seen_calls.add(call_id)
            try:
                value = await self.terminal.execute(params["tool"], params["arguments"])
                success = True  # Nonzero shell exit is a valid tool observation.
            except ValueError as exc:
                value, success = {"error": str(exc)}, False
            await self.send({"id": message["id"], "result": {
                "contentItems": [{"type": "inputText", "text": json.dumps(value, ensure_ascii=False)}],
                "success": success,
            }})
        elif method == "turn/completed":
            self.completed = params["turn"]
        elif method == "thread/tokenUsage/updated":
            usage = params["tokenUsage"]["total"]
            self.context.n_input_tokens = usage.get("inputTokens")
            self.context.n_cache_tokens = usage.get("cachedInputTokens")
            self.context.n_output_tokens = usage.get("outputTokens")
        return message

    async def request(self, method, params):
        self.sequence += 1
        request_id = self.sequence
        await self.send({"id": request_id, "method": method, "params": params})
        while True:
            message = await self.receive()
            if message.get("id") == request_id and "method" not in message:
                if "error" in message:
                    raise RuntimeError(f"{method}: {message['error']}")
                return message["result"]

    async def run(self, model, effort, cwd, instruction):
        await self.request("initialize", {
            "clientInfo": {"name": "harbor_windows_codex", "version": VERSION},
            "capabilities": {"experimentalApi": True},
        })
        await self.send({"method": "initialized", "params": {}})
        started = await self.request("thread/start", {
            "model": model, "cwd": str(cwd), "approvalPolicy": "never",
            "sandbox": "read-only", "ephemeral": True, "environments": [],
            "dynamicTools": [TOOL], "config": SAFE_CONFIG,
            "developerInstructions": (
                "Solve the benchmark using sandbox_terminal. It executes bash in a Linux sandbox, "
                f"initial directory {self.terminal.cwd}. The Windows host is not the task environment. "
                "Use terminal commands to read and edit task files. Do not spawn agents, request input, "
                "use external knowledge services, or seek hidden tests/reference solutions. "
                "No local execution environment is available."
            ),
        })
        await self.request("turn/start", {
            "threadId": started["thread"]["id"], "effort": effort,
            "input": [{"type": "text", "text": instruction}], "environments": [],
        })
        while self.completed is None:
            await self.receive()
        if self.completed["status"] != "completed":
            raise RuntimeError(f"Codex turn did not complete: {self.completed}")


async def stop_process(process):
    if process.returncode is not None:
        return
    process.stdin.close()
    try:
        await asyncio.wait_for(process.wait(), timeout=5)
    except TimeoutError:
        if os.name == "nt":
            killer = await asyncio.create_subprocess_exec(
                "taskkill", "/PID", str(process.pid), "/T", "/F",
                stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
                creationflags=subprocess.CREATE_NO_WINDOW,
            )
            await killer.wait()
        else:
            process.kill()
        await asyncio.wait_for(process.wait(), timeout=10)


async def capture_patch(environment, cwd: str, target: Path, base_commit: str):
    # Required benchmark artifact, not a checkout status/diff inspection.
    if len(base_commit) not in (40, 64) or any(c not in "0123456789abcdef" for c in base_commit):
        raise ValueError("Expected the captured base commit hash")
    remote = f"/tmp/codex-model-{uuid4().hex}.patch"
    index = f"/tmp/codex-index-{uuid4().hex}"
    result = await environment.exec(
        command=(f"export GIT_INDEX_FILE={index}; trap 'rm -f {index}' EXIT; "
                 f"git read-tree {base_commit} && git add -A && git diff --cached --binary {base_commit} >{remote}"),
        cwd=cwd, timeout_sec=120,
    )
    if result.return_code != 0:
        raise RuntimeError(f"Patch capture failed: {result.stderr}")
    await environment.download_file(remote, target)


class WindowsCodex(BaseAgent):
    options_model = WindowsCodexOptions

    @staticmethod
    def name():
        return "windows-local-codex"

    def version(self):
        return VERSION

    @classmethod
    def preflight(cls, kwargs=None, env=None):
        options = cls.parse_options(kwargs, env)
        if env:
            raise ValueError("Do not use --ae: Harbor forwards agent env into the sandbox. Use host login or ambient API keys instead")
        if os.name != "nt":
            raise ValueError("This adapter runs the local Windows fork; launch Harbor on Windows")
        validate_binary(Path(options.codex_binary))
        home = Path(options.auth_home).expanduser().resolve(strict=True)
        if not (home / "auth.json").is_file() and not any(
            os.environ.get(key) for key in ("OPENAI_API_KEY", "CODEX_API_KEY")
        ):
            raise ValueError("Log in to the fork or provide an API key before launching")

    async def setup(self, environment):
        self.preflight(self.options.model_dump(), self.extra_env)
        if self.mcp_servers or self.skills_dir:
            raise ValueError("Task MCP servers/skills are not supported by this terminal-only adapter")

    async def run(self, instruction, environment, context):
        self.preflight(self.options.model_dump(), self.extra_env)
        if not self.model_name:
            raise ValueError("Pass an explicit model; no upstream/default-model fallback is permitted")
        if "/" in self.model_name and not self.model_name.startswith("openai/"):
            raise ValueError("This adapter supports OpenAI models only; custom provider config is not imported")
        binary = validate_binary(Path(self.options.codex_binary))
        source_home = Path(self.options.auth_home).expanduser().resolve(strict=True)
        self.logs_dir.mkdir(parents=True, exist_ok=True)
        detected = await environment.exec(
            command=("if test -d /app/.git; then cd /app; elif test -d /testbed/.git; then cd /testbed; fi; pwd"),
            timeout_sec=30,
        )
        cwd = (detected.stdout or "").strip()
        if detected.return_code or not cwd.startswith("/") or "\n" in cwd:
            raise RuntimeError("Cannot determine sandbox working directory")
        base_commit = None
        if self.options.capture_patch:
            base = await environment.exec(command="git rev-parse --verify HEAD", cwd=cwd, timeout_sec=30)
            base_commit = (base.stdout or "").strip()
            if base.return_code or len(base_commit) not in (40, 64) or any(c not in "0123456789abcdef" for c in base_commit):
                raise RuntimeError("Cannot capture the initial repository commit")
        terminal = SandboxTerminal(environment, self.logs_dir, cwd)
        await terminal.setup()
        context.metadata = {
            "adapter": self.name(), "adapter_version": VERSION,
            "codex_binary": str(binary), "codex_sha256": file_sha256(binary),
            "code_mode_host_sha256": file_sha256(binary.with_name("codex-code-mode-host.exe")),
            "sandbox_cwd": cwd, "native_host_tools": False,
            "base_commit": base_commit,
            "benchmark_harness": "hosted-dynamic-terminal", "budget_sec": self.options.budget_sec,
        }
        write_json(self.logs_dir / "runtime.json", context.metadata)
        # Never place auth in Harbor logs (which may be uploaded or copied into
        # the sandbox). Do not modify the user's fork or upstream home/config.
        with tempfile.TemporaryDirectory(prefix="codex-benchmark-") as temp:
            home = Path(temp)
            work = home / "work"
            work.mkdir()
            auth = source_home / "auth.json"
            if auth.is_file():
                shutil.copyfile(auth, home / "auth.json")
            child_env = {k: v for k, v in os.environ.items() if not k.startswith("CODEX_") or k == "CODEX_API_KEY"}
            child_env.update(self.extra_env)
            child_env.update(CODEX_HOME=str(home), CODEX_EXEC_SERVER_URL="none")
            args = [str(binary), "app-server", "--listen", "stdio://"]
            for key, value in SAFE_CONFIG.items():
                args.extend(["-c", f"{key}={json.dumps(value)}"])
            process = None
            try:
                with (self.logs_dir / "app-server.stderr.log").open("wb") as stderr, (
                    self.logs_dir / "app-server.jsonl"
                ).open("w", encoding="utf-8", newline="\n") as log:
                    process = await asyncio.create_subprocess_exec(
                        *args, cwd=work, env=child_env, stdin=asyncio.subprocess.PIPE,
                        stdout=asyncio.subprocess.PIPE, stderr=stderr, limit=16 * 1024 * 1024,
                        creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0,
                    )
                    async with asyncio.timeout(self.options.budget_sec):
                        await AppServer(process, terminal, log, context).run(
                            self.model_name.removeprefix("openai/"), self.options.reasoning_effort,
                            work, instruction,
                        )
            finally:
                try:
                    if process is not None:
                        await stop_process(process)
                finally:
                    await terminal.stop()
                if self.options.capture_patch:
                    await capture_patch(environment, cwd, self.logs_dir / "model.patch", base_commit)


class ReplayOptions(AgentOptions):
    source_job: str


def task_name(path: str) -> str:
    return PurePosixPath(path.replace("\\", "/")).name


def find_patch(source: Path, name: str) -> Path:
    matches = []
    for result_path in sorted(source.glob("*/result.json")):
        result = json.loads(result_path.read_text(encoding="utf-8"))
        if task_name(result["task_name"]) == name:
            patch = result_path.parent / "agent" / "model.patch"
            matches.append(patch)
    if len(matches) != 1 or not matches[0].is_file():
        raise ValueError(f"Expected exactly one captured patch for {name}, found {len(matches)}")
    return matches[0]


class WindowsPatchReplay(BaseAgent):
    """Apply only a captured diff; fail closed on missing/ambiguous patches."""

    options_model = ReplayOptions

    @staticmethod
    def name():
        return "windows-codex-patch-replay"

    def version(self):
        return VERSION

    async def setup(self, environment):
        pass

    async def run(self, instruction, environment, context):
        config = json.loads((self.logs_dir.parent / "config.json").read_text(encoding="utf-8"))
        name = task_name(config["task"]["path"])
        patch = find_patch(Path(self.options.source_job), name)
        self.logs_dir.mkdir(parents=True, exist_ok=True)
        write_json(self.logs_dir / "replay.json", {"task": name, "patch": str(patch), "sha256": file_sha256(patch)})
        if patch.stat().st_size == 0:
            return
        remote = f"/tmp/codex-replay-{uuid4().hex}.patch"
        await environment.upload_file(patch, remote)
        result = await environment.exec(
            command=("if test -d /app/.git; then cd /app; elif test -d /testbed/.git; then cd /testbed; else exit 1; fi; "
                     f"git apply --check {remote} && git apply {remote}"),
            timeout_sec=120,
        )
        if result.return_code != 0:
            raise RuntimeError(f"Patch did not apply cleanly: {result.stderr}")
