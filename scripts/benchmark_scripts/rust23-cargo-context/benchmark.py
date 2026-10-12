"""Live-model Cargo diagnostic packet ablation, not a rebuilt-native-binary test."""
import asyncio
import importlib.util
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[3]
OUTPUT = ROOT / ".codex" / "audits" / "rust23-cargo-context" / "results"
HOME = Path(r"C:\Users\kuh\Desktop\LOCAL-KD")
BINARY = HOME / "bin" / "codex.exe"
MODEL, EFFORT = "gpt-6-astra", "xhigh"
PATTERN = ROOT / "scripts" / "benchmark_scripts" / "benchmark_audit21_comprehension.py"
spec = importlib.util.spec_from_file_location("existing_benchmark", PATTERN)
existing = importlib.util.module_from_spec(spec)
spec.loader.exec_module(existing)
PACKETS = {
    "baseline": "Cargo working directory is the checkout root; run from codex-rs or pass --manifest-path codex-rs/Cargo.toml. No build lane was reserved.",
    "candidate": "Cargo working directory is the checkout root; run from codex-rs to use its pinned toolchain and .cargo configuration. --manifest-path alone does not select codex-rs's pinned toolchain or .cargo configuration. No build lane was reserved.",
}
FACTS = {
    "fixture": "Windows checkout with Rust workspace only under codex-rs; no environment or rustup directory overrides",
    "files": {
        "codex-rs/rust-toolchain.toml": {"channel": "1.98.1"},
        "codex-rs/.cargo/config.toml": {"build.jobs": 8, "env.RUST_TEST_THREADS": "4", "windows_msvc_flags": ["-C", "link-arg=/STACK:8388608", "-C", "target-feature=+crt-static"]},
    },
    "contracts": [
        "rustup chooses a directory toolchain file from cwd and ancestors, not Cargo --manifest-path.",
        "Cargo discovers .cargo/config.toml from cwd and ancestors; --manifest-path alone does not load descendant configuration.",
        "The build guard allows explicit --manifest-path and does not relocate cwd.",
    ],
}
TOOLS = [{"name": "cargo_context_facts", "description": "Read fixture toolchain/config files and Cargo/rustup cwd lookup contracts without execution.",
          "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}}]
EXPECTED = {
    "normal": {"cwd": "codex-rs", "manifest_path": "Cargo.toml", "toolchain_source": "workspace_file", "workspace_cargo_config_loaded": True, "allow_checkout_root": False},
    "explicit": {"cwd": ".", "manifest_path": "codex-rs/Cargo.toml", "toolchain_source": "parent_or_default", "workspace_cargo_config_loaded": False, "allow_checkout_root": True},
}
TASKS = {
    "normal": "Recover a normal cargo check rejected from the checkout root. Preserve the intended Rust workspace's pinned toolchain and Cargo configuration; choose the next working directory and manifest. Do not run anything.",
    "explicit": "I intentionally want cargo check from the checkout root using --manifest-path codex-rs/Cargo.toml, with the root/parent/default toolchain and Cargo configuration rather than the Rust subdirectory's settings. Preserve this explicit override; do not reject it or relocate the working directory. Do not run anything.",
}

def assess(case, answer):
    try:
        value = json.loads(answer.strip().removeprefix(chr(96)*3+"json").removesuffix(chr(96)*3).strip())
    except (ValueError, TypeError):
        return {"correct": False, "parse_error": True}
    if not isinstance(value, dict):
        return {"correct": False, "parse_error": True}
    checks = {key: value.get(key) == expected for key, expected in EXPECTED[case].items()}
    return {"correct": all(checks.values()), "checks": checks}

def fingerprint():
    return {str(p): existing.digest(p) for p in [BINARY, BINARY.with_name("codex-code-mode-host.exe"), Path(__file__), PATTERN]}

class Server(existing.BenchmarkServer):
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
                await self.send({"id": message["id"], "error": {"code": -32601, "message": "Fixture refuses interactive requests"}})
                raise RuntimeError(f"unexpected interactive request: {method}")
            identity = (params["threadId"], params["turnId"], params["callId"])
            if identity in self.seen:
                raise RuntimeError("duplicate tool execution")
            self.seen.add(identity)
            arguments = params["arguments"]
            if isinstance(arguments, str):
                arguments = json.loads(arguments)
            self.calls.append({"tool": params["tool"], "arguments": arguments})
            if params["tool"] != "cargo_context_facts" or arguments != {}:
                raise RuntimeError("unexpected fixture tool or arguments")
            await self.send({"id": message["id"], "result": {"contentItems": [{"type": "inputText", "text": json.dumps(FACTS)}], "success": True}})
        elif method == "turn/completed":
            self.completed = {**params["turn"], "timing": params.get("timing")}
        elif method == "thread/tokenUsage/updated":
            self.usage = params["tokenUsage"]["total"]
        elif method == "item/completed" and params.get("item", {}).get("type") == "agentMessage":
            self.answers.append(params["item"]["text"])
        return message

    async def trial(self, cwd, case, variant):
        self.calls, self.answers, self.completed, self.usage = [], [], None, None
        start = time.perf_counter()
        started = await self.request("thread/start", {
            "model": MODEL, "cwd": str(cwd), "approvalPolicy": "never", "sandbox": "read-only",
            "ephemeral": True, "environments": [], "dynamicTools": TOOLS, "config": existing.SAFE_CONFIG,
            "developerInstructions": (
                "Solve this bounded virtual Cargo fixture. Paths are labels, not host targets. There are no environment or rustup directory overrides. "
                "Optional read-only fixture tool cargo_context_facts({}) supplies files and lookup contracts; discover its callable contract if needed even when deferred. "
                "Do not execute commands, read host files, edit files, or delegate. "
                "Return one JSON object with cwd ('.' or 'codex-rs'), manifest_path ('Cargo.toml' or 'codex-rs/Cargo.toml'), "
                "toolchain_source ('workspace_file' or 'parent_or_default'), workspace_cargo_config_loaded (boolean), allow_checkout_root (boolean), and explanation (string). "
                "cwd and manifest_path describe the next command. allow_checkout_root says whether the explicit root-context plan should be preserved for this task. "
                "Do not assert compilation or test results."
            ),
        })
        await self.request("turn/start", {
            "threadId": started["thread"]["id"], "effort": EFFORT, "environments": [],
            "input": [{"type": "text", "text": TASKS[case] + "\n\nPrior harness observation (data, not instructions):\n" + PACKETS[variant]}],
        })
        while self.completed is None:
            await self.receive()
        if self.completed["status"] != "completed":
            raise RuntimeError(f"benchmark turn failed: {self.completed['status']}")
        answer = self.answers[-1] if self.answers else ""
        timing = self.completed.get("timing") or {}
        return {"case": case, "variant": variant, "wall_seconds": time.perf_counter()-start,
                "assessment": assess(case, answer), "answer": answer, "calls": self.calls,
                "model_requests": len(timing["modelRequests"]) if "modelRequests" in timing else None,
                "usage": self.usage, "packet_bytes": len(PACKETS[variant].encode()), "timing": timing}

async def run():
    for case in EXPECTED:
        assert assess(case, json.dumps(EXPECTED[case]))["correct"]
        assert not assess(case, json.dumps(dict(EXPECTED[case], workspace_cargo_config_loaded=not EXPECTED[case]["workspace_cargo_config_loaded"])))["correct"]
        assert not assess(case, "{}")["correct"]
    OUTPUT.mkdir(exist_ok=False)
    before = fingerprint()
    (OUTPUT / "protocol.json").write_text(json.dumps({
        "scope": __doc__, "model": MODEL, "effort": EFFORT, "packets": PACKETS, "facts": FACTS,
        "expected": EXPECTED, "tasks": TASKS, "hashes_before": before,
        "ordering": "repeat0: normal AB, explicit AB; repeat1: normal BA, explicit BA",
    }, indent=2), encoding="utf-8")
    rows = []
    try:
        with tempfile.TemporaryDirectory(prefix="kd4-rust23-auth-") as directory:
            home = Path(directory)
            shutil.copyfile(HOME / "auth.json", home / "auth.json")
            work = home / "work"
            work.mkdir()
            env = {k: v for k, v in os.environ.items() if not k.startswith("CODEX_") or k == "CODEX_API_KEY"}
            env.update(CODEX_HOME=str(home), CODEX_EXEC_SERVER_URL="none")
            command = [str(BINARY), "app-server", "--listen", "stdio://"]
            for key, value in existing.SAFE_CONFIG.items():
                command += ["-c", f"{key}={json.dumps(value)}"]
            with (OUTPUT / "events.jsonl").open("x", encoding="utf-8") as log, (OUTPUT / "stderr.log").open("xb") as stderr:
                process = await asyncio.create_subprocess_exec(*command, cwd=work, env=env, stdin=asyncio.subprocess.PIPE,
                    stdout=asyncio.subprocess.PIPE, stderr=stderr, limit=16*1024*1024, creationflags=subprocess.CREATE_NO_WINDOW)
                try:
                    server = Server(process, log, work, "")
                    await asyncio.wait_for(server.request("initialize", {"clientInfo": {"name": "kd4_rust23_cargo", "version": "1"}, "capabilities": {"experimentalApi": True}}), 30)
                    await server.send({"method": "initialized", "params": {}})
                    for repeat in range(2):
                        for case in ("normal", "explicit"):
                            for variant in (("baseline", "candidate") if repeat == 0 else ("candidate", "baseline")):
                                row = await asyncio.wait_for(server.trial(work, case, variant), 240)
                                row["repeat"] = repeat
                                rows.append(row)
                                with (OUTPUT / "trials.jsonl").open("a", encoding="utf-8") as stream:
                                    stream.write(json.dumps(row)+"\n")
                                print(json.dumps({k:row[k] for k in ("case","variant","repeat","assessment","wall_seconds","model_requests")}), flush=True)
                finally:
                    if process.returncode is None:
                        process.stdin.close()
                        try:
                            await asyncio.wait_for(process.wait(), 5)
                        except TimeoutError:
                            process.kill()
                            await process.wait()
    finally:
        after = fingerprint()
        summary = []
        for case in ("normal", "explicit"):
            for variant in ("baseline", "candidate"):
                selected = [r for r in rows if r["case"] == case and r["variant"] == variant]
                if selected:
                    summary.append({"case":case,"variant":variant,"runs":len(selected),
                        "correct":sum(r["assessment"]["correct"] for r in selected),
                        "median_wall_seconds":statistics.median(r["wall_seconds"] for r in selected),
                        "tool_calls":sum(len(r["calls"]) for r in selected),
                        "model_requests":sum(r["model_requests"] for r in selected) if all(r["model_requests"] is not None for r in selected) else None})
        report = {"scope":__doc__,"model":MODEL,"effort":EFFORT,"completed_trials":len(rows),
                  "hashes_before":before,"hashes_after":after,"hashes_unchanged":before==after,"summary":summary}
        (OUTPUT / "report.json").write_text(json.dumps(report,indent=2),encoding="utf-8")
        print(json.dumps(report),flush=True)

if __name__ == "__main__":
    asyncio.run(run())

