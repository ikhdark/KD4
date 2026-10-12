"""Controlled projection replay; no compiler, Cargo, provider, or test execution."""

# Support direct execution (including python -I) from any working directory.
if __package__ in (None, ""):
    import sys as _sys
    from pathlib import Path as _Path

    _sys.path.insert(0, str(_Path(__file__).resolve().parents[2]))

import contextlib
import hashlib
import inspect
import io
import json
from pathlib import Path
import statistics
import subprocess
import time
from unittest import mock

from scripts.build import rust_test_runner as runner_module
from scripts.build.test_rust_test_runner import RunnerTestCase

ROOT = Path(__file__).resolve().parents[2]

BASELINE_SOURCE = "def _failure_diagnostic_excerpt(\r\n    result: subprocess.CompletedProcess[str], stream: str, tail: str,\r\n) -> str:\r\n    \"\"\"Display selected diagnostics, never replace retained bytes or test proof.\r\n\r\n    A nextest failure may precede thousands of passes; a linker command may be\r\n    one enormous line. Tail-only receipts make the caller read the log again to\r\n    learn the cause. Keep a bounded, explicitly partial excerpt plus the tail.\r\n    The existing line iterator bounds memory and skips oversized records.\r\n    \"\"\"\r\n    path = getattr(result, f\"{stream}_path\", None)\r\n    if path is None:\r\n        return tail\r\n    heading = \"Selected failure diagnostics (partial; full log below):\\n\"\r\n    separator = \"\\n... remaining log omitted; tail follows ...\\n\"\r\n    tail = tail[-MAX_FAILURE_STREAM_CHARS:]\r\n    tail_size = min(len(tail), MAX_FAILURE_STREAM_CHARS // 4)\r\n    budget = MAX_FAILURE_STREAM_CHARS - len(heading) - len(separator) - tail_size\r\n    selected: list[str] = []\r\n    context = 0\r\n    compiler_context = False\r\n    try:\r\n        # Complete short logs should not be summarized or scanned again.\r\n        if path.stat().st_size <= len(tail.encode(\"utf-8\")):\r\n            return tail\r\n        for raw in _output_lines(result, stream):\r\n            line = re.sub(r\"\\x1b\\[[0-9;]*m\", \"\", raw).rstrip(\"\\r\\n\")\r\n            diagnostic = bool(re.search(\r\n                r\"^\\s*(?:error(?:\\[E\\d+\\])?:|(?:lld-link|rust-lld): error:|\"\r\n                r\"(?:=\\s*)?note: (?:lld-link|rust-lld): error:)|\"\r\n                r\"\\bpanicked at\\b\", line,\r\n            ))\r\n            # Rustc puts causal types, bounds and secondary await/borrow spans\r\n            # beyond the primary caret. Keep its rendered diagnostic block,\r\n            # not just four lines; unrelated warning notes are not evidence\r\n            # for this error. The existing total and per-line bounds still\r\n            # apply, and exact diagnostics remain in the retained log.\r\n            compiler_start = bool(re.match(r\"\\s*error(?:\\[E\\d+\\])?:\", line))\r\n            continuation = compiler_context and (\r\n                not line.strip() or bool(re.match(\r\n                    r\"\\s*(?:\\d+\\s*\\||\\||-->|:::|(?:=\\s*)?(?:note|help):)\", line,\r\n                ))\r\n            )\r\n            compiler_context = compiler_start or continuation\r\n            status = bool(re.match(\r\n                r\"\\s*(?:FAIL|LEAK-FAIL|TIMEOUT|EXECFAIL|ABORT)\\s+\\[\", line,\r\n            ))\r\n            if not (diagnostic or status or context or continuation):\r\n                continue\r\n            if diagnostic:\r\n                context = 4\r\n            elif context:\r\n                context -= 1\r\n            # Bound long assertion values separately so they cannot displace\r\n            # every later failure. This is an excerpt, not exact source data.\r\n            if len(line) > 512:\r\n                line = line[:384] + \" ... [line shortened] ... \" + line[-96:]\r\n            line += \"\\n\"\r\n            if len(line) > budget:\r\n                break\r\n            selected.append(line)\r\n            budget -= len(line)\r\n    except OSError:\r\n        # Diagnostic selection must not mask the command's original failure.\r\n        return tail\r\n    if not selected:\r\n        return tail\r\n    return heading + \"\".join(selected) + separator + (tail[-tail_size:] if tail_size else \"\")\r\n"
BASELINE_ARTIFACT = "01a12e18-a2c8-7dc1-8174-e77b875df07c"
BASELINE_SOURCE_RANGE = {"start": 764, "end": 831}
OUTPUT_DIR = ROOT / ".codex" / "audits" / "rust23-continuation"

