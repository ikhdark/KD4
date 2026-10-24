#!/usr/bin/env python3
"""Compare finished KD4 executables on an identical, real CLI coding workflow.

No build duration enters the score. See lto-runtime-benchmark.md for scope.
"""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import os
from pathlib import Path
import shlex
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from scripts.common.atomic_json import write_json_atomic
from scripts.common.generated_output_lock import GenerationLockError, repository_lock
from scripts.common.process_owner import run_owned

ROOT = Path(__file__).resolve().parents[2]
EXE = ".exe" if os.name == "nt" else ""
PROGRAMS = ("codex" + EXE, "codex-code-mode-host" + EXE)
ANSWER = "KD4 benchmark: category totals fixed and all six tests passed."
TEST_MARKER = "KD4_BENCH_SIX_TESTS_OK"
BEFORE = """def summarize(rows):
    totals = {}
    for category, amount in rows:
        totals[category] = amount
    return totals
"""
AFTER = BEFORE.replace(
    "totals[category] = amount", "totals[category] = totals.get(category, 0) + amount"
)
TESTS = """import unittest
from calc import summarize

class TotalsTests(unittest.TestCase):
    def test_empty(self):
        self.assertEqual(summarize([]), {})
    def test_single(self):
        self.assertEqual(summarize([("a", 7)]), {"a": 7})
    def test_repeated(self):
        self.assertEqual(summarize([("a", 2), ("a", 3)]), {"a": 5})
    def test_interleaved(self):
        self.assertEqual(summarize([("a", 2), ("b", 8), ("a", 3)]), {"a": 5, "b": 8})
    def test_negative(self):
        self.assertEqual(summarize([("a", 5), ("a", -7)]), {"a": -2})
    def test_zero(self):
        self.assertEqual(summarize([("a", 3), ("a", 0)]), {"a": 3})
"""
PATCH = """*** Begin Patch
*** Update File: calc.py
@@
-        totals[category] = amount
+        totals[category] = totals.get(category, 0) + amount
*** End Patch"""


def workflow_scripts():
    # These are model-issued calls, not Python-side substitutes for KD4 tools.
    test_source = (
        "import sys, unittest; "
        "suite = unittest.defaultTestLoader.loadTestsFromName('test_calc'); "
        "result = unittest.TextTestRunner().run(suite); "
        f"print('{TEST_MARKER}' if result.wasSuccessful() and result.testsRun == 6 else 'FAILED'); "
        "sys.exit(0 if result.wasSuccessful() and result.testsRun == 6 else 1)"
    )
    argv = [sys.executable, "-B", "-c", test_source]
    command = subprocess.list2cmdline(argv) if os.name == "nt" else shlex.join(argv)
    if os.name == "nt":
        # exec_command uses PowerShell; cmd.exe's quoting rules are different.
        command = "& " + " ".join("'" + arg.replace("'", "''") + "'" for arg in argv)
    return [
        'for (const r of await Promise.all([tools.read_file({path:"calc.py"}), '
        'tools.read_file({path:"test_calc.py"})])) text(r);',
        f"const patch = await tools.apply_patch({json.dumps(PATCH)}); "
        'if (!patch.success) throw Error(JSON.stringify(patch)); text("PATCH_OK");',
        f"const r = await tools.exec_command({json.dumps({'cmd': command})}); "
        f"if (r.exit_code !== 0 || !r.output.includes({json.dumps(TEST_MARKER)})) "
        "throw Error(JSON.stringify(r)); text(r.output);",
    ]


def tool_output(request, call_id):
    outputs = [
        item
        for item in request.get("input", [])
        if item.get("type") == "custom_tool_call_output"
        and item.get("call_id") == call_id
    ]
    if len(outputs) != 1:
        raise ValueError(f"expected exactly one result for {call_id}")
    output = outputs[0].get("output")
    text = json.dumps(output, ensure_ascii=False)
    if (
        (isinstance(output, dict) and output.get("success") is False)
        or "Script failed with cell ID" in text
        or "Script error:" in text
    ):
        raise ValueError(f"failed Code Mode cell {call_id}: {text[:2000]}")
    return text


