"""Isolated native nextest receipt probe; never builds the repository workspace.

Run with ``python -B -m scripts.benchmark_scripts.benchmark_rust_receipt_boundaries --experimental-json``.
Use ``--end-to-end`` for production run_gates receipt validation; ``--case`` narrows it.
``--prepared --workdir PATH`` reuses only an explicitly prepared fixture binary,
not previous test outcomes. Every command owns and reaps its subprocess tree.
"""

from __future__ import annotations

# Support direct execution (including python -I) from any working directory.
if __package__ in (None, ""):
    import sys as _sys
    from pathlib import Path as _Path

    _sys.path.insert(0, str(_Path(__file__).resolve().parents[2]))


import argparse
from contextlib import redirect_stderr
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time

from scripts.common import process_owner
from scripts.build import rust_test_runner
from scripts.build.validation_metrics import ValidationMetrics


SOURCE = r'''
#[test]
fn fixture_a_plain() { eprintln!("native first test"); }

#[test]
fn fixture_b_payload() {
    println!("PASS [0.001s] receipt-fixture phantom");
    eprintln!("    PASS [0.001s] receipt-fixture phantom_indented");
    println!("────────────\nPASS [0.001s] receipt-fixture phantom_rule");
    println!("diagnostic\r────────────\nPASS [0.001s] receipt-fixture phantom_cr");
    println!("diagnostic\u{2028}────────────\nPASS [0.001s] receipt-fixture phantom_unicode");
    eprintln!("Summary [0.000s] 0 tests run: 0 passed");
}

#[test]
fn fixture_c_plain() { println!("native final test"); }
'''
EXPECTED = ["fixture_a_plain", "fixture_b_payload", "fixture_c_plain"]
PHANTOMS = ["phantom", "phantom_indented", "phantom_rule", "phantom_cr", "phantom_unicode"]
FAILURE_SOURCE = r'''
#[test]
fn fixture_d_large_failure() {
    println!("{}", "x".repeat(1_100_000));
    panic!("native deliberate failure");
}
'''


def artifact(path):
    data = path.read_bytes()
    return {"path": str(path), "bytes": len(data),
            "sha256": hashlib.sha256(data).hexdigest()}


def event_summary(path):
    """Project real retained events without copying large payloads into reports."""
    events = []
    for line in path.read_bytes().split(b"\n"):
        if not line:
            continue
        event = json.loads(line)
        events.append({**{key: value for key, value in event.items()
                         if key not in {"stdout", "stderr"}},
                       "keys": list(event), "line_bytes": len(line),
                       "captured_bytes": {key: len(event[key].encode("utf-8"))
                                          for key in ("stdout", "stderr") if key in event}})
    return events


def expected_records(case):
    """Exact status, binary and test identities justified by the fixture source."""
    if case == "bin":
        return [("PASS", "receipt-fixture::bin/receipt_fixture", EXPECTED[0])]
    names = EXPECTED[:2] if case == "defective" else [] if case == "large" else EXPECTED
    records = [("PASS", "receipt-fixture", name) for name in names]
    if case in {"large", "partial"}:
        records.append(("FAIL", "receipt-fixture::large", "fixture_d_large_failure"))
    return records


class PrebuiltRunner(rust_test_runner.RustTestRunner):
    """Only selection changes: receipts, executor and checking stay production."""

    def _selection_command(self, verb, target, *batch):
        if batch:
            raise RuntimeError("fixture adapter requires separate prebuilt selections")
        return ["cargo", "nextest", verb, "--binaries-metadata",
                str(self.prebuilt[target.name]), "--cargo-metadata", str(self.cargo_metadata)]


