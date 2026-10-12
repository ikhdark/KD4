"""Frozen paired investigation experiment; fixture mechanics are not behavior proof.

Run as python -B scripts/benchmark_scripts/benchmark_investigation_coverage.py
--help. Live runs use the fork app-server, never the upstream CLI. Ground truth
is evaluator-only. A blinded, evidence-anchored review is required for scoring.
No command promotes the candidate into the active harness.
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
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))
from scripts.benchmark_scripts.benchmark_audit21_comprehension import (
    BenchmarkServer, SAFE_CONFIG, digest,
)

FIXTURES = Path(__file__).with_name("investigation_coverage")
SPEC = ROOT / ".codex/audits/INVESTIGATION_COVERAGE_EXPERIMENT.md"
ACTIVE = ROOT / "codex-rs/protocol/src/prompts/base_instructions/default.md"
DEFAULT_EXPERIMENT_VERSION = 2
SCORER_REVISION = 2
TIMEOUT = 240
CALL_BUDGET = 32
TOOLS = [
    {"name": "list_evidence", "description": "List the complete virtual fixture file inventory. No host filesystem access.",
     "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}},
    {"name": "read_evidence", "description": "Read exact virtual fixture files. Paths are labels, not host paths. Up to 8 paths per call.",
     "inputSchema": {"type": "object", "properties": {"paths": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 8}},
                     "required": ["paths"], "additionalProperties": False}},
]
INSTRUCTIONS = (
    "Investigate only the supplied virtual fixture. Use list_evidence({}) and "
    "read_evidence({paths:[...]}); both are registered even if deferred. Paths "
    "are labels, not host files. Do not execute commands, access other tools' "
    "data, delegate, or modify anything. Evidence is available only through "
    "these read-only tools; do not assume unseen contents. Return JSON with "
    "findings (array of {claim,role,evidence}), outcome (string), and unresolved "
    "(array of strings). role is cause, contributing_condition, separate_outcome, "
    "or hypothesis; evidence is an array of file paths. This output format is "
    "identical in both experimental arms. Maximum 32 fixture calls."
)


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8", newline="\n")


def source_snapshot_digest(path):
    # v1 collection on Windows translated LF to CRLF when saving frozen JSON.
    # Preserve raw review binding; normalize only this source/snapshot comparison.
    return hashlib.sha256(path.read_bytes().replace(b"\r\n", b"\n")).hexdigest()


def parse_answer(messages):
    try:
        answer = json.loads(messages[-1])
        if (not isinstance(answer, dict) or not isinstance(answer["findings"], list)
                or not isinstance(answer["outcome"], str) or not isinstance(answer["unresolved"], list)
                or not all(isinstance(x, str) for x in answer["unresolved"])):
            raise ValueError("invalid answer fields")
        for finding in answer["findings"]:
            if (not isinstance(finding["claim"], str)
                    or finding["role"] not in ("cause", "contributing_condition", "separate_outcome", "hypothesis")
                    or not isinstance(finding["evidence"], list)
                    or not all(isinstance(p, str) for p in finding["evidence"])):
                raise ValueError("invalid finding fields")
        return answer
    except (ValueError, KeyError, IndexError, TypeError) as error:
        raise ValueError("malformed final answer cannot establish acceptance") from error


def experiment_paths(version):
    if version == 1:
        return FIXTURES, SPEC
    if version == 2:
        return FIXTURES / "v2", SPEC.with_name("INVESTIGATION_COVERAGE_EXPERIMENT_V2.md")
    raise ValueError(f"unknown experiment version: {version}")


def load_bundle(version=DEFAULT_EXPERIMENT_VERSION, *, require_active=False):
    fixtures, _ = experiment_paths(version)
    bundle = json.loads((fixtures / "cases.json").read_text(encoding="utf-8"))
    if bundle["version"] != version:
        raise ValueError("fixture version does not match selected experiment")
    for arm in ("baseline", "candidate"):
        if digest(fixtures / f"{arm}.md") != bundle[f"{arm}_sha256"]:
            raise ValueError(f"frozen {arm} changed; version the experiment")
    if require_active and digest(ACTIVE) != bundle["baseline_sha256"]:
        raise ValueError("active baseline changed; rebase/version before comparing")
    ids = set()
    for case in bundle["cases"]:
        if case["id"] in ids:
            raise ValueError("duplicate case")
        ids.add(case["id"])
        for required in ("scope", "coverage", "exclusions", "counterfactuals"):
            if not case[required]:
                raise ValueError(f"missing ground-truth coverage: {required}")
        causes = set()
        for cause in case["causes"]:
            if cause["id"] in causes or not cause["claim"] or not cause["materiality"]:
                raise ValueError("invalid cause oracle")
            causes.add(cause["id"])
        for item in case["causes"] + case["other"]:
            if not item["evidence"] or not set(item["evidence"]) <= case["files"].keys():
                raise ValueError("oracle references missing evidence")
    if sum(c["split"] == "heldout" for c in bundle["cases"]) != 3:
        raise ValueError("acceptance requires exactly three frozen held-out cases")
    return bundle


def schedule(bundle):
    rows = []
    for index, case in enumerate(bundle["cases"]):
        for repeat in range(4 if case["split"] == "heldout" else 1):
            arms = ("baseline", "candidate") if (index + repeat) % 2 == 0 else ("candidate", "baseline")
            rows.extend((case["id"], repeat, arm) for arm in arms)
    return rows


class BudgetExceeded(RuntimeError):
    pass


class InvestigationServer(BenchmarkServer):
    """Reuse existing transport; expose only the in-memory fixture projection."""

    def __init__(self, process, log, files):
        super().__init__(process, log, Path("unused"), "")
        self.files = files
        self.protocol_items = []

    def dispatch(self, tool, arguments):
        if len(self.calls) >= CALL_BUDGET:
            raise BudgetExceeded("fixture call budget exhausted")
        self.calls.append({"tool": tool, "arguments": arguments, "read_paths": []})
        if tool == "list_evidence" and arguments == {}:
            return {"paths": sorted(self.files), "complete": True}
        if (tool != "read_evidence" or not isinstance(arguments, dict)
                or set(arguments) != {"paths"} or not isinstance(arguments["paths"], list)
                or not 1 <= len(arguments["paths"]) <= 8
                or not all(isinstance(p, str) for p in arguments["paths"])):
            raise RuntimeError("unexpected tool or arguments; no host operation executed")
        result = []
        for path in arguments["paths"]:
            if path not in self.files:
                result.append({"path": path, "error": "not in virtual fixture"})
            else:
                result.append({"path": path, "text": self.files[path], "complete": True})
                self.calls[-1]["read_paths"].append(path)
        return {"files": result}

    async def receive(self):
        line = await self.process.stdout.readline()
        if not line:
            raise RuntimeError("app-server EOF before completion")
        self.log.write(line.decode("utf-8"))
        self.log.flush()
        message = json.loads(line)
        method, params = message.get("method"), message.get("params", {})
        if method and "id" in message:
            if method != "item/tool/call":
                await self.send({"id": message["id"], "error": {"code": -32601, "message": "No interactive requests"}})
                raise RuntimeError(f"unexpected interactive request: {method}")
            identity = (params["threadId"], params["turnId"], params["callId"])
            if identity in self.seen:
                raise RuntimeError("duplicate fixture call identity")
            self.seen.add(identity)
            arguments = params["arguments"]
            if isinstance(arguments, str):
                arguments = json.loads(arguments)
            value = self.dispatch(params["tool"], arguments)
            await self.send({"id": message["id"], "result": {
                "contentItems": [{"type": "inputText", "text": json.dumps(value)}], "success": True}})
        elif method == "turn/completed":
            self.completed = {**params["turn"], "timing": params.get("timing")}
        elif method == "thread/tokenUsage/updated":
            self.usage = params["tokenUsage"]["total"]
        elif method == "item/completed":
            item = params.get("item", {})
            if item.get("type") == "agentMessage":
                self.answers.append(item["text"])
            elif item.get("type") not in (None, "userMessage", "reasoning"):
                self.protocol_items.append(item.get("type"))
                if item.get("type") in ("commandExecution", "fileChange", "mcpToolCall", "webSearch"):
                    raise RuntimeError("fixture isolation violation")
        return message

    async def trial(self, args, cwd, case, arm, prompt):
        self.calls, self.answers, self.completed, self.usage = [], [], None, None
        self.protocol_items = []
        self.files = case["files"]
        start = time.perf_counter()
        thread = await self.request("thread/start", {
            "model": args.model, "cwd": str(cwd), "approvalPolicy": "never", "sandbox": "read-only",
            "ephemeral": True, "environments": [], "dynamicTools": TOOLS, "config": SAFE_CONFIG,
            "baseInstructions": prompt, "developerInstructions": INSTRUCTIONS,
        })
        if thread.get("model") != args.model:
            raise RuntimeError("requested model was not confirmed; no fallback allowed")
        await self.request("turn/start", {"threadId": thread["thread"]["id"], "effort": args.effort,
            "environments": [], "input": [{"type": "text", "text": case["question"]}]})
        while self.completed is None:
            await self.receive()
        if self.completed["status"] != "completed":
            raise RuntimeError(f"non-completed model turn: {self.completed['status']}")
        return {"case": case["id"], "split": case["split"], "arm": arm,
                "status": "completed", "wall_seconds": time.perf_counter() - start,
                "messages": self.answers, "calls": self.calls, "usage": self.usage,
                "protocol_items": self.protocol_items, "timing": self.completed.get("timing")}


async def close_server(process):
    if process.returncode is None:
        process.stdin.close()
        try:
            await asyncio.wait_for(process.wait(), 5)
        except TimeoutError:
            process.kill()
            await process.wait()


async def execute(args):
    bundle = load_bundle(args.experiment_version, require_active=True)
    fixtures, spec = experiment_paths(args.experiment_version)
    prompts = {arm: (fixtures / f"{arm}.md").read_text(encoding="utf-8") for arm in ("baseline", "candidate")}
    cases = {c["id"]: c for c in bundle["cases"]}
    args.output.mkdir(parents=True, exist_ok=False)
    write_json(args.output / "frozen-cases.json", bundle)
    for arm, text in prompts.items():
        (args.output / f"{arm}.md").write_text(text, encoding="utf-8", newline="\n")
    report = {"status": "running", "model": args.model, "effort": args.effort,
              "experiment_version": bundle["version"], "scorer_revision": SCORER_REVISION,
              "binary_sha256": digest(args.binary), "script_sha256": digest(Path(__file__)),
              "transport_sha256": digest(Path(__file__).with_name("benchmark_audit21_comprehension.py")),
              "spec_sha256": digest(spec), "cases_sha256": digest(fixtures / "cases.json"),
              "baseline_sha256": bundle["baseline_sha256"], "candidate_sha256": bundle["candidate_sha256"],
              "timeout_seconds": TIMEOUT, "call_budget": CALL_BUDGET, "safe_config": SAFE_CONFIG,
              "planned_trials": len(schedule(bundle)), "completed_trials": 0, "startup_seconds": 0.0}
    rows = []
    started = time.perf_counter()
    try:
        with tempfile.TemporaryDirectory(prefix="kd4-investigation-") as directory:
            home = Path(directory)
            shutil.copyfile(args.fork_home / "auth.json", home / "auth.json")
            work = home / "work"
            work.mkdir()
            env = {k: v for k, v in os.environ.items() if not k.startswith("CODEX_") or k == "CODEX_API_KEY"}
            env.update(CODEX_HOME=str(home), CODEX_EXEC_SERVER_URL="none")
            command = [str(args.binary), "app-server", "--listen", "stdio://"]
            for key, value in SAFE_CONFIG.items():
                command += ["-c", f"{key}={json.dumps(value)}"]
            # One owned process; no model work or auth cleanup outlives this block.
            with (args.output / "events.jsonl").open("x", encoding="utf-8") as log, (args.output / "stderr.log").open("xb") as stderr:
                process = await asyncio.create_subprocess_exec(*command, cwd=work, env=env,
                    stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=stderr,
                    limit=16 * 1024 * 1024, creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0)
                try:
                    server = InvestigationServer(process, log, {})
                    await asyncio.wait_for(server.request("initialize", {"clientInfo": {"name": "kd4_investigation_coverage", "version": "1"},
                                              "capabilities": {"experimentalApi": True}}), 30)
                    await server.send({"method": "initialized", "params": {}})
                    report["startup_seconds"] = time.perf_counter() - started
                    for case_id, repeat, arm in schedule(bundle):
                        case = cases[case_id]
                        trial_start = time.perf_counter()
                        try:
                            row = await asyncio.wait_for(server.trial(args, work, case, arm, prompts[arm]), TIMEOUT)
                        except (TimeoutError, BudgetExceeded) as error:
                            row = {"case": case_id, "split": case["split"], "arm": arm,
                                   "status": "timeout" if isinstance(error, TimeoutError) else "budget_exhausted",
                                   "wall_seconds": time.perf_counter() - trial_start, "messages": server.answers,
                                   "calls": server.calls, "usage": server.usage, "protocol_items": server.protocol_items, "timing": None}
                        row.update(repeat=repeat, trial_id=hashlib.sha256(f"{case_id}/{repeat}/{arm}".encode()).hexdigest()[:16])
                        rows.append(row)
                        with (args.output / "trials.jsonl").open("a", encoding="utf-8") as stream:
                            stream.write(json.dumps(row) + "\n")
                        report["completed_trials"] = len(rows)
                        write_json(args.output / "run.json", report)
                        print(json.dumps({k: row[k] for k in ("case", "repeat", "arm", "status", "wall_seconds")}), flush=True)
                        if row["status"] != "completed":
                            # Do not leave a timed-out turn running alongside another trial.
                            raise RuntimeError("bounded trial incomplete; full paired evaluation is pending")
                finally:
                    await close_server(process)
        if digest(args.binary) != report["binary_sha256"]:
            raise RuntimeError("installed binary changed during matched runs")
        load_bundle(args.experiment_version, require_active=True)
        report["status"] = "completed_review_pending"
    except Exception as error:
        report.update(status="blocked", error=f"{type(error).__name__}: {error}")
        raise
    finally:
        report["wall_seconds"] = time.perf_counter() - started
        write_json(args.output / "run.json", report)


def review_packet(output):
    bundle = json.loads((output / "frozen-cases.json").read_text(encoding="utf-8"))
    rows = [json.loads(line) for line in (output / "trials.jsonl").read_text(encoding="utf-8").splitlines()]
    cases = {case["id"]: case for case in bundle["cases"]}
    packets = []
    for row in sorted(rows, key=lambda r: r["trial_id"]):
        case = cases[row["case"]]
        packets.append({"trial_id": row["trial_id"], "case": row["case"], "status": row["status"],
                        "messages": row["messages"], "calls": row["calls"],
                        "oracle": {k: case[k] for k in ("scope", "coverage", "causes", "other", "exclusions")}})
    return {"trials_sha256": digest(output / "trials.jsonl"), "cases_sha256": digest(output / "frozen-cases.json"),
            "review_instructions": "No keyword grading. For each trial supply causes:{id:{stage:not_discovered|unresolved|incorrect|resolved,evidence:[paths],rationale:text}}, claims:[{text,supported:boolean,rationale}], classification_errors:[text], unexplained_observations:[text], omitted_material_evidence:[paths], premature_closure:boolean. Only unresolved (correctly identified but not resolved) and resolved receive discovery credit; incorrect means the identification was rejected and receives none. Include all substantive established claims, not a convenient subset. Anchor discovery to messages, resolution to evidence actually read. All fields mandatory. Do not reveal arms before review is saved.",
            "packets": packets}


def scored_row(row, review, case):
    expected = {c["id"]: c for c in case["causes"]}
    if set(review["causes"]) != set(expected):
        raise ValueError("review must classify every cause")
    read_paths = {p for call in row["calls"] for p in call["read_paths"]}
    discovered = resolved = 0
    for cause_id, judgment in review["causes"].items():
        stage = judgment["stage"]
        if stage not in ("not_discovered", "unresolved", "incorrect", "resolved") or not judgment["rationale"].strip():
            raise ValueError("unreviewed cause")
        if stage in ("unresolved", "resolved"):
            discovered += 1
        if stage == "resolved":
            required = set(expected[cause_id]["evidence"])
            if not required <= set(judgment["evidence"]) <= read_paths:
                raise ValueError("resolved cause lacks required observed evidence")
            resolved += 1
    claims = review["claims"]
    if not isinstance(claims, list):
        raise ValueError("claims review missing")
    for claim in claims:
        if type(claim["supported"]) is not bool or not claim["text"].strip() or not claim["rationale"].strip():
            raise ValueError("invalid claim review")
    for field in ("classification_errors", "unexplained_observations", "omitted_material_evidence"):
        if not isinstance(review[field], list) or not all(isinstance(x, str) and x for x in review[field]):
            raise ValueError(f"invalid {field}")
    if type(review["premature_closure"]) is not bool:
        raise ValueError("invalid closure judgment")
    usage = row["usage"] or {}
    tokens = usage.get("totalTokens")
    if type(tokens) is not int or tokens <= 0 or row["wall_seconds"] <= 0:
        raise ValueError("missing primary cost metric")
    return {"case": row["case"], "arm": row["arm"], "repeat": row["repeat"],
            "recall": discovered / len(expected), "complete": discovered == len(expected),
            "resolved": resolved == len(expected), "claims": len(claims),
            "unsupported": sum(not c["supported"] for c in claims),
            "classification_errors": len(review["classification_errors"]),
            "unexplained": len(review["unexplained_observations"]),
            "premature": int(review["premature_closure"]),
            "omitted_evidence": len(review["omitted_material_evidence"]),
            "wall": row["wall_seconds"], "tokens": tokens, "calls": len(row["calls"])}


def aggregate(rows):
    if not rows:
        raise ValueError("empty arm")
    value = {key: statistics.mean(row[key] for row in rows) for key in ("recall", "complete", "resolved", "tokens", "calls")}
    value["wall"] = statistics.median(row["wall"] for row in rows)
    for key in ("claims", "unsupported", "classification_errors", "unexplained", "premature", "omitted_evidence"):
        value[key] = sum(row[key] for row in rows)
    value["unsupported_rate"] = value["unsupported"] / value["claims"] if value["claims"] else 0
    return value


def decide(rows):
    by_case = {}
    cases = sorted({row["case"] for row in rows})
    for case in cases:
        by_case[case] = {arm: aggregate([r for r in rows if r["case"] == case and r["arm"] == arm]) for arm in ("baseline", "candidate")}
    totals = {arm: aggregate([r for r in rows if r["arm"] == arm]) for arm in ("baseline", "candidate")}
    b, c = totals["baseline"], totals["candidate"]
    checks = {
        "complete_rate": c["complete"] >= .8,
        "complete_gain": c["complete"] - b["complete"] >= .2 - 1e-12,
        "recall_gain": c["recall"] - b["recall"] >= .1 - 1e-12,
        "case_consistency": sum(v["candidate"]["complete"] > v["baseline"]["complete"] for v in by_case.values()) >= 2,
        "no_case_regression": all(v["candidate"][k] >= v["baseline"][k] for v in by_case.values() for k in ("complete", "recall", "resolved")),
        "unsupported_claims": c["unsupported_rate"] <= .05 and c["unsupported_rate"] <= b["unsupported_rate"],
        "quality": all(c[k] <= b[k] for k in ("classification_errors", "unexplained", "premature")),
        "overall_cost": all(c[k] <= 1.5 * b[k] for k in ("wall", "tokens", "calls")),
        "case_cost": all(v["candidate"][k] <= 2 * v["baseline"][k] for v in by_case.values() for k in ("wall", "tokens", "calls")),
    }
    return {"decision": "passes_bounded_gate_no_automatic_promotion" if all(checks.values()) else "retain_baseline",
            "checks": checks, "overall": totals, "by_case": by_case, "rows": rows}


def evaluate(output, review_path):
    run = json.loads((output / "run.json").read_text(encoding="utf-8"))
    if run["status"] != "completed_review_pending":
        raise ValueError("incomplete run cannot pass")
    bundle = json.loads((output / "frozen-cases.json").read_text(encoding="utf-8"))
    version = run.get("experiment_version", 1)
    _, spec = experiment_paths(version)
    if bundle["version"] != version:
        raise ValueError("run and fixture experiment versions differ")
    rows = [json.loads(line) for line in (output / "trials.jsonl").read_text(encoding="utf-8").splitlines()]
    expected = schedule(bundle)
    if [(r["case"], r["repeat"], r["arm"]) for r in rows] != expected or any(r["status"] != "completed" for r in rows):
        raise ValueError("incomplete or reordered paired trials")
    for row in rows:
        parse_answer(row["messages"])
    review = json.loads(review_path.read_text(encoding="utf-8"))
    if (review["trials_sha256"] != digest(output / "trials.jsonl")
            or review["cases_sha256"] != digest(output / "frozen-cases.json")
            or run["cases_sha256"] != source_snapshot_digest(output / "frozen-cases.json")
            or run["spec_sha256"] != digest(spec)):
        raise ValueError("review, oracle, or acceptance specification changed")
    judgments = {r["trial_id"]: r for r in review["reviews"]}
    if len(judgments) != len(rows) or len(review["reviews"]) != len(rows) or set(judgments) != {r["trial_id"] for r in rows}:
        raise ValueError("full nonduplicated review required")
    cases = {c["id"]: c for c in bundle["cases"]}
    scored = [scored_row(r, judgments[r["trial_id"]], cases[r["case"]]) for r in rows]
    result = decide([r for r in scored if cases[r["case"]]["split"] == "heldout"])
    result.update(review_sha256=digest(review_path), run_sha256=digest(output / "run.json"),
                  experiment_version=version, scorer_revision=SCORER_REVISION,
                  evaluation_script_sha256=digest(Path(__file__)),
                  development=[r for r in scored if cases[r["case"]]["split"] == "development"],
                  limitation="Small authored closed-world fixtures, not historical causality or open-world guarantee.")
    write_json(output / "evaluation.json", result)
    return result


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=("check", "run", "review-packet", "score"))
    parser.add_argument("--experiment-version", type=int, choices=(1, 2), default=DEFAULT_EXPERIMENT_VERSION,
                        help="check/run version (default: 2); archived scoring uses the run manifest")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--fork-home", type=Path)
    parser.add_argument("--model")
    parser.add_argument("--effort", choices=("low", "medium", "high", "xhigh"), default="high")
    parser.add_argument("--review", type=Path)
    args = parser.parse_args(argv)
    if args.phase == "check":
        bundle = load_bundle(args.experiment_version, require_active=True)
        print(json.dumps({"experiment_version": bundle["version"], "scorer_revision": SCORER_REVISION,
                          "cases": len(bundle["cases"]), "trials": len(schedule(bundle)), "active_baseline_unchanged": True}))
        return 0
    if args.output is None:
        parser.error("--output required")
    args.output = args.output.resolve()
    if args.phase == "run":
        if not args.binary or not args.fork_home or not args.model:
            parser.error("run requires --binary, --fork-home, --model; live model use may be billable")
        args.binary, args.fork_home = args.binary.resolve(strict=True), args.fork_home.resolve(strict=True)
        asyncio.run(execute(args))
    elif args.phase == "review-packet":
        packet = review_packet(args.output)
        path = args.output / "review-packet.json"
        if path.exists():
            raise ValueError("review packet already exists; do not overwrite review inputs")
        write_json(path, packet)
        print(json.dumps({"path": str(path), "trials": len(packet["packets"])}))
    else:
        if not args.review:
            parser.error("score requires --review")
        result = evaluate(args.output, args.review)
        print(json.dumps({k: result[k] for k in ("decision", "checks", "overall", "by_case")}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
