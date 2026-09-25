#!/usr/bin/env python3
"""Validate KD4 feature ownership, reachability, and executable test routes."""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from collections import Counter
from collections.abc import Sequence
from contextlib import ExitStack, contextmanager
from contextvars import ContextVar
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

import tomllib

if __package__:
    from scripts import rust_build_status, rust_test_runner
    from scripts.process_owner import run_finite
else:
    # runpy-based just recipes do not put this script's directory on sys.path.
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    import rust_build_status
    import rust_test_runner
    from process_owner import run_finite

_validation_lane = ContextVar("feature_validation_lane", default=None)
_validation_session = ContextVar("feature_validation_session", default=None)


@contextmanager
def validation_session():
    """Retain a lazily acquired lane across export and runtime verification."""
    with ExitStack() as stack:
        token = _validation_session.set(stack)
        try:
            yield
        finally:
            _validation_session.reset(token)


@contextmanager
def validation_lane(repo_root):
    active = _validation_lane.get()
    if active is not None and active[0] == repo_root.resolve():
        yield active[1]
        return
    inherited = os.environ.get("CODEX_CARGO_LANE_TARGET_DIR")
    owner = os.environ.get("CODEX_CARGO_LANE_OWNER_PID")
    if (
        inherited
        and owner
        and rust_build_status.lane_active_lock_is_held(Path(inherited))
    ):
        token = _validation_lane.set((repo_root.resolve(), Path(inherited)))
        try:
            yield Path(inherited)
        finally:
            _validation_lane.reset(token)
        return
    session = _validation_session.get()
    if session is not None:
        target = _enter_core_lane(session, repo_root)
        token = _validation_lane.set((repo_root.resolve(), target))
        session.callback(_validation_lane.reset, token)
        yield target
        return
    with ExitStack() as stack:
        target = _enter_core_lane(stack, repo_root)
        token = _validation_lane.set((repo_root.resolve(), target))
        try:
            yield target
        finally:
            _validation_lane.reset(token)


def _enter_core_lane(stack: ExitStack, repo_root: Path) -> Path:
    reservation = rust_build_status.reserve_cargo_lane(
        repo_root=repo_root,
        requested_lane="core-tests",
        command=["cargo", "nextest", "run", "-p", "codex-core"],
    )
    try:
        _, target = stack.enter_context(reservation)
    except (RuntimeError, ValueError) as exc:
        # A busy shared lane refuses a duplicate cold build. Nothing ran, so
        # report it through the runner's error path instead of crashing.
        raise rust_test_runner.RunnerError(
            f"no Cargo lane is available for KD4 validation: {exc}",
            outcome="not_executed",
        ) from exc
    return target


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
ALLOWED_RUNTIME_VERIFICATION_KINDS = frozenset({"contract_test", "integration_test"})
# Strict like kd4-rust-tests.toml: a misspelled optional key would skip its check.
ALLOWED_FEATURE_KEYS = frozenset(
    "id version status capability_kind owner external_owner summary "
    "upstream_equivalent config_keys runtime_feature_key runtime_status "
    "runtime_status_source benchmark_on benchmark_control runtime_verification "
    "evidence generated_artifacts retired_paths".split()
)
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