class CodingProvider:
    """Four fixed responses; never let a failed tool result become a success."""

    def __init__(self):
        self.scripts = workflow_scripts()
        self.requests = 0
        self.error = None

    def respond(self, request):
        stage = self.requests
        self.requests += 1
        if stage > 3:
            raise ValueError("extra generation/retry: workloads are no longer matched")
        if stage:
            output = tool_output(request, f"step-{stage - 1}")
            required = [
                ("def summarize", "test_interleaved"),
                ("PATCH_OK",),
                (TEST_MARKER,),
            ][stage - 1]
            if not all(marker in output for marker in required):
                raise ValueError(
                    f"step {stage - 1} did not deliver its required result: {output[:2000]}"
                )
        item = (
            {
                "type": "custom_tool_call",
                "call_id": f"step-{stage}",
                "name": "exec",
                "input": self.scripts[stage],
            }
            if stage < 3
            else {
                "type": "message",
                "role": "assistant",
                "id": "answer",
                "content": [{"type": "output_text", "text": ANSWER}],
            }
        )
        events = [
            {"type": "response.created", "response": {"id": f"response-{stage}"}},
            {"type": "response.output_item.done", "item": item},
            {
                "type": "response.completed",
                "response": {
                    "id": f"response-{stage}",
                    "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0},
                },
            },
        ]
        return "".join(
            f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events
        ).encode()


@contextmanager
def serve_provider(provider, io_timeout=5):
    class Server(HTTPServer):
        def get_request(self):
            connection, address = super().get_request()
            connection.settimeout(io_timeout)
            return connection, address

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            try:
                size = int(self.headers.get("Content-Length", "0"))
                if self.path != "/v1/responses" or not 0 < size <= 16 * 1024 * 1024:
                    raise ValueError("unexpected provider request")
                body = provider.respond(json.loads(self.rfile.read(size)))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            except Exception as error:
                provider.error = str(error)
                self.send_error(400, "invalid benchmark workflow")

        def log_message(self, *_args):
            pass

    server = Server(("127.0.0.1", 0), Handler)
    server.timeout = 1
    thread = threading.Thread(
        target=server.serve_forever, kwargs={"poll_interval": 0.05}
    )
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}/v1"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=10)
        if thread.is_alive():
            raise RuntimeError("benchmark provider did not stop")


def verify_result(result, provider, workspace):
    if result.returncode or provider.error or provider.requests != 4:
        raise ValueError(
            f"coding workflow failed: exit={result.returncode}, requests={provider.requests}, provider={provider.error}\n{result.stderr[-8000:]}"
        )
    events = [json.loads(line) for line in result.stdout.splitlines() if line.strip()]
    answers = [
        e["item"].get("text")
        for e in events
        if e.get("type") == "item.completed"
        and e.get("item", {}).get("type") == "agent_message"
    ]
    if (
        answers != [ANSWER]
        or sum(e.get("type") == "turn.completed" for e in events) != 1
    ):
        raise ValueError("missing or duplicate final answer/turn completion")
    if any(e.get("type") in ("error", "turn.failed") for e in events):
        raise ValueError("CLI emitted an error")
    if (workspace / "calc.py").read_text(encoding="utf-8") != AFTER:
        raise ValueError("real apply_patch did not produce the exact required source")
    if (workspace / "test_calc.py").read_text(encoding="utf-8") != TESTS:
        raise ValueError("the workload's test oracle was modified")
    # Independent computation outside KD4; never trust the scripted final answer.
    oracle = run_owned(
        [sys.executable, "-B", "-m", "unittest", "-q", "test_calc"],
        cwd=workspace,
        timeout=30,
        capture_output=True,
        text=True,
    )
    if oracle.returncode or "Ran 6 tests" not in oracle.stderr:
        raise ValueError(f"independent fixture verification failed: {oracle.stderr}")
    return timing_metrics(next(e for e in events if e.get("type") == "turn.completed"))


def timing_metrics(completed):
    """Existing core elapsed-time diagnostics, never reconstructed CPU time."""
    fields = {
        "core_turn_seconds": ("inclusiveDurationNs",),
        "orchestration_seconds": ("exclusive", "orchestrationNs"),
        "stream_processing_seconds": ("unions", "modelStreamProcessingUnionNs"),
    }
    timing = completed.get("timing")
    if timing is not None and (
        not isinstance(timing, dict)
        or timing.get("profileValid") is not True
        or timing.get("classificationComplete") is not True
    ):
        raise ValueError("core timing is invalid or incompletely classified")
    metrics = {}
    for name, path in fields.items():
        value = timing
        for part in path:
            value = value.get(part) if isinstance(value, dict) else None
        if value is not None and (type(value) is not int or value < 0):
            raise ValueError(f"invalid timing field: {name}")
        metrics[name] = None if value is None else value / 1_000_000_000
    return metrics


