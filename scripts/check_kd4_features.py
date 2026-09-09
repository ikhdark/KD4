#!/usr/bin/env python3
"""Validate KD4 feature ownership, reachability, and executable test routes."""

from __future__ import annotations

import argparse
import ast
import json
import os
import re
import subprocess
from collections import Counter
from collections.abc import Sequence
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

import tomllib

if __package__:
    from scripts import rust_test_runner
else:
    import rust_test_runner

REPO_ROOT = Path(__file__).resolve().parents[1]
MANIFEST_FILE_NAME = "kd4_features.toml"
DEFAULT_MANIFEST = REPO_ROOT / MANIFEST_FILE_NAME
SOURCE_OWNERS_FILE_NAME = "source_owners.toml"
SELF_FEATURE_ID = "kd4-feature-manifest"
SCHEMA_VERSION = 2
STATUS_SEMANTICS = "implementation_lifecycle"
ALLOWED_STATUSES = frozenset({"enabled", "disabled", "orphaned", "planned", "replaced"})
ALLOWED_RUNTIME_STATUSES = frozenset({"enabled", "disabled"})
ALLOWED_CAPABILITY_KINDS = frozenset({"runtime", "workflow", "library", "guidance"})
ALLOWED_EVIDENCE_KINDS = frozenset(
    {"entrypoint", "module", "registration", "config", "protocol", "test", "workflow"}
)
ALLOWED_RUNTIME_VERIFICATION_KINDS = frozenset({"contract_test", "integration_test"})
COMMIT_SHA_PATTERN = re.compile(r"^[0-9a-f]{40}$")


class ProjectConfigError(ValueError):
    pass


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


def _verification_route(
    verification: dict[str, Any],
    repo_root: Path,
    rust_manifest: rust_test_runner.Manifest | None = None,
) -> str:
    """Accept only a single test selector in its declared source owner/binary."""
    command = verification.get("command")
    symbol = verification.get("symbol")
    if not isinstance(symbol, str) or not re.fullmatch(
        r"[A-Za-z_][A-Za-z_0-9]*", symbol
    ):
        raise ValueError("verification symbol must be one test function name")
    if not isinstance(command, list) or not all(
        isinstance(arg, str) for arg in command
    ):
        raise ValueError("verification command must be an argument array")
    source, error = _safe_repo_path(repo_root, verification.get("path"))
    if error or source is None:
        raise ValueError(error or "missing source")
    if source.suffix == ".py":
        module = (
            source.relative_to(repo_root).with_suffix("").as_posix().replace("/", ".")
        )
        tree = ast.parse(source.read_text(encoding="utf-8"))
        selectors = {
            f"{module}.{node.name}.{symbol}"
            for node in tree.body
            if isinstance(node, ast.ClassDef)
            if any(
                isinstance(item, ast.FunctionDef) and item.name == symbol
                for item in node.body
            )
        }
        if (
            len(command) == 4
            and re.fullmatch(r"python(?:3(?:\.\d+)?)?(?:\.exe)?", Path(command[0]).name)
            and command[1:3] == ["-m", "unittest"]
            and command[3] in selectors
        ):
            return "unittest"
        raise ValueError(
            "Python verification must select the declared module, class and test with unittest"
        )
    cargo_path = next(
        (
            parent / "Cargo.toml"
            for parent in source.parents
            if parent.is_relative_to(repo_root) and (parent / "Cargo.toml").is_file()
        ),
        None,
    )
    if cargo_path is None:
        raise ValueError("Rust verification source has no Cargo owner")
    cargo = tomllib.loads(cargo_path.read_text(encoding="utf-8"))
    package = cargo["package"]["name"]
    relative = source.relative_to(cargo_path.parent)
    selector = ["--lib"]
    if relative.parts[0] == "tests":
        if len(relative.parts) == 2:
            selector = ["--test", source.stem]
        else:
            # Existing suite aggregators register their first module explicitly.
            candidates = [
                p
                for p in (cargo_path.parent / "tests").glob("*.rs")
                if re.search(
                    r"\bmod\s+" + re.escape(relative.parts[1]) + r"\s*;",
                    p.read_text(encoding="utf-8"),
                )
            ]
            if len(candidates) != 1:
                raise ValueError(
                    "verification source must resolve to one integration test binary"
                )
            selector = ["--test", candidates[0].stem]
    else:
        for binary in cargo.get("bin", []):
            if binary.get("path") == relative.as_posix():
                selector = ["--bin", binary["name"]]
    if (
        len(command) != 6
        or command[:3] != ["python", "scripts/rust_test_runner.py", "run-gate"]
        or command[4:] != ["--profile", "fast"]
    ):
        raise ValueError(
            "Rust verification must use a named exact-test gate in scripts/rust_test_runner.py"
        )
    if rust_manifest is None:
        rust_manifest = rust_test_runner.Manifest.load(
            repo_root / "codex-rs/.config/kd4-rust-tests.toml"
        )
    gate = rust_manifest.gates.get(command[3])
    if gate is None or len(gate.steps) != 1:
        raise ValueError("capability gate must name one test target")
    step = gate.steps[0]
    target = rust_manifest.targets[step.target]
    if target.selection_args() != ["-p", package, *selector]:
        raise ValueError(
            "capability gate must select the declared source's package and test binary"
        )
    if not any(test.split("::")[-1] == symbol for test in step.tests):
        raise ValueError("capability gate must require the declared test symbol")
    return "nextest"