def _rust_scope_items(
    text: str,
) -> dict[tuple[str, str], list[tuple[str | None, str | None]]]:
    """Read named module/function declarations at this scope, excluding literal bodies."""
    token = re.compile(
        r'//[^\n]*|/\*|r(?P<hashes>\#*)".*?"(?P=hashes)|'
        r'"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])\'|[A-Za-z_][A-Za-z_0-9]*|[^\s]',
        re.DOTALL,
    )
    tokens = []
    cursor = 0
    while match := token.search(text, cursor):
        value = match.group()
        cursor = match.end()
        if value.startswith("//"):
            continue
        if value == "/*":
            depth = 1
            while depth:
                nested = re.search(r"/\*|\*/", text[cursor:])
                if nested is None:
                    raise ValueError("unclosed Rust comment in verification source")
                cursor += nested.end()
                depth += 1 if nested.group() == "/*" else -1
            continue
        tokens.append((value, match.start(), cursor))
    items: dict[tuple[str, str], list[tuple[str | None, str | None]]] = {}
    index = 0
    path = None
    while index < len(tokens):
        value = tokens[index][0]
        if (
            value == "#"
            and index + 5 < len(tokens)
            and [item[0] for item in tokens[index : index + 4]]
            == ["#", "[", "path", "="]
            and tokens[index + 5][0] == "]"
        ):
            literal = tokens[index + 4][0]
            if not literal.startswith('"'):
                raise ValueError(
                    "unsupported Rust path attribute in verification route"
                )
            path = json.loads(literal)
            index += 6
            continue
        if value in {"mod", "fn"} and index + 2 < len(tokens):
            name = tokens[index + 1][0]
            after_name = index + 2
            if value == "fn":
                items.setdefault((value, name), []).append((None, None))
            elif tokens[after_name][0] == ";":
                items.setdefault((value, name), []).append((path, None))
                path = None
                index += 3
                continue
            elif tokens[after_name][0] == "{":
                depth = 1
                end = after_name + 1
                while end < len(tokens) and depth:
                    depth += (tokens[end][0] == "{") - (tokens[end][0] == "}")
                    end += 1
                if depth:
                    raise ValueError("unclosed Rust module in verification source")
                body = text[tokens[after_name][2] : tokens[end - 1][1]]
                items.setdefault((value, name), []).append((path, body))
                path = None
                index = end
                continue
        if value in {"{", "[", "("}:
            closing = {"{": "}", "[": "]", "(": ")"}[value]
            depth = 1
            index += 1
            while index < len(tokens) and depth:
                depth += (tokens[index][0] == value) - (tokens[index][0] == closing)
                index += 1
            if value == "{":
                path = None
            continue
        if value == ";":
            path = None
        index += 1
    return items


