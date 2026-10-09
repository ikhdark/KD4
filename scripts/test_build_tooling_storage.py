#!/usr/bin/env python3

import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import rust_build_status
from scripts import rust_build_status_support
from scripts import tool_versions
from scripts.build_tooling_test_support import REPO_ROOT
from scripts.build_tooling_test_support import load_toml
from scripts.build_tooling_test_support import powershell
from scripts.build_tooling_test_support import ps_single_quote


CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)


class WarmLaneReservationTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.root = self.repo / "codex-rs" / "target" / "lanes"
        rust_build_status.initialize_cargo_lanes_root(self.repo, self.root)

    def warm(self, name, *, profile="debug"):
        lane = self.root / name
        for part in (".fingerprint", "deps", "incremental"):
            (lane / profile / part).mkdir(parents=True)
        return lane

    def reserve(self, **kwargs):
        return rust_build_status.reserve_cargo_lane(
            repo_root=self.repo,
            lane_root=self.root,
            requested_lane="core-tests",
            command=["cargo", "nextest", "run"],
            **kwargs,
        )

    def test_reuses_busy_warm_lane_after_release_without_holding_coordination_lock(
        self,
    ):
        lane = self.warm("core-tests")
        owner = rust_build_status._try_acquire_binary_file_lock(
            lane / ".lane-active.lock"
        )
        self.assertIsNotNone(owner)

        def release_owner(_delay):
            with rust_build_status.cargo_lane_coordination_lock(
                self.root, timeout_seconds=0.01
            ):
                rust_build_status._release_binary_file_lock(owner)
                owner.close()

        try:
            with (
                mock.patch.object(
                    rust_build_status.time, "sleep", side_effect=release_owner
                ) as sleep,
                self.reserve(warm_wait_seconds=1) as (name, target),
            ):
                self.assertEqual((name, target), ("core-tests", lane))
                self.assertTrue(rust_build_status.lane_active_lock_is_held(target))
            self.assertEqual(sleep.call_count, 1)
            self.assertFalse((self.root / "core-tests-2").exists())
        finally:
            if not owner.closed:
                rust_build_status._release_binary_file_lock(owner)
                owner.close()

    def test_idle_warm_sibling_wins_without_waiting(self):
        base = self.warm("core-tests")
        sibling = self.warm("core-tests-2")
        for lane, used in ((base, 200), (sibling, 100)):
            stamp = lane / rust_build_status.LANE_LAST_USED_STAMP
            stamp.touch()
            os.utime(stamp, (used, used))
        with (
            self.reserve(),
            mock.patch.object(rust_build_status.time, "sleep") as sleep,
        ):
            with self.reserve() as (name, target):
                self.assertEqual((name, target), ("core-tests-2", sibling))
            sleep.assert_not_called()
        self.assertTrue(base.is_dir())

    def test_custom_profile_reuses_compatible_sibling_instead_of_empty_base(self):
        base = self.root / "core-tests"
        base.mkdir()
        sibling = self.warm("core-tests-2", profile="local-release")
        context = {"options": ["--profile", "local-release"]}
        for lane in (base, sibling):
            (lane / ".lane-build-context.json").write_text(json.dumps(context))

        with mock.patch.object(rust_build_status.time, "sleep") as sleep:
            with self.reserve(build_context=context) as (name, target):
                self.assertEqual((name, target), ("core-tests-2", sibling))
                self.assertTrue(rust_build_status.lane_active_lock_is_held(target))
            sleep.assert_not_called()
        self.assertFalse((base / ".lane-active.lock").exists())

    def test_partial_and_nested_profile_directories_are_not_warm(self):
        partial = self.root / "core-tests-2" / "local-release"
        for part in (".fingerprint", "deps"):
            (partial / part).mkdir(parents=True)
        nested = self.root / "core-tests-3" / "archive" / "local-release"
        for part in (".fingerprint", "deps", "incremental"):
            (nested / part).mkdir(parents=True)

        with self.reserve() as (name, _):
            self.assertEqual(name, "core-tests")
        self.assertFalse((partial.parent / ".lane-active.lock").exists())
        self.assertFalse((nested.parent.parent / ".lane-active.lock").exists())

    def test_indirect_profile_does_not_rank_a_lane_as_warm(self):
        external = self.repo / "external-profile"
        for part in (".fingerprint", "deps", "incremental"):
            (external / part).mkdir(parents=True)
        sentinel = external / "keep.txt"
        sentinel.write_text("keep")
        link = self.root / "core-tests-2" / "local-release"
        link.parent.mkdir()
        if os.name == "nt":
            result = subprocess.run(
                [
                    powershell(),
                    "-NoProfile",
                    "-Command",
                    f"New-Item -ItemType Junction -Path {ps_single_quote(link)} "
                    f"-Target {ps_single_quote(external)} | Out-Null",
                ],
                capture_output=True,
                text=True,
                timeout=30,
                creationflags=CREATE_NO_WINDOW,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
        else:
            link.symlink_to(external, target_is_directory=True)

        with self.reserve(warm_wait_seconds=0) as (name, _):
            self.assertEqual(name, "core-tests")
        self.assertEqual(sentinel.read_text(), "keep")
        self.assertFalse((link.parent / ".lane-active.lock").exists())
        self.assertFalse((external / ".lane-active.lock").exists())

    def test_wait_deadline_falls_back_to_cold_lane(self):
        self.warm("core-tests")
        now = [0.0]

        def advance(delay):
            now[0] += delay

        with (
            self.reserve(),
            mock.patch.object(
                rust_build_status.time, "monotonic", side_effect=lambda: now[0]
            ),
            mock.patch.object(rust_build_status.time, "sleep", side_effect=advance),
        ):
            with self.reserve(warm_wait_seconds=0.5, allow_cold_overflow=True) as (
                name,
                _,
            ):
                self.assertEqual(name, "core-tests-2")
            self.assertEqual(now[0], 0.5)

    def test_unconfirmed_cleanup_and_cold_lanes_do_not_delay_work(self):
        lane = self.warm("core-tests")
        (lane / ".lane-cleanup-unconfirmed").touch()
        with mock.patch.object(rust_build_status.time, "sleep") as sleep:
            with self.reserve() as (name, _):
                self.assertEqual(name, "core-tests-2")
            sleep.assert_not_called()

    def test_default_deadline_does_not_create_cold_sibling_or_release_owner(self):
        lane = self.warm("core-tests")
        with self.reserve(), mock.patch.object(rust_build_status.time, "sleep") as sleep:
            with self.assertRaisesRegex(RuntimeError, "no cold overflow"):
                with self.reserve():
                    self.fail("must not start cold work")
            sleep.assert_not_called()
            self.assertTrue(rust_build_status.lane_active_lock_is_held(lane))
            self.assertFalse((self.root / "core-tests-2").exists())

    def test_matching_recorded_settings_win_over_idle_incompatible_artifacts(self):
        base = self.warm("core-tests")
        sibling = self.warm("core-tests-2")
        (base / ".lane-build-context.json").write_text(json.dumps({"linker": "old"}))
        (sibling / ".lane-build-context.json").write_text(json.dumps({"linker": "new"}))
        with self.reserve(build_context={"linker": "new"}) as (_, selected):
            self.assertEqual(selected, sibling)
            with self.assertRaisesRegex(RuntimeError, "no cold overflow"):
                with self.reserve(build_context={"linker": "new"}, warm_wait_seconds=0):
                    self.fail(
                        "must not rebuild incompatible lane while matching owner runs"
                    )

    def test_explicit_family_prefers_recent_compatible_idle_sibling(self):
        base, sibling = self.warm("core-tests"), self.warm("core-tests-2")
        for lane, used in ((base, 100), (sibling, 200)):
            stamp = lane / rust_build_status.LANE_LAST_USED_STAMP
            stamp.touch()
            os.utime(stamp, (used, used))
        with self.reserve() as (_, selected):
            self.assertEqual(selected, sibling)
            self.assertTrue(rust_build_status.lane_active_lock_is_held(sibling))
            with self.reserve() as (_, other):
                self.assertEqual(other, base)

    def test_auto_lane_ignores_only_cargo_display_flags(self):
        base = ["cargo", "build"]
        identity = rust_build_status._auto_lane_base(base)
        for flags in (["--quiet"], ["-v"], ["--color", "always"], ["--color=never"]):
            self.assertEqual(identity, rust_build_status._auto_lane_base(base + flags))
        for flags in (
            ["--release"],
            ["--features", "other"],
            ["--", "--quiet"],
            ["--config", "build.rustflags=['--quiet']"],
        ):
            self.assertNotEqual(
                identity, rust_build_status._auto_lane_base(base + flags)
            )

    def test_context_tracks_inline_and_file_config_but_not_program_arguments(self):
        command = ["cargo", "build", "-p", "example"]
        context = lambda extra: rust_build_status.cargo_build_context(
            self.repo, command + extra, {}
        )
        plain = context([])
        self.assertNotEqual(plain, context(["--config", 'build.rustflags=["--cfg=a"]']))
        self.assertEqual(
            context(["--config", 'build.rustflags=["--cfg=a"]']),
            context(['--config=build.rustflags=["--cfg=a"]']),
        )
        self.assertNotEqual(
            context(["--config", 'build.rustflags=["--cfg=a"]']),
            context(["--config", 'build.rustflags=["--cfg=b"]']),
        )
        self.assertEqual(plain, context(["--", "--config", "ignored.toml"]))
        config = self.repo / "flags.toml"
        config.write_text('[build]\nrustflags = ["--cfg=a"]')
        before = context(["--config", str(config)])
        self.assertEqual(before, context(["--config=" + str(config)]))
        config.write_text('[build]\nrustflags = ["--cfg=b"]')
        self.assertNotEqual(before, context(["--config", str(config)]))
        relative = rust_build_status.cargo_build_context(
            self.repo,
            [
                "cargo",
                "-C",
                str(self.repo),
                "build",
                "-p",
                "example",
                "--config",
                "flags.toml",
            ],
            {},
        )
        self.assertEqual(
            relative["configs"][str(config)],
            context(["--config", str(config)])["configs"][str(config)],
        )

    def test_context_ignores_test_filters_but_tracks_build_inputs(self):
        args = ["cargo", "nextest", "run", "-p", "example"]
        context = rust_build_status.cargo_build_context(
            self.repo, args + ["-E", "test(a)"], {}
        )
        self.assertEqual(
            context,
            rust_build_status.cargo_build_context(
                self.repo, args + ["-E", "test(b)"], {}
            ),
        )
        self.assertNotEqual(
            context,
            rust_build_status.cargo_build_context(
                self.repo, args + ["--features", "x"], {}
            ),
        )
        self.assertNotEqual(
            context,
            rust_build_status.cargo_build_context(
                self.repo, args, {"RUSTFLAGS": "changed"}
            ),
        )
        self.assertNotEqual(
            context,
            rust_build_status.cargo_build_context(self.repo, args + ["--release"], {}),
        )
        # Nextest's --profile picks test policy; --cargo-profile picks artifacts.
        for profile in (["--profile", "local"], ["--profile=fast"], ["-P", "ci"]):
            self.assertEqual(
                context,
                rust_build_status.cargo_build_context(self.repo, args + profile, {}),
            )
        built = rust_build_status.cargo_build_context(
            self.repo, ["cargo", "build", "-p", "example", "--profile", "dev-small"], {}
        )
        for profile in (["--cargo-profile", "dev-small"], ["--cargo-profile=dev-small"]):
            self.assertEqual(
                built,
                rust_build_status.cargo_build_context(self.repo, args + profile, {}),
            )
        # Named core targets and gates share one lane by design; which one ran
        # last is not a build setting and must not rank that lane as changed.
        named = [
            ["just", "_core-test-reserved", "fast", "core_lib", "-E", "test(=a)"],
            ["just", "_core-test-reserved", "local", "core_windows"],
            ["just", "_core-gate-reserved", "core-retry-output", "windows-process"],
        ]
        self.assertEqual(
            len(
                {
                    json.dumps(
                        rust_build_status.cargo_build_context(self.repo, command, {}),
                        sort_keys=True,
                    )
                    for command in named
                }
            ),
            1,
        )
        config = self.repo / "codex-rs" / ".cargo" / "config.toml"
        config.parent.mkdir(parents=True)
        config.write_text('[build]\nrustflags = ["changed"]\n')
        self.assertNotEqual(
            context, rust_build_status.cargo_build_context(self.repo, args, {})
        )


    def test_named_runner_context_tracks_build_profile_not_test_policy(self):
        runner = [sys.executable, str(REPO_ROOT / "scripts" / "rust_test_runner.py")]
        context = lambda command: rust_build_status.cargo_build_context(
            self.repo, command, {}
        )
        plain = context([*runner, "run-target", "app_server_lib"])
        small = context([*runner, "--cargo-profile", "dev-small", "run-target", "app_server_lib"])
        self.assertNotEqual(plain, small)
        self.assertEqual(small["options"], ["--profile", "dev-small"])
        for policy in ("fast", "local"):
            for flags in (["--profile", policy], ["--profile=" + policy]):
                with self.subTest(flags=flags):
                    self.assertEqual(
                        plain, context([*runner, "run-target", *flags, "app_server_lib"])
                    )
                    self.assertEqual(
                        small, context([*runner, "--cargo-profile=dev-small", "run-target", *flags, "app_server_lib"])
                    )
        self.assertEqual(
            small, context(["just", "_core-test-small-reserved", "app_server_lib", "--profile", "local"])
        )
        self.assertEqual(
            plain, context(["just", "_core-test-reserved", "fast", "app_server_lib", "--profile", "local"])
        )
        self.assertEqual(
            plain, context(["just", "_core-gate-reserved", "example", "--profile", "local"])
        )
        # Only the exact local runner owns this profile contract.
        foreign = context([sys.executable, "foreign/rust_test_runner.py", "--profile", "local"])
        self.assertEqual(foreign["options"], ["--profile", "local"])
        opaque = context([sys.executable, "-X", "utf8", runner[1], "--profile", "local"])
        self.assertEqual(opaque["options"], ["--profile", "local"])

    def test_named_test_policy_change_uses_idle_compatible_lane(self):
        runner = [sys.executable, str(REPO_ROOT / "scripts" / "rust_test_runner.py")]
        command = lambda profile: [*runner, "run-target", "--profile", profile, "app_server_lib"]
        context = lambda profile: rust_build_status.cargo_build_context(
            self.repo, command(profile), {}
        )
        busy, idle = self.warm("core-tests"), self.warm("core-tests-2")
        (busy / ".lane-build-context.json").write_text(json.dumps(context("local")))
        (idle / ".lane-build-context.json").write_text(json.dumps(context("fast")))
        owner = rust_build_status._try_acquire_binary_file_lock(busy / ".lane-active.lock")
        self.assertIsNotNone(owner)
        try:
            with mock.patch.object(rust_build_status.time, "sleep") as sleep:
                with self.reserve(build_context=context("local"), warm_wait_seconds=0) as (_, selected):
                    self.assertEqual(selected, idle)
                    self.assertTrue(rust_build_status.lane_active_lock_is_held(busy))
                sleep.assert_not_called()
            self.assertFalse((self.root / "core-tests-3").exists())
        finally:
            rust_build_status._release_binary_file_lock(owner)
            owner.close()


class NamedRunnerLanePreflightTest(unittest.TestCase):
    def test_invalid_filters_fail_before_reservation_or_child_work(self):
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)
            runner = REPO_ROOT / "scripts" / "rust_test_runner.py"
            for filters in (
                ["--offline"],
                ["-p", "another-package"],
                ["--no-tests=pass"],
                ["--profile"],
                ["--command-timeout-seconds", "0"],
                ["--", "--nocapture"],
            ):
                with (
                    self.subTest(filters=filters),
                    mock.patch.object(rust_build_status, "reserve_cargo_lane") as reserve,
                    mock.patch.object(rust_build_status, "run_owned") as run,
                    mock.patch.object(rust_build_status, "local_rust_env") as setup,
                    self.assertRaises(ValueError),
                ):
                    rust_build_status.run_in_cargo_lane(
                        repo_root=repo, requested_lane="core-tests",
                        command=[sys.executable, str(runner), "run-target", "app_server_lib", "--", *filters],
                    )
                reserve.assert_not_called()
                setup.assert_not_called()
                run.assert_not_called()
            self.assertFalse((repo / "codex-rs" / "target").exists())

    def test_preflight_preserves_valid_arguments_without_loading_metadata(self):
        from scripts import rust_test_runner

        runner = REPO_ROOT / "scripts" / "rust_test_runner.py"
        for arguments in (
            ["run-target", "app_server_lib", "--", "-E", "test(example)"],
            ["run-target", "core_lib", "--all", "--no-fail-fast", "--profile", "fast"],
            ["--target-dir", "relative", "run-target", "app_server_lib", "-E", "test(example)", "--", "--exact", "example", "--skip", "excluded"],
            ["run-target", "app_server_lib", "--command-timeout-seconds=30"],
            ["run-gate", "example", "--profile", "fast"],
        ):
            command = [sys.executable, str(runner), *arguments]
            original = list(command)
            with (
                self.subTest(arguments=arguments),
                mock.patch.object(rust_test_runner, "load_metadata") as metadata,
                mock.patch.object(rust_test_runner.Manifest, "load") as manifest,
            ):
                rust_build_status._guard_named_runner_args(command)
            self.assertEqual(command, original)
            metadata.assert_not_called()
            manifest.assert_not_called()

    def test_opaque_foreign_and_help_commands_remain_with_their_owners(self):
        from scripts import rust_test_runner

        runner = str(REPO_ROOT / "scripts" / "rust_test_runner.py")
        for command in (
            ["python", "other/rust_test_runner.py", "run-target", "example", "--offline"],
            ["python", "-X", "utf8", runner, "run-target", "example"],
            ["other-interpreter", runner, "run-target", "example"],
            [sys.executable, runner, "--help"],
        ):
            with (
                self.subTest(command=command),
                mock.patch.object(rust_test_runner, "build_parser") as parser,
            ):
                rust_build_status._guard_named_runner_args(command)
            parser.assert_not_called()

    def test_parser_failure_retains_lane_timing_without_reserving(self):
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)
            timing = repo / "timing.json"
            with (
                mock.patch.object(rust_build_status, "reserve_cargo_lane") as reserve,
                contextlib.redirect_stderr(io.StringIO()),
                self.assertRaisesRegex(ValueError, "no build lane was reserved"),
            ):
                rust_build_status.run_in_cargo_lane(
                    repo_root=repo, requested_lane="core-tests", timing_path=timing,
                    command=[sys.executable, str(REPO_ROOT / "scripts" / "rust_test_runner.py"), "run-target"],
                )
            reserve.assert_not_called()
            receipt = json.loads(timing.read_text())
            self.assertEqual(receipt["status"], "error")
            self.assertIsNone(receipt["resolvedLane"])
            self.assertIsNone(receipt["phaseDurationsMs"]["command"])


