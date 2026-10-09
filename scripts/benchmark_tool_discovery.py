"""Offline native discovery workflow benchmark (not a live-model A/B).

Runs the same installed app-server and code-mode host for both workflows. A
scripted loopback provider chooses the cells; request count is therefore a
controlled input, not evidence that a model follows the revised prompt.
No Cargo build, credentials, user config, or external provider is used.
"""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess
import time

from scripts.mock_responses_websocket_server import (
    _event_assistant_message,
    _event_response_completed,
    _event_response_created,
    websockets,
)


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def cells(variant, known=False):
    guard = '''
if (resolve_tool("bench.missing") !== undefined) throw Error("unavailable tool exposed");
const current = resolve_tool("bench.echo");
if (!current || !current.description.includes("discovery-benchmark-v2")) throw Error("stale contract");
if (!current.description.includes("value")) throw Error("argument contract missing");
'''
    invoke = guard + 'text(await resolve_tool("bench.echo")({value:"marker-42"}));'
    if known:
        return ([guard + 'text(resolve_tool("bench.echo"));'] if variant == "baseline" else []) + [invoke]
    if variant == "baseline":
        return ['const names = ALL_TOOL_NAMES.filter(n => n.startsWith("bench__echo")); if (names.length !== 1) throw Error("discovery incomplete"); text(names);',
                guard + 'text(resolve_tool("bench.echo"));', invoke]
    return [guard + 'const matches = ALL_TOOLS.filter(t => t.name === current.name); if (matches.length !== 1 || matches[0].description !== current.description) throw Error("contract mismatch"); text(matches);', invoke]


async def trial(binary, root, variant, known, model_delay_ms):
    root.mkdir()
    home = root / "home"
    home.mkdir()
    scripts = cells(variant, known)
    requests, transcript, outputs, calls = [], [], [], []
    failures = []

    async def model(socket):
        try:
            async for raw in socket:
                request = json.loads(raw)
                requests.append(request)
                number = sum(r.get("generate") is not False for r in requests)
                identity = f"discovery-{len(requests)}"
                events = [_event_response_created(identity)]
                if request.get("generate") is not False:
                    if model_delay_ms:
                        await asyncio.sleep(model_delay_ms / 1000)
                    for item in request.get("input", []):
                        if item.get("type") == "custom_tool_call_output":
                            outputs.append(item)
                    if number <= len(scripts):
                        events.append({"type": "response.output_item.done", "item": {
                            "type": "custom_tool_call", "call_id": f"cell-{number}",
                            "name": "exec", "input": scripts[number - 1],
                        }})
                    else:
                        if number != len(scripts) + 1:
                            raise AssertionError("unexpected model continuation")
                        rendered = json.dumps(outputs)
                        if "marker-42" not in rendered or any(
                            token in rendered for token in ("Error:", '"success": false')
                        ):
                            raise AssertionError(f"native cell failed: {rendered}")
                        events.append(_event_assistant_message("answer", "marker-42"))
                events.append(_event_response_completed(identity))
                for event in events:
                    await socket.send(json.dumps(event))
        except websockets.exceptions.ConnectionClosed:
            pass
        except Exception as error:
            failures.append(repr(error))

    async with websockets.serve(model, "127.0.0.1", 0, max_size=8 * 1024 * 1024) as server:
        port = server.sockets[0].getsockname()[1]
        command = [str(binary), "app-server", "--listen", "stdio://",
                   "-c", 'model_provider="offline"',
                   "-c", f'model_providers.offline={{name="offline",base_url="http://127.0.0.1:{port}/v1",wire_api="responses",supports_websockets=true}}',
                   "-c", "features.code_mode=true", "-c", "features.code_mode_only=true"]
        # Remove ambient credentials; the loopback provider requires no auth.
        env = {k: v for k, v in os.environ.items() if k not in (
            "OPENAI_API_KEY", "CODEX_API_KEY", "OPENAI_BASE_URL", "CODEX_HOME")}
        env.update(CODEX_HOME=str(home), CODEX_EXEC_SERVER_URL="none")
        stderr = (root / "stderr.log").open("wb")
        started = time.perf_counter()
        process = await asyncio.create_subprocess_exec(
            *command, cwd=root, env=env, stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE, stderr=stderr, limit=8 * 1024 * 1024,
            **({"creationflags": subprocess.CREATE_NO_WINDOW} if os.name == "nt" else {}),
        )
        sequence = 0

        async def send(message):
            process.stdin.write((json.dumps(message) + "\n").encode())
            await process.stdin.drain()

        async def receive():
            line = await asyncio.wait_for(process.stdout.readline(), 30)
            if not line:
                raise RuntimeError(f"app-server exited; see {root / 'stderr.log'}")
            message = json.loads(line)
            transcript.append(message)
            if "method" in message and "id" in message:
                if message["method"] != "item/tool/call":
                    await send({"id": message["id"], "error": {"code": -32601, "message": "benchmark denies interactive requests"}})
                    raise AssertionError(f"unexpected interactive request: {message['method']}")
                params = message["params"]
                calls.append(params)
                if params["tool"] != "echo" or params["arguments"] != {"value": "marker-42"}:
                    raise AssertionError(f"incorrect tool dispatch: {params}")
                await send({"id": message["id"], "result": {
                    "contentItems": [{"type": "inputText", "text": "marker-42"}], "success": True,
                }})
            return message

        async def request(method, params):
            nonlocal sequence
            sequence += 1
            identity = sequence
            await send({"id": identity, "method": method, "params": params})
            while True:
                message = await receive()
                if message.get("id") == identity and "method" not in message:
                    if "error" in message:
                        raise RuntimeError(f"{method}: {message['error']}")
                    return message["result"]

        try:
            async with asyncio.timeout(60 + model_delay_ms / 1000 * (len(scripts) + 1)):
                await request("initialize", {"clientInfo": {"name": "tool_discovery_benchmark", "version": "1"},
                                             "capabilities": {"experimentalApi": True}})
                await send({"method": "initialized", "params": {}})
                thread = await request("thread/start", {
                    "model": "gpt-5.2", "cwd": str(root), "approvalPolicy": "never",
                    "sandbox": "read-only", "ephemeral": True, "environments": [],
                    "dynamicTools": [{"type": "namespace", "name": "bench", "description": "Offline fixture.", "tools": [{
                        "type": "function", "name": "echo", "description": "discovery-benchmark-v2: return value unchanged.",
                        "inputSchema": {"type": "object", "properties": {"value": {"type": "string"}},
                                        "required": ["value"], "additionalProperties": False},
                    }]}],
                    "config": {"web_search": "disabled", "features.apps": False,
                               "features.plugins": False, "features.tool_suggest": False,
                               "features.hooks": False, "features.multi_agent": False,
                               "features.multi_agent_v2": False, "features.shell_tool": False,
                               "features.shell_snapshot": False},
                })
                turn_started = time.perf_counter()
                await request("turn/start", {"threadId": thread["thread"]["id"], "environments": [],
                    "input": [{"type": "text", "text": "Discover bench.echo and return marker-42 using it."}]})
                while True:
                    message = await receive()
                    if message.get("method") == "turn/completed":
                        if message["params"]["turn"]["status"] != "completed":
                            raise AssertionError(message)
                        break
                completed = time.perf_counter()
                if failures or len(calls) != 1:
                    raise AssertionError({"providerFailures": failures, "toolCalls": calls})
                count = sum(r.get("generate") is not False for r in requests)
                if count != len(scripts) + 1:
                    raise AssertionError(f"request count {count}")
                return {"variant": variant, "knownContract": known, "modelRequests": count,
                        "cells": len(scripts), "toolCalls": len(calls), "passed": True,
                        "providerRequestBytes": sum(len(json.dumps(r).encode()) for r in requests),
                        "toolOutputBytes": sum(len(json.dumps(r).encode()) for r in outputs),
                        "providerTokens": None,
                        "turnWallMs": (completed - turn_started) * 1000,
                        "startupMs": (turn_started - started) * 1000,
                        "totalWallMs": (completed - started) * 1000}
        finally:
            process.stdin.close()
            try:
                await asyncio.wait_for(process.wait(), 10)
            except TimeoutError:
                process.kill()
                await process.wait()
            stderr.close()
            # Keep the complete settled batch, including failed trial evidence.
            for name, value in (("requests", requests), ("transcript", transcript),
                                ("calls", calls), ("failures", failures)):
                (root / f"{name}.json").write_text(json.dumps(value, indent=2), encoding="utf-8")


