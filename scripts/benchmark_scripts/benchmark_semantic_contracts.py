"""Offline, narrow semantic counterexamples through the real oracle worker.

Measures complete subprocess requests, including startup and JSON transport;
not model latency, semantic equivalence, or repository-wide dependency recall.
Fixtures and expected relationships are authored independently of worker output.
"""

import argparse
import hashlib
import json
from pathlib import Path
import statistics
import subprocess
import sys
import time


def patched_boundary_cases():
    """Fixed mock counterexamples; expected paths follow Python lookup semantics."""
    product = "import subprocess\ndef run(): return subprocess.run(['tool'])\n"
    base = """from unittest.mock import patch
from product import run
@patch("product.subprocess.run")
def test_fake(mocked):
    run()
    mocked.assert_called_once()
"""
    candidate = """def test_candidate(mocked):
    run()
    mocked.assert_called_once()
"""
    entrypoint = base + '@patch("test_case.run")\n' + candidate
    return {
        "patched_entrypoint": {"product.py": product, "test_case.py": entrypoint},
        "patched_module_entrypoint": {"product.py": product, "test_case.py": base + '''import product as api
@patch("product.run")
def test_candidate(mocked):
    api.run()
    mocked.assert_called_once()
'''},
        "patched_intermediate": {"product.py": product + "def wrapper(): return run()\n",
            "test_case.py": base + '''from product import wrapper
@patch("product.run")
def test_candidate(mocked):
    wrapper()
    mocked.assert_called_once()
'''},
        "opaque_patch": {"product.py": product, "test_case.py": base
            + 'TARGET = "test_case.run"\n@patch(TARGET)\n' + candidate},
        # Patching the exporting module does not replace an already imported
        # function binding. This call really does enter the original body.
        "unaffected_import_binding": {"product.py": product, "test_case.py": base + '''@patch("product.run")
def test_candidate(mocked):
    run()
    mocked.assert_not_called()
'''},
        "patched_entrypoint_with_real": {"product.py": product,
            "test_case.py": entrypoint + "def test_real(): run()\n"},
    }


def cases():
    return {
        "package_import": {
            "pkg/__init__.py": "from .product import run\ndef test_package(): run()\n",
            "pkg/product.py": "def run(): return 7\n",
        },
        "shadowed_calls": {
            "product.py": "def run(): return 7\n",
            "test_case.py": """from product import run
import product
def test_direct(): run()
def test_parameter(run): run()
def test_local():
    run = lambda: None
    run()
def test_module(product): product.run()
def test_comprehension(): [run() for run in ()]
""",
        },
        "match_contract": {
            "product.py": """def run(value):
    match value:
        case 1: return {"value": 1}
        case _: return {"value": 2}
""",
            "test_case.py": """import unittest
from product import run
class Case(unittest.TestCase):
    def test_control(self): self.assertEqual(run(1)["value"], run(2)["value"])
""",
        },
        "decorated_boundary": {
            "product.py": """import subprocess
def suppress(function): return lambda: None
@suppress
def run(): return subprocess.run(["tool"])
""",
            "test_case.py": """from unittest.mock import patch
from product import run
@patch("product.subprocess.run")
def test_fake(mocked):
    run()
    mocked.assert_called_once()
def test_unpatched(): run()
""",
        },
        "ordinary_boundary": {
            "product.py": "import subprocess\ndef run(): return subprocess.run(['tool'])\n",
            "test_case.py": """from unittest.mock import patch
from product import run
@patch("product.subprocess.run")
def test_fake(mocked):
    run()
    mocked.assert_called_once()
def test_unpatched(): run()
""",
        },
        "unknown_dispatch": {"test_case.py": "def test_dynamic(subject): subject.run()\n"},
        "invalid_syntax": {"broken.py": "def broken("},
        **patched_boundary_cases(),
    }