def measure(bundle, prompt_bytes, timeout, catalog):
    with tempfile.TemporaryDirectory(prefix="kd4-lto-task-") as temp:
        base = Path(temp)
        workspace, home = base / "work", base / "home"
        workspace.mkdir()
        home.mkdir()
        (workspace / "calc.py").write_text(BEFORE, encoding="utf-8")
        (workspace / "test_calc.py").write_text(TESTS, encoding="utf-8")
        catalog_path = base / "models.json"
        catalog_path.write_text(catalog, encoding="utf-8")
        model = next(
            model["slug"]
            for model in json.loads(catalog)["models"]
            if model.get("apply_patch_tool_type") == "freeform"
        )
        provider = CodingProvider()
        with serve_provider(provider) as url:
            config = (
                f'model_providers.lto_bench={{name="Local benchmark",base_url={json.dumps(url)},'
                'wire_api="responses",requires_openai_auth=false,supports_websockets=false,'
                "request_max_retries=0,stream_max_retries=0}"
            )
            env = {
                key: value
                for key, value in os.environ.items()
                if not key.startswith(("CODEX_", "OPENAI_"))
            }
            env.update(
                CODEX_HOME=str(home),
                CODEX_SQLITE_HOME=str(home),
                CODEX_CODE_MODE_HOST_PATH=str(bundle / PROGRAMS[1]),
            )
            argv = [
                str(bundle / PROGRAMS[0]),
                "exec",
                "--ignore-user-config",
                "--ignore-rules",
                "--skip-git-repo-check",
                "--json",
                "--color",
                "never",
                "--sandbox",
                "danger-full-access",
                "-C",
                str(workspace),
            ]
            for override in [
                config,
                'model_provider="lto_bench"',
                f"model={json.dumps(model)}",
                f"model_catalog_json={json.dumps(str(catalog_path))}",
                'approval_policy="never"',
                "features.code_mode=true",
                "features.code_mode_host=true",
                "features.code_mode_only=true",
                "features.apps=false",
                'web_search="disabled"',
                "model_context_window=2000000",
                "model_auto_compact_token_limit=1900000",
            ]:
                argv += ["-c", override]
            prompt = (
                "Fix summarize in calc.py to sum repeated categories. Read both files, patch only calc.py, and run all six tests.\n"
                + "x" * prompt_bytes
            )
            argv.append("-")
            prompt_path = base / "prompt.txt"
            prompt_path.write_text(prompt, encoding="utf-8")
            with prompt_path.open(encoding="utf-8") as stdin:
                start = time.perf_counter()
                result = run_owned(
                    argv,
                    cwd=workspace,
                    env=env,
                    stdin=stdin,
                    timeout=timeout,
                    capture_output=True,
                    text=True,
                    encoding="utf-8",
                )
                elapsed = time.perf_counter() - start
        metrics = verify_result(result, provider, workspace)
        return {"seconds": elapsed, **metrics}


def source_fingerprint(root=ROOT):
    digest = hashlib.sha256()
    for tree in (root / "codex-rs", root / "third_party"):
        for parent, dirs, files in os.walk(tree):
            dirs[:] = sorted(
                d for d in dirs if d not in ("target", ".git", "__pycache__")
            )
            for name in sorted(files):
                path = Path(parent) / name
                digest.update(path.relative_to(root).as_posix().encode() + b"\0")
                digest.update(hashlib.sha256(path.read_bytes()).digest())
    return digest.hexdigest()


def build_command(lto, codegen_units, opt_level=3, name=None):
    profile = f"lto-runtime-{name or lto}"
    command = ["cargo", "--config", f'profile.{profile}.inherits="local-release"']
    # None means inherit the current profile, not a hard-coded approximation.
    for key, value in (
        ("lto", lto),
        ("codegen-units", codegen_units),
        ("opt-level", opt_level),
        ("incremental", False if lto is not None else None),
    ):
        if value is not None:
            command += ["--config", f"profile.{profile}.{key}={json.dumps(value)}"]
    return command + [
        "build",
        "--locked",
        "--profile",
        profile,
        "-p",
        "codex-cli",
        "-p",
        "codex-code-mode-host",
        "--bin",
        "codex",
        "--bin",
        "codex-code-mode-host",
    ]


