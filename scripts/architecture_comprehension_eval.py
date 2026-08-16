#!/usr/bin/env python3
"""Evaluate bounded architecture slices against representative missed relationships."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys

REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts import source_owners  # noqa: E402


DEFAULT_CASES = Path(__file__).with_name("architecture_comprehension_cases.json")


def load_cases(path: Path) -> list[dict]:
    cases = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(cases, list) or not cases:
        raise ValueError("architecture comprehension cases must be a nonempty array")
    return cases


def _facet_relationships(slice_: dict, facet: str) -> list[dict]:
    value = slice_.get(facet, {})
    relationships = value.get("relationships", []) if isinstance(value, dict) else []
    return relationships if isinstance(relationships, list) else []


def evaluate_case(case: dict, manifest: dict, digest: str, root: Path) -> dict:
    ranked_relationship_limit = case.get("ranked_relationships_per_facet", 1)
    slice_ = source_owners.architecture_slice(
        manifest,
        digest,
        root,
        case["owners"],
        max_relationships=32,
        focus=case.get("focus", case["description"]),
    )
    expected = case.get("expected", [])
    missing = []
    matched_relationships: set[tuple[str, str, str]] = set()
    ranked_matched_relationships: set[tuple[str, str, str]] = set()
    ranked_missing = []
    for expectation in expected:
        facet = expectation["facet"]
        needle = expectation["target_contains"]
        matches = [
            relationship
            for relationship in _facet_relationships(slice_, facet)
            if needle in relationship.get("target", "")
            or needle in relationship.get("evidence", "")
        ]
        if not matches:
            missing.append(expectation)
            ranked_missing.append(expectation)
            continue
        match = matches[0]
        matched_relationships.add((facet, match["source"], match["target"]))
        ranked_matches = [
            relationship
            for relationship in _facet_relationships(slice_, facet)[
                :ranked_relationship_limit
            ]
            if needle in relationship.get("target", "")
            or needle in relationship.get("evidence", "")
        ]
        if ranked_matches:
            ranked_match = ranked_matches[0]
            ranked_matched_relationships.add(
                (facet, ranked_match["source"], ranked_match["target"])
            )
        else:
            ranked_missing.append(expectation)

    all_relationships = [
        (facet, relationship["source"], relationship["target"])
        for facet in source_owners.ARCHITECTURE_FACETS
        for relationship in _facet_relationships(slice_, facet)
    ]
    completeness_failures = []
    if slice_["truncated"]:
        completeness_failures.append("truncated")
    if slice_["omitted_relationships"]:
        completeness_failures.append(
            f"omitted_relationships={slice_['omitted_relationships']}"
        )
    completeness_failures.extend(slice_["material_unknowns"])
    completeness_failures.extend(
        f"missing_relationship={item['facet']}:{item['target_contains']}"
        for item in missing
    )

    relationship_count = len(all_relationships)
    useful_count = len(matched_relationships)
    noise_count = max(0, relationship_count - useful_count)
    ranked_relationships = [
        (facet, relationship["source"], relationship["target"])
        for facet in source_owners.ARCHITECTURE_FACETS
        for relationship in _facet_relationships(slice_, facet)[
            :ranked_relationship_limit
        ]
    ]
    ranked_noise_count = max(
        0, len(ranked_relationships) - len(ranked_matched_relationships)
    )
    slice_bytes = len(json.dumps(slice_, sort_keys=True).encode("utf-8"))
    return {
        "id": case["id"],
        "description": case["description"],
        "classification": "insufficient" if completeness_failures else "sufficient",
        "completeness_failures": completeness_failures,
        "expected_relationships": len(expected),
        "matched_relationships": len(expected) - len(missing),
        "relationship_count": relationship_count,
        "noise_relationships": noise_count,
        "noise_ratio": noise_count / relationship_count if relationship_count else 0.0,
        "ranked_relationships_per_facet": ranked_relationship_limit,
        "ranked_matched_relationships": len(ranked_matched_relationships),
        "ranked_relationship_recall": (
            len(ranked_matched_relationships) / len(expected) if expected else 1.0
        ),
        "ranked_noise_relationships": ranked_noise_count,
        "ranking_failures": [
            f"ranked_out={item['facet']}:{item['target_contains']}"
            for item in ranked_missing
        ],
        "slice_bytes": slice_bytes,
        "late_relationship_discoveries": len(missing),
        "snapshot": slice_["snapshot"],
    }


def evaluate(
    manifest_path: Path = source_owners.DEFAULT_MANIFEST,
    cases_path: Path = DEFAULT_CASES,
    root: Path = REPO_ROOT,
) -> dict:
    manifest, digest = source_owners.load_and_validate(manifest_path, root)
    results = [
        evaluate_case(case, manifest, digest, root) for case in load_cases(cases_path)
    ]
    expected = sum(result["expected_relationships"] for result in results)
    matched = sum(result["matched_relationships"] for result in results)
    ranked_matched = sum(result["ranked_matched_relationships"] for result in results)
    slice_bytes = sum(result["slice_bytes"] for result in results)
    broad_map_bytes = source_owners.DEFAULT_SOURCEMAP.stat().st_size
    noise_relationships = sum(result["noise_relationships"] for result in results)
    ranked_noise_relationships = sum(
        result["ranked_noise_relationships"] for result in results
    )
    return {
        "status": (
            "passed"
            if all(
                result["classification"] == "sufficient"
                and not result["ranking_failures"]
                for result in results
            )
            else "failed"
        ),
        "cases": results,
        "summary": {
            "case_count": len(results),
            "passed_cases": sum(
                result["classification"] == "sufficient" for result in results
            ),
            "relationship_recall": matched / expected if expected else 1.0,
            "ranked_relationship_recall": (
                ranked_matched / expected if expected else 1.0
            ),
            "late_relationship_discoveries": sum(
                result["late_relationship_discoveries"] for result in results
            ),
            "slice_bytes": slice_bytes,
            "broad_sourcemap_bytes_per_case": broad_map_bytes,
            "reading_reduction_ratio": (
                1.0 - slice_bytes / (broad_map_bytes * len(results))
                if broad_map_bytes and results
                else 0.0
            ),
            "noise_relationships": noise_relationships,
            "ranked_noise_relationships": ranked_noise_relationships,
            "ranked_noise_reduction_ratio": (
                1.0 - ranked_noise_relationships / noise_relationships
                if noise_relationships
                else 0.0
            ),
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=source_owners.DEFAULT_MANIFEST)
    parser.add_argument("--cases", type=Path, default=DEFAULT_CASES)
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument("--json", action="store_true", help="Emit the full report.")
    args = parser.parse_args()
    try:
        report = evaluate(args.manifest, args.cases, args.repo_root)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(error, file=sys.stderr)
        return 1
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        summary = report["summary"]
        print(
            "architecture comprehension: "
            f"{report['status']}; cases={summary['passed_cases']}/{summary['case_count']}; "
            f"recall={summary['relationship_recall']:.3f}; "
            f"ranked_recall={summary['ranked_relationship_recall']:.3f}; "
            f"reading_reduction={summary['reading_reduction_ratio']:.3f}; "
            f"ranked_noise_reduction={summary['ranked_noise_reduction_ratio']:.3f}; "
            f"late_relationships={summary['late_relationship_discoveries']}; "
            f"noise_relationships={summary['noise_relationships']}"
            f"->{summary['ranked_noise_relationships']}"
        )
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