def checks(name, report):
    result = {
        "never_claims_complete_behavior": report["analysis_complete"] is False,
        "advisory_not_proof": report["advisory_only"] is True,
        "parse_status": report["parse_complete"] is (name != "invalid_syntax"),
    }
    reasons = {row["reason"] for row in report["diagnostics"]}
    if name in patched_boundary_cases():
        needs_warning = name not in {"unaffected_import_binding", "patched_entrypoint_with_real"}
        result["patched_path_not_real_coverage"] = ("external_boundary_only_mocked" in reasons) == needs_warning
        if name != "unaffected_import_binding":
            result["replacement_path_unknown"] = any(
                row["reason"] == "boundary_reachability_unknown"
                and row.get("test_callable") == "test_case.test_candidate"
                for row in report["unknown"]
            )
    elif name == "package_import":
        result["relative_package_identity"] = report["tests"]["pkg.test_package"]["reached"] == ["pkg.product.run"]
    elif name == "shadowed_calls":
        for test in ("parameter", "local", "module", "comprehension", "direct"):
            expected = ["product.run"] if test == "direct" else []
            result[test + "_identity"] = report["tests"]["test_case.test_" + test]["reached"] == expected
        result["unknowns_retained"] = len(report["unknown"]) >= 4
    elif name == "match_contract":
        result["no_false_parameter_independence"] = "compared_value_independent_of_parameter" not in reasons
    elif name.endswith("boundary"):
        result["mocked_boundary_status"] = ("external_boundary_only_mocked" in reasons) == (name == "decorated_boundary")
        if name == "decorated_boundary":
            result["wrapper_uncertainty"] = any(row["reason"] == "boundary_reachability_unknown" for row in report["unknown"])
    elif name == "unknown_dispatch":
        result["dynamic_not_resolved"] = report["tests"]["test_case.test_dynamic"]["reached"] == []
        result["unknown_retained"] = any(row.get("target") == "subject.run" for row in report["unknown"])
    elif name == "invalid_syntax":
        result["parse_failure_retained"] = any(row["reason"] == "parse_error" for row in report["unknown"])
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not 1 <= args.runs <= 20:
        parser.error("runs must be 1..20")
    if args.output.exists():
        parser.error("output must be a new file")
    worker = (Path(__file__).resolve().parents[1] / "maintenance" / "python_test_oracles.py")
    worker_hash = hashlib.sha256(worker.read_bytes()).hexdigest()
    rows = []
    for iteration in range(args.runs):
        for name, sources in cases().items():
            request = json.dumps({"sources": sources}).encode()
            start = time.perf_counter()
            process = subprocess.run([sys.executable, "-I", "-B", str(worker)],
                                     input=request, capture_output=True, timeout=15)
            elapsed = (time.perf_counter() - start) * 1000
            if process.returncode:
                raise RuntimeError(process.stderr.decode(errors="replace"))
            report = json.loads(process.stdout)
            rows.append({"case": name, "iteration": iteration, "wall_ms": elapsed,
                         "output_bytes": len(process.stdout), "checks": checks(name, report),
                         "report": report})
    if hashlib.sha256(worker.read_bytes()).hexdigest() != worker_hash:
        raise RuntimeError("worker changed during measurement")
    summary = {
        "worker_sha256": worker_hash, "python": sys.version,
        "checks_passed": sum(sum(row["checks"].values()) for row in rows),
        "checks_total": sum(len(row["checks"]) for row in rows),
        "median_case_ms": {name: statistics.median(row["wall_ms"] for row in rows if row["case"] == name) for name in cases()},
        "median_task_ms": statistics.median(sum(row["wall_ms"] for row in rows if row["iteration"] == iteration) for iteration in range(args.runs)),
        "worker_requests_per_task": len(cases()), "model_requests": 0,
        "scope": "authored counterexamples through isolated worker subprocesses; no live-model, workspace-wide, or concurrency speedup claim",
    }
    with args.output.open("x", encoding="utf-8") as output:
        json.dump({"summary": summary, "fixtures": cases(), "observations": rows}, output, indent=2)
    print(json.dumps({**summary, "report": str(args.output.resolve())}, indent=2))
    return 0 if summary["checks_passed"] == summary["checks_total"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
