#!/usr/bin/env python3
"""Narrow production-runner benchmarks with fake Cargo output, not live tests."""
from __future__ import annotations

import contextlib
import copy
import hashlib
import io
import json
import statistics
import sys
import time
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scripts import rust_test_runner as owner
from scripts.test_rust_test_runner import FakeExecutor, MANIFEST_DATA, RunnerTestCase


def main() -> None:
    fixture = RunnerTestCase()
    fixture.setUp()
    try:
        step = copy.deepcopy(MANIFEST_DATA['gates']['demo-gate']['steps'][0])
        manifest = fixture.manifest(gates={f'gate-{i}': {'description': 'overlap', 'steps': [step]}
                                           for i in range(8)})
        samples, counts = [], []
        for _ in range(7):
            runner, executor = fixture.runner(manifest=manifest)
            started = time.perf_counter()
            with contextlib.redirect_stderr(io.StringIO()):
                runner.check_gates(list(manifest.gates), include_generated=False)
            samples.append((time.perf_counter() - started) * 1000)
            counts.append(len(executor.calls))
        discovery = {'median_ms': statistics.median(samples), 'samples_ms': samples,
                     'list_invocations': counts}
        listing = {f'module::test_{i}': False for i in range(10_000)}
        executor = FakeExecutor(default_listing=listing)
        runner, _ = fixture.runner(executor=executor)
        samples, decodes = [], []
        for _ in range(7):
            with mock.patch.object(owner.json, 'loads', wraps=json.loads) as loads:
                started = time.perf_counter()
                with contextlib.redirect_stderr(io.StringIO()):
                    selected = runner._list_tests(runner.target('core_lib'), [], discovered_build={})
                samples.append((time.perf_counter() - started) * 1000)
                decodes.append(loads.call_count)
            assert selected == listing
        print(json.dumps({'scope': 'Runner orchestration and JSON CPU; fake Cargo, no test proof reuse',
                          'source_sha256': hashlib.sha256(Path(owner.__file__).read_bytes()).hexdigest(),
                          'overlapping_gates': discovery,
                          'listing_decode': {'median_ms': statistics.median(samples),
                                             'samples_ms': samples, 'json_decodes': decodes}}, indent=2))
    finally:
        fixture.doCleanups()


if __name__ == '__main__':
    main()
