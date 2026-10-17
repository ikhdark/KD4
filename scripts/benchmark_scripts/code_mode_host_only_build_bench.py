"""Build-graph, binary-size and relink benchmark for the host-only Code Mode split.

usage: python -I -B code_mode_host_only_build_bench.py --codex-rs DIR --target-dir DIR
           [--runs N] [--label NAME] [--json OUT]

Validation T1 of `.codex/audits/CODE_MODE_HOST_ONLY_AUDIT_2026-10-16.md`. Uses a
dedicated Cargo target directory (dev profile) so shared test lanes stay
untouched. Reports:

graph      whether `v8` is in the normal dependency graph of codex-cli and
           codex-core (`cargo tree -e normal -i v8`).
sizes      codex.exe, codex-code-mode-host.exe and the newest codex-tools and
           codex-rollout-trace unit-test binaries.
relink     wall time of an incremental rebuild after touching one file that
           only its target compiles: `cli/src/main.rs` for codex.exe and a
           `*_tests.rs` module for each unit-test binary. Touching changes only
           the modification time, so other lanes rebuild nothing they build.
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import statistics
import subprocess
import time

RELINKS = (
    ("codex_cli_bin", "cli/src/main.rs", ["build", "-p", "codex-cli"]),
    ("codex_tools_unit_tests", "tools/src/code_mode_tests.rs", ["test", "-p", "codex-tools", "--lib", "--no-run"]),
    ("rollout_trace_unit_tests", "rollout-trace/src/protocol_event_tests.rs",
     ["test", "-p", "codex-rollout-trace", "--lib", "--no-run"]),
)


def cargo(codex_rs: pathlib.Path, target_dir: pathlib.Path, args: list[str]) -> tuple[int, float, str]:
    started = time.perf_counter()
    result = subprocess.run(
        ["cargo", *args, "--target-dir", str(target_dir)],
        cwd=codex_rs, capture_output=True, text=True, encoding="utf-8", errors="replace", check=False,
    )
    return result.returncode, time.perf_counter() - started, result.stdout + result.stderr


def v8_in_graph(codex_rs: pathlib.Path, package: str) -> bool:
    result = subprocess.run(
        ["cargo", "tree", "-p", package, "-e", "normal", "-i", "v8", "--prefix", "none"],
        cwd=codex_rs, capture_output=True, text=True, encoding="utf-8", errors="replace", check=False,
    )
    if result.returncode == 0:
        return bool(result.stdout.strip())
    if "did not match any packages" in result.stderr:
        return False
    raise RuntimeError(f"cargo tree failed for {package}: {result.stderr.strip()}")


def newest(directory: pathlib.Path, pattern: str) -> int | None:
    candidates = sorted(directory.glob(pattern), key=lambda path: path.stat().st_mtime)
    return candidates[-1].stat().st_size if candidates else None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--codex-rs", required=True, type=pathlib.Path)
    parser.add_argument("--target-dir", required=True, type=pathlib.Path)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--label", default="run")
    parser.add_argument("--json", type=pathlib.Path)
    args = parser.parse_args()
    codex_rs = args.codex_rs.resolve()
    target_dir = args.target_dir.resolve()

    report: dict = {
        "label": args.label,
        "graph": {package: v8_in_graph(codex_rs, package) for package in ("codex-cli", "codex-core")},
    }
    warmups = (["build", "-p", "codex-cli", "-p", "codex-code-mode-host"],) + tuple(command for _, _, command in RELINKS)
    for command in warmups:
        code, _, output = cargo(codex_rs, target_dir, command)
        if code != 0:
            raise SystemExit(f"warm-up failed: cargo {' '.join(command)}\n{output[-4000:]}")

    relink = {}
    for name, touched, command in RELINKS:
        samples = []
        for _ in range(args.runs):
            os.utime(codex_rs / touched, None)
            code, seconds, output = cargo(codex_rs, target_dir, command)
            if code != 0:
                raise SystemExit(f"relink failed: cargo {' '.join(command)}\n{output[-4000:]}")
            samples.append(round(seconds, 2))
        relink[name] = {"seconds": samples, "median": statistics.median(samples)}
    report["relink"] = relink

    debug = target_dir / "debug"
    report["sizes"] = {
        "codex.exe": (debug / "codex.exe").stat().st_size,
        "codex-code-mode-host.exe": (debug / "codex-code-mode-host.exe").stat().st_size,
        "codex_tools unit tests": newest(debug / "deps", "codex_tools-*.exe"),
        "codex_rollout_trace unit tests": newest(debug / "deps", "codex_rollout_trace-*.exe"),
    }
    text = json.dumps(report, indent=2)
    if args.json:
        args.json.write_text(text + "\n", encoding="utf-8")
    print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
