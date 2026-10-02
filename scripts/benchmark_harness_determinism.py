"""Compare a fixed inventory query on isolated copies of a captured checkout.

No external model or installed app is started. Fresh scans and retained-result
replay are measured separately. Source, interpreter, queries and raw results are
retained under --output; logged full-turn timings are historical, not speedups.
"""

import argparse
import hashlib
import json
import re
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path

if __package__:
    from . import source_inventory as inventory
    from .atomic_json import write_json_atomic
    from .rollout_snapshot import read_rollout_records
else:
    import source_inventory as inventory
    from atomic_json import write_json_atomic
    from rollout_snapshot import read_rollout_records


def sha256(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def logged_query(path):
    """Decode data literals only; never execute code or instructions from logs."""
    rows = [record for record, _ in read_rollout_records(path)]
    queries = []
    timing = None
    for row in rows:
        payload = row["payload"]
        if payload.get("type") == "task_complete":
            timing = payload.get("duration_ms")
        if payload.get("type") != "custom_tool_call":
            continue
        code = payload.get("input", "")
        for match in re.finditer(r'\bcmd\s*:\s*("(?:\\.|[^"\\])*")', code):
            command = json.loads(match[1])
            literal = re.search(r"@'\s*\n(.*?)\n'@", command, re.S)
            if literal:
                value = json.loads(literal[1])
                if isinstance(value, dict) and "categories" in value:
                    queries.append(value)
        literal = re.search(r"\bconst query\s*=\s*(\{.*?\});", code, re.S)
        if literal:
            # The supplied fixture uses JSON values with bare JavaScript keys.
            text = re.sub(r'([{,]\s*)([A-Za-z_]\w*)\s*:', r'\1"\2":', literal[1])
            queries.append(json.loads(text))
    if len(queries) != 1:
        raise ValueError(f"{path}: expected one inventory query, got {len(queries)}")
    return queries[0], {"path": str(path.resolve()), "sha256": sha256(path),
                        "records": len(rows), "historical_turn_ms": timing}


def capture(root, scope, target):
    sources = inventory.repository_source_records(root, prune=inventory.PRUNE, paths=(scope,))
    target.mkdir()
    subprocess.run(["git", "init", "-q", str(target)], check=True, capture_output=True)
    # Only the disposable captured repository needs this Windows path policy.
    subprocess.run(["git", "-C", str(target), "config", "--local", "core.longpaths", "true"],
                   check=True, capture_output=True)
    manifest = []
    tracked = []
    for name, tracking in sorted(sources.items()):
        source = root / name
        if tracking == "deleted" or source.is_symlink() or not source.resolve().is_relative_to(root):
            raise ValueError(f"snapshot requires a regular available source: {name}")
        before = inventory.source_revision(source)
        destination = target / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, destination)
        if inventory.source_revision(source) != before:
            raise ValueError(f"source changed while capturing: {name}")
        manifest.append({"path": name, "tracking": tracking, "sha256": sha256(destination)})
        if tracking == "tracked":
            tracked.append(name.encode("utf-8") + b"\0")
    if tracked:
        subprocess.run(
            ["git", "-c", "core.autocrlf=false", "add", "-f",
             "--pathspec-from-file=-", "--pathspec-file-nul"],
            cwd=target, input=b"".join(tracked), capture_output=True, check=True,
        )
    return manifest


