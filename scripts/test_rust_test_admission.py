"""Cross-process runner admission proof using Python children, never Cargo."""
from __future__ import annotations

import argparse
import contextlib
from dataclasses import replace
import hashlib
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

from scripts import rust_build_status as lanes, rust_test_runner as runner
from scripts.test_rust_test_runner import RunnerTestCase


class TargetAdmissionTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.target = self.root / "target"

    def child(self, name, target, cargo_profile=None):
        # Attempt/ready/release files are explicit barriers, not timing guesses.
        program = r'''
import pathlib, sys, time
from scripts import rust_build_status as lanes
root, target, name = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]), sys.argv[3]
original = lanes._try_acquire_binary_file_lock
def acquire(path):
    (root / (name + '.attempt')).touch()
    return original(path)
lanes._try_acquire_binary_file_lock = acquire
def observe_busy(probe):
    def wrapped(path):
        busy = probe(path)
        if busy:
            (root / (name + '.blocked')).touch()
        return busy
    return wrapped
lanes._binary_file_lock_is_busy = observe_busy(lanes._binary_file_lock_is_busy)
lanes._cargo_lock_file_is_busy = observe_busy(lanes._cargo_lock_file_is_busy)
with lanes.reserve_rust_test_target(target, timeout_seconds=10, cargo_profile=sys.argv[4] or None):
    (root / (name + '.ready')).touch()
    deadline = time.monotonic() + 15
    while not (root / (name + '.release')).exists():
        if time.monotonic() >= deadline: raise RuntimeError('fixture not released')
        time.sleep(.01)
'''
        process = subprocess.Popen(
            [sys.executable, "-c", program, str(self.root), str(target), name, cargo_profile or ""],
            cwd=lanes.REPO_ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
        )
        def cleanup():
            if process.poll() is None:
                process.kill()
            process.communicate(timeout=5)
        self.addCleanup(cleanup)
        return process

    def barrier(self, name, process):
        deadline = time.monotonic() + 10
        while not (self.root / name).exists():
            if process.poll() is not None:
                self.fail(repr(process.communicate(timeout=1)))
            if time.monotonic() >= deadline:
                self.fail(f"missing barrier {name}")
            time.sleep(.01)

    def release(self, name, process):
        (self.root / (name + ".release")).touch()
        _, stderr = process.communicate(timeout=10)
        self.assertEqual(process.returncode, 0, stderr)

    def test_same_target_waits_aliases_collide_other_target_proceeds(self):
        first = self.child("first", self.target)
        self.barrier("first.ready", first)
        second = self.child("second", self.target / ".." / "target")
        self.barrier("second.attempt", second)
        independent = self.child("independent", self.root / "other")
        self.barrier("independent.ready", independent)
        self.assertFalse((self.root / "second.ready").exists())
        self.assertTrue(lanes.cargo_lock_is_busy(self.target))
        self.release("independent", independent)
        self.release("first", first)
        self.barrier("second.ready", second)
        self.release("second", second)
        self.assertFalse(lanes.cargo_lock_is_busy(self.target))

    def test_crashed_owner_releases_os_lock_without_removing_lock_file(self):
        owner = self.child("owner", self.target)
        self.barrier("owner.ready", owner)
        owner.kill()
        owner.communicate(timeout=5)
        self.assertTrue((self.target / ".rust-test-runner.lock").is_file())
        with lanes.reserve_rust_test_target(self.target, timeout_seconds=1):
            self.assertTrue(lanes.cargo_lock_is_busy(self.target))

    def test_runner_waits_for_lane_and_cargo_owners_without_dispatching(self):
        # Hold real locks in this process; the child must wait before any work
        # starts. A different target must remain usable while it waits.
        for relative in [".lane-active.lock", "debug/.cargo-lock", "triple/debug/.cargo-lock"]:
            with self.subTest(lock=relative):
                target = self.root / relative.replace("/", "-")
                path = target / relative
                path.parent.mkdir(parents=True)
                handle = lanes._try_acquire_binary_file_lock(path)
                self.assertIsNotNone(handle)
                try:
                    child = self.child("external", target)
                    self.barrier("external.blocked", child)
                    # Acquiring the independent target ensures the waiter has a
                    # chance to make progress without relying on a short sleep.
                    with lanes.reserve_rust_test_target(self.root / "independent"):
                        self.assertFalse((self.root / "external.ready").exists())
                finally:
                    lanes._release_binary_file_lock(handle)
                    handle.close()
                self.barrier("external.ready", child)
                self.release("external", child)
                for suffix in ["attempt", "blocked", "ready", "release"]:
                    (self.root / f"external.{suffix}").unlink(missing_ok=True)

    def test_inherited_lane_allows_parent_but_not_competing_runner_or_cargo(self):
        self.target.mkdir()
        handle = lanes._try_acquire_binary_file_lock(self.target / ".lane-active.lock")
        self.assertIsNotNone(handle)
        try:
            with mock.patch.dict(os.environ, {"CODEX_CARGO_LANE_TARGET_DIR": str(self.root / "other")}):
                with self.assertRaises(TimeoutError):
                    with lanes.reserve_rust_test_target(self.target, timeout_seconds=.02):
                        self.fail("different inherited lane admitted")
            with mock.patch.dict(os.environ, {"CODEX_CARGO_LANE_TARGET_DIR": str(self.target / ".." / "target")}):
                with lanes.reserve_rust_test_target(self.target) as receipt:
                    self.assertTrue(receipt["inherited_lane_reservation"])
                    with self.assertRaises(TimeoutError):
                        with lanes.reserve_rust_test_target(self.target, timeout_seconds=.02):
                            self.fail("parent context bypassed another runner")
                profile = self.target / "debug"
                profile.mkdir()
                cargo = lanes._try_acquire_binary_file_lock(profile / ".cargo-lock")
                try:
                    with self.assertRaises(TimeoutError):
                        with lanes.reserve_rust_test_target(self.target, timeout_seconds=.02):
                            self.fail("parent context bypassed Cargo")
                finally:
                    lanes._release_binary_file_lock(cargo)
                    cargo.close()
        finally:
            lanes._release_binary_file_lock(handle)
            handle.close()

    def test_idle_admission_acquires_runner_lease_once(self):
        for profile in (None, "test", "dev-small"):
            with self.subTest(profile=profile):
                with mock.patch.object(
                    lanes, "_try_acquire_binary_file_lock",
                    wraps=lanes._try_acquire_binary_file_lock,
                ) as acquire:
                    with lanes.reserve_rust_test_target(self.target, cargo_profile=profile):
                        pass
                self.assertEqual(
                    [call.args[0] for call in acquire.call_args_list],
                    [self.target / ".rust-test-runner.lock"],
                )

    def test_failed_cargo_probe_releases_the_newly_acquired_runner_lease(self):
        with mock.patch.object(lanes, "_cargo_profile_locks_are_busy", side_effect=OSError("fixture")):
            with self.assertRaises(OSError):
                with lanes.reserve_rust_test_target(self.target, cargo_profile="test"):
                    self.fail("failed probe admitted")
        self.assertFalse(lanes.cargo_lock_is_busy(self.target))

    def test_selected_profile_finishes_while_unrelated_build_still_holds_lock(self):
        # A causal wall-clock check: the new path completes before the blocker
        # is released; the legacy whole-target probe cannot make progress.
        profile = self.target / "release"
        profile.mkdir(parents=True)
        handle = lanes._try_acquire_binary_file_lock(profile / ".cargo-lock")
        self.assertIsNotNone(handle)
        try:
            legacy = self.child("legacy", self.target)
            self.barrier("legacy.blocked", legacy)
            selected = self.child("selected", self.target, cargo_profile="test")
            self.barrier("selected.ready", selected)
            self.release("selected", selected)
            self.assertFalse((self.root / "legacy.ready").exists())
            self.assertTrue(lanes.cargo_lock_is_busy(self.target))
            self.assertFalse(lanes.cargo_lock_is_busy(self.target, profile="debug"))
        finally:
            lanes._release_binary_file_lock(handle)
            handle.close()
        self.barrier("legacy.ready", legacy)
        self.release("legacy", legacy)

    def test_profile_aliases_still_wait_for_their_host_and_target_locks(self):
        for cargo_profile, directory in [
            ("dev", "debug"), ("test", "debug"),
            ("release", "release"), ("bench", "release"),
            ("dev-small", "dev-small"),
        ]:
            for prefix in ("", "triple"):
                with self.subTest(profile=cargo_profile, prefix=prefix):
                    target = self.root / f"{cargo_profile}-{prefix}"
                    profile = target / prefix / directory
                    profile.mkdir(parents=True)
                    handle = lanes._try_acquire_binary_file_lock(profile / ".cargo-lock")
                    self.assertIsNotNone(handle)
                    try:
                        with self.assertRaises(TimeoutError):
                            with lanes.reserve_rust_test_target(
                                target, cargo_profile=cargo_profile, timeout_seconds=.02,
                            ):
                                self.fail("active build profile admitted")
                        other = "release" if directory == "debug" else "test"
                        with lanes.reserve_rust_test_target(target, cargo_profile=other) as receipt:
                            self.assertEqual(
                                receipt["cargo_profile_directory"],
                                "release" if other == "release" else "debug",
                            )
                    finally:
                        lanes._release_binary_file_lock(handle)
                        handle.close()

    def test_profile_selection_never_bypasses_another_runner(self):
        with lanes.reserve_rust_test_target(self.target, cargo_profile="test"):
            for profile in (None, "test", "release", "dev-small"):
                with self.subTest(profile=profile), self.assertRaises(TimeoutError):
                    with lanes.reserve_rust_test_target(
                        self.target, cargo_profile=profile, timeout_seconds=.02,
                    ):
                        self.fail("another runner bypassed the whole-invocation lease")

    def test_invalid_profiles_reject_before_creating_target(self):
        for value in ("", "../release", "debug/release", "debug\\release"):
            with self.subTest(profile=value), self.assertRaises(ValueError):
                with lanes.reserve_rust_test_target(self.target, cargo_profile=value):
                    self.fail("profile path admitted")
        self.assertFalse(self.target.exists())

    def test_direct_runner_shortcuts_forward_only_the_owned_target(self):
        for command in [
            ["just", "_core-test-reserved", "fast", "core_lib", "-E", "test(example)"],
            ["just", "_core-test-small-reserved", "core_lib", "-E", "test(example)"],
            ["just", "_core-gate-reserved", "example"],
        ]:
            with self.subTest(command=command):
                env = {"CODEX_CARGO_LANE_TARGET_DIR": "obsolete"}
                result = lanes._direct_reserved_lane_command(
                    command, env, repo_root=self.root, target_dir=self.target,
                )
                self.assertIsNotNone(result)
                self.assertEqual(env["CODEX_CARGO_LANE_TARGET_DIR"], str(self.target))
                self.assertEqual(result[result.index("--target-dir") + 1], str(self.target))
                parsed = runner.build_parser().parse_args(result[2:])
                self.assertEqual(parsed.profile, "fast")
                self.assertEqual(
                    parsed.cargo_profile,
                    "dev-small" if command[1] == "_core-test-small-reserved" else None,
                )

    @unittest.skipUnless(os.name == "nt", "Windows reserved recipes")
    def test_reserved_recipe_fallbacks_keep_parent_lease_through_admission(self):
        # Execute the actual recipe bodies with a tiny runner that only admits,
        # so fallback PowerShell paths cannot silently wait on their own parent.
        probe = self.root / "probe.py"
        probe.write_text(
            "import json, sys\n"
            f"sys.path.insert(0, {str(lanes.REPO_ROOT)!r})\n"
            "from scripts import rust_build_status as lanes, rust_test_runner as runner\n"
            "from pathlib import Path\n"
            "args = runner.build_parser().parse_args()\n"
            "with lanes.reserve_rust_test_target(Path(args.target_dir), "
            "cargo_profile=args.cargo_profile or 'test', timeout_seconds=.1) as receipt:\n"
            "    print(json.dumps(receipt))\n",
            encoding="utf-8",
        )
        lines = (lanes.REPO_ROOT / "justfile").read_text(encoding="utf-8").splitlines()
        self.target.mkdir()
        handle = lanes._try_acquire_binary_file_lock(self.target / ".lane-active.lock")
        self.assertIsNotNone(handle)
        try:
            for recipe, arguments in [
                ("_core-test-reserved", ["fast", "core_lib", "-E", "test(one)"]),
                ("_core-test-small-reserved", ["core_lib", "-E", "test(one)"]),
                ("_core-gate-reserved", ["fixture"]),
            ]:
                with self.subTest(recipe=recipe):
                    index = next(i for i, line in enumerate(lines) if line.startswith(recipe + " "))
                    body = lines[index + 1].strip()
                    body = body.replace(
                        'python "{{ justfile_directory() }}\\scripts\\rust_test_runner.py"',
                        f"& '{sys.executable.replace(chr(39), chr(39) * 2)}' '{str(probe).replace(chr(39), chr(39) * 2)}'",
                    )
                    for key, value in {"target": "core_lib", "profile": "fast", "rust_min_stack": lanes.RUST_MIN_STACK_BYTES}.items():
                        body = body.replace("{{ " + key + " }}", value)
                    self.assertNotIn("{{", body)
                    wrapper = self.root / "wrapper.ps1"
                    wrapper.write_text(body + "\nexit $LASTEXITCODE\n", encoding="utf-8")
                    result = subprocess.run(
                        ["pwsh", "-NoProfile", "-File", str(wrapper), recipe, *arguments],
                        env=dict(os.environ, CODEX_CARGO_LANE_TARGET_DIR=str(self.target)),
                        capture_output=True, text=True, timeout=15,
                        creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
                    )
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    receipt = json.loads(result.stdout)
                    self.assertTrue(receipt["inherited_lane_reservation"])
                    self.assertEqual(
                        receipt["cargo_profile_directory"],
                        "dev-small" if recipe == "_core-test-small-reserved" else "debug",
                    )
        finally:
            lanes._release_binary_file_lock(handle)
            handle.close()

    def test_timeout_and_cancel_waiter_do_not_release_owner(self):
        with lanes.reserve_rust_test_target(self.target):
            with self.assertRaises(TimeoutError):
                with lanes.reserve_rust_test_target(self.target, timeout_seconds=.02):
                    self.fail("timed out waiter dispatched")
            with mock.patch.object(lanes.time, "sleep", side_effect=KeyboardInterrupt):
                with self.assertRaises(KeyboardInterrupt):
                    with lanes.reserve_rust_test_target(self.target):
                        self.fail("cancelled waiter dispatched")
            self.assertTrue(lanes.cargo_lock_is_busy(self.target))
        with lanes.reserve_rust_test_target(self.target):
            pass

    def test_failures_release_and_unconfirmed_cleanup_quarantines(self):
        for error in [ValueError("fixture failure"), KeyboardInterrupt()]:
            with self.assertRaises(type(error)):
                with lanes.reserve_rust_test_target(self.target):
                    raise error
            self.assertFalse(lanes.cargo_lock_is_busy(self.target))
        with self.assertRaises(runner.RunnerError):
            with lanes.reserve_rust_test_target(self.target):
                raise runner.RunnerError("unconfirmed", outcome="cleanup_failed")
        with self.assertRaisesRegex(RuntimeError, "quarantined"):
            with lanes.reserve_rust_test_target(self.target):
                self.fail("quarantined target admitted")

    def test_invalid_deadlines_reject_before_creating_target(self):
        for value in [0, -1, float("inf"), float("nan")]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                with lanes.reserve_rust_test_target(self.target, timeout_seconds=value):
                    self.fail("invalid deadline admitted")
        self.assertFalse(self.target.exists())

    @unittest.skipUnless(os.name == "nt", "Windows lane owner")
    def test_managed_lane_reservation_and_pruner_recognize_runner_lease(self):
        root = self.root / "codex-rs" / "target" / "lanes"
        lanes.initialize_cargo_lanes_root(self.root, root)
        target = root / "example"
        with lanes.reserve_rust_test_target(target):
            # The short root lock is not held through execution.
            with lanes.cargo_lane_coordination_lock(root, timeout_seconds=.05):
                self.assertIn("example", lanes.locked_lane_names([target]))
            with self.assertRaisesRegex(RuntimeError, "busy"):
                with lanes.reserve_cargo_lane(
                    repo_root=self.root, lane_root=root, requested_lane="example",
                    command=["cargo", "check"], warm_wait_seconds=0,
                ):
                    self.fail("busy lane reused")

    @unittest.skipUnless(os.name == "nt", "Windows PowerShell lease probe")
    def test_powershell_lane_probe_recognizes_runner_lease(self):
        script = lanes.REPO_ROOT / "scripts" / "cargo-lane.ps1"
        command = r'''
$tokens=$null; $errors=$null
$ast=[System.Management.Automation.Language.Parser]::ParseFile($args[0], [ref]$tokens, [ref]$errors)
if($errors.Count){throw $errors[0]}
foreach($name in @('Test-ExclusiveLaneFileBusy','Test-CargoLockBusy')) {
    $fn=$ast.Find({param($n) $n -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq $name}, $true)
    . ([scriptblock]::Create($fn.Extent.Text))
}
if(-not (Test-CargoLockBusy -TargetDir $args[1])){throw 'live runner lease not recognized'}
'''
        probe = self.root / "probe.ps1"
        probe.write_text(command)
        with lanes.reserve_rust_test_target(self.target):
            result = subprocess.run(
                ["pwsh", "-NoProfile", "-File", str(probe), str(script), str(self.target)],
                capture_output=True, text=True, timeout=15,
                creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            )
        self.assertEqual(result.returncode, 0, result.stderr)