def snapshot_sources(destination, root=ROOT):
    before = source_fingerprint(root)
    for name in ("codex-rs", "third_party"):
        shutil.copytree(
            root / name,
            destination / name,
            ignore=shutil.ignore_patterns("target", ".git", "__pycache__"),
        )
    if source_fingerprint(root) != before or source_fingerprint(destination) != before:
        raise ValueError("source changed while staging; no comparison was started")
    return before


def variants(matrix, codegen_units):
    baseline = dict(
        name="baseline", lto=None, codegen_units=None, opt_level=None, cpu="default"
    )
    result = [
        baseline,
        dict(name="fat", lto="fat", codegen_units=codegen_units, opt_level=3, cpu="default"),
    ]
    if matrix == "screening":
        fat = result[-1]
        result.insert(1, {**baseline, "name": "native", "cpu": "native"})
        if codegen_units != 1:
            result.append({**fat, "name": "fat-cgu1", "codegen_units": 1})
        result.append({**baseline, "name": "opt2", "opt_level": 2})
    return result


def build_environment(spec, host, inherited=None):
    env = dict(os.environ if inherited is None else inherited)
    for key in list(env):
        if key.startswith("CARGO_PROFILE_") or key == "CARGO_INCREMENTAL":
            del env[key]
    if spec["cpu"] == "native":
        if "RUSTFLAGS" in env or "CARGO_ENCODED_RUSTFLAGS" in env:
            raise ValueError(
                "native comparison requires preserving target-specific Rust flags; unset global RUSTFLAGS/CARGO_ENCODED_RUSTFLAGS"
            )
        key = "CARGO_TARGET_" + host.upper().replace("-", "_") + "_RUSTFLAGS"
        env[key] = (env.get(key, "") + " -C target-cpu=native").strip()
    return env


def build_in_lane(spec, host, destination, source_root):
    target = Path(os.environ["CODEX_CARGO_LANE_TARGET_DIR"])
    command = build_command(
        spec["lto"], spec["codegen_units"], spec["opt_level"], spec["name"]
    ) + ["--target-dir", str(target)]
    subprocess.run(
        command,
        cwd=source_root / "codex-rs",
        env=build_environment(spec, host),
        check=True,
    )
    destination.mkdir(exist_ok=True)
    for name in PROGRAMS:
        shutil.copy2(target / f"lto-runtime-{spec['name']}" / name, destination / name)


def binary_identities(bundle):
    return {
        name: hashlib.sha256((bundle / name).read_bytes()).hexdigest()
        for name in PROGRAMS
    }