@dataclass
class _TestOutcome:
    symbol: str
    route: str
    selector: str
    passed: set[str]
    skipped: bool = False
    failed: bool = False
    zero_tests: bool = False

    def observe(self, line: str) -> None:
        line = re.sub(r"\x1b\[[0-9;]*m", "", line).strip()
        self.zero_tests |= bool(
            re.search(r"\b(?:running 0 tests|Ran 0 tests|0 tests run)\b", line)
        )
        identity = status = None
        if self.route == "unittest":
            match = re.fullmatch(r"(\w+) \(([^)]+)\)(?: .*?)? \.\.\. (.+)", line)
            if match and match[1] == self.symbol:
                qualified = (
                    match[2]
                    if match[2].endswith("." + self.symbol)
                    else match[2] + "." + self.symbol
                )
                if qualified == self.selector:
                    identity, status = qualified, match[3]
        if identity is None or identity.split("::")[-1].split(".")[-1] != self.symbol:
            return
        if status in {"ok", "PASS"}:
            self.passed.add(identity)
        elif status and status.startswith(("skipped", "ignored", "SKIP")):
            self.skipped = True
        else:
            self.failed = True

    def verdict(self, returncode: int) -> str:
        if returncode or self.failed:
            return "failed"
        if self.skipped:
            return "skipped"
        if len(self.passed) == 1:
            return "passed"
        if len(self.passed) > 1:
            return "ambiguous_test_identity"
        return "zero_tests" if self.zero_tests else "not_executed"


