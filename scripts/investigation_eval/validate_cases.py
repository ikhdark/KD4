#!/usr/bin/env python3
"""Validate the frozen investigation evaluation manifest."""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import tempfile
from collections import Counter
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_CASES = Path(__file__).with_name("cases.jsonl")
ALLOWED_CATEGORIES = {
    "clean-control",
    "wiring",
    "lifecycle",
    "cancellation",
    "persistence",
    "config",
    "protocol",
    "local-logic",
}
REQUIRED_FIELDS = {
    "id",
    "category",
    "repository",
    "base_commit",
    "patch",
    "prompt",
    "expected_findings",
    "forbidden_findings",
    "notes",
}
CASE_ID_RE = re.compile(r"^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")


class ValidationError(ValueError):
    """A corpus validation failure."""


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise ValidationError(message)


def _repo_relative_file(value: Any, *, field: str, case_id: str) -> Path:
    _require(isinstance(value, str) and value, f"{case_id}: {field} must be a non-empty string")
    candidate = Path(value)
    _require(not candidate.is_absolute(), f"{case_id}: {field} must be repository-relative")
    resolved = (REPO_ROOT / candidate).resolve()
    try:
        resolved.relative_to(REPO_ROOT)
    except ValueError as exc:
        raise ValidationError(f"{case_id}: {field} escapes the repository") from exc
    _require(resolved.is_file(), f"{case_id}: {field} does not exist: {value}")
    return resolved


def load_cases(path: Path = DEFAULT_CASES) -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for line_number, raw_line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        if not raw_line.strip():
            continue
        try:
            value = json.loads(raw_line)
        except json.JSONDecodeError as exc:
            raise ValidationError(f"{path}:{line_number}: invalid JSON: {exc.msg}") from exc
        _require(isinstance(value, dict), f"{path}:{line_number}: record must be an object")
        cases.append(value)
    _require(cases, f"{path}: no cases found")
    return cases


def _validate_expected_findings(case: dict[str, Any]) -> None:
    case_id = case["id"]
    findings = case["expected_findings"]
    _require(isinstance(findings, list), f"{case_id}: expected_findings must be an array")
    for index, finding in enumerate(findings):
        prefix = f"{case_id}: expected_findings[{index}]"
        _require(isinstance(finding, dict), f"{prefix} must be an object")
        _require(set(finding) == {"kind", "required_locators"}, f"{prefix} has the wrong fields")
        _require(
            isinstance(finding["kind"], str) and finding["kind"],
            f"{prefix}.kind must be a non-empty string",
        )
        locators = finding["required_locators"]
        _require(isinstance(locators, list) and locators, f"{prefix}.required_locators must be non-empty")
        _require(
            all(isinstance(locator, str) and locator for locator in locators),
            f"{prefix}.required_locators must contain non-empty strings",
        )


