#!/usr/bin/env python3
"""Validate KD4's declared feature ownership and static reachability evidence."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
from collections import Counter
from collections.abc import Sequence
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

import tomllib

REPO_ROOT = Path(__file__).resolve().parents[1]
MANIFEST_FILE_NAME = "kd4_features.toml"
DEFAULT_MANIFEST = REPO_ROOT / MANIFEST_FILE_NAME
SELF_FEATURE_ID = "kd4-feature-manifest"
SCHEMA_VERSION = 2
STATUS_SEMANTICS = "implementation_lifecycle"
ALLOWED_STATUSES = frozenset({"enabled", "disabled", "orphaned", "planned", "replaced"})
ALLOWED_RUNTIME_STATUSES = frozenset({"enabled", "disabled"})
ALLOWED_CAPABILITY_KINDS = frozenset({"runtime", "workflow", "library", "guidance"})
ALLOWED_EVIDENCE_KINDS = frozenset(
    {"entrypoint", "module", "registration", "config", "protocol", "test", "workflow"}
)
COMMIT_SHA_PATTERN = re.compile(r"^[0-9a-f]{40}$")


@dataclass(frozen=True)
class Finding:
    level: str
    code: str
    message: str
    feature_id: str | None = None


@dataclass(frozen=True)
class CheckResult:
    schema_version: int | None
    feature_count: int
    status_counts: dict[str, int]
    runtime_status_counts: dict[str, int]
    findings: tuple[Finding, ...]

    @property
    def ok(self) -> bool:
        return not any(finding.level == "error" for finding in self.findings)

    def to_json(self) -> dict[str, Any]:
        return {
            "ok": self.ok,
            "schemaVersion": self.schema_version,
            "featureCount": self.feature_count,
            "statusCounts": self.status_counts,
            "runtimeStatusCounts": self.runtime_status_counts,
            "findings": [asdict(finding) for finding in self.findings],
        }


def _safe_repo_path(
    repo_root: Path, path_text: object
) -> tuple[Path | None, str | None]:
    if not isinstance(path_text, str) or not path_text.strip():
        return None, "path must be a non-empty string"
    relative = Path(path_text)
    if relative.is_absolute() or ".." in relative.parts:
        return None, f"path must stay repo-relative: {path_text!r}"
    root = repo_root.resolve()
    candidate = (root / relative).resolve()
    if not candidate.is_relative_to(root):
        return None, f"path escapes repository root: {path_text!r}"
    return candidate, None


def _required_text(
    feature: dict[str, Any], key: str, feature_id: str
) -> Finding | None:
    value = feature.get(key)
    if isinstance(value, str) and value.strip():
        return None
    return Finding(
        "error", "missing-field", f"{key} must be a non-empty string", feature_id
    )


def _project_feature_override(repo_root: Path, feature_key: str) -> bool | None:
    config_path = repo_root / ".codex" / "config.toml"
    if not config_path.is_file():
        return None
    try:
        with config_path.open("rb") as config_file:
            value: object = tomllib.load(config_file)
    except (OSError, tomllib.TOMLDecodeError):
        return None
    for part in feature_key.split("."):
        if not isinstance(value, dict) or part not in value:
            return None
        value = value[part]
    if isinstance(value, bool):
        return value
    if isinstance(value, dict) and isinstance(value.get("enabled"), bool):
        return value["enabled"]
    return None


def _feature_default(repo_root: Path, feature_key: str) -> bool | None:
    key = feature_key.removeprefix("features.")
    registry_path = repo_root / "codex-rs" / "features" / "src" / "lib.rs"
    try:
        registry = registry_path.read_text(encoding="utf-8")
    except (OSError, UnicodeError):
        return None
    pattern = re.compile(
        rf'FeatureSpec\s*\{{(?:(?!FeatureSpec\s*\{{).)*?key:\s*"{re.escape(key)}"'
        rf"(?:(?!FeatureSpec\s*\{{).)*?default_enabled:\s*(true|false)",
        flags=re.DOTALL,
    )
    match = pattern.search(registry)
    if match is None:
        return None
    return match.group(1) == "true"


def _validate_runtime_status(
    *,
    feature: dict[str, Any],
    feature_id: str,
    repo_root: Path,
    findings: list[Finding],
) -> str | None:
    config_keys = feature.get("config_keys")
    feature_config_keys = (
        [key for key in config_keys if key.startswith("features.")]
        if isinstance(config_keys, list)
        else []
    )
    runtime_feature_key = feature.get("runtime_feature_key")
    runtime_status = feature.get("runtime_status")
    runtime_status_source = feature.get("runtime_status_source")

    if not feature_config_keys:
        if any(
            field in feature
            for field in (
                "runtime_feature_key",
                "runtime_status",
                "runtime_status_source",
            )
        ):
            findings.append(
                Finding(
                    "error",
                    "unexpected-runtime-status",
                    "runtime status fields require a features.* config key",
                    feature_id,
                )
            )
        return None

    if runtime_feature_key not in feature_config_keys:
        findings.append(
            Finding(
                "error",
                "invalid-runtime-feature-key",
                "runtime_feature_key must select one declared features.* config key",
                feature_id,
            )
        )
    if runtime_status not in ALLOWED_RUNTIME_STATUSES:
        findings.append(
            Finding(
                "error",
                "invalid-runtime-status",
                f"unsupported runtime_status {runtime_status!r}",
                feature_id,
            )
        )
        return None
    if not isinstance(runtime_status_source, str) or not runtime_status_source:
        findings.append(
            Finding(
                "error",
                "invalid-runtime-status-source",
                "runtime_status_source must be a non-empty string",
                feature_id,
            )
        )
        return runtime_status
    if not isinstance(runtime_feature_key, str):
        return runtime_status

    project_override = _project_feature_override(repo_root, runtime_feature_key)
    if project_override is not None:
        expected_enabled = project_override
        expected_source = ".codex/config.toml"
    else:
        feature_default = _feature_default(repo_root, runtime_feature_key)
        if feature_default is None:
            findings.append(
                Finding(
                    "error",
                    "unresolved-runtime-status",
                    f"could not resolve effective state for {runtime_feature_key}",
                    feature_id,
                )
            )
            return runtime_status
        expected_enabled = feature_default
        expected_source = "codex-rs/features/src/lib.rs"

    expected_status = "enabled" if expected_enabled else "disabled"
    if runtime_status != expected_status:
        findings.append(
            Finding(
                "error",
                "stale-runtime-status",
                f"runtime_status is {runtime_status!r}, but {runtime_feature_key} resolves to {expected_status!r}",
                feature_id,
            )
        )
    if runtime_status_source != expected_source:
        findings.append(
            Finding(
                "error",
                "stale-runtime-status-source",
                f"runtime_status_source must be {expected_source!r}",
                feature_id,
            )
        )
    return runtime_status


def _validate_declared_paths(
    *,
    feature_id: str,
    field: str,
    value: object,
    repo_root: Path,
    expect_present: bool,
    findings: list[Finding],
) -> None:
    if value is None:
        return
    if not isinstance(value, list):
        findings.append(
            Finding(
                "error",
                f"invalid-{field.replace('_', '-')}",
                f"{field} must be an array of repo-relative paths",
                feature_id,
            )
        )
        return

    for path_text in value:
        path, path_error = _safe_repo_path(repo_root, path_text)
        if path_error is not None:
            findings.append(
                Finding(
                    "error",
                    f"invalid-{field.replace('_', '-')}",
                    path_error,
                    feature_id,
                )
            )
            continue
        assert path is not None
        relative = path.relative_to(repo_root.resolve()).as_posix()
        if expect_present and not path.exists():
            findings.append(
                Finding(
                    "error",
                    "missing-generated-artifact",
                    f"declared generated artifact does not exist: {relative}",
                    feature_id,
                )
            )
        elif not expect_present and path.exists():
            findings.append(
                Finding(
                    "error",
                    "parallel-implementation",
                    f"retired parallel implementation still exists: {relative}",
                    feature_id,
                )
            )


def _validate_evidence(
    *,
    feature_id: str,
    evidence_items: object,
    repo_root: Path,
    findings: list[Finding],
    text_cache: dict[Path, str],
) -> Counter[str]:
    kinds: Counter[str] = Counter()
    if not isinstance(evidence_items, list):
        findings.append(
            Finding(
                "error", "invalid-evidence", "evidence must be an array", feature_id
            )
        )
        return kinds

    for index, evidence in enumerate(evidence_items):
        if not isinstance(evidence, dict):
            findings.append(
                Finding(
                    "error",
                    "invalid-evidence",
                    f"evidence[{index}] must be a table",
                    feature_id,
                )
            )
            continue

        kind = evidence.get("kind")
        if kind not in ALLOWED_EVIDENCE_KINDS:
            findings.append(
                Finding(
                    "error",
                    "invalid-evidence-kind",
                    f"evidence[{index}] has unsupported kind {kind!r}",
                    feature_id,
                )
            )
            continue
        kinds[kind] += 1

        path, path_error = _safe_repo_path(repo_root, evidence.get("path"))
        if path_error is not None:
            findings.append(
                Finding("error", "invalid-evidence-path", path_error, feature_id)
            )
            continue
        assert path is not None
        if not path.is_file():
            findings.append(
                Finding(
                    "error",
                    "missing-evidence-path",
                    f"{path.relative_to(repo_root.resolve()).as_posix()} does not exist",
                    feature_id,
                )
            )
            continue

        contains = evidence.get("contains")
        regex = evidence.get("regex")
        contains_present = "contains" in evidence
        regex_present = "regex" in evidence
        if contains_present == regex_present:
            findings.append(
                Finding(
                    "error",
                    "invalid-evidence-match",
                    f"evidence[{index}] must set exactly one of contains or regex",
                    feature_id,
                )
            )
            continue
        match_value = contains if contains_present else regex
        if not isinstance(match_value, str) or not match_value:
            match_name = "contains" if contains_present else "regex"
            findings.append(
                Finding(
                    "error",
                    "invalid-evidence-match",
                    f"evidence[{index}] {match_name} must be a non-empty string",
                    feature_id,
                )
            )
            continue

        try:
            if path not in text_cache:
                text_cache[path] = path.read_text(encoding="utf-8")
            text = text_cache[path]
        except (OSError, UnicodeError) as exc:
            findings.append(
                Finding(
                    "error",
                    "unreadable-evidence",
                    f"failed to read {path}: {exc}",
                    feature_id,
                )
            )
            continue

        if isinstance(contains, str) and contains not in text:
            findings.append(
                Finding(
                    "error",
                    "stale-evidence",
                    f"{evidence['path']} no longer contains {contains!r}",
                    feature_id,
                )
            )
        elif isinstance(regex, str):
            try:
                matched = re.search(regex, text, flags=re.MULTILINE) is not None
            except re.error as exc:
                findings.append(
                    Finding(
                        "error",
                        "invalid-evidence-regex",
                        f"{evidence['path']} regex is invalid: {exc}",
                        feature_id,
                    )
                )
            else:
                if not matched:
                    findings.append(
                        Finding(
                            "error",
                            "stale-evidence",
                            f"{evidence['path']} no longer matches {regex!r}",
                            feature_id,
                        )
                    )
        elif not isinstance(contains, str):
            findings.append(
                Finding(
                    "error",
                    "invalid-evidence-match",
                    f"evidence[{index}] match value must be a string",
                    feature_id,
                )
            )
    return kinds


def validate_manifest(
    manifest_path: Path = DEFAULT_MANIFEST,
    *,
    repo_root: Path = REPO_ROOT,
    strict: bool = True,
) -> CheckResult:
    findings: list[Finding] = []
    try:
        with manifest_path.open("rb") as manifest_file:
            manifest = tomllib.load(manifest_file)
    except (OSError, tomllib.TOMLDecodeError) as exc:
        return CheckResult(
            schema_version=None,
            feature_count=0,
            status_counts={},
            runtime_status_counts={},
            findings=(Finding("error", "manifest-load", str(exc)),),
        )

    schema_version = manifest.get("schema_version")
    if schema_version != SCHEMA_VERSION:
        findings.append(
            Finding(
                "error",
                "schema-version",
                f"expected schema_version {SCHEMA_VERSION}, found {schema_version!r}",
            )
        )

    if manifest.get("status_semantics") != STATUS_SEMANTICS:
        findings.append(
            Finding(
                "error",
                "status-semantics",
                f"status_semantics must be {STATUS_SEMANTICS!r}",
            )
        )

    upstream_commit = manifest.get("upstream_commit")
    if not isinstance(upstream_commit, str) or not COMMIT_SHA_PATTERN.fullmatch(
        upstream_commit
    ):
        findings.append(
            Finding(
                "error",
                "invalid-upstream-commit",
                "upstream_commit must be a lowercase 40-character Git commit SHA",
            )
        )
    elif (
        repo_root.resolve() == REPO_ROOT.resolve()
        and manifest_path.resolve() == DEFAULT_MANIFEST.resolve()
    ):
        resolved = subprocess.run(
            ["git", "cat-file", "-e", f"{upstream_commit}^{{commit}}"],
            cwd=repo_root,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        if resolved.returncode != 0:
            findings.append(
                Finding(
                    "error",
                    "unknown-upstream-commit",
                    "upstream_commit does not resolve to a commit in this repository",
                )
            )

    features = manifest.get("features")
    if not isinstance(features, list):
        findings.append(
            Finding("error", "invalid-features", "features must be an array")
        )
        features = []

    seen_ids: set[str] = set()
    status_counts: Counter[str] = Counter()
    runtime_status_counts: Counter[str] = Counter()
    text_cache: dict[Path, str] = {}
    for index, feature in enumerate(features):
        if not isinstance(feature, dict):
            findings.append(
                Finding(
                    "error", "invalid-feature", f"features[{index}] must be a table"
                )
            )
            continue

        raw_id = feature.get("id")
        feature_id = (
            raw_id if isinstance(raw_id, str) and raw_id else f"features[{index}]"
        )
        if isinstance(raw_id, str) and raw_id and raw_id in seen_ids:
            findings.append(
                Finding(
                    "error",
                    "duplicate-id",
                    f"duplicate feature id {raw_id!r}",
                    feature_id,
                )
            )
        elif isinstance(raw_id, str) and raw_id:
            seen_ids.add(raw_id)
        else:
            findings.append(
                Finding(
                    "error",
                    "missing-field",
                    "id must be a non-empty string",
                    feature_id,
                )
            )

        for key in ("summary", "owner", "upstream_equivalent"):
            finding = _required_text(feature, key, feature_id)
            if finding is not None:
                findings.append(finding)

        version = feature.get("version")
        if not isinstance(version, int) or version < 1:
            findings.append(
                Finding(
                    "error",
                    "invalid-version",
                    "version must be a positive integer",
                    feature_id,
                )
            )

        status = feature.get("status")
        if status not in ALLOWED_STATUSES:
            findings.append(
                Finding(
                    "error",
                    "invalid-status",
                    f"unsupported status {status!r}",
                    feature_id,
                )
            )
        else:
            status_counts[status] += 1

        capability_kind = feature.get("capability_kind")
        if capability_kind not in ALLOWED_CAPABILITY_KINDS:
            findings.append(
                Finding(
                    "error",
                    "invalid-capability-kind",
                    f"unsupported capability_kind {capability_kind!r}",
                    feature_id,
                )
            )

        owner = feature.get("owner")
        if isinstance(owner, str) and owner.strip():
            owner_path, owner_error = _safe_repo_path(repo_root, owner)
            if owner_error is not None:
                findings.append(
                    Finding("error", "invalid-owner", owner_error, feature_id)
                )
            elif owner_path is not None and not owner_path.exists():
                findings.append(
                    Finding(
                        "error",
                        "missing-owner",
                        f"owner path does not exist: {owner}",
                        feature_id,
                    )
                )

        config_keys = feature.get("config_keys")
        if not isinstance(config_keys, list) or not all(
            isinstance(key, str) and key for key in config_keys
        ):
            findings.append(
                Finding(
                    "error",
                    "invalid-config-keys",
                    "config_keys must be an array of non-empty strings",
                    feature_id,
                )
            )

        runtime_status = _validate_runtime_status(
            feature=feature,
            feature_id=feature_id,
            repo_root=repo_root,
            findings=findings,
        )
        if runtime_status is not None:
            runtime_status_counts[runtime_status] += 1

        _validate_declared_paths(
            feature_id=feature_id,
            field="generated_artifacts",
            value=feature.get("generated_artifacts"),
            repo_root=repo_root,
            expect_present=True,
            findings=findings,
        )
        _validate_declared_paths(
            feature_id=feature_id,
            field="retired_paths",
            value=feature.get("retired_paths"),
            repo_root=repo_root,
            expect_present=False,
            findings=findings,
        )

        evidence_kinds = _validate_evidence(
            feature_id=feature_id,
            evidence_items=feature.get("evidence", []),
            repo_root=repo_root,
            findings=findings,
            text_cache=text_cache,
        )
        if status == "enabled":
            if evidence_kinds["entrypoint"] == 0:
                findings.append(
                    Finding(
                        "error",
                        "missing-entrypoint",
                        "enabled feature has no declared entrypoint evidence",
                        feature_id,
                    )
                )
            if (
                capability_kind in {"runtime", "workflow", "guidance"}
                and evidence_kinds["registration"] == 0
            ):
                findings.append(
                    Finding(
                        "error",
                        "missing-registration",
                        "enabled feature has no declared registration evidence",
                        feature_id,
                    )
                )
            if (
                capability_kind in {"runtime", "workflow"}
                and evidence_kinds["test"] == 0
            ):
                findings.append(
                    Finding(
                        "error",
                        "missing-test",
                        "enabled runtime/workflow feature has no declared test evidence",
                        feature_id,
                    )
                )
        if status == "orphaned":
            findings.append(
                Finding(
                    "error" if strict else "warning",
                    "orphaned-feature",
                    "feature is present but has no accepted live registration",
                    feature_id,
                )
            )
        if status == "replaced" and feature.get("upstream_equivalent") == "none":
            findings.append(
                Finding(
                    "error",
                    "missing-upstream-replacement",
                    "replaced feature must identify its upstream equivalent",
                    feature_id,
                )
            )

    is_repository_manifest = (
        repo_root.resolve() == REPO_ROOT.resolve()
        and manifest_path.resolve() == DEFAULT_MANIFEST.resolve()
    )
    if is_repository_manifest and SELF_FEATURE_ID not in seen_ids:
        findings.append(
            Finding(
                "error",
                "missing-self-feature",
                f"repository manifest must declare {SELF_FEATURE_ID!r}",
                SELF_FEATURE_ID,
            )
        )

    return CheckResult(
        schema_version=schema_version if isinstance(schema_version, int) else None,
        feature_count=len(features),
        status_counts=dict(sorted(status_counts.items())),
        runtime_status_counts=dict(sorted(runtime_status_counts.items())),
        findings=tuple(findings),
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument(
        "--strict",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Fail on orphaned features (default: enabled).",
    )
    parser.add_argument(
        "--json", action="store_true", help="Emit one JSON result object."
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    manifest_path = args.manifest
    if not manifest_path.is_absolute():
        manifest_path = args.repo_root / manifest_path
    result = validate_manifest(
        manifest_path, repo_root=args.repo_root, strict=args.strict
    )
    if args.json:
        print(json.dumps(result.to_json(), sort_keys=True))
    else:
        verdict = "PASSED" if result.ok else "FAILED"
        counts = ", ".join(
            f"{status}={count}" for status, count in result.status_counts.items()
        )
        print(
            f"KD4 FEATURE CHECK {verdict}: {result.feature_count} feature(s); {counts}; "
            f"runtime={result.runtime_status_counts}"
        )
        for finding in result.findings:
            feature = f" [{finding.feature_id}]" if finding.feature_id else ""
            print(
                f"[{finding.level.upper()}]{feature} {finding.code}: {finding.message}"
            )
    return 0 if result.ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
