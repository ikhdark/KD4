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
from statistics import median
from typing import Any, Sequence


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_TIMEOUT_SECONDS = 1_800


@dataclass(frozen=True)
class Scenario:
    name: str
    command: tuple[str, ...]
    cwd: Path
    default_iterations: int
    category: str


@dataclass(frozen=True)
class Sample:
    elapsed_ms: float
    exit_code: int
    stdout_bytes: int
    stderr_bytes: int


@dataclass(frozen=True)
class ScenarioResult:
    name: str
    category: str
    command: tuple[str, ...]
    cwd: str
    status: str
    reason: str | None
    samples: tuple[Sample, ...]
    cold_ms: float | None
    warm_p50_ms: float | None
    p50_ms: float | None
    p95_ms: float | None
    min_ms: float | None
    max_ms: float | None

    @property
    def passed(self) -> bool:
        return self.status in {"passed", "skipped"}


def percentile(values: Sequence[float], fraction: float) -> float:
    if not values:
        raise ValueError("percentile requires at least one value")
    if not 0 <= fraction <= 1:
        raise ValueError("fraction must be between zero and one")
    ordered = sorted(values)
    position = (len(ordered) - 1) * fraction
    lower = int(position)
    upper = min(lower + 1, len(ordered) - 1)
    weight = position - lower
    return ordered[lower] * (1 - weight) + ordered[upper] * weight


def _installed_codex_path(repo_root: Path) -> Path:
    publish_dir = os.environ.get("CODEX_LOCAL_PUBLISH_DIR")
    if publish_dir:
        return Path(publish_dir) / ("codex.exe" if os.name == "nt" else "codex")
    return repo_root.parent / "LOCAL-KD" / ("codex.exe" if os.name == "nt" else "codex")