def execute_runtime_verification(
    manifest_path: Path,
    *,
    feature_id: str | None,
    repo_root: Path,
    quiet: bool = False,
    outcomes: list[dict[str, Any]] | None = None,
) -> int:
    """Execute one selected, or every enabled, runtime verification command."""
    try:
        with manifest_path.open("rb") as manifest_file:
            manifest = tomllib.load(manifest_file)
    except (OSError, tomllib.TOMLDecodeError) as exc:
        if not quiet:
            print(f"runtime verification manifest could not be read: {exc}")
        return 2

    eligible = [
        feature
        for feature in manifest.get("features", [])
        if isinstance(feature, dict)
        and feature.get("status") == "enabled"
        and feature.get("capability_kind") == "runtime"
    ]
    matching = (
        [feature for feature in eligible if feature.get("id") == feature_id]
        if feature_id is not None
        else eligible
    )
    if feature_id is not None and len(matching) != 1:
        if not quiet:
            print(
                f"runtime verification feature must resolve exactly once: {feature_id!r}"
            )
        return 2
    # Resolve every route before executing anything. Rust uses the same gate
    # runner as `just core-gate`; compatible Python selectors share one process.
    rust_manifest = None
    rust_features = []
    python_groups: dict[str, list[dict[str, Any]]] = {}
    try:
        for feature in matching:
            verification = feature.get("runtime_verification")
            if not isinstance(feature.get("id"), str) or not isinstance(
                verification, dict
            ):
                raise TypeError("feature has no executable runtime verification")
            if (
                str(verification.get("path", "")).endswith(".rs")
                and rust_manifest is None
            ):
                rust_manifest = rust_test_runner.Manifest.load(
                    repo_root / "codex-rs/.config/kd4-rust-tests.toml"
                )
            route = _verification_route(verification, repo_root, rust_manifest)
            if route == "nextest":
                rust_features.append(feature)
            else:
                python_groups.setdefault(verification["command"][0], []).append(feature)
    except (
        TypeError,
        ValueError,
        OSError,
        KeyError,
        SyntaxError,
        rust_test_runner.RunnerError,
    ) as exc:
        if not quiet:
            print(f"invalid runtime verification: {exc}")
        return 2

    if rust_features:
        gates = list(
            dict.fromkeys(
                feature["runtime_verification"]["command"][3]
                for feature in rust_features
            )
        )
        try:
            assert rust_manifest is not None
            cwd = repo_root / "codex-rs"
            metadata = rust_test_runner.load_metadata(cwd=cwd)
            runner = rust_test_runner.RustTestRunner(
                rust_manifest,
                metadata,
                cwd=cwd,
                profile="fast",
                target_dir=rust_test_runner._resolve_target_dir(
                    None, metadata, cwd=cwd
                ),
            )
            # Exact selectors and the same execution's outcomes supply proof;
            # feature verification must not add a preliminary test discovery run.
            completed = runner.run_gates(gates, quiet=quiet, discover=False)
        except (rust_test_runner.RunnerError, OSError) as exc:
            for feature in rust_features:
                if outcomes is not None:
                    outcomes.append(
                        {
                            "feature_id": feature["id"],
                            "outcome": getattr(exc, "outcome", "failed"),
                            "test_identities": [],
                            "returncode": 2,
                            "evidence_kind": feature["runtime_verification"]["kind"],
                            "error": str(exc),
                        }
                    )
            if not quiet:
                print(f"KD4 RUNTIME VERIFICATION failed: {exc}")
            return 2
        for feature in rust_features:
            verification = feature["runtime_verification"]
            if outcomes is not None:
                outcomes.append(
                    {
                        "feature_id": feature["id"],
                        "outcome": "passed",
                        "test_identities": completed[verification["command"][3]],
                        "returncode": 0,
                        "evidence_kind": verification["kind"],
                    }
                )
            if not quiet:
                print(f"KD4 TEST RESULT [{feature['id']}]: passed")

    for interpreter, features in python_groups.items():
        observers = {
            feature["runtime_verification"]["command"][3]: _TestOutcome(
                feature["runtime_verification"]["symbol"],
                "unittest",
                feature["runtime_verification"]["command"][3],
                set(),
            )
            for feature in features
        }
        command = [interpreter, "-m", "unittest", "-v", *observers]
        if not quiet:
            for feature in features:
                print(
                    f"KD4 RUNTIME VERIFICATION [{feature['id']}]: {' '.join(command)}"
                )
        try:
            with subprocess.Popen(
                command,
                cwd=repo_root,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                encoding="utf-8",
                errors="replace",
                env={**os.environ, "INSTA_UPDATE": "no"},
            ) as process:
                assert process.stdout is not None
                for line in process.stdout:
                    for observer in observers.values():
                        observer.observe(line)
                    if not quiet:
                        print(line, end="")
                returncode = process.wait()
        except OSError as exc:
            if not quiet:
                print(f"runtime verification could not start: {exc}")
            return 2
        failed = False
        for feature in features:
            verification = feature["runtime_verification"]
            outcome = observers[verification["command"][3]]
            verdict = outcome.verdict(returncode)
            if outcomes is not None:
                outcomes.append(
                    {
                        "feature_id": feature["id"],
                        "outcome": verdict,
                        "test_identities": sorted(outcome.passed),
                        "returncode": returncode,
                        "evidence_kind": verification["kind"],
                    }
                )
            if not quiet:
                print(f"KD4 TEST RESULT [{feature['id']}]: {verdict}")
            failed |= verdict != "passed"
        if failed:
            return returncode or 1
    return 0


def _safe_repo_path(
    repo_root: Path, path_text: object
) -> tuple[Path | None, str | None]:
    if not isinstance(path_text, str) or not path_text.strip():
        return None, "path must be a non-empty string"
    relative = Path(path_text)
    if relative.is_absolute() or ".." in relative.parts:
        return None, f"path must stay repo-relative: {path_text!r}"
    root = repo_root
    candidate = (root / relative).resolve()
    if not candidate.is_relative_to(root):
        return None, f"path escapes repository root: {path_text!r}"
    return candidate, None