def end_to_end(root, output, env, checked, cases):
    """Independent expectations come from authored rustc fixture bodies, not parsing."""
    large_source = output / "large.rs"
    large_source.write_text(FAILURE_SOURCE, encoding="utf-8")
    # Real Cargo metadata describes each target; Cargo never builds any of them.
    (output / "Cargo.toml").write_text(
        '[package]\nname="receipt-fixture"\nversion="0.0.0"\nedition="2021"\n'
        + '[lib]\npath=' + json.dumps(str(root / "fixture.rs")) + '\n'
        + '[[bin]]\nname="receipt_fixture"\npath=' + json.dumps(str(root / "fixture.rs")) + '\n'
        + '[[test]]\nname="large"\npath="large.rs"\n', encoding="utf-8")
    metadata_raw = json.loads(checked("e2e-metadata", ["cargo", "metadata", "--format-version",
        "1", "--no-deps", "--offline", "--manifest-path", str(output / "Cargo.toml")]))
    cargo_metadata = output / "cargo-metadata.json"
    cargo_metadata.write_text(json.dumps(metadata_raw), encoding="utf-8")
    template = json.loads((root / "binaries-metadata.json").read_text(encoding="utf-8"))
    lib_binary = dict(template["rust-binaries"]["receipt-fixture"])
    lib_binary["package-id"] = metadata_raw["packages"][0]["id"]
    paths = {}
    for name, kind, binary_name, binary_id in (
        ("normal", "lib", "receipt_fixture", "receipt-fixture"),
        ("large", "test", "large", "receipt-fixture::large"),
        ("bin", "bin", "receipt_fixture", "receipt-fixture::bin/receipt_fixture"),
    ):
        binary = {**lib_binary, "kind": kind, "binary-name": binary_name, "binary-id": binary_id}
        if name == "large":
            executable = output / ("large.exe" if os.name == "nt" else "large")
            binary["binary-path"] = str(executable)
            if "large" in cases or "partial" in cases:
                checked("e2e-compile-large", ["rustc", "--test", "--edition=2021",
                        "--crate-name", "receipt_failure", str(large_source), "-o", str(executable)])
        paths[name] = output / f"{name}-binaries.json"
        paths[name].write_text(json.dumps({**template, "rust-binaries": {binary_id: binary}}),
                               encoding="utf-8")
    manifest = rust_test_runner.Manifest.from_data({"version": 1, "helpers": {},
        "targets": {"normal": {"package": "receipt-fixture", "lib": True, "helpers": []},
                    "large": {"package": "receipt-fixture", "test": "large", "helpers": []},
                    "bin": {"package": "receipt-fixture", "bin": "receipt_fixture", "helpers": []}},
        "gates": {name: {"steps": [{"target": target, "tests": tests}]}
                  for name, target, tests in (("normal", "normal", EXPECTED),
                    ("defective", "normal", EXPECTED[:2] + PHANTOMS),
                    ("large", "large", ["fixture_d_large_failure"]),
                    ("bin", "bin", [EXPECTED[0]]))}})
    metadata = rust_test_runner.MetadataIndex.from_json(metadata_raw)
    metadata.validate_manifest(manifest)
    rows = []
    for case in cases:
        gates = ["large", "normal"] if case == "partial" else [case]
        runner = PrebuiltRunner(manifest, metadata, cwd=output, env=env,
            target_dir=root / "target", command_timeout_seconds=60, success_output="immediate")
        runner.prebuilt, runner.cargo_metadata = paths, cargo_metadata
        logs = output / (case + "-logs")
        logs.mkdir()
        runner.base_env.update({"CODEX_RUST_TEST_LOG_DIR": str(logs),
                               "NEXTEST_TEST_THREADS": "1"})
        runner.metrics = ValidationMetrics(["isolated-native-benchmark", case])
        row = {"case": case, "gates": gates}
        start = time.perf_counter()
        with (logs / "runner.stderr").open("w", encoding="utf-8") as diagnostics, redirect_stderr(diagnostics):
            try:
                receipts = runner.run_gates(gates, quiet=True)
                row["outcome"] = "passed"
            except rust_test_runner.RunnerError as error:
                row.update(outcome=error.outcome, error=str(error))
                receipts = error.completed_gates
        row["run_gates_seconds"] = time.perf_counter() - start
        row["completed_gates"] = dict(receipts or {})
        row["completed_tests"] = receipts.completed_tests() if receipts is not None else {}
        row["commands"] = runner.metrics.record["commands"]
        row["fixture_binary"] = artifact(Path(lib_binary["binary-path"]))
        row["artifacts"] = [artifact(path) for path in sorted(logs.glob("*"))]
        row["streams"] = []
        for path in sorted(logs.glob("rust-test-stdout-*")):
            result = subprocess.CompletedProcess([], 0, "", "")
            result.stdout_path = path
            row["streams"].append({"stdout": str(path), "events": event_summary(path),
                                   "current_records": list(rust_test_runner._nextest_results(result))})
        if case == "defective":
            raw = subprocess.CompletedProcess([], 0,
                b"".join(path.read_bytes() for path in logs.glob("rust-test-stdout-*")),
                b"".join(path.read_bytes() for path in logs.glob("rust-test-stderr-*")))
            baseline = frozen_baseline(raw)
            selected = EXPECTED[:2] + PHANTOMS
            row["frozen_baseline_records_same_bytes"] = baseline
            row["frozen_baseline_would_pass_gate"] = (
                sorted(baseline) == sorted(["PASS", "receipt-fixture", test] for test in selected))
            row["expectation_met"] = (row["outcome"] != "passed"
                and row["completed_tests"] == {"receipt-fixture": EXPECTED[:2]}
                and row["frozen_baseline_would_pass_gate"])
        elif case in {"normal", "bin"}:
            row["expectation_met"] = row["outcome"] == "passed" and row["completed_gates"] == {
                case: EXPECTED if case == "normal" else [EXPECTED[0]]}
        else:
            row["expectation_met"] = row["outcome"] != "passed" and row["completed_gates"] == (
                {"normal": EXPECTED} if case == "partial" else {})
            row["failed_identity_retained"] = any(
                record == ("FAIL", "receipt-fixture::large", "fixture_d_large_failure")
                for stream in row["streams"] for record in stream["current_records"])
        row["expected_records"] = expected_records(case)
        row["exact_records"] = sorted(
            record for stream in row["streams"] for record in stream["current_records"]
        ) == sorted(row["expected_records"])
        row["expectation_met"] &= row["exact_records"]
        rows.append(row)
    return rows


