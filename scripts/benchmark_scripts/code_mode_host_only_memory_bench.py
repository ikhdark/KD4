"""Idle memory of `codex app-server` for the host-only Code Mode split.

usage: python -I -B code_mode_host_only_memory_bench.py --codex PATH [--runs N] [--json OUT]

Validation for `.codex/audits/CODE_MODE_HOST_ONLY_AUDIT_2026-10-16.md`. Starts
`codex app-server` with a throwaway CODEX_HOME, completes `initialize`, waits
three seconds and samples the process working set and private bytes through
PowerShell. Steady-state cells run in the host, so this measures what linking
V8 into codex.exe costs a process that never initializes it.
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import statistics
import subprocess
import tempfile
import time


def memory(pid: int) -> tuple[int, int]:
    output = subprocess.run(
        ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
         f"$p = Get-Process -Id {pid}; \"$($p.WorkingSet64) $($p.PrivateMemorySize64)\""],
        capture_output=True, text=True, check=True,
    ).stdout.split()
    return int(output[0]), int(output[1])


def sample(codex: pathlib.Path) -> tuple[int, int]:
    # app-server's background plugin clone can still hold a lock file here
    # after the kill; leftover files must not discard the sample.
    with tempfile.TemporaryDirectory(ignore_cleanup_errors=True) as home:
        process = subprocess.Popen(
            [str(codex), "app-server"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            env={**os.environ, "CODEX_HOME": home},
            text=True, encoding="utf-8",
        )
        try:
            request = {"id": 1, "method": "initialize",
                       "params": {"clientInfo": {"name": "memory-bench", "version": "0"}}}
            process.stdin.write(json.dumps(request) + "\n")
            process.stdin.flush()
            while True:
                line = process.stdout.readline()
                if not line:
                    raise SystemExit("app-server exited before answering initialize")
                if json.loads(line).get("id") == 1:
                    break
            time.sleep(3)
            return memory(process.pid)
        finally:
            process.kill()
            process.wait()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--codex", required=True, type=pathlib.Path)
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--json", type=pathlib.Path)
    args = parser.parse_args()
    samples = [sample(args.codex.resolve()) for _ in range(args.runs)]
    report = {
        "codex": str(args.codex),
        "binary_bytes": args.codex.stat().st_size,
        "working_set_bytes": [working for working, _ in samples],
        "private_bytes": [private for _, private in samples],
        "median_working_set_bytes": statistics.median(working for working, _ in samples),
        "median_private_bytes": statistics.median(private for _, private in samples),
    }
    text = json.dumps(report, indent=2)
    if args.json:
        args.json.write_text(text + "\n", encoding="utf-8")
    print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