def _executable_source_text(path: Path, text: str) -> str:
    """Exclude comments from source-marker reachability checks."""
    if path.suffix.lower() not in {".rs", ".py", ".ps1", ".js", ".ts"}:
        return text
    if path.suffix.lower() in {".rs", ".js", ".ts"}:
        text = re.sub(r"/\*.*?\*/", "", text, flags=re.DOTALL)
        return re.sub(r"(?m)^\s*//.*$", "", text)
    return re.sub(r"(?m)^\s*#.*$", "", text)


def _required_text(
    feature: dict[str, Any], key: str, feature_id: str
) -> Finding | None:
    value = feature.get(key)
    if isinstance(value, str) and value.strip():
        return None
    return Finding(
        "error", "missing-field", f"{key} must be a non-empty string", feature_id
    )


def _project_feature_override(
    repo_root: Path,
    feature_key: str,
    config_cache: dict[str, object],
) -> bool | None:
    config_path = repo_root / ".codex" / "config.toml"
    if "project_config" not in config_cache:
        if not config_path.is_file():
            config_cache["project_config"] = None
        else:
            try:
                with config_path.open("rb") as config_file:
                    config_cache["project_config"] = tomllib.load(config_file)
            except (OSError, tomllib.TOMLDecodeError) as exc:
                config_cache["project_config"] = ProjectConfigError(
                    f"could not read .codex/config.toml: {exc}"
                )
    value = config_cache["project_config"]
    if isinstance(value, ProjectConfigError):
        raise value
    if value is None:
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


def _load_feature_defaults(repo_root: Path) -> dict[str, bool] | None:
    manifest_path = repo_root / "codex-rs" / "Cargo.toml"
    if not manifest_path.is_file():
        return None
    try:
        completed = subprocess.run(
            [
                "cargo",
                "run",
                "--quiet",
                "--manifest-path",
                str(manifest_path),
                "-p",
                "codex-features",
                "--bin",
                "codex-features-export",
            ],
            cwd=repo_root,
            check=False,
            capture_output=True,
            text=True,
            encoding="utf-8",
        )
    except (OSError, UnicodeError):
        return None
    if completed.returncode != 0:
        return None
    try:
        entries = json.loads(completed.stdout)
    except (json.JSONDecodeError, TypeError):
        return None
    if not isinstance(entries, list):
        return None
    defaults: dict[str, bool] = {}
    for entry in entries:
        if not isinstance(entry, dict):
            return None
        key = entry.get("key")
        default_enabled = entry.get("defaultEnabled")
        if (
            not isinstance(key, str)
            or not key
            or not isinstance(default_enabled, bool)
            or key in defaults
        ):
            return None
        defaults[key] = default_enabled
    return defaults


def _feature_default(
    repo_root: Path,
    feature_key: str,
    registry_cache: dict[str, dict[str, bool] | None],
) -> bool | None:
    key = feature_key.removeprefix("features.")
    if "defaults" not in registry_cache:
        registry_cache["defaults"] = _load_feature_defaults(repo_root)
    defaults = registry_cache["defaults"]
    return defaults.get(key) if defaults is not None else None


