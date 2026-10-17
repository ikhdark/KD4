"""Cargo --timings capture for the codex-core crate-split work.

usage:
  python -I -B core_crate_split_timings.py run --codex-rs DIR --target-dir DIR
      --out DIR --label NAME [--clean PKG ...] [--touch FILE] -- CARGO-ARGS...
  python -I -B core_crate_split_timings.py summarize HTML [--unit NAME ...]
  python -I -B core_crate_split_timings.py compare BEFORE.json AFTER.json [--unit NAME ...]
  python -I -B core_crate_split_timings.py probe --file FILE --find OLD --replace NEW
  python -I -B core_crate_split_timings.py rounds RESULTS-DIR
  python -I -B core_crate_split_timings.py paired RESULTS-DIR

`rounds` and `paired` print markdown tables for the reports written by
core_crate_split_protocol.sh's sequential and paired modes.

`run` applies the repository's local Rust tool environment (sccache wrapper,
lld-link), optionally runs `cargo clean -p PKG` for each --clean package and
touches --touch files, then runs `cargo CARGO-ARGS --timings --target-dir DIR`
from --codex-rs so its pinned toolchain and .cargo configuration apply. It
writes NAME.log, NAME.html (the Cargo timing report) and NAME.json (wall time,
per-unit durations and rustc process-count samples taken every few seconds,
which include this build's own compilers) into --out. On Windows the cargo
process tree runs in a job object, so NAME.json also records the build's total
CPU seconds and its largest process's peak commit; CPU time is far less
sensitive than wall time to other builds sharing the machine. Use a dedicated
--target-dir; this script never touches shared build lanes.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import pathlib
import re
import shutil
import statistics
import subprocess
import sys
import threading
import time

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from scripts.build.rust_tool_env import local_rust_env

SAMPLE_SECONDS = 5.0


def rustc_process_count() -> int | None:
    try:
        result = subprocess.run(
            ["tasklist", "/FO", "CSV", "/NH", "/FI", "IMAGENAME eq rustc.exe"],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=20,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    return sum(
        1 for line in result.stdout.splitlines() if line.startswith('"rustc.exe"')
    )


class JobAccounting:
    """Windows job object totalling CPU time and peak commit for a process tree.

    Children a process starts after `assign` join its job. Cargo resolves the
    workspace before it starts compilers, so only its quick `rustc -vV` probe
    can race the assignment.
    """

    def __init__(self) -> None:
        import ctypes
        from ctypes import wintypes

        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.CreateJobObjectW.argtypes = [wintypes.LPVOID, wintypes.LPCWSTR]
        kernel32.CreateJobObjectW.restype = wintypes.HANDLE
        kernel32.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
        kernel32.AssignProcessToJobObject.restype = wintypes.BOOL
        kernel32.QueryInformationJobObject.argtypes = [
            wintypes.HANDLE,
            ctypes.c_int,
            wintypes.LPVOID,
            wintypes.DWORD,
            wintypes.LPDWORD,
        ]
        kernel32.QueryInformationJobObject.restype = wintypes.BOOL
        kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
        self._ctypes = ctypes
        self._kernel32 = kernel32
        self._handle = kernel32.CreateJobObjectW(None, None)
        if not self._handle:
            raise OSError(ctypes.get_last_error(), "CreateJobObjectW failed")

    def assign(self, process: subprocess.Popen) -> None:
        if not self._kernel32.AssignProcessToJobObject(
            self._handle, int(process._handle)
        ):
            raise OSError(
                self._ctypes.get_last_error(), "AssignProcessToJobObject failed"
            )

    def totals(self) -> dict:
        ctypes = self._ctypes
        from ctypes import wintypes

        class Accounting(ctypes.Structure):
            _fields_ = (
                ("TotalUserTime", ctypes.c_int64),
                ("TotalKernelTime", ctypes.c_int64),
                ("ThisPeriodTotalUserTime", ctypes.c_int64),
                ("ThisPeriodTotalKernelTime", ctypes.c_int64),
                ("TotalPageFaultCount", wintypes.DWORD),
                ("TotalProcesses", wintypes.DWORD),
                ("ActiveProcesses", wintypes.DWORD),
                ("TotalTerminatedProcesses", wintypes.DWORD),
            )

        class BasicLimit(ctypes.Structure):
            _fields_ = (
                ("PerProcessUserTimeLimit", ctypes.c_int64),
                ("PerJobUserTimeLimit", ctypes.c_int64),
                ("LimitFlags", wintypes.DWORD),
                ("MinimumWorkingSetSize", ctypes.c_size_t),
                ("MaximumWorkingSetSize", ctypes.c_size_t),
                ("ActiveProcessLimit", wintypes.DWORD),
                ("Affinity", ctypes.c_size_t),
                ("PriorityClass", wintypes.DWORD),
                ("SchedulingClass", wintypes.DWORD),
            )

        class IoCounters(ctypes.Structure):
            _fields_ = tuple(
                (name, ctypes.c_uint64)
                for name in (
                    "ReadOperationCount",
                    "WriteOperationCount",
                    "OtherOperationCount",
                    "ReadTransferCount",
                    "WriteTransferCount",
                    "OtherTransferCount",
                )
            )

        class ExtendedLimit(ctypes.Structure):
            _fields_ = (
                ("BasicLimitInformation", BasicLimit),
                ("IoInfo", IoCounters),
                ("ProcessMemoryLimit", ctypes.c_size_t),
                ("JobMemoryLimit", ctypes.c_size_t),
                ("PeakProcessMemoryUsed", ctypes.c_size_t),
                ("PeakJobMemoryUsed", ctypes.c_size_t),
            )

        accounting, extended = Accounting(), ExtendedLimit()
        # JobObjectBasicAccountingInformation = 1, JobObjectExtendedLimitInformation = 9.
        for info_class, info in ((1, accounting), (9, extended)):
            if not self._kernel32.QueryInformationJobObject(
                self._handle, info_class, ctypes.byref(info), ctypes.sizeof(info), None
            ):
                raise OSError(
                    ctypes.get_last_error(), "QueryInformationJobObject failed"
                )
        return {
            "cpu_seconds": round(
                (accounting.TotalUserTime + accounting.TotalKernelTime) / 1e7, 2
            ),
            "job_processes": accounting.TotalProcesses,
            "peak_process_commit_mb": round(extended.PeakProcessMemoryUsed / 2**20),
        }

    def close(self) -> None:
        self._kernel32.CloseHandle(self._handle)


def parse_unit_data(html: str) -> list[dict]:
    marker = "UNIT_DATA = "
    start = html.find(marker)
    if start < 0:
        raise ValueError("timing report has no UNIT_DATA")
    decoder = json.JSONDecoder()
    units, _ = decoder.raw_decode(html, start + len(marker))
    return units


def unit_key(unit: dict) -> str:
    target = unit.get("target", "").strip()
    return f"{unit['name']} {target}" if target else unit["name"]


def section_seconds(unit: dict, name: str) -> float | None:
    for section, span in unit.get("sections") or []:
        if section == name:
            return round(float(span["end"]) - float(span["start"]), 2)
    return None


def summarize_units(units: list[dict]) -> dict:
    rows = []
    for unit in units:
        start = float(unit.get("start", 0.0))
        duration = float(unit.get("duration", 0.0))
        rows.append(
            {
                "unit": unit_key(unit),
                "name": unit["name"],
                "start": round(start, 2),
                "duration": round(duration, 2),
                "end": round(start + duration, 2),
                "frontend": section_seconds(unit, "frontend"),
                "codegen": section_seconds(unit, "codegen"),
            }
        )
    rows.sort(key=lambda row: row["end"])
    makespan = max((row["end"] for row in rows), default=0.0)
    return {"units": rows, "makespan": round(makespan, 2), "unit_count": len(rows)}


def run(args: argparse.Namespace) -> int:
    codex_rs = args.codex_rs.resolve()
    target_dir = args.target_dir.resolve()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    for suffix in (".log", ".html", ".json"):
        if (out / f"{args.label}{suffix}").exists():
            raise SystemExit(f"refusing to overwrite {out / (args.label + suffix)}")
    cargo_args = list(args.cargo_args)
    if cargo_args and cargo_args[0] == "--":
        cargo_args = cargo_args[1:]
    if not cargo_args:
        raise SystemExit("missing cargo arguments")

    env = dict(os.environ)
    env.update(local_rust_env(env, repo_root=codex_rs.parent, which=shutil.which))
    env.pop("CARGO_TARGET_DIR", None)

    log_path = out / f"{args.label}.log"
    with log_path.open("w", encoding="utf-8") as log:
        for package in args.clean:
            command = ["cargo", "clean", "-p", package, "--target-dir", str(target_dir)]
            log.write(f"$ {' '.join(command)}\n")
            log.flush()
            subprocess.run(
                command,
                cwd=codex_rs,
                env=env,
                stdout=log,
                stderr=subprocess.STDOUT,
                check=True,
            )
        for touched in args.touch:
            os.utime(codex_rs / touched, None)
            log.write(f"touched {touched}\n")

        timings_dir = target_dir / "cargo-timings"
        before = (
            set(timings_dir.glob("cargo-timing-*.html"))
            if timings_dir.exists()
            else set()
        )
        command = ["cargo", *cargo_args, "--timings", "--target-dir", str(target_dir)]
        log.write(f"$ {' '.join(command)}\n")
        log.flush()

        samples: list[tuple[float, int | None]] = []
        stop = threading.Event()
        started = time.perf_counter()

        def sampler() -> None:
            while not stop.is_set():
                samples.append(
                    (round(time.perf_counter() - started, 1), rustc_process_count())
                )
                stop.wait(SAMPLE_SECONDS)

        job = None
        if os.name == "nt":
            try:
                job = JobAccounting()
            except OSError as error:
                log.write(f"job accounting unavailable: {error}\n")
        thread = threading.Thread(target=sampler, daemon=True)
        started_at = dt.datetime.now().astimezone().isoformat(timespec="seconds")
        thread.start()
        process = subprocess.Popen(
            command, cwd=codex_rs, env=env, stdout=log, stderr=subprocess.STDOUT
        )
        if job is not None:
            try:
                job.assign(process)
            except OSError as error:
                job.close()
                job = None
                log.write(f"job accounting unavailable: {error}\n")
        code = process.wait()
        wall = time.perf_counter() - started
        stop.set()
        thread.join()
        after_count = rustc_process_count()
        accounting = {}
        if job is not None:
            accounting = job.totals()
            job.close()
        log.write(f"exit={code} wall={wall:.2f}s {accounting}\n")

    report: dict = {
        "label": args.label,
        "command": command,
        "clean": args.clean,
        "touch": args.touch,
        "started_at": started_at,
        "exit_code": code,
        "wall_seconds": round(wall, 2),
        "rustc_samples": samples,
        "rustc_after_exit": after_count,
        **accounting,
    }
    new_reports = (
        sorted(set(timings_dir.glob("cargo-timing-*.html")) - before)
        if timings_dir.exists()
        else []
    )
    if new_reports:
        html_path = out / f"{args.label}.html"
        shutil.copyfile(new_reports[-1], html_path)
        report.update(
            summarize_units(parse_unit_data(html_path.read_text(encoding="utf-8")))
        )
    (out / f"{args.label}.json").write_text(
        json.dumps(report, indent=2) + "\n", encoding="utf-8"
    )
    print_summary(report, args.unit)
    return code


def print_summary(report: dict, focus: list[str]) -> None:
    busy = [count for _, count in report.get("rustc_samples", []) if count is not None]
    print(
        f"{report['label']}: exit={report['exit_code']} wall={report['wall_seconds']}s "
        f"cpu={report.get('cpu_seconds')}s peak-commit={report.get('peak_process_commit_mb')}MB "
        f"makespan={report.get('makespan')}s units={report.get('unit_count')} "
        f"rustc processes max={max(busy, default=None)} after-exit={report.get('rustc_after_exit')}"
    )
    units = report.get("units", [])
    shown = (
        [row for row in units if any(name == row["name"] for name in focus)]
        if focus
        else []
    )
    longest = sorted(units, key=lambda row: -row["duration"])[:8]
    for row in shown + [row for row in longest if row not in shown]:
        print(
            f"  {row['unit']:<60} start={row['start']:>8.2f} dur={row['duration']:>8.2f} "
            f"frontend={row.get('frontend')} codegen={row.get('codegen')} end={row['end']:>8.2f}"
        )


def summarize(args: argparse.Namespace) -> int:
    report = summarize_units(parse_unit_data(args.html.read_text(encoding="utf-8")))
    report.update({"label": args.html.stem, "exit_code": None, "wall_seconds": None})
    print_summary(report, args.unit)
    return 0


def load_report(path: pathlib.Path) -> dict:
    report = json.loads(path.read_text(encoding="utf-8"))
    html_path = path.with_suffix(".html")
    if html_path.exists():
        report.update(
            summarize_units(parse_unit_data(html_path.read_text(encoding="utf-8")))
        )
    return report


def compare(args: argparse.Namespace) -> int:
    before = load_report(args.before)
    after = load_report(args.after)
    print(
        f"wall: {before['wall_seconds']}s -> {after['wall_seconds']}s; "
        f"makespan: {before.get('makespan')}s -> {after.get('makespan')}s"
    )
    index_before = {row["unit"]: row for row in before.get("units", [])}
    index_after = {row["unit"]: row for row in after.get("units", [])}
    names = args.unit or sorted(
        {row["name"] for row in before.get("units", []) + after.get("units", [])}
    )
    for key in sorted(set(index_before) | set(index_after)):
        row_b, row_a = index_before.get(key), index_after.get(key)
        name = (row_b or row_a)["name"]
        if name not in names:
            continue

        def cell(row: dict | None) -> str:
            if row is None:
                return "-"
            return (
                f"{row['duration']} (fe {row.get('frontend')}, cg {row.get('codegen')})"
            )

        print(f"  {key:<50} {cell(row_b):>28} -> {cell(row_a)}")
    return 0


SEQUENTIAL_LABEL = re.compile(
    r"^(before|after)(?:-r\d+)?-(0a|0b|1a|1b|2|w|3|4|5[ab]?)-"
)
SEQUENTIAL_STEPS = {
    "0a": "cold build",
    "0b": "cold lib-test build",
    "1": "core from scratch, build",
    "2": "edit turn.rs, build",
    "w": "lib-test build after 1-2",
    "3": "edit turn.rs, lib-test build",
    "4": "edit validation.rs, build",
    "5": "edit validation.rs, build its own unit tests",
}
PAIRED_LABEL = re.compile(r"^(before|after)-([1-4]-[a-z-]+)-r(\d+)$")
PAIRED_STEPS = {
    "1-scratch-build": "core from scratch, build",
    "2-edit-turn-build": "edit turn.rs, build",
    "3-edit-turn-libtest": "edit turn.rs, lib-test build",
    "4-scratch-libtest": "core from scratch, lib-test build",
}


def run_metrics(report: dict) -> dict:
    units = report.get("units", [])
    core = next((row for row in units if row["unit"] == "codex-core"), None)
    lib_test = next(
        (row for row in units if row["unit"].endswith('"lib" (test)')), None
    )
    window = []
    if core and core["duration"]:
        window = [
            count
            for ts, count in report.get("rustc_samples", [])
            if count is not None and core["start"] <= ts <= core["end"]
        ]
    return {
        "wall": report["wall_seconds"],
        "cpu": report.get("cpu_seconds"),
        "core": core["duration"] if core and core["duration"] else None,
        "lib_test": lib_test["duration"] if lib_test else None,
        "lib_test_start": lib_test["start"] if lib_test else None,
        # Other compilers running while codex-core compiled (its own rustc excluded).
        "others": round(sum(window) / len(window) - 1, 1) if window else None,
        "commit": report.get("peak_process_commit_mb"),
    }


def load_results(
    directory: pathlib.Path, pattern: re.Pattern
) -> list[tuple[re.Match, dict]]:
    found = []
    for path in sorted(directory.glob("*.json")):
        if path.stem.endswith("-warmup"):
            continue
        report = load_report(path)
        match = pattern.match(report.get("label", ""))
        if match:
            found.append((match, run_metrics(report)))
    return found


def fmt(value: float) -> str:
    return f"{value:.1f}" if isinstance(value, float) else str(value)


def rounds(args: argparse.Namespace) -> int:
    groups: dict[str, dict[str, list[dict]]] = {}
    for match, metrics in load_results(args.results, SEQUENTIAL_LABEL):
        step = {"1a": "1", "1b": "1", "5a": "5", "5b": "5"}.get(
            match.group(2), match.group(2)
        )
        groups.setdefault(step, {}).setdefault(match.group(1), []).append(metrics)
    print("| Step | Metric | Before | After | Median change |")
    print("|---|---|---|---|---|")
    for step, name in SEQUENTIAL_STEPS.items():
        before, after = (
            groups.get(step, {}).get("before", []),
            groups.get(step, {}).get("after", []),
        )
        for key, metric in (
            ("wall", "wall s"),
            ("cpu", "CPU s"),
            ("core", "codex-core unit s"),
            ("lib_test", "lib test unit s"),
            ("lib_test_start", "lib test start s"),
            ("others", "other rustc during core"),
        ):
            b = [row[key] for row in before if row[key] is not None]
            a = [row[key] for row in after if row[key] is not None]
            if not b and not a:
                continue
            change = ""
            if b and a and key != "others":
                mb, ma = statistics.median(b), statistics.median(a)
                change = f"{ma - mb:+.1f} ({(ma - mb) / mb * 100:+.0f}%)"
            print(
                f"| {step} {name} | {metric} | {', '.join(map(fmt, b)) or '-'} | "
                f"{', '.join(map(fmt, a)) or '-'} | {change} |"
            )
    return 0


def paired(args: argparse.Namespace) -> int:
    runs: dict[tuple[str, int], dict[str, dict]] = {}
    for match, metrics in load_results(args.results, PAIRED_LABEL):
        state, step, rep = match.groups()
        runs.setdefault((step, int(rep)), {})[state] = metrics
    print("| Step | Metric | Before | After | Median paired change |")
    print("|---|---|---|---|---|")
    for step, name in PAIRED_STEPS.items():
        complete = [
            runs[key]
            for key in sorted(runs)
            if key[0] == step and {"before", "after"} <= runs[key].keys()
        ]
        for key, metric in (
            ("wall", "wall s"),
            ("cpu", "CPU s"),
            ("core", "codex-core unit s"),
            ("lib_test", "lib test unit s"),
            ("lib_test_start", "lib test start s"),
            ("commit", "peak commit MB"),
        ):
            pairs = [
                (run["before"][key], run["after"][key])
                for run in complete
                if run["before"][key] is not None and run["after"][key] is not None
            ]
            if not pairs:
                continue
            absolute = statistics.median(a - b for b, a in pairs)
            relative = statistics.median((a - b) / b * 100 for b, a in pairs)
            print(
                f"| {name} | {metric} | {', '.join(fmt(b) for b, _ in pairs)} | "
                f"{', '.join(fmt(a) for _, a in pairs)} | {absolute:+.1f} ({relative:+.1f}%) |"
            )
    return 0


def probe(args: argparse.Namespace) -> int:
    text = args.file.read_text(encoding="utf-8", newline="")
    if text.count(args.find) != 1:
        raise SystemExit(
            f"expected exactly one occurrence of {args.find!r} in {args.file}"
        )
    args.file.write_text(
        text.replace(args.find, args.replace), encoding="utf-8", newline=""
    )
    print(f"probe edit applied to {args.file}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    sub = parser.add_subparsers(dest="cmd", required=True)
    probe_parser = sub.add_parser(
        "probe", help="exact single replacement used as a benchmark source edit"
    )
    probe_parser.add_argument("--file", required=True, type=pathlib.Path)
    probe_parser.add_argument("--find", required=True)
    probe_parser.add_argument("--replace", required=True)
    run_parser = sub.add_parser("run")
    run_parser.add_argument("--codex-rs", required=True, type=pathlib.Path)
    run_parser.add_argument("--target-dir", required=True, type=pathlib.Path)
    run_parser.add_argument("--out", required=True, type=pathlib.Path)
    run_parser.add_argument("--label", required=True)
    run_parser.add_argument("--clean", action="append", default=[])
    run_parser.add_argument("--touch", action="append", default=[])
    run_parser.add_argument("--unit", action="append", default=[])
    run_parser.add_argument("cargo_args", nargs=argparse.REMAINDER)
    summarize_parser = sub.add_parser("summarize")
    summarize_parser.add_argument("html", type=pathlib.Path)
    summarize_parser.add_argument("--unit", action="append", default=[])
    compare_parser = sub.add_parser("compare")
    compare_parser.add_argument("before", type=pathlib.Path)
    compare_parser.add_argument("after", type=pathlib.Path)
    compare_parser.add_argument("--unit", action="append", default=[])
    for name in ("rounds", "paired"):
        sub.add_parser(name).add_argument("results", type=pathlib.Path)
    args = parser.parse_args()
    commands = {
        "run": run,
        "summarize": summarize,
        "compare": compare,
        "probe": probe,
        "rounds": rounds,
        "paired": paired,
    }
    return commands[args.cmd](args)


if __name__ == "__main__":
    raise SystemExit(main())
