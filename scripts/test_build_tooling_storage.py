#!/usr/bin/env python3

import contextlib
import io
import importlib.util
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib
import unittest
from unittest import mock

from scripts import rust_build_status
from scripts import tool_versions


REPO_ROOT = Path(__file__).resolve().parents[1]
CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)


def powershell() -> str | None:
    # Prefer Windows PowerShell 5.1: the justfile invokes these scripts via
    # `powershell -NoProfile -File ...`, so tests should exercise the same
    # host (5.1 has stricter native-stderr and StrictMode semantics).
    return shutil.which("powershell") or shutil.which("pwsh")


def pwsh_only() -> str | None:
    # invoke-rust-perf-env.ps1 runs under pwsh 7.4+ in production (recipes
    # invoke it inline in the just-shell pwsh session), and its -NoSccache
    # proof depends on pwsh's empty-env-var semantics, so its tests must not
    # fall back to Windows PowerShell 5.1.
    return shutil.which("pwsh")


def ps_single_quote(value: str | Path) -> str:
    return "'" + str(value).replace("'", "''") + "'"


def load_just_shell_module():
    path = REPO_ROOT / "scripts" / "just-shell.py"
    spec = importlib.util.spec_from_file_location("just_shell", path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_format_module():
    path = REPO_ROOT / "scripts" / "format.py"
    spec = importlib.util.spec_from_file_location("format_script", path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_root_maintenance_module():
    path = REPO_ROOT / "scripts" / "root_maintenance.py"
    spec = importlib.util.spec_from_file_location("root_maintenance", path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_toml(path: Path):
    return tomllib.loads(path.read_text(encoding="utf-8"))


class BuildToolingStorageTest(unittest.TestCase):
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
        self.assertIn("active Rust jobs: 2 total, 1 shared-target, 1 lane", report)
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
        self.assertNotIn("Where-Object", command)

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

    def test_prune_stray_target_dirs_removes_read_only_trees(self) -> None:
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

            removed = rust_build_status.prune_stray_cargo_target_dirs(
                repo_root=repo_root,
            )

        self.assertEqual(
            [path.name for path in removed], ["codex-tools-responses-check"]
        )
        self.assertFalse(stray_root.exists())

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
            ("--size-workers", "0"),
        ):
            with (
                self.subTest(option=option),
                contextlib.redirect_stderr(io.StringIO()),
                self.assertRaises(SystemExit),
            ):
                rust_build_status.main(["prune", option, value])

    def test_lane_regexes_use_shared_tooling_patterns(self) -> None:
        self.assertEqual(
            rust_build_status.LANE_RE.pattern,
            tool_versions.LANE_PATH_PATTERN,
        )
        self.assertEqual(
            rust_build_status.JUST_LANE_RE.pattern,
            tool_versions.JUST_LANE_PATTERN,
        )

    def test_new_local_lane_recipes_are_detected_from_just_commands(self) -> None:
        cargo_lane_text = (REPO_ROOT / "scripts" / "cargo-lane.ps1").read_text(
            encoding="utf-8"
        )
        self.assertIn("watch-lane", cargo_lane_text)
        self.assertIn("coverage-lane", cargo_lane_text)

        for command in (
            "just watch-lane codex-core",
            "just coverage-lane codex-core",
        ):
            self.assertEqual(
                rust_build_status.lane_name_for_process(
                    rust_build_status.RustProcess(
                        pid=99,
                        name="just.exe",
                        command_line=command,
                    )
                ),
                "codex-core",
            )

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
        self.assertIn("Remove-Item -Recurse -LiteralPath", report)
        self.assertNotIn("active\\debug", report)


if __name__ == "__main__":
    unittest.main()
