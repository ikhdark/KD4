from __future__ import annotations

import json
import subprocess
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]


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

    def run_audit(
        self,
        *args: str,
        expected_returncode: int = 0,
    ) -> tuple[subprocess.CompletedProcess[str], dict[str, object]]:
        completed = subprocess.run(
            [
                "just",
                "kd4-sync-audit",
                "--repo-root",
                str(self.repo),
                "--json",
                *args,
            ],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
        self.assertEqual(
            completed.returncode,
            expected_returncode,
            msg=completed.stderr or completed.stdout,
        )
        payload = json.loads(completed.stdout)
        self.assertEqual(payload["repository"], str(self.repo.resolve()))
        return completed, payload

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

    def test_cli_rejects_stale_local_upstream_ref_through_git_remote(self) -> None:
        self.create_divergence(conflict=False)
        stale = self.git("rev-parse", "refs/remotes/upstream/main").stdout.strip()
        self.git("update-ref", "refs/remotes/upstream/main", self.base)

        _, audit = self.run_audit("--strict", expected_returncode=1)

        self.assertEqual(audit["upstream_remote_tip"], stale)
        self.assertTrue(audit["upstream_ref_stale"])
        self.assertFalse(audit["safe_for_in_place_sync"])

    def test_cli_accepts_clean_trial_merge_for_pristine_worktree(self) -> None:
        self.create_divergence(conflict=False)

        _, audit = self.run_audit("--strict")

        self.assertEqual((audit["ahead"], audit["behind"]), (1, 1))
        self.assertEqual(audit["merge_forecast"]["status"], "clean")
        self.assertTrue(audit["safe_for_in_place_sync"])

    def test_cli_requires_isolated_strategy_for_real_merge_conflict(self) -> None:
        self.create_divergence(conflict=True)

        _, audit = self.run_audit("--strict", expected_returncode=1)

        self.assertEqual(audit["merge_forecast"]["status"], "conflicts")
        self.assertIn("shared.txt", audit["merge_forecast"]["conflict_paths"])
        self.assertFalse(audit["safe_for_in_place_sync"])
        self.assertEqual(
            audit["recommended_strategy"],
            "isolated-worktree-capability-by-capability",
        )

    def test_cli_rejects_dirty_worktree_through_strict_exit_status(self) -> None:
        self.create_divergence(conflict=False)
        (self.repo / "local.txt").write_text("dirty\n", encoding="utf-8")

        _, audit = self.run_audit("--strict", expected_returncode=1)

        self.assertEqual(audit["worktree"]["untracked_paths"], 1)
        self.assertFalse(audit["safe_for_in_place_sync"])

    def test_cli_reports_only_real_path_for_modify_delete_conflict(self) -> None:
        self.git("checkout", "-b", "upstream")
        self.git("rm", "shared.txt")
        self.git("commit", "-m", "upstream deletes shared")
        upstream = self.git("rev-parse", "HEAD").stdout.strip()
        self.git("update-ref", "refs/remotes/upstream/main", upstream)
        self.git("update-ref", "refs/heads/main", upstream)
        self.git("remote", "add", "upstream", str(self.repo))

        self.git("checkout", "-b", "fork", self.base)
        (self.repo / "shared.txt").write_text(
            "fork modifies shared\n", encoding="utf-8"
        )
        self.git("add", "shared.txt")
        self.git("commit", "-m", "fork modifies shared")

        _, audit = self.run_audit("--strict", expected_returncode=1)

        forecast = audit["merge_forecast"]
        self.assertEqual(forecast["status"], "conflicts")
        self.assertEqual(forecast["conflict_paths"], ["shared.txt"])
        self.assertTrue(
            any(
                "CONFLICT (modify/delete)" in message
                for message in forecast["messages"]
            )
        )

    def test_cli_keeps_hex_named_conflict_path_distinct_from_result_tree(self) -> None:
        hex_path = "b" * 40
        (self.repo / hex_path).write_text("base\n", encoding="utf-8")
        self.git("add", hex_path)
        self.git("commit", "-m", "add hex-named path")
        self.base = self.git("rev-parse", "HEAD").stdout.strip()

        self.git("checkout", "-b", "upstream")
        (self.repo / hex_path).write_text("upstream\n", encoding="utf-8")
        self.git("add", hex_path)
        self.git("commit", "-m", "upstream modifies hex-named path")
        upstream = self.git("rev-parse", "HEAD").stdout.strip()
        self.git("update-ref", "refs/remotes/upstream/main", upstream)
        self.git("update-ref", "refs/heads/main", upstream)
        self.git("remote", "add", "upstream", str(self.repo))

        self.git("checkout", "-b", "fork", self.base)
        (self.repo / hex_path).write_text("fork\n", encoding="utf-8")
        self.git("add", hex_path)
        self.git("commit", "-m", "fork modifies hex-named path")

        _, audit = self.run_audit("--strict", expected_returncode=1)

        forecast = audit["merge_forecast"]
        self.assertRegex(forecast["result_tree"], r"^[0-9a-f]{40,64}$")
        self.assertNotEqual(forecast["result_tree"], hex_path)
        self.assertEqual(forecast["conflict_paths"], [hex_path])

    def test_cli_atomic_output_failure_removes_temporary_file(self) -> None:
        self.create_divergence(conflict=False)
        blocked_target = self.repo / "audit.json"
        blocked_target.mkdir()
        (blocked_target / "keep.txt").write_text("keep\n", encoding="utf-8")

        completed = subprocess.run(
            [
                "just",
                "kd4-sync-audit",
                "--repo-root",
                str(self.repo),
                "--output",
                str(blocked_target),
            ],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )

        self.assertNotEqual(completed.returncode, 0)
        self.assertTrue(blocked_target.is_dir())
        self.assertEqual(list(self.repo.glob(".audit.json.*.tmp")), [])


if __name__ == "__main__":
    unittest.main()