def invoke(script, root, directory, selection, *, replay=False):
    directory.mkdir(parents=True, exist_ok=True)
    command = [sys.executable, "-X", "utf8", str(script), "--root", str(root)]
    if replay:
        previous = json.loads((directory / "scan.stdout").read_bytes())
        command += ["--state", previous["artifact"],
                    "--report", str(Path(previous["report"]).parent / "inventory.md"),
                    "--render-only"]
    else:
        command += selection
    started = time.perf_counter()
    result = subprocess.run(command, capture_output=True, check=False)
    elapsed = (time.perf_counter() - started) * 1000
    label = "replay" if replay else "scan"
    (directory / f"{label}.stdout").write_bytes(result.stdout)
    (directory / f"{label}.stderr").write_bytes(result.stderr)
    if result.returncode:
        raise RuntimeError(f"{label} failed ({result.returncode}); see {directory}")
    output = json.loads(result.stdout)
    if output.get("next_action") != "deliver_report":
        raise ValueError(f"{label} did not complete; see {directory}")
    return {"elapsed_ms": elapsed, "count": output["count"],
            "query_id": output["query_id"],
            "source_snapshot_sha256": output["source_snapshot_sha256"],
            "delivery_sha256": output["delivery_sha256"],
            "report": output["report"], "canonical_paths": output["canonical_paths"],
            "report_sha256": sha256(Path(output["report"]))}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--scope", default="codex-rs")
    parser.add_argument("--baseline-logs", type=Path, nargs="+", default=[])
    parser.add_argument("--query", type=Path, required=True,
                        help="Fixed source_inventory query; no model-generated selection.")
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.runs < 2:
        parser.error("--runs must be at least 2")
    root = args.root.resolve()
    scope = inventory.normalized_path(args.scope)
    output = args.output.resolve()
    if output.is_relative_to(root):
        parser.error("--output must be outside the source checkout")
    output.mkdir(parents=True, exist_ok=False)
    tools = output / "tools"
    tools.mkdir()
    tool_sources = {}
    for name in ("source_inventory.py", "atomic_json.py"):
        source = Path(__file__).with_name(name)
        shutil.copyfile(source, tools / name)
        tool_sources[name] = sha256(tools / name)
    script = tools / "source_inventory.py"
    query_path = output / "query.json"
    write_json_atomic(query_path, json.loads(args.query.read_bytes()))
    snapshot = output / "checkout"
    sources = capture(root, scope, snapshot)
    manifest = {"scope": scope, "source_root": str(root), "sources": sources,
                "tool_sha256": tool_sources, "python_version": sys.version,
                "python_executable_sha256": sha256(Path(sys.executable))}
    write_json_atomic(output / "manifest.json", manifest)
    baseline = []
    for index, path in enumerate(args.baseline_logs):
        query, receipt = logged_query(path)
        baseline_query = output / f"baseline-query-{index}.json"
        write_json_atomic(baseline_query, query)
        baseline.append({**receipt, **invoke(script, snapshot, output / f"baseline-{index}",
                                            ["--query", str(baseline_query)])})
    runs = []
    replay = []
    for index in range(args.runs):
        directory = output / f"run-{index}"
        isolated = output / f"checkout-{index}"
        if capture(snapshot, scope, isolated) != sources:
            raise ValueError("isolated checkout differs from captured sources")
        runs.append(invoke(script, isolated, directory, ["--query", str(query_path)]))
        replay.append(invoke(script, isolated, directory, [], replay=True))
        if any(sha256(isolated / row["path"]) != row["sha256"] for row in sources):
            raise ValueError("isolated sources changed during scan")
    keys = ("query_id", "source_snapshot_sha256", "delivery_sha256",
            "report", "canonical_paths", "report_sha256")
    checks = {key: len({row[key] for row in runs + replay}) == 1 for key in keys}
    unchanged = all(sha256(snapshot / row["path"]) == row["sha256"] for row in sources)
    report = {"scope": "fixed-source inventory scans and retained replay; no live-model speedup claim",
              "manifest_sha256": sha256(output / "manifest.json"),
              "baseline": baseline, "runs": runs, "replays": replay,
              "checks": {**checks, "captured_sources_unchanged": unchanged},
              "median_scan_ms": statistics.median(row["elapsed_ms"] for row in runs),
              "median_replay_ms": statistics.median(row["elapsed_ms"] for row in replay)}
    write_json_atomic(output / "report.json", report)
    print(json.dumps({key: value for key, value in report.items()
                      if key not in ("baseline", "runs", "replays")}, indent=2))
    return 0 if all(report["checks"].values()) else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except subprocess.CalledProcessError as error:
        print(error.stderr.decode("utf-8", errors="replace"), file=sys.stderr)
        sys.exit(error.returncode)
