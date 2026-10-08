#!/usr/bin/env python3
"""Measure repeatable KD4 local workflow baselines and emit structured JSON."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, BinaryIO, Sequence

try:
    from scripts.process_owner import owned_process, CleanupFailed
    from scripts.atomic_json import write_json_atomic
    from scripts import kd4_model_attempt_analysis
except ImportError:  # Direct script execution places scripts/ on sys.path.
    from process_owner import owned_process, CleanupFailed
    from atomic_json import write_json_atomic
    import kd4_model_attempt_analysis


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_TIMEOUT_SECONDS = 1_800
FAILURE_OUTPUT_TAIL_BYTES = 4_096


def _run_scenario(
    command: tuple[str, ...],
    *,
    cwd: Path,
    stdout: BinaryIO,
    stderr: BinaryIO,
    timeout: float,
) -> subprocess.CompletedProcess:
    try:
        with owned_process(command, cwd=cwd, stdout=stdout, stderr=stderr) as process:
            returncode = process.wait(timeout=timeout)
    except CleanupFailed as error:
        raise RuntimeError(
            "Process cleanup unconfirmed; remaining measurements aborted"
        ) from error
    return subprocess.CompletedProcess(command, returncode)


@dataclass(frozen=True)
class Scenario:
    name: str
    command: tuple[str, ...]
    cwd: Path
    default_iterations: int
    category: str
    required: bool = True


@dataclass(frozen=True)
class Sample:
    elapsed_ms: float
    exit_code: int | None
    stdout_bytes: int
    stderr_bytes: int
    outcome: str = "completed"
    stdout_path: str | None = None
    stderr_path: str | None = None


@dataclass(frozen=True)
class ScenarioResult:
    name: str
    category: str
    required: bool
    command: tuple[str, ...]
    cwd: str
    status: str
    reason: str | None
    samples: tuple[Sample, ...]
    cold_ms: float | None
    warm_p50_ms: float | None
    warm_p95_ms: float | None
    p50_ms: float | None
    p95_ms: float | None
    min_ms: float | None
    max_ms: float | None

    @property
    def passed(self) -> bool:
        return self.status == "passed"


class ScenarioAborted(RuntimeError):
    def __init__(self, result: ScenarioResult):
        super().__init__(result.reason)
        self.result = result


def _retain_output(output: BinaryIO, stream: str) -> str:
    output.seek(0)
    with tempfile.NamedTemporaryFile(
        prefix="kd4-perf-", suffix=f".{stream}.log", delete=False
    ) as retained:
        shutil.copyfileobj(output, retained)
    return retained.name


def _percentile_from_ordered(ordered: Sequence[float], fraction: float) -> float:
    if not ordered:
        raise ValueError("percentile requires at least one value")
    if not 0 <= fraction <= 1:
        raise ValueError("fraction must be between zero and one")
    position = (len(ordered) - 1) * fraction
    lower = int(position)
    upper = min(lower + 1, len(ordered) - 1)
    weight = position - lower
    return ordered[lower] * (1 - weight) + ordered[upper] * weight


def percentile(values: Sequence[float], fraction: float) -> float:
    return _percentile_from_ordered(sorted(values), fraction)


def _ordered_sample_statistics(
    values: Sequence[float],
) -> tuple[float, float, float, float]:
    ordered = sorted(values)
    return (
        _percentile_from_ordered(ordered, 0.50),
        _percentile_from_ordered(ordered, 0.95),
        ordered[0],
        ordered[-1],
    )


def _output_size_and_tail(output: BinaryIO) -> tuple[int, str]:
    output.flush()
    output.seek(0, os.SEEK_END)
    size = output.tell()
    output.seek(max(0, size - FAILURE_OUTPUT_TAIL_BYTES))
    tail = output.read(FAILURE_OUTPUT_TAIL_BYTES).decode("utf-8", errors="replace")
    return size, tail


def _failure_reason(message: str, stdout_tail: str, stderr_tail: str) -> str:
    diagnostics = []
    if stdout_tail:
        diagnostics.append(f"stdout tail: {stdout_tail!r}")
    if stderr_tail:
        diagnostics.append(f"stderr tail: {stderr_tail!r}")
    return f"{message}; " + "; ".join(diagnostics) if diagnostics else message


def _installed_codex_path(install_dir: Path | None = None) -> Path:
    if install_dir is not None:
        publish_dir = install_dir
    else:
        configured_publish_dir = os.environ.get("CODEX_LOCAL_PUBLISH_DIR")
        publish_dir = (
            Path(configured_publish_dir)
            if configured_publish_dir
            else Path.home() / "Desktop" / "LOCAL-KD" / "bin"
        )
    return publish_dir / "codex.exe"


def scenario_catalog(
    repo_root: Path = REPO_ROOT, *, install_dir: Path | None = None
) -> dict[str, Scenario]:
    repo_root = repo_root.resolve()
    codex_rs = repo_root / "codex-rs"
    installed_codex = _installed_codex_path(install_dir)
    return {
        "python-startup": Scenario(
            "python-startup",
            (sys.executable, "-c", "pass"),
            repo_root,
            7,
            "startup",
        ),
        "git-status": Scenario(
            "git-status",
            ("git", "status", "--porcelain=v2", "--untracked-files=no"),
            repo_root,
            5,
            "repository",
        ),
        "installed-codex-version": Scenario(
            "installed-codex-version",
            (str(installed_codex), "--version"),
            repo_root,
            5,
            "startup",
        ),
        "focused-core-test": Scenario(
            "focused-core-test",
            (
                "just",
                "core-test-fast",
                "core_lib",
                "-E",
                "test(=agent::task_capabilities::tests::typed_agents_inherit_every_non_root_tool_class)",
            ),
            codex_rs,
            2,
            "test",
        ),
        "local-cli-build": Scenario(
            "local-cli-build",
            ("cargo", "build", "-p", "codex-cli"),
            codex_rs,
            2,
            "build",
        ),
        "app-server-initialize-test": Scenario(
            "app-server-initialize-test",
            (
                "cargo",
                "nextest",
                "run",
                "-p",
                "codex-app-server",
                "-E",
                "test(initialize_response_includes_local_runtime_metadata)",
            ),
            codex_rs,
            2,
            "app-server",
        ),
        "desktop-publish-dry-run": Scenario(
            "desktop-publish-dry-run",
            ("just", "publish-local-codex-final", "-DryRun"),
            repo_root,
            2,
            "desktop-publish",
        ),
    }


PROFILE_SCENARIOS = {
    "quick": ("python-startup", "git-status"),
    "phase0": (
        "python-startup",
        "git-status",
        "installed-codex-version",
        "focused-core-test",
        "local-cli-build",
        "app-server-initialize-test",
        "desktop-publish-dry-run",
    ),
}


def _executable_available(command: str, cwd: Path) -> bool:
    candidate = Path(command)
    if candidate.is_absolute():
        return candidate.is_file()
    if any(separator in command for separator in ("/", "\\")):
        return (cwd / candidate).is_file()
    return shutil.which(command) is not None


def measure_scenario(
    scenario: Scenario,
    *,
    iterations: int | None = None,
    timeout_seconds: int = DEFAULT_TIMEOUT_SECONDS,
) -> ScenarioResult:
    count = scenario.default_iterations if iterations is None else iterations
    if count < 1:
        raise ValueError("iterations must be positive")
    if timeout_seconds <= 0:
        raise ValueError("timeout must be positive")
    if not _executable_available(scenario.command[0], scenario.cwd):
        return ScenarioResult(
            name=scenario.name,
            category=scenario.category,
            required=scenario.required,
            command=scenario.command,
            cwd=str(scenario.cwd),
            status="skipped",
            reason=f"executable is unavailable: {scenario.command[0]}",
            samples=(),
            cold_ms=None,
            warm_p50_ms=None,
            warm_p95_ms=None,
            p50_ms=None,
            p95_ms=None,
            min_ms=None,
            max_ms=None,
        )

    samples: list[Sample] = []
    reason: str | None = None
    aborted: BaseException | None = None
    for _ in range(count):
        with (
            tempfile.TemporaryFile() as stdout_file,
            tempfile.TemporaryFile() as stderr_file,
        ):
            exit_code = None
            outcome = "completed"
            started = time.perf_counter_ns()
            try:
                completed = _run_scenario(
                    scenario.command,
                    cwd=scenario.cwd,
                    stdout=stdout_file,
                    stderr=stderr_file,
                    timeout=timeout_seconds,
                )
                exit_code = completed.returncode
                if exit_code != 0:
                    reason = f"command exited {exit_code}"
            except subprocess.TimeoutExpired as exc:
                outcome, reason = "timeout", str(exc)
            except OSError as exc:
                outcome, reason = "launch-error", str(exc)
            except (RuntimeError, KeyboardInterrupt) as exc:
                outcome, reason, aborted = "aborted", str(exc) or "interrupted", exc
            elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
            stdout_bytes, stdout_tail = _output_size_and_tail(stdout_file)
            stderr_bytes, stderr_tail = _output_size_and_tail(stderr_file)
            stdout_path = stderr_path = None
            if reason is not None:
                stdout_path = _retain_output(stdout_file, "stdout")
                stderr_path = _retain_output(stderr_file, "stderr")
                reason = _failure_reason(reason, stdout_tail, stderr_tail)
        samples.append(
            Sample(
                elapsed_ms=round(elapsed_ms, 3),
                exit_code=exit_code,
                stdout_bytes=stdout_bytes,
                stderr_bytes=stderr_bytes,
                outcome=outcome,
                stdout_path=stdout_path,
                stderr_path=stderr_path,
            )
        )
        if reason is not None:
            break

    passed = len(samples) == count and all(sample.exit_code == 0 for sample in samples)
    # A failed invocation times failure handling, not the scenario: it stays in
    # samples but never enters statistics. The loop stops at the first failure,
    # so successful samples are a prefix and the first one is the cold run.
    elapsed = [sample.elapsed_ms for sample in samples if sample.exit_code == 0]
    warm = elapsed[1:]
    p50_ms, p95_ms, min_ms, max_ms = (
        _ordered_sample_statistics(elapsed) if elapsed else (None, None, None, None)
    )
    warm_p50_ms, warm_p95_ms = (
        _ordered_sample_statistics(warm)[:2] if warm else (None, None)
    )
    result = ScenarioResult(
        name=scenario.name,
        category=scenario.category,
        required=scenario.required,
        command=scenario.command,
        cwd=str(scenario.cwd),
        status="passed" if passed else "failed",
        reason=reason,
        samples=tuple(samples),
        cold_ms=elapsed[0] if elapsed else None,
        warm_p50_ms=round(warm_p50_ms, 3) if warm_p50_ms is not None else None,
        warm_p95_ms=round(warm_p95_ms, 3) if warm_p95_ms is not None else None,
        p50_ms=round(p50_ms, 3) if p50_ms is not None else None,
        p95_ms=round(p95_ms, 3) if p95_ms is not None else None,
        min_ms=min_ms,
        max_ms=max_ms,
    )
    if aborted is not None:
        raise ScenarioAborted(result) from aborted
    return result


def _git_text(repo_root: Path, *args: str) -> str | None:
    try:
        completed = subprocess.run(
            ["git", *args],
            cwd=repo_root,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=30,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    return completed.stdout.strip() if completed.returncode == 0 else None


def _legacy_git_repository_metadata(
    repo_root: Path,
) -> tuple[str | None, str | None, int | None]:
    status = _git_text(repo_root, "status", "--porcelain=v1", "--untracked-files=all")
    return (
        _git_text(repo_root, "rev-parse", "HEAD"),
        _git_text(repo_root, "branch", "--show-current"),
        len(status.splitlines()) if status is not None else None,
    )


def _git_repository_metadata(
    repo_root: Path,
) -> tuple[str | None, str | None, int | None]:
    status = _git_text(
        repo_root,
        "status",
        "--porcelain=v2",
        "--branch",
        "--untracked-files=all",
    )
    if status is None:
        return _legacy_git_repository_metadata(repo_root)

    head: str | None = None
    branch: str | None = None
    saw_head = False
    saw_branch = False
    dirty_paths = 0
    for line in status.splitlines():
        if line.startswith("# branch.oid "):
            saw_head = True
            value = line.removeprefix("# branch.oid ")
            head = None if value == "(initial)" else value
        elif line.startswith("# branch.head "):
            saw_branch = True
            value = line.removeprefix("# branch.head ")
            branch = None if value == "(detached)" else value
        elif not line.startswith("# "):
            dirty_paths += 1
    if not saw_head or not saw_branch:
        return _legacy_git_repository_metadata(repo_root)
    return head, branch, dirty_paths


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def environment_metadata(
    repo_root: Path, *, hash_binary: bool, install_dir: Path | None = None
) -> dict[str, Any]:
    installed_codex = _installed_codex_path(install_dir)
    binary: dict[str, Any] = {
        "path": str(installed_codex),
        "exists": installed_codex.is_file(),
    }
    if installed_codex.is_file():
        stat = installed_codex.stat()
        binary.update({"size": stat.st_size, "mtimeNs": stat.st_mtime_ns})
        if hash_binary:
            binary["sha256"] = _sha256(installed_codex)
    head, branch, dirty_paths = _git_repository_metadata(repo_root)
    return {
        "capturedAt": datetime.now(timezone.utc).isoformat(),
        "repository": str(repo_root.resolve()),
        "head": head,
        "branch": branch,
        "dirtyPaths": dirty_paths,
        "platform": platform.platform(),
        "python": sys.version,
        "cpuCount": os.cpu_count(),
        "installedCodex": binary,
    }


def build_parser() -> argparse.ArgumentParser:
    catalog = scenario_catalog()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument(
        "--install-dir",
        type=Path,
        help="Installed Codex directory (defaults to CODEX_LOCAL_PUBLISH_DIR or the platform local-publish location).",
    )
    parser.add_argument("--profile", choices=sorted(PROFILE_SCENARIOS), default="quick")
    parser.add_argument("--scenario", action="append", choices=sorted(catalog))
    parser.add_argument("--iterations", type=int)
    parser.add_argument("--timeout-seconds", type=int, default=DEFAULT_TIMEOUT_SECONDS)
    parser.add_argument("--hash-binary", action="store_true")
    parser.add_argument("--allow-failures", action="store_true")
    parser.add_argument(
        "--allow-incomplete",
        action="store_true",
        help="Allow required scenarios to be skipped without failing the snapshot.",
    )
    parser.add_argument("--output", type=Path)
    parser.add_argument(
        "--model-attempt-jsonl",
        action="append",
        type=Path,
        help="Analyze privacy-safe codex.model_attempt JSONL telemetry.",
    )
    parser.add_argument(
        "--model-attempt-report",
        type=Path,
        help="Write a human-readable model-attempt report.",
    )
    parser.add_argument("--json", action="store_true")
    parser.add_argument(
        "--analysis-only", action="store_true",
        help="Analyze --model-attempt-jsonl without workflow scenarios or Git/binary probes.",
    )
    return parser


def _preflight_destination(path: Path) -> None:
    if path.exists() and not path.is_file():
        raise ValueError(f"report destination is not a file: {path}")
    if path.is_file():
        # Verify access without truncating an existing report.
        with path.open("r+b"):
            pass
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryFile(dir=path.parent):
        pass


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    repo_root = args.repo_root.resolve()
    catalog = scenario_catalog(repo_root, install_dir=args.install_dir)
    names = () if args.analysis_only else tuple(args.scenario or PROFILE_SCENARIOS[args.profile])
    attempt_analysis = human_report = None
    try:
        if args.analysis_only and (not args.model_attempt_jsonl or args.scenario or args.hash_binary):
            raise ValueError("--analysis-only requires --model-attempt-jsonl and excludes --scenario/--hash-binary")
        if args.iterations is not None and args.iterations < 1:
            raise ValueError("iterations must be positive")
        if args.timeout_seconds <= 0:
            raise ValueError("timeout must be positive")
        if args.model_attempt_report and not args.model_attempt_jsonl:
            raise ValueError("--model-attempt-report requires --model-attempt-jsonl")
        inputs = {path.resolve() for path in args.model_attempt_jsonl or ()}
        destinations = [
            path.resolve()
            for path in (args.output, args.model_attempt_report)
            if path is not None
        ]
        if len(set(destinations)) != len(destinations) or inputs.intersection(
            destinations
        ):
            raise ValueError(
                "report destinations must be distinct from each other and inputs"
            )
        if args.model_attempt_jsonl:
            attempts, exclusions = kd4_model_attempt_analysis.load_jsonl(
                args.model_attempt_jsonl
            )
            attempt_analysis = kd4_model_attempt_analysis.analyze(attempts, exclusions)
            human_report = kd4_model_attempt_analysis.render(attempt_analysis)
        for path in destinations:
            _preflight_destination(path)
    except (OSError, ValueError) as exc:
        parser.error(str(exc))

    environment = None if args.analysis_only else environment_metadata(
        repo_root, hash_binary=args.hash_binary, install_dir=args.install_dir
    )
    results: list[ScenarioResult] = []
    abort_reason = None
    for name in names:
        if not args.json:
            print(f"[RUN] {name}", flush=True)
        try:
            result = measure_scenario(
                catalog[name],
                iterations=args.iterations,
                timeout_seconds=args.timeout_seconds,
            )
        except ScenarioAborted as exc:
            results.append(exc.result)
            abort_reason = str(exc)
            break
        except (OSError, RuntimeError, KeyboardInterrupt) as exc:
            abort_reason = str(exc) or "interrupted"
            break
        results.append(result)
        if not args.json:
            print(
                f"[{result.status.upper()}] {name}: "
                f"first_invocation={result.cold_ms}ms warm_p50={result.warm_p50_ms}ms "
                f"warm_p95={result.warm_p95_ms}ms"
            )
            if result.reason is not None:
                print(result.reason)
                for sample in result.samples:
                    if sample.stdout_path is not None:
                        print(f"full output: {sample.stdout_path} {sample.stderr_path}")

    failed = [result.name for result in results if result.status == "failed"]
    skipped = [result.name for result in results if result.status == "skipped"]
    skipped_required = [
        result.name
        for result in results
        if result.status == "skipped" and result.required
    ]
    complete = abort_reason is None and not skipped_required
    ok = abort_reason is None and not failed and (complete or args.allow_incomplete)
    payload = {
        "schemaVersion": 1,
        "analysisOnly": args.analysis_only,
        "profile": args.profile,
        "environment": environment,
        "results": [asdict(result) for result in results],
        "failedScenarios": failed,
        "skippedScenarios": skipped,
        "skippedRequiredScenarios": skipped_required,
        "complete": complete,
        "incomplete": not complete,
        "allowIncomplete": args.allow_incomplete,
        "ok": ok,
        "abortReason": abort_reason,
        "pendingScenarios": list(names[len(results) :]),
    }
    # Save completed evidence before optional report rendering or console output.
    if attempt_analysis is not None:
        payload["modelAttemptAnalysis"] = attempt_analysis
    if args.output is not None:
        write_json_atomic(args.output, payload)
    if human_report is not None:
        if args.model_attempt_report is not None:
            args.model_attempt_report.parent.mkdir(parents=True, exist_ok=True)
            args.model_attempt_report.write_text(human_report + "\n", encoding="utf-8")
        if not args.json:
            print(human_report)
    if args.json:
        print(json.dumps(payload, sort_keys=True))
    if abort_reason is not None and not args.json:
        print(f"[ABORTED] {abort_reason}", file=sys.stderr)
        for result in results:
            for sample in result.samples:
                if sample.outcome == "aborted" and sample.stdout_path is not None:
                    print(f"full output: {sample.stdout_path} {sample.stderr_path}")
    return 0 if abort_reason is None and (ok or args.allow_failures) else 1


if __name__ == "__main__":
    raise SystemExit(main())