def _validate_runtime_status(
    *,
    feature: dict[str, Any],
    feature_id: str,
    repo_root: Path,
    findings: list[Finding],
    feature_registry_cache: dict[str, dict[str, bool] | None],
    project_config_cache: dict[str, object],
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

    try:
        project_override = _project_feature_override(
            repo_root, runtime_feature_key, project_config_cache
        )
    except ProjectConfigError as exc:
        findings.append(
            Finding(
                "error",
                "invalid-project-config",
                str(exc),
                feature_id,
            )
        )
        return runtime_status
    if project_override is not None:
        expected_enabled = project_override
        expected_source = ".codex/config.toml"
    else:
        feature_default = _feature_default(
            repo_root, runtime_feature_key, feature_registry_cache
        )
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
            text = _executable_source_text(path, text_cache[path])
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


def _validate_runtime_verification(
    *,
    feature_id: str,
    verification: object,
    repo_root: Path,
    findings: list[Finding],
    text_cache: dict[Path, str],
    rust_manifest_cache: dict[Path, rust_test_runner.Manifest],
) -> bool:
    if not isinstance(verification, dict):
        findings.append(
            Finding(
                "error",
                "missing-runtime-verification",
                "enabled runtime feature must name an executable contract or integration test",
                feature_id,
            )
        )
        return False

    kind = verification.get("kind")
    if kind not in ALLOWED_RUNTIME_VERIFICATION_KINDS:
        findings.append(
            Finding(
                "error",
                "invalid-runtime-verification",
                f"runtime_verification.kind must be one of {sorted(ALLOWED_RUNTIME_VERIFICATION_KINDS)!r}",
                feature_id,
            )
        )
        return False

    path, path_error = _safe_repo_path(repo_root, verification.get("path"))
    if path_error is not None:
        findings.append(
            Finding("error", "invalid-runtime-verification", path_error, feature_id)
        )
        return False
    assert path is not None
    if not path.is_file():
        findings.append(
            Finding(
                "error",
                "stale-runtime-verification",
                f"runtime verification path {verification.get('path')!r} does not exist",
                feature_id,
            )
        )
        return False

    symbol = verification.get("symbol")
    command = verification.get("command")
    if not isinstance(symbol, str) or not symbol:
        findings.append(
            Finding(
                "error",
                "invalid-runtime-verification",
                "runtime_verification.symbol must be a non-empty string",
                feature_id,
            )
        )
        return False
    if (
        not isinstance(command, list)
        or not command
        or not all(isinstance(argument, str) and argument for argument in command)
    ):
        findings.append(
            Finding(
                "error",
                "invalid-runtime-verification",
                "runtime_verification.command must be a non-empty string array selecting symbol",
                feature_id,
            )
        )
        return False

    try:
        if path not in text_cache:
            text_cache[path] = path.read_text(encoding="utf-8")
        text = _executable_source_text(path, text_cache[path])
    except (OSError, UnicodeError) as exc:
        findings.append(
            Finding(
                "error",
                "unreadable-runtime-verification",
                f"failed to read {path}: {exc}",
                feature_id,
            )
        )
        return False
    if symbol not in text:
        findings.append(
            Finding(
                "error",
                "stale-runtime-verification",
                f"{verification.get('path')} no longer contains {symbol!r}",
                feature_id,
            )
        )
        return False
    try:
        rust_manifest = None
        if path.suffix == ".rs":
            manifest_path = repo_root / "codex-rs/.config/kd4-rust-tests.toml"
            if manifest_path not in rust_manifest_cache:
                rust_manifest_cache[manifest_path] = rust_test_runner.Manifest.load(
                    manifest_path
                )
            rust_manifest = rust_manifest_cache[manifest_path]
        _verification_route(verification, repo_root, rust_manifest)
    except (
        ValueError,
        OSError,
        KeyError,
        SyntaxError,
        rust_test_runner.RunnerError,
    ) as exc:
        findings.append(
            Finding("error", "invalid-runtime-verification", str(exc), feature_id)
        )
        return False

    if path.suffix.lower() == ".py":
        try:
            module = ast.parse(text)
        except SyntaxError as exc:
            findings.append(
                Finding(
                    "error",
                    "invalid-runtime-verification",
                    f"{verification.get('path')} is not valid Python: {exc}",
                    feature_id,
                )
            )
            return False
        matching_tests = [
            node
            for node in ast.walk(module)
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))
            and node.name == symbol
        ]
        if not matching_tests or all(
            all(
                isinstance(statement, ast.Pass)
                or (
                    isinstance(statement, ast.Expr)
                    and isinstance(statement.value, ast.Constant)
                    and isinstance(statement.value.value, str)
                )
                for statement in test.body
            )
            for test in matching_tests
        ):
            findings.append(
                Finding(
                    "error",
                    "vacuous-runtime-verification",
                    f"{verification.get('path')} test {symbol!r} has no executable contract body",
                    feature_id,
                )
            )
            return False
    return True


