#!/usr/bin/env python3
"""GitHub Windows evidence adapter; Rust selection/execution stays in the runner."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import time
import tomllib
import traceback
import urllib.request
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from scripts.common.process_owner import run_owned

ROOT = Path(__file__).resolve().parents[2]
RUST = ROOT / "codex-rs"
GATES = ("windows-process", "capability-preferred-powershell-host",
         "validation-command-recognition")
NEXTEST_VERSION = "0.9.149"
NEXTEST_URL = (
    "https://github.com/nextest-rs/nextest/releases/download/"
    f"cargo-nextest-{NEXTEST_VERSION}/cargo-nextest-{NEXTEST_VERSION}-x86_64-pc-windows-msvc.zip"
)
NEXTEST_SHA256 = "bf904b85c7c5c2a29c340053a8d9d8e8027ff0d55a2bf7cef7d23032f8166438"
MAX_FILE_BYTES = 64 * 1024 * 1024
MAX_TOTAL_BYTES = 256 * 1024 * 1024


def now():
    return datetime.now(timezone.utc).isoformat()


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(path)


def file_hash(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def json_lines(path):
    with path.open(encoding="utf-8", errors="replace") as stream:
        for line in stream:
            try:
                value = json.loads(line)
            except ValueError:
                continue
            if isinstance(value, dict):
                yield value


class Evidence:
    def __init__(self):
        self.directory = Path(os.environ["KD4_CI_EVIDENCE"]).resolve()
        self.target = Path(os.environ["KD4_CI_TARGET"]).resolve()
        self.path = self.directory / "summary.json"
        self.directory.mkdir(parents=True, exist_ok=True)
        self.data = json.loads(self.path.read_text(encoding="utf-8")) if self.path.exists() else {
            "schema": "kd4_windows_validation_v1", "started_at": now(),
            "finished_at": None, "status": "incomplete", "operations": [],
            "limitations": ["Focused compilation and tests do not prove a performance improvement.",
                            "No CLI/core, Code Mode host/V8, live model, or elevated sandbox setup coverage.",
                            "Test-binary hashes are post-execution observations, not compiler-read attestations."],
        }

    def save(self):
        write_json(self.path, self.data)

    def run(self, name, argv, *, cwd=ROOT, timeout=120):
        row = {"name": name, "argv": list(map(str, argv)), "cwd": str(cwd),
               "started_at": now(), "ended_at": None, "elapsed_seconds": None,
               "exit_code": None, "status": "incomplete"}
        self.data["operations"].append(row)
        self.save()
        started = time.monotonic()
        print(f"{name}: {subprocess.list2cmdline(row['argv'])}", flush=True)
        try:
            for channel in ("stdout", "stderr"):
                row[channel] = f"operations/{len(self.data['operations']):02d}-{name}.{channel}.log"
            stdout = self.directory / row["stdout"]
            stderr = self.directory / row["stderr"]
            stdout.parent.mkdir(parents=True, exist_ok=True)
            with stdout.open("wb") as out, stderr.open("wb") as err:
                result = run_owned(row["argv"], cwd=cwd, timeout=timeout, stdout=out, stderr=err)
            row["exit_code"] = result.returncode
            row["status"] = "passed" if result.returncode == 0 else "failed"
            if result.returncode:
                # Retain complete streams; print bounded diagnostics into Actions too.
                for path in (stdout, stderr):
                    with path.open("rb") as stream:
                        stream.seek(max(0, path.stat().st_size - 12000))
                        print(stream.read().decode("utf-8", errors="replace"), file=sys.stderr)
                raise RuntimeError(f"{name} exited {result.returncode}; see {stdout} and {stderr}")
            return stdout.read_text(encoding="utf-8", errors="replace") if stdout.stat().st_size < MAX_FILE_BYTES else ""
        except BaseException as error:
            row["error"] = str(error)
            row["status"] = "failed"
            raise
        finally:
            row["ended_at"] = now()
            row["elapsed_seconds"] = round(time.monotonic() - started, 6)
            self.save()

    def initialize(self):
        if os.name != "nt" or sys.version_info < (3, 11):
            raise RuntimeError("Windows and Python >=3.11 are required")
        self.data["github"] = {key: os.environ.get(key) for key in (
            "GITHUB_REPOSITORY", "GITHUB_SHA", "GITHUB_REF", "GITHUB_EVENT_NAME",
            "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_WORKFLOW_REF", "GITHUB_WORKFLOW_SHA")}
        self.data["environment"] = {
            "platform": platform.platform(), "python": sys.version, "python_executable": sys.executable,
            "cpu_count": os.cpu_count(), "target_directory": str(self.target),
            "variables": {key: os.environ.get(key) for key in (
                "ImageOS", "ImageVersion", "RUNNER_OS", "RUNNER_ARCH", "RUNNER_ENVIRONMENT",
                "CARGO_BUILD_JOBS", "CARGO_INCREMENTAL", "NEXTEST_TEST_THREADS", "RUST_TEST_THREADS",
                "RUST_MIN_STACK", "RUST_BACKTRACE", "CODEX_REQUIRE_WINDOWS_SANDBOX_PROCESS_TESTS")},
        }
        self.save()
        sha = self.run("source-sha", ["git", "rev-parse", "HEAD"]).strip()
        self.data["source"] = {"tested_sha": sha, "expected_sha": os.environ.get("KD4_EXPECTED_SHA"),
                               "pr_head_sha": os.environ.get("KD4_HEAD_SHA"),
                               "pr_base_sha": os.environ.get("KD4_BASE_SHA"),
                               "commit": self.run("source-commit", ["git", "show", "-s", "--format=fuller", "HEAD"]),
                               "lock_sha256": file_hash(RUST / "Cargo.lock")}
        if sha != os.environ["KD4_EXPECTED_SHA"]:
            raise RuntimeError("Checked-out revision differs from the event SHA")
        self.check_source("initial-source-status")
        self.run("windows-environment", ["pwsh", "-NoProfile", "-Command",
            "$ErrorActionPreference='Stop'; Get-CimInstance Win32_OperatingSystem | "
            "Select-Object Caption,Version,BuildNumber,TotalVisibleMemorySize,FreePhysicalMemory | "
            "ConvertTo-Json; Get-PSDrive -PSProvider FileSystem | "
            "Select-Object Name,Used,Free | ConvertTo-Json; $PSVersionTable | ConvertTo-Json"])
        self.run("git-version", ["git", "--version"])
        manifest = tomllib.loads((RUST / ".config/kd4-rust-tests.toml").read_text(encoding="utf-8"))
        expected = set()
        for gate in GATES:
            for step in manifest["gates"][gate]["steps"]:
                target = manifest["targets"][step["target"]]
                binary = target["package"] + (f"::bin/{target['bin']}" if "bin" in target else "")
                expected.update((binary, test) for test in step["tests"])
        self.data["selection"] = {"gates": list(GATES), "expected_tests": [
            {"binary": binary, "test": test} for binary, test in sorted(expected)]}
        self.save()

    def check_source(self, name):
        status = self.run(name, ["git", "status", "--porcelain", "--untracked-files=normal"])
        if status.strip():
            raise RuntimeError(f"Source tree changed: {status}")
        if file_hash(RUST / "Cargo.lock") != self.data["source"]["lock_sha256"]:
            raise RuntimeError("Cargo.lock changed during validation")

    def setup(self):
        toolchain = tomllib.loads((RUST / "rust-toolchain.toml").read_text(encoding="utf-8"))["toolchain"]
        self.run("rustup-install", ["rustup", "toolchain", "install", toolchain["channel"],
                 "--profile", "minimal", "--component", ",".join(toolchain["components"])], timeout=600)
        # No installer scripts and no cargo-install compilation. Verify a pinned release archive.
        started = time.monotonic()
        row = {"name": "nextest-download", "url": NEXTEST_URL, "started_at": now(),
               "expected_sha256": NEXTEST_SHA256, "status": "incomplete", "exit_code": None}
        self.data["operations"].append(row)
        self.save()
        try:
            tools = Path(os.environ["RUNNER_TEMP"]) / "kd4-ci-tools"
            tools.mkdir(exist_ok=True)
            archive = tools / "nextest.zip"
            with urllib.request.urlopen(NEXTEST_URL, timeout=60) as response, archive.open("wb") as output:
                shutil.copyfileobj(response, output)
            row["sha256"] = file_hash(archive)
            if row["sha256"] != NEXTEST_SHA256:
                raise RuntimeError("nextest archive SHA-256 mismatch")
            with zipfile.ZipFile(archive) as bundle:
                members = [entry for entry in bundle.infolist() if Path(entry.filename).name == "cargo-nextest.exe"]
                if len(members) != 1:
                    raise RuntimeError("nextest archive must contain exactly one cargo-nextest.exe")
                with bundle.open(members[0]) as source, (tools / "cargo-nextest.exe").open("wb") as destination:
                    shutil.copyfileobj(source, destination)
            os.environ["PATH"] = str(tools) + os.pathsep + os.environ["PATH"]
            with open(os.environ["GITHUB_PATH"], "a", encoding="utf-8") as path_file:
                path_file.write(str(tools) + "\n")
            row["status"], row["exit_code"] = "passed", 0
        finally:
            row["ended_at"] = now()
            row["elapsed_seconds"] = round(time.monotonic() - started, 6)
            self.save()
        self.data["tools"] = {
            "rustc": self.run("rustc-version", ["rustc", "-Vv"], cwd=RUST),
            "cargo": self.run("cargo-version", ["cargo", "-Vv"], cwd=RUST),
            "nextest": self.run("nextest-version", ["cargo", "nextest", "--version"], cwd=RUST),
            "nextest_executable_sha256": file_hash(tools / "cargo-nextest.exe"),
            "toolchain": toolchain,
        }
        vswhere = Path(os.environ["ProgramFiles(x86)"]) / "Microsoft Visual Studio/Installer/vswhere.exe"
        installations = self.run("msvc-environment", [str(vswhere), "-latest", "-products", "*",
            "-requires", "Microsoft.VisualStudio.Component.VC.Tools.x86.x64", "-format", "json"])
        if not json.loads(installations):
            raise RuntimeError("Visual C++ x64 tools are not installed")
        self.save()

    def build(self):
        self.run("build-sandbox-helpers", ["cargo", "build", "--locked", "--profile", "dev-small",
                 "--target-dir", str(self.target), "-p", "codex-windows-sandbox", "--bins",
                 "--message-format=json-render-diagnostics"], cwd=RUST, timeout=1680)
        self.check_source("post-build-source-status")

    def test(self):
        self.run("rust-gates", [sys.executable, "-I", "-B", "scripts/build/rust_test_runner.py",
                 "--target-dir", str(self.target), "--cargo-profile", "dev-small", "run-gate",
                 "--profile", "fast", "--no-fail-fast", "--command-timeout-seconds", "1200", *GATES], timeout=1680)
        self.check_source("post-test-source-status")

    def finalize(self):
        problems = []
        steps = json.loads(os.environ.get("KD4_STEP_RESULTS", "{}"))
        self.data["workflow_steps_before_upload"] = steps
        for name in ("checkout", "initialize", "cache", "setup", "build", "test"):
            if steps.get(name, {}).get("outcome") != "success":
                problems.append(f"Workflow step {name}: {steps.get(name, {}).get('outcome', 'missing')}")
        try:
            self.check_source("final-source-status")
        except Exception as error:
            problems.append(str(error))

        # Retain only the runner-owned log directory, never homes, profiles or session state.
        logs = self.target / "test-runner-logs"
        retained = self.directory / "runner"
        retained.mkdir(exist_ok=True)
        budget = MAX_TOTAL_BYTES
        for directory in (self.directory / "operations", logs):
            if not directory.exists():
                problems.append(f"Missing log directory: {directory}")
                continue
            for path in sorted(directory.iterdir()):
                if not path.is_file() or path.suffix == ".lock":
                    continue
                destination = retained / path.name if directory == logs else path
                size = path.stat().st_size
                allowed = min(MAX_FILE_BYTES, max(0, budget))
                if size > allowed:
                    problems.append(f"Evidence exceeds retention bound: {path.name} ({size} bytes)")
                    # Explicit tail-only retention; incomplete evidence always fails.
                    with path.open("rb") as source:
                        source.seek(size - allowed)
                        tail = source.read(allowed)
                    destination.write_bytes(tail)
                elif destination != path:
                    shutil.copyfile(path, destination)
                budget -= min(size, allowed)

        receipts = []
        for operation in self.data["operations"]:
            if operation.get("name") == "rust-gates" and "stdout" in operation:
                path = self.directory / operation["stdout"]
                if path.is_file():
                    receipts.extend(row for row in json_lines(path)
                                    if row.get("kind") == "codex_test_execution_v1")
        expected = {(row["binary"], row["test"]) for row in self.data.get("selection", {}).get("expected_tests", [])}
        receipt = receipts[0] if len(receipts) == 1 else None
        passed = {(row["binary"], row["test"]) for row in (receipt or {}).get("executions", [])}
        if not expected or not receipt or receipt.get("exit_code") != 0 or passed != expected:
            problems.append("Missing, failing or incomplete runner execution receipt")
        if receipt and receipt.get("executed_tests") != len(passed):
            problems.append("Runner execution count disagrees with test identities")
        self.data["tests"] = {"expected_count": len(expected), "confirmed_pass_count": len(passed),
                              "unconfirmed": [{"binary": binary, "test": test} for binary, test in sorted(expected - passed)],
                              "receipt": receipt,
                              "failure_details": "runner logs and operations/*rust-gates.stderr.log"}
        # Reuse the existing reporter parser for failure observations; never count
        # its standalone observations as a substitute for the runner's gate receipt.
        from scripts.build.rust_test_runner import _nextest_results
        outcomes = []
        for path in sorted(retained.glob("rust-test-stdout-*.log")):
            parsed = _nextest_results(subprocess.CompletedProcess([], 0, stdout=path.read_text(encoding="utf-8", errors="replace"), stderr=""))
            outcomes.extend({"status": status, "binary": binary, "test": test}
                            for status, binary, test in parsed.verified_outcomes)
        self.data["tests"]["reporter_outcomes"] = outcomes
        self.data["tests"]["confirmed_failure_count"] = sum(row["status"] == "FAIL" for row in outcomes) if outcomes else None
        reporter_passes = {(row["binary"], row["test"]) for row in outcomes if row["status"] == "PASS"}
        if reporter_passes != passed or not outcomes:
            problems.append("Raw reporter logs disagree with or are missing from the execution receipt")

        metrics = list(retained.glob("validation-*.json"))
        if len(metrics) != 1 or json.loads(metrics[0].read_text(encoding="utf-8")).get("outcome") != "passed":
            problems.append("Missing or unsuccessful runner timing ledger")

        executables = {}
        for name in ("codex-command-runner", "codex-windows-sandbox-setup"):
            executables[name] = self.target / "dev-small" / (name + ".exe")
        # This target is initially empty and compiled artifacts are never cached.
        # Require one generated test executable per selected binary; report these
        # as output-file observations rather than claiming a runtime attestation.
        for binary in {binary for binary, _ in expected}:
            stem = binary.split("::bin/")[-1].replace("-", "_")
            matches = list((self.target / "dev-small" / "deps").glob(stem + "-*.exe"))
            if len(matches) == 1:
                executables[binary] = matches[0]
            else:
                problems.append(f"Expected one generated test executable for {binary}; found {len(matches)}")
        self.data["executables"] = []
        for name, path in sorted(executables.items()):
            entry = {"identity": name, "path": str(path), "state": "missing"}
            if path.is_file() and path.resolve().is_relative_to(self.target):
                entry.update(state="observed", bytes=path.stat().st_size, sha256=file_hash(path))
            else:
                problems.append(f"Missing or invalid executable: {name}")
            self.data["executables"].append(entry)
        if not {binary for binary, _ in expected}.issubset(executables):
            problems.append("Generated test executable identities are incomplete")
        for operation in self.data["operations"]:
            if operation.get("status") != "passed":
                problems.append(f"Operation {operation['name']}: {operation.get('status')}")
            for channel in ("stdout", "stderr"):
                if channel in operation and not (self.directory / operation[channel]).is_file():
                    problems.append(f"Missing {channel} log for {operation['name']}")
        self.data["evidence_files"] = [
            {"path": str(path.relative_to(self.directory)).replace("\\", "/"),
             "bytes": path.stat().st_size, "sha256": file_hash(path)}
            for path in sorted(self.directory.rglob("*")) if path.is_file() and path != self.path]
        self.data["problems"] = problems
        self.data["status"] = "passed" if not problems else "failed_or_incomplete"
        self.data["finished_at"] = now()
        self.data["elapsed_seconds"] = (datetime.fromisoformat(self.data["finished_at"]) -
                                         datetime.fromisoformat(self.data["started_at"])).total_seconds()
        self.save()
        message = (f"Windows validation: **{self.data['status']}**\n\n"
                   f"Tested SHA: `{self.data.get('source', {}).get('tested_sha', 'unknown')}`\n\n"
                   f"Confirmed passes: {len(passed)}/{len(expected)}. "
                   "Download the windows-validation artifact for summary.json and complete retained logs.\n")
        if problems:
            message += "\n" + "\n".join(f"- {problem}" for problem in problems) + "\n"
        if os.environ.get("GITHUB_STEP_SUMMARY"):
            with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as output:
                output.write(message)
        print(message)
        return 1 if problems else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=("initialize", "setup", "build", "test", "finalize"))
    args = parser.parse_args()
    evidence = Evidence()
    try:
        return getattr(evidence, args.phase)() or 0
    except Exception:
        error = traceback.format_exc()
        evidence.data.setdefault("phase_errors", {})[args.phase] = error
        evidence.data["status"] = "failed_or_incomplete"
        evidence.save()
        print(error, file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
