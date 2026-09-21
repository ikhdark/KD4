"""Behavior regressions verified from folder (5), reports 21 through 25."""

import contextlib
import copy
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

from scripts import atomic_json, root_maintenance, source_inventory
from scripts import process_owner, rust_build_status, stage_npm_packages
from scripts import kd4_turn_latency_audit as audit
from scripts.codex_package import cli, cargo
from scripts.test_kd4_timing_analysis import timing_profile
from scripts import app_server_schema_runtime_check as schema, generated_output_lock
import threading


class Report26ValidationRegressions(unittest.TestCase):
    def test_finite_results_reach_schema_and_formatter_callers(self):
        from scripts import config_schema_check
        from scripts.build_tooling_test_support import load_format_module

        for module in (schema, config_schema_check):
            def bounded(args, **kwargs):
                return process_owner.run_finite(args, timeout=0.2, **kwargs)

            with mock.patch.object(module, "run_finite", side_effect=bounded), contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()) as diagnostic:
                self.assertEqual(module.run([sys.executable, "-c", "import time; time.sleep(60)"], cwd=Path.cwd()), 124)
            self.assertIn("timed_out", diagnostic.getvalue())
        formatter = load_format_module()
        command = formatter.Command((sys.executable, "-c", "import sys; sys.stdout.write('x'*2097152+'END')"))
        result = formatter.run_formatter_group(formatter.FormatterGroup("noisy", (command,)))
        self.assertEqual(result.returncode, 0)
        self.assertIn("output truncated", result.output)
        self.assertLess(len(result.output), 66000)
        self.assertTrue(result.output.endswith("END\n"))

    def test_finite_output_is_bounded_and_observer_sees_every_chunk(self):
        observed = 0

        def observe(chunk):
            nonlocal observed
            observed += len(chunk)

        result = process_owner.run_finite(
            [sys.executable, "-c", "import sys; sys.stdout.write('x'*2097152 + 'END')"],
            timeout=10,
            output_limit=1024,
            observe=observe,
        )
        self.assertEqual((result.status, result.returncode), ("passed", 0))
        self.assertEqual(observed, 2097155)
        self.assertEqual(len(result.stdout), 1024)
        self.assertTrue(result.stdout.endswith("END"))
        self.assertTrue(result.output_truncated)

    def test_finite_timeout_reaps_grandchild_holding_stdout(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / "orphan"
            child = f"import time; from pathlib import Path; time.sleep(1.5); Path({str(marker)!r}).touch(); time.sleep(60)"
            parent = f"import subprocess,sys; subprocess.Popen([sys.executable,'-c',{child!r}])"
            result = process_owner.run_finite(
                [sys.executable, "-c", parent], timeout=0.5
            )
            self.assertEqual((result.status, result.returncode), ("timed_out", 124))
            self.assertLess(result.elapsed, 5)
            time.sleep(1.6)
            self.assertFalse(marker.exists())

    def test_finite_cancellation_stops_silent_child_and_startup_is_classified(self):
        with process_owner.operation() as operation:
            timer = threading.Timer(0.3, operation.cancelled.set)
            timer.start()
            try:
                result = process_owner.run_finite(
                    [sys.executable, "-c", "import time; time.sleep(60)"], timeout=10
                )
            finally:
                timer.cancel()
                timer.join()
        self.assertEqual((result.status, result.returncode), ("cancelled", 130))
        self.assertLess(result.elapsed, 5)
        result = process_owner.run_finite(
            [str(Path(tempfile.gettempdir()) / "missing-report26-tool.exe")]
        )
        self.assertEqual((result.status, result.returncode), ("could_not_start", 127))

    def test_compatibility_keeps_fixed_baseline_across_unrelated_commit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def git(*args):
                return subprocess.run(
                    ["git", *args], cwd=root, capture_output=True, text=True, check=True
                ).stdout.strip()

            git("init", "-q")
            git("config", "user.email", "fixture@example.invalid")
            git("config", "user.name", "Fixture")
            bundle = root / schema.STABLE_SCHEMA_BUNDLE
            bundle.parent.mkdir(parents=True)
            bundle.write_text('{"properties":{"name":{"type":"string"}}}')
            git("add", ".")
            git("commit", "-qm", "contract")
            contract = git("rev-parse", "HEAD")
            bundle.write_text('{"properties":{}}')
            git("commit", "-qam", "break")
            with (
                contextlib.redirect_stdout(io.StringIO()),
                contextlib.redirect_stderr(io.StringIO()),
            ):
                self.assertEqual(
                    schema.run_stable_compatibility_check(root, contract), 1
                )
                (root / "unrelated").touch()
                git("add", ".")
                git("commit", "-qm", "unrelated")
                self.assertEqual(
                    schema.run_stable_compatibility_check(root, "HEAD^"), 0
                )
                resolved = schema.resolve_baseline(root, contract)
                self.assertEqual(resolved, contract)
                self.assertEqual(
                    schema.run_stable_compatibility_check(root, resolved), 1
                )
                self.assertEqual(
                    schema.run_stable_compatibility_check(
                        root, resolved, ["$/properties/name:removed"]
                    ),
                    0,
                )
                self.assertIsNone(schema.resolve_baseline(root, "missing-contract"))

    def test_stable_cli_requires_baseline_before_validation_or_mutation(self):
        with (
            mock.patch.dict(os.environ, {}, clear=True),
            mock.patch.object(schema, "run_protocol_check") as check,
        ):
            with (
                contextlib.redirect_stderr(io.StringIO()),
                self.assertRaises(SystemExit) as error,
            ):
                schema.main(["--mode", "check"])
            self.assertEqual(error.exception.code, 2)
            check.assert_not_called()

    def test_lock_waits_for_release_and_distinguishes_timeout_from_io_error(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            ready = root / "ready"
            program = (
                "import sys; from pathlib import Path; "
                "from scripts.generated_output_lock import generated_output_lock\n"
                "with generated_output_lock(Path(sys.argv[1]), 'owner-a'):\n"
                " Path(sys.argv[2]).touch()\n"
                " sys.stdin.read(1)\n"
            )
            with process_owner.owned_process(
                [sys.executable, "-c", program, str(root), str(ready)],
                stdin=subprocess.PIPE,
            ) as child:
                deadline = time.monotonic() + 5
                while not ready.exists() and time.monotonic() < deadline:
                    time.sleep(0.02)
                self.assertTrue(ready.exists())
                lock = root / ".codex/locks/generated-output.lock"
                before = (lock.stat().st_size, lock.stat().st_mtime_ns)
                with (
                    contextlib.redirect_stderr(io.StringIO()),
                    self.assertRaisesRegex(
                        generated_output_lock.GenerationLockError,
                        "acquisition timed out",
                    ),
                ):
                    with generated_output_lock.generated_output_lock(
                        root, "owner-b", timeout=0.1
                    ):
                        self.fail("acquired an owned lock")
                self.assertEqual((lock.stat().st_size, lock.stat().st_mtime_ns), before)
                timer = threading.Timer(
                    0.15, lambda: (child.stdin.write(b"x"), child.stdin.flush())
                )
                timer.start()
                try:
                    with (
                        contextlib.redirect_stderr(io.StringIO()),
                        generated_output_lock.generated_output_lock(
                            root, "owner-b", timeout=5
                        ),
                    ):
                        self.assertGreater(lock.stat().st_size, 0)
                    self.assertIn('"owner":"owner-b"', lock.read_text())
                finally:
                    timer.join()
                    child.stdin.close()
                self.assertEqual(child.wait(timeout=5), 0)
            with mock.patch.object(
                generated_output_lock,
                "_acquire_nonblocking",
                side_effect=OSError("disk failure"),
            ) as acquire:
                with self.assertRaisesRegex(OSError, "disk failure"):
                    with generated_output_lock.generated_output_lock(
                        root, "owner-c", timeout=5
                    ):
                        self.fail("ignored I/O error")
                self.assertEqual(acquire.call_count, 1)


class ScriptReportRegressions(unittest.TestCase):
    def test_recent_overflow_survives_expired_base(self):
        root = Path("lanes")
        base, recent = root / "pkg", root / "pkg-2"
        self.assertEqual(
            rust_build_status.protected_warm_lane_names(
                [base, recent],
                keep_warm_per_base=1,
                lane_mtime=lambda path: 1 if path == base else 100,
            ),
            {"pkg-2"},
        )
        self.assertEqual(
            rust_build_status.protected_warm_lane_names(
                [recent, base],
                keep_warm_per_base=1,
                lane_mtime=lambda _: 100,
            ),
            {"pkg"},
        )

    def test_changed_tests_and_adjacent_production_across_owned_roots(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            roots = tuple(
                root / path.relative_to(root_maintenance.REPO_ROOT)
                for path in root_maintenance.SCRIPT_AUDIT_ROOTS
            )
            with mock.patch.multiple(
                root_maintenance,
                REPO_ROOT=root,
                SCRIPTS_ROOT=root / "scripts",
                SCRIPT_AUDIT_ROOTS=roots,
            ):
                for owned in roots:
                    owned.mkdir(parents=True, exist_ok=True)
                    source, test = owned / "MixedCase.py", owned / "test_MixedCase.py"
                    source.write_text("pass\n")
                    test.write_text("pass\n")
                    relative = test.relative_to(root)
                    target = root_maintenance.python_test_target(relative)
                    self.assertEqual(
                        root_maintenance.test_modules_for_changed_path(str(relative)),
                        (target,),
                    )
                    self.assertEqual(
                        root_maintenance.test_modules_for_changed_path(
                            str(source.relative_to(root))
                        ),
                        (target,),
                    )
                    self.assertEqual(
                        root_maintenance.python_lint_targets([str(relative)]),
                        [relative.as_posix()],
                    )

    def test_scan_budget_continues_without_rereading_completed_prefix(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            states = {}
            for index in range(9):
                name = f"{index}.txt"
                (root / name).write_bytes(b"x" * 8)
                states[name] = "tracked"
            query = {
                "categories": [
                    {"name": "files", "paths": ["*.txt"], "verification": "path"}
                ]
            }
            with (
                mock.patch.object(source_inventory, "MAX_SCAN_BYTES", 64),
                mock.patch.object(
                    source_inventory, "repository_source_records", return_value=states
                ),
            ):
                first, state = source_inventory.inventory(root, query)
                self.assertEqual((first["count"], first["scan_pending"]), (8, 1))
                second, state = source_inventory.inventory(root, query, state)
                self.assertEqual((second["count"], second["source_bytes_read"]), (9, 8))
                self.assertTrue(second["ready_to_render"])
                self.assertEqual(first["scan_epoch"], second["scan_epoch"])
                refreshed, _ = source_inventory.inventory(
                    root, query, state, refresh=True
                )
                self.assertNotEqual(refreshed["scan_epoch"], second["scan_epoch"])
                self.assertEqual(refreshed["source_bytes_read"], 64)

    def test_atomic_output_preserves_hardlinked_source_and_checkpoint_on_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, output = root / "source", root / "report"
            source.write_bytes(b"source")
            os.link(source, output)
            atomic_json.write_bytes_atomic(output, b"report")
            self.assertEqual(source.read_bytes(), b"source")
            with mock.patch.object(
                atomic_json.os, "fsync", side_effect=OSError("disk full")
            ):
                with self.assertRaises(OSError):
                    atomic_json.write_bytes_atomic(output, b"broken")
            self.assertEqual(output.read_bytes(), b"report")
            self.assertEqual(list(root.glob("*.tmp")), [])
            immutable = root / "immutable"
            atomic_json.write_bytes_atomic(immutable, b"complete", immutable=True)
            with self.assertRaises(ValueError):
                atomic_json.write_bytes_atomic(immutable, b"different", immutable=True)
            self.assertEqual(immutable.read_bytes(), b"complete")

    def test_owned_process_reaps_descendants_after_parent_exits(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / "late"
            child = f"import time; from pathlib import Path; time.sleep(0.5); Path({str(marker)!r}).write_text('orphan')"
            parent = f"import subprocess,sys; subprocess.Popen([sys.executable,'-c',{child!r}])"
            result = process_owner.run_owned([sys.executable, "-c", parent], timeout=10)
            self.assertEqual(result.returncode, 0)
            time.sleep(0.7)
            self.assertFalse(marker.exists())

    def test_pool_failure_cancels_owned_sibling_and_preserves_primary_error(self):
        started = __import__("threading").Event()

        def run():
            started.set()
            return process_owner.run_owned(
                [sys.executable, "-c", "import time; time.sleep(60)"]
            )

        def fail():
            started.wait(5)
            raise ValueError("dependency failed")

        before = time.monotonic()
        with self.assertRaisesRegex(ValueError, "dependency failed"):
            with process_owner.OwnedThreadPoolExecutor(max_workers=2) as executor:
                sibling = executor.submit(run)
                executor.submit(fail)
                sibling.result()
        self.assertLess(time.monotonic() - before, 10)

    def test_terminal_conflicts_are_order_independent_and_malformed_is_local(self):
        first = timing_profile()
        second = copy.deepcopy(first)
        second["inclusiveDurationNs"] *= 9
        malformed = copy.deepcopy(first)
        malformed["inclusiveDurationNs"] = None
        for profiles in ((first, second), (second, first)):
            with tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "rollout.jsonl"
                rows = []
                for turn, value in [("conflict", p) for p in profiles] + [
                    ("bad", malformed),
                    ("good", first),
                ]:
                    rows.append(
                        {
                            "type": "event_msg",
                            "payload": {
                                "type": "task_complete",
                                "turn_id": turn,
                                "timing": value,
                            },
                        }
                    )
                path.write_text("\n".join(json.dumps(row) for row in rows))
                report = audit.analyze_session_path(path, Path(directory))
                self.assertEqual(report["coverage"]["validCompleteProfiles"], 1)
                self.assertEqual(report["coverage"]["conflictingTerminalProfiles"], 1)
                self.assertFalse(report["auditDecision"]["readyToFinalize"])

    def test_publication_rolls_back_package_archive_and_sidecar(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package, archive, sidecar = (
                root / "package",
                root / "archive.zip",
                root / "checksums",
            )
            package.mkdir()
            (package / "old").write_bytes(b"old package")
            archive.write_bytes(b"old archive")
            with self.assertRaisesRegex(OSError, "sidecar"):
                with cli.publication_transaction([package, archive, sidecar]):
                    with cli.staged_package_destination(
                        package, reuse_existing=True
                    ) as staged:
                        staged.mkdir()
                        (staged / "new").write_bytes(b"new")
                    cli.write_text_atomically(archive, "new archive")
                    cli.write_text_atomically(sidecar, "partial")
                    raise OSError("sidecar failed")
            self.assertEqual((package / "old").read_bytes(), b"old package")
            self.assertFalse((package / "new").exists())
            self.assertEqual(archive.read_bytes(), b"old archive")
            self.assertFalse(sidecar.exists())

    def test_npm_cancellation_restores_all_outputs_and_clears_journal(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output, staged = root / "output", root / "staged"
            output.mkdir()
            staged.mkdir()
            results = []
            for name in ("one.tgz", "two.tgz"):
                (output / name).write_bytes(b"old")
                (staged / name).write_bytes(b"new")
                results.append(
                    stage_npm_packages.StagePackageResult(name, staged / name, "")
                )
            original = stage_npm_packages.replace_package_file

            def replace(source, destination):
                if source.name == "two.tgz":
                    raise KeyboardInterrupt
                original(source, destination)

            with mock.patch.object(
                stage_npm_packages, "replace_package_file", side_effect=replace
            ):
                with self.assertRaises(KeyboardInterrupt):
                    stage_npm_packages.commit_staged_packages(results, output)
            self.assertEqual(
                [(output / name).read_bytes() for name in ("one.tgz", "two.tgz")],
                [b"old", b"old"],
            )
            self.assertFalse((output / ".npm-activation.json").exists())

    def test_tool_contents_invalidate_identity_preserving_size_and_mtime(self):
        with tempfile.TemporaryDirectory() as directory:
            tool = Path(directory) / "linker.exe"
            tool.write_bytes(b"aaaa")
            stat = tool.stat()
            first = cargo.executable_content_identity(str(tool), dict(os.environ))
            tool.write_bytes(b"bbbb")
            os.utime(tool, ns=(stat.st_atime_ns, stat.st_mtime_ns))
            second = cargo.executable_content_identity(str(tool), dict(os.environ))
            self.assertNotEqual(first["sha256"], second["sha256"])

    def test_activation_rollback_retains_old_outputs_without_copying(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package, checksum = root / "package", root / "checksum"
            package.mkdir()
            (package / "old").write_bytes(b"old")
            checksum.write_text("old checksum")
            with mock.patch.object(
                cli.shutil, "copytree", side_effect=AssertionError("extra copy")
            ):
                with self.assertRaisesRegex(OSError, "final output"):
                    with cli.publication_transaction([package, checksum]):
                        with cli.staged_package_destination(
                            package, reuse_existing=True
                        ) as staging:
                            staging.mkdir()
                            (staging / "new").write_bytes(b"new")
                        cli.write_text_atomically(checksum, "new checksum")
                        raise OSError("final output failed")
            self.assertEqual([p.name for p in package.iterdir()], ["old"])
            self.assertEqual(checksum.read_text(), "old checksum")
            self.assertEqual(list(root.glob("*.backup-*")), [])

    def test_npm_recovery_rejects_unowned_backup_before_mutation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            innocent = root / "notes"
            innocent.write_bytes(b"keep")
            (root / ".npm-activation.json").write_text(
                json.dumps(
                    {
                        "version": 1,
                        "phase": "committed",
                        "entries": [
                            {
                                "name": "one.tgz",
                                "backup": "notes",
                                "old": None,
                                "new": None,
                            }
                        ],
                    }
                )
            )
            with self.assertRaisesRegex(ValueError, "filenames"):
                stage_npm_packages.recover_package_activation(root)
            self.assertEqual(innocent.read_bytes(), b"keep")

    def test_cross_target_prebuilt_cannot_label_distributable_version(self):
        from scripts.codex_package.test_cli import request_args
        from scripts.codex_package.targets import TARGET_SPECS

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            args = request_args(
                target="aarch64-pc-windows-msvc",
                variant="codex",
                package_dir=root / "package",
                archive_output=[root / "out.zip"],
                release_version="1.2.3",
                entrypoint_bin=root / "prebuilt.exe",
            )
            with mock.patch.object(
                cli, "default_target", return_value="x86_64-pc-windows-msvc"
            ):
                with self.assertRaisesRegex(RuntimeError, "release-version evidence"):
                    cli.validate_cli_request(
                        args, TARGET_SPECS[args.target], args.package_dir
                    )

    def test_nextest_progress_counter_and_binary_identity(self):
        from scripts.test_rust_test_runner import MANIFEST_DATA, METADATA_PACKAGES
        from scripts.rust_test_runner import (
            Manifest,
            RustTestRunner,
            MetadataIndex,
            RunnerError,
        )

        with tempfile.TemporaryDirectory() as directory:
            data = copy.deepcopy(MANIFEST_DATA)
            data["gates"]["identity"] = {
                "description": "Verify binary receipt",
                "steps": [{"target": "core_lib", "tests": ["proof"], "helpers": []}],
            }
            runner = RustTestRunner(
                Manifest.from_data(data),
                MetadataIndex.from_json(
                    {"packages": METADATA_PACKAGES, "target_directory": directory}
                ),
                target_dir=Path(directory),
            )
            for binary in ("codex-core", "wrong-package"):
                result = subprocess.CompletedProcess(
                    [], 0, f"PASS [ 0.1s] (1/1) {binary} proof\n", ""
                )
                with mock.patch.object(runner, "_checked", return_value=result):
                    if binary == "codex-core":
                        self.assertEqual(
                            runner.run_gates(["identity"], quiet=True),
                            {"identity": ["proof"]},
                        )
                    else:
                        with self.assertRaises(RunnerError):
                            runner.run_gates(["identity"], quiet=True)

    def test_feature_lane_is_lazy_and_held_across_both_phases(self):
        from scripts import check_kd4_features as feature

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            entered, released = [], []

            @contextlib.contextmanager
            def reserve(**kwargs):
                entered.append(kwargs)
                try:
                    yield "core-tests", root / "lane"
                finally:
                    released.append(True)

            with (
                mock.patch.object(
                    feature.rust_build_status, "reserve_cargo_lane", reserve
                ),
                mock.patch.dict(os.environ, {}, clear=True),
            ):
                with feature.validation_session():
                    self.assertEqual(entered, [])
                    with feature.validation_lane(root) as first:
                        self.assertEqual(first, root / "lane")
                    self.assertEqual(released, [])
                    with feature.validation_lane(root) as second:
                        self.assertEqual(first, second)
                    self.assertEqual(len(entered), 1)
                self.assertEqual(released, [True])
                self.assertIsNone(feature._validation_lane.get())

    def test_staged_bytes_must_match_inputs_observed_before_copy(self):
        from scripts.codex_package import layout
        from scripts.codex_package.targets import (
            PackageInputs,
            PACKAGE_VARIANTS,
            TARGET_SPECS,
        )

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            paths = [root / name for name in ("codex", "host", "rg", "runner", "setup")]
            for path in paths:
                path.write_bytes(path.name.encode())
            inputs = PackageInputs(*paths)
            package = root / "package"
            package.mkdir()
            copy_file = layout.copy_executable

            def changed_copy(source, destination, **kwargs):
                if source == paths[0]:
                    source.write_bytes(b"changed after input capture")
                copy_file(source, destination, **kwargs)

            with mock.patch.object(layout, "copy_executable", side_effect=changed_copy):
                with self.assertRaisesRegex(
                    RuntimeError, "input changed during staging: entrypoint"
                ):
                    layout.build_package_dir(
                        package,
                        "1.2.3",
                        PACKAGE_VARIANTS["codex"],
                        TARGET_SPECS["x86_64-pc-windows-msvc"],
                        inputs,
                    )
            self.assertFalse((package / "codex-package.json").exists())

    def test_missing_license_fails_before_build(self):
        from scripts.codex_package.test_cli import request_args
        from scripts.codex_package.targets import TARGET_SPECS

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            args = request_args(
                target="x86_64-pc-windows-msvc",
                variant="codex",
                package_dir=root / "package",
            )
            with mock.patch.object(cli, "REPO_ROOT", root):
                with self.assertRaisesRegex(RuntimeError, "missing: LICENSE"):
                    cli.validate_cli_request(
                        args, TARGET_SPECS[args.target], args.package_dir
                    )


if __name__ == "__main__":
    unittest.main()
