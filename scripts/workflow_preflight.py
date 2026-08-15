#!/usr/bin/env python3
"""Validate and resolve a KD4 shared-worktree coordination manifest."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import uuid
from datetime import datetime, timedelta, timezone
from pathlib import Path, PurePosixPath
from typing import Any, Sequence

try:
    from scripts.generated_output_lock import GenerationLockError
    from scripts.generated_output_lock import repository_lock
except ModuleNotFoundError:
    from generated_output_lock import GenerationLockError
    from generated_output_lock import repository_lock


SCHEMA_VERSION = 1
WORKSPACE_STRATEGIES = {"auto", "shared", "isolated"}
DEFAULT_LEASE_SECONDS = 3600
MIN_LEASE_SECONDS = 60
MAX_LEASE_SECONDS = 86400
REQUIRED_FIELDS = {
    "schema_version",
    "assignment_id",
    "root_task_id",
    "repository_root",
    "starting_revision",
    "path_claims",
    "contract_claims",
    "dependencies",
    "generated_outputs",
    "generated_output_owner",
    "validation_owner",
    "validation_commands",
    "cargo_lane",
    "workspace_strategy",
}


class PreflightError(ValueError):
    """A manifest is unsafe or incomplete."""


def git_bytes(root: Path, *args: str) -> bytes:
    try:
        completed = subprocess.run(
            ["git", "-C", str(root), *args],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
    except OSError as error:
        raise PreflightError(f"could not run git: {error}") from error
    if completed.returncode != 0:
        detail = (
            completed.stderr.decode("utf-8", errors="replace").strip()
            or f"exit code {completed.returncode}"
        )
        raise PreflightError(f"git {' '.join(args)} failed: {detail}")
    return completed.stdout


def git(root: Path, *args: str) -> str:
    return git_bytes(root, *args).decode("utf-8", errors="replace")


def nonempty_string(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise PreflightError(f"{field} must be a non-empty string")
    value = value.strip()
    if re.search(r"<[^<>\r\n]+>", value):
        raise PreflightError(f"{field} contains an unresolved template placeholder")
    return value


def string_list(value: Any, field: str) -> list[str]:
    if not isinstance(value, list):
        raise PreflightError(f"{field} must be an array")
    result = [nonempty_string(item, field) for item in value]
    if len(set(result)) != len(result):
        raise PreflightError(f"{field} contains duplicates")
    return result


def normalize_claim_path(value: Any, root: Path) -> str:
    path = nonempty_string(value, "path_claims.path").replace("\\", "/")
    candidate = PurePosixPath(path)
    if candidate.is_absolute() or any(
        part in {"", ".", ".."} for part in candidate.parts
    ):
        raise PreflightError(f"unsafe repository-relative claim path: {path}")
    normalized = candidate.as_posix()
    resolved = (root / Path(*candidate.parts)).resolve(strict=False)
    try:
        resolved.relative_to(root)
    except ValueError as error:
        raise PreflightError(f"claim resolves outside repository: {path}") from error
    return normalized


def normalize_claims(value: Any, root: Path) -> list[dict[str, Any]]:
    if not isinstance(value, list):
        raise PreflightError("path_claims must be an array")
    claims: list[dict[str, Any]] = []
    for item in value:
        if not isinstance(item, dict) or set(item) != {"path", "recursive"}:
            raise PreflightError(
                "each path claim must contain only `path` and `recursive`"
            )
        if not isinstance(item["recursive"], bool):
            raise PreflightError("path_claims.recursive must be a boolean")
        claim = {
            "path": normalize_claim_path(item["path"], root),
            "recursive": item["recursive"],
        }
        if claim in claims:
            raise PreflightError(f"duplicate path claim: {claim['path']}")
        claims.append(claim)
    return claims


def repository_paths_are_case_insensitive(root: Path) -> bool:
    """Detect case aliases without writing a probe into the repository."""
    current = root.resolve()
    for path in (current, *current.parents):
        swapped = "".join(
            character.swapcase() if character.isalpha() else character
            for character in path.name
        )
        if not swapped or swapped == path.name:
            continue
        alias = path.with_name(swapped)
        try:
            if alias.exists() and os.path.samefile(path, alias):
                return True
        except OSError:
            continue
    return os.name == "nt"


def claim_covers(
    claim: dict[str, Any], path: str, *, case_insensitive: bool = False
) -> bool:
    claim_path = claim["path"].casefold() if case_insensitive else claim["path"]
    candidate = path.casefold() if case_insensitive else path
    return candidate == claim_path or (
        claim["recursive"] and candidate.startswith(f"{claim_path}/")
    )


def claims_overlap(
    left: dict[str, Any], right: dict[str, Any], *, case_insensitive: bool = False
) -> bool:
    return claim_covers(
        left, right["path"], case_insensitive=case_insensitive
    ) or claim_covers(right, left["path"], case_insensitive=case_insensitive)


def workspace_fingerprint(root: Path) -> str:
    digest = hashlib.sha256()
    tracked_diff = git_bytes(root, "diff", "--binary", "--no-ext-diff", "HEAD", "--")
    digest.update(len(tracked_diff).to_bytes(8, "big"))
    digest.update(tracked_diff)
    untracked = git_bytes(root, "ls-files", "--others", "--exclude-standard", "-z")
    paths = sorted(path for path in untracked.split(b"\0") if path)
    for raw_path in paths:
        relative = Path(os.fsdecode(raw_path))
        absolute = root / relative
        digest.update(len(raw_path).to_bytes(8, "big"))
        digest.update(raw_path)
        if absolute.is_symlink():
            payload = os.readlink(absolute).encode("utf-8", errors="surrogateescape")
        else:
            try:
                payload = absolute.read_bytes()
            except OSError as error:
                raise PreflightError(
                    f"could not fingerprint untracked path {relative}: {error}"
                ) from error
        digest.update(len(payload).to_bytes(8, "big"))
        digest.update(payload)
    return digest.hexdigest()


def git_metadata_path(root: Path, argument: str) -> Path:
    value = git(root, "rev-parse", argument).strip()
    path = Path(value)
    if not path.is_absolute():
        path = root / path
    return path.resolve()


def persistent_identity(path: Path) -> str:
    path.parent.mkdir(parents=True, exist_ok=True)
    candidate = uuid.uuid4().hex
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{path.name}.", suffix=".tmp", dir=path.parent, text=True
    )
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="\n") as handle:
            handle.write(candidate)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(temporary, 0o600)

        lock_path = path.with_name(f".{path.name}.lock")
        with repository_lock(lock_path, candidate, f"identity {path.name}"):
            try:
                existing = path.read_text(encoding="utf-8").strip()
            except FileNotFoundError:
                existing = ""
            except OSError as error:
                raise PreflightError(
                    f"could not read repository identity {path}: {error}"
                ) from error
            if existing:
                return existing
            temporary.replace(path)
            os.chmod(path, 0o600)
            try:
                directory_fd = os.open(path.parent, os.O_RDONLY)
            except OSError:
                directory_fd = None
            if directory_fd is not None:
                try:
                    os.fsync(directory_fd)
                finally:
                    os.close(directory_fd)
            return candidate
    finally:
        temporary.unlink(missing_ok=True)


def repository_identities(root: Path) -> tuple[str, str]:
    common = git_metadata_path(root, "--git-common-dir")
    worktree_git_dir = git_metadata_path(root, "--git-dir")
    identity_root = common / "codex"
    return (
        persistent_identity(identity_root / "repository-id"),
        persistent_identity(worktree_git_dir / "codex" / "workspace-id"),
    )


def manifest_repository_root(raw: dict[str, Any], manifest_path: Path) -> Path:
    repository_value = nonempty_string(raw.get("repository_root"), "repository_root")
    repository_candidate = Path(repository_value)
    if not repository_candidate.is_absolute():
        repository_candidate = manifest_path.parent / repository_candidate
    return Path(
        git(repository_candidate.resolve(), "rev-parse", "--show-toplevel").strip()
    ).resolve()


def workflow_registry_dir(root: Path) -> Path:
    return git_metadata_path(root, "--git-common-dir") / "codex" / "workflow-preflight"


def registry_receipt_path(root: Path, assignment_id: str) -> Path:
    name = hashlib.sha256(assignment_id.encode("utf-8")).hexdigest()
    return workflow_registry_dir(root) / f"{name}.json"


def receipt_is_active(receipt: dict[str, Any], now: datetime) -> bool:
    raw_expiry = receipt.get("expires_at")
    if not isinstance(raw_expiry, str):
        return False
    try:
        expiry = datetime.fromisoformat(raw_expiry)
    except ValueError:
        return False
    if expiry.tzinfo is None:
        return False
    return expiry > now


def active_registry_receipts(
    root: Path, assignment_id: str, now: datetime
) -> list[dict[str, Any]]:
    current_path = registry_receipt_path(root, assignment_id)
    registry = workflow_registry_dir(root)
    if not registry.is_dir():
        return []
    active: list[dict[str, Any]] = []
    for path in sorted(registry.glob("*.json")):
        if path == current_path:
            continue
        receipt = load_json(path)
        if receipt_is_active(receipt, now):
            active.append(receipt)
        else:
            path.unlink(missing_ok=True)
    return active


def normalize_lane_path(value: Any, field: str, root: Path) -> str:
    path = Path(nonempty_string(value, field))
    if not path.is_absolute():
        path = root / path
    return str(path.resolve(strict=False))


def manifest_fingerprint(manifest: dict[str, Any]) -> str:
    payload = json.dumps(
        manifest,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=True,
    ).encode("utf-8")
    return hashlib.sha256(payload).hexdigest()


def load_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise PreflightError(f"could not read manifest {path}: {error}") from error
    if not isinstance(value, dict):
        raise PreflightError(f"manifest {path} must contain one JSON object")
    return value


def resolve_manifest(
    raw: dict[str, Any],
    *,
    manifest_path: Path,
    against: Sequence[dict[str, Any]] = (),
    lease_seconds: int = DEFAULT_LEASE_SECONDS,
    now: datetime | None = None,
) -> dict[str, Any]:
    if not MIN_LEASE_SECONDS <= lease_seconds <= MAX_LEASE_SECONDS:
        raise PreflightError(
            f"lease_seconds must be between {MIN_LEASE_SECONDS} and {MAX_LEASE_SECONDS}"
        )
    recorded_at = now or datetime.now(timezone.utc)
    unknown = set(raw) - REQUIRED_FIELDS
    missing = REQUIRED_FIELDS - set(raw)
    if unknown:
        raise PreflightError(f"unknown manifest fields: {sorted(unknown)}")
    if missing:
        raise PreflightError(f"missing manifest fields: {sorted(missing)}")
    if raw["schema_version"] != SCHEMA_VERSION:
        raise PreflightError(f"schema_version must be {SCHEMA_VERSION}")

    assignment_id = nonempty_string(raw["assignment_id"], "assignment_id")
    root_task_id = nonempty_string(raw["root_task_id"], "root_task_id")
    repository_root = manifest_repository_root(raw, manifest_path)
    repository_id, workspace_id = repository_identities(repository_root)
    commit = git(repository_root, "rev-parse", "HEAD").strip()
    starting_workspace_fingerprint = workspace_fingerprint(repository_root)
    requested_revision = nonempty_string(raw["starting_revision"], "starting_revision")
    if requested_revision != "auto" and requested_revision != commit:
        raise PreflightError(
            f"starting_revision is stale: requested {requested_revision}, current {commit}"
        )

    path_claims = normalize_claims(raw["path_claims"], repository_root)
    repository_case_insensitive = repository_paths_are_case_insensitive(repository_root)
    contract_claims = string_list(raw["contract_claims"], "contract_claims")
    dependencies = string_list(raw["dependencies"], "dependencies")
    generated_outputs = [
        normalize_claim_path(path, repository_root)
        for path in string_list(raw["generated_outputs"], "generated_outputs")
    ]
    generated_owner = nonempty_string(
        raw["generated_output_owner"], "generated_output_owner"
    )
    validation_owner = nonempty_string(raw["validation_owner"], "validation_owner")
    validation_commands = string_list(raw["validation_commands"], "validation_commands")
    strategy = nonempty_string(raw["workspace_strategy"], "workspace_strategy")
    if strategy not in WORKSPACE_STRATEGIES:
        raise PreflightError(
            f"workspace_strategy must be one of {sorted(WORKSPACE_STRATEGIES)}"
        )
    if generated_outputs and generated_owner.casefold() == "none":
        raise PreflightError(
            "generated outputs require a concrete generated_output_owner"
        )
    for output in generated_outputs:
        if not any(
            claim_covers(claim, output, case_insensitive=repository_case_insensitive)
            for claim in path_claims
        ):
            raise PreflightError(f"generated output is outside path claims: {output}")

    cargo_lane = raw["cargo_lane"]
    if not isinstance(cargo_lane, dict) or set(cargo_lane) != {
        "target_dir",
        "cargo_home",
    }:
        raise PreflightError(
            "cargo_lane must contain only `target_dir` and `cargo_home`"
        )
    target_dir = normalize_lane_path(
        cargo_lane["target_dir"], "cargo_lane.target_dir", repository_root
    )
    cargo_home = normalize_lane_path(
        cargo_lane["cargo_home"], "cargo_lane.cargo_home", repository_root
    )

    resolved = {
        "schema_version": SCHEMA_VERSION,
        "assignment_id": assignment_id,
        "root_task_id": root_task_id,
        "repository_root": str(repository_root),
        "repository_id": repository_id,
        "workspace_id": workspace_id,
        "starting_revision": {
            "commit": commit,
            "workspace_fingerprint": starting_workspace_fingerprint,
        },
        "path_claims": path_claims,
        "contract_claims": contract_claims,
        "dependencies": dependencies,
        "generated_outputs": generated_outputs,
        "generated_output_owner": generated_owner,
        "validation_owner": validation_owner,
        "validation_commands": validation_commands,
        "cargo_lane": {
            "target_dir": target_dir,
            "cargo_home": cargo_home,
        },
        "workspace_strategy": strategy,
        "advisories": [],
        "recorded_at": recorded_at.isoformat(),
        "expires_at": (recorded_at + timedelta(seconds=lease_seconds)).isoformat(),
    }

    for active in against:
        if active.get("assignment_id") == assignment_id:
            continue
        active_root = nonempty_string(active.get("repository_root"), "repository_root")
        active_root_path = Path(active_root).resolve()
        active_repository_id = active.get("repository_id")
        active_workspace_id = active.get("workspace_id")
        if not isinstance(active_repository_id, str) or not isinstance(
            active_workspace_id, str
        ):
            active_repository_id, active_workspace_id = repository_identities(
                active_root_path
            )
        if active_repository_id != repository_id:
            continue
        active_strategy = nonempty_string(
            active.get("workspace_strategy"), "workspace_strategy"
        )
        active_claims = normalize_claims(active.get("path_claims"), active_root_path)
        compare_case_insensitively = (
            repository_case_insensitive
            or repository_paths_are_case_insensitive(active_root_path)
        )
        overlap = [
            (left["path"], right["path"])
            for left in path_claims
            for right in active_claims
            if claims_overlap(left, right, case_insensitive=compare_case_insensitively)
        ]
        shared_contracts = sorted(
            set(contract_claims)
            & set(string_list(active.get("contract_claims"), "contract_claims"))
        )
        isolated_elsewhere = workspace_id != active_workspace_id and (
            strategy == "isolated" or active_strategy == "isolated"
        )
        if (overlap or shared_contracts) and not isolated_elsewhere:
            resolved["advisories"].append(
                {
                    "kind": "claim_overlap",
                    "assignment_id": active.get("assignment_id"),
                    "paths": overlap,
                    "contracts": shared_contracts,
                }
            )
        active_lane = active.get("cargo_lane")
        active_target_dir = (
            normalize_lane_path(
                active_lane.get("target_dir"),
                "cargo_lane.target_dir",
                active_root_path,
            )
            if isinstance(active_lane, dict)
            else None
        )
        if (
            active_target_dir is not None
            and os.path.normcase(active_target_dir) == os.path.normcase(target_dir)
        ):
            resolved["advisories"].append(
                {
                    "kind": "cargo_lane_overlap",
                    "assignment_id": active.get("assignment_id"),
                    "target_dir": target_dir,
                }
            )

    resolved["manifest_fingerprint"] = manifest_fingerprint(resolved)
    return resolved


def write_atomic(path: Path, value: dict[str, Any]) -> None:
    payload = json.dumps(value, indent=2, sort_keys=True) + "\n"
    write_atomic_bytes(path, payload.encode("utf-8"))


def write_atomic_bytes(path: Path, payload: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{path.name}.",
        suffix=".tmp",
        dir=path.parent,
    )
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path, nargs="?")
    parser.add_argument(
        "--against",
        type=Path,
        action="append",
        default=[],
        help="Resolved active manifest to report path, contract, and Cargo overlap advisories.",
    )
    parser.add_argument(
        "--output",
        type=Path,
        help="Atomically write the resolved preflight receipt.",
    )
    parser.add_argument(
        "--release",
        metavar="ASSIGNMENT_ID",
        help="Release a previously published active preflight receipt.",
    )
    parser.add_argument(
        "--repository-root",
        type=Path,
        help="Repository used with --release (defaults to the current directory).",
    )
    parser.add_argument(
        "--lease-seconds",
        type=int,
        default=DEFAULT_LEASE_SECONDS,
        help=(
            "Active receipt lifetime before crash recovery removes it "
            f"({MIN_LEASE_SECONDS}-{MAX_LEASE_SECONDS}; default: {DEFAULT_LEASE_SECONDS})."
        ),
    )
    args = parser.parse_args(argv)
    try:
        if args.release:
            release_root = args.repository_root or Path.cwd()
            root = Path(
                git(release_root.resolve(), "rev-parse", "--show-toplevel").strip()
            ).resolve()
            lock_path = (
                git_metadata_path(root, "--git-common-dir")
                / "codex"
                / "workflow-preflight.lock"
            )
            with repository_lock(
                lock_path, f"release:{args.release}", "workflow preflight registry"
            ):
                registry_receipt_path(root, args.release).unlink(missing_ok=True)
            print(f"released workflow preflight receipt for {args.release}")
            return 0
        if args.manifest is None:
            parser.error("manifest is required unless --release is used")
        manifest_path = args.manifest.resolve()
        raw = load_json(args.manifest)
        root = manifest_repository_root(raw, manifest_path)
        assignment_id = nonempty_string(raw.get("assignment_id"), "assignment_id")
        lock_path = (
            git_metadata_path(root, "--git-common-dir")
            / "codex"
            / "workflow-preflight.lock"
        )
        with repository_lock(lock_path, assignment_id, "workflow preflight registry"):
            now = datetime.now(timezone.utc)
            explicit_receipts = [load_json(path) for path in args.against]
            resolved = resolve_manifest(
                raw,
                manifest_path=manifest_path,
                against=[
                    *active_registry_receipts(root, assignment_id, now),
                    *(
                        receipt
                        for receipt in explicit_receipts
                        if receipt_is_active(receipt, now)
                    ),
                ],
                lease_seconds=args.lease_seconds,
                now=now,
            )
            receipt_path = registry_receipt_path(root, assignment_id)
            previous_receipt = (
                receipt_path.read_bytes() if receipt_path.exists() else None
            )
            write_atomic(receipt_path, resolved)
            try:
                if args.output:
                    write_atomic(args.output, resolved)
            except OSError as output_error:
                try:
                    if previous_receipt is None:
                        receipt_path.unlink(missing_ok=True)
                    else:
                        write_atomic_bytes(receipt_path, previous_receipt)
                except OSError as rollback_error:
                    raise PreflightError(
                        "output publication failed and the previous registry receipt "
                        f"could not be restored: output={output_error}; rollback={rollback_error}"
                    ) from output_error
                raise
        print(json.dumps(resolved, indent=2, sort_keys=True))
        return 0
    except (GenerationLockError, PreflightError, OSError) as error:
        print(f"workflow preflight failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
