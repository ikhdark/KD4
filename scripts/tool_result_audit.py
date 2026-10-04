#!/usr/bin/env python3
"""Deterministic payload audit of explicitly selected rollout snapshots.

No private reasoning is inferred: field mentions are lexical observations only.
Token estimates use the harness's ceil(UTF-8 bytes / 4), not provider billing.
"""

from __future__ import annotations

import argparse
import collections
import contextlib
import hashlib
import json
import re
from pathlib import Path

try:
    from scripts.rollout_snapshot import hydrate_rollout_record, read_rollout_snapshot
except ImportError:
    from rollout_snapshot import hydrate_rollout_record, read_rollout_snapshot


FIELDS = (
    "output",
    "exit_code",
    "error",
    "session_id",
    "execution_state",
    "process_exited",
    "output_complete",
    "output_reduced",
    "stdout",
    "stderr",
    "streams_complete",
    "artifact_id",
    "raw_output_artifact_id",
    "results",
    "complete",
    "continuation",
    "continuation_stop",
    "canonical_sha256",
    "source_sha256",
    "retained_bytes",
    "canonical_bytes",
    "subdivision_plan",
    "child_selectors",
    "structuredContent",
    "content",
    "description",
    "tools",
)
ARTIFACT = re.compile(
    r'"(?P<key>raw_output_artifact_id|artifact_id)"\s*:\s*"'
    r'(?P<id>[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12})"'
)