class BuildToolingStorageTest(unittest.TestCase):
    def test_wrong_checkout_cwd_fails_before_lane_reservation(self):
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)
            (repo / "codex-rs").mkdir()
            (repo / "codex-rs" / "Cargo.toml").write_text("[workspace]\n")
            timing = repo / "timing.json"
            with (
                contextlib.chdir(repo),
                mock.patch.object(
                    rust_build_status, "reserve_cargo_lane",
                    side_effect=RuntimeError("must not wait for a lane"),
                ) as reserve,
                mock.patch.object(rust_build_status, "run_owned") as run,
                self.assertRaisesRegex(ValueError, "working directory.*codex-rs"),
            ):
                rust_build_status.run_in_cargo_lane(
                    repo_root=repo, requested_lane="core-tests",
                    command=["cargo", "test", "-p", "codex-git-utils", "--lib"],
                    warm_wait_seconds=600, timing_path=timing,
                )
            reserve.assert_not_called()
            run.assert_not_called()
            self.assertFalse((repo / "codex-rs" / "target").exists())
            receipt = json.loads(timing.read_text())
            self.assertEqual(receipt["status"], "error")
            self.assertIsNone(receipt["resolvedLane"])
            self.assertIsNone(receipt["phaseDurationsMs"]["command"])

    def test_checkout_cwd_guard_preserves_explicit_paths_and_non_build_commands(self):
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)
            rust = repo / "codex-rs"
            rust.mkdir()
            (rust / "Cargo.toml").write_text("[workspace]\n")
            allowed = [
                ["cargo", "check", "--manifest-path", "codex-rs/Cargo.toml"],
                ["cargo", "check", "--manifest-path=codex-rs/Cargo.toml"],
                ["cargo", "-C", "codex-rs", "check"],
                ["cargo", "-Ccodex-rs", "check"],
                ["cargo", "check", "--help"],
                ["cargo", "test", "-h"],
                ["cargo", "--version"],
                ["cargo", "install", "tool"],
                ["cargo", "nextest", "run", "--archive-file", "tests.tar.zst"],
                ["cargo", "watch", "-C", "codex-rs", "-xcheck"],
                ["python", "build.py"],
                [],
            ]
            with contextlib.chdir(repo):
                for command in allowed:
                    with self.subTest(command=command):
                        rust_build_status._guard_cargo_checkout_cwd(command, repo)
                for command in [
                    ["cargo", "check"],
                    ["cargo.exe", "+nightly", "--color", "always", "build"],
                    ["cargo", "t", "-p", "example"],
                    # Flags forwarded to the executable are not Cargo overrides.
                    ["cargo", "run", "--", "--manifest-path", "other.toml"],
                    ["cargo", "test", "--", "--help"],
                ]:
                    with self.subTest(command=command), self.assertRaises(ValueError):
                        rust_build_status._guard_cargo_checkout_cwd(command, repo)
            with contextlib.chdir(rust):
                rust_build_status._guard_cargo_checkout_cwd(["cargo", "check"], repo)
            # A real manifest in the checkout or an ancestor still belongs to Cargo.
            with contextlib.chdir(repo):
                (repo / "Cargo.toml").write_text("[workspace]\n")
                rust_build_status._guard_cargo_checkout_cwd(["cargo", "check"], repo)
            nested = repo / "nested"
            (nested / "codex-rs").mkdir(parents=True)
            (nested / "codex-rs" / "Cargo.toml").write_text("[workspace]\n")
            with contextlib.chdir(nested):
                rust_build_status._guard_cargo_checkout_cwd(["cargo", "check"], nested)

    def test_dry_prune_reuses_target_inventory_with_independent_partition_budgets(self):
        for location in ("lanes", "nested/lanes", "external"):
            with self.subTest(location=location), tempfile.TemporaryDirectory() as temp:
                repo = Path(temp)
                target = repo / "codex-rs" / "target"
                root = (
                    repo / "external" if location == "external" else target / location
                )
                rust_build_status.initialize_cargo_lanes_root(repo, root)
                lane = root / "warm"
                lane.mkdir()
                size = rust_build_status_support.HARDLINK_CHECK_MIN_BYTES
                artifact = lane / "artifact"
                artifact.write_bytes(b"a" * size)
                for part in ("debug", "release"):
                    profile = target / part
                    profile.mkdir(parents=True)
                    os.link(artifact, profile / "binary")
                    os.link(artifact, profile / "uplift")
                os.link(artifact, target / "root-binary")
                os.link(artifact, target / "root-uplift")
                with mock.patch.dict(os.environ, {"CODEX_CARGO_LANES_ROOT": str(root)}):
                    snapshot = rust_build_status.BuildStatusSnapshot.collect(
                        repo_root=repo, processes=[]
                    )
                    full = rust_build_status_support.directory_size_bytes(target)
                    with mock.patch.object(
                        rust_build_status_support.os, "scandir", wraps=os.scandir
                    ) as scans:
                        self.assertEqual(snapshot.target_size(), full)
                        self.assertEqual(
                            snapshot.non_lane_size(root, size_workers=1), (4 * size, 0)
                        )
                    # Each real directory is traversed exactly once; shared links
                    # count once globally, separately in each budget partition.
                    visited = [str(call.args[0]) for call in scans.call_args_list]
                    self.assertEqual(len(visited), len(set(visited)))
                    with (
                        mock.patch.object(
                            rust_build_status,
                            "directory_size_bytes",
                            side_effect=AssertionError("rescan"),
                        ),
                        mock.patch.object(
                            rust_build_status,
                            "target_non_lane_size_bytes",
                            side_effect=AssertionError("rescan"),
                        ),
                    ):
                        # External lane contents were not in the target inventory.
                        if location == "external":
                            snapshot._lane_sizes[lane] = (size, 0)
                        report = rust_build_status.prune_stale_lanes_report(
                            repo_root=repo,
                            snapshot=snapshot,
                            dry_run=True,
                            max_age_days=None,
                            max_total_target_bytes=4 * size,
                        )
                    self.assertIn(f"would prune: {lane}", report)
                    self.assertIn(
                        f"target disk: {rust_build_status.format_bytes(full[0])}",
                        report,
                    )
                    self.assertTrue(artifact.is_file())

    def test_direct_lane_and_runner_share_setup_and_record_success_only(self):
        from scripts.rust_tool_env import local_rust_env

        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)
            command = ["cargo", "check", "-p", "example"]
            target = repo / "codex-rs" / "target" / "lanes" / "unit"
            which = lambda name, **_kwargs: f"/tools/{name}"
            expected = local_rust_env({}, repo_root=repo, which=which)
            for code in (0, 7):
                with (
                    mock.patch.dict(os.environ, {}, clear=True),
                    mock.patch.object(
                        rust_build_status.shutil, "which", side_effect=which
                    ),
                    mock.patch.object(
                        rust_build_status, "request_cargo_lane_maintenance"
                    ),
                    mock.patch.object(
                        rust_build_status,
                        "run_owned",
                        return_value=subprocess.CompletedProcess(command, code),
                    ) as run,
                    contextlib.redirect_stderr(io.StringIO()),
                ):
                    self.assertEqual(
                        rust_build_status.run_in_cargo_lane(
                            repo_root=repo, requested_lane="unit", command=command
                        ),
                        code,
                    )
                for key, value in expected.items():
                    self.assertEqual(run.call_args.kwargs["env"][key], value)
                self.assertFalse(rust_build_status.lane_active_lock_is_held(target))
                marker = target / ".lane-build-context.json"
                if code == 0:
                    self.assertEqual(
                        json.loads(marker.read_text())["environment"]["RUSTC_WRAPPER"],
                        "/tools/sccache",
                    )
                    marker.unlink()
                else:
                    self.assertFalse(marker.exists())

    def test_generic_lane_command_receives_default_test_stack(self):
        # `just coverage-lane` and `just cargo-lane <lane> cargo test` run
        # libtest workers without nextest's Windows stack setup script.
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)
            command = ["cargo", "llvm-cov", "-p", "example"]
            for inherited, expected in ((None, "8388608"), ("42", "42")):
                env = {} if inherited is None else {"RUST_MIN_STACK": inherited}
                with (
                    self.subTest(inherited=inherited),
                    mock.patch.dict(os.environ, env, clear=True),
                    mock.patch.object(
                        rust_build_status.shutil, "which", return_value=None
                    ),
                    mock.patch.object(
                        rust_build_status, "request_cargo_lane_maintenance"
                    ),
                    mock.patch.object(
                        rust_build_status,
                        "run_owned",
                        return_value=subprocess.CompletedProcess(command, 0),
                    ) as run,
                    contextlib.redirect_stderr(io.StringIO()),
                ):
                    self.assertEqual(
                        rust_build_status.run_in_cargo_lane(
                            repo_root=repo, requested_lane="coverage", command=command
                        ),
                        0,
                    )
                    self.assertEqual(
                        run.call_args.kwargs["env"]["RUST_MIN_STACK"], expected
                    )

    def test_lane_timing_cli_separates_phases_and_preserves_failed_child(self):
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)
            output = repo / "timing.json"
            target = repo / "codex-rs" / "target" / "lanes" / "unit"

            def child(command, *, env, check, stdout, stderr, prepare_sccache, creationflags):
                self.assertEqual(creationflags, getattr(subprocess, "BELOW_NORMAL_PRIORITY_CLASS", 0))
                self.assertTrue(prepare_sccache)
                self.assertIs(stdout, sys.stdout)
                self.assertIs(stderr, sys.stderr)
                self.assertTrue(rust_build_status.lane_active_lock_is_held(target))
                self.assertNotIn("CARGO_TARGET_DIR", env)
                self.assertEqual(
                    command[-4:],
                    ["--target-dir", str(target.resolve()), "-p", "example"],
                )
                return subprocess.CompletedProcess(command, 7)

            with (
                mock.patch.dict(os.environ, {}, clear=True),
                mock.patch.object(
                    rust_build_status, "request_cargo_lane_maintenance"
                ) as maintain,
                mock.patch.object(rust_build_status, "run_owned", side_effect=child),
                mock.patch.object(
                    rust_build_status.time,
                    "perf_counter_ns",
                    side_effect=[
                        0,
                        2_000_000,
                        5_000_000,
                        12_000_000,
                        23_000_000,
                        24_000_000,
                    ],
                ),
            ):
                self.assertEqual(
                    rust_build_status.main(
                        [
                            "run-lane",
                            "--repo-root",
                            str(repo),
                            "--lane",
                            "unit",
                            "--timing-json",
                            str(output),
                            "--",
                            "cargo",
                            "check",
                            "-p",
                            "example",
                        ]
                    ),
                    7,
                )
            record = json.loads(output.read_text())
            self.assertEqual(
                record["phaseDurationsMs"],
                {
                    "reservation": 2,
                    "setup": 3,
                    "maintenance": 7,
                    "command": 11,
                    "release": 1,
                },
            )
            self.assertEqual(record["totalMs"], 24)
            self.assertEqual(
                (record["schemaVersion"], record["status"], record["exitCode"]),
                (1, "failed", 7),
            )
            self.assertEqual(record["resolvedLane"], "unit")
            self.assertEqual(record["command"], ["cargo", "check", "-p", "example"])
            maintain.assert_called_once_with(repo, target.parent.resolve())
            self.assertFalse(rust_build_status.lane_active_lock_is_held(target))

    def test_lane_timing_records_reservation_failure_without_launch(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / "timing.json"
            with (
                contextlib.chdir(REPO_ROOT / "codex-rs"),
                mock.patch.object(
                    rust_build_status,
                    "reserve_cargo_lane",
                    side_effect=RuntimeError("busy"),
                ),
                mock.patch.object(rust_build_status, "run_owned") as child,
                mock.patch.object(
                    rust_build_status.time,
                    "perf_counter_ns",
                    side_effect=[0, 9_000_000],
                ),
                contextlib.redirect_stderr(io.StringIO()),
            ):
                self.assertEqual(
                    rust_build_status.main(
                        [
                            "run-lane",
                            "--lane",
                            "unit",
                            "--timing-json",
                            str(output),
                            "--",
                            "cargo",
                            "check",
                        ]
                    ),
                    2,
                )
            child.assert_not_called()
            record = json.loads(output.read_text())
            self.assertEqual(
                record["phaseDurationsMs"],
                {
                    "reservation": 9,
                    "setup": None,
                    "maintenance": None,
                    "command": None,
                    "release": None,
                },
            )
            self.assertEqual(record["status"], "error")
            self.assertEqual(record["errorType"], "RuntimeError")
            self.assertIsNone(record["exitCode"])
            # Reusing the path must preserve earlier evidence and prevent a build.
            with (
                mock.patch.object(rust_build_status, "run_owned") as child,
                contextlib.redirect_stderr(io.StringIO()),
            ):
                self.assertEqual(
                    rust_build_status.main(
                        [
                            "run-lane",
                            "--lane",
                            "unit",
                            "--timing-json",
                            str(output),
                            "--",
                            "cargo",
                            "check",
                        ]
                    ),
                    2,
                )
            child.assert_not_called()
            self.assertEqual(json.loads(output.read_text()), record)

    def test_indirect_roots_are_rejected_before_launch_or_prune(self):
        for source in ("default", "environment", "explicit", "ancestor"):
            with self.subTest(source=source), tempfile.TemporaryDirectory() as temp:
                repo = Path(temp) / "repo"
                external = Path(temp) / "external"
                external.mkdir()
                lanes = repo / "codex-rs" / "target" / "lanes"
                link = lanes if source in {"default", "ancestor"} else repo / "custom"
                if source == "ancestor":
                    link = lanes.parent
                link.parent.mkdir(parents=True)
                outside_lanes = external / "lanes" if source == "ancestor" else external
                sentinel = outside_lanes / "family-photos" / "keep.txt"
                sentinel.parent.mkdir(parents=True)
                sentinel.write_text("keep", encoding="utf-8")
                if os.name == "nt":
                    result = subprocess.run(
                        [
                            powershell(),
                            "-NoProfile",
                            "-Command",
                            f"New-Item -ItemType Junction -Path {ps_single_quote(link)} "
                            f"-Target {ps_single_quote(external)} | Out-Null",
                        ],
                        capture_output=True,
                        text=True,
                        timeout=30,
                        creationflags=CREATE_NO_WINDOW,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                else:
                    link.symlink_to(external, target_is_directory=True)
                selected = lanes if source == "ancestor" else link
                env = (
                    {}
                    if source in {"default", "ancestor"}
                    else {"CODEX_CARGO_LANES_ROOT": str(selected)}
                )
                with (
                    mock.patch.dict(os.environ, env, clear=True),
                    mock.patch.object(rust_build_status, "run_owned") as child,
                    contextlib.redirect_stderr(io.StringIO()),
                ):
                    arguments = ["run-lane", "--repo-root", str(repo), "--lane", "unit"]
                    if source == "explicit":
                        arguments += ["--lanes-root", str(selected)]
                    self.assertEqual(
                        rust_build_status.main([*arguments, "--", "cargo", "check"]), 2
                    )
                    with self.assertRaisesRegex(
                        rust_build_status.CargoLanesRootValidationError, "indirect"
                    ):
                        rust_build_status.prune_stale_lanes(
                            repo_root=repo,
                            processes=[],
                            keep_warm_per_base=0,
                        )
                child.assert_not_called()
                self.assertEqual(sentinel.read_text(encoding="utf-8"), "keep")
                self.assertEqual(
                    sorted(path.name for path in outside_lanes.iterdir()),
                    ["family-photos"],
                )

    def test_disappearing_profile_does_not_authorize_pruning(self):
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)
            lane = repo / "codex-rs" / "target" / "lanes" / "unit"
            profile = lane / "debug"
            profile.mkdir(parents=True)
            sentinel = lane / "keep.txt"
            sentinel.write_text("keep", encoding="utf-8")
            original = Path.iterdir

            def raced_iterdir(path):
                if path == profile:
                    raise FileNotFoundError("profile disappeared during inspection")
                return original(path)

            with mock.patch.object(Path, "iterdir", raced_iterdir):
                self.assertTrue(rust_build_status.cargo_lock_is_busy(lane))
                self.assertEqual(
                    rust_build_status.prune_stale_lanes(
                        repo_root=repo,
                        processes=[],
                        keep_warm_per_base=0,
                    ),
                    [],
                )
            self.assertEqual(sentinel.read_text(encoding="utf-8"), "keep")

    def test_failed_deletion_does_not_stamp_success_and_retries_are_throttled(self):
        for error in (
            PermissionError("held file"),
            FileNotFoundError("child vanished"),
        ):
            with self.subTest(error=error), tempfile.TemporaryDirectory() as temp:
                repo = Path(temp)
                lanes = repo / "codex-rs" / "target" / "lanes"
                old = lanes / "old"
                old.mkdir(parents=True)
                (old / "artifact").write_text("old", encoding="utf-8")
                (old / ".lane-last-used").touch()
                os.utime(old / ".lane-last-used", (1, 1))
                with (
                    mock.patch.dict(os.environ, {}, clear=True),
                    mock.patch.object(
                        rust_build_status, "active_rust_processes", return_value=[]
                    ),
                    contextlib.redirect_stderr(io.StringIO()),
                ):
                    with mock.patch.object(
                        rust_build_status,
                        "remove_tree_allow_readonly",
                        side_effect=error,
                    ) as remove:
                        rust_build_status.maintain_cargo_lanes(repo, lanes)
                        self.assertEqual(remove.call_count, 1)
                    trash = list(lanes.glob("old.trash-*"))
                    self.assertEqual(len(trash), 1)
                    self.assertEqual((trash[0] / "artifact").read_text(), "old")
                    self.assertFalse((lanes / ".gc-stamp").exists())
                    retry = lanes / ".gc-retry"
                    self.assertTrue(retry.is_file())
                    with mock.patch.object(
                        rust_build_status, "prune_stale_lanes"
                    ) as prune:
                        rust_build_status.maintain_cargo_lanes(repo, lanes)
                        prune.assert_not_called()
                    os.utime(retry, (1, 1))
                    rust_build_status.maintain_cargo_lanes(repo, lanes)
                self.assertFalse(list(lanes.glob("*.trash-*")))
                self.assertTrue((lanes / ".gc-stamp").is_file())
                self.assertFalse(retry.exists())

    def test_maintenance_skips_held_lock_then_rechecks_success_stamp(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            handle = rust_build_status._try_acquire_binary_file_lock(
                root / ".lane-gc.lock"
            )
            self.assertIsNotNone(handle)
            try:
                with mock.patch.object(rust_build_status, "prune_stale_lanes") as prune:
                    rust_build_status.maintain_cargo_lanes(root, root)
                    prune.assert_not_called()
                self.assertFalse((root / ".gc-stamp").exists())
            finally:
                rust_build_status._release_binary_file_lock(handle)
                handle.close()
            acquire = rust_build_status._try_acquire_binary_file_lock

            def completed_while_acquiring(path):
                (root / ".gc-stamp").write_text(
                    "another caller completed", encoding="utf-8"
                )
                return acquire(path)

            with (
                mock.patch.dict(os.environ, {}, clear=True),
                mock.patch.object(
                    rust_build_status,
                    "_try_acquire_binary_file_lock",
                    side_effect=completed_while_acquiring,
                ),
                mock.patch.object(rust_build_status, "prune_stale_lanes") as prune,
            ):
                rust_build_status.maintain_cargo_lanes(root, root)
                prune.assert_not_called()
            self.assertEqual(
                (root / ".gc-stamp").read_text(), "another caller completed"
            )

    def test_run_lane_proceeds_while_another_process_owns_maintenance(self):
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)
            lanes = repo / "codex-rs" / "target" / "lanes"
            rust_build_status.initialize_cargo_lanes_root(repo, lanes)
            lock = rust_build_status._try_acquire_binary_file_lock(
                lanes / ".lane-gc.lock"
            )
            self.assertIsNotNone(lock)
            with (
                lock,
                mock.patch.dict(os.environ, {"CODEX_CARGO_LANE_MAINTENANCE_SYNC": "1"}),
                mock.patch.object(rust_build_status, "prune_stale_lanes") as prune,
            ):
                result = rust_build_status.run_in_cargo_lane(
                    repo_root=repo,
                    requested_lane="unit",
                    command=[sys.executable, "-c", "raise SystemExit(7)"],
                )
                self.assertEqual(result, 7)
                prune.assert_not_called()
                self.assertFalse((lanes / ".gc-stamp").exists())

    def test_maintenance_rechecks_stamp_after_acquiring_lock(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            acquire = rust_build_status._try_acquire_binary_file_lock

            def finish_other_maintenance(path):
                (root / ".gc-stamp").write_text("other command completed")
                return acquire(path)

            with (
                mock.patch.dict(os.environ, {}, clear=True),
                mock.patch.object(
                    rust_build_status,
                    "_try_acquire_binary_file_lock",
                    side_effect=finish_other_maintenance,
                ),
                mock.patch.object(rust_build_status, "prune_stale_lanes") as prune,
            ):
                rust_build_status.maintain_cargo_lanes(root, root)
            prune.assert_not_called()
            self.assertEqual(
                (root / ".gc-stamp").read_text(), "other command completed"
            )
            with acquire(root / ".lane-gc.lock") as released:
                self.assertIsNotNone(released)

    def test_auto_lane_prefers_last_used_stamp_over_directory_mtime(self):
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)
            lanes = repo / "codex-rs" / "target" / "lanes"
            rust_build_status.initialize_cargo_lanes_root(repo, lanes)
            for name, stamp_time, dir_time in (("probe-2", 10, 30), ("probe-3", 20, 1)):
                lane = lanes / name
                lane.mkdir()
                stamp = lane / ".lane-last-used"
                stamp.touch()
                os.utime(stamp, (stamp_time, stamp_time))
                os.utime(lane, (dir_time, dir_time))
            with rust_build_status.reserve_cargo_lane(
                repo_root=repo,
                requested_lane="auto",
                command=["cargo", "check", "-p=probe"],
            ) as (name, target):
                self.assertEqual(name, "probe-3")
                self.assertEqual(target, lanes / "probe-3")

    def test_run_lane_maintains_before_failed_child_without_forcing_post_cleanup(self):
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)
            lanes = repo / "codex-rs" / "target" / "lanes"
            rust_build_status.initialize_cargo_lanes_root(repo, lanes)
            old = lanes / "old"
            old.mkdir()
            (old / "artifact").write_text("old", encoding="utf-8")
            old_stamp = old / ".lane-last-used"
            old_stamp.write_text("old", encoding="utf-8")
            os.utime(old_stamp, (1, 1))
            late = lanes / "late"
            # The child proves pre-cleanup happened. An expired lane it creates
            # must survive until scheduled or explicitly requested maintenance.
            script = (
                "import os,pathlib,sys; "
                "assert not pathlib.Path(sys.argv[1]).exists(); "
                "p=pathlib.Path(sys.argv[2]); p.mkdir(); os.utime(p,(1,1)); sys.exit(7)"
            )
            with (
                mock.patch.dict(os.environ, {"CODEX_CARGO_LANE_MAINTENANCE_SYNC": "1"}),
                mock.patch.object(
                    rust_build_status, "active_rust_processes", return_value=[]
                ),
                mock.patch.object(
                    rust_build_status, "directory_sizes_bytes"
                ) as lane_sizes,
                mock.patch.object(
                    rust_build_status, "target_non_lane_size_bytes"
                ) as target_size,
            ):
                result = rust_build_status.run_in_cargo_lane(
                    repo_root=repo,
                    requested_lane="unit",
                    command=[sys.executable, "-c", script, str(old), str(late)],
                )
            self.assertEqual(result, 7)
            self.assertFalse(old.exists())
            self.assertTrue(late.exists())
            self.assertTrue((lanes / "unit").is_dir())
            self.assertTrue((lanes / ".gc-stamp").is_file())
            lane_sizes.assert_not_called()
            target_size.assert_not_called()

    def test_maintenance_honors_budgets_interval_and_failure(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            with (
                mock.patch.dict(os.environ, {}, clear=True),
                mock.patch.object(rust_build_status, "prune_stale_lanes") as prune,
            ):
                rust_build_status.maintain_cargo_lanes(root, root)
                self.assertIsNone(prune.call_args.kwargs["max_total_lane_bytes"])
                self.assertIsNone(prune.call_args.kwargs["max_total_target_bytes"])
                rust_build_status.maintain_cargo_lanes(root, root)
                self.assertEqual(prune.call_count, 1)
                stamp = root / ".gc-stamp"
                previous = stamp.read_bytes()
                os.utime(stamp, (1, 1))
                os.environ["CODEX_CARGO_LANE_MAX_TOTAL_BYTES"] = str(200 * 1024**3)
                os.environ["CODEX_CARGO_TARGET_MAX_TOTAL_BYTES"] = str(250 * 1024**3)
                prune.side_effect = OSError("failed maintenance")
                rust_build_status.maintain_cargo_lanes(root, root)
                self.assertEqual(prune.call_count, 2)
                self.assertEqual(
                    prune.call_args.kwargs["max_total_lane_bytes"], 200 * 1024**3
                )
                self.assertEqual(
                    prune.call_args.kwargs["max_total_target_bytes"], 250 * 1024**3
                )
                self.assertEqual(stamp.read_bytes(), previous)
                self.assertNotIn("CODEX_CARGO_LANES_ROOT", os.environ)

    def test_configured_lane_root_and_nested_disk_accounting(self):
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)
            lanes = repo / "codex-rs" / "target" / "nested" / "lanes"
            with mock.patch.dict(os.environ, {"CODEX_CARGO_LANES_ROOT": str(lanes)}):
                with rust_build_status.reserve_cargo_lane(
                    repo_root=repo, requested_lane="check", command=["cargo", "check"]
                ) as (_, target):
                    self.assertEqual(target.parent, lanes)
                    (target / "artifact").write_bytes(b"0123456789")
                    outside = lanes.parent / "outside"
                    outside.write_bytes(b"abc")
                    size, errors = rust_build_status_support.target_non_lane_size_bytes(
                        repo_root=repo, lane_root=lanes
                    )
                    self.assertEqual((size, errors), (3, 0))

    def test_disk_metadata_failure_is_counted(self):
        entry = mock.Mock()
        entry.is_junction.side_effect = OSError("denied")
        entries = mock.MagicMock()
        entries.__enter__.return_value = iter([entry])
        with (
            tempfile.TemporaryDirectory() as temp,
            mock.patch.object(
                rust_build_status_support.os, "scandir", return_value=entries
            ),
        ):
            self.assertEqual(
                rust_build_status_support.directory_size_bytes(Path(temp)), (0, 1)
            )

    def test_disk_accounting_counts_cargo_hardlinks_once(self):
        with tempfile.TemporaryDirectory() as temp:
            lane = Path(temp)
            size = rust_build_status_support.HARDLINK_CHECK_MIN_BYTES
            original = lane / "debug" / "deps" / "codex-abc123.exe"
            original.parent.mkdir(parents=True)
            original.write_bytes(b"x" * size)
            # Cargo uplifts binaries by hardlinking them into the profile dir.
            os.link(original, lane / "debug" / "codex.exe")
            (lane / "debug" / "small.d").write_bytes(b"abc")

            self.assertEqual(
                rust_build_status_support.directory_size_bytes(lane), (size + 3, 0)
            )

    def test_lane_rejects_reserved_trash_namespace_before_creation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "lanes"
            with self.assertRaisesRegex(ValueError, "invalid Cargo lane"):
                with rust_build_status.reserve_cargo_lane(
                    repo_root=Path(directory),
                    requested_lane="active.trash-20260102030405000",
                    command=["cargo", "check"],
                    lane_root=root,
                ):
                    self.fail("reserved a trash path")
            self.assertFalse(root.exists())

    def test_build_aliases_and_nextest_list_receive_target(self):
        target = Path("lane").resolve()
        for arguments in (
            [name]
            for name in (
                "b",
                "c",
                "t",
                "r",
                "d",
                "clean",
                "rustdoc",
                "package",
                "install",
                "publish",
            )
        ):
            command = ["cargo", *arguments]
            self.assertEqual(
                rust_build_status._cargo_command_with_target_dir(command, target),
                [*command, "--target-dir", str(target)],
            )
        self.assertEqual(
            rust_build_status._cargo_command_with_target_dir(
                ["cargo", "nextest", "list"], target
            ),
            ["cargo", "nextest", "list", "--target-dir", str(target)],
        )
        with self.assertRaisesRegex(ValueError, "Unsupported Cargo command"):
            rust_build_status._cargo_command_with_target_dir(
                ["cargo", "local-alias"], target
            )

    def test_every_watch_exec_and_default_obey_reserved_target(self):
        target = Path("lane").resolve()
        updated = rust_build_status._cargo_command_with_target_dir(
            ["cargo", "watch", "-x", "test --", "--exec=check", "--"], target
        )
        self.assertEqual(
            updated,
            [
                "cargo",
                "watch",
                "-x",
                f"test --target-dir {target} --",
                f"--exec=check --target-dir {target}",
                "--",
            ],
        )
        self.assertEqual(
            rust_build_status._cargo_command_with_target_dir(
                ["cargo", "watch", "--"], target
            ),
            ["cargo", "watch", "-x", f"check --target-dir {target}", "--"],
        )
        with self.assertRaisesRegex(ValueError, "--shell"):
            rust_build_status._cargo_command_with_target_dir(
                ["cargo", "watch", "-x", "check", "-s", "cargo test"], target
            )

    def test_watch_attached_short_options_cannot_bypass_reserved_target(self):
        target = Path("lane").resolve()
        for option in ("-scommand", "-s=command", "-qscommand", "-cqs=command"):
            with (
                self.subTest(option=option),
                self.assertRaisesRegex(ValueError, "--shell"),
            ):
                rust_build_status._cargo_command_with_target_dir(
                    ["cargo", "watch", "-x", "check", option], target
                )
        for option in ("-xcheck", "-x=check"):
            with self.subTest(option=option):
                self.assertEqual(
                    rust_build_status._cargo_command_with_target_dir(
                        ["cargo", "watch", "-x", "build", option], target
                    ),
                    [
                        "cargo",
                        "watch",
                        "-x",
                        f"build --target-dir {target}",
                        f"-xcheck --target-dir {target}",
                    ],
                )
        with self.assertRaisesRegex(ValueError, "target-dir"):
            rust_build_status._cargo_command_with_target_dir(
                ["cargo", "watch", "-xcheck", "-xbuild --target-dir elsewhere"], target
            )
        self.assertEqual(
            rust_build_status._cargo_command_with_target_dir(
                ["cargo", "watch", "-cqxcheck", "-qw", "build"], target
            ),
            [
                "cargo",
                "watch",
                "-cq",
                f"-xcheck --target-dir {target}",
                "-q",
                "-w",
                "build",
            ],
        )
        with self.assertRaisesRegex(ValueError, "positional commands"):
            rust_build_status._cargo_command_with_target_dir(
                ["cargo", "watch", "build", "--target-dir", "elsewhere"], target
            )

    def test_run_lane_rejects_attached_shell_before_launch_and_releases_lock(self):
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)
            lanes = repo / "codex-rs" / "target" / "lanes"
            with (
                mock.patch.object(subprocess, "run") as launch,
                self.assertRaisesRegex(ValueError, "--shell"),
            ):
                rust_build_status.run_in_cargo_lane(
                    repo_root=repo,
                    requested_lane="unit",
                    lane_root=lanes,
                    command=["cargo", "watch", "-xcheck", "-qsecho escaped"],
                )
            launch.assert_not_called()
            self.assertFalse(rust_build_status.lane_active_lock_is_held(lanes / "unit"))

    def test_process_lane_patterns_accept_powershell_forms(self):
        for command, expected in [
            ("powershell -lane:core-", "core-"),
            ('powershell -LANE "quoted"', "quoted"),
            ("powershell -Lane:'quoted-'", "quoted-"),
            ("just cargo-lane core-", "core-"),
        ]:
            self.assertEqual(
                rust_build_status._lane_name_from_command_line(command), expected
            )

    def test_run_lane_holds_reservation_without_exporting_cargo_target_env(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir) / "repo"
            lanes_root = Path(temp_dir) / "lanes"
            output = Path(temp_dir) / "child.txt"
            script = (
                "import os, pathlib, sys; "
                "pathlib.Path(sys.argv[1]).write_text("
                "os.environ['CODEX_CARGO_LANE_TARGET_DIR'] + '\\n' + "
                "str('CARGO_TARGET_DIR' in os.environ), encoding='utf-8')"
            )

            result = rust_build_status.run_in_cargo_lane(
                repo_root=repo_root,
                requested_lane="unit",
                lane_root=lanes_root,
                command=[sys.executable, "-c", script, str(output)],
            )

            lines = output.read_text(encoding="utf-8").splitlines()
            self.assertEqual(result, 0)
            self.assertEqual(Path(lines[0]), (lanes_root / "unit").resolve())
            self.assertEqual(lines[1], "False")
            self.assertFalse(
                rust_build_status.lane_active_lock_is_held(lanes_root / "unit")
            )

    def test_reserve_lane_suffixes_an_active_explicit_lane(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir) / "repo"
            lanes_root = Path(temp_dir) / "lanes"
            with rust_build_status.reserve_cargo_lane(
                repo_root=repo_root,
                requested_lane="unit",
                command=["cargo", "check"],
                lane_root=lanes_root,
            ) as first:
                with rust_build_status.reserve_cargo_lane(
                    repo_root=repo_root,
                    requested_lane="unit",
                    command=["cargo", "check"],
                    lane_root=lanes_root,
                    allow_cold_overflow=True,
                    warm_wait_seconds=0,
                ) as second:
                    self.assertEqual(first[0], "unit")
                    self.assertEqual(second[0], "unit-2")

    def test_cargo_command_target_dir_is_injected_once(self) -> None:
        target = Path("lane-target").resolve()
        self.assertEqual(
            rust_build_status._cargo_command_with_target_dir(
                ["cargo", "nextest", "run", "-p", "codex-core"],
                target,
            ),
            [
                "cargo",
                "nextest",
                "run",
                "--target-dir",
                str(target),
                "-p",
                "codex-core",
            ],
        )
        explicit = ["cargo", "check", "--target-dir", str(target)]
        self.assertEqual(
            rust_build_status._cargo_command_with_target_dir(explicit, target),
            explicit,
        )

    def test_run_lane_directly_launches_known_nextest_recipe_with_argv(self) -> None:
        target = Path("C:/target path/lane")
        maintenance = mock.patch.object(
            rust_build_status, "request_cargo_lane_maintenance"
        )
        maintenance.start()
        self.addCleanup(maintenance.stop)
        command = [
            "just",
            "_test-lane-local-reserved",
            "-p",
            "codex-app-server",
            "test(filter with spaces)",
        ]
        completed = subprocess.CompletedProcess(command, 9)
        with (
            mock.patch.object(
                rust_build_status,
                "reserve_cargo_lane",
                return_value=contextlib.nullcontext(("unit", target)),
            ),
            mock.patch.object(
                rust_build_status.shutil,
                "which",
                return_value=None,
            ),
            mock.patch.object(
                rust_build_status,
                "run_owned",
                return_value=completed,
            ) as run,
        ):
            result = rust_build_status.run_in_cargo_lane(
                repo_root=Path.cwd(),
                requested_lane="unit",
                command=command,
            )

        self.assertEqual(result, 9)
        self.assertEqual(
            run.call_args.args[0],
            [
                "cargo",
                "nextest",
                "run",
                "--target-dir",
                str(target),
                "--no-fail-fast",
                "-p",
                "codex-app-server",
                "test(filter with spaces)",
            ],
        )
        child_env = run.call_args.kwargs["env"]
        self.assertEqual(child_env["NEXTEST_PROFILE"], "local")
        self.assertNotIn("CODEX_CARGO_LANE_TARGET_DIR", child_env)
        self.assertEqual(
            child_env["RUST_MIN_STACK"], rust_build_status.RUST_MIN_STACK_BYTES
        )

    def test_run_lane_rejects_all_generic_core_selections_before_launch(self):
        commands = []
        for recipe, prefix in (
            ("_test-lane-fast-reserved", []),
            ("_test-lane-local-reserved", []),
            ("_test-lane-package-reserved", ["codex-tui"]),
        ):
            for selection in (
                ["--package=codex-core"],
                ["-pcodex-core"],
                ["-p=codex-core"],
                ["-p", "path+file:///repo/core#codex-core@0.0.0"],
                ["-p", "path+file:///repo/core#0.0.0"],
                ["--workspace"],
                ["-p", "codex-core"],
                [],
            ):
                if prefix and not selection:
                    continue
                commands.append(["just", recipe, *prefix, *selection])
        # Raw test runs would skip the named targets' helper builds and fail on
        # a stale codex-code-mode-host only after compiling.
        commands += [
            ["cargo", "nextest", "run", "--profile", "local", "-p", "codex-core"],
            ["cargo", "nextest", "-P", "local", "r", "--package=codex-core"],
            ["cargo", "+stable", "test", "-pcodex-core", "--test", "all"],
            ["cargo", "t", "--workspace"],
        ]
        for command in commands:
            with (
                self.subTest(command=command),
                mock.patch.object(
                    rust_build_status,
                    "reserve_cargo_lane",
                    return_value=contextlib.nullcontext(
                        ("unit", Path("C:/target/lane"))
                    ),
                ),
                mock.patch.object(rust_build_status, "run_owned") as run,
                mock.patch.object(
                    rust_build_status, "request_cargo_lane_maintenance"
                ) as maintain,
                self.assertRaises(ValueError),
            ):
                rust_build_status.run_in_cargo_lane(
                    repo_root=Path.cwd(), requested_lane="unit", command=command
                )
            run.assert_not_called()
            maintain.assert_not_called()

    def test_raw_lane_tests_without_package_follow_cargo_directory_selection(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "Cargo.toml").write_text('[workspace]\nmembers = ["core", "tui"]\n')
            for directory, name in (("core", "codex-core"), ("tui", "codex-tui")):
                (root / directory / "src").mkdir(parents=True)
                (root / directory / "Cargo.toml").write_text(
                    f'[package]\nname = "{name}"\n'
                )
            (root / "lane").mkdir()
            for cwd, command, launches in (
                (root / "tui" / "src", ["cargo", "test"], True),
                (
                    root,
                    ["cargo", "nextest", "run", "--manifest-path", "tui/Cargo.toml"],
                    True,
                ),
                (root / "core", ["cargo", "nextest", "run"], False),
                (root, ["cargo", "test"], False),
            ):
                with (
                    self.subTest(cwd=cwd, command=command),
                    contextlib.chdir(cwd),
                    mock.patch.object(
                        rust_build_status,
                        "reserve_cargo_lane",
                        return_value=contextlib.nullcontext(("unit", root / "lane")),
                    ),
                    mock.patch.object(
                        rust_build_status, "request_cargo_lane_maintenance"
                    ),
                    mock.patch.object(
                        rust_build_status.shutil, "which", return_value=None
                    ),
                    mock.patch.object(rust_build_status, "run_owned") as run,
                ):
                    run.return_value.returncode = 0
                    if launches:
                        rust_build_status.run_in_cargo_lane(
                            repo_root=root, requested_lane="unit", command=command
                        )
                    else:
                        with self.assertRaises(ValueError):
                            rust_build_status.run_in_cargo_lane(
                                repo_root=root, requested_lane="unit", command=command
                            )
                self.assertEqual(run.called, launches)

    def test_direct_reserved_lane_commands_cover_fast_and_package_recipes(
        self,
    ) -> None:
        cases = (
            (
                [
                    "just",
                    "_test-lane-fast-reserved",
                    "-p",
                    "codex-app-server",
                    "filter with spaces",
                ],
                [
                    "cargo",
                    "nextest",
                    "run",
                    "-p",
                    "codex-app-server",
                    "filter with spaces",
                ],
            ),
            (
                [
                    "just",
                    "_test-lane-package-reserved",
                    "codex-cli",
                    "filter with spaces",
                ],
                [
                    "cargo",
                    "nextest",
                    "run",
                    "-p",
                    "codex-cli",
                    "filter with spaces",
                ],
            ),
        )
        for command, expected in cases:
            with self.subTest(command=command):
                child_env: dict[str, str] = {}
                actual = rust_build_status._direct_reserved_lane_command(
                    command,
                    child_env,
                    repo_root=REPO_ROOT,
                    target_dir=Path("lane-target"),
                )

                self.assertEqual(actual, expected)
                self.assertEqual(child_env["NEXTEST_PROFILE"], "fast")
                self.assertEqual(
                    child_env["RUST_MIN_STACK"],
                    rust_build_status.RUST_MIN_STACK_BYTES,
                )

    def test_compile_and_run_reuse_package_lane_for_equivalent_package_spellings(self):
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)
            target = repo / "codex-rs" / "target" / "lanes" / "codex-tui"
            for selection, flags in (
                (["-p", "codex-tui"], ["--no-run"]),
                (["-pcodex-tui"], ["first_filter"]),
                (["-p=codex-tui"], ["second_filter"]),
                (["--package=codex-tui"], ["--no-fail-fast"]),
                (["--package", "codex-tui"], ["third_filter"]),
            ):
                with (
                    self.subTest(selection=selection),
                    mock.patch.object(
                        rust_build_status, "request_cargo_lane_maintenance"
                    ) as maintain,
                    mock.patch.object(
                        rust_build_status.shutil, "which", return_value=None
                    ),
                    mock.patch.object(rust_build_status, "run_owned") as run,
                ):
                    run.return_value.returncode = 0
                    self.assertEqual(
                        rust_build_status.run_in_cargo_lane(
                            repo_root=repo,
                            requested_lane="auto",
                            command=["cargo", "nextest", "run", *selection, *flags],
                        ),
                        0,
                    )
                self.assertEqual(
                    run.call_args.args[0],
                    [
                        "cargo",
                        "nextest",
                        "run",
                        "--target-dir",
                        str(target),
                        *selection,
                        *flags,
                    ],
                )
                maintain.assert_called_once_with(repo, target.parent)
                self.assertFalse(rust_build_status.lane_active_lock_is_held(target))

    def test_watch_exec_retains_package_lane_affinity(self):
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)
            target = repo / "codex-rs" / "target" / "lanes" / "codex-tui"
            for selection in (
                ["-x", "check -p codex-tui"],
                ["--exec=check -p=codex-tui"],
            ):
                with (
                    self.subTest(selection=selection),
                    mock.patch.object(
                        rust_build_status, "request_cargo_lane_maintenance"
                    ),
                    mock.patch.object(
                        rust_build_status.shutil, "which", return_value=None
                    ),
                    mock.patch.object(rust_build_status, "run_owned") as run,
                ):
                    run.return_value.returncode = 0
                    self.assertEqual(
                        rust_build_status.run_in_cargo_lane(
                            repo_root=repo,
                            requested_lane="auto",
                            command=["cargo", "watch", *selection],
                        ),
                        0,
                    )
                    self.assertEqual(
                        run.call_args.kwargs["env"]["CODEX_CARGO_LANE_TARGET_DIR"],
                        str(target),
                    )
                    self.assertIn(f"--target-dir {target}", run.call_args.args[0][-1])

    def test_core_reserved_recipes_launch_runner_directly_with_reserved_target(self):
        cases = (
            (
                [
                    "_core-test-reserved",
                    "local",
                    "core_lib",
                    "--no-fail-fast",
                    "-E",
                    "test(alpha with spaces)",
                ],
                [
                    "run-target",
                    "--profile",
                    "local",
                    "core_lib",
                    "--no-fail-fast",
                    "-E",
                    "test(alpha with spaces)",
                ],
                "local",
            ),
            (
                ["_core-test-reserved", "fast", "core_lib", "alpha"],
                ["run-target", "--profile", "fast", "core_lib", "alpha"],
                "fast",
            ),
            (
                ["_core-gate-reserved", "first", "second"],
                ["run-gate", "--profile", "fast", "first", "second"],
                "fast",
            ),
        )
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)
            target = repo / "codex-rs" / "target" / "lanes" / "core-tests"
            for arguments, expected, profile in cases:
                with (
                    self.subTest(arguments=arguments),
                    mock.patch.object(
                        rust_build_status, "request_cargo_lane_maintenance"
                    ),
                    mock.patch.object(rust_build_status, "run_owned") as run,
                ):

                    def child(command, *, env, check, stdout, stderr, prepare_sccache, creationflags):
                        self.assertEqual(creationflags, getattr(subprocess, "BELOW_NORMAL_PRIORITY_CLASS", 0))
                        self.assertTrue(prepare_sccache)
                        self.assertIs(stdout, sys.stdout)
                        self.assertIs(stderr, sys.stderr)
                        self.assertTrue(
                            rust_build_status.lane_active_lock_is_held(target)
                        )
                        self.assertEqual(env["NEXTEST_PROFILE"], profile)
                        self.assertEqual(env["RUST_MIN_STACK"], "8388608")
                        self.assertEqual(env["CODEX_CARGO_LANE_TARGET_DIR"], str(target))
                        self.assertNotIn("CARGO_TARGET_DIR", env)
                        # The parent still owns the lane. The runner inherits
                        # that reservation without reacquiring it or deadlocking.
                        with mock.patch.dict(os.environ, env, clear=True):
                            with rust_build_status.reserve_rust_test_target(
                                target, timeout_seconds=.1,
                            ) as admission:
                                self.assertTrue(admission["inherited_lane_reservation"])
                        return subprocess.CompletedProcess(command, 7)

                    run.side_effect = child
                    self.assertEqual(
                        rust_build_status.run_in_cargo_lane(
                            repo_root=repo,
                            requested_lane="core-tests",
                            command=["just", *arguments],
                        ),
                        7,
                    )
                self.assertEqual(
                    run.call_args.args[0],
                    [
                        sys.executable,
                        str(repo / "scripts" / "rust_test_runner.py"),
                        "--target-dir",
                        str(target),
                        *expected,
                    ],
                )
                self.assertFalse(rust_build_status.lane_active_lock_is_held(target))

    def test_cargo_command_parses_toolchain_and_value_taking_global_options(
        self,
    ) -> None:
        target = Path("lane-target").resolve()

        self.assertEqual(
            rust_build_status._cargo_command_with_target_dir(
                [
                    "cargo",
                    "+nightly",
                    "--config",
                    "profile.dev.debug=0",
                    "-C",
                    "codex-rs",
                    "-Zunstable-options",
                    "check",
                    "-p",
                    "codex-core",
                ],
                target,
            ),
            [
                "cargo",
                "+nightly",
                "--config",
                "profile.dev.debug=0",
                "-C",
                "codex-rs",
                "-Zunstable-options",
                "check",
                "--target-dir",
                str(target),
                "-p",
                "codex-core",
            ],
        )

    def test_cargo_command_rejects_target_dir_outside_reserved_lane(self) -> None:
        target = Path("lane-target").resolve()

        with self.assertRaisesRegex(ValueError, "does not match reserved lane"):
            rust_build_status._cargo_command_with_target_dir(
                ["cargo", "check", "--target-dir=custom"],
                target,
            )

    def test_cargo_watch_rejects_shell_and_mismatched_exec_target(self) -> None:
        target = Path("lane-target").resolve()

        for shell_option in ("-s", "--shell", "--shell=powershell"):
            with self.subTest(shell_option=shell_option):
                with self.assertRaisesRegex(ValueError, "is not allowed"):
                    rust_build_status._cargo_command_with_target_dir(
                        ["cargo", "watch", shell_option, "cargo check"],
                        target,
                    )
        with self.assertRaisesRegex(ValueError, "does not match reserved lane"):
            rust_build_status._cargo_command_with_target_dir(
                ["cargo", "watch", "-x", "check --target-dir custom"],
                target,
            )

    def test_cargo_config_preserves_profile_incremental_defaults(self) -> None:
        config = load_toml(REPO_ROOT / "codex-rs" / ".cargo" / "config.toml")

        self.assertNotIn("incremental", config["build"])

    def test_missing_lane_mtime_is_safe_during_concurrent_gc(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            missing_lane = Path(temp_dir) / "already-pruned"

            self.assertEqual(rust_build_status.lane_last_used_mtime(missing_lane), 0.0)

    def test_rust_build_doctor_reports_cache_linker_and_contention(self) -> None:
        fixture = tempfile.TemporaryDirectory()
        self.addCleanup(fixture.cleanup)
        report = rust_build_status.build_doctor_report(
            repo_root=Path(fixture.name),
            processes=[
                rust_build_status.RustProcess(
                    pid=42,
                    name="cargo.exe",
                    command_line="cargo nextest run -p codex-core",
                ),
                rust_build_status.RustProcess(
                    pid=43,
                    name="rustc.exe",
                    command_line="rustc --out-dir codex-rs\\target\\lanes\\ui\\debug",
                ),
            ],
            tool_lookup=lambda name: (
                f"C:/tools/{name}.exe" if name == "sccache" else None
            ),
            env={},
        )

        self.assertIn("sccache: C:/tools/sccache.exe", report)
        self.assertIn(
            "MSVC linker config x86_64-pc-windows-msvc: (unset)",
            report,
        )
        self.assertIn(
            "MSVC linker config aarch64-pc-windows-msvc: (unset)",
            report,
        )
        self.assertIn(
            "MSVC linker env CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER: (unset)",
            report,
        )
        # The doctor's own env lacks RUSTC_WRAPPER, but just/run-lane builds get it.
        self.assertIn("RUSTC_WRAPPER: (unset)", report)
        self.assertIn("RUSTC_WRAPPER=C:/tools/sccache.exe", report)
        self.assertIn("active Rust processes: 2 total, 1 lane, 1 without lane", report)
        self.assertIn("their target dir is not verified", report)
        self.assertNotIn("shared-target jobs are active", report)

    def test_windows_process_discovery_uses_cim_filter(self) -> None:
        with mock.patch.object(rust_build_status.subprocess, "run") as run:
            run.return_value.stdout = "[]"

            self.assertEqual(rust_build_status.active_rust_processes_windows(), [])

        command = run.call_args.args[0][-1]
        self.assertIn("Get-CimInstance Win32_Process -Filter", command)
        self.assertIn("Name = 'cargo.exe'", command)
        self.assertIn("Name = 'pwsh.exe'", command)
        self.assertIn("$selfPid = $PID", command)
        self.assertIn("ProcessId != $selfPid", command)
        self.assertNotIn("Where-Object", command)

    def test_doctor_fast_path_skips_disk_but_preserves_readiness_report(self):
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)
            snapshot = rust_build_status.BuildStatusSnapshot.collect(
                repo_root=repo, processes=[]
            )
            kwargs = dict(
                repo_root=repo, snapshot=snapshot, tool_lookup=lambda _: None, env={}
            )
            with mock.patch.object(
                rust_build_status_support, "target_disk_report_lines",
                side_effect=AssertionError("readiness must not scan disk"),
            ):
                fast = rust_build_status.build_doctor_report(**kwargs, include_disk=False)
            with mock.patch.object(
                rust_build_status_support, "target_disk_report_lines",
                return_value=["disk details"],
            ) as disk:
                full = rust_build_status.build_doctor_report(**kwargs)
                self.assertEqual(
                    full,
                    rust_build_status.build_doctor_report(**kwargs, include_disk=True),
                )
                self.assertEqual(disk.call_count, 2)
            notice = "target disk: not scanned (use `doctor --include-disk` or `disk`)"
            self.assertEqual(fast.replace(notice, "disk details"), full)

    def test_doctor_cli_disk_scan_is_explicit(self):
        for arguments, include_disk in [
            (["doctor"], False), (["doctor", "--include-disk"], True)
        ]:
            with self.subTest(arguments=arguments), mock.patch.object(
                rust_build_status, "build_doctor_report", return_value="doctor result"
            ) as report, contextlib.redirect_stdout(io.StringIO()) as output:
                self.assertEqual(rust_build_status.main(arguments), 0)
                report.assert_called_once_with(include_disk=include_disk)
                self.assertEqual(output.getvalue(), "doctor result\n")

    def test_windows_process_discovery_rechecks_exited_but_preserves_hidden_rows(self):
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is required")
        # Execute the actual query pipeline: one stale row has exited, while a
        # second process is still alive with inaccessible command-line metadata.
        fixture = """
function Get-CimInstance {
    [CmdletBinding()] param($ClassName, $Filter)
    if ($Filter -eq 'ProcessId=11') { return }
    if ($Filter -eq 'ProcessId=12') {
        [pscustomobject]@{Name='cargo.exe';ProcessId=12;CommandLine=''}
        return
    }
    [pscustomobject]@{Name='cargo.exe';ProcessId=11;CommandLine=''}
    [pscustomobject]@{Name='cargo.exe';ProcessId=12;CommandLine=''}
}
"""
        run = subprocess.run

        def query(command, **kwargs):
            return run([shell, "-NoProfile", "-Command", fixture + command[-1]], **kwargs)

        with mock.patch.object(rust_build_status.subprocess, "run", side_effect=query):
            processes = rust_build_status.active_rust_processes_windows()
        self.assertEqual(
            processes,
            [rust_build_status.RustProcess(pid=12, name="cargo.exe", command_line="")],
        )

    def test_windows_process_discovery_failure_is_not_an_empty_scan(self) -> None:
        with (
            mock.patch.object(
                rust_build_status.subprocess,
                "run",
                side_effect=subprocess.TimeoutExpired("powershell", 10),
            ),
            self.assertRaisesRegex(
                rust_build_status.RustProcessScanError,
                "Windows Rust process scan failed",
            ),
        ):
            rust_build_status.active_rust_processes_windows()

    def test_unverified_process_activity_refuses_prune_and_reports_unknown(self):
        failed_scan = mock.patch.object(
            rust_build_status.subprocess,
            "run",
            side_effect=subprocess.TimeoutExpired("powershell", 10),
        )
        hidden_command_line = mock.patch.object(
            rust_build_status,
            "active_rust_processes",
            return_value=[
                rust_build_status.RustProcess(pid=9, name="cargo.exe", command_line="")
            ],
        )
        for case, evidence in (
            ("failed scan", failed_scan),
            ("hidden command line", hidden_command_line),
        ):
            with (
                self.subTest(case=case),
                tempfile.TemporaryDirectory() as temp_dir,
                mock.patch.dict(os.environ, {}, clear=True),
                evidence,
            ):
                repo_root = Path(temp_dir)
                # An old, lock-free lane that a raw `cargo --target-dir` job
                # could be using; only process evidence would reveal that.
                lane = repo_root / "codex-rs" / "target" / "lanes" / "raw-user"
                lane.mkdir(parents=True)
                (lane / "artifact").write_text("keep", encoding="utf-8")
                stamp = lane / ".lane-last-used"
                stamp.touch()
                os.utime(stamp, (1, 1))
                with self.assertRaisesRegex(
                    rust_build_status.RustProcessScanError,
                    "lane activity is unverified",
                ):
                    rust_build_status.prune_stale_lanes(repo_root=repo_root)
                report = rust_build_status.build_doctor_report(
                    repo_root=repo_root, tool_lookup=lambda _name: None, env={}
                )

                self.assertEqual((lane / "artifact").read_text(), "keep")
                self.assertIn("active Rust processes: unknown (", report)
                self.assertIn("pruning refused:", report)
                self.assertNotIn("safe prune suggestions:", report)

    def test_posix_process_matching_ignores_cargo_substrings(self) -> None:
        self.assertFalse(
            rust_build_status.is_rust_process(
                rust_build_status.RustProcess(
                    pid=1,
                    name="editor",
                    command_line="editor /repo/codex-rs/Cargo.toml",
                )
            )
        )
        self.assertTrue(
            rust_build_status.is_rust_process(
                rust_build_status.RustProcess(
                    pid=2,
                    name="sh",
                    command_line="sh -c 'cargo test'",
                )
            )
        )

    def test_process_classification_is_observed_once_for_snapshot_consumers(
        self,
    ) -> None:
        with (
            tempfile.TemporaryDirectory() as temp_dir,
            mock.patch.object(
                rust_build_status,
                "_classify_rust_process",
                wraps=rust_build_status._classify_rust_process,
            ) as classify,
        ):
            process = rust_build_status.RustProcess(
                pid=7,
                name="pwsh.exe",
                command_line="pwsh just cargo-lane ui cargo check",
            )
            snapshot = rust_build_status.BuildStatusSnapshot.collect(
                repo_root=Path(temp_dir),
                processes=[process],
            )
            self.assertEqual(snapshot.lane_name_for(snapshot.processes[0]), "ui")
            self.assertEqual(
                rust_build_status.shared_target_rust_processes(
                    snapshot.processes,
                    snapshot.lane_names_by_process,
                ),
                [],
            )
            self.assertEqual(
                rust_build_status.active_lane_names(snapshot.processes),
                {"ui"},
            )

        self.assertEqual(classify.call_count, 1)

    def test_shared_process_filter_reuses_one_process_classification(self) -> None:
        with mock.patch.object(
            rust_build_status,
            "_classify_rust_process",
            wraps=rust_build_status._classify_rust_process,
        ) as classify:
            process = rust_build_status.RustProcess(
                pid=8,
                name="pwsh.exe",
                command_line="pwsh cargo check",
            )
            shared = rust_build_status.shared_target_rust_processes([process])

        self.assertEqual(len(shared), 1)
        self.assertEqual(classify.call_count, 1)

    def test_lane_candidates_reuse_one_directory_observation(self) -> None:
        class FakeEntry:
            def __init__(self, root: Path, name: str, mtime: float) -> None:
                self.name = name
                self.path = str(root / name)
                self._observation = os.stat_result(
                    (0o040755, 0, 0, 1, 0, 0, 0, mtime, mtime, mtime)
                )
                self.stat_calls = 0

            def stat(self, *, follow_symlinks: bool) -> os.stat_result:
                self.assert_follow_symlinks = follow_symlinks
                self.stat_calls += 1
                return self._observation

        root = Path("C:/lanes")
        older = FakeEntry(root, "unit", 1.0)
        newer = FakeEntry(root, "unit-2", 2.0)
        unrelated = FakeEntry(root, "other", 3.0)
        with (
            mock.patch.object(
                rust_build_status.os,
                "scandir",
                return_value=contextlib.nullcontext([older, newer, unrelated]),
            ),
            mock.patch.object(
                rust_build_status,
                "lane_last_used_mtime",
                side_effect=lambda path: 10.0 if path.name == "unit" else 5.0,
            ),
        ):
            candidates = rust_build_status._lane_reservation_candidates(
                root,
                "unit",
                prefer_warm=True,
            )

        self.assertEqual(
            [candidate.name for candidate in candidates[:2]],
            ["unit", "unit-2"],
        )
        self.assertEqual(older.stat_calls, 1)
        self.assertEqual(newer.stat_calls, 1)
        self.assertEqual(unrelated.stat_calls, 0)
        self.assertFalse(older.assert_follow_symlinks)

    def test_target_scan_reuses_lane_sizes_without_losing_hardlink_accounting(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            repo = Path(temp_dir)
            target = repo / "codex-rs" / "target"
            first, second = target / "lanes" / "first", target / "lanes" / "second"
            first.mkdir(parents=True)
            second.mkdir()
            size = rust_build_status_support.HARDLINK_CHECK_MIN_BYTES
            original = first / "binary"
            original.write_bytes(b"x" * size)
            os.link(original, first / "uplift")
            os.link(original, second / "binary")
            os.link(original, target / "shared-binary")
            (first / "small").write_bytes(b"abc")
            snapshot = rust_build_status.BuildStatusSnapshot.collect(
                repo_root=repo, processes=[]
            )
            self.assertEqual(snapshot.target_size(), (size + 3, 0))
            sizes = snapshot.lane_sizes(
                [first, second],
                size_workers=2,
                lane_size=lambda path: self.fail(f"rescanned {path}"),
            )
            self.assertEqual(sizes, {first: (size + 3, 0), second: (size, 0)})
            # Lanes outside target were not observed by the full-target walk.
            external = repo / "external-lane"
            external.mkdir()
            (external / "artifact").write_bytes(b"outside")
            snapshot.lane_dirs.append(external)
            snapshot.target_size()
            self.assertEqual(
                snapshot.lane_sizes([external], size_workers=1), {external: (7, 0)}
            )

    def test_optimize_uses_requested_policy_threshold_and_one_size_scan(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            repo = Path(temp_dir)
            lanes = repo / "codex-rs" / "target" / "lanes"
            for name in ("warm", "warm-2"):
                lane = lanes / name
                lane.mkdir(parents=True)
                (lane / "artifact").write_bytes(b"test")
            with (
                mock.patch.object(
                    rust_build_status, "active_rust_processes", return_value=[]
                ),
                mock.patch.object(
                    rust_build_status,
                    "directory_size_bytes",
                    wraps=rust_build_status.directory_size_bytes,
                ) as scan,
                mock.patch.object(
                    rust_build_status,
                    "target_non_lane_size_bytes",
                    wraps=rust_build_status.target_non_lane_size_bytes,
                ) as non_lane_scan,
            ):
                report = rust_build_status_support.target_optimize_report(
                    repo_root=repo,
                    dry_run=True,
                    warn_bytes=1,
                    keep_warm_per_base=2,
                    max_age_days=14,
                    max_lane_bytes=8,
                    max_total_lane_bytes=8,
                    max_total_target_bytes=8,
                )
            self.assertEqual(scan.call_count, 1)
            non_lane_scan.assert_not_called()
            self.assertIn("target disk: 8 B", report)
            self.assertIn("target warning threshold: 1 B", report)
            self.assertIn("target disk warning:", report)
            self.assertIn("warm-protected: warm, warm-2", report)
            self.assertNotIn("prunable:", report)
            self.assertNotIn("safe prune suggestions:", report)
            self.assertIn("no stale lanes to prune", report)
            self.assertTrue((lanes / "warm" / "artifact").exists())

    def test_optimize_post_prune_disk_report_is_fresh(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            repo = Path(temp_dir)
            lane = repo / "codex-rs" / "target" / "lanes" / "oversized"
            lane.mkdir(parents=True)
            (lane / "artifact").write_bytes(b"test")
            with mock.patch.object(
                rust_build_status, "active_rust_processes", return_value=[]
            ):
                report = rust_build_status_support.target_optimize_report(
                    repo_root=repo,
                    max_lane_bytes=1,
                    warn_bytes=1,
                    include_prune_disk_report=True,
                )
            self.assertFalse(lane.exists())
            self.assertIn("prunable:\n  oversized", report)
            self.assertIn(f"pruned: {lane}", report)
            self.assertNotIn("safe prune suggestions:", report)
            sizes = [
                line for line in report.splitlines() if line.startswith("target disk:")
            ]
            remaining = sum(
                path.stat().st_size
                for path in (repo / "codex-rs" / "target").rglob("*")
                if path.is_file()
            )
            self.assertLess(remaining, 4)
            self.assertEqual(sizes, ["target disk: 4 B", f"target disk: {remaining} B"])

    def test_target_disk_report_warns_when_target_exceeds_budget(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            target = repo_root / "codex-rs" / "target" / "debug"
            target.mkdir(parents=True)
            (target / "artifact.bin").write_bytes(b"abcd")

            report = rust_build_status.target_disk_report(
                repo_root=repo_root,
                warn_bytes=3,
            )

        self.assertIn("target disk: 4 B", report)
        self.assertIn("target disk warning:", report)
        self.assertIn("just target-prune --dry-run --max-total-target-gib", report)
        # A report cannot prove the shared target idle; never advise deleting it.
        self.assertNotIn("remove `codex-rs/target`", report)
        self.assertNotIn("automatic lane GC will recheck", report)

    def test_target_disk_report_flags_stray_cargo_target_dirs(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            target_root = repo_root / "codex-rs" / "target"
            stray_debug = target_root / "codex-core-registry-check" / "debug"
            protected = target_root / "dev-small"
            ambiguous = target_root / "schema-probe-plan"
            for cargo_dir in (stray_debug, protected):
                (cargo_dir / ".fingerprint").mkdir(parents=True)
                (cargo_dir / "deps").mkdir()
                (cargo_dir / "build").mkdir()
                (cargo_dir / "incremental").mkdir()
            ambiguous.mkdir()

            report = rust_build_status.target_disk_report(
                repo_root=repo_root,
                warn_bytes=100,
            )

        self.assertIn("stray cargo target dirs: codex-core-registry-check", report)
        self.assertIn("just cargo-lane <lane>", report)
        self.assertNotIn("dev-small", report)
        self.assertNotIn("schema-probe-plan", report)

    def test_directory_size_skips_reparse_points(self) -> None:
        class FakeReparseEntry:
            path = "outside"

            def is_junction(self) -> bool:
                return True

            def is_dir(self, *, follow_symlinks: bool) -> bool:
                raise AssertionError("reparse point should be skipped before traversal")

            def stat(self, *, follow_symlinks: bool):
                raise AssertionError("junction probe should be sufficient")

        with (
            tempfile.TemporaryDirectory() as temp_dir,
            mock.patch.object(rust_build_status_support.os, "name", "nt"),
            mock.patch.object(
                rust_build_status_support.os,
                "scandir",
                return_value=contextlib.nullcontext([FakeReparseEntry()]),
            ),
        ):
            size, errors = rust_build_status.directory_size_bytes(Path(temp_dir))

        self.assertEqual((size, errors), (0, 0))

    def test_prune_stray_target_dirs_reports_but_preserves_trees(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            stray_root = (
                repo_root / "codex-rs" / "target" / "codex-tools-responses-check"
            )
            stray_debug = stray_root / "debug"
            (stray_debug / ".fingerprint").mkdir(parents=True)
            (stray_debug / "deps").mkdir()
            (stray_debug / "build").mkdir()
            read_only_file = stray_debug / "deps" / "artifact.rlib"
            read_only_file.write_text("artifact", encoding="utf-8")
            read_only_file.chmod(0o400)

            with mock.patch.object(
                rust_build_status, "remove_tree_allow_readonly"
            ) as remove_tree:
                detected = rust_build_status.prune_stray_cargo_target_dirs(
                    repo_root=repo_root,
                )

            self.assertEqual(
                [path.name for path in detected], ["codex-tools-responses-check"]
            )
            remove_tree.assert_not_called()
            self.assertTrue(stray_root.exists())

    def test_prune_stale_lanes_removes_only_inactive_lanes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            stale_lane = lane_root / "stale"
            active_lane = lane_root / "active"
            stale_lane.mkdir(parents=True)
            active_lane.mkdir(parents=True)
            (stale_lane / "artifact.txt").write_text("stale", encoding="utf-8")
            (active_lane / "artifact.txt").write_text("active", encoding="utf-8")

            removed = rust_build_status.prune_stale_lanes(
                repo_root=repo_root,
                processes=[
                    rust_build_status.RustProcess(
                        pid=7,
                        name="rustc.exe",
                        command_line=f"rustc --out-dir {active_lane}\\debug",
                    )
                ],
                keep_warm_per_base=0,
                max_age_days=None,
            )

            self.assertEqual([path.name for path in removed], ["stale"])
            self.assertFalse(stale_lane.exists())
            self.assertTrue(active_lane.exists())

    def test_prune_rejects_unmarked_custom_root_without_deleting_children(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            custom_root = Path(temp_dir) / "ordinary-root"
            ordinary_dir = custom_root / "family-photos"
            ordinary_dir.mkdir(parents=True)
            (ordinary_dir / "photo.txt").write_text("keep", encoding="utf-8")
            stderr = io.StringIO()

            with (
                mock.patch.dict(
                    rust_build_status.os.environ,
                    {"CODEX_CARGO_LANES_ROOT": str(custom_root)},
                ),
                contextlib.redirect_stderr(stderr),
            ):
                result = rust_build_status.main(
                    ["prune", "--all", "--skip-disk-report"]
                )

            self.assertEqual(result, 2)
            self.assertTrue(ordinary_dir.exists())
            self.assertIn(
                "refusing to prune unrecognized Cargo lanes root",
                stderr.getvalue(),
            )

    def test_prune_accepts_marked_custom_root(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir) / "repo"
            custom_root = Path(temp_dir) / "custom-lanes"
            stale_lane = custom_root / "stale"
            stale_lane.mkdir(parents=True)
            (custom_root / rust_build_status.CARGO_LANES_ROOT_MARKER).write_text(
                rust_build_status.CARGO_LANES_ROOT_MARKER_CONTENT + "\n",
                encoding="utf-8",
            )

            with mock.patch.dict(
                rust_build_status.os.environ,
                {"CODEX_CARGO_LANES_ROOT": str(custom_root)},
            ):
                removed = rust_build_status.prune_stale_lanes(
                    repo_root=repo_root,
                    processes=[],
                    keep_warm_per_base=0,
                    max_age_days=None,
                )

            self.assertEqual(removed, [stale_lane])
            self.assertFalse(stale_lane.exists())

    def test_locked_lane_is_active_and_not_pruned(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            locked = lane_root / "locked"
            stale = lane_root / "stale"
            locked.mkdir(parents=True)
            stale.mkdir(parents=True)

            with mock.patch.object(
                rust_build_status,
                "cargo_lock_is_busy",
                side_effect=lambda path: path.name == "locked",
            ):
                snapshot = rust_build_status.BuildStatusSnapshot.collect(
                    repo_root=repo_root,
                    processes=[],
                )
                removed = rust_build_status.prune_stale_lanes(
                    repo_root=repo_root,
                    snapshot=snapshot,
                    keep_warm_per_base=0,
                    max_age_days=None,
                )

            self.assertIn("locked", snapshot.active_lanes)
            self.assertEqual([path.name for path in removed], ["stale"])
            self.assertTrue(locked.exists())
            self.assertFalse(stale.exists())

    def test_unreadable_lock_files_are_treated_as_busy(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            lane = Path(temp_dir)
            (lane / "debug").mkdir()
            with mock.patch.object(Path, "stat", side_effect=PermissionError("denied")):
                self.assertTrue(rust_build_status.cargo_lock_is_busy(lane))
                self.assertTrue(rust_build_status.lane_active_lock_is_held(lane))

    def test_prune_rechecks_locks_before_delete(self) -> None:
        for lock_check in ("cargo_lock_is_busy", "lane_active_lock_is_held"):
            with (
                self.subTest(lock_check=lock_check),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                repo_root = Path(temp_dir)
                lane = repo_root / "codex-rs" / "target" / "lanes" / "late-busy"
                lane.mkdir(parents=True)
                snapshot = rust_build_status.BuildStatusSnapshot.collect(
                    repo_root=repo_root,
                    processes=[],
                )

                with mock.patch.object(
                    rust_build_status, lock_check, return_value=True
                ):
                    removed = rust_build_status.prune_stale_lanes(
                        repo_root=repo_root,
                        snapshot=snapshot,
                        keep_warm_per_base=0,
                        max_age_days=None,
                    )

                self.assertEqual(removed, [])
                self.assertTrue(lane.exists())

    def test_prune_skips_path_that_becomes_indirect(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane = repo_root / "codex-rs" / "target" / "lanes" / "racy"
            lane.mkdir(parents=True)

            with (
                mock.patch.object(
                    rust_build_status, "prunable_lane_dirs", return_value=[lane]
                ),
                mock.patch.object(
                    rust_build_status,
                    "is_indirect_directory",
                    side_effect=[False, True],
                ),
                mock.patch.object(
                    rust_build_status, "cargo_lock_is_busy", return_value=False
                ),
                mock.patch.object(
                    rust_build_status,
                    "lane_active_lock_is_held",
                    return_value=False,
                ),
            ):
                removed = rust_build_status.prune_stale_lanes(
                    repo_root=repo_root,
                    keep_warm_per_base=0,
                    max_age_days=None,
                )

            self.assertEqual(removed, [])
            self.assertTrue(lane.exists())

    def test_prune_strays_skips_indirect_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            stray = repo_root / "codex-rs" / "target" / "stray"
            stray.mkdir(parents=True)

            with (
                mock.patch.object(
                    rust_build_status, "stray_cargo_target_dirs", return_value=[stray]
                ),
                mock.patch.object(
                    rust_build_status, "is_indirect_directory", return_value=True
                ),
            ):
                removed = rust_build_status.prune_stray_cargo_target_dirs(
                    repo_root=repo_root
                )

            self.assertEqual(removed, [])
            self.assertTrue(stray.exists())

    def test_prune_strays_warns_and_skips_outside_target_root(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir) / "repo"
            target_root = repo_root / "codex-rs" / "target"
            target_root.mkdir(parents=True)
            outside = Path(temp_dir) / "outside"
            outside.mkdir()
            stderr = io.StringIO()

            with (
                mock.patch.object(
                    rust_build_status,
                    "stray_cargo_target_dirs",
                    return_value=[outside],
                ),
                contextlib.redirect_stderr(stderr),
            ):
                removed = rust_build_status.prune_stray_cargo_target_dirs(
                    repo_root=repo_root
                )

            self.assertEqual(removed, [])
            self.assertTrue(outside.exists())
            self.assertIn("warning: skipping stray target outside", stderr.getvalue())

    def test_prune_strays_never_calls_delete_after_classification(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            stray = repo_root / "codex-rs" / "target" / "stray"
            stray.mkdir(parents=True)
            with (
                mock.patch.object(
                    rust_build_status,
                    "stray_cargo_target_dirs",
                    return_value=[stray],
                ),
                mock.patch.object(
                    rust_build_status,
                    "remove_tree_allow_readonly",
                ) as remove_tree,
            ):
                detected = rust_build_status.prune_stray_cargo_target_dirs(
                    repo_root=repo_root
                )

            self.assertEqual(detected, [stray])
            remove_tree.assert_not_called()
            self.assertTrue(stray.exists())

    def test_prune_stale_lanes_keeps_two_most_recent_warm_lanes_per_base(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            newest = lane_root / "codex-core-3"
            middle = lane_root / "codex-core-2"
            oldest = lane_root / "codex-core"
            for lane in (newest, middle, oldest):
                lane.mkdir(parents=True)
                (lane / "artifact.txt").write_text(lane.name, encoding="utf-8")

            removed = rust_build_status.prune_stale_lanes(
                repo_root=repo_root,
                processes=[],
                keep_warm_per_base=2,
                now_timestamp=400.0,
                lane_mtime=lambda path: {newest: 300.0, middle: 200.0, oldest: 100.0}[
                    path
                ],
            )

            self.assertEqual([path.name for path in removed], ["codex-core"])
            self.assertTrue(newest.exists())
            self.assertTrue(middle.exists())
            self.assertFalse(oldest.exists())

    def test_warm_budget_groups_only_reservation_suffixes_as_siblings(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            # Two distinct auto lanes whose hex digests happen to be all digits,
            # plus a real reservation sibling of the second one.
            mtimes = {"cargo-12345678": 100.0, "cargo-87654321": 200.0}
            mtimes["cargo-87654321-2"] = 150.0
            for name in mtimes:
                (lane_root / name).mkdir(parents=True)

            prunable = rust_build_status.prunable_lane_dirs(
                repo_root=repo_root,
                processes=[],
                keep_warm_per_base=1,
                max_age_days=None,
                now_timestamp=300.0,
                lane_mtime=lambda path: mtimes[path.name],
            )

        self.assertEqual([path.name for path in prunable], ["cargo-87654321-2"])

    def test_prune_stale_lanes_removes_timestamped_lanes_even_with_warm_budget(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            stable = lane_root / "codex-core"
            timestamped = lane_root / "codex-core-20260608183755"
            stable.mkdir(parents=True)
            timestamped.mkdir(parents=True)

            removed = rust_build_status.prune_stale_lanes(
                repo_root=repo_root,
                processes=[],
                keep_warm_per_base=2,
            )

            self.assertEqual([path.name for path in removed], [timestamped.name])
            self.assertTrue(stable.exists())
            self.assertFalse(timestamped.exists())

    def test_prune_stale_lanes_removes_lanes_over_age_budget(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            old = lane_root / "old"
            fresh = lane_root / "fresh"
            old.mkdir(parents=True)
            fresh.mkdir(parents=True)
            old_time = 1_700_000_000
            fresh_time = 1_700_086_400
            for lane in (old, fresh):
                (lane / "artifact.txt").write_text(lane.name, encoding="utf-8")
            old.touch()
            fresh.touch()

            removed = rust_build_status.prune_stale_lanes(
                repo_root=repo_root,
                processes=[],
                keep_warm_per_base=2,
                max_age_days=1,
                now_timestamp=fresh_time + 1,
                lane_mtime=lambda path: old_time if path.name == "old" else fresh_time,
            )

            self.assertEqual([path.name for path in removed], ["old"])
            self.assertFalse(old.exists())
            self.assertTrue(fresh.exists())

    def test_prune_stale_lanes_applies_warm_budget_before_size_scan(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            protected = lane_root / "codex-core"
            pruned_by_warm_budget = lane_root / "codex-core-2"
            protected.mkdir(parents=True)
            pruned_by_warm_budget.mkdir(parents=True)
            size_calls: list[str] = []

            def lane_size(path: Path) -> tuple[int, int]:
                size_calls.append(path.name)
                return 0, 0

            removed = rust_build_status.prune_stale_lanes(
                repo_root=repo_root,
                processes=[],
                keep_warm_per_base=1,
                max_lane_bytes=1,
                lane_mtime=lambda _path: 1.0,
                now_timestamp=1.0,
                lane_size=lane_size,
            )

            self.assertEqual([path.name for path in removed], ["codex-core-2"])
            self.assertEqual(size_calls, ["codex-core"])

    def test_prune_stale_lanes_applies_global_ceiling_by_inactive_lru(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            oldest = lane_root / "oldest"
            newest = lane_root / "newest"
            active = lane_root / "active"
            for lane in (oldest, newest, active):
                lane.mkdir(parents=True)
                (lane / "artifact.txt").write_text(lane.name, encoding="utf-8")

            lane_mtimes = {"oldest": 1.0, "newest": 2.0, "active": 3.0}
            removed = rust_build_status.prune_stale_lanes(
                repo_root=repo_root,
                processes=[
                    rust_build_status.RustProcess(
                        pid=7,
                        name="rustc.exe",
                        command_line=f"rustc --out-dir {active}\\debug",
                    )
                ],
                keep_warm_per_base=1,
                max_age_days=None,
                max_total_lane_bytes=120,
                lane_mtime=lambda path: lane_mtimes[path.name],
                lane_size=lambda _path: (60, 0),
            )

            self.assertEqual([path.name for path in removed], ["oldest"])
            self.assertFalse(oldest.exists())
            self.assertTrue(newest.exists())
            self.assertTrue(active.exists())

    def test_global_ceiling_accounts_for_lanes_already_selected_by_policy(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            protected = lane_root / "codex-core"
            warm_budget_victim = lane_root / "codex-core-2"
            protected.mkdir(parents=True)
            warm_budget_victim.mkdir(parents=True)

            removed = rust_build_status.prune_stale_lanes(
                repo_root=repo_root,
                processes=[],
                keep_warm_per_base=1,
                max_age_days=None,
                max_total_lane_bytes=60,
                lane_mtime=lambda path: 1.0 if path == warm_budget_victim else 2.0,
                lane_size=lambda _path: (60, 0),
            )

            self.assertEqual([path.name for path in removed], [warm_budget_victim.name])
            self.assertTrue(protected.exists())
            self.assertFalse(warm_budget_victim.exists())

    def test_target_ceiling_subtracts_non_lane_target_usage(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            target_root = repo_root / "codex-rs" / "target"
            lane_root = target_root / "lanes"
            oldest = lane_root / "oldest"
            newest = lane_root / "newest"
            for lane in (oldest, newest):
                lane.mkdir(parents=True)
            debug = target_root / "debug"
            debug.mkdir()
            (debug / "artifact.bin").write_bytes(b"x" * 80)

            removed = rust_build_status.prune_stale_lanes(
                repo_root=repo_root,
                processes=[],
                keep_warm_per_base=1,
                max_age_days=None,
                max_total_target_bytes=140,
                lane_mtime=lambda path: 1.0 if path == oldest else 2.0,
                lane_size=lambda _path: (60, 0),
            )

            self.assertEqual([path.name for path in removed], [oldest.name])
            self.assertFalse(oldest.exists())
            self.assertTrue(newest.exists())

    def test_prune_report_can_skip_disk_scan(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            (lane_root / "stale").mkdir(parents=True)

            report = rust_build_status.prune_stale_lanes_report(
                repo_root=repo_root,
                processes=[],
                dry_run=True,
                keep_warm_per_base=0,
                max_age_days=None,
                include_disk_report=False,
            )

        self.assertIn("would prune:", report)
        self.assertNotIn("target root:", report)

    def test_lane_size_workers_are_capped(self) -> None:
        self.assertEqual(rust_build_status.bounded_size_workers(99, 10), 4)
        self.assertEqual(rust_build_status.bounded_size_workers(2, 1), 1)

    def test_prune_cli_rejects_destructive_negative_budgets(self) -> None:
        for option, value in (
            ("--keep-warm-per-base", "-1"),
            ("--max-age-days", "-1"),
            ("--max-lane-gib", "-1"),
            ("--max-lane-bytes", "-1"),
            ("--max-total-lane-gib", "-1"),
            ("--max-total-lane-bytes", "-1"),
            ("--max-total-target-gib", "-1"),
            ("--max-total-target-bytes", "-1"),
            ("--size-workers", "0"),
        ):
            with (
                self.subTest(option=option),
                contextlib.redirect_stderr(io.StringIO()),
                self.assertRaises(SystemExit),
            ):
                rust_build_status.main(["prune", option, value])

    def test_lane_regexes_use_shared_tooling_patterns(self) -> None:
        patterns = tool_versions.cargo_lane_patterns()
        self.assertEqual(
            rust_build_status.LANE_RE.pattern,
            patterns["lane_path_pattern"],
        )
        self.assertEqual(
            rust_build_status.JUST_LANE_RE.pattern,
            patterns["just_lane_pattern"],
        )
        self.assertEqual(
            rust_build_status.SCRIPT_LANE_RE.pattern,
            patterns["script_lane_pattern"],
        )
        self.assertEqual(
            rust_build_status.JUST_FIXED_LANE_RE.pattern,
            patterns["just_fixed_lane_pattern"],
        )
        self.assertEqual(
            rust_build_status.JUST_FIXED_LANE_NAMES,
            patterns["just_fixed_lane_names"],
        )

    def test_cargo_lane_main_uses_parameterized_recipe_not_fixed_alias(self) -> None:
        patterns = tool_versions.cargo_lane_patterns()
        process = rust_build_status.RustProcess(
            pid=1,
            name="just.exe",
            command_line="just cargo-lane main cargo check",
        )

        self.assertEqual(rust_build_status.lane_name_for_process(process), "main")
        self.assertNotIn("cargo-lane-main", patterns["just_fixed_lane_names"])
        self.assertIsNone(
            rust_build_status.JUST_FIXED_LANE_RE.search("just cargo-lane-main")
        )

    def test_lane_pattern_registry_drives_python_and_powershell(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")

        cases = {
            r"cargo check --target-dir C:\repo\target\lanes\path-lane": "path-lane",
            "powershell -File scripts/cargo-lane.ps1 -Lane script-lane cargo check": "script-lane",
            "powershell -lane:core- cargo check": "core-",
            'powershell -LANE "quoted" cargo check': "quoted",
            "powershell -Lane:'quoted-' cargo check": "quoted-",
            "pwsh -Command python scripts/rust_build_status.py run-lane --lane core -- cargo check": "core",
            "python scripts/rust_build_status.py run-lane --lane='core-' -- cargo check": "core-",
            'python scripts/rust_build_status.py run-lane --lane "quoted" -- cargo check': "quoted",
            'just cargo-lane "core" cargo check': "core",
            "just test-lane-fast 'core-' -p codex-core": "core-",
            "just watch-lane recipe-lane": "recipe-lane",
            "just test-lane-main": "main",
            "just release-lane": "release",
            "python scripts/rust_build_status.py run-lane --lane auto -- cargo check": None,
            'python scripts/rust_build_status.py run-lane --lane="auto" -- cargo check': None,
            "powershell -Lane:'AUTO' cargo check": None,
            'just cargo-lane "auto" cargo check': None,
            "just cargo-lane auto-2 cargo check": "auto-2",
            r"cargo check --target-dir C:\repo\target\lanes\auto": "auto",
            "python script.py --lane-name core cargo check": None,
            "python script.py --lane core/invalid cargo check": None,
        }
        for index, (command_line, expected) in enumerate(cases.items(), start=1):
            with self.subTest(command_line=command_line):
                process = rust_build_status.RustProcess(
                    pid=index,
                    name="powershell.exe",
                    command_line=command_line,
                )
                self.assertEqual(
                    rust_build_status.lane_name_for_process(process), expected
                )
                self.assertEqual(
                    rust_build_status.shared_target_rust_processes([process]),
                    [process] if expected is None else [],
                )

        pattern_script = REPO_ROOT / "scripts" / "cargo-lane-patterns.ps1"
        command_lines_json = json.dumps(list(cases))
        command = (
            f". {ps_single_quote(pattern_script)}; "
            f"$commandLines = ConvertFrom-Json {ps_single_quote(command_lines_json)}; "
            "$results = @($commandLines | ForEach-Object { "
            "[PSCustomObject]@{names = @(Get-CargoLaneNamesFromCommandLines -CommandLines @($_))} "
            "}); ConvertTo-Json -Depth 4 -Compress -InputObject $results"
        )
        result = subprocess.run(
            [
                shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                command,
            ],
            cwd=REPO_ROOT,
            text=True,
            capture_output=True,
            check=False,
            creationflags=CREATE_NO_WINDOW,
            timeout=30,
        )
        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertEqual(
            json.loads(result.stdout),
            [{"names": [name] if name is not None else []} for name in cases.values()],
        )

        cargo_lane_text = (REPO_ROOT / "scripts" / "cargo-lane.ps1").read_text(
            encoding="utf-8"
        )
        self.assertIn("Get-CargoLaneNamesFromCommandLines", cargo_lane_text)
        self.assertNotIn("watch-lane", cargo_lane_text)
        self.assertNotIn("release-lane", cargo_lane_text)

    def test_lane_report_marks_active_lanes_and_emits_safe_prune_suggestions(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            stale_lane = lane_root / "stale"
            prunable_lane = lane_root / "stale-2"
            active_lane = lane_root / "active"
            stale_lane.mkdir(parents=True)
            prunable_lane.mkdir(parents=True)
            active_lane.mkdir(parents=True)
            (stale_lane / "artifact.txt").write_text("stale", encoding="utf-8")

            report = rust_build_status.lane_report(
                repo_root=repo_root,
                processes=[
                    rust_build_status.RustProcess(
                        pid=7,
                        name="rustc.exe",
                        command_line=f"rustc --out-dir {active_lane}\\debug",
                    )
                ],
            )

        self.assertIn("active: active", report)
        self.assertIn("stale: stale", report)
        self.assertIn("warm-protected: stale", report)
        self.assertIn("prunable:", report)
        self.assertIn("stale-2", report)
        self.assertIn("safe prune suggestions:", report)
        self.assertIn("just target-prune", report)
        self.assertNotIn("Remove-Item -Recurse -Force", report)
        self.assertNotIn("active\\debug", report)

    def test_trash_dirs_are_pending_deletion_not_lanes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            (lane_root / "core").mkdir(parents=True)
            trash = lane_root / "core.trash-20260901000000000"
            (trash / "debug").mkdir(parents=True)
            stamp = trash / ".lane-last-used"
            stamp.touch()
            os.utime(stamp, (1, 1))

            report = rust_build_status.lane_report(repo_root=repo_root, processes=[])
            planned = rust_build_status.prune_stale_lanes(
                repo_root=repo_root, processes=[], dry_run=True
            )
            with (
                mock.patch.object(
                    rust_build_status,
                    "remove_tree_allow_readonly",
                    side_effect=PermissionError("held"),
                ),
                contextlib.redirect_stderr(io.StringIO()),
                self.assertRaisesRegex(OSError, "pending paths"),
            ):
                rust_build_status.prune_stale_lanes(repo_root=repo_root, processes=[])
            remaining = sorted(
                path.name for path in lane_root.iterdir() if path.is_dir()
            )

            self.assertIn("stale: core\n", report + "\n")
            self.assertIn(
                "pending deletion (renamed by an earlier prune): " + trash.name,
                report,
            )
            self.assertNotIn("prunable:", report)
            self.assertEqual(planned, [trash])
            # A failed retry keeps the same name instead of growing another suffix.
            self.assertEqual(remaining, ["core", trash.name])

    def test_lane_report_separates_active_quarantined_and_prunable(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            for name in ("ui", "expired", "stuck"):
                (lane_root / name).mkdir(parents=True)
            (lane_root / "stuck" / ".lane-cleanup-unconfirmed").touch()
            expired_stamp = lane_root / "expired" / ".lane-last-used"
            expired_stamp.touch()
            os.utime(expired_stamp, (1, 1))

            report = rust_build_status.lane_report(
                repo_root=repo_root,
                processes=[
                    rust_build_status.RustProcess(
                        pid=7,
                        name="cargo.exe",
                        # The invocation's casing differs from the on-disk lane.
                        command_line=r"cargo build --target-dir C:\r\target\lanes\UI",
                    )
                ],
            )

        self.assertIn("active: ui\n", report)
        self.assertNotIn("active without directory", report)
        self.assertIn("): stuck", report)
        self.assertIn("quarantined (process cleanup unconfirmed", report)
        # Age makes it prunable, so it must not also be listed as warm-protected.
        self.assertIn("  expired", report)
        self.assertNotIn("warm-protected: expired", report)

    def test_build_doctor_displays_reserved_lane_without_process(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            with mock.patch.dict(
                rust_build_status.os.environ,
                {"CODEX_CARGO_LANE_ACTIVE_NAMES": "reserved"},
                clear=True,
            ):
                snapshot = rust_build_status.BuildStatusSnapshot.collect(
                    repo_root=repo_root,
                    processes=[],
                )
            report = rust_build_status.build_doctor_report(
                repo_root=repo_root,
                snapshot=snapshot,
                tool_lookup=lambda _name: None,
                env={},
            )

        self.assertIn("active lanes: reserved", report)


if __name__ == "__main__":
    unittest.main()