def _validate_contract_schema(
    *,
    feature: dict[str, Any],
    feature_id: str,
    repo_root: Path,
    findings: list[Finding],
    text_cache: dict[Path, str],
) -> None:
    declared = feature.get("contract_schema_version")
    source_text = feature.get("contract_schema_source")
    symbol = feature.get("contract_schema_symbol")
    present = [
        key in feature
        for key in (
            "contract_schema_version",
            "contract_schema_source",
            "contract_schema_symbol",
        )
    ]
    if not any(present):
        return
    if not all(present):
        findings.append(
            Finding(
                "error",
                "invalid-contract-schema",
                "contract schema version, source, and symbol must be declared together",
                feature_id,
            )
        )
        return
    if not isinstance(declared, int) or declared < 1:
        findings.append(
            Finding(
                "error",
                "invalid-contract-schema",
                "contract_schema_version must be a positive integer",
                feature_id,
            )
        )
        return
    path, path_error = _safe_repo_path(repo_root, source_text)
    if path_error is not None:
        findings.append(
            Finding("error", "invalid-contract-schema", path_error, feature_id)
        )
        return
    if not isinstance(symbol, str) or not symbol:
        findings.append(
            Finding(
                "error",
                "invalid-contract-schema",
                "contract_schema_symbol must be a non-empty string",
                feature_id,
            )
        )
        return
    assert path is not None
    if not path.is_file():
        findings.append(
            Finding(
                "error",
                "stale-contract-schema",
                f"contract schema source {source_text!r} does not exist",
                feature_id,
            )
        )
        return
    try:
        if path not in text_cache:
            text_cache[path] = path.read_text(encoding="utf-8")
        text = text_cache[path]
    except (OSError, UnicodeError) as exc:
        findings.append(
            Finding(
                "error",
                "unreadable-contract-schema",
                f"failed to read {path}: {exc}",
                feature_id,
            )
        )
        return
    match = re.search(
        rf"\b{re.escape(symbol)}\b\s*:\s*[^=]+?=\s*(\d+)\s*;",
        text,
    )
    if match is None:
        findings.append(
            Finding(
                "error",
                "stale-contract-schema",
                f"{source_text} no longer declares integer constant {symbol!r}",
                feature_id,
            )
        )
    elif int(match.group(1)) != declared:
        findings.append(
            Finding(
                "error",
                "contract-schema-drift",
                f"declared schema version {declared} does not match {symbol}={match.group(1)}",
                feature_id,
            )
        )


