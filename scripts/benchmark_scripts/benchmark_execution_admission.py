"""Measure real uncontended Windows admission/handoff and occupied-lane rejection.

Uses temporary lanes only: no Cargo, checkout copying, or validation launches.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import statistics
import sys
import tempfile
import time

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from scripts.build import rust_build_status as lanes


def measure(factory, samples, *, busy=False):
    values = []
    for _ in range(samples):
        started = time.perf_counter()
        rejected = False
        try:
            with factory():
                pass
        except lanes.RustAdmissionBusy:
            rejected = True
        if rejected != busy:
            raise AssertionError(f"expected busy={busy}, observed {rejected}")
        values.append((time.perf_counter() - started) * 1000)
    ordered = sorted(values)
    return {"samples": samples, "median_ms": statistics.median(values),
            "p95_ms": ordered[max(0, (95 * samples + 99) // 100 - 1)],
            "max_ms": max(values), "raw_ms": values}


@contextmanager
def inheritance(target):
    key = "CODEX_CARGO_LANE_TARGET_DIR"
    previous = os.environ.get(key)
    os.environ[key] = str(target)
    try:
        yield
    finally:
        if previous is None:
            os.environ.pop(key, None)
        else:
            os.environ[key] = previous


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--samples", type=int, default=50)
    parser.add_argument("--output", type=Path, help="Optional JSON artifact including raw samples")
    args = parser.parse_args(argv)
    if os.name != "nt" or args.samples < 1:
        parser.error("requires Windows and a positive sample count")
    with tempfile.TemporaryDirectory(prefix="kd4-admission-") as temporary:
        repo = Path(temporary)
        root = repo / "codex-rs" / "target" / "lanes"
        lanes.initialize_cargo_lanes_root(repo, root)
        target = root / "fixture"
        admit = lambda: lanes.reserve_rust_test_target(target)
        with admit():
            pass
        result = {"ordinary_managed": measure(admit, args.samples)}
        with admit():
            result["occupied_runner"] = measure(admit, args.samples, busy=True)
        with lanes.reserve_cargo_lane(
            repo_root=repo, lane_root=root, requested_lane="fixture", command=[],
        ):
            result["occupied_lane"] = measure(admit, args.samples, busy=True)
            with inheritance(target):
                result["inherited_handoff"] = measure(admit, args.samples)
    source = Path(lanes.__file__)
    report = {"scope": "temporary warmed Windows lanes; no Cargo or end-to-end latency claim",
              "source_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
              "results": result}
    if args.output:
        args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({**report, "results": {
        key: {name: value for name, value in row.items() if name != "raw_ms"}
        for key, row in result.items()
    }}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