def digest(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()

def main():
    historical_path = OUTPUT_DIR / "diagnostic-packets.json"
    historical = json.loads(historical_path.read_text(encoding="utf-8"))
    runner_path = Path(runner_module.__file__)
    runner_hash_before = hashlib.sha256(runner_path.read_bytes()).hexdigest()
    candidate_function = runner_module._failure_diagnostic_excerpt
    candidate_source = inspect.getsource(candidate_function)
    scope = dict(vars(runner_module))
    exec(compile(BASELINE_SOURCE, "<retained-pre-edit-diagnostic-selector>", "exec"), scope)
    baseline_function = scope["_failure_diagnostic_excerpt"]
    selectors = {"baseline": baseline_function, "candidate": candidate_function}
    fixture = RunnerTestCase()
    fixture.setUp()
    cases = []
    try:
        for case in historical["cases"]:
            diagnostic = case["observed_compiler_stderr"]
            log_path = OUTPUT_DIR / ("replay-" + case["name"] + ".log")
            raw = log_path.read_text(encoding="utf-8")
            expected_raw = (
                diagnostic
                + historical["projection_setup"]["appended_warning_line"] * historical["projection_setup"]["appended_warning_count"]
                + historical["projection_setup"]["final_line"]
            )
            if raw != expected_raw:
                raise RuntimeError("Replay input does not match captured diagnostic and declared warning tail")
            raw_hash = hashlib.sha256(log_path.read_bytes()).hexdigest()
            required = case["independent_required_strings"]
            for phrase in required:
                if phrase not in diagnostic:
                    raise RuntimeError("Independent compiler oracle lacks required phrase: " + phrase)
            packets = {}
            runs = []
            for pair in range(3):
                order = ["baseline", "candidate"] if pair % 2 == 0 else ["candidate", "baseline"]
                for arm in order:
                    result = subprocess.CompletedProcess([], case["compiler_exit_code"], "", raw[-runner_module.MAX_FAILURE_STREAM_CHARS:])
                    result.stderr_path = log_path
                    runner, _ = fixture.runner()
                    runner.executor = mock.Mock(return_value=result)
                    phase_stderr = io.StringIO()
                    with mock.patch.object(runner_module, "_failure_diagnostic_excerpt", selectors[arm]), contextlib.redirect_stderr(phase_stderr):
                        started = time.perf_counter_ns()
                        try:
                            runner._checked(["cargo", "nextest", "list"], env={}, capture=runner_module.CAPTURE_STDOUT)
                        except runner_module.RunnerError as error:
                            packet = str(error)
                            same_result = error.result is result
                        else:
                            raise RuntimeError("Expected retained compiler failure")
                        elapsed_ns = time.perf_counter_ns() - started
                    missing = [phrase for phrase in required if phrase not in packet]
                    proofs = list(runner_module._nextest_results(result))
                    raw_unchanged = hashlib.sha256(log_path.read_bytes()).hexdigest() == raw_hash
                    if arm == "candidate" and missing:
                        raise RuntimeError("Candidate lost compiler evidence: " + repr(missing))
                    if arm == "baseline" and not missing:
                        raise RuntimeError("Baseline no longer reproduces the observed evidence loss")
                    if runner.executor.call_count != 1 or not raw_unchanged or proofs or not same_result:
                        raise RuntimeError("Replay altered execution/proof/raw retention semantics")
                    if arm in packets and packets[arm]["packet"] != packet:
                        raise RuntimeError("Repeated projection packets are not identical")
                    packets.setdefault(arm, {
                        "packet": packet, "packet_sha256": digest(packet),
                        "packet_bytes": len(packet.encode("utf-8")),
                        "missing_required_strings": missing,
                    })
                    runs.append({
                        "pair": pair + 1, "pair_order": order, "arm": arm,
                        "checked_nanoseconds": elapsed_ns,
                        "phase_stderr": phase_stderr.getvalue(),
                        "executor_calls": runner.executor.call_count,
                        "raw_unchanged": raw_unchanged, "test_proof": proofs,
                        "exception_result_is_input": same_result,
                        "packet_sha256": digest(packet),
                    })
            cases.append({
                "name": case["name"], "source": case["source"],
                "source_sha256": digest(case["source"]),
                "compiler_stderr": diagnostic, "compiler_stderr_sha256": digest(diagnostic),
                "raw_log_path": str(log_path), "raw_log_sha256": raw_hash,
                "required_strings": required, "semantic_rubric": case["semantic_rubric"],
                "packets": packets, "timed_runs": runs,
                "median_checked_nanoseconds": {
                    arm: statistics.median(run["checked_nanoseconds"] for run in runs if run["arm"] == arm)
                    for arm in selectors
                },
            })
    finally:
        fixture.doCleanups()
    runner_hash_after = hashlib.sha256(runner_path.read_bytes()).hexdigest()
    if runner_hash_before != runner_hash_after:
        raise RuntimeError("Runner source changed during replay")
    if runner_module._failure_diagnostic_excerpt is not candidate_function:
        raise RuntimeError("Replay did not restore current selector")
    report = {
        "schema_version": 1, "experiment": "fresh_controlled_projection_replay",
        "original_live_packets": False,
        "historical_observations_sha256": hashlib.sha256(historical_path.read_bytes()).hexdigest(),
        "launcher_stub": "Existing RunnerTestCase runner with Mock(return_value=CompletedProcess); no Cargo/rustc launched.",
        "real_owner_path": "RustTestRunner._checked -> _failure_detail -> _failure_diagnostic_excerpt",
        "baseline": {"source": BASELINE_SOURCE, "source_sha256": digest(BASELINE_SOURCE),
                     "retained_artifact": BASELINE_ARTIFACT, "retained_source_lines": BASELINE_SOURCE_RANGE},
        "candidate": {"source": candidate_source, "source_sha256": digest(candidate_source)},
        "shared_runner_source_sha256": runner_hash_before,
        "shared_owner_source_sha256": {
            name: digest(inspect.getsource(getattr(runner_module.RustTestRunner, name)))
            for name in ["_checked", "_failure_detail"]
        },
        "cases": cases,
        "limitations": [
            "Fresh replay packets, not recovered original baseline/candidate live invocation packets.",
            "Compiler diagnostics are previously captured rustc 1.98.1 decoded stderr; no new compilation.",
            "Only the selector varies; current launcher, reporting, retention, and proof code is shared.",
            "Three alternating timed pairs per case measure local projection path only, not model latency, compiler latency or end-to-end task speed.",
            "No provider calls or passing tests rerun.",
        ],
    }
    print(json.dumps(report, ensure_ascii=False, indent=2))

if __name__ == "__main__":
    main()
