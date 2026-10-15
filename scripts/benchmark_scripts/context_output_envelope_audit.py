#!/usr/bin/env python3
"""Measure the per-output envelope overhead of KD4 read/exec tool results.

A `read_file`-style tool result reaching the model is three parts:

  1. a JSON status header (`total_lines`, `canonical_bytes`, `selection_status`,
     `source_sha256`, `results[]`, `path`, `canonical_uri`, ...);
  2. a `[source {...}]` provenance line repeating `path`, `selector`,
     `canonical_range`, `complete` and `text_lines` from part 1;
  3. the actual file/command body.

Parts 1 and 2 are the envelope. This script measures the envelope's share of
model-visible tokens and, within it, the fields that are byte-wise derivable
from another field in the same envelope:

* `canonical_uri` is `path` re-encoded as a file: URI.
* the `[source ...]` line's `path`, `selector`, `canonical_range`, `complete`
  and `text_lines` restate `results[0]` / top-level fields of the header.

Derivable-field tokens are a measured upper bound on what removing the
restatement could save without losing any fact: every removed field is
reconstructible from a field that stays. Nothing about the body is counted,
and this script does not propose dropping provenance itself.

Usage:
  context_output_envelope_audit.py SESSION.jsonl [...] [--json]
  context_output_envelope_audit.py --latest N [--json]
"""

from __future__ import annotations

import argparse
import json
import math
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2]))
from scripts.benchmark_scripts.rollout_snapshot import discover_rollouts, iter_rollout_records
from collections import defaultdict

SESSION_ROOT = pathlib.Path(r"C:\Users\kuh\Desktop\LOCAL-KD\sessions")

OUTPUT_TYPES = {"function_call_output", "custom_tool_call_output"}
SOURCE_PREFIX = "[source "

# Fields in the `[source {...}]` line that restate a header field verbatim.
SOURCE_RESTATED = ("path", "selector", "canonical_range", "complete", "text_lines")


def est_tokens(text: str) -> int:
    return math.ceil(len(text.encode("utf-8", "replace")) / 4)


def output_text(payload: dict) -> str:
    value = payload.get("output")
    if isinstance(value, str):
        return value
    parts: list[str] = []
    if isinstance(value, dict):
        for field in ("content", "text"):
            inner = value.get(field)
            if isinstance(inner, str):
                parts.append(inner)
            elif isinstance(inner, list):
                for entry in inner:
                    if isinstance(entry, dict):
                        for sub in ("text", "input_text", "output_text"):
                            if isinstance(entry.get(sub), str):
                                parts.append(entry[sub])
    return "\n".join(parts)


def split_envelope(text: str) -> tuple[dict | None, str | None, int] | None:
    """Return (header, source_line, body_offset) for an enveloped output."""
    if not text.startswith("{"):
        return None
    newline = text.find("\n")
    if newline < 0:
        return None
    try:
        header = json.loads(text[:newline])
    except json.JSONDecodeError:
        return None
    if not isinstance(header, dict):
        return None
    rest = text[newline + 1 :]
    if rest.startswith(SOURCE_PREFIX):
        end = rest.find("\n")
        source_line = rest if end < 0 else rest[:end]
        return header, source_line, newline + 1 + len(source_line)
    return header, None, newline


def derivable_tokens(header: dict, source_line: str | None) -> dict[str, int]:
    """Tokens held by fields reconstructible from a field that would remain."""
    found: dict[str, int] = defaultdict(int)

    path = header.get("path")
    uri = header.get("canonical_uri")
    if isinstance(path, str) and isinstance(uri, str):
        # file:///C:/a/b.rs is path with separators normalized: derivable.
        normalized = "file:///" + path.replace("\\", "/")
        if uri == normalized or uri.replace("%3A", ":") == normalized:
            found["canonical_uri"] += est_tokens(f'"canonical_uri":"{uri}",')

    if source_line:
        try:
            payload = json.loads(source_line[len(SOURCE_PREFIX) : -1])
        except (json.JSONDecodeError, IndexError):
            payload = None
        if isinstance(payload, dict):
            results = header.get("results")
            first = results[0] if isinstance(results, list) and results else {}
            if not isinstance(first, dict):
                first = {}
            for key in SOURCE_RESTATED:
                if key not in payload:
                    continue
                value = payload[key]
                if value == header.get(key) or value == first.get(key):
                    found[f"source.{key}"] += est_tokens(
                        json.dumps({key: value})[1:-1] + ","
                    )
    return dict(found)


