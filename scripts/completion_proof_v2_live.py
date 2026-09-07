"""Inventory V2 reader used by the existing completion runner.

This module verifies structure and exact bytes. Only Core's private activation
record authorizes selecting a generation; a public manifest is not that record.
"""

from __future__ import annotations

import hashlib
import json
import os
import uuid
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

try:
    from scripts import completion_proof_inventory_v2 as contracts
    from scripts.focused_live_successor_catalog import resolve_current_successor_v1
except ImportError:  # pragma: no cover - direct script execution
    # The trusted runner is launched by file path (Just's runpy launcher and
    # the reconciliation worker child), so `scripts` is not importable there.
    import completion_proof_inventory_v2 as contracts  # type: ignore[no-redef]
    from focused_live_successor_catalog import (  # type: ignore[no-redef]
        resolve_current_successor_v1,
    )

INVENTORY = "frozen-test-inventory-v2.json"
LEDGER = "test-replacements-v2.json"
MEMBERS = (
    INVENTORY,
    LEDGER,
    "frozen-test-inventory-v2-recoveries.json",
    "frozen-test-inventory-v2-recovery-transition-receipts.json",
    "frozen-test-inventory-v2-doctest-recapture.json",
    "frozen-test-inventory-v2-unittest-recapture.json",
    "frozen-test-inventory-v2-unittest-source-exceptions.json",
)


V1_LEDGER = ".codex/validation/test-replacements-v1.json"
V1_INVENTORY = ".codex/validation/frozen-test-inventory-v1.json"

# Windows junctions are reparse points that `Path.is_symlink` does not report.
_is_junction = getattr(os.path, "isjunction", lambda _path: False)

# Exact member byte sets whose predecessor closure already validated in this
# process. Validation is a pure function of those bytes (the fresh issuer
# rejects every foreign applicability MAC either way), and one focused run
# reads the same generation for status, inventory, and reconciliation.
_VALIDATED_BUNDLES: set[str] = set()


def is_v2(config: Mapping[str, Any]) -> bool:
    return Path(str(config.get("frozen_inventory", ""))).name == INVENTORY


def _reject_reparse_points(root: Path, relative: str) -> None:
    """Match Core's reader: no symlink or junction on any path component."""
    candidate = Path(relative)
    parts = candidate.parts
    if candidate.is_absolute() or not parts or any(part in {".", ".."} for part in parts):
        raise ValueError("inventory generation paths must stay inside the repository")
    path = root
    for part in parts:
        path = path / part
        if path.is_symlink() or _is_junction(path):
            raise ValueError(f"inventory generation path is a reparse point: {relative}")


def read_bundle(root: Path, config: Mapping[str, Any]) -> tuple[dict, dict]:
    inventory_path = root / config["frozen_inventory"]
    ledger_path = root / config["replacement_ledger"]
    if ledger_path.parent != inventory_path.parent or ledger_path.name != LEDGER:
        raise ValueError(
            "V2 config must select inventory and ledger from one generation"
        )
    directory = inventory_path.parent
    _reject_reparse_points(root, str(config["frozen_inventory"]))
    raw = {}
    for name in MEMBERS:
        path = directory / name
        if path.is_symlink() or _is_junction(path) or not path.is_file():
            raise ValueError(f"V2 bundle member is not a regular file: {name}")
        raw[name] = path.read_bytes()
    predecessor = (root / V1_LEDGER).read_bytes()
    inventory = json.loads(raw[INVENTORY])
    ledger = json.loads(raw[LEDGER])
    digest = hashlib.sha256()
    for name in MEMBERS:
        digest.update(name.encode("utf-8"))
        digest.update(len(raw[name]).to_bytes(8, "big"))
        digest.update(raw[name])
    digest.update(len(predecessor).to_bytes(8, "big"))
    digest.update(predecessor)
    bundle_key = digest.hexdigest()
    if bundle_key not in _VALIDATED_BUNDLES:
        # A fresh verifier cannot accept an old caller's applicability MAC. Pending
        # exceptions are retained, and need current host authority before full proof.
        issuer = contracts.ActiveHostApplicabilityIssuerV1(
            str(uuid.uuid4()), os.urandom(32)
        )
        contracts.validate_inventory_ledger_predecessor_closure(
            inventory,
            ledger,
            raw["frozen-test-inventory-v2-recoveries.json"],
            json.loads(raw["frozen-test-inventory-v2-recovery-transition-receipts.json"]),
            issuer,
            raw["frozen-test-inventory-v2-doctest-recapture.json"],
            raw["frozen-test-inventory-v2-unittest-recapture.json"],
            predecessor,
        )
        _VALIDATED_BUNDLES.add(bundle_key)
    frozen = json.loads((root / V1_INVENTORY).read_bytes())
    if config["frozen_inventory_hash"] != frozen["inventory_hash"]:
        raise ValueError("V2 selection changed the immutable frozen baseline identity")
    return inventory, ledger


