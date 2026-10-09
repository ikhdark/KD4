#!/usr/bin/env python3
"""Narrow, offline rust-analyzer LSP navigation benchmark; not a runtime index.

Uses an isolated dependency-free fixture, disables checks/build scripts/macros,
and measures cold startup separately from warm serial/batched RPCs. No model
latency is simulated. --output retains every response and timing observation.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import shutil
import statistics
import subprocess
import tempfile
import threading
import time


SOURCE = """pub struct Value;
pub trait Execute { fn execute(&self); }
impl Execute for Value { fn execute(&self) { target(); } }
pub fn target() {}
pub fn caller(value: &Value) { target(); value.execute(); }
pub fn dependent(value: Value) -> Value { value }
pub mod shadow { pub fn target() {} }
"""


class Lsp:
    def __init__(self, executable, root, timeout):
        self.timeout = timeout
        self.messages = queue.Queue()
        self.transcript = []
        self.sequence = 0
        self.pending = {}
        self.stderr = tempfile.TemporaryFile()
        self.process = subprocess.Popen(
            [executable], cwd=root, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=self.stderr, env={**os.environ, "CARGO_NET_OFFLINE": "true"},
        )
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()

    def _read(self):
        try:
            while True:
                headers = {}
                while True:
                    line = self.process.stdout.readline()
                    if not line:
                        raise EOFError("rust-analyzer stdout closed")
                    if line == b"\r\n":
                        break
                    name, value = line.decode("ascii").split(":", 1)
                    headers[name.lower()] = value.strip()
                size = int(headers["content-length"])
                body = self.process.stdout.read(size)
                if len(body) != size:
                    raise EOFError("incomplete LSP frame")
                self.messages.put(json.loads(body))
        except Exception as error:
            self.messages.put(error)

    def send(self, method, params, request=True):
        message = {"jsonrpc": "2.0", "method": method, "params": params}
        if request:
            self.sequence += 1
            message["id"] = self.sequence
        self._write(message)
        return message.get("id")

    def _write(self, message):
        body = json.dumps(message).encode()
        self.process.stdin.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
        self.process.stdin.flush()

    def receive(self, deadline):
        message = self.messages.get(timeout=max(0.001, deadline - time.monotonic()))
        if isinstance(message, Exception):
            raise message
        self.transcript.append(message)
        if "method" in message and "id" in message:
            result = [None for _ in message["params"]["items"]] if message["method"] == "workspace/configuration" else None
            self._write({"jsonrpc": "2.0", "id": message["id"], "result": result})
        elif "id" in message:
            self.pending[message["id"]] = message
        return message

    def result(self, identity):
        deadline = time.monotonic() + self.timeout
        while identity not in self.pending:
            self.receive(deadline)
        response = self.pending.pop(identity)
        if "error" in response:
            raise RuntimeError(response["error"])
        return response["result"]

    def close(self):
        try:
            if self.process.poll() is None:
                self.result(self.send("shutdown", None))
                self.send("exit", None, request=False)
                self.process.wait(timeout=5)
        finally:
            if self.process.poll() is None:
                self.process.kill()
                self.process.wait()
            self.reader.join(timeout=5)
            self.process.stdin.close()
            self.process.stdout.close()
            self.stderr.close()


def location_lines(result):
    return sorted({entry.get("targetSelectionRange", entry.get("range"))["start"]["line"]
                   for entry in (result if isinstance(result, list) else [result]) if entry})


def benchmark(executable, iterations, timeout):
    with tempfile.TemporaryDirectory(prefix="kd4-semantic-navigation-") as temporary:
        root = Path(temporary)
        (root / "src").mkdir()
        (root / "Cargo.toml").write_text('[package]\nname="navigation_fixture"\nversion="0.0.0"\nedition="2021"\n', encoding="utf-8")
        source = root / "src" / "lib.rs"
        source.write_text(SOURCE, encoding="utf-8", newline="")
        started = time.perf_counter()
        client = Lsp(executable, root, timeout)
        try:
            client.result(client.send("initialize", {
                "processId": os.getpid(), "rootUri": root.as_uri(),
                "capabilities": {"experimental": {"serverStatusNotification": True}},
                "initializationOptions": {"checkOnSave": False,
                    "cargo": {"buildScripts": {"enable": False}, "sysroot": None},
                    "procMacro": {"enable": False}},
            }))
            client.send("initialized", {}, request=False)
            deadline = time.monotonic() + timeout
            while True:
                message = client.receive(deadline)
                if message.get("method") == "experimental/serverStatus" and message["params"].get("quiescent"):
                    break
            cold_ms = (time.perf_counter() - started) * 1000
            client.send("textDocument/didOpen", {"textDocument": {
                "uri": source.as_uri(), "languageId": "rust", "version": 1, "text": SOURCE}}, request=False)
            lines = SOURCE.splitlines()
            def position(line, word):
                return {"textDocument": {"uri": source.as_uri()},
                        "position": {"line": line, "character": lines[line].index(word)}}
            queries = [
                ("textDocument/definition", position(4, "target")),
                ("textDocument/references", {**position(3, "target"), "context": {"includeDeclaration": False}}),
                ("textDocument/implementation", position(1, "Execute")),
                ("textDocument/typeDefinition", position(5, "value")),
                ("textDocument/prepareCallHierarchy", position(3, "target")),
                ("textDocument/prepareCallHierarchy", position(4, "caller")),
            ]
            observations = []
            for iteration in range(iterations):
                # Alternate order to avoid attributing cache warming to batching.
                for batched in ([False, True] if iteration % 2 == 0 else [True, False]):
                    start = time.perf_counter()
                    if batched:
                        ids = [client.send(method, params) for method, params in queries]
                        results = [client.result(identity) for identity in ids]
                    else:
                        results = [client.result(client.send(method, params)) for method, params in queries]
                    incoming = client.result(client.send("callHierarchy/incomingCalls", {"item": results[4][0]}))
                    outgoing = client.result(client.send("callHierarchy/outgoingCalls", {"item": results[5][0]}))
                    measured_ms = (time.perf_counter() - start) * 1000
                    checks = [location_lines(results[0]) == [3], location_lines(results[1]) == [2, 4],
                              location_lines(results[2]) == [2], location_lines(results[3]) == [0],
                              sorted(entry["from"]["name"] for entry in incoming) == ["caller", "execute"],
                              sorted(entry["to"]["name"] for entry in outgoing) == ["execute", "target"]]
                    observations.append({"iteration": iteration, "batched": batched, "wall_ms": measured_ms,
                                         "checks": checks, "results": results, "incoming": incoming, "outgoing": outgoing})
            return {"fixture_sha256": hashlib.sha256(SOURCE.encode()).hexdigest(), "cold_start_ms": cold_ms,
                    "observations": observations, "transcript": client.transcript}
        finally:
            client.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rust-analyzer", default="rust-analyzer")
    parser.add_argument("--iterations", type=int, default=7)
    parser.add_argument("--timeout", type=float, default=60)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not 1 <= args.iterations <= 50 or not 1 <= args.timeout <= 120:
        parser.error("iterations must be 1..50 and timeout 1..120 seconds")
    if args.output.exists():
        parser.error("output must be a new file")
    executable = shutil.which(args.rust_analyzer)
    if not executable:
        parser.error("rust-analyzer is not installed")
    report = benchmark(executable, args.iterations, args.timeout)
    report["version"] = subprocess.check_output([executable, "--version"], text=True).strip()
    encoded = json.dumps(report, ensure_ascii=True, indent=2).encode()
    with args.output.open("xb") as output:
        output.write(encoded)
    assert args.output.read_bytes() == encoded
    summary = {"report": str(args.output.resolve()), "bytes": len(encoded), "sha256": hashlib.sha256(encoded).hexdigest(),
               "version": report["version"], "cold_start_ms": report["cold_start_ms"],
               "accuracy": {"passed": sum(sum(row["checks"]) for row in report["observations"]),
                            "total": len(report["observations"]) * 6},
               "warm_median_ms": {str(batched): statistics.median(row["wall_ms"] for row in report["observations"] if row["batched"] == batched)
                                  for batched in [False, True]},
               "rpc_calls_per_iteration": 8, "model_calls": 0,
               "limits": "Isolated no-dependency fixture; no workspace-wide indexing, macro, build-script, or live-model benchmark."}
    print(json.dumps(summary, indent=2))
    return 0 if summary["accuracy"]["passed"] == summary["accuracy"]["total"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
