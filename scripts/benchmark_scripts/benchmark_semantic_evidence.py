"""Opt-in semantic advisory worker benchmark, not a live-model speedup claim.

Uses the regression tests' authored corpus and measures complete isolated worker
startup, input transport, parsing, analysis, output transport and teardown.
Never executes the analyzed source. Writes only a caller-chosen new report.
"""

# Support direct execution (including python -I) from any working directory.
if __package__ in (None, ""):
    import sys as _sys
    from pathlib import Path as _Path

    _sys.path.insert(0, str(_Path(__file__).resolve().parents[2]))

import argparse
import hashlib
import json
from pathlib import Path
import statistics
import subprocess
import sys
import time

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from scripts.maintenance.test_python_test_oracles import semantic_cases, semantic_case_checks


def benchmark(iterations):
    worker = (Path(__file__).resolve().parents[1] / "maintenance" / "python_test_oracles.py")
    worker_hash = hashlib.sha256(worker.read_bytes()).hexdigest()
    observations = []
    for case in semantic_cases():
        payload = json.dumps({"sources": case["sources"]}).encode()
        for iteration in range(iterations):
            start = time.perf_counter()
            result = subprocess.run([sys.executable, "-I", "-B", str(worker)], input=payload,
                                    capture_output=True, timeout=30)
            wall_ms = (time.perf_counter() - start) * 1000
            if result.returncode:
                raise RuntimeError(result.stderr.decode(errors="replace"))
            report = json.loads(result.stdout)
            observations.append({"case": case["name"], "iteration": iteration, "wall_ms": wall_ms,
                                 "checks": semantic_case_checks(case, report),
                                 "input_bytes": len(payload), "output_bytes": len(result.stdout),
                                 "report": report})
    if hashlib.sha256(worker.read_bytes()).hexdigest() != worker_hash:
        raise RuntimeError("analyzer changed during benchmark")
    return {"worker_sha256": worker_hash, "python": sys.version, "observations": observations,
            "corpus": semantic_cases(), "model_requests": 0, "worker_calls": len(observations),
            "limits": "Fixed Python advisory corpus only; no live-model TTFT, repair, concurrency, or repository-wide impact benchmark."}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--iterations", type=int, default=3)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not 1 <= args.iterations <= 20:
        parser.error("iterations must be 1..20")
    if args.output.exists():
        parser.error("output must be new")
    report = benchmark(args.iterations)
    data = json.dumps(report, sort_keys=True, indent=2).encode()
    with args.output.open("xb") as target:
        target.write(data)
    rows = report["observations"]
    passed = sum(all(row["checks"].values()) for row in rows)
    print(json.dumps({"report": str(args.output.resolve()), "sha256": hashlib.sha256(data).hexdigest(),
                      "passed_cases": passed, "total_cases": len(rows),
                      "median_worker_ms": statistics.median(row["wall_ms"] for row in rows),
                      "total_worker_ms": sum(row["wall_ms"] for row in rows),
                      "failed_cases": sorted({row["case"] for row in rows if not all(row["checks"].values())}),
                      "model_requests": 0, "worker_sha256": report["worker_sha256"]}, indent=2))
    return 0 if passed == len(rows) else 1


if __name__ == "__main__":
    raise SystemExit(main())
