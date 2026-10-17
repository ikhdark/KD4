"""Corpus measurements for the 2026-10-16 Code Mode host-only audit.

usage: python -I -B code_mode_host_audit_corpus.py --home CODEX_HOME [--json OUT]

Reads every rollout under CODEX_HOME/sessions and CODEX_HOME/archived_sessions
once; `rollout_payload_artifact` stubs resolve from CODEX_HOME/rollout-payloads.
Read-only. Reports, for code-mode `exec` cells and `wait` calls:

usage      cells, waits, first-line pragmas (`persist`, `deliver`) and lexical
           `store(` / `load(` use. Lexical counts are estimates.
events     model-visible exec/wait outputs under 6,000 bytes that contain a
           host-error, recovery or store-commit marker. Short outputs can still
           be source reads; inspect the samples before counting one.
generations
           cell IDs in `Script ... with cell ID` headers. A host session's first
           generation uses the same `<namespace>/<n>` form as the in-process
           runtime, so in-process fallback is not visible here (it is logged
           only); a `g<N>:` prefix marks a cell from a reopened host connection.
receipts   top-level JSON receipt lines the harness appends to exec/wait output,
           with their share of output bytes.
directory  `nested_result_recovery_directory` entries split into pure duplicates
           (the same call shown inline untruncated), recoveries of a truncated
           inline preview, and results not shown inline; payload kinds; and
           artifact ids from receipts that a later exec cell in the same rollout
           references.
concurrency
           peak number of threads with recent activity: each rollout is split at
           idle gaps over 5 minutes and each segment is extended by the
           app-server's 5-minute unload delay. `all` combines processes;
           `by_process` approximates one host per process: every thread of an
           app-server originator (Codex Desktop) shares one process, and each
           `codex exec` root thread with its subagents is its own process.
"""

from __future__ import annotations

import argparse
import collections
import datetime
import json
import pathlib
import re

MARKERS = (
    "code-mode host",  # transport, spawn, admission and protocol errors
    "stale code-mode host generation",
    "exec runtime ended unexpectedly",
    "cancellation cleanup timed out",
    "exec_result_unavailable",
    "code_mode_store_commit_failure",
    "Named-state persistence was not enabled",
    "durability is unconfirmed",
    "owned by another runtime",
)
SHORT_OUTPUT_BYTES = 6_000
CELL_ID = re.compile(r"^Script (?:running|terminated|completed|failed) with cell ID (\S+)")
GENERATION = re.compile(r"g(\d+):")
ARTIFACT_ID = re.compile(r'"artifact_id":\s*"([0-9a-f-]{36})"')
STORE = re.compile(r"(?<![\w.])store\s*\(")
LOAD = re.compile(r"(?<![\w.])load\s*\(")
RETAINED_KEYS = {"call_id", "tool_name", "output_truncated", "result"}
UNLOAD_DELAY = datetime.timedelta(minutes=5)  # app-server THREAD_UNLOADING_DELAY


def peak_concurrency(segments: list[tuple[datetime.datetime, datetime.datetime]]) -> dict:
    events = sorted([(start, 1) for start, _ in segments] + [(end, -1) for _, end in segments])
    live = peak = 0
    peak_at = None
    daily: dict[str, int] = collections.defaultdict(int)
    for moment, delta in events:
        live += delta
        if live > peak:
            peak, peak_at = live, moment.isoformat()
        day = moment.date().isoformat()
        daily[day] = max(daily[day], live)
    return {"segments": len(segments), "peak": peak, "peak_at": peak_at, "daily_peaks": dict(sorted(daily.items()))}


def process_key(thread_id: str, threads: dict[str, dict]) -> str:
    seen = set()
    thread = threads.get(thread_id, {})
    while thread.get("parent") in threads and thread["parent"] not in seen:
        seen.add(thread["parent"])
        thread = threads[thread["parent"]]
    originator = thread.get("originator") or "unknown"
    return f"{originator}:{thread.get('id')}" if originator == "codex_exec" else originator


def concurrency_by_process(segments: list, threads: dict[str, dict]) -> dict:
    grouped = collections.defaultdict(list)
    for start, end, thread_id in segments:
        grouped[process_key(thread_id, threads)].append((start, end))
    results = {key: peak_concurrency(spans) for key, spans in grouped.items()}
    exec_peaks = [result["peak"] for key, result in results.items() if key.startswith("codex_exec:")]
    summary = {key: result for key, result in results.items() if not key.startswith("codex_exec:")}
    summary["codex_exec"] = {"processes": len(exec_peaks), "max_peak": max(exec_peaks, default=0)}
    return summary