def frozen_baseline(result):
    """Pre-fix human reporter parser, frozen only for this benchmark comparison."""
    records = []
    for stream in (result.stdout, result.stderr):
        for line in stream.decode("utf-8", errors="replace").splitlines():
            if re.match(r"\s*Summary\s+\[[^]\r\n]+\]\s+\d+ tests? run:", line):
                break
            match = re.fullmatch(
                r"\s*(PASS|LEAK|FAIL|LEAK-FAIL|TIMEOUT|EXECFAIL)\s+\[[^]\r\n]+\]\s+"
                r"(?:\(\d+/\d+\)\s+)?(\S+)\s+(\S+)\s*", line,
            )
            if match:
                records.append(list(match.groups()))
    return records


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workdir", type=Path)
    parser.add_argument("--prepared", action="store_true")
    parser.add_argument("--format", choices=("human", "libtest-json-plus"),
                        action="append", dest="formats")
    parser.add_argument("--experimental-json", action="store_true")
    parser.add_argument("--message-format-version")
    parser.add_argument("--end-to-end", action="store_true")
    parser.add_argument("--case", choices=("normal", "defective", "large", "partial", "bin"),
                        action="append", dest="cases")
    args = parser.parse_args(argv)
    if args.prepared and args.workdir is None:
        parser.error("--prepared requires --workdir")
    root = (args.workdir or Path(tempfile.mkdtemp(prefix="rust-receipt-boundary-"))).resolve()
    root.mkdir(parents=True, exist_ok=True)
    output = Path(tempfile.mkdtemp(prefix="run-", dir=root))
    report = {"scope": "isolated rustc fixture; not repository Rust validation",
              "workdir": str(root), "output": str(output), "expected_tests": EXPECTED,
              "commands": [], "runs": [], "production_runs": [],
              "source": artifact(Path(__file__)),
              "runner_source": artifact(Path(rust_test_runner.__file__))}
    env = os.environ.copy()
    for key in list(env):
        if key.startswith("NEXTEST_"):
            env.pop(key)
    env["CARGO_TARGET_DIR"] = str(root / "target")
    if args.experimental_json:
        env["NEXTEST_EXPERIMENTAL_LIBTEST_JSON"] = "1"

    def run(label, command):
        start = time.perf_counter()
        result = process_owner.run_owned(command, cwd=root, env=env,
                                         capture_output=True, timeout=60)
        stdout = output / f"{label}.stdout"
        stderr = output / f"{label}.stderr"
        stdout.write_bytes(result.stdout)
        stderr.write_bytes(result.stderr)
        report["commands"].append({"label": label, "argv": command,
                                   "elapsed_seconds": time.perf_counter() - start,
                                   "exit_code": result.returncode,
                                   "stdout": str(stdout), "stderr": str(stderr)})
        return result

    def checked(label, command):
        result = run(label, command)
        if result.returncode:
            raise RuntimeError(f"{label} failed: {result.stderr.decode('utf-8', errors='replace')}")
        return result.stdout.decode("utf-8").strip()

    try:
        if not args.prepared:
            (root / "fixture.rs").write_text(SOURCE, encoding="utf-8")
            (root / "Cargo.toml").write_text(
                '[package]\nname="receipt-fixture"\nversion="0.0.0"\nedition="2021"\n'
                '[lib]\npath="fixture.rs"\n', encoding="utf-8")
            target = root / "target" / "debug"
            target.mkdir(parents=True, exist_ok=True)
            binary = target / ("receipt_fixture.exe" if os.name == "nt" else "receipt_fixture")
            checked("rustc-version", ["rustc", "-vV"])
            checked("nextest-version", ["cargo-nextest", "nextest", "--version"])
            host = checked("host", ["rustc", "--print", "host-tuple"])
            libdir = checked("libdir", ["rustc", "--print", "target-libdir"])
            checked("compile", ["rustc", "--test", "--edition=2021", "--crate-name",
                                "receipt_fixture", str(root / "fixture.rs"), "-o", str(binary)])
            metadata = checked("metadata", ["cargo", "metadata", "--format-version", "1",
                                              "--no-deps", "--offline"])
            (root / "cargo-metadata.json").write_text(metadata, encoding="utf-8")
            package_id = json.loads(metadata)["packages"][0]["id"]
            platform = {"triple": host, "target-features": "unknown"}
            build = {"rust-build-meta": {
                "target-directory": str(root / "target"),
                "build-directory": str(root / "target"),
                "base-output-directories": ["debug"], "non-test-binaries": {},
                "build-script-out-dirs": {}, "build-script-info": {}, "linked-paths": [],
                "platforms": {"host": {"platform": platform,
                    "libdir": {"status": "available", "path": libdir}}, "targets": []},
                "target-platforms": [platform], "target-platform": None,
            }, "rust-binaries": {"receipt-fixture": {
                "binary-id": "receipt-fixture", "binary-name": "receipt_fixture",
                "package-id": package_id, "kind": "lib", "binary-path": str(binary),
                "build-platform": "target",
            }}}
            (root / "binaries-metadata.json").write_text(json.dumps(build), encoding="utf-8")
        if args.end_to_end:
            report["production_runs"] = end_to_end(root, output, env, checked,
                args.cases or ["normal", "defective", "large", "partial", "bin"])
        for fmt in args.formats or ([] if args.end_to_end else ["libtest-json-plus"]):
            start = time.perf_counter()
            result = run(fmt, ["cargo-nextest", "nextest", "run", "--binaries-metadata",
                str(root / "binaries-metadata.json"), "--cargo-metadata",
                str(root / "cargo-metadata.json"), "-E", "test(fixture_)", "--retries", "0",
                "--no-fail-fast", "--test-threads", "1", "--success-output", "immediate",
                "--failure-output", "immediate", "--status-level", "pass",
                "--final-status-level", "none", "--show-progress", "none", "--color", "never",
                "--user-config-file", "none", "--no-tests", "fail", "--message-format", fmt,
                *(["--message-format-version", args.message_format_version]
                  if args.message_format_version else [])])
            parsed = subprocess.CompletedProcess(result.args, result.returncode, "", "")
            parsed.stdout_path = output / f"{fmt}.stdout"
            parsed.stderr_path = output / f"{fmt}.stderr"
            current = list(rust_test_runner._nextest_results(parsed))
            entry = {"format": fmt, "exit_code": result.returncode,
                     "child_and_receipt_seconds": time.perf_counter() - start,
                     "current_records": current, "baseline_records": frozen_baseline(result),
                     "current_exact": sorted(row[2] for row in current) == EXPECTED}
            if fmt != "human":
                entry["json_events"] = [json.loads(line) for line in result.stdout.split(b"\n") if line]
            report["runs"].append(entry)
    except Exception as error:
        report["error"] = f"{type(error).__name__}: {error}"
    (output / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps({"report": str(output / "report.json"), "error": report.get("error"),
        "production_runs": [{key: row[key] for key in (
            "case", "outcome", "run_gates_seconds", "completed_gates", "completed_tests", "expectation_met")}
            for row in report["production_runs"]], "runs": report["runs"]}, indent=2))
    return int("error" in report or any(
        run["exit_code"] or (run["format"] != "human" and not run["current_exact"])
        for run in report["runs"]
    ) or any(not row["expectation_met"] for row in report["production_runs"]))


if __name__ == "__main__":
    raise SystemExit(main())
