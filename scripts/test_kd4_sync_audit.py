from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path

from scripts import kd4_sync_audit


class Kd4SyncAuditTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tempdir.cleanup)
        self.repo = Path(self.tempdir.name)
        self.git("init", "-b", "main")
        self.git("config", "user.name", "KD4 Test")
        self.git("config", "user.email", "kd4@example.invalid")
        (self.repo / "shared.txt").write_text("base\n", encoding="utf-8")
        self.git("add", "shared.txt")
        self.git("commit", "-m", "base")
        self.base = self.git("rev-parse", "HEAD").stdout.strip()

    def git(self, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["git", *args],
            cwd=self.repo,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=check,
        )

    def create_divergence(self, *, conflict: bool) -> None:
        self.git("checkout", "-b", "upstream")
        (self.repo / "upstream.txt").write_text("upstream\n", encoding="utf-8")
        if conflict:
            (self.repo / "shared.txt").write_text("upstream\n", encoding="utf-8")
        self.git("add", ".")
        self.git("commit", "-m", "upstream")
        upstream = self.git("rev-parse", "HEAD").stdout.strip()
        self.git("update-ref", "refs/remotes/upstream/main", upstream)
        self.git("update-ref", "refs/heads/main", upstream)
        self.git("remote", "add", "upstream", str(self.repo))

        self.git("checkout", "-b", "fork", self.base)
        (self.repo / "fork.txt").write_text("fork\n", encoding="utf-8")
        if conflict:
            (self.repo / "shared.txt").write_text("fork\n", encoding="utf-8")
        self.git("add", ".")
        self.git("commit", "-m", "fork")

    def test_stale_local_upstream_ref_is_not_safe(self) -> None:
        self.create_divergence(conflict=False)
        stale = self.git("rev-parse", "refs/remotes/upstream/main").stdout.strip()
        self.git("update-ref", "refs/remotes/upstream/main", self.base)

        audit = kd4_sync_audit.audit_repository(self.repo)

        self.assertEqual(audit.upstream_remote_tip, stale)
        self.assertTrue(audit.upstream_ref_stale)
        self.assertFalse(audit.safe_for_in_place_sync)

    def test_clean_trial_merge_is_safe_for_pristine_worktree(self) -> None:
        self.create_divergence(conflict=False)

        audit = kd4_sync_audit.audit_repository(self.repo)

        self.assertEqual((audit.ahead, audit.behind), (1, 1))
        self.assertEqual(audit.merge_forecast.status, "clean")
        self.assertTrue(audit.safe_for_in_place_sync)

    def test_conflicting_trial_merge_requires_isolated_strategy(self) -> None:
        self.create_divergence(conflict=True)

        audit = kd4_sync_audit.audit_repository(self.repo)

        self.assertEqual(audit.merge_forecast.status, "conflicts")
        self.assertIn("shared.txt", audit.merge_forecast.conflict_paths)
        self.assertFalse(audit.safe_for_in_place_sync)
        self.assertEqual(
            audit.recommended_strategy,
            "isolated-worktree-capability-by-capability",
        )

    def test_dirty_worktree_is_never_reported_safe(self) -> None:
        self.create_divergence(conflict=False)
        (self.repo / "local.txt").write_text("dirty\n", encoding="utf-8")

        audit = kd4_sync_audit.audit_repository(self.repo)

        self.assertEqual(audit.worktree.untracked_paths, 1)
        self.assertFalse(audit.safe_for_in_place_sync)

    def test_modify_delete_message_does_not_add_prose_as_conflict_path(self) -> None:
        tree = "a" * 40
        completed = subprocess.CompletedProcess(
            ["git", "merge-tree"],
            1,
            stdout=(
                f"{tree}\n"
                "shared.txt\n"
                "\n"
                "CONFLICT (modify/delete): shared.txt deleted in HEAD and "
                "modified in upstream. Version upstream of shared.txt left in tree.\n"
            ),
            stderr="",
        )

        forecast = kd4_sync_audit.parse_merge_forecast(completed)

        self.assertEqual(forecast.conflict_paths, ("shared.txt",))

    def test_hex_conflict_path_is_not_mistaken_for_result_tree(self) -> None:
        hex_path = "b" * 40
        completed = subprocess.CompletedProcess(
            ["git", "merge-tree"],
            1,
            stdout=(
                "merge-tree-error\n"
                f"{hex_path}\n"
                "\n"
                f"CONFLICT (content): Merge conflict in {hex_path}\n"
            ),
            stderr="",
        )

        forecast = kd4_sync_audit.parse_merge_forecast(completed)

        self.assertIsNone(forecast.result_tree)
        self.assertEqual(forecast.conflict_paths, (hex_path,))

    def test_atomic_json_writer_removes_temp_file_on_serialization_error(
        self,
    ) -> None:
        target = self.repo / "audit.json"

        with self.assertRaises(TypeError):
            kd4_sync_audit.write_json_atomic(target, {"bad": object()})

        self.assertEqual(list(self.repo.glob("*.tmp")), [])
        self.assertFalse(target.exists())


if __name__ == "__main__":
    unittest.main()