def _validate_git_object(commit: str, *, case_id: str) -> None:
    result = subprocess.run(
        ["git", "cat-file", "-e", f"{commit}^{{commit}}"],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    _require(result.returncode == 0, f"{case_id}: base_commit is not available in this checkout")


def _validate_patch(patch: Path, commit: str, *, case_id: str) -> None:
    patch_lines = patch.read_text(encoding="utf-8").splitlines()
    raw_added = sum(line.startswith("+") and not line.startswith("+++") for line in patch_lines)
    raw_removed = sum(line.startswith("-") and not line.startswith("---") for line in patch_lines)
    numstat = subprocess.run(
        ["git", "apply", "--numstat", str(patch)],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    _require(numstat.returncode == 0, f"{case_id}: patch has invalid unified-diff structure")
    declared_added = 0
    declared_removed = 0
    for line in numstat.stdout.splitlines():
        fields = line.split("\t", maxsplit=2)
        _require(len(fields) == 3, f"{case_id}: patch numstat output is invalid")
        _require(fields[0].isdigit() and fields[1].isdigit(), f"{case_id}: binary patches are unsupported")
        declared_added += int(fields[0])
        declared_removed += int(fields[1])
    _require(
        (declared_added, declared_removed) == (raw_added, raw_removed),
        f"{case_id}: patch hunk counts do not cover every added or removed line",
    )

    with tempfile.TemporaryDirectory(prefix="investigation-eval-index-") as temp_dir:
        index_path = Path(temp_dir) / "index"
        index_env = os.environ.copy()
        index_env["GIT_INDEX_FILE"] = str(index_path)
        read_tree = subprocess.run(
            ["git", "read-tree", commit],
            cwd=REPO_ROOT,
            env=index_env,
            capture_output=True,
            text=True,
            check=False,
        )
        _require(read_tree.returncode == 0, f"{case_id}: could not load base_commit into a temporary index")
        result = subprocess.run(
            ["git", "apply", "--check", "--cached", "--whitespace=error-all", str(patch)],
            cwd=REPO_ROOT,
            env=index_env,
            capture_output=True,
            text=True,
            check=False,
        )
    detail = (result.stderr or result.stdout).strip()
    _require(result.returncode == 0, f"{case_id}: patch does not apply cleanly: {detail}")


def validate_cases(cases: list[dict[str, Any]]) -> None:
    seen_ids: set[str] = set()
    category_counts: Counter[str] = Counter()

    for case in cases:
        _require(set(case) == REQUIRED_FIELDS, f"record fields must be exactly {sorted(REQUIRED_FIELDS)}")
        case_id = case["id"]
        _require(isinstance(case_id, str) and CASE_ID_RE.fullmatch(case_id) is not None, "invalid case id")
        _require(case_id not in seen_ids, f"duplicate case id: {case_id}")
        seen_ids.add(case_id)

        category = case["category"]
        _require(category in ALLOWED_CATEGORIES, f"{case_id}: invalid category: {category}")
        category_counts[category] += 1
        _require(
            isinstance(case["repository"], str) and case["repository"],
            f"{case_id}: repository must be a non-empty logical name",
        )
        commit = case["base_commit"]
        _require(
            isinstance(commit, str) and COMMIT_RE.fullmatch(commit) is not None,
            f"{case_id}: base_commit must be a lowercase full commit SHA",
        )
        if case["repository"] == "kd4":
            _validate_git_object(commit, case_id=case_id)

        prompt = _repo_relative_file(case["prompt"], field="prompt", case_id=case_id)
        _require(prompt.suffix == ".md", f"{case_id}: prompt must be Markdown")
        patch_value = case["patch"]
        if patch_value is not None:
            patch = _repo_relative_file(patch_value, field="patch", case_id=case_id)
            _require(patch.suffix == ".patch", f"{case_id}: patch must use the .patch suffix")
            _validate_patch(patch, commit, case_id=case_id)

        _validate_expected_findings(case)
        forbidden = case["forbidden_findings"]
        _require(isinstance(forbidden, list), f"{case_id}: forbidden_findings must be an array")
        _require(
            all(isinstance(kind, str) and kind for kind in forbidden),
            f"{case_id}: forbidden_findings must contain non-empty strings",
        )
        _require(isinstance(case["notes"], str), f"{case_id}: notes must be a string")

    _require(category_counts["clean-control"] >= 2, "corpus requires at least two clean controls")
    required_groups = {
        "wiring": {"wiring"},
        "lifecycle/cancellation": {"lifecycle", "cancellation"},
        "persistence/migration": {"persistence"},
        "config/protocol": {"config", "protocol"},
        "local-logic": {"local-logic"},
    }
    for label, categories in required_groups.items():
        _require(
            any(category_counts[category] for category in categories),
            f"corpus requires at least one {label} case",
        )


def main() -> int:
    try:
        cases = load_cases()
        validate_cases(cases)
    except (OSError, ValidationError) as exc:
        print(f"investigation corpus validation failed: {exc}", file=sys.stderr)
        return 1

    counts = Counter(case["category"] for case in cases)
    summary = ", ".join(f"{category}={counts[category]}" for category in sorted(counts))
    print(f"validated {len(cases)} investigation cases ({summary})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
