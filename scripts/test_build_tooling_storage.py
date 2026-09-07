#!/usr/bin/env python3

import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import time
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


class BuildToolingStorageTest(unittest.TestCase):
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
                rust_build_status.subprocess,
                "run",
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

    def test_run_lane_keeps_shell_fallback_when_core_helpers_are_required(
        self,
    ) -> None:
        target = Path("C:/target/lane")
        command = [
            "just",
            "_test-lane-fast-reserved",
            "-p",
            "codex-core",
            "test(core_filter)",
        ]
        with (
            mock.patch.dict(rust_build_status.os.environ, {}, clear=True),
            mock.patch.object(
                rust_build_status,
                "reserve_cargo_lane",
                return_value=contextlib.nullcontext(("unit", target)),
            ),
            mock.patch.object(rust_build_status.shutil, "which", return_value=None),
            mock.patch.object(
                rust_build_status.subprocess,
                "run",
                return_value=subprocess.CompletedProcess(command, 0),
            ) as run,
        ):
            result = rust_build_status.run_in_cargo_lane(
                repo_root=Path.cwd(),
                requested_lane="unit",
                command=command,
            )

        self.assertEqual(result, 0)
        self.assertEqual(run.call_args.args[0], command)
        child_env = run.call_args.kwargs["env"]
        self.assertNotIn("NEXTEST_PROFILE", child_env)
        self.assertEqual(child_env["CODEX_CARGO_LANE_TARGET_DIR"], str(target))

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
                )

                self.assertEqual(actual, expected)
                self.assertEqual(child_env["NEXTEST_PROFILE"], "fast")
                self.assertEqual(
                    child_env["RUST_MIN_STACK"],
                    rust_build_status.RUST_MIN_STACK_BYTES,
                )

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

    def test_cargo_config_disables_duplicate_incremental_cache_by_default(self) -> None:
        config = load_toml(REPO_ROOT / "codex-rs" / ".cargo" / "config.toml")

        self.assertFalse(config["build"]["incremental"])

    def test_missing_lane_mtime_is_safe_during_concurrent_gc(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            missing_lane = Path(temp_dir) / "already-pruned"

            self.assertEqual(rust_build_status.lane_last_used_mtime(missing_lane), 0.0)

    def test_rust_build_doctor_reports_cache_linker_and_contention(self) -> None:
        report = rust_build_status.build_doctor_report(
            repo_root=REPO_ROOT,
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
        self.assertIn("active Rust processes: 2 total, 1 shared-target, 1 lane", report)
        self.assertIn(
            "shared-target jobs are active; prefer `just test-lane-fast <lane> ...`",
            report,
        )

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

    def test_windows_process_discovery_warns_on_failure(self) -> None:
        stderr = io.StringIO()
        with (
            mock.patch.object(
                rust_build_status.subprocess,
                "run",
                side_effect=subprocess.TimeoutExpired("powershell", 10),
            ),
            contextlib.redirect_stderr(stderr),
        ):
            self.assertEqual(rust_build_status.active_rust_processes_windows(), [])

        self.assertIn("warning: Windows Rust process scan failed", stderr.getvalue())

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
        with mock.patch.object(
            rust_build_status.os,
            "scandir",
            return_value=contextlib.nullcontext([older, newer, unrelated]),
        ):
            candidates = rust_build_status._lane_reservation_candidates(
                root,
                "unit",
                prefer_warm=True,
            )

        self.assertEqual(
            [candidate.name for candidate in candidates[:2]],
            ["unit-2", "unit"],
        )
        self.assertEqual(older.stat_calls, 1)
        self.assertEqual(newer.stat_calls, 1)
        self.assertEqual(unrelated.stat_calls, 0)
        self.assertFalse(older.assert_follow_symlinks)

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
        self.assertIn("just target-prune", report)

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

        with tempfile.TemporaryDirectory() as temp_dir:
            with (
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
            with mock.patch.object(Path, "stat", side_effect=PermissionError("denied")):
                self.assertTrue(rust_build_status.cargo_lock_is_busy(lane))
                self.assertTrue(rust_build_status.lane_active_lock_is_held(lane))

    def test_prune_rechecks_lane_lock_before_delete(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            lane = lane_root / "late-busy"
            lane.mkdir(parents=True)
            snapshot = rust_build_status.BuildStatusSnapshot.collect(
                repo_root=repo_root,
                processes=[],
            )

            with mock.patch.object(
                rust_build_status,
                "cargo_lock_is_busy",
                return_value=True,
            ):
                removed = rust_build_status.prune_stale_lanes(
                    repo_root=repo_root,
                    snapshot=snapshot,
                    keep_warm_per_base=0,
                    max_age_days=None,
                )

            self.assertEqual(removed, [])
            self.assertTrue(lane.exists())

    def test_prune_rechecks_active_reservation_before_delete(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane = repo_root / "codex-rs" / "target" / "lanes" / "late-reserved"
            lane.mkdir(parents=True)
            snapshot = rust_build_status.BuildStatusSnapshot.collect(
                repo_root=repo_root,
                processes=[],
            )

            with mock.patch.object(
                rust_build_status, "lane_active_lock_is_held", return_value=True
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

    def test_prune_stale_lanes_keeps_two_newest_warm_lanes_per_base(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            lane_root = repo_root / "codex-rs" / "target" / "lanes"
            newest = lane_root / "codex-core"
            middle = lane_root / "codex-core-2"
            oldest = lane_root / "codex-core-3"
            for lane in (newest, middle, oldest):
                lane.mkdir(parents=True)
                (lane / "artifact.txt").write_text(lane.name, encoding="utf-8")

            removed = rust_build_status.prune_stale_lanes(
                repo_root=repo_root,
                processes=[],
                keep_warm_per_base=2,
            )

            self.assertEqual([path.name for path in removed], ["codex-core-3"])
            self.assertTrue(newest.exists())
            self.assertTrue(middle.exists())
            self.assertFalse(oldest.exists())

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

        command_lines = [
            r"cargo check --target-dir C:\repo\target\lanes\path-lane",
            "powershell -File scripts/cargo-lane.ps1 -Lane script-lane cargo check",
            "just watch-lane recipe-lane",
            "just test-lane-main",
            "just release-lane",
        ]
        expected = {
            rust_build_status.lane_name_for_process(
                rust_build_status.RustProcess(
                    pid=index,
                    name="powershell.exe",
                    command_line=command_line,
                )
            )
            for index, command_line in enumerate(command_lines, start=1)
        }
        self.assertNotIn(None, expected)

        pattern_script = REPO_ROOT / "scripts" / "cargo-lane-patterns.ps1"
        command_lines_json = json.dumps(command_lines)
        command = (
            f". {ps_single_quote(pattern_script)}; "
            f"$commandLines = ConvertFrom-Json {ps_single_quote(command_lines_json)}; "
            "$names = @(Get-CargoLaneNamesFromCommandLines -CommandLines $commandLines); "
            "ConvertTo-Json -Compress -InputObject $names"
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
        self.assertEqual(set(json.loads(result.stdout)), expected)

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
