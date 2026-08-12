#!/usr/bin/env python3
"""Validate the frozen investigation evaluation manifest."""

from __future__ import annotations

import argparse
import hashlib
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
    "patch",
    "prompt",
    "expected_findings",
    "forbidden_findings",
    "notes",
}
CASE_ID_RE = re.compile(r"^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$")


class ValidationError(ValueError):
    """A corpus validation failure."""


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise ValidationError(message)


def _repo_relative_file(value: Any, *, field: str, case_id: str) -> Path:
    _require(
        isinstance(value, str) and value,
        f"{case_id}: {field} must be a non-empty string",
    )
    candidate = Path(value)
    _require(
        not candidate.is_absolute(), f"{case_id}: {field} must be repository-relative"
    )
    resolved = (REPO_ROOT / candidate).resolve()
    try:
        resolved.relative_to(REPO_ROOT)
    except ValueError as exc:
        raise ValidationError(f"{case_id}: {field} escapes the repository") from exc
    _require(resolved.is_file(), f"{case_id}: {field} does not exist: {value}")
    return resolved


def load_cases(path: Path = DEFAULT_CASES) -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for line_number, raw_line in enumerate(
        path.read_text(encoding="utf-8").splitlines(), start=1
    ):
        if not raw_line.strip():
            continue
        try:
            value = json.loads(raw_line)
        except json.JSONDecodeError as exc:
            raise ValidationError(
                f"{path}:{line_number}: invalid JSON: {exc.msg}"
            ) from exc
        _require(
            isinstance(value, dict), f"{path}:{line_number}: record must be an object"
        )
        cases.append(value)
    _require(cases, f"{path}: no cases found")
    return cases


def case_fingerprint(case: dict[str, Any]) -> str:
    """Bind a recorded result to the exact manifest, prompt, and patch content."""
    digest = hashlib.sha256()
    digest.update(
        json.dumps(case, sort_keys=True, separators=(",", ":")).encode("utf-8")
    )
    for field in ("prompt", "patch"):
        digest.update(b"\0")
        digest.update(field.encode("ascii"))
        value = case[field]
        if value is None:
            digest.update(b"\0")
            continue
        path = _repo_relative_file(value, field=field, case_id=case["id"])
        digest.update(path.read_bytes())
    return digest.hexdigest()


def _validate_expected_findings(case: dict[str, Any]) -> None:
    case_id = case["id"]
    findings = case["expected_findings"]
    _require(
        isinstance(findings, list), f"{case_id}: expected_findings must be an array"
    )
    seen_kinds: set[str] = set()
    for index, finding in enumerate(findings):
        prefix = f"{case_id}: expected_findings[{index}]"
        _require(isinstance(finding, dict), f"{prefix} must be an object")
        _require(
            set(finding) == {"kind", "required_locators"},
            f"{prefix} has the wrong fields",
        )
        _require(
            isinstance(finding["kind"], str) and finding["kind"],
            f"{prefix}.kind must be a non-empty string",
        )
        _require(
            finding["kind"] not in seen_kinds,
            f"{prefix}.kind duplicates an earlier expected finding",
        )
        seen_kinds.add(finding["kind"])
        locators = finding["required_locators"]
        _require(
            isinstance(locators, list) and locators,
            f"{prefix}.required_locators must be non-empty",
        )
        _require(
            all(isinstance(locator, str) and locator for locator in locators),
            f"{prefix}.required_locators must contain non-empty strings",
        )
        _require(
            len(set(locators)) == len(locators),
            f"{prefix}.required_locators must not contain duplicates",
        )


def _scan_unified_diff(
    patch_lines: list[str],
) -> tuple[int, int, list[str], list[str]]:
    raw_added = 0
    raw_removed = 0
    old_paths: list[str] = []
    new_paths: list[str] = []
    in_hunk = False
    for line in patch_lines:
        if line.startswith("diff --git "):
            in_hunk = False
        elif line.startswith("@@ "):
            in_hunk = True
        elif not in_hunk and line.startswith("--- "):
            old_paths.append(line.removeprefix("--- "))
        elif not in_hunk and line.startswith("+++ "):
            new_paths.append(line.removeprefix("+++ "))
        elif in_hunk and line.startswith("+"):
            raw_added += 1
        elif in_hunk and line.startswith("-"):
            raw_removed += 1
    return raw_added, raw_removed, old_paths, new_paths


