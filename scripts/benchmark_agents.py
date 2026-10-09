#!/usr/bin/env python3
"""Plan/check benchmark jobs; only --execute starts billable Harbor work."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import sys
import zipfile
from datetime import UTC, datetime
from pathlib import Path

import tomllib

SWE_REVISION = "66f92766bba642462d4bbe5479e83f91f9211862"
SWE_ARCHIVE_SHA256 = "317967aae26b7d475dc34ee2350981bf4e2eaf4b34dbf4833095ac2fee8f0fc1"
HARBOR_VERSION = "0.24.0"
MODAL_VERSION = "1.6.1"
REPO_ROOT = Path(__file__).resolve().parents[1]


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def verify_swe(root):
    archive = root / "source.zip"
    if sha256(archive) != SWE_ARCHIVE_SHA256:
        raise ValueError("Cached SWE source archive does not match the pinned download")
    with zipfile.ZipFile(archive) as source:
        name = f"SWE-bench_Pro-os-{SWE_REVISION}/v2/SHA256SUMS"
        checksums = source.read(name)
    base = (root / "v2").resolve()
    if (base / "SHA256SUMS").read_bytes() != checksums:
        raise ValueError("Cached SHA256SUMS was modified")
    count = 0
    for line in checksums.decode().splitlines():
        expected, relative = line.split(None, 1)
        path = (base / relative.lstrip("*")).resolve()
        if not path.is_relative_to(base) or sha256(path) != expected:
            raise ValueError(f"SWE checksum mismatch: {relative}")
        count += 1
    return count


def harbor_python(explicit=None):
    if explicit:
        return Path(explicit).resolve(strict=True)
    result = subprocess.run(["uv", "tool", "dir"], capture_output=True, text=True, check=True)
    path = Path(result.stdout.strip()) / "harbor" / "Scripts" / "python.exe"
    if not path.is_file():
        raise ValueError("Install harbor[modal]==0.24.0 or pass --harbor-python")
    return path


def positive(value):
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def parser():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("benchmark", choices=["terminal", "swe", "regrade"])
    p.add_argument("--fork-home", type=Path, default=os.environ.get("CODEX_HOME"))
    p.add_argument("--codex-binary", type=Path)
    p.add_argument("--harbor-python")
    p.add_argument("--model", help="Defaults to the fork's configured model, never upstream Codex")
    p.add_argument("--reasoning-effort", choices=["minimal", "low", "medium", "high", "xhigh"])
    p.add_argument("--tasks", type=positive, default=1, help="Task limit (default: one smoke task)")
    p.add_argument("--concurrency", type=positive, default=1)
    p.add_argument("--task", action="append", default=[], help="Exact task name or Harbor filter; repeatable")
    p.add_argument("--job-name")
    p.add_argument("--source-job", type=Path, help="Required for regrade; selects the source job's tasks")
    p.add_argument("--check", action="store_true", help="Validate cache, adapter and Harbor config, without running")
    p.add_argument("--execute", action="store_true", help="Explicitly start a potentially billable run")
    return p


def build_command(args):
    if args.fork_home is None:
        raise ValueError("Set CODEX_HOME to the fork home or pass --fork-home explicitly")
    home = args.fork_home.expanduser().resolve(strict=True)
    config_path = home / "config.toml"
    config = tomllib.loads(config_path.read_text(encoding="utf-8")) if config_path.exists() else {}
    model = args.model or config.get("model")
    if args.benchmark != "regrade" and not model:
        raise ValueError("Pass --model (no implicit model fallback)")
    if args.benchmark != "regrade" and not args.model and config.get("model_provider", "openai") != "openai":
        raise ValueError("Custom provider configuration is not imported; pass an explicit OpenAI --model")
    if args.benchmark != "regrade":
        binary = (args.codex_binary or home / "bin" / "codex.exe").resolve(strict=True)
    swe = home / "benchmarks" / f"swe-bench-pro-v2-{SWE_REVISION[:12]}"
    name = args.job_name or f"windows-codex-{args.benchmark}-{datetime.now(UTC):%Y%m%d-%H%M%SZ}"
    if Path(name).name != name or any(c in name for c in '/\\:') or name in (".", ".."):
        raise ValueError("job-name must be a single directory name")
    jobs = home / "benchmarks" / "jobs"
    if (jobs / name).exists():
        raise ValueError("Job already exists; choose a new job-name (no automatic resume/overwrite)")
    command = ["run", "-e", "modal", "-n", str(args.concurrency), "-k", "1", "--max-retries", "0",
               "--jobs-dir", str(jobs), "--job-name", name]
    filters = args.task
    if args.benchmark == "terminal":
        command += ["-d", "terminal-bench/terminal-bench@4.0.0"]
    else:
        if not (swe / "v2" / "tasks").is_dir():
            raise ValueError(f"Missing pinned SWE V2 cache: {swe}")
        command += ["-p", str(swe / "v2" / "tasks")]
    if args.benchmark == "regrade":
        if args.source_job is None:
            raise ValueError("regrade requires --source-job")
        if args.task or args.tasks != 1:
            raise ValueError("regrade selects every source task; do not pass --task or --tasks")
        source = args.source_job.resolve(strict=True)
        filters = []
        for path in sorted(source.glob("*/result.json")):
            result = json.loads(path.read_text(encoding="utf-8"))
            name = result["task_name"].replace("\\", "/").split("/")[-1]
            if name in filters or not (path.parent / "agent" / "model.patch").is_file():
                raise ValueError(f"Ambiguous or missing source patch: {name}")
            if not (swe / "v2" / "tasks" / name / "task.toml").is_file():
                raise ValueError(f"Source task is not in the pinned SWE dataset: {name}")
            filters.append(name)
        if not filters:
            raise ValueError("Source job contains no trial results")
        command += ["-a", "scripts.harbor_windows_codex:WindowsPatchReplay", "-m", "replay",
                    "--ak", f"source_job={source}"]
    else:
        command += ["--n-tasks", str(args.tasks), "-a", "scripts.harbor_windows_codex:WindowsCodex", "-m", model,
                    "--ak", f"codex_binary={binary}", "--ak", f"auth_home={home}",
                    "--ak", f"reasoning_effort={args.reasoning_effort or config.get('model_reasoning_effort', 'high')}"]
        if args.benchmark == "swe":
            command += ["--disable-verification", "--ak", "capture_patch=true", "--ak", "budget_sec=2940"]
    for task in filters:
        command += ["-i", task]
    return command, swe


def main(argv=None):
    args = parser().parse_args(argv)
    try:
        command, swe = build_command(args)
        python = harbor_python(args.harbor_python)
        environment = dict(os.environ, PYTHONPATH=str(REPO_ROOT))
        invocation = [str(python), "-c", "from harbor.cli.main import app; app()", *command]
        print(json.dumps({"argv": invocation, "cwd": str(REPO_ROOT), "execute": args.execute,
                          "warning": "Custom terminal harness; Modal/model usage may be billable. No publishing."}, indent=2))
        if args.check or args.execute:
            if args.benchmark != "terminal":
                print(f"Verified SWE files: {verify_swe(swe)}")
            check = (
                "from importlib.metadata import version; "
                f"assert version('harbor') == '{HARBOR_VERSION}', 'Harbor version drift'; "
                f"assert version('modal') == '{MODAL_VERSION}', 'Modal version drift'; "
                "from scripts.harbor_windows_codex import WindowsCodex; "
            )
            if args.benchmark != "regrade":
                options = dict(item.split("=", 1) for index, item in enumerate(command) if index and command[index-1] == "--ak")
                check += f"WindowsCodex.preflight({options!r})"
            subprocess.run([str(python), "-c", check], cwd=REPO_ROOT, env=environment, check=True)
            subprocess.run([*invocation, "--print-config"], cwd=REPO_ROOT, env=environment, check=True)
        if args.execute:
            return subprocess.call(invocation, cwd=REPO_ROOT, env=environment)
        print("Not started. Add --execute only when ready to incur sandbox/model usage.")
        return 0
    except (OSError, ValueError, subprocess.CalledProcessError) as exc:
        print(f"Benchmark setup error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