def _source_owner_evidence(
    *,
    source_owner_id: object,
    repo_root: Path,
    feature_id: str,
    findings: list[Finding],
    owner_cache: dict[str, dict[str, Any]] | None,
    owner_observation_cache: dict[str, tuple[frozenset[str], Counter[str]]],
    text_cache: dict[Path, str],
) -> tuple[Counter[str], dict[str, dict[str, Any]] | None]:
    kinds: Counter[str] = Counter()
    if not isinstance(source_owner_id, str) or not source_owner_id.strip():
        findings.append(
            Finding(
                "error",
                "invalid-source-owner",
                "source_owner must be a non-empty source_owners.toml owner id",
                feature_id,
            )
        )
        return kinds, owner_cache

    if owner_cache is None:
        owner_path = repo_root / SOURCE_OWNERS_FILE_NAME
        try:
            with owner_path.open("rb") as owner_file:
                owner_manifest = tomllib.load(owner_file)
        except (OSError, tomllib.TOMLDecodeError) as exc:
            findings.append(
                Finding(
                    "error",
                    "source-owner-load",
                    f"failed to load {SOURCE_OWNERS_FILE_NAME}: {exc}",
                    feature_id,
                )
            )
            return kinds, {}
        owners = owner_manifest.get("owners")
        if not isinstance(owners, list):
            findings.append(
                Finding(
                    "error",
                    "source-owner-load",
                    f"{SOURCE_OWNERS_FILE_NAME} owners must be an array",
                    feature_id,
                )
            )
            return kinds, {}
        owner_cache = {
            owner["id"]: owner
            for owner in owners
            if isinstance(owner, dict)
            and isinstance(owner.get("id"), str)
            and owner["id"]
        }

    owner = owner_cache.get(source_owner_id)
    if owner is None:
        findings.append(
            Finding(
                "error",
                "missing-source-owner",
                f"{SOURCE_OWNERS_FILE_NAME} has no owner {source_owner_id!r}",
                feature_id,
            )
        )
        return kinds, owner_cache

    cached_observation = owner_observation_cache.get(source_owner_id)
    if cached_observation is not None:
        feature_ids, cached_kinds = cached_observation
        if feature_id not in feature_ids:
            findings.append(
                Finding(
                    "error",
                    "source-owner-feature-mismatch",
                    f"owner {source_owner_id!r} does not declare feature {feature_id!r}",
                    feature_id,
                )
            )
            return kinds, owner_cache
        return Counter(cached_kinds), owner_cache

    declared_feature_ids = owner.get("feature_ids")
    feature_ids = (
        frozenset(item for item in declared_feature_ids if isinstance(item, str))
        if isinstance(declared_feature_ids, list)
        else frozenset()
    )
    if feature_id not in feature_ids:
        findings.append(
            Finding(
                "error",
                "source-owner-feature-mismatch",
                f"owner {source_owner_id!r} does not declare feature {feature_id!r}",
                feature_id,
            )
        )
        return kinds, owner_cache

    finding_count_before_observation = len(findings)

    def marker_is_live(marker: object, label: str) -> bool:
        if isinstance(marker, str):
            path_text = marker
            symbol = None
        elif isinstance(marker, dict):
            path_text = marker.get("path")
            symbol = marker.get("symbol")
        else:
            findings.append(
                Finding(
                    "error",
                    "invalid-source-owner-evidence",
                    f"{label} must be a path string or evidence table",
                    feature_id,
                )
            )
            return False

        path, path_error = _safe_repo_path(repo_root, path_text)
        if path_error is not None:
            findings.append(
                Finding(
                    "error",
                    "invalid-source-owner-evidence",
                    f"{label}: {path_error}",
                    feature_id,
                )
            )
            return False
        assert path is not None
        if not path.is_file():
            findings.append(
                Finding(
                    "error",
                    "stale-source-owner-evidence",
                    f"{label} path {path_text!r} does not exist",
                    feature_id,
                )
            )
            return False
        if symbol is None:
            return True
        if not isinstance(symbol, str) or not symbol:
            findings.append(
                Finding(
                    "error",
                    "invalid-source-owner-evidence",
                    f"{label} symbol must be a non-empty string",
                    feature_id,
                )
            )
            return False
        try:
            if path not in text_cache:
                text_cache[path] = path.read_text(encoding="utf-8")
            text = _executable_source_text(path, text_cache[path])
        except (OSError, UnicodeError) as exc:
            findings.append(
                Finding(
                    "error",
                    "unreadable-source-owner-evidence",
                    f"failed to read {path}: {exc}",
                    feature_id,
                )
            )
            return False
        if symbol not in text:
            findings.append(
                Finding(
                    "error",
                    "stale-source-owner-evidence",
                    f"{label} path {path_text!r} no longer contains {symbol!r}",
                    feature_id,
                )
            )
            return False
        return True

    primary_entries = owner.get("primary_entries")
    if isinstance(primary_entries, list):
        kinds["entrypoint"] = sum(
            marker_is_live(marker, f"primary_entries[{index}]")
            for index, marker in enumerate(primary_entries)
        )
    tests = owner.get("tests")
    if isinstance(tests, list):
        kinds["test"] = sum(
            marker_is_live(marker, f"tests[{index}]")
            for index, marker in enumerate(tests)
        )
    relationships = owner.get("relationships")
    if isinstance(relationships, list):
        for index, relationship in enumerate(relationships):
            if (
                not isinstance(relationship, dict)
                or relationship.get("category") != "runtime_registration"
            ):
                continue
            evidence = relationship.get("evidence")
            if not isinstance(evidence, list) or not evidence:
                findings.append(
                    Finding(
                        "error",
                        "invalid-source-owner-evidence",
                        f"relationships[{index}] runtime registration has no evidence",
                        feature_id,
                    )
                )
                continue
            if all(
                marker_is_live(
                    marker, f"relationships[{index}].evidence[{marker_index}]"
                )
                for marker_index, marker in enumerate(evidence)
            ):
                kinds["registration"] += 1
    if len(findings) == finding_count_before_observation:
        owner_observation_cache[source_owner_id] = (feature_ids, Counter(kinds))
    return kinds, owner_cache


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

    repo_root = repo_root.resolve()
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
    rust_manifest_cache: dict[Path, rust_test_runner.Manifest] = {}
    feature_registry_cache: dict[str, dict[str, bool] | None] = {}
    project_config_cache: dict[str, object] = {}
    owner_cache: dict[str, dict[str, Any]] | None = None
    owner_observation_cache: dict[str, tuple[frozenset[str], Counter[str]]] = {}
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

        for key in ("summary", "upstream_equivalent"):
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
        external_owner = feature.get("external_owner")
        has_owner = isinstance(owner, str) and bool(owner.strip())
        has_external_owner = isinstance(external_owner, str) and bool(
            external_owner.strip()
        )
        if not has_owner and not has_external_owner:
            findings.append(
                Finding(
                    "error",
                    "missing-field",
                    "feature must declare owner or external_owner",
                    feature_id,
                )
            )
        elif has_owner and has_external_owner:
            findings.append(
                Finding(
                    "error",
                    "invalid-owner",
                    "feature must declare exactly one of owner or external_owner",
                    feature_id,
                )
            )
        if has_owner:
            assert isinstance(owner, str)
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
        if has_external_owner and status != "planned":
            findings.append(
                Finding(
                    "error",
                    "invalid-external-owner",
                    "external_owner is only valid for a planned unsupported surface",
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
            feature_registry_cache=feature_registry_cache,
            project_config_cache=project_config_cache,
        )
        if runtime_status is not None:
            runtime_status_counts[runtime_status] += 1

        _validate_contract_schema(
            feature=feature,
            feature_id=feature_id,
            repo_root=repo_root,
            findings=findings,
            text_cache=text_cache,
        )

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

        source_owner = feature.get("source_owner")
        if status == "planned" and (
            source_owner is not None or feature.get("evidence")
        ):
            findings.append(
                Finding(
                    "error",
                    "planned-feature-has-production-route",
                    "planned feature must not declare live source-owner or inline route evidence",
                    feature_id,
                )
            )
        if source_owner is None:
            evidence_kinds = _validate_evidence(
                feature_id=feature_id,
                evidence_items=feature.get("evidence", []),
                repo_root=repo_root,
                findings=findings,
                text_cache=text_cache,
            )
        else:
            if feature.get("evidence"):
                findings.append(
                    Finding(
                        "error",
                        "duplicate-evidence-authority",
                        "source_owner and inline evidence cannot both author reachability",
                        feature_id,
                    )
                )
            evidence_kinds, owner_cache = _source_owner_evidence(
                source_owner_id=source_owner,
                repo_root=repo_root,
                feature_id=feature_id,
                findings=findings,
                owner_cache=owner_cache,
                owner_observation_cache=owner_observation_cache,
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
            if capability_kind == "runtime":
                _validate_runtime_verification(
                    feature_id=feature_id,
                    verification=feature.get("runtime_verification"),
                    repo_root=repo_root,
                    findings=findings,
                    text_cache=text_cache,
                    rust_manifest_cache=rust_manifest_cache,
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
    parser.add_argument(
        "--static-only",
        action="store_true",
        help="Check declared evidence presence without executing tests.",
    )
    parser.add_argument(
        "--run-runtime-verification",
        metavar="FEATURE_ID",
        help="Execute only this feature's declared runtime test instead of every enabled runtime test.",
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
    outcomes: list[dict[str, Any]] = []
    if args.static_only:
        if args.run_runtime_verification:
            raise SystemExit(
                "--static-only cannot be combined with --run-runtime-verification"
            )
        if args.json:
            payload = result.to_json()
            payload.update(
                staticEvidence="present" if result.ok else "invalid",
                runtimeVerification="not_run",
                runtimeVerificationExitCode=None,
            )
            print(json.dumps(payload, sort_keys=True))
        else:
            print(
                f"KD4 DECLARED EVIDENCE: {'PRESENT' if result.ok else 'INVALID'}; runtime tests not run"
            )
            for finding in result.findings:
                print(
                    f"[{finding.level}] [{finding.feature_id}] {finding.code}: {finding.message}"
                )
        return 0 if result.ok else 1
    if args.json:
        runtime_exit_code = (
            execute_runtime_verification(
                manifest_path,
                feature_id=args.run_runtime_verification,
                repo_root=args.repo_root,
                quiet=True,
                outcomes=outcomes,
            )
            if result.ok
            else None
        )
        payload = result.to_json()
        payload["runtimeVerificationExitCode"] = runtime_exit_code
        payload["runtimeVerificationResults"] = outcomes
        payload["staticEvidence"] = "present" if result.ok else "invalid"
        payload["ok"] = result.ok and runtime_exit_code == 0
        print(json.dumps(payload, sort_keys=True))
        return 1 if runtime_exit_code is None else runtime_exit_code
    else:
        verdict = "PASSED" if result.ok else "FAILED"
        counts = ", ".join(
            f"{status}={count}" for status, count in result.status_counts.items()
        )
        print(
            f"KD4 DECLARED EVIDENCE {verdict}: {result.feature_count} feature(s); {counts}; "
            f"runtime={result.runtime_status_counts}"
        )
        for finding in result.findings:
            feature = f" [{finding.feature_id}]" if finding.feature_id else ""
            print(
                f"[{finding.level.upper()}]{feature} {finding.code}: {finding.message}"
            )
    if not result.ok:
        return 1
    return execute_runtime_verification(
        manifest_path,
        feature_id=args.run_runtime_verification,
        repo_root=args.repo_root,
    )


if __name__ == "__main__":
    raise SystemExit(main())