def audit(path: pathlib.Path) -> dict:
    enveloped = 0
    plain = 0
    envelope_tokens = 0
    body_tokens = 0
    source_line_tokens = 0
    derivable: dict[str, int] = defaultdict(int)
    malformed = 0

    # Strict verified hydration: unresolved coverage aborts rather than reporting zero.
    for record, wire_bytes in iter_rollout_records(path):
        if record.get("type") != "response_item":
            continue
        payload = record.get("payload")
        if not isinstance(payload, dict):
            continue
        if payload.get("type") not in OUTPUT_TYPES:
            continue
        text = output_text(payload)
        if not text:
            continue
        split = split_envelope(text)
        if split is None:
            plain += 1
            body_tokens += est_tokens(text)
            continue
        header, source_line, body_offset = split
        enveloped += 1
        envelope_tokens += est_tokens(text[:body_offset])
        body_tokens += est_tokens(text[body_offset:])
        if source_line:
            source_line_tokens += est_tokens(source_line)
        for key, tokens in derivable_tokens(header, source_line).items():
            derivable[key] += tokens

    return {
        "session": path.name,
        "malformed_records": malformed,
        "enveloped_outputs": enveloped,
        "plain_outputs": plain,
        "envelope_tokens": envelope_tokens,
        "source_line_tokens": source_line_tokens,
        "body_tokens": body_tokens,
        "derivable_tokens": dict(
            sorted(derivable.items(), key=lambda kv: -kv[1])
        ),
        "derivable_total": sum(derivable.values()),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="*", type=pathlib.Path)
    parser.add_argument("--latest", type=int)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()

    paths = list(args.paths)
    if args.latest:
        if not SESSION_ROOT.exists():
            print(f"session root not found: {SESSION_ROOT}", file=sys.stderr)
            return 2
        paths = sorted(
            discover_rollouts(SESSION_ROOT),
            key=lambda p: p.stat().st_mtime,
            reverse=True,
        )[: args.latest]
    if not paths:
        parser.error("no sessions selected")
    missing = [p for p in paths if not p.is_file()]
    if missing:
        for path in missing:
            print(f"missing: {path}", file=sys.stderr)
        return 2

    reports = [audit(path) for path in paths]
    if args.json:
        print(json.dumps({"sessions": reports}, indent=2))
        return 0

    env = sum(r["envelope_tokens"] for r in reports)
    src = sum(r["source_line_tokens"] for r in reports)
    body = sum(r["body_tokens"] for r in reports)
    enveloped = sum(r["enveloped_outputs"] for r in reports)
    plain = sum(r["plain_outputs"] for r in reports)
    derivable: dict[str, int] = defaultdict(int)
    for report in reports:
        for key, tokens in report["derivable_tokens"].items():
            derivable[key] += tokens
    total = env + body or 1

    print(f"sessions: {len(reports)}")
    print(f"outputs: {enveloped:,} enveloped, {plain:,} plain")
    print(f"tool-output est tokens: {total:,}")
    print(
        f"  envelope      {env:>10,}  {100 * env / total:5.2f}%"
        f"   (of which [source ...] line {src:,})"
    )
    print(f"  body          {body:>10,}  {100 * body / total:5.2f}%")
    if enveloped:
        print(f"  envelope per enveloped output: {env / enveloped:.1f} tok avg")
    print("\nderivable (restated) envelope fields:")
    for key, tokens in sorted(derivable.items(), key=lambda kv: -kv[1]):
        print(f"  {key:<24}{tokens:>9,} tok")
    grand = sum(derivable.values())
    print(
        f"  {'TOTAL':<24}{grand:>9,} tok = {100 * grand / total:.3f}% of tool output"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
