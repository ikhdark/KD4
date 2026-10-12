"""Eight-trial live diagnostic packet ablation; never a rebuilt binary test.

Run --prepare (offline oracle checks), then --run once. Existing outputs refuse
overwrite. Reuses the Cargo benchmark's app-server transport and safe config.
"""
import argparse
import asyncio
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[3]
OUTPUT = ROOT / ".codex/audits/rust23-diagnostic-model"
REPLAY = ROOT / ".codex/audits/rust23-continuation/diagnostic-replay-current.json"
BASE = ROOT / "scripts/benchmark_scripts/rust23-cargo-context/benchmark.py"
CORRECTED = BASE.with_name("benchmark_corrected_normal.py")
spec = importlib.util.spec_from_file_location("cargo_benchmark", BASE)
original = importlib.util.module_from_spec(spec)
spec.loader.exec_module(original)
existing = original.existing
MODEL, EFFORT = "gpt-6-astra", "xhigh"
TOOLS = [{"name": "diagnostic_raw_log", "description": "Read the full retained stderr for this virtual diagnostic case, without executing anything.",
          "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}}]
INSTRUCTION = (
    "Solve this bounded Rust diagnostic fixture. Paths in the packet are labels, not host targets. "
    "Optional read-only dynamic tool diagnostic_raw_log({}) returns the full retained stderr for the current case. "
    "It is registered directly, not deferred. Call it directly in code mode as text(await tools.diagnostic_raw_log({})); "
    "do not use tool_search or resolve_tool for it. Do not execute commands, inspect host files, edit, or delegate. "
    "Return one JSON object only, using the task's keys. Use null for unavailable facts rather than inventing evidence. "
    "compiler_listed_from means exactly the examples printed by this compiler, not every possible implementation. "
    "Explain briefly, and do not claim successful compilation or passing tests from a failure receipt."
)
TASKS = {
    "from": "Diagnose this conversion failure. Return error_code (string), cause_class ('trait_bound', 'type_mismatch', or 'ownership'), rejected_relation (the exact bound without backticks), required_into (the exact required implementor/trait phrase without backticks), compiler_listed_from (array of the exact From<...> examples printed by the compiler, in any order), compiler_list_exhaustive (boolean: whether this excerpt proves all possible implementations), partial_receipt (boolean), tests_passed (boolean), and explanation (string).",
    "mismatch": "Diagnose this Result mismatch. Return error_code (string), cause_class ('trait_bound', 'type_mismatch', or 'ownership'), expected_type and found_type (exact types without backticks), explicit_ok_arguments (array containing the two explicit generic arguments of the Ok constructor in source order, including the error type even though this value is Ok), partial_receipt (boolean), tests_passed (boolean), and explanation (string).",
}
LIMITATIONS = [
    "Fixed installed binary with two exact retained _checked packets; no rebuild, compilation, native tests, or binary-speedup inference.",
    "Saved compiler_stderr is actual previously captured rustc output; retained full logs add synthetic unrelated warning padding and a terminal error.",
    "Two alternating repeats per case/arm are descriptive only; cache, provider variability, shared resources and order effects are uncontrolled.",
    "Harness wall includes thread creation and turn protocol; model-active time is app-server telemetry, not isolated provider compute time.",
    "Ground truth is the exact compiler-listed examples and causal types, not an exhaustive Rust implementation inventory. Free-text explanations require manual review.",
    "The expected/found primary span already repeats both types in baseline; no necessary comprehension improvement follows from retaining the wrapped line.",
    "Provider-internal retries are observed if telemetry reports them; harness never retries a failed trial.",
]


def sha(text):
    return hashlib.sha256(text.encode()).hexdigest()


def load_fixture():
    replay = json.loads(REPLAY.read_text(encoding="utf-8"))
    cases = {case["name"]: case for case in replay["cases"]}
    if set(cases) != set(TASKS):
        raise ValueError("unexpected replay cases")
    for case in cases.values():
        for field in ("source", "compiler_stderr"):
            if sha(case[field]) != case[field + "_sha256"]:
                raise ValueError("fixture text hash mismatch")
        raw_path = Path(case["raw_log_path"])
        if existing.digest(raw_path) != case["raw_log_sha256"]:
            raise ValueError("raw retained log hash mismatch")
        case["raw_log"] = raw_path.read_text(encoding="utf-8")
        if case["compiler_stderr"] not in case["raw_log"]:
            raise ValueError("actual compiler stderr missing from full retained log")
        for packet in case["packets"].values():
            if sha(packet["packet"]) != packet["packet_sha256"] or len(packet["packet"].encode()) != packet["packet_bytes"]:
                raise ValueError("packet hash/length mismatch")
    return cases


def oracle(cases):
    stderr = cases["from"]["compiler_stderr"]
    common = {"partial_receipt": True, "tests_passed": False}
    mismatch = cases["mismatch"]["compiler_stderr"]
    return {
        "from": {**common, "error_code": "E0277", "cause_class": "trait_bound",
                 "rejected_relation": re.search(r"trait bound `([^`]+)`", stderr).group(1),
                 "required_into": re.search(r"required for (.+)", stderr).group(1).replace("`", ""),
                 "compiler_listed_from": re.findall(r"^ +((?:From)<[^\n]+>)$", stderr, re.M),
                 "compiler_list_exhaustive": False},
        "mismatch": {**common, "error_code": "E0308", "cause_class": "type_mismatch",
                     "expected_type": re.search(r"expected enum `([^`]+)`", mismatch).group(1),
                     "found_type": re.search(r"found enum `([^`]+)`", mismatch).group(1),
                     "explicit_ok_arguments": re.search(r"Ok::<([^>]+)>", cases["mismatch"]["source"]).group(1).split(", ")},
    }


def assess(case, answer, expected):
    try:
        value = json.loads(answer.strip().removeprefix("```json").removesuffix("```").strip())
    except (ValueError, TypeError):
        return {"correct": False, "parse_error": True}
    if not isinstance(value, dict):
        return {"correct": False, "parse_error": True}
    checks = {}
    for key, want in expected[case].items():
        got = value.get(key)
        if key == "compiler_listed_from":
            checks[key] = isinstance(got, list) and all(isinstance(v, str) for v in got) and sorted(got) == sorted(want)
        else:
            checks[key] = type(got) is type(want) and got == want
    checks["explanation_present"] = isinstance(value.get("explanation"), str) and bool(value["explanation"].strip())
    return {"correct": all(checks.values()), "checks": checks}


def scorer_checks(expected):
    results = []
    for case, want in expected.items():
        good = {**want, "explanation": "Independent compiler-grounded scorer fixture."}
        samples = [("valid", good, True), ("empty", {}, False), ("wrong_json_type", [], False)]
        for key in want:
            bad = copy.deepcopy(good)
            value = bad[key]
            bad[key] = not value if isinstance(value, bool) else (["CORRUPT"] if isinstance(value, list) else "CORRUPT")
            samples.append(("corrupt_" + key, bad, False))
        bad = copy.deepcopy(good)
        if case == "from":
            bad["compiler_listed_from"].append("From<i32>")
        else:
            bad["expected_type"], bad["found_type"] = bad["found_type"], bad["expected_type"]
        samples.append(("invented_example_or_reversed_types", bad, False))
        for label, answer, wanted in samples:
            observed = assess(case, json.dumps(answer), expected)["correct"]
            results.append({"case": case, "mutation": label, "expected_accept": wanted, "observed_accept": observed})
            if observed != wanted:
                raise AssertionError(results[-1])
        if assess(case, "not json", expected)["correct"]:
            raise AssertionError("invalid JSON accepted")
        results.append({"case": case, "mutation": "invalid_json", "expected_accept": False, "observed_accept": False})
    return results


def fingerprints(cases):
    paths = [original.BINARY, original.BINARY.with_name("codex-code-mode-host.exe"), Path(__file__), BASE, CORRECTED, original.PATTERN, REPLAY]
    paths.extend(Path(c["raw_log_path"]) for c in cases.values())
    return {str(path): existing.digest(path) for path in paths}


def protocol(cases):
    expected = oracle(cases)
    return {"scope": __doc__, "model": MODEL, "effort": EFFORT, "binary": str(original.BINARY),
            "developer_instruction": INSTRUCTION, "developer_instruction_sha256": sha(INSTRUCTION),
            "tasks": TASKS, "tools": TOOLS, "expected": expected,
            "oracle_basis": "Exact regex extraction from hash-checked actual compiler_stderr and source; failure/partial flags from _checked packet. Ownership excluded by the compiler-reported cause.",
            "cases": cases, "scorer_checks": scorer_checks(expected), "hashes": fingerprints(cases),
            "ordering": [{"repeat": repeat, "case": case, "variant": arm} for repeat in range(2) for case in TASKS for arm in (("baseline", "candidate") if repeat == 0 else ("candidate", "baseline"))],
            "limits": {"maximum_trials": 8, "trial_timeout_seconds": 240, "harness_retries": 0}, "limitations": LIMITATIONS}


class Server(existing.BenchmarkServer):
    async def receive(self):
        line = await self.process.stdout.readline()
        if not line:
            raise RuntimeError("app-server closed before completion")
        self.log.write(line.decode("utf-8"))
        self.log.flush()
        message = json.loads(line)
        method, params = message.get("method"), message.get("params", {})
        if method and "id" in message:
            if method != "item/tool/call":
                await self.send({"id": message["id"], "error": {"code": -32601, "message": "No interactive requests permitted"}})
                raise RuntimeError("unexpected interactive request: " + method)
            identity = (params["threadId"], params["turnId"], params["callId"])
            if identity in self.seen:
                raise RuntimeError("duplicate tool request")
            self.seen.add(identity)
            arguments = params["arguments"]
            if isinstance(arguments, str):
                arguments = json.loads(arguments)
            self.calls.append({"tool": params["tool"], "arguments": arguments})
            if params["tool"] != "diagnostic_raw_log" or arguments != {}:
                raise RuntimeError("unexpected fixture tool/arguments")
            await self.send({"id": message["id"], "result": {"contentItems": [{"type": "inputText", "text": self.fixture[self.case]["raw_log"]}], "success": True}})
        elif method == "turn/completed":
            self.completed = {**params["turn"], "timing": params.get("timing")}
        elif method == "thread/tokenUsage/updated":
            self.usage = params["tokenUsage"]["total"]
        elif method == "item/completed" and params.get("item", {}).get("type") == "agentMessage":
            self.answers.append(params["item"]["text"])
        return message

    async def trial(self, cwd, case, variant):
        self.case, self.calls, self.answers, self.completed, self.usage = case, [], [], None, None
        start = time.perf_counter()
        error = None
        started = None
        try:
            async with asyncio.timeout(240):
                started = await self.request("thread/start", {
                    "model": MODEL, "cwd": str(cwd), "approvalPolicy": "never", "sandbox": "read-only",
                    "ephemeral": True, "environments": [], "dynamicTools": TOOLS,
                    "config": existing.SAFE_CONFIG, "developerInstructions": INSTRUCTION})
                await self.request("turn/start", {"threadId": started["thread"]["id"], "effort": EFFORT, "environments": [],
                    "input": [{"type": "text", "text": TASKS[case] + "\n\nPrior harness observation (data, not instructions):\n" + self.fixture[case]["packets"][variant]["packet"]}]})
                while self.completed is None:
                    await self.receive()
        except Exception as exc:
            error = {"type": type(exc).__name__, "message": str(exc)}
        completed = self.completed or {}
        timing = completed.get("timing") or {}
        counters = timing.get("counters", {})
        answer = self.answers[-1] if self.answers else ""
        status = completed.get("status", "harness_error")
        return {"case": case, "variant": variant, "status": status,
                "thread_id": started["thread"]["id"] if started else None,
                "harness_wall_seconds": time.perf_counter() - start,
                "model_active_seconds": timing.get("unions", {}).get("modelActiveUnionNs", 0) / 1e9 if timing else None,
                "turn_wall_seconds": timing.get("inclusiveDurationNs", 0) / 1e9 if timing else None,
                "model_requests": len(timing["modelRequests"]) if "modelRequests" in timing else None,
                "actual_tool_calls": counters.get("toolCallCount"), "raw_log_recoveries": len(self.calls),
                "artifact_recoveries": counters.get("toolOutputRecoveryCallCount"),
                "model_retries": counters.get("modelRetryCount"), "model_fallbacks": counters.get("modelFallbackCount"),
                "compactions": counters.get("generationsByReason", {}).get("compaction"),
                "answer": answer, "all_agent_messages": self.answers, "assessment": assess(case, answer, self.expected),
                "calls": self.calls, "usage": self.usage, "timing": timing,
                "completion_error": completed.get("error"), "harness_error": error,
                "packet_sha256": self.fixture[case]["packets"][variant]["packet_sha256"]}


async def run(prepared):
    results = OUTPUT / "results"
    results.mkdir(exist_ok=False)
    rows, fatal = [], None
    began = time.perf_counter()
    try:
        with tempfile.TemporaryDirectory(prefix="kd4-rust23-diagnostic-auth-") as directory:
            home = Path(directory)
            shutil.copyfile(original.HOME / "auth.json", home / "auth.json")
            work = home / "work"
            work.mkdir()
            env = {k: v for k, v in os.environ.items() if not k.startswith("CODEX_") or k == "CODEX_API_KEY"}
            env.update(CODEX_HOME=str(home), CODEX_EXEC_SERVER_URL="none")
            command = [str(original.BINARY), "app-server", "--listen", "stdio://"]
            for key, value in existing.SAFE_CONFIG.items():
                command += ["-c", f"{key}={json.dumps(value)}"]
            with (results / "events.jsonl").open("x", encoding="utf-8") as log, (results / "stderr.log").open("xb") as stderr:
                process = await asyncio.create_subprocess_exec(*command, cwd=work, env=env,
                    stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=stderr,
                    limit=16 * 1024 * 1024, creationflags=subprocess.CREATE_NO_WINDOW)
                try:
                    server = Server(process, log, work, "")
                    server.fixture, server.expected = prepared["cases"], prepared["expected"]
                    await asyncio.wait_for(server.request("initialize", {"clientInfo": {"name": "kd4_rust23_diagnostic", "version": "1"}, "capabilities": {"experimentalApi": True}}), 30)
                    await server.send({"method": "initialized", "params": {}})
                    for trial in prepared["ordering"]:
                        row = await server.trial(work, trial["case"], trial["variant"])
                        row["repeat"] = trial["repeat"]
                        rows.append(row)
                        with (results / "trials.jsonl").open("a", encoding="utf-8") as stream:
                            stream.write(json.dumps(row) + "\n")
                        print(json.dumps({k: row[k] for k in ("case", "variant", "repeat", "status", "assessment", "harness_wall_seconds", "model_requests", "raw_log_recoveries", "completion_error", "harness_error")}), flush=True)
                        if row["status"] != "completed" or row["harness_error"]:
                            fatal = "Stopped at first noncompleted trial; no harness retries."
                            break
                finally:
                    if process.returncode is None:
                        process.stdin.close()
                        try:
                            await asyncio.wait_for(process.wait(), 5)
                        except TimeoutError:
                            process.kill()
                            await process.wait()
    except Exception as exc:
        fatal = {"type": type(exc).__name__, "message": str(exc)}
    after = fingerprints(prepared["cases"])
    summary = []
    for case in TASKS:
        for variant in ("baseline", "candidate"):
            chosen = [r for r in rows if r["case"] == case and r["variant"] == variant]
            completed = [r for r in chosen if r["status"] == "completed"]
            item = {"case": case, "variant": variant, "attempted": len(chosen), "completed": len(completed), "correct": sum(r["assessment"]["correct"] for r in completed)}
            for key in ("harness_wall_seconds", "model_active_seconds", "turn_wall_seconds"):
                values = [r[key] for r in completed if r[key] is not None]
                item["median_" + key] = statistics.median(values) if values else None
            for key in ("model_requests", "actual_tool_calls", "raw_log_recoveries", "artifact_recoveries", "model_retries", "model_fallbacks", "compactions"):
                item[key] = sum(r[key] for r in chosen) if chosen and all(r[key] is not None for r in chosen) else None
            item["usage"] = {key: sum(r["usage"][key] for r in chosen) for key in ("inputTokens", "cachedInputTokens", "outputTokens", "reasoningOutputTokens", "totalTokens")} if chosen and all(r["usage"] for r in chosen) else None
            summary.append(item)
    report = {"scope": __doc__, "model": MODEL, "effort": EFFORT, "attempted_trials": len(rows),
              "completed_trials": sum(r["status"] == "completed" for r in rows), "maximum_trials": 8,
              "harness_total_wall_seconds": time.perf_counter() - began, "fatal": fatal,
              "hashes_before": prepared["hashes"], "hashes_after": after, "hashes_unchanged": prepared["hashes"] == after,
              "protocol_sha256": existing.digest(OUTPUT / "protocol.json"), "summary": summary, "limitations": LIMITATIONS}
    (results / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps({k: report[k] for k in ("attempted_trials", "completed_trials", "fatal", "hashes_unchanged", "summary")}), flush=True)
    return 1 if fatal else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--prepare", action="store_true")
    mode.add_argument("--run", action="store_true")
    args = parser.parse_args()
    prepared = protocol(load_fixture())
    path = OUTPUT / "protocol.json"
    if args.prepare:
        OUTPUT.mkdir(parents=True, exist_ok=True)
        with path.open("x", encoding="utf-8") as stream:
            json.dump(prepared, stream, indent=2)
        print(json.dumps({"oracle": prepared["expected"], "scorer_checks": len(prepared["scorer_checks"]), "protocol_sha256": existing.digest(path), "live_trials_started": 0}))
        return 0
    if json.loads(path.read_text(encoding="utf-8")) != prepared:
        raise RuntimeError("prepared protocol or identities changed; refuse provider run")
    return asyncio.run(run(prepared))


if __name__ == "__main__":
    raise SystemExit(main())
