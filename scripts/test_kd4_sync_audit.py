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

        self.git("checkout", "-b", "fork", self.base)
        (self.repo / "fork.txt").write_text("fork\n", encoding="utf-8")
        if conflict:
            (self.repo / "shared.txt").write_text("fork\n", encoding="utf-8")
        self.git("add", ".")
        self.git("commit", "-m", "fork")

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


if __name__ == "__main__":
    unittest.main()