def compiler_identity():
    output = run_owned(
        ["rustc", "-vV"],
        cwd=ROOT / "codex-rs",
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    host = next(
        line.removeprefix("host: ")
        for line in output.splitlines()
        if line.startswith("host: ")
    )
    flags = {
        key: value
        for key, value in os.environ.items()
        if key in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTUP_TOOLCHAIN")
        or (key.startswith("CARGO_TARGET_") and key.endswith("_RUSTFLAGS"))
    }
    return {"rustc": output.strip(), "host": host, "flags": flags}


def prepare_experiment(directory, specs, compiler, root=ROOT):
    manifest_path = directory / "experiment.json"
    if manifest_path.exists():
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        if (
            manifest.get("version") != 1
            or manifest.get("variants") != specs
            or manifest.get("compiler") != compiler
        ):
            raise ValueError(
                "experiment settings/compiler changed; use a new experiment directory"
            )
        if source_fingerprint(directory / "source") != manifest["source_sha256"]:
            raise ValueError("frozen experiment source was modified")
        return manifest
    if any(path.name != "run.lock" for path in directory.iterdir()):
        raise ValueError(
            "refusing to adopt a nonempty directory without an experiment manifest"
        )
    source = snapshot_sources(directory / "source", root)
    manifest = dict(
        version=1, variants=specs, compiler=compiler, source_sha256=source, completed={}
    )
    write_json_atomic(manifest_path, manifest)
    return manifest


def build_bundles(directory, specs, compiler):
    manifest = prepare_experiment(directory, specs, compiler)
    bundles = {}
    for spec in specs:
        label = spec["name"]
        destination = directory / label
        if label in manifest["completed"]:
            if binary_identities(destination) != manifest["completed"][label]:
                raise ValueError(f"completed runtime bundle was modified: {label}")
            print(f"reusing verified build: {label}", file=sys.stderr)
        else:
            command = [
                sys.executable,
                "-B",
                str(ROOT / "scripts/build/rust_build_status.py"),
                "run-lane",
                "--lane",
                "lto-runtime",
                "--",
                sys.executable,
                "-B",
                str(Path(__file__).resolve()),
                "_build",
                json.dumps(spec),
                compiler["host"],
                str(destination),
                str(directory / "source"),
            ]
            subprocess.run(
                command,
                cwd=ROOT / "codex-rs",
                env=build_environment({**spec, "cpu": "default"}, compiler["host"]),
                check=True,
            )
            if source_fingerprint(directory / "source") != manifest["source_sha256"]:
                raise ValueError("frozen build inputs changed; comparison invalid")
            manifest["completed"][label] = binary_identities(destination)
            write_json_atomic(directory / "experiment.json", manifest)
        bundles[label] = destination
    return bundles, manifest["source_sha256"]


@contextmanager
def experiment_directory(path):
    if path is None:
        with tempfile.TemporaryDirectory(prefix="kd4-lto-bundles-") as temp:
            yield Path(temp)
    else:
        directory = path.resolve()
        directory.mkdir(parents=True, exist_ok=True)
        with repository_lock(
            directory / "run.lock",
            "KD4 runtime benchmark",
            resource="runtime experiment",
        ):
            yield directory


def ordered_rounds(labels, rounds):
    rotations = [labels[i:] + labels[:i] for i in range(len(labels))]
    orders = rotations + [tuple(reversed(order)) for order in rotations]
    return [orders[index % len(orders)] for index in range(rounds)]


def summarize(samples):
    summary = []
    for label, prompt_bytes in sorted(
        {(s["variant"], s["prompt_bytes"]) for s in samples}
    ):
        times = [
            s["seconds"]
            for s in samples
            if s["variant"] == label
            and s["prompt_bytes"] == prompt_bytes
            and not s["warmup"]
        ]
        if not times:
            raise ValueError("no measured samples")
        row = {
            "variant": label,
            "prompt_bytes": prompt_bytes,
            "samples": len(times),
            "median_seconds": statistics.median(times),
            "min_seconds": min(times),
            "max_seconds": max(times),
        }
        for metric in (
            "core_turn_seconds",
            "orchestration_seconds",
            "stream_processing_seconds",
        ):
            observed = [
                s[metric]
                for s in samples
                if s["variant"] == label
                and s["prompt_bytes"] == prompt_bytes
                and not s["warmup"]
                and s.get(metric) is not None
            ]
            row[metric + "_samples"] = len(observed)
            row["median_" + metric] = statistics.median(observed) if observed else None
        summary.append(row)
    return summary


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    if argv and argv[0] == "_build":
        build_in_lane(json.loads(argv[1]), argv[2], Path(argv[3]), Path(argv[4]))
        return 0
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--bundle",
        action="append",
        default=[],
        metavar="LABEL=DIRECTORY",
        help="Use prebuilt codex + code-mode-host instead of building. Labels do not prove compiler settings.",
    )
    parser.add_argument(
        "--rounds",
        type=int,
        default=6,
        help="Measured rounds; use a multiple of the variant count to balance positions.",
    )
    parser.add_argument("--warmups", type=int, default=1)
    parser.add_argument(
        "--matrix",
        choices=("lto", "screening"),
        default="lto",
        help="Compare current local-release and fat; screening adds baseline/native CPU, fat/cgu1 and baseline/opt-level 2",
    )
    parser.add_argument(
        "--experiment-dir",
        type=Path,
        help="Keep frozen source and verified build identities so interrupted builds can resume; never publishes binaries",
    )
    parser.add_argument(
        "--codegen-units",
        type=int,
        default=16,
        help="Workspace codegen units for fat candidates only; baseline inherits current local-release unchanged.",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=180,
        help="Per-workflow timeout in seconds; failure aborts the comparison.",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Print a compact summary, not raw tool/session logs.",
    )
    args = parser.parse_args(argv)
    if (
        args.rounds < 1
        or args.warmups < 0
        or args.codegen_units < 1
        or args.timeout <= 0
    ):
        parser.error(
            "rounds, codegen-units and timeout must be positive; warmups cannot be negative"
        )
    catalog = (ROOT / "codex-rs/models-manager/models.json").read_text(encoding="utf-8")
    if not any(
        model.get("apply_patch_tool_type") == "freeform"
        for model in json.loads(catalog)["models"]
    ):
        raise ValueError(
            "bundled model catalog has no model with native apply_patch support"
        )
    specs = variants(args.matrix, args.codegen_units)
    with experiment_directory(args.experiment_dir) as directory:
        bundles = {}
        source = None
        compiler = None
        if args.bundle:
            for spec in args.bundle:
                label, separator, bundle_directory = spec.partition("=")
                if not separator or not label or label in bundles:
                    parser.error("each bundle needs a unique LABEL=DIRECTORY")
                bundles[label] = Path(bundle_directory).resolve()
        else:
            compiler = compiler_identity()
            for spec in specs:
                build_environment(spec, compiler["host"])
            bundles, source = build_bundles(directory, specs, compiler)
            catalog = (
                directory / "source/codex-rs/models-manager/models.json"
            ).read_text(encoding="utf-8")
        for bundle in bundles.values():
            for name in PROGRAMS:
                if not (bundle / name).is_file():
                    raise ValueError(f"missing runtime bundle member: {bundle / name}")
        if len(bundles) > 6:
            parser.error("compare at most six bundles per experiment")
        identities = {
            label: {
                name: hashlib.sha256((bundle / name).read_bytes()).hexdigest()
                for name in PROGRAMS
            }
            for label, bundle in bundles.items()
        }
        samples = []
        for warmup, orders in [
            (True, [tuple(bundles)] * args.warmups),
            (False, ordered_rounds(tuple(bundles), args.rounds)),
        ]:
            for order in orders:
                for label in order:
                    for prompt_bytes in (4 * 1024, 512 * 1024):
                        metrics = measure(
                            bundles[label], prompt_bytes, args.timeout, catalog
                        )
                        samples.append(
                            dict(
                                variant=label,
                                prompt_bytes=prompt_bytes,
                                **metrics,
                                warmup=warmup,
                            )
                        )
                        print(
                            f"{'warmup' if warmup else 'sample'} {label} {prompt_bytes // 1024}KiB: {metrics['seconds']:.3f}s (correct)",
                            file=sys.stderr,
                        )
        for label, bundle in bundles.items():
            if any(
                hashlib.sha256((bundle / name).read_bytes()).hexdigest()
                != identities[label][name]
                for name in PROGRAMS
            ):
                raise ValueError(
                    "a runtime bundle changed during measurement; comparison invalid"
                )
        summary = summarize(samples)
        report = {
            "completed_at_unix_ms": time.time_ns() // 1_000_000,
            "scope": "actual codex exec process start through coding-task completion, process exit and owned-child cleanup",
            "build_time_scored": False,
            "provider": "fixed loopback Responses/SSE; no real model inference",
            "timing_scope": "core_turn, orchestration and stream_processing are distinct elapsed-time diagnostics, not CPU time; they overlap and must not be added",
            "catalog_sha256": hashlib.sha256(catalog.encode()).hexdigest(),
            "work_per_sample": {
                "requests": 4,
                "code_mode_cells": 3,
                "file_reads": 2,
                "patches": 1,
                "test_commands": 1,
                "passing_tests": 6,
            },
            "source_sha256": source,
            "compiler": compiler,
            "variants": specs if source else None,
            "codegen_units": args.codegen_units if source else None,
            "binaries": identities,
            "results": summary,
        }
        if args.experiment_dir:
            write_json_atomic(directory / "result.json", report)
        if args.json:
            print(json.dumps(report, indent=2))
        else:
            print("\nActual KD4 coding workflow (seconds; build time excluded)")
            print(
                "variant      prompt    n     median       min       max   orchestration"
            )
            for row in summary:
                orchestration = row["median_orchestration_seconds"]
                orchestration_text = (
                    "unknown" if orchestration is None else f"{orchestration:.6f}s"
                )
                print(
                    f"{row['variant']:<12} {row['prompt_bytes'] // 1024:>4}KiB {row['samples']:>4} {row['median_seconds']:>10.3f} {row['min_seconds']:>9.3f} {row['max_seconds']:>9.3f} {orchestration_text:>15}"
                )
            print(
                "Lower is faster for this workload only; small differences may be noise."
            )
            print(
                "Scripted inference; excludes native Desktop UI and live model latency. No profile is changed or published."
            )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (
        ValueError,
        OSError,
        subprocess.SubprocessError,
        GenerationLockError,
    ) as error:
        print(f"Benchmark incomplete: {error}", file=sys.stderr)
        raise SystemExit(1)
