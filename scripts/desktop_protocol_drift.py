#!/usr/bin/env python3
"""Report app-server methods Codex Desktop sent that this checkout does not register.

Desktop logs every request the local app-server rejected as an unknown method
variant. Comparing those names against the generated ``ClientRequest`` schema
turns silent Desktop toasts into a list of missing protocol methods. This is a
name-level smoke check: a method that is present may still differ in params,
response, or notifications, so it complements rather than replaces contract
tests.

Unreadable logs and enum rejections without a ClientRequest method discriminator
are reported as incomplete evidence (exit 2), not a clean comparison.

The default schema is the stable one, so methods registered as experimental are
reported missing. To compare against the experimental surface Desktop uses, run
``codex app-server generate-json-schema --experimental --out DIR`` with the
binary under test and pass ``--schema DIR/ClientRequest.json``.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time
from collections.abc import Iterable
from dataclasses import dataclass
from dataclasses import field
from pathlib import Path

UNKNOWN_VARIANT = re.compile(r"unknown variant `([^`]+)`")
EXPECTED_VARIANTS = re.compile(r", expected (?:one of )?((?:`[^`]+`(?:, | or )?)+)")
DEFAULT_SCHEMA = Path("codex-rs/app-server-protocol/schema/json/ClientRequest.json")


@dataclass
class RejectedMethod:
    method: str
    count: int = 0
    logs: set[str] = field(default_factory=set)


@dataclass
class RejectionScan:
    rejected: dict[str, RejectedMethod] = field(default_factory=dict)
    unclassified: dict[str, RejectedMethod] = field(default_factory=dict)
    errors: list[str] = field(default_factory=list)


def default_logs_dir() -> Path | None:
    """Prefer the plain Desktop log path; fall back to the MSIX-virtualized one.

    Inside the packaged app, ``%LOCALAPPDATA%\\Codex\\Logs`` is a virtualized
    view. From an ordinary process the same files live under the package's
    ``LocalCache`` directory, so both locations are probed.
    """
    local_app_data = os.environ.get("LOCALAPPDATA")
    if not local_app_data:
        return None
    plain = Path(local_app_data) / "Codex" / "Logs"
    if plain.is_dir():
        return plain
    packages = Path(local_app_data) / "Packages"
    if packages.is_dir():
        for package in sorted(packages.glob("OpenAI.Codex_*")):
            virtualized = package / "LocalCache" / "Local" / "Codex" / "Logs"
            if virtualized.is_dir():
                return virtualized
    return plain


def schema_methods(schema: dict) -> set[str]:
    """Collect every ``method`` enum value declared by the ClientRequest schema."""
    methods: set[str] = set()

    def visit(node: object) -> None:
        if isinstance(node, dict):
            method = node.get("method")
            if isinstance(method, dict):
                for value in method.get("enum", []):
                    if isinstance(value, str):
                        methods.add(value)
            for value in node.values():
                visit(value)
        elif isinstance(node, list):
            for value in node:
                visit(value)

    visit(schema)
    return methods


def rejected_methods(log_paths: Iterable[Path]) -> RejectionScan:
    scan = RejectionScan()
    for log_path in log_paths:
        try:
            text = log_path.read_text(encoding="utf-8", errors="replace")
        except OSError as error:
            scan.errors.append(f"{log_path}: {error}")
            continue
        for match in UNKNOWN_VARIANT.finditer(text):
            method = match.group(1)
            expected = EXPECTED_VARIANTS.match(text, match.end())
            # Both nested parameter enums and the method discriminator produce
            # "unknown variant". Only ClientRequest's variants include initialize.
            methods = (
                scan.rejected
                if expected and "`initialize`" in expected.group(1)
                else scan.unclassified
            )
            entry = methods.setdefault(method, RejectedMethod(method))
            entry.count += 1
            entry.logs.add(str(log_path))
    return scan


def recent_log_files(logs_dir: Path, max_age_days: float) -> list[Path]:
    cutoff = time.time() - max_age_days * 86_400
    files = []
    for path in logs_dir.rglob("*.log"):
        if path.stat().st_mtime >= cutoff:
            files.append(path)
    return sorted(files)


def drift_report(
    rejected: dict[str, RejectedMethod], registered: set[str]
) -> tuple[list[RejectedMethod], list[RejectedMethod]]:
    """Split rejected methods into those still missing and those now registered."""
    missing = sorted(
        (entry for entry in rejected.values() if entry.method not in registered),
        key=lambda entry: entry.method,
    )
    present = sorted(
        (entry for entry in rejected.values() if entry.method in registered),
        key=lambda entry: entry.method,
    )
    return missing, present


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--logs",
        type=Path,
        default=default_logs_dir(),
        help="Desktop log directory (default: %%LOCALAPPDATA%%\\Codex\\Logs)",
    )
    parser.add_argument(
        "--schema",
        type=Path,
        default=DEFAULT_SCHEMA,
        help="ClientRequest JSON schema to compare against (default: stable only)",
    )
    parser.add_argument(
        "--days",
        type=float,
        default=7.0,
        help="Only scan logs modified in the last N days",
    )
    parser.add_argument("--json", action="store_true", help="Emit a JSON report")
    args = parser.parse_args(argv)

    if args.logs is None or not args.logs.is_dir():
        print(f"log directory not found: {args.logs}", file=sys.stderr)
        return 2
    if not args.schema.is_file():
        print(f"schema not found: {args.schema}", file=sys.stderr)
        return 2

    try:
        log_files = recent_log_files(args.logs, args.days)
    except OSError as error:
        print(f"could not enumerate Desktop logs: {error}", file=sys.stderr)
        return 2
    if not log_files:
        # No scanned logs is missing evidence, not an absence of drift.
        print(
            f"no Desktop logs modified in the last {args.days:g} day(s) under {args.logs}",
            file=sys.stderr,
        )
        return 2
    registered = schema_methods(json.loads(args.schema.read_text(encoding="utf-8")))
    scan = rejected_methods(log_files)
    missing, present = drift_report(scan.rejected, registered)
    unclassified = sorted(scan.unclassified.values(), key=lambda entry: entry.method)

    if args.json:
        print(
            json.dumps(
                {
                    "missing": [
                        {"method": m.method, "count": m.count, "logs": sorted(m.logs)}
                        for m in missing
                    ],
                    "registered_since": [
                        {"method": m.method, "count": m.count} for m in present
                    ],
                    "unclassified_rejections": [
                        {"variant": m.method, "count": m.count, "logs": sorted(m.logs)}
                        for m in unclassified
                    ],
                    "scan_errors": scan.errors,
                },
                indent=2,
            )
        )
    else:
        if not scan.rejected and not unclassified and not scan.errors:
            print(
                f"no rejected methods found in {len(log_files)} recent Desktop log(s)"
            )
        for entry in missing:
            print(
                f"MISSING  {entry.method}  (rejected {entry.count}x in {len(entry.logs)} log(s))"
            )
        for entry in present:
            # The schema proves registration here, not that the runtime that
            # rejected the method has since been updated.
            print(
                f"present  {entry.method}  (rejected {entry.count}x; registered in schema)"
            )
        for entry in unclassified:
            print(
                f"unclassified  {entry.method}  (rejected {entry.count}x; "
                "not proven to be a method rejection)"
            )
        for error in scan.errors:
            print(f"could not read Desktop log: {error}", file=sys.stderr)
    if scan.errors or unclassified:
        return 2
    return 1 if missing else 0


if __name__ == "__main__":
    raise SystemExit(main())