def encoded(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def text_body(value):
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        return "\n".join(
            text_body(item.get("text", item))
            if isinstance(item, dict)
            else text_body(item)
            for item in value
        )
    return encoded(value).decode("utf-8")


def json_packets(text):
    """Only complete JSON lines/whole packets; never parse clipped fragments."""
    try:
        return [json.loads(text)]
    except ValueError:
        packets = []
        for line in text.splitlines():
            if line.startswith(("{", "[")):
                try:
                    packets.append(json.loads(line))
                except ValueError:
                    pass
        return packets


def structures(value, pointer=""):
    yield pointer, value
    if isinstance(value, dict):
        for key, item in value.items():
            yield from structures(
                item, pointer + "/" + key.replace("~", "~0").replace("/", "~1")
            )
    elif isinstance(value, list):
        for index, item in enumerate(value):
            yield from structures(item, f"{pointer}/{index}")


def audit(path):
    snapshot = read_rollout_snapshot(path)
    records = []
    incomplete_tail = False
    with contextlib.closing(snapshot.stream), snapshot.open_lines() as lines:
        for number, line in enumerate(lines, 1):
            try:
                item = json.loads(line)
            except ValueError:
                if line.endswith(b"\n"):
                    raise ValueError(
                        f"invalid complete record {path}:{number}"
                    ) from None
                incomplete_tail = True
                break
            if not isinstance(item, dict):
                raise ValueError(f"non-object record {path}:{number}")
            records.append(
                (number, hydrate_rollout_record(item, snapshot.path), len(line))
            )
    session = next(
        (r["payload"] for _, r, _ in records if r["type"] == "session_meta"), {}
    )
    home = next(
        (p.parent for p in path.parents if p.name in {"sessions", "archived_sessions"}),
        None,
    )
    artifact_root = (
        home / "tool-output" / session.get("id", "missing") if home else None
    )
    calls = {}
    outputs = []
    manifests = collections.Counter()
    manifest_bytes = 0
    timings = {}
    metadata = collections.Counter()
    repeated = {}
    artifact_cache = {}
    for index, (line, row, wire_bytes) in enumerate(records):
        payload = row.get("payload", {})
        kind = payload.get("type")
        if row["type"] == "tool_manifest":
            manifests[hashlib.sha256(encoded(payload)).hexdigest()] += 1
            manifest_bytes += wire_bytes
        if kind == "task_complete" and payload.get("timing"):
            timings[payload.get("turn_id", str(line))] = payload["timing"]
        if row["type"] != "response_item":
            continue
        if kind in {"function_call", "custom_tool_call"}:
            calls[payload["call_id"]] = payload
            continue
        if kind not in {"function_call_output", "custom_tool_call_output"}:
            continue
        body = text_body(payload.get("output", ""))
        data = body.encode("utf-8")
        call_id = payload["call_id"]
        call = calls.get(call_id, {})
        source = text_body(call.get("input", call.get("arguments", "")))
        digest = hashlib.sha256(data).hexdigest()
        following = []
        for _, later, _ in records[index + 1 :]:
            p = later.get("payload", {})
            if later["type"] == "response_item" and p.get("type") in {
                "function_call",
                "custom_tool_call",
            }:
                break
            if later["type"] == "response_item" and (
                p.get("type") == "reasoning"
                or (p.get("type") == "message" and p.get("role") == "assistant")
            ):
                following.append(text_body(p.get("summary", p.get("content", ""))))
        subsequent = "\n".join(following)
        artifact_refs = []
        for match in ARTIFACT.finditer(body):
            key = match["id"]
            if key not in artifact_cache:
                evidence = {"id": key, "available": False}
                if artifact_root is not None:
                    raw_path = artifact_root / f"{key}.log"
                    meta_path = artifact_root / f"{key}.meta.json"
                    if raw_path.is_file():
                        raw = raw_path.read_bytes()
                        evidence.update(
                            available=True,
                            retained_file_bytes=len(raw),
                            sha256=hashlib.sha256(raw).hexdigest(),
                            path=str(raw_path),
                        )
                        if meta_path.is_file():
                            meta = json.loads(meta_path.read_bytes())
                            evidence["canonical_bytes"] = meta.get("canonical_bytes")
                            evidence["canonical_sha256"] = meta.get("canonical_sha256")
                            evidence["canonical_verified"] = evidence[
                                "sha256"
                            ] == meta.get("canonical_sha256") and len(raw) == meta.get(
                                "canonical_bytes"
                            )
                artifact_cache[key] = evidence
            ref = {"field": match["key"], "id": key}
            if ref not in artifact_refs:
                artifact_refs.append(ref)
        lists = []
        descriptions = []
        exact_mirrors = 0
        for packet_index, packet in enumerate(json_packets(body)):
            for pointer, value in structures(packet, f"/{packet_index}"):
                if isinstance(value, list) and len(value) > 50:
                    lists.append(
                        {
                            "pointer": pointer,
                            "items": len(value),
                            "bytes": len(encoded(value)),
                        }
                    )
                if not isinstance(value, dict):
                    continue
                for field in FIELDS:
                    if field in value:
                        metadata[field] += 1
                if isinstance(value.get("description"), str):
                    desc = value["description"]
                    descriptions.append(
                        {
                            "pointer": pointer,
                            "bytes": len(desc.encode()),
                            "json_escape_bytes": len(encoded(desc))
                            - len(desc.encode())
                            - 2,
                        }
                    )
                if "structuredContent" in value and isinstance(
                    value.get("content"), list
                ):
                    for content in value["content"]:
                        if (
                            isinstance(content, dict)
                            and set(content) == {"type", "text"}
                            and content["type"] == "text"
                        ):
                            try:
                                if (
                                    json.loads(content["text"])
                                    == value["structuredContent"]
                                ):
                                    exact_mirrors += len(encoded(content))
                            except (ValueError, TypeError):
                                pass
        outputs.append(
            {
                "line": line,
                "call_id": call_id,
                "tool": call.get("name"),
                "visible_bytes": len(data),
                "visible_estimated_tokens": (len(data) + 3) // 4,
                "sha256": digest,
                "exact_duplicate_of": repeated.get(digest),
                "truncation_marker": any(
                    marker in body
                    for marker in (
                        "truncated output",
                        '"output_truncated":true',
                        "[... ",
                        "[omitted ",
                    )
                ),
                "recovery_call_mentions": len(
                    re.findall(r"(?:tools\.)?read_tool_output\s*\(", source)
                ),
                "producer_field_mentions": [
                    field
                    for field in FIELDS
                    if re.search(
                        r"\." + field + r"\b|\[['\"]" + field + r"['\"]\]", source
                    )
                ],
                "later_reasoning_field_mentions": [
                    field
                    for field in FIELDS
                    if re.search(r"\b" + field + r"\b", subsequent)
                ],
                "artifact_refs": artifact_refs,
                "oversized_lists": lists,
                "descriptions": descriptions,
                "exact_mirror_bytes": exact_mirrors,
            }
        )
        repeated.setdefault(digest, call_id)
    counters = collections.Counter()
    for timing in timings.values():
        for key, value in timing.get("counters", {}).items():
            if key.startswith(("toolOutput", "truncationInduced")) and isinstance(
                value, int
            ):
                counters[key] += value
    return {
        "snapshot": snapshot.metadata(),
        "records": len(records),
        "incomplete_tail": incomplete_tail,
        "session_id": session.get("id"),
        "results": outputs,
        "artifacts": list(artifact_cache.values()),
        "manifest_records": sum(manifests.values()),
        "distinct_manifest_hashes": len(manifests),
        "manifest_wire_bytes": manifest_bytes,
        "metadata_occurrences": dict(metadata),
        "timing_counter_totals": dict(counters),
        "summary": {
            "results": len(outputs),
            "visible_bytes": sum(o["visible_bytes"] for o in outputs),
            "visible_estimated_tokens": sum(
                o["visible_estimated_tokens"] for o in outputs
            ),
            "truncated_results": sum(o["truncation_marker"] for o in outputs),
            "recovery_call_mentions": sum(o["recovery_call_mentions"] for o in outputs),
            "exact_duplicate_results": sum(
                o["exact_duplicate_of"] is not None for o in outputs
            ),
            "oversized_lists": sum(len(o["oversized_lists"]) for o in outputs),
        },
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", type=Path, nargs="+")
    parser.add_argument(
        "--output",
        type=Path,
        required=True,
        help="New complete JSON evidence ledger; never overwrite",
    )
    args = parser.parse_args()
    report = {
        "version": 1,
        "limitations": [
            "UTF-8 bytes are exact; tokens are ceil(bytes/4) estimates, not provider attribution.",
            "Raw nested return objects are not generally logged. Artifact bytes describe referenced evidence, not necessarily the entire call envelope.",
            "Counter totals may include both nested and outer projections; do not equate them with model-visible response-item totals.",
            "Field mentions and recovery-call source mentions are lexical evidence, not measured mental consumption or proven causality.",
            "Lists/descriptions/metadata are classified only inside complete JSON packets. Textual lists are not classified.",
        ],
        "sessions": [audit(path) for path in args.paths],
    }
    with args.output.open("x", encoding="utf-8", newline="\n") as handle:
        json.dump(report, handle, ensure_ascii=False, indent=2)
        handle.write("\n")
    print(
        json.dumps(
            {
                "report": str(args.output.resolve()),
                "sessions": [
                    dict(id=s["session_id"], **s["summary"]) for s in report["sessions"]
                ],
            }
        )
    )


if __name__ == "__main__":
    main()
