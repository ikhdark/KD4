#!/usr/bin/env python3
"""Dispatch harness-owned tools without depending on the target checkout."""

import argparse
import json
from pathlib import Path
import runpy
import sys


COMMANDS = {
    "inventory": ("source_inventory.py", "--describe"),
    "session-audit": ("kd4_turn_latency_audit.py", "--help"),
    "tool-results": ("tool_result_audit.py", "--describe"),
    "validation": ("validation_metrics.py", "--describe"),
    "snapshot": ("rollout_snapshot.py", "--help"),
}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--describe", action="store_true")
    parser.add_argument("command", nargs="?", choices=COMMANDS)
    parser.add_argument("arguments", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    if args.describe:
        if args.command:
            parser.error("--describe is a standalone catalog operation")
        print(json.dumps({
            "format": "codex_harness_tools_v1",
            "launcher": [sys.executable, "-I", "-B", str(Path(__file__).resolve())],
            "commands": {name: {"contract": contract} for name, (_, contract) in COMMANDS.items()},
            "paths": "Target paths use the caller's cwd; installed code is resolved beside this launcher.",
            "retention": "Use authorized --output publication and --from-report replay for audit ledgers; do not emit full reports through bounded tool output.",
            "requirements": "Python 3.11+; Git for inventory; compression.zstd or backports.zstd for compressed rollouts.",
        }, sort_keys=True))
        return 0
    if args.command is None:
        parser.error("choose a command or --describe")
    if sys.version_info < (3, 11):
        parser.error("bundled harness tools require Python 3.11 or newer")
    if not sys.flags.isolated:
        parser.error("run with python -I -B to isolate imports from the target repository")
    library = Path(__file__).resolve().parent / "lib"
    source = library / COMMANDS[args.command][0]
    if not source.is_file():
        parser.error("installed harness tool payload is missing; use a harness build with bundled tools")
    arguments = args.arguments
    if args.command == "session-audit" and not any(
        arg in {"--json", "--summary-json", "--help", "-h"} for arg in arguments
    ):
        arguments = [*arguments, "--summary-json"]
    # The isolated interpreter excludes cwd/PYTHONPATH. Add only bundled owners;
    # their existing sibling-import fallback resolves the embedded dependency closure.
    sys.path.insert(0, str(library))
    sys.argv = [str(source), *arguments]
    runpy.run_path(str(source), run_name="__main__")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
