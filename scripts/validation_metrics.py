#!/usr/bin/env python3
"""Validation work/proof ledger and deterministic, evidence-conservative reports."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import sys
import time
import uuid
from collections import Counter
from contextlib import contextmanager
from datetime import datetime, timezone
from pathlib import Path

try:
    from .atomic_json import write_json_atomic
except ImportError:
    from atomic_json import write_json_atomic

KIND = "codex_validation_metrics_v1"
REF_KIND = "codex_validation_metrics_ref_v1"
PHASES = ("admission", "preparation", "provenance", "reconciliation")
REASONS = (
    "first_necessary_execution", "inputs_changed", "previous_stale",
    "previous_failed", "previous_interrupted", "dependency_invalidated",
    "redundant", "unknown",
)


def digest(value):
    return hashlib.sha256(json.dumps(
        value, sort_keys=True, ensure_ascii=True, separators=(",", ":")
    ).encode()).hexdigest()


class ValidationMetrics:
    """Observation only: never decides whether to run, skip, or reuse a check."""

    def __init__(self, argv, *, clock=time.monotonic):
        self.clock = clock
        self.started = clock()
        self.path = None
        self.record = {
            "kind": KIND, "run_id": str(uuid.uuid4()),
            "producer": "rust_test_runner", "mode": "executed",
            "argv": list(argv), "command_digest": digest(list(argv)),
            "working_directory": str(Path.cwd().resolve()),
            "started_at": datetime.now(timezone.utc).isoformat(),
            "finished_at": None, "outcome": "running",
            "lifecycle_seconds": None,
            "phases": {name: None for name in PHASES},
            "commands": [], "prerequisite_ids": None,
            "input_digest": None, "input_coverage": "unknown",
            "dependency_manifest": None,
            "proof": {
                "obligations": [], "completed_tests": {},
                "covered_paths": None, "coverage": "unknown",
                "status": "unknown", "freshness": "unknown",
                "freshness_basis": None, "reused_from_run_id": None,
            },
            "reason": "unknown", "reason_basis": None,
        }

    @contextmanager
    def phase(self, name):
        started = self.clock()
        try:
            yield
        finally:
            self.record["phases"][name] = (
                (self.record["phases"].get(name) or 0) + self.clock() - started
            )

    def bind(self, target_dir):
        self.path = Path(target_dir) / "test-runner-logs" / (
            "validation-" + self.record["run_id"] + ".json"
        )
        self.checkpoint()

    def checkpoint(self):
        if self.path is not None:
            try:
                write_json_atomic(self.path, self.record)
            except OSError as error:
                # Metrics must not turn passed validation into a failed command.
                print(f"Validation metrics retention unavailable: {error}", file=sys.stderr)
                self.path = None

    def dependencies(self, manifest):
        self.record["input_digest"] = digest(manifest)
        self.record["input_coverage"] = manifest.get("coverage", "unknown")
        # This is declared provenance, not the files/surfaces proved by tests.
        self.record["dependency_manifest"] = manifest

    def command(self, argv, phase, attempt):
        row = {
            "command_id": str(uuid.uuid4()), "argv": list(argv),
            "command_digest": digest(list(argv)), "phase": phase,
            "attempt": attempt, "outcome": "running", "exit_code": None,
            "wall_seconds": None, "reported_build_seconds": None,
            "reported_test_seconds": None, "process_wait_seconds": None,
            "cleanup_seconds": None,
        }
        self.record["commands"].append(row)
        self.checkpoint()
        return row

    def completed(self, receipts, obligations):
        self.record["proof"]["completed_tests"] = receipts
        self.record["proof"]["obligations"] = list(obligations)
        # Exact executed test IDs are proof of execution, not current revision coverage.
        self.record["proof"]["status"] = "passed" if any(receipts.values()) else "unknown"

    def finish(self, outcome):
        self.record["outcome"] = outcome
        self.record["lifecycle_seconds"] = self.clock() - self.started
        self.record["finished_at"] = datetime.now(timezone.utc).isoformat()
        if outcome != "passed":
            self.record["proof"]["status"] = "pending" if outcome == "busy" else outcome
        self.checkpoint()
        try:
            raw = self.path.read_bytes() if self.path is not None else None
        except OSError:
            raw = None
        if raw is None:
            print(json.dumps(self.record, sort_keys=True), file=sys.stderr)
        else:
            print(json.dumps({
                "kind": REF_KIND, "run_id": self.record["run_id"],
                "path": str(self.path.resolve()), "bytes": len(raw),
                "sha256": hashlib.sha256(raw).hexdigest(),
            }, sort_keys=True), file=sys.stderr)


def _duration(value):
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError("duration must be numeric or null")
    if not math.isfinite(value) or value < 0:
        raise ValueError("duration must be finite and nonnegative")
    return value


def _sum_known(values):
    values = list(values)
    known = [v for v in values if _duration(v) is not None]
    return {
        "seconds": sum(known) if known else None,
        "measured": len(known), "unknown": len(values) - len(known),
    }


def _unattributed(row):
    wall = row.get("lifecycle_seconds")
    if wall is None:
        return None
    # These runner phases are disjoint. Unobserved phases stay in the residual;
    # nested child cleanup/build/test times are deliberately not subtracted.
    measured = sum(value or 0 for value in row["phases"].values())
    measured += sum(command.get("wall_seconds") or 0 for command in row["commands"])
    return wall - measured if measured <= wall else None


def _timestamp(value):
    if not isinstance(value, str):
        raise ValueError("timestamp must be an ISO-8601 string")
    result = datetime.fromisoformat(value)
    if result.tzinfo is None:
        raise ValueError("timestamp must include its timezone")
    return result


def _validate(row):
    if not isinstance(row, dict) or row.get("kind") != KIND:
        raise ValueError("expected a validation ledger object")
    if not isinstance(row.get("run_id"), str) or not row["run_id"]:
        raise ValueError("expected a nonempty run_id")
    for key in ("phases", "proof"):
        if not isinstance(row.get(key), dict):
            raise ValueError(f"{key} must be an object")
    if not isinstance(row.get("commands"), list) or any(
        not isinstance(command, dict) for command in row["commands"]
    ):
        raise ValueError("commands must be a list of objects")
    if row.get("reason_basis") is not None and not isinstance(row["reason_basis"], dict):
        raise ValueError("reason_basis must be an object or null")
    _timestamp(row["started_at"])
    if row.get("finished_at") is not None:
        if _timestamp(row["finished_at"]) < _timestamp(row["started_at"]):
            raise ValueError("finish timestamp precedes launch")
    _duration(row.get("lifecycle_seconds"))
    for duration in row["phases"].values():
        _duration(duration)


def summarize(records):
    """Deduplicate by identity; never infer proof or redundancy from command text."""
    unique = {}
    duplicates = 0
    for row in records:
        _validate(row)
        key = row["run_id"]
        if key in unique:
            if unique[key] != row:
                raise ValueError(f"conflicting records for run_id {key}")
            duplicates += 1
        unique[key] = row
    rows = sorted(unique.values(), key=lambda row: (_timestamp(row["started_at"]), row["run_id"]))
    executed = [row for row in rows if row["mode"] == "executed"]
    reused = [row for row in rows if row["mode"] == "reused"]
    if len(executed) + len(reused) != len(rows):
        raise ValueError("mode must be executed or reused")
    commands = [command for row in executed for command in row["commands"]]
    work = {"lifecycle": _sum_known(row["lifecycle_seconds"] for row in executed)}
    work["unattributed_lifecycle"] = _sum_known(_unattributed(row) for row in executed)
    for phase in PHASES:
        work[phase] = _sum_known(row["phases"].get(phase) for row in executed)
    for field in ("wall_seconds", "reported_build_seconds", "reported_test_seconds",
                  "process_wait_seconds", "cleanup_seconds"):
        work[field] = _sum_known(command.get(field) for command in commands)
    # Cargo's Finished duration is build wall time, not pure compiler CPU/wall.
    work["compile_seconds"] = {"seconds": None, "measured": 0, "unknown": len(commands)}
    reasons = Counter()
    proof_states = Counter()
    new_current_proof = 0
    successful_validation_work = []
    classified = []
    repeat_observations = []
    prior_by_command = {}
    for row in rows:
        proof = row["proof"]
        freshness = proof["freshness"]
        # A producer must supply a revision-bound basis, not merely say "current".
        current = (
            freshness == "current" and bool(proof.get("freshness_basis"))
            and row.get("input_coverage") == "complete"
            and bool(row.get("input_digest"))
            and proof.get("coverage") == "verified"
            and bool(proof.get("obligations"))
        )
        proof_states["current" if current else "stale" if freshness == "stale" else "unknown"] += 1
        if row["mode"] == "executed":
            reason = row.get("reason", "unknown")
            if reason not in REASONS or not row.get("reason_basis"):
                reason = "unknown"
            # Chronology alone is not why a command ran: selected inputs may omit
            # an intervening run, and a planned repetition may be intentional.
            command_key = (row.get("working_directory"), row["command_digest"])
            previous = prior_by_command.get(command_key)
            if (
                reason == "unknown" and command_key[0] and previous
                and previous.get("finished_at")
                and _timestamp(previous["finished_at"]) <= _timestamp(row["started_at"])
                and previous["proof"].get("obligations") == proof.get("obligations")
                and proof.get("obligations")
            ):
                repeat_observations.append({
                    "run_id": row["run_id"], "previous_observed_run_id": previous["run_id"],
                    "previous_outcome": previous["outcome"], "causal_reason_verified": False,
                })
            # A redundant classification requires already-current prior proof of
            # this exact obligation/input/prerequisite identity.
            if reason == "redundant":
                prior = unique.get(row["reason_basis"].get("prior_run_id"))
                before = prior.get("proof", {}) if prior else {}
                if not (
                    prior and _timestamp(prior["started_at"]) < _timestamp(row["started_at"])
                    and prior.get("finished_at")
                    and _timestamp(prior["finished_at"]) <= _timestamp(row["started_at"])
                    and prior["outcome"] == "passed"
                    and before.get("status") == "passed"
                    and row["reason_basis"].get("prior_proof_current_at_launch") is True
                    and row["reason_basis"].get("no_repeat_requirement") is True
                    and row.get("working_directory")
                    and row["working_directory"] == prior.get("working_directory")
                    and row.get("input_coverage") == prior.get("input_coverage") == "complete"
                    and row.get("input_digest") and row["input_digest"] == prior.get("input_digest")
                    and row["command_digest"] == prior["command_digest"]
                    and proof.get("obligations") == before.get("obligations")
                    and proof.get("obligations")
                    and row.get("prerequisite_ids") is not None
                    and row["prerequisite_ids"] == prior.get("prerequisite_ids")
                    and before.get("coverage") == "verified"
                    and before.get("covered_paths")
                    and proof.get("covered_paths") == before["covered_paths"]
                    and before.get("freshness_basis")
                ):
                    reason = "unknown"
            reasons[reason] += 1
            classified.append((row, reason))
            prior_by_command[command_key] = row
            if current and row["outcome"] == "passed" and proof["status"] == "passed" and reason != "redundant":
                new_current_proof += 1
                successful_validation_work.append(row["lifecycle_seconds"])
    work["redundant_lifecycle"] = _sum_known(
        row["lifecycle_seconds"] for row, reason in classified if reason == "redundant"
    )
    work["unclassified_lifecycle"] = _sum_known(
        row["lifecycle_seconds"] for row, reason in classified if reason == "unknown"
    )
    work["successful_current_validation_lifecycle"] = _sum_known(successful_validation_work)
    return {
        "schema_version": 1, "launches": len(executed), "reuse_decisions": len(reused),
        "child_commands": len(commands), "duplicate_records": duplicates,
        "outcomes": dict(sorted(Counter(row["outcome"] for row in executed).items())),
        "work": work, "proof_freshness": dict(sorted(proof_states.items())),
        "launches_producing_current_proof": new_current_proof,
        "reasons": dict(sorted(reasons.items())),
        "justified_reruns": sum(reasons[k] for k in (
            "inputs_changed", "previous_stale", "previous_failed",
            "previous_interrupted", "dependency_invalidated",
        )),
        "redundant_reruns": reasons["redundant"],
        "classification": [{"run_id": row["run_id"], "reason": reason} for row, reason in classified],
        "repeat_observations": repeat_observations,
        "records": rows,
        "limitations": [
            "Ledger producer assertions are observations, not permission to reuse or skip checks.",
            "Current means at the explicit freshness basis, not automatically at report time.",
            "Child command wall sums are work, not elapsed critical-path time.",
            "Successful current-validation lifecycle is summed work, not time to proof; use turn timing for end-to-end comparisons including failures, repair/model gaps, requests, tool calls and correctness.",
            "Reported build/test and cleanup/process times are subsets of command wall; do not add them.",
            "Missing instrumentation is unknown, not zero. Old launches cannot be reconstructed from summary text.",
            "Preparation, provenance and reconciliation exclude child command wall when produced by the Rust runner.",
        ],
    }


def _packets(value):
    """Visit output carriers only, not arbitrary source strings or command arguments."""
    if isinstance(value, str):
        try:
            parsed = json.loads(value)
        except ValueError:
            for line in value.splitlines():
                try:
                    parsed = json.loads(line)
                except ValueError:
                    continue
                if not isinstance(parsed, str):
                    yield from _packets(parsed)
        else:
            if not isinstance(parsed, str):
                yield from _packets(parsed)
    elif isinstance(value, list):
        for child in value:
            yield from _packets(child)
    elif isinstance(value, dict):
        kind = value.get("kind")
        if isinstance(kind, str) and kind in {KIND, REF_KIND}:
            yield value
        else:
            for key in ("output", "stdout", "stderr", "text", "content", "result", "value"):
                if key in value:
                    yield from _packets(value[key])


def read_inputs(paths):
    try:
        from .rollout_snapshot import hydrate_rollout_record, read_rollout_snapshot
    except ImportError:
        from rollout_snapshot import hydrate_rollout_record, read_rollout_snapshot
    records, coverage, unresolved = [], [], []
    candidates = set()
    for path in paths:
        if path.is_dir():
            candidates.update(path.rglob("validation-*.json"))
            candidates.update(path.rglob("*.jsonl"))
            candidates.update(path.rglob("*.jsonl.zst"))
        elif path.is_file():
            candidates.add(path)
        else:
            raise ValueError(f"input does not exist: {path}")
    for path in sorted(candidates, key=str):
        if path.name.endswith((".jsonl", ".jsonl.zst")):
            with read_rollout_snapshot(path) as snapshot:
                count = 0
                for line, row, error, _ in snapshot.decoded_lines():
                    count += 1
                    if error:
                        unresolved.append({"path": str(path), "line": line, "error": error})
                        continue
                    try:
                        row = hydrate_rollout_record(row, path)
                        if not isinstance(row, dict):
                            raise ValueError("rollout record must be an object")
                    except (OSError, ValueError, TypeError, KeyError) as error:
                        unresolved.append({"path": str(path), "line": line, "error": str(error)})
                        continue
                    if row.get("kind") == KIND:
                        records.append(row)
                    elif row.get("type") == "response_item":
                        payload = row.get("payload", {})
                        if not isinstance(payload, dict):
                            unresolved.append({"path": str(path), "line": line,
                                               "error": "response_item payload must be an object"})
                            continue
                        if payload.get("type") in ("function_call_output", "custom_tool_call_output"):
                            for packet in _packets(payload.get("output")):
                                if packet["kind"] == KIND:
                                    records.append(packet)
                                else:
                                    try:
                                        data = Path(packet["path"]).read_bytes()
                                        if len(data) != packet["bytes"] or hashlib.sha256(data).hexdigest() != packet["sha256"]:
                                            raise ValueError("validation ledger reference hash/size mismatch")
                                        record = json.loads(data)
                                        _validate(record)
                                        if record.get("run_id") != packet["run_id"]:
                                            raise ValueError("validation ledger reference run_id mismatch")
                                        records.append(record)
                                    except (OSError, ValueError, TypeError, KeyError) as error:
                                        unresolved.append({"path": str(path), "line": line, "error": str(error)})
                coverage.append({"path": str(path), "records": count, "bytes": snapshot.byte_length, "sha256": snapshot.sha256})
        else:
            data = path.read_bytes()
            records.append(json.loads(data))
            coverage.append({"path": str(path), "records": 1, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()})
    return records, coverage, unresolved


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", type=Path, nargs="*", help="Ledger files/directories or rollout JSONL snapshots")
    parser.add_argument("--describe", action="store_true", help="Describe metrics and their evidence requirements")
    parser.add_argument("--json", action="store_true", help="Emit the complete machine-readable report")
    args = parser.parse_args(argv)
    if args.describe:
        print(json.dumps({
            "kind": KIND, "inputs": "explicit ledgers, ledger directories, or rollout JSONL(.zst)",
            "producer": "rust_test_runner CLI run-target/run-gate/check-gates",
            "identity": ["run_id", "command_digest", "input_digest", "prerequisite_ids"],
            "work": ["admission", "preparation", "provenance", "reconciliation",
                     "command_wall", "reported_build", "reported_test", "cleanup", "process_wait"],
            "proof": ["obligations", "completed_tests", "covered_paths", "coverage", "status", "freshness", "reused_from_run_id"],
            "reasons": REASONS,
            "unknowns": "null; freshness/coverage and repeat reasons require independent evidence",
            "compatibility": "no validation scheduling, replay, or runtime proof policy changes",
        }, indent=2))
        return 0
    if not args.paths:
        parser.error("provide ledger/rollout paths or --describe")
    try:
        records, coverage, unresolved = read_inputs(args.paths)
        report = summarize(records)
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"validation_metrics: {error}", file=sys.stderr)
        return 2
    report["coverage"] = coverage
    report["unresolved"] = unresolved
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(f"{report['launches']} launches; {report['child_commands']} child commands; {report['reuse_decisions']} reuse decisions")
        for field, metric in report["work"].items():
            seconds = "unknown" if metric["seconds"] is None else f"{metric['seconds']:.3f}s"
            print(f"{field}: {seconds} ({metric['measured']} measured, {metric['unknown']} unknown)")
        print(f"Current proof: {report['launches_producing_current_proof']}; justified reruns: {report['justified_reruns']}; redundant: {report['redundant_reruns']}")
        print("Reasons: " + json.dumps(report["reasons"], sort_keys=True))
        print(f"Coverage: {len(coverage)} inputs; {len(unresolved)} unresolved references/records")
        for limit in report["limitations"]:
            print(f"- {limit}")
    return 1 if unresolved else 0


if __name__ == "__main__":
    raise SystemExit(main())