def status(root: Path, config: Mapping[str, Any]) -> tuple[int, int]:
    _, ledger = read_bundle(root, config)
    dispositions = [row["disposition"] for row in ledger["rows"]]
    return (
        sum(row["kind"] == "unresolved" for row in dispositions),
        sum(
            row["kind"] == "replacement" and row["contract"]["state"] != "accepted"
            for row in dispositions
        ),
    )


def reconcile(
    root: Path,
    config: Mapping[str, Any],
    current_rows: Sequence[Mapping],
    known_validation_ids: set[str] | None,
    transition_readiness: bool,
) -> dict:
    inventory, ledger = read_bundle(root, config)
    current = {row["baseline_id"]: row for row in current_rows}
    if len(current) != len(current_rows):
        raise ValueError("current inventory has duplicate IDs")
    declarations = {}
    for item in inventory["declaration_universe"]:
        obligation = (
            contracts.frozen_baseline_obligation_id_v2(item)
            if item["kind"] == "frozen-baseline"
            else item["obligation_id"]
        )
        declarations[obligation] = item
    referenced: set[str] = set()
    executable: set[str] = set()
    errors: list[str] = []
    for row in ledger["rows"]:
        obligation = row["obligation_id"]
        disposition = row["disposition"]
        kind = disposition["kind"]
        if kind == "unresolved":
            if not transition_readiness:
                errors.append(f"{obligation}: baseline resolution remains unresolved")
            continue
        if kind == "recovered-container":
            # The closure validator requires every child row. Children remain
            # independent obligations and are visited by this same loop.
            continue
        if kind == "exception":
            if not transition_readiness:
                errors.append(f"{obligation}: exception requires current host approval")
            continue
        if kind == "current":
            entry = declarations[obligation]["entry"]
            identity = entry["executable_identity"]
            ids = [identity["test_id"]]
        elif kind == "replacement":
            contract = disposition["contract"]
            if contract["state"] != "accepted" and not transition_readiness:
                errors.append(f"{obligation}: replacement remains unadmitted")
            ids = contract["legacy_replacement_hint"]["replacement_ids"]
            if row["baseline_id"] in current:
                errors.append(f"{obligation}: old baseline test is still discovered")
            if contract["state"] == "accepted":
                validation = contract["accepted"]["candidate"]["validation_id"]
                validation = {
                    "codex-rust-tests": "rust.nextest.workspace",
                    "codex-rust-doctests": "rust.doctest.workspace",
                    "windows-sandbox-smoke": "windows.sandbox-smoke",
                }.get(validation, validation)
                if (
                    known_validation_ids is not None
                    and validation not in known_validation_ids
                ):
                    errors.append(
                        f"{obligation}: unknown replacement validation {validation}"
                    )
        else:
            raise ValueError(f"unsupported V2 disposition: {kind}")
        resolved = []
        for identity in ids:
            successor = resolve_current_successor_v1(current, identity)
            if successor is None:
                errors.append(
                    f"{obligation}: replacement IDs are not discovered: {identity}"
                )
            else:
                resolved.append(successor["baseline_id"])
        if len(set(resolved)) != len(resolved):
            errors.append(f"{obligation}: duplicate resolved current identity")
        referenced.update(resolved)
        executable.update(resolved)
    if not transition_readiness:
        for identity in sorted(set(current) - referenced):
            errors.append(f"{identity}: current identity has no admitted obligation")
    if errors:
        raise ValueError(
            "V2 inventory reconciliation failed: " + "; ".join(errors[:20])
        )
    required: dict[str, list[str]] = {}
    for identity in sorted(executable):
        required.setdefault(current[identity]["framework"], []).append(
            current[identity]["native_id"]
        )
    return {
        "required_by_framework": required,
        "exceptions": [],
        "additions": [],
        "overrides": [],
        "current_ids": set(current),
    }