async def main_async(args):
    binary = args.binary.resolve(strict=True)
    host = binary.with_name("codex-code-mode-host.exe" if os.name == "nt" else "codex-code-mode-host")
    identities = {str(p): digest(p) for p in (binary, host)}
    args.output.mkdir(parents=True, exist_ok=False)
    rows = []
    for known in (False, True):
        for pair in range(args.runs):
            # Alternate order to avoid assigning all cold/warm effects to one side.
            order = ("baseline", "candidate") if pair % 2 == 0 else ("candidate", "baseline")
            for variant in order:
                row = await trial(binary, args.output / f"{known}-{pair}-{variant}", variant, known, args.model_delay_ms)
                row["pair"] = pair
                rows.append(row)
    if identities != {str(p): digest(p) for p in (binary, host)}:
        raise RuntimeError("binary changed during benchmark")
    repo = Path(__file__).resolve().parents[1]
    report = {"scope": "native app-server/code-mode controlled discovery workflows", "binarySha256": identities,
              "sourceSha256": {str(p.relative_to(repo)): digest(p) for p in (
                  Path(__file__).resolve(), repo / "codex-rs/code-mode-protocol/src/description/exec_prompt.rs")},
              "modelDelayMs": args.model_delay_ms, "trials": rows,
              "limitations": ["Scripted model; does not measure live model adoption of prompt guidance.",
                              "Installed binary, not a rebuild of the working tree.",
                              "Dynamic namespace fixture; no real MCP service or schema refresh during a live cell.",
                              "Any configured provider delay is injected, not observed inference latency."]}
    (args.output / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    for known in (False, True):
        print(json.dumps({"knownContract": known, **{v: {
            "medianTurnMs": statistics.median(r["turnWallMs"] for r in rows if r["knownContract"] == known and r["variant"] == v),
            "modelRequests": next(r["modelRequests"] for r in rows if r["knownContract"] == known and r["variant"] == v),
        } for v in ("baseline", "candidate")}}))
    print(args.output / "report.json")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True, help="New directory retaining all trial evidence")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--model-delay-ms", type=int, default=0)
    args = parser.parse_args()
    if not 1 <= args.runs <= 20 or not 0 <= args.model_delay_ms <= 10000:
        parser.error("runs must be 1..20 and model-delay-ms 0..10000")
    asyncio.run(main_async(args))