def resolve(home: pathlib.Path, record: dict) -> tuple[str | None, dict]:
    if record.get("type") == "rollout_payload_artifact":
        payload = record.get("payload") or {}
        path = home / "rollout-payloads" / f"{payload.get('sha256')}.json"
        try:
            return payload.get("item_type"), json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return "missing", {}
    return record.get("type"), record


def receipt_kind(value: dict) -> str | None:
    if "nested_result_recovery_directory" in value:
        return "recovery_directory"
    if RETAINED_KEYS <= value.keys():
        return "retained_nested_result"
    if "nested_command_failure" in value:
        return "nested_command_failure"
    if value.get("nested_command_display_reduced"):
        return "nested_command_display_reduced"
    if "delivery_refused" in value:
        return "delivery_refused"
    if value.get("output_complete") is False and "output_loss" in value:
        return "output_loss"
    if {"session_id", "session_capabilities", "continuation"} == value.keys():
        return "session_capabilities_receipt"
    return None


def json_lines(text: str):
    for line in text.split("\n"):
        stripped = line.strip()
        if stripped.startswith("{") and stripped.endswith("}"):
            try:
                value = json.loads(stripped)
            except ValueError:
                continue
            if isinstance(value, dict):
                yield line, value


def scan(home: pathlib.Path) -> dict:
    usage = collections.Counter()
    headers = collections.Counter()
    receipts = collections.Counter()
    receipt_bytes = collections.Counter()
    outputs_with = collections.Counter()
    directory = collections.Counter()
    followed = collections.Counter()
    generations = collections.Counter()
    reopened = []
    segments = []
    threads: dict[str, dict] = {}
    events = collections.defaultdict(list)
    first = last = None
    for root in ("sessions", "archived_sessions"):
        for path in sorted((home / root).rglob("rollout-*.jsonl")):
            usage["rollouts"] += 1
            stamp = path.name[8:27]
            first = min(first or stamp, stamp)
            last = max(last or stamp, stamp)
            calls: dict[str, str] = {}
            pending: dict[str, str] = {}
            max_generation = 1
            times = []
            thread = {"id": path.stem[-36:], "originator": None, "parent": None}
            with path.open(encoding="utf-8") as handle:
                for raw in handle:
                    try:
                        record = json.loads(raw)
                    except ValueError:
                        continue
                    if record.get("timestamp"):
                        times.append(datetime.datetime.fromisoformat(record["timestamp"].replace("Z", "+00:00")))
                    kind, body = resolve(home, record)
                    payload = body.get("payload", body) if isinstance(body, dict) else {}
                    if kind == "session_meta" and isinstance(payload, dict) and thread["originator"] is None:
                        thread["originator"] = payload.get("originator")
                        spawn = (payload.get("source") or {}) if isinstance(payload.get("source"), dict) else {}
                        thread["parent"] = (spawn.get("subagent") or {}).get("thread_spawn", {}).get("parent_thread_id")
                    if kind != "response_item" or not isinstance(payload, dict):
                        continue
                    item = payload.get("type")
                    if item == "custom_tool_call" and payload.get("name") == "exec":
                        calls[payload.get("call_id")] = "exec"
                        usage["exec_cells"] += 1
                        source = payload.get("input") or ""
                        head = source.lstrip()[:200]
                        if head.startswith("// @exec:"):
                            usage["pragma"] += 1
                            compact = head.replace(" ", "")
                            usage["pragma_persist"] += '"persist":true' in compact
                            usage["pragma_deliver"] += '"deliver"' in compact
                        usage["cells_lexically_using_store"] += bool(STORE.search(source))
                        usage["cells_lexically_using_load"] += bool(LOAD.search(source))
                        for artifact, origin in list(pending.items()):
                            if artifact in source:
                                followed[origin] += 1
                                del pending[artifact]
                    elif item == "function_call" and payload.get("name") == "wait":
                        calls[payload.get("call_id")] = "wait"
                        usage["wait_calls"] += 1
                    elif item in ("custom_tool_call_output", "function_call_output") and payload.get("call_id") in calls:
                        output = payload.get("output")
                        text = output if isinstance(output, str) else json.dumps(output)
                        size = len(text.encode("utf-8"))
                        usage["outputs"] += 1
                        usage["output_bytes"] += size
                        first_line = text.split("\n", 1)[0]
                        header = first_line.split(" with cell ID")[0]
                        if header.startswith("Script "):
                            headers[header] += 1
                        cell = CELL_ID.match(first_line)
                        if cell:
                            prefix = GENERATION.match(cell.group(1))
                            generation = int(prefix.group(1)) if prefix else 1
                            generations[generation] += 1
                            max_generation = max(max_generation, generation)
                        if size < SHORT_OUTPUT_BYTES:
                            for marker in MARKERS:
                                if marker in text:
                                    events[marker].append(f"{stamp} {calls[payload['call_id']]}: {text[:240]}")
                        retained: dict[str, bool] = {}
                        kinds_seen = set()
                        directories = []
                        for line, value in json_lines(text):
                            kind_name = receipt_kind(value)
                            if kind_name is None:
                                continue
                            receipts[kind_name] += 1
                            receipt_bytes[kind_name] += len(line.encode("utf-8"))
                            kinds_seen.add(kind_name)
                            if kind_name == "retained_nested_result":
                                retained[value["call_id"]] = bool(value["output_truncated"])
                            elif kind_name == "recovery_directory":
                                directories.append(value["nested_result_recovery_directory"])
                            if kind_name in ("recovery_directory", "nested_command_display_reduced"):
                                for artifact in ARTIFACT_ID.findall(line):
                                    pending.setdefault(artifact, kind_name)
                        for line in text.split("\n"):
                            if line.strip().startswith("Running command session_id:"):
                                receipts["running_command_handle"] += 1
                                receipt_bytes["running_command_handle"] += len(line.encode("utf-8"))
                                kinds_seen.add("running_command_handle")
                        for kind_name in kinds_seen:
                            outputs_with[kind_name] += 1
                        for entries in directories:
                            for entry in entries:
                                size = len(json.dumps(entry).encode("utf-8"))
                                outcome = entry.get("outcome") or {}
                                payload_kind = (
                                    "inline_result" if "result" in outcome
                                    else "artifact_pointer" if "artifact_id" in outcome
                                    else "retention_failed_preview" if "output" in outcome
                                    else "legacy_flat_preview" if "output" in entry
                                    else "other"
                                )
                                directory[f"payload_{payload_kind}"] += 1
                                call_id = entry.get("call_id")
                                if call_id in retained and not retained[call_id]:
                                    relation = "pure_duplicate_of_inline"
                                elif call_id in retained:
                                    relation = "recovers_truncated_inline"
                                else:
                                    relation = "not_shown_inline"
                                directory[relation] += 1
                                directory[f"{relation}_bytes"] += size
            if max_generation > 1:
                reopened.append(f"{stamp} max_generation={max_generation}")
            threads[thread["id"]] = thread
            times.sort()
            if times:
                start = previous = times[0]
                for moment in times[1:]:
                    if moment - previous > UNLOAD_DELAY:
                        segments.append((start, previous + UNLOAD_DELAY, thread["id"]))
                        start = moment
                    previous = moment
                segments.append((start, previous + UNLOAD_DELAY, thread["id"]))
    total = max(1, usage["output_bytes"])
    return {
        "window": [first, last],
        "usage": dict(usage),
        "script_headers": dict(headers),
        "cell_outputs_by_host_generation": dict(sorted(generations.items())),
        "rollouts_with_reopened_host": {"count": len(reopened), "samples": reopened[:10]},
        "events_in_short_outputs": {
            marker: {"count": len(samples), "samples": samples[:3]} for marker, samples in events.items()
        },
        "receipts": {
            kind: {
                "items": receipts[kind],
                "outputs": outputs_with[kind],
                "bytes": receipt_bytes[kind],
                "share_of_output_bytes": round(receipt_bytes[kind] / total, 5),
            }
            for kind in sorted(receipts)
        },
        "recovery_directory": dict(directory),
        "artifact_ids_later_referenced_by_exec_source": dict(followed),
        "concurrency": {
            "all": peak_concurrency([(start, end) for start, end, _ in segments]),
            "by_process": concurrency_by_process(segments, threads),
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--home", required=True, type=pathlib.Path)
    parser.add_argument("--json", type=pathlib.Path)
    args = parser.parse_args()
    result = scan(args.home)
    text = json.dumps(result, indent=2)
    if args.json:
        args.json.write_text(text + "\n", encoding="utf-8")
    print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
