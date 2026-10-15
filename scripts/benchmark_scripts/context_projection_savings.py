#!/usr/bin/env python3
"""Replay the model-visible read envelope projection over recorded rollouts.

This mirrors the two omissions added to
`code-mode-protocol::output_projection::model_visible_tool_result`:

* `canonical_uri` when it is an exact `file:` re-encoding of `path`;
* a `selector_bindings[].resolved_selectors` whose single entry was delivered
  verbatim as the selector of an `ok` + `complete` result, and whose binding
  carries no `syntax_error` marker.

It reports the measured token delta per session and in total, so the change is
quantified against real traffic rather than a synthetic envelope. Token counts
are the harness estimate ceil(UTF-8 bytes / 4); they are not provider billing,
and these bytes sit in the cached prompt prefix, so the delta is the reduction
in prompt bytes, not a billed-token saving.

Usage:
  context_projection_savings.py SESSION.jsonl [...] [--json]
  context_projection_savings.py --latest N [--json]
"""

from __future__ import annotations

import argparse
import json
import math
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2]))
from scripts.benchmark_scripts.rollout_snapshot import discover_rollouts, iter_rollout_records

SESSION_ROOT = pathlib.Path(r"C:\Users\kuh\Desktop\LOCAL-KD\sessions")
OUTPUT_TYPES = {"function_call_output", "custom_tool_call_output"}
UNRESERVED = set(
    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789/\\.-_~"
)


def est_tokens(text: str) -> int:
    return math.ceil(len(text.encode("utf-8", "replace")) / 4)


def derived_file_uri(path: str) -> str | None:
    """Port of `output_projection::derived_file_uri`."""
    if len(path) >= 2 and path[1] == ":" and path[0].isalpha():
        body, checked = path, path[2:]
    elif path.startswith("/") and not path.startswith("//"):
        body = checked = path[1:]
    else:
        return None
    if not body or any(char not in UNRESERVED for char in checked):
        return None
    uri = "file:///" + body.replace("\\", "/")
    return None if uri.startswith("file:////") else uri


def project(header: dict) -> dict:
    """Port of the two read_file envelope omissions."""
    projected = dict(header)
    path, uri = header.get("path"), header.get("canonical_uri")
    if (
        isinstance(path, str)
        and isinstance(uri, str)
        and derived_file_uri(path) == uri
    ):
        projected.pop("canonical_uri", None)

    delivered = [
        result.get("selector")
        for result in header.get("results") or []
        if isinstance(result, dict)
        and result.get("status") == "ok"
        and result.get("complete") is True
    ]
    bindings = header.get("selector_bindings")
    if delivered and isinstance(bindings, list):
        rebuilt = []
        for binding in bindings:
            if not isinstance(binding, dict):
                rebuilt.append(binding)
                continue
            binding = dict(binding)
            resolved = binding.get("resolved_selectors")
            if (
                isinstance(resolved, list)
                and len(resolved) == 1
                and resolved[0] in delivered
                and "syntax_error" not in binding
            ):
                binding.pop("resolved_selectors")
            rebuilt.append(binding)
        projected["selector_bindings"] = rebuilt
    return projected


def audit(path: pathlib.Path) -> dict:
    before = after = 0
    outputs = uri_hits = binding_hits = 0
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
        text = payload.get("output")
        if not isinstance(text, str) or not text.startswith("{"):
            continue
        newline = text.find("\n")
        if newline < 0:
            continue
        try:
            header = json.loads(text[:newline])
        except json.JSONDecodeError:
            continue
        if not isinstance(header, dict):
            continue
        outputs += 1
        projected = project(header)
        raw_line = json.dumps(header, separators=(",", ":"))
        new_line = json.dumps(projected, separators=(",", ":"))
        before += est_tokens(raw_line)
        after += est_tokens(new_line)
        if "canonical_uri" in header and "canonical_uri" not in projected:
            uri_hits += 1
        for original, updated in zip(
            header.get("selector_bindings") or [],
            projected.get("selector_bindings") or [],
        ):
            if isinstance(original, dict) and isinstance(updated, dict):
                binding_hits += int(
                    "resolved_selectors" in original
                    and "resolved_selectors" not in updated
                )
    return {
        "session": path.name,
        "malformed_records": malformed,
        "enveloped_outputs": outputs,
        "header_tokens_before": before,
        "header_tokens_after": after,
        "header_tokens_saved": before - after,
        "canonical_uri_omissions": uri_hits,
        "resolved_selectors_omissions": binding_hits,
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

    before = sum(r["header_tokens_before"] for r in reports)
    after = sum(r["header_tokens_after"] for r in reports)
    outputs = sum(r["enveloped_outputs"] for r in reports)
    uri_hits = sum(r["canonical_uri_omissions"] for r in reports)
    binding_hits = sum(r["resolved_selectors_omissions"] for r in reports)
    saved = before - after
    print(f"sessions: {len(reports)}")
    print(f"enveloped read outputs: {outputs:,}")
    print(f"header tokens before: {before:,}")
    print(f"header tokens after:  {after:,}")
    pct = (100 * saved / before) if before else 0.0
    print(f"header tokens saved:  {saved:,}  ({pct:.2f}% of envelope headers)")
    print(f"  canonical_uri omitted in {uri_hits:,} outputs")
    print(f"  resolved_selectors omitted in {binding_hits:,} bindings")
    if outputs:
        print(f"  per enveloped output: {saved / outputs:.1f} tok avg")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