def _validate_patch(patch: Path, *, case_id: str) -> set[str]:
    patch_text = patch.read_text(encoding="utf-8")
    patch_lines = patch_text.splitlines()
    raw_added, raw_removed, old_paths, new_paths = _scan_unified_diff(patch_lines)
    _require(
        old_paths and len(old_paths) == len(new_paths),
        f"{case_id}: patch must contain paired file headers",
    )
    for old_path, new_path in zip(old_paths, new_paths, strict=True):
        _require(
            old_path == "/dev/null", f"{case_id}: fixtures must add new files only"
        )
        _require(
            new_path.startswith("b/investigation_cases/"),
            f"{case_id}: fixture path must stay under investigation_cases/",
        )
        relative = Path(new_path.removeprefix("b/"))
        _require(
            not relative.is_absolute() and ".." not in relative.parts,
            f"{case_id}: fixture path escapes investigation_cases/",
        )
    with tempfile.TemporaryDirectory(prefix="investigation-eval-index-") as temp_dir:
        temp_root = Path(temp_dir)
        repo = temp_root / "repo"
        normalized_patch = temp_root / "fixture.patch"
        normalized_patch.write_bytes(patch_text.encode("utf-8"))

        isolated_env = os.environ.copy()
        isolated_env["GIT_CONFIG_NOSYSTEM"] = "1"
        isolated_env["GIT_CONFIG_GLOBAL"] = os.devnull
        initialized = subprocess.run(
            ["git", "init", "--quiet", str(repo)],
            cwd=temp_root,
            env=isolated_env,
            capture_output=True,
            text=True,
            check=False,
        )
        _require(
            initialized.returncode == 0,
            f"{case_id}: could not initialize a disposable validation repository",
        )
        empty_index = subprocess.run(
            ["git", "read-tree", "--empty"],
            cwd=repo,
            env=isolated_env,
            capture_output=True,
            text=True,
            check=False,
        )
        _require(
            empty_index.returncode == 0,
            f"{case_id}: could not initialize the disposable validation index",
        )

        numstat = subprocess.run(
            ["git", "apply", "--numstat", str(normalized_patch)],
            cwd=repo,
            env=isolated_env,
            capture_output=True,
            text=True,
            check=False,
        )
        _require(
            numstat.returncode == 0,
            f"{case_id}: patch has invalid unified-diff structure",
        )
        declared_added = 0
        declared_removed = 0
        for line in numstat.stdout.splitlines():
            fields = line.split("\t", maxsplit=2)
            _require(len(fields) == 3, f"{case_id}: patch numstat output is invalid")
            _require(
                fields[0].isdigit() and fields[1].isdigit(),
                f"{case_id}: binary patches are unsupported",
            )
            declared_added += int(fields[0])
            declared_removed += int(fields[1])
            _require(
                fields[2].replace("\\", "/").startswith("investigation_cases/"),
                f"{case_id}: patch numstat path escapes investigation_cases/",
            )
        _require(
            (declared_added, declared_removed) == (raw_added, raw_removed),
            f"{case_id}: patch hunk counts do not cover every added or removed line",
        )
        _require(
            declared_removed == 0,
            f"{case_id}: fixtures must not remove existing lines",
        )

        result = subprocess.run(
            [
                "git",
                "apply",
                "--check",
                "--cached",
                "--whitespace=error-all",
                str(normalized_patch),
            ],
            cwd=repo,
            env=isolated_env,
            capture_output=True,
            text=True,
            check=False,
        )
    detail = (result.stderr or result.stdout).strip()
    _require(
        result.returncode == 0, f"{case_id}: patch does not apply cleanly: {detail}"
    )
    return {path.removeprefix("b/").replace("\\", "/") for path in new_paths}


def validate_cases(cases: list[dict[str, Any]]) -> None:
    seen_ids: set[str] = set()
    fixture_owners: dict[str, str] = {}
    category_counts: Counter[str] = Counter()

    for case in cases:
        _require(
            set(case) == REQUIRED_FIELDS,
            f"record fields must be exactly {sorted(REQUIRED_FIELDS)}",
        )
        case_id = case["id"]
        _require(
            isinstance(case_id, str) and CASE_ID_RE.fullmatch(case_id) is not None,
            "invalid case id",
        )
        _require(case_id not in seen_ids, f"duplicate case id: {case_id}")
        seen_ids.add(case_id)

        category = case["category"]
        _require(
            category in ALLOWED_CATEGORIES, f"{case_id}: invalid category: {category}"
        )
        category_counts[category] += 1
        _require(
            isinstance(case["repository"], str) and case["repository"],
            f"{case_id}: repository must be a non-empty logical name",
        )
        prompt = _repo_relative_file(case["prompt"], field="prompt", case_id=case_id)
        _require(prompt.suffix == ".md", f"{case_id}: prompt must be Markdown")
        patch_value = case["patch"]
        if patch_value is not None:
            patch = _repo_relative_file(patch_value, field="patch", case_id=case_id)
            _require(
                patch.suffix == ".patch", f"{case_id}: patch must use the .patch suffix"
            )
            for fixture_path in _validate_patch(patch, case_id=case_id):
                previous_owner = fixture_owners.get(fixture_path)
                _require(
                    previous_owner is None,
                    f"{case_id}: fixture path {fixture_path!r} is also owned by {previous_owner}",
                )
                fixture_owners[fixture_path] = case_id

        _validate_expected_findings(case)
        forbidden = case["forbidden_findings"]
        _require(
            isinstance(forbidden, list),
            f"{case_id}: forbidden_findings must be an array",
        )
        _require(
            all(isinstance(kind, str) and kind for kind in forbidden),
            f"{case_id}: forbidden_findings must contain non-empty strings",
        )
        expected_kinds = {finding["kind"] for finding in case["expected_findings"]}
        forbidden_kinds = set(forbidden)
        _require(
            len(forbidden_kinds) == len(forbidden),
            f"{case_id}: forbidden_findings must not contain duplicates",
        )
        contradictory = expected_kinds & forbidden_kinds
        _require(
            not contradictory,
            f"{case_id}: finding kinds cannot be both expected and forbidden: {sorted(contradictory)}",
        )
        _require(isinstance(case["notes"], str), f"{case_id}: notes must be a string")

    _require(
        category_counts["clean-control"] >= 2,
        "corpus requires at least two clean controls",
    )
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
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--show-fingerprints",
        action="store_true",
        help="print each case fingerprint after validation",
    )
    args = parser.parse_args()
    try:
        cases = load_cases()
        validate_cases(cases)
    except (OSError, ValidationError) as exc:
        print(f"investigation corpus validation failed: {exc}", file=sys.stderr)
        return 1

    counts = Counter(case["category"] for case in cases)
    summary = ", ".join(f"{category}={counts[category]}" for category in sorted(counts))
    print(f"validated {len(cases)} investigation cases ({summary})")
    if args.show_fingerprints:
        for case in cases:
            print(f"{case['id']} {case_fingerprint(case)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
