"""Regression checks for warm-lane admission defaults and checkout preflight."""
import contextlib
import inspect
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from scripts import benchmark_stream_delivery, rust_build_status, rust_test_runner


class LaneLatencyTests(unittest.TestCase):
    def test_busy_and_short_timeout_never_launch_a_child_or_claim_completion(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            lanes = root / "codex-rs" / "target" / "lanes"
            rust_build_status.initialize_cargo_lanes_root(root, lanes)
            for blocker in ("lane", "coordination"):
                for wait in (0, .1):
                    now = [0.0]
                    def sleep(delay):
                        now[0] += delay
                    with contextlib.ExitStack() as stack:
                        if blocker == "lane":
                            stack.enter_context(rust_build_status.reserve_cargo_lane(
                                repo_root=root, lane_root=lanes, requested_lane="fixture", command=[]))
                        else:
                            stack.enter_context(rust_build_status.cargo_lane_coordination_lock(lanes))
                        stack.enter_context(mock.patch.object(rust_build_status.time, "monotonic", side_effect=lambda: now[0]))
                        slept = stack.enter_context(mock.patch.object(rust_build_status.time, "sleep", side_effect=sleep))
                        child = stack.enter_context(mock.patch.object(rust_build_status, "run_owned"))
                        stack.enter_context(mock.patch.object(rust_build_status, "local_rust_env", return_value={}))
                        stack.enter_context(mock.patch.object(rust_build_status, "cargo_build_context", return_value={}))
                        stderr = stack.enter_context(contextlib.redirect_stderr(io.StringIO()))
                        timing = root / f"{blocker}-{wait}.json"
                        argv = ["run-lane", "--lane", "fixture", "--repo-root", str(root),
                                "--lanes-root", str(lanes), "--warm-wait-seconds", str(wait),
                                "--timing-json", str(timing), "--", sys.executable, "-c", "pass"]
                        self.assertEqual(rust_build_status.main(argv), 75)
                        child.assert_not_called()
                        self.assertAlmostEqual(now[0], wait)
                        if wait == 0:
                            slept.assert_not_called()
                        else:
                            self.assertGreater(slept.call_count, 0)
                        status = next(json.loads(line) for line in stderr.getvalue().splitlines() if line.startswith("{"))
                        self.assertEqual(status["status"], "busy")
                        self.assertEqual(status["validation_status"], "pending")
                        self.assertEqual(status["invocation"][2:], argv)
                        self.assertEqual(status["working_directory"], str(Path.cwd().resolve()))
                        for flag in ("executed", "queued", "automatic_retry", "automatic_resume"):
                            self.assertIs(status[flag], False)
                        record = json.loads(timing.read_text())
                        self.assertEqual((record["status"], record["exitCode"]), ("busy", 75))
                        self.assertIsNone(record["phaseDurationsMs"]["command"])
                    self.assertFalse((lanes / "fixture-2").exists())

    def test_explicit_wait_dispatches_once_after_release(self):
        with tempfile.TemporaryDirectory() as temp, contextlib.ExitStack() as owner:
            root = Path(temp)
            lanes = root / "codex-rs" / "target" / "lanes"
            owner.enter_context(rust_build_status.reserve_cargo_lane(
                repo_root=root, lane_root=lanes, requested_lane="fixture", command=[]))
            with (
                mock.patch.object(rust_build_status.time, "sleep", side_effect=lambda _: owner.close()) as sleep,
                mock.patch.object(rust_build_status, "run_owned", return_value=subprocess.CompletedProcess([], 0)) as child,
                mock.patch.object(rust_build_status, "local_rust_env", return_value={}),
                mock.patch.object(rust_build_status, "cargo_build_context", return_value={}),
                mock.patch.object(rust_build_status, "request_cargo_lane_maintenance"),
                contextlib.redirect_stderr(io.StringIO()),
            ):
                self.assertEqual(rust_build_status.run_in_cargo_lane(
                    repo_root=root, lane_root=lanes, requested_lane="fixture", warm_wait_seconds=1,
                    command=[sys.executable, "-c", "pass"]), 0)
                sleep.assert_called_once()
                child.assert_called_once()
            self.assertFalse((lanes / "fixture-2").exists())

    @unittest.skipUnless(os.name == "nt" and shutil.which("just") and shutil.which("pwsh"), "Windows just recipes")
    def test_real_just_wrapper_preserves_busy_and_explicit_wait_without_retries(self):
        repo = rust_build_status.REPO_ROOT
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "codex-rs").mkdir()
            (root / "scripts").mkdir()
            shutil.copyfile(repo / "justfile", root / "justfile")
            (root / "scripts" / "just-shell.py").write_text(
                "import runpy, sys\n"
                f"adapter=runpy.run_path({str(repo / 'scripts' / 'just-shell.py')!r})\n"
                "run_python = adapter['run_python']\n"
                "if __name__ == '__main__':\n"
                " raise SystemExit(adapter['run_powershell'](sys.argv[1], sys.argv[2], sys.argv[3:]))\n")
            (root / "scripts" / "rust_build_status.py").write_text(
                "import json, os, sys\nprint(json.dumps(sys.argv[1:]))\nsys.exit(int(os.environ['STUB_EXIT']))\n")
            for code, wait in ((75, None), (75, "5"), (0, None), (2, None)):
                options = [] if wait is None else ["--set", "rust_validation_wait_seconds", wait]
                result = subprocess.run(["just", *options, "core-gate", "fixture"], cwd=root,
                    env=dict(os.environ, STUB_EXIT=str(code)), capture_output=True, text=True, timeout=15,
                    creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
                self.assertEqual(result.returncode, code, result.stderr)
                command = json.loads(result.stdout)  # Exactly one invocation, not a retry loop.
                self.assertEqual(command[command.index("--warm-wait-seconds") + 1], wait or "0")
                self.assertEqual(command[-2:], ["_core-gate-reserved", "fixture"])

    def test_warm_wait_defaults_and_explicit_override(self):
        for function in (rust_build_status.reserve_cargo_lane, rust_build_status.run_in_cargo_lane):
            self.assertEqual(inspect.signature(function).parameters["warm_wait_seconds"].default, 0.0)
        for options, expected in (([], 0.0), (["--warm-wait-seconds", "0"], 0.0), (["--warm-wait-seconds", "42"], 42.0)):
            with mock.patch.object(rust_build_status, "run_in_cargo_lane", return_value=0) as run:
                self.assertEqual(rust_build_status.main([
                    "run-lane", "--lane", "core-tests", *options, "--", "cargo", "check",
                ]), 0)
            self.assertEqual(run.call_args.kwargs["warm_wait_seconds"], expected)

    def test_runner_wait_defaults_and_explicit_override(self):
        self.assertEqual(inspect.signature(rust_build_status.reserve_rust_test_target).parameters["timeout_seconds"].default, 0.0)
        for options, expected in (([], 0.0), (["--admission-timeout-seconds", "42"], 42.0)):
            args = rust_test_runner.build_parser().parse_args([*options, "check-manifest"])
            self.assertEqual(args.admission_timeout_seconds, expected)

    def test_warm_wait_rejects_nonfinite_and_negative_values(self):
        for value in (-1, float("nan"), float("inf")):
            with self.subTest(value=value), self.assertRaises(ValueError):
                rust_build_status._nonnegative_wait_seconds(value)

    def test_checkout_guard_rejects_wrong_root_without_relocating(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp).resolve()
            rust = root / "codex-rs"
            rust.mkdir()
            (rust / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")
            with contextlib.chdir(root):
                with self.assertRaisesRegex(ValueError, "No build lane was reserved"):
                    rust_build_status._guard_cargo_checkout_cwd(["cargo", "check"], root)
                rust_build_status._guard_cargo_checkout_cwd([
                    "cargo", "check", "--manifest-path", "codex-rs/Cargo.toml",
                ], root)
                self.assertEqual(Path.cwd(), root)
            with contextlib.chdir(rust):
                rust_build_status._guard_cargo_checkout_cwd(["cargo", "check"], root)


class StreamBenchmarkTests(unittest.TestCase):
    def test_isolated_copy_excludes_only_renderer_dependent_tests(self):
        root = Path(benchmark_stream_delivery.__file__).resolve().parents[1]
        rust = root / "codex-rs"
        source_paths = (
            "tui/src/table_detect.rs", "tui/src/streaming/chunking.rs",
            "tui/src/streaming/table_holdback.rs", "tui/src/markdown_stream.rs",
        )
        originals = {path: (rust / path).read_bytes() for path in source_paths}
        captured = {}

        def run(command, cwd):
            if command[:2] == ["rustc", "--version"]:
                return "fixture compiler"
            if command[0] == "rustc":
                work = Path(command[command.index("-o") + 1]).parent
                captured.update({path: (work / Path(path).name).read_bytes()
                                 for path in source_paths})
                return ""
            if Path(command[0]).stem == "probe":
                return '{"scanner_25600_lines_us":1}\n'
            return "fixture tests passed"

        output = io.StringIO()
        with (mock.patch.object(benchmark_stream_delivery, "run", side_effect=run),
              mock.patch.object(sys, "argv", ["benchmark_stream_delivery", "--iterations", "1"]),
              contextlib.redirect_stdout(output)):
            benchmark_stream_delivery.main()

        table_marker = b"#[cfg(test)]\nmod tests {"
        holdback_marker = (b"    #[test]\n"
                           b"    fn indented_fence_markers_do_not_change_table_holdback() {")
        for path, original in originals.items():
            self.assertEqual((rust / path).read_bytes(), original)
            expected = original
            if path.endswith("/table_detect.rs"):
                self.assertEqual(original.count(table_marker), 1)
                expected = original.replace(table_marker, b"#[cfg(any())]\nmod tests {")
            elif path.endswith("/table_holdback.rs"):
                self.assertEqual(original.count(holdback_marker), 1)
                expected = original.replace(holdback_marker, b"    #[cfg(any())]\n" + holdback_marker)
            self.assertEqual(captured[path], expected, path)
        self.assertIn(
            "table_holdback::tests::indented_fence_markers_do_not_change_table_holdback",
            json.loads(output.getvalue())["excluded_tests"],
        )


if __name__ == "__main__":
    unittest.main()
