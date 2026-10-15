#!/usr/bin/env python3
"""Assert the read-envelope projection never removes unrecoverable evidence.

Replays `context_projection_savings.project` (the port of the shipped
`output_projection` omissions) over every enveloped read output in the selected
rollouts and checks the invariants that recovery depends on:

* recovery and provenance fields are byte-identical after projection:
  `recovery`, `continuation`, `page_selectors`, `artifact_id`, `source_sha256`,
  `selector_errors`, `criterion_evidence`, `results`;
* a `canonical_uri` that is NOT an exact re-encoding of `path` is retained;
* a `selector_bindings` entry whose resolved range was not delivered verbatim by
  an `ok` + `complete` result keeps `resolved_selectors` -- this is the
  merge/sort case where the binding is the only record of the item's extent;
* binding identity (`selector_index`, `qualified_name`, `kind`, `syntax_error`)
  is always retained.

Exits nonzero on the first violated invariant and reports per-check counts, so
a check that saw zero recorded cases is visible as a coverage gap rather than
being reported as a pass.

Usage:
  context_projection_recoverability.py SESSION.jsonl [...]
  context_projection_recoverability.py --latest N
"""

from __future__ import annotations

import argparse
import json
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2]))
from scripts.benchmark_scripts.rollout_snapshot import discover_rollouts, iter_rollout_records

from scripts.benchmark_scripts.context_projection_savings import (
    SESSION_ROOT,
    derived_file_uri,
    project,
)

# Fields that carry evidence or a recovery path; projection must not touch them.
PRESERVED_FIELDS = (
    "recovery",
    "continuation",
    "page_selectors",
    "artifact_id",
    "source_sha256",
    "selector_errors",
    "criterion_evidence",
    "results",
)
IDENTITY_FIELDS = ("selector_index", "qualified_name", "kind", "syntax_error")


def enveloped_headers(path: pathlib.Path):
    # Strict verified hydration: unresolved coverage aborts rather than reporting zero.
    for record, wire_bytes in iter_rollout_records(path):
        if record.get("type") != "response_item":
            continue
        payload = record.get("payload")
        if not isinstance(payload, dict):
            continue
        if payload.get("type") not in {
            "function_call_output",
            "custom_tool_call_output",
        }:
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
        if isinstance(header, dict):
            yield header


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="*", type=pathlib.Path)
    parser.add_argument("--latest", type=int)
    args = parser.parse_args()

    paths = list(args.paths)
    if args.latest:
        paths = sorted(
            discover_rollouts(SESSION_ROOT),
            key=lambda p: p.stat().st_mtime,
            reverse=True,
        )[: args.latest]
    if not paths:
        parser.error("no sessions selected")

    counts = {
        "outputs": 0,
        "preserved_field_instances": 0,
        "recovery_envelopes": 0,
        "canonical_uri_exceptions": 0,
        "non_matching_bindings": 0,
        "identity_fields": 0,
    }
    violations: list[str] = []

    for path in paths:
        for header in enveloped_headers(path):
            counts["outputs"] += 1
            projected = project(header)

            for field in PRESERVED_FIELDS:
                if field not in header:
                    continue
                counts["preserved_field_instances"] += 1
                counts["recovery_envelopes"] += int(field == "recovery")
                if projected.get(field) != header[field]:
                    violations.append(f"{path.name}: altered {field!r}")

            uri, raw_path = header.get("canonical_uri"), header.get("path")
            if isinstance(uri, str) and (
                not isinstance(raw_path, str) or derived_file_uri(raw_path) != uri
            ):
                counts["canonical_uri_exceptions"] += 1
                if projected.get("canonical_uri") != uri:
                    violations.append(
                        f"{path.name}: dropped non-derivable canonical_uri {uri!r}"
                    )

            delivered = [
                result.get("selector")
                for result in header.get("results") or []
                if isinstance(result, dict)
                and result.get("status") == "ok"
                and result.get("complete") is True
            ]
            for original, updated in zip(
                header.get("selector_bindings") or [],
                projected.get("selector_bindings") or [],
            ):
                if not isinstance(original, dict) or not isinstance(updated, dict):
                    continue
                resolved = original.get("resolved_selectors")
                if isinstance(resolved, list) and not (
                    len(resolved) == 1 and resolved[0] in delivered
                ):
                    counts["non_matching_bindings"] += 1
                    if updated.get("resolved_selectors") != resolved:
                        violations.append(
                            f"{path.name}: dropped an undelivered resolved range"
                        )
                for field in IDENTITY_FIELDS:
                    if field not in original:
                        continue
                    counts["identity_fields"] += 1
                    if updated.get(field) != original[field]:
                        violations.append(f"{path.name}: lost binding {field!r}")

    print(f"sessions: {len(paths)}")
    for key, value in counts.items():
        print(f"  {key:<28}{value:>8,}")
    for key in ("canonical_uri_exceptions", "non_matching_bindings"):
        if counts[key] == 0:
            print(
                f"COVERAGE GAP: no recorded {key}; that branch is unexercised by"
                " this corpus and is covered only by unit tests"
            )
    if violations:
        print(f"\nVIOLATIONS: {len(violations)}", file=sys.stderr)
        for violation in violations[:20]:
            print(f"  {violation}", file=sys.stderr)
        return 1
    print("\nno violated invariants")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