def scenario_catalog(repo_root: Path = REPO_ROOT) -> dict[str, Scenario]:
    repo_root = repo_root.resolve()
    codex_rs = repo_root / "codex-rs"
    installed_codex = _installed_codex_path(repo_root)
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
        "feature-check": Scenario(
            "feature-check",
            (sys.executable, "scripts/check_kd4_features.py", "--json"),
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
        "verify-local-plan": Scenario(
            "verify-local-plan",
            (
                sys.executable,
                "scripts/verify_local.py",
                "--plan",
                "--changed",
                "kd4_features.toml",
                "--json",
            ),
            repo_root,
            3,
            "validation",
        ),
        "focused-core-test": Scenario(
            "focused-core-test",
            (
                "cargo",
                "nextest",
                "run",
                "-p",
                "codex-core",
                "-E",
                "test(verify_local_is_registered_only_for_a_supported_local_repo)",
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
            ("just", "publish-local-codex-final-dry-run"),
            repo_root,
            2,
            "desktop-publish",
        ),
    }


PROFILE_SCENARIOS = {
    "quick": ("python-startup", "git-status", "feature-check"),
    "phase0": (
        "python-startup",
        "git-status",
        "feature-check",
        "installed-codex-version",
        "verify-local-plan",
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
    if not _executable_available(scenario.command[0], scenario.cwd):
        return ScenarioResult(
            name=scenario.name,
            category=scenario.category,
            command=scenario.command,
            cwd=str(scenario.cwd),
            status="skipped",
            reason=f"executable is unavailable: {scenario.command[0]}",
            samples=(),
            cold_ms=None,
            warm_p50_ms=None,
            p50_ms=None,
            p95_ms=None,
            min_ms=None,
            max_ms=None,
        )

    samples: list[Sample] = []
    reason: str | None = None
    for _ in range(count):
        started = time.perf_counter_ns()
        try:
            completed = subprocess.run(
                scenario.command,
                cwd=scenario.cwd,
                capture_output=True,
                timeout=timeout_seconds,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            reason = str(exc)
            break
        elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
        samples.append(
            Sample(
                elapsed_ms=round(elapsed_ms, 3),
                exit_code=completed.returncode,
                stdout_bytes=len(completed.stdout),
                stderr_bytes=len(completed.stderr),
            )
        )
        if completed.returncode != 0:
            reason = f"command exited {completed.returncode}"
            break

    elapsed = [sample.elapsed_ms for sample in samples]
    passed = len(samples) == count and all(sample.exit_code == 0 for sample in samples)
    warm = elapsed[1:]
    return ScenarioResult(
        name=scenario.name,
        category=scenario.category,
        command=scenario.command,
        cwd=str(scenario.cwd),
        status="passed" if passed else "failed",
        reason=reason,
        samples=tuple(samples),
        cold_ms=elapsed[0] if elapsed else None,
        warm_p50_ms=round(median(warm), 3) if warm else None,
        p50_ms=round(percentile(elapsed, 0.50), 3) if elapsed else None,
        p95_ms=round(percentile(elapsed, 0.95), 3) if elapsed else None,
        min_ms=min(elapsed) if elapsed else None,
        max_ms=max(elapsed) if elapsed else None,
    )


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


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def environment_metadata(repo_root: Path, *, hash_binary: bool) -> dict[str, Any]:
    installed_codex = _installed_codex_path(repo_root)
    binary: dict[str, Any] = {
        "path": str(installed_codex),
        "exists": installed_codex.is_file(),
    }
    if installed_codex.is_file():
        stat = installed_codex.stat()
        binary.update({"size": stat.st_size, "mtimeNs": stat.st_mtime_ns})
        if hash_binary:
            binary["sha256"] = _sha256(installed_codex)
    status = _git_text(repo_root, "status", "--porcelain=v1", "--untracked-files=all")
    return {
        "capturedAt": datetime.now(timezone.utc).isoformat(),
        "repository": str(repo_root.resolve()),
        "head": _git_text(repo_root, "rev-parse", "HEAD"),
        "branch": _git_text(repo_root, "branch", "--show-current"),
        "dirtyPaths": len(status.splitlines()) if status else 0,
        "platform": platform.platform(),
        "python": sys.version,
        "cpuCount": os.cpu_count(),
        "installedCodex": binary,
    }


def write_json_atomic(path: Path, payload: dict[str, Any]) -> None:
    path = path.resolve()
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        mode="w",
        encoding="utf-8",
        dir=path.parent,
        prefix=f".{path.name}.",
        suffix=".tmp",
        delete=False,
    ) as temporary:
        json.dump(payload, temporary, indent=2, sort_keys=True)
        temporary.write("\n")
        temporary_path = Path(temporary.name)
    os.replace(temporary_path, path)


def build_parser() -> argparse.ArgumentParser:
    catalog = scenario_catalog()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument("--profile", choices=sorted(PROFILE_SCENARIOS), default="quick")
    parser.add_argument("--scenario", action="append", choices=sorted(catalog))
    parser.add_argument("--iterations", type=int)
    parser.add_argument("--timeout-seconds", type=int, default=DEFAULT_TIMEOUT_SECONDS)
    parser.add_argument("--hash-binary", action="store_true")
    parser.add_argument("--allow-failures", action="store_true")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--json", action="store_true")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    repo_root = args.repo_root.resolve()
    catalog = scenario_catalog(repo_root)
    names = tuple(args.scenario or PROFILE_SCENARIOS[args.profile])
    results: list[ScenarioResult] = []
    for name in names:
        if not args.json:
            print(f"[RUN] {name}", flush=True)
        result = measure_scenario(
            catalog[name],
            iterations=args.iterations,
            timeout_seconds=args.timeout_seconds,
        )
        results.append(result)
        if not args.json:
            print(
                f"[{result.status.upper()}] {name}: "
                f"cold={result.cold_ms}ms warm_p50={result.warm_p50_ms}ms "
                f"p95={result.p95_ms}ms"
            )

    failed = [result.name for result in results if result.status == "failed"]
    payload = {
        "schemaVersion": 1,
        "profile": args.profile,
        "environment": environment_metadata(repo_root, hash_binary=args.hash_binary),
        "results": [asdict(result) for result in results],
        "failedScenarios": failed,
        "ok": not failed,
    }
    if args.output is not None:
        write_json_atomic(args.output, payload)
    if args.json:
        print(json.dumps(payload, sort_keys=True))
    return 0 if not failed or args.allow_failures else 1


if __name__ == "__main__":
    raise SystemExit(main())