class AdmissionDispatchTest(RunnerTestCase):
    def dispatch(self, command="run-gate", *, current_manifest=None, fingerprint=None, dependencies=None, cargo_profile=None):
        instance, _ = self.runner()
        instance.cargo_profile = cargo_profile
        args = argparse.Namespace(command=command, manifest=self.temp_dir / "manifest.toml",
                                  admission_timeout_seconds=.02, names=["fixture"], name="fixture")
        args.manifest.write_text("fixture")
        output = io.StringIO()
        ledger = {"fixture": ["b", "a"]}
        observations = []
        def evidence(*_args):
            self.assertTrue(lanes.cargo_lock_is_busy(self.target_dir))
            observations.append("evidence")
            return dependencies if dependencies is not None else {"automatic_replay_allowed": False}
        def execute(*_args, **_kwargs):
            self.assertTrue(lanes.cargo_lock_is_busy(self.target_dir))
            observations.append("execute")
            return ledger
        with (
            mock.patch.object(runner.Manifest, "load", return_value=current_manifest or instance.manifest),
            mock.patch.object(runner, "execution_dependency_manifest", side_effect=evidence),
            mock.patch.object(instance, "run_gates", side_effect=execute),
            mock.patch.object(instance, "run_target", side_effect=execute),
            mock.patch.object(instance, "check_gates", side_effect=execute),
            contextlib.redirect_stdout(output), contextlib.redirect_stderr(io.StringIO()),
        ):
            runner._dispatch_with_admission(args, instance, instance.metadata, fingerprint, [], False)
        return output.getvalue(), observations, ledger

    def test_admission_covers_fresh_evidence_execution_and_receipt_in_order(self):
        for command in ["run-gate", "run-target"]:
            with self.subTest(command=command):
                text, order, ledger = self.dispatch(command)
                receipt = json.loads(text)
                self.assertEqual(order, ["evidence", "execute"])
                self.assertEqual(receipt["completed_tests"], ledger)
                self.assertEqual(receipt["admission"]["target_dir"], str(self.target_dir.resolve()))
                self.assertGreaterEqual(receipt["admission"]["wait_seconds"], 0)
                self.assertFalse(receipt["dependency_manifest"]["automatic_replay_allowed"])
                self.assertFalse(lanes.cargo_lock_is_busy(self.target_dir))

    def test_check_gates_is_admitted_but_emits_no_execution_receipt(self):
        text, order, _ = self.dispatch("check-gates")
        self.assertEqual((text, order), ("", ["execute"]))

    def test_dispatch_uses_cargo_profile_not_nextest_profile_for_admission(self):
        profile = self.target_dir / "release"
        profile.mkdir()
        handle = lanes._try_acquire_binary_file_lock(profile / ".cargo-lock")
        self.assertIsNotNone(handle)
        try:
            for command in ("run-target", "run-gate", "check-gates"):
                with self.subTest(command=command):
                    text, order, _ = self.dispatch(command)
                    self.assertIn("execute", order)
                    if text:
                        self.assertEqual(json.loads(text)["admission"]["cargo_profile_directory"], "debug")
                    with self.assertRaises(runner.RunnerError) as error:
                        self.dispatch(command, cargo_profile="release")
                    self.assertEqual(error.exception.outcome, "timed_out")
        finally:
            lanes._release_binary_file_lock(handle)
            handle.close()

    def test_bulk_provenance_is_retained_before_execution_without_bloating_receipts(self):
        manifest = {
            "version": 1, "producer": "rust_test_runner", "captured": "before_execution",
            "file_inputs": [{"path": f"source/日本語/{i}/Cargo.toml", "sha256": "a" * 64}
                            for i in range(128)],
            "source_roots": [f"source/日本語/{i}" for i in range(128)],
            "omitted_file_inputs": 3, "omitted_source_roots": 2,
            "execution_context_sha256": "b" * 64,
            "coverage": "declared_not_exhaustive", "automatic_replay_allowed": False,
            "unresolved_dependencies": ["transitive_and_generated_inputs"],
        }
        for command in ["run-target", "run-gate"]:
            with self.subTest(command=command):
                text, order, ledger = self.dispatch(command, dependencies=manifest)
                receipt = json.loads(text)
                summary = receipt["dependency_manifest"]
                retained = summary["retained_manifest"]
                path = Path(retained["path"])
                self.assertTrue(path.is_absolute())
                self.assertEqual(path.parent, self.target_dir / "test-runner-logs")
                raw = path.read_bytes()
                self.assertEqual(len(raw), retained["bytes"])
                self.assertEqual(hashlib.sha256(raw).hexdigest(), retained["sha256"])
                self.assertEqual(json.loads(raw), manifest)
                self.assertLess(len(text.encode("utf-8")), len(raw) // 10)
                self.assertEqual(summary["file_inputs"], [])
                self.assertEqual(summary["source_roots"], [])
                self.assertEqual(summary["omitted_file_inputs"], 131)
                self.assertEqual(summary["omitted_source_roots"], 130)
                self.assertFalse(summary["automatic_replay_allowed"])
                self.assertEqual(summary["coverage"], "declared_not_exhaustive")
                self.assertEqual(receipt["completed_tests"], ledger)
                self.assertEqual(receipt["executed_tests"], 2)
                self.assertEqual(order, ["evidence", "execute"])
        with mock.patch.object(runner.RustTestRunner, "_retain_text", return_value=None):
            text, _, _ = self.dispatch(dependencies=manifest)
            self.assertEqual(json.loads(text)["dependency_manifest"], manifest)

    def test_changed_manifest_and_runner_fingerprint_fail_before_dispatch(self):
        with self.assertRaisesRegex(runner.RunnerError, "manifest changed"):
            self.dispatch(current_manifest=replace(self.manifest(), version=99))
        with self.assertRaisesRegex(runner.RunnerError, "runner inputs changed"):
            self.dispatch(fingerprint="stale")
        self.assertFalse(lanes.cargo_lock_is_busy(self.target_dir))

    def test_timeout_and_cancellation_are_classified_without_dispatch(self):
        with lanes.reserve_rust_test_target(self.target_dir):
            with self.assertRaises(runner.RunnerError) as timed:
                self.dispatch()
            self.assertEqual(timed.exception.outcome, "timed_out")
            with mock.patch.object(lanes.time, "sleep", side_effect=KeyboardInterrupt):
                with self.assertRaises(runner.RunnerError) as cancelled:
                    self.dispatch()
            self.assertEqual(cancelled.exception.outcome, "cancelled")
        self.assertFalse(lanes.cargo_lock_is_busy(self.target_dir))

    def test_cli_rejects_bad_timeout_without_metadata_or_execution(self):
        with mock.patch.object(runner, "load_metadata") as metadata:
            for value in ["0", "-1", "nan", "inf"]:
                with contextlib.redirect_stderr(io.StringIO()):
                    self.assertEqual(runner.main(["--admission-timeout-seconds", value, "check-manifest"]), 2)
            metadata.assert_not_called()


if __name__ == "__main__":
    unittest.main()