def _rust_test_source(binary_root: Path, identity: str, repo_root: Path) -> Path | None:
    """Resolve the exact selected module chain, including #[path] and inline modules."""
    source = binary_root.resolve()
    if not source.is_file():
        return None
    text = source.read_text(encoding="utf-8")
    module_dir = attribute_dir = source.parent
    components = identity.split("::")
    for component in components[:-1]:
        declarations = _rust_scope_items(text).get(("mod", component), [])
        if len(declarations) != 1:
            return None
        path, body = declarations[0]
        if body is not None:
            module_dir = attribute_dir / path if path else module_dir / component
            attribute_dir = module_dir
            text = body
            continue
        if path:
            candidates = [attribute_dir / path]
        else:
            candidates = [
                module_dir / f"{component}.rs",
                module_dir / component / "mod.rs",
            ]
        candidates = [
            candidate.resolve() for candidate in candidates if candidate.is_file()
        ]
        if len(candidates) != 1 or not candidates[0].is_relative_to(
            repo_root.resolve()
        ):
            return None
        source = candidates[0]
        text = source.read_text(encoding="utf-8")
        attribute_dir = source.parent
        module_dir = (
            source.parent if source.name == "mod.rs" else source.with_suffix("")
        )
    return (
        source
        if len(_rust_scope_items(text).get(("fn", components[-1]), [])) == 1
        else None
    )


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
    if source.suffix != ".rs":
        raise ValueError(
            "runtime verification must name a Rust test selected by an exact-test gate"
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
    binary_root = cargo_path.parent / cargo.get("lib", {}).get("path", "src/lib.rs")
    if relative.parts[0] == "tests":
        if len(relative.parts) == 2:
            selector = ["--test", source.stem]
            binary_root = source
        else:
            # Legacy aggregators use `mod suite;`; bounded shards keep `suite`
            # inline and explicitly register the selected source within it.
            candidates = [
                p
                for p in (cargo_path.parent / "tests").glob("*.rs")
                if re.search(
                    r"\bmod\s+" + re.escape(relative.parts[1]) + r"\s*;",
                    p.read_text(encoding="utf-8"),
                )
                or (
                    len(relative.parts) == 3
                    and re.search(
                        r"\bmod\s+" + re.escape(relative.parts[1]) + r"\s*\{",
                        p.read_text(encoding="utf-8"),
                    )
                    and re.search(
                        r'#\[path\s*=\s*"'
                        + re.escape(source.name)
                        + r'"\]\s*mod\s+'
                        + re.escape(source.stem)
                        + r"\s*;",
                        p.read_text(encoding="utf-8"),
                    )
                )
            ]
            if len(candidates) != 1:
                raise ValueError(
                    "verification source must resolve to one integration test binary"
                )
            selector = ["--test", candidates[0].stem]
            binary_root = candidates[0]
    else:
        for binary in cargo.get("bin", []):
            if binary.get("path") == relative.as_posix():
                selector = ["--bin", binary["name"]]
                binary_root = source
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
    if gate is None:
        raise ValueError("capability gate must name an existing gate")
    matching_steps = [
        step
        for step in gate.steps
        if rust_manifest.targets[step.target].selection_args()
        == ["-p", package, *selector]
    ]
    if not matching_steps:
        raise ValueError(
            "capability gate must select the declared source's package and test binary"
        )
    if not any(
        test.split("::")[-1] == symbol
        and _rust_test_source(binary_root, test, repo_root) == source.resolve()
        for step in matching_steps
        for test in step.tests
    ):
        raise ValueError(
            f"capability gate must require the exact source-qualified test identity of {symbol!r}"
        )
    return "nextest"


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
    if not matching:
        return 0
    # Resolve every route before executing anything. Capability gates use the
    # same runner as `just core-gate`.
    try:
        rust_manifest = rust_test_runner.Manifest.load(
            repo_root / "codex-rs/.config/kd4-rust-tests.toml"
        )
        for feature in matching:
            verification = feature.get("runtime_verification")
            if not isinstance(feature.get("id"), str) or not isinstance(
                verification, dict
            ):
                raise TypeError("feature has no executable runtime verification")
            _verification_route(verification, repo_root, rust_manifest)
    except (
        TypeError,
        ValueError,
        OSError,
        KeyError,
        rust_test_runner.RunnerError,
    ) as exc:
        if not quiet:
            print(f"invalid runtime verification: {exc}")
        return 2

    gates = list(
        dict.fromkeys(
            feature["runtime_verification"]["command"][3] for feature in matching
        )
    )
    try:
        cwd = repo_root / "codex-rs"
        with validation_lane(repo_root) as target:
            metadata = rust_test_runner.load_metadata(cwd=cwd)
            runner = rust_test_runner.RustTestRunner(
                rust_manifest,
                metadata,
                cwd=cwd,
                profile="fast",
                target_dir=target,
            )
            # Exact selectors and the same execution's outcomes supply proof.
            # The runner still validates any explicitly declared filters before
            # batching; generated exact selectors need no preliminary discovery.
            completed = runner.run_gates(gates, quiet=quiet, discover=False)
    except (rust_test_runner.RunnerError, OSError) as exc:
        for feature in matching:
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
    for feature in matching:
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
        with validation_lane(repo_root) as target:
            completed = run_finite(
                [
                    "cargo",
                    "run",
                    "--target-dir",
                    str(target),
                    "--quiet",
                    "--manifest-path",
                    str(manifest_path),
                    "-p",
                    "codex-features",
                    "--bin",
                    "codex-features-export",
                ],
                cwd=repo_root,
                output_limit=4 * 1024 * 1024,
                stderr=None,
            )
    except rust_test_runner.RunnerError as exc:
        # Keep the cause visible next to the unresolved-runtime-status finding.
        print(f"KD4 feature defaults unavailable: {exc}", file=sys.stderr)
        return None
    except (OSError, UnicodeError):
        return None
    if completed.returncode != 0 or completed.output_truncated:
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
        [
            key
            for key in config_keys
            if isinstance(key, str) and key.startswith("features.")
        ]
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
    if (
        not isinstance(runtime_status, str)
        or runtime_status not in ALLOWED_RUNTIME_STATUSES
    ):
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


def _stripped_source(
    path: Path, text_cache: dict[Path, str], stripped_cache: dict[Path, str]
) -> str:
    """Read a file once and strip its comments once, however many items cite it."""
    if path not in stripped_cache:
        if path not in text_cache:
            text_cache[path] = path.read_text(encoding="utf-8")
        stripped_cache[path] = _executable_source_text(path, text_cache[path])
    return stripped_cache[path]


def _validate_evidence(
    *,
    feature_id: str,
    evidence_items: object,
    repo_root: Path,
    findings: list[Finding],
    text_cache: dict[Path, str],
    stripped_cache: dict[Path, str] | None = None,
) -> Counter[str]:
    kinds: Counter[str] = Counter()
    if stripped_cache is None:
        stripped_cache = {}
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
        if not isinstance(kind, str) or kind not in ALLOWED_EVIDENCE_KINDS:
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
        if not isinstance(contains, str) or not contains:
            findings.append(
                Finding(
                    "error",
                    "invalid-evidence-match",
                    f"evidence[{index}] contains must be a non-empty string",
                    feature_id,
                )
            )
            continue

        try:
            text = _stripped_source(path, text_cache, stripped_cache)
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

        if contains not in text:
            findings.append(
                Finding(
                    "error",
                    "stale-evidence",
                    f"{evidence['path']} no longer contains {contains!r}",
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
    if not isinstance(kind, str) or kind not in ALLOWED_RUNTIME_VERIFICATION_KINDS:
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

    # The route resolves the gate's exact test identity to one `fn` declaration
    # in this source, so a renamed, commented, or relocated test fails here.
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
        rust_test_runner.RunnerError,
    ) as exc:
        findings.append(
            Finding("error", "invalid-runtime-verification", str(exc), feature_id)
        )
        return False
    return True


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
    stripped_cache: dict[Path, str] = {}
    rust_manifest_cache: dict[Path, rust_test_runner.Manifest] = {}
    feature_registry_cache: dict[str, dict[str, bool] | None] = {}
    project_config_cache: dict[str, object] = {}
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
        if not isinstance(status, str) or status not in ALLOWED_STATUSES:
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
        if (
            not isinstance(capability_kind, str)
            or capability_kind not in ALLOWED_CAPABILITY_KINDS
        ):
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

        unknown_keys = sorted(set(feature) - ALLOWED_FEATURE_KEYS)
        if unknown_keys:
            findings.append(
                Finding(
                    "error",
                    "unknown-feature-key",
                    f"unsupported feature keys: {', '.join(unknown_keys)}",
                    feature_id,
                )
            )
        if status == "planned" and feature.get("evidence"):
            findings.append(
                Finding(
                    "error",
                    "planned-feature-has-production-route",
                    "planned feature must not declare live inline route evidence",
                    feature_id,
                )
            )
        evidence_kinds = _validate_evidence(
            feature_id=feature_id,
            evidence_items=feature.get("evidence", []),
            repo_root=repo_root,
            findings=findings,
            text_cache=text_cache,
            stripped_cache=stripped_cache,
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
                capability_kind in ("runtime", "workflow", "guidance")
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
            # Runtime features prove their test through runtime_verification.
            if capability_kind == "workflow" and evidence_kinds["test"] == 0:
                findings.append(
                    Finding(
                        "error",
                        "missing-test",
                        "enabled workflow feature has no declared test evidence",
                        feature_id,
                    )
                )
            if capability_kind == "runtime":
                _validate_runtime_verification(
                    feature_id=feature_id,
                    verification=feature.get("runtime_verification"),
                    repo_root=repo_root,
                    findings=findings,
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
    with validation_session():
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
