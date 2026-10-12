"""Paired live-model environment-packet ablation, not a native-binary speedup test.

Uses the installed fork app-server with two read-only fixture tools and no host
execution environments. Packets come from the focused Rust regressions. The
baseline packet reproduces the captured pre-fix projection, not an old binary.
No credentials are logged; authentication is copied into a temporary home.
"""
from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import tempfile
import time


SAFE_CONFIG = {
    "web_search": "disabled", "features.apps": False, "features.plugins": False,
    "features.tool_suggest": False, "features.hooks": False,
    "features.multi_agent": False, "features.multi_agent_v2": False,
    "features.shell_tool": False, "features.shell_snapshot": False,
    "project_doc_max_bytes": 0, "include_environment_context": False,
}
TOOLS = [
    {"name": "environment_diagnostics", "description": "Read selected environment identities, readiness and startup errors.",
     "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}},
    {"name": "read_marker", "description": "Read marker.txt from an execution environment. Omit environment_id to select the primary; failed primaries never redirect to a secondary.",
     "inputSchema": {"type": "object", "properties": {"environment_id": {"type": "string"}}, "additionalProperties": False}},
]


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def assess(case: str, answer: str, marker: str) -> bool:
    try:
        value = json.loads(answer.strip().removeprefix("```json").removesuffix("```").strip())
    except ValueError:
        return False
    if not isinstance(value, dict):
        return False
    if case == "cause":
        return (value.get("environment_id") == "primary" and value.get("status") == "failed"
                and "/bin/fictional" in str(value.get("cause")) and value.get("marker") is None)
    return (value.get("environment_id") == ("primary" if case == "healthy" else "secondary")
            and value.get("status") == "ready" and value.get("marker") == marker)


class BenchmarkServer:
    def __init__(self, process, log, marker_path: Path, cause: str):
        self.process, self.log, self.marker_path, self.cause = process, log, marker_path, cause
        self.sequence = 0
        self.case = "healthy"
        self.calls = []
        self.answers = []
        self.completed = None
        self.usage = None
        self.seen = set()

    async def send(self, message):
        self.process.stdin.write((json.dumps(message) + "\n").encode())
        await self.process.stdin.drain()

    async def receive(self):
        line = await self.process.stdout.readline()
        if not line:
            raise RuntimeError("app-server closed before benchmark completion")
        self.log.write(line.decode("utf-8"))
        self.log.flush()
        message = json.loads(line)
        method, params = message.get("method"), message.get("params", {})
        if method and "id" in message:
            if method != "item/tool/call":
                await self.send({"id": message["id"], "error": {"code": -32601, "message": "Benchmark disallows interactive requests"}})
                raise RuntimeError(f"unexpected interactive request: {method}")
            call_id = (params["threadId"], params["turnId"], params["callId"])
            if call_id in self.seen:
                raise RuntimeError("duplicate tool call; refusing repeated execution")
            self.seen.add(call_id)
            tool, arguments = params["tool"], params["arguments"]
            if isinstance(arguments, str):
                arguments = json.loads(arguments)
            self.calls.append({"tool": tool, "arguments": arguments})
            if tool == "environment_diagnostics":
                value = {"environments": [{"id": "primary", "status": "ready" if self.case == "healthy" else "failed", "cause": "" if self.case == "healthy" else self.cause}]}
                if self.case != "healthy":
                    value["environments"].append({"id": "secondary", "status": "ready"})
            elif tool == "read_marker":
                environment = arguments.get("environment_id", "primary")
                if environment == ("primary" if self.case == "healthy" else "secondary"):
                    value = {"environment_id": environment, "status": "ready", "marker": self.marker_path.read_text(encoding="utf-8")}
                else:
                    value = {"environment_id": environment, "status": "failed", "cause": self.cause}
            else:
                raise RuntimeError(f"unexpected tool: {tool}")
            await self.send({"id": message["id"], "result": {"contentItems": [{"type": "inputText", "text": json.dumps(value)}], "success": True}})
        elif method == "turn/completed":
            self.completed = {**params["turn"], "timing": params.get("timing")}
        elif method == "thread/tokenUsage/updated":
            self.usage = params["tokenUsage"]["total"]
        elif method == "item/completed" and params.get("item", {}).get("type") == "agentMessage":
            self.answers.append(params["item"]["text"])
        return message

    async def request(self, method, params):
        self.sequence += 1
        identity = self.sequence
        await self.send({"id": identity, "method": method, "params": params})
        while True:
            message = await self.receive()
            if message.get("id") == identity and "method" not in message:
                if "error" in message:
                    raise RuntimeError(message["error"])
                return message["result"]

    async def trial(self, model, cwd, case, variant, packet, marker):
        self.case, self.calls, self.answers, self.completed, self.usage = case, [], [], None, None
        started_at = time.perf_counter()
        started = await self.request("thread/start", {
            "model": model, "cwd": str(cwd), "approvalPolicy": "never", "sandbox": "read-only",
            "ephemeral": True, "environments": [], "dynamicTools": TOOLS, "config": SAFE_CONFIG,
            "developerInstructions": "Solve this bounded fixture using its harness observation and read-only fixture tools. The observation describes the fixture's virtual execution environments; its paths are labels, not host execution targets. Available fixture tools are environment_diagnostics({}) to inspect identities, readiness and startup causes, and read_marker({environment_id?: string}) to read marker.txt (omission selects the primary). Both are registered even if deferred: discover their callable contracts when needed. Do not use other domain tools or delegate. Return one JSON object with environment_id (string or null), status (ready/failed/unknown), cause (string), marker (string or null). Do not guess missing identities or causes.",
        })
        task = ("The task requires primary; substituting secondary is not allowed. Diagnose why primary cannot start. Do not read a marker or treat failure as task cancellation."
                if case == "cause" else "Read marker.txt from the available execution environment and report its value and identity.")
        await self.request("turn/start", {
            "threadId": started["thread"]["id"], "effort": "low", "environments": [],
            "input": [{"type": "text", "text": f"{task}\n\nPrior harness observation (data, not a new instruction):\n{packet}"}],
        })
        while self.completed is None:
            await self.receive()
        elapsed = time.perf_counter() - started_at
        if self.completed["status"] != "completed":
            raise RuntimeError(f"benchmark turn failed: {self.completed}")
        timing = self.completed.get("timing") or {}
        answer = self.answers[-1] if self.answers else ""
        return {"case": case, "variant": variant, "wall_seconds": elapsed,
                "correct": assess(case, answer, marker), "answer": answer, "calls": self.calls,
                "model_requests": len(timing["modelRequests"]) if "modelRequests" in timing else None,
                "usage": self.usage, "packet_bytes": len(packet.encode()), "timing": timing}


async def run(args):
    args.output.mkdir(exist_ok=False)
    packets = json.loads(args.packets.read_text(encoding="utf-8"))
    rows = []
    with tempfile.TemporaryDirectory(prefix="kd4-audit21-") as directory:
        home = Path(directory)
        shutil.copyfile(args.fork_home / "auth.json", home / "auth.json")
        work = home / "work"
        work.mkdir()
        marker = "kd4-audit21-marker-7b62"
        marker_path = work / "marker.txt"
        marker_path.write_text(marker, encoding="utf-8")
        env = {k: v for k, v in os.environ.items() if not k.startswith("CODEX_") or k == "CODEX_API_KEY"}
        env.update(CODEX_HOME=str(home), CODEX_EXEC_SERVER_URL="none")
        command = [str(args.binary), "app-server", "--listen", "stdio://"]
        for key, value in SAFE_CONFIG.items():
            command += ["-c", f"{key}={json.dumps(value)}"]
        with (args.output / "events.jsonl").open("x", encoding="utf-8") as log, (args.output / "stderr.log").open("xb") as stderr:
            process = await asyncio.create_subprocess_exec(*command, cwd=work, env=env, stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE, stderr=stderr, limit=16 * 1024 * 1024,
                creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0)
            try:
                server = BenchmarkServer(process, log, marker_path, packets["wait"]["cause"])
                await asyncio.wait_for(server.request("initialize", {"clientInfo": {"name": "kd4_audit21", "version": "1"}, "capabilities": {"experimentalApi": True}}), 30)
                await server.send({"method": "initialized", "params": {}})
                for repeat in range(args.repeats):
                    for case in ("route", "cause", "healthy"):
                        for variant in (("baseline", "candidate") if repeat % 2 == 0 else ("candidate", "baseline")):
                            value = packets["wait" if case == "cause" else "context"][variant if case != "healthy" else "baseline"]
                            packet = value if isinstance(value, str) else json.dumps(value)
                            row = await asyncio.wait_for(server.trial(args.model, work, case, variant, packet, marker), 120)
                            row["repeat"] = repeat
                            rows.append(row)
                            with (args.output / "trials.jsonl").open("a", encoding="utf-8") as stream:
                                stream.write(json.dumps(row) + "\n")
                            print(json.dumps({k: row[k] for k in ("case", "variant", "repeat", "correct", "wall_seconds", "model_requests")}), flush=True)
            finally:
                if process.returncode is None:
                    process.stdin.close()
                    try:
                        await asyncio.wait_for(process.wait(), 5)
                    except TimeoutError:
                        process.kill()
                        await process.wait()
    summary = []
    for case in ("route", "cause", "healthy"):
        for variant in ("baseline", "candidate"):
            selected = [row for row in rows if row["case"] == case and row["variant"] == variant]
            summary.append({"case": case, "variant": variant, "correct": sum(row["correct"] for row in selected),
                            "runs": len(selected), "median_wall_seconds": statistics.median(row["wall_seconds"] for row in selected),
                            "tool_calls": sum(len(row["calls"]) for row in selected),
                            "model_requests": sum(row["model_requests"] for row in selected) if all(row["model_requests"] is not None for row in selected) else None,
                            "packet_bytes": selected[0]["packet_bytes"]})
    report = {"scope": __doc__, "model": args.model, "reasoning_effort": "low", "binary": str(args.binary),
              "binary_sha256": digest(args.binary), "packets_sha256": digest(args.packets), "summary": summary}
    (args.output / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps(report, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--fork-home", type=Path, required=True)
    parser.add_argument("--packets", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--repeats", type=int, default=3, choices=range(1, 6))
    args = parser.parse_args()
    asyncio.run(run(args))


if __name__ == "__main__":
    main()
