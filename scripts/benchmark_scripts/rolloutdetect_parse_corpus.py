#!/usr/bin/env python3
"""Run RolloutDetect `parse --summary-json` over stable saved rollouts (read-only).

Each stable rollout (not modified within --stable-seconds) under the fork
home's `sessions` and `archived_sessions` gets one bounded summary written to
`OUTDIR/<rollout stem>.summary.json`; a summary newer than its rollout is
kept, so an interrupted run resumes. Failures are listed in `OUTDIR/failures.json`
and remain failures until a successful retry, even while excluded as live.

Usage:
  rolloutdetect_parse_corpus.py --rolloutdetect DIR --out OUTDIR [--home DIR] [--jobs N]
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import subprocess
import sys
import time
from pathlib import Path

DEFAULT_HOME = Path.home() / "Desktop" / "LOCAL-KD"
LAUNCH = ("import sys; sys.path.insert(0, sys.argv.pop(1)); "
          "from rolloutdetect.cli import main; raise SystemExit(main())")


def parse_one(rolloutdetect: Path, rollout: Path, target: Path, repo: Path, timeout: int) -> str | None:
    command = [sys.executable, "-B", "-c", LAUNCH, str(rolloutdetect), "parse", str(rollout),
               "--summary-json", "--repo-root", str(repo), "--timeout", str(timeout)]
    result = subprocess.run(command, capture_output=True, text=True, encoding="utf-8",
                            errors="replace", timeout=timeout + 60)
    try:
        json.loads(result.stdout)
    except json.JSONDecodeError:
        return f"exit={result.returncode} stderr={result.stderr.strip()[-400:]}"
    if result.returncode != 0:
        return f"exit={result.returncode} stderr={result.stderr.strip()[-400:]}"
    target.write_text(result.stdout, encoding="utf-8")
    return None


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--rolloutdetect", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--home", type=Path, default=DEFAULT_HOME)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--timeout", type=int, default=300)
    parser.add_argument("--stable-seconds", type=float, default=900.0)
    args = parser.parse_args(argv)
    args.out.mkdir(parents=True, exist_ok=True)
    now = time.time()
    rollouts = [path for root in (args.home / "sessions", args.home / "archived_sessions")
                for path in sorted([*root.rglob("*.jsonl"), *root.rglob("*.jsonl.zst")])
                if now - path.stat().st_mtime >= args.stable_seconds]
    todo = [(path, args.out / f"{path.name.split('.jsonl')[0]}.summary.json") for path in rollouts]
    failure_path = args.out / "failures.json"
    previous_failures = json.loads(failure_path.read_text(encoding="utf-8")) if failure_path.exists() else {}
    todo = [(path, target) for path, target in todo
            if str(path) in previous_failures
            or not target.exists() or target.stat().st_mtime < path.stat().st_mtime]
    failures = dict(previous_failures)
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        futures = {pool.submit(parse_one, args.rolloutdetect, path, target, args.repo, args.timeout): path
                   for path, target in todo}
        for future in concurrent.futures.as_completed(futures):
            path = futures[future]
            try:
                error = future.result()
            except Exception as exc:  # noqa: BLE001 - record and continue
                error = f"{type(exc).__name__}: {exc}"
            if error:
                failures[str(path)] = error
            else:
                failures.pop(str(path), None)
    failure_path.write_text(json.dumps(failures, indent=1), encoding="utf-8")
    print(json.dumps({"stable_rollouts": len(rollouts), "parsed_now": len(todo),
                      "failures": len(failures)}, indent=1))
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
