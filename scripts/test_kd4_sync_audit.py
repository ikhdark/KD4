from __future__ import annotations

import contextlib
import io
import json
import os
import shlex
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import kd4_sync_audit


class Kd4SyncAuditTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tempdir.cleanup)
        self.repo = Path(self.tempdir.name)

    def init_repo(self, shared_path: str = "shared.txt") -> None:
        self.git("init", "-b", "main")
        self.git("config", "user.name", "KD4 Test")
        self.git("config", "user.email", "kd4@example.invalid")
        (self.repo / shared_path).write_text("base\n", encoding="utf-8")
        self.git("add", shared_path)
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
            timeout=30,
        )

    def create_divergence(
        self, *, conflict: bool, shared_path: str = "shared.txt"
    ) -> None:
        self.init_repo(shared_path)
        self.git("checkout", "-b", "upstream")
        (self.repo / "upstream.txt").write_text("upstream\n", encoding="utf-8")
        if conflict:
            (self.repo / shared_path).write_text("upstream\n", encoding="utf-8")
        self.git("add", ".")
        self.git("commit", "-m", "upstream")
        upstream = self.git("rev-parse", "HEAD").stdout.strip()
        self.git("update-ref", "refs/remotes/upstream/main", upstream)
        self.git("update-ref", "refs/heads/main", upstream)
        self.git("remote", "add", "upstream", str(self.repo))

        self.git("checkout", "-b", "fork", self.base)
        (self.repo / "fork.txt").write_text("fork\n", encoding="utf-8")
        if conflict:
            (self.repo / shared_path).write_text("fork\n", encoding="utf-8")
        self.git("add", ".")
        self.git("commit", "-m", "fork")

    def run_audit(self, *args: str) -> tuple[int, dict]:
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = kd4_sync_audit.main(["--repo-root", str(self.repo), "--json", *args])
        return code, json.loads(output.getvalue())

    def test_first_unstaged_path_preserves_porcelain_columns(self) -> None:
        self.create_divergence(conflict=False)
        (self.repo / "shared.txt").write_text("changed\n", encoding="utf-8")
        audit = kd4_sync_audit.audit_repository(self.repo)
        self.assertEqual(audit.worktree.staged_paths, 0)
        self.assertEqual(audit.worktree.unstaged_paths, 1)

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

        head = self.git("rev-parse", "HEAD").stdout
        branch = self.git("symbolic-ref", "HEAD").stdout
        status = self.git("status", "--porcelain=v1").stdout
        audit = kd4_sync_audit.audit_repository(self.repo)
        self.assertEqual(self.git("rev-parse", "HEAD").stdout, head)
        self.assertEqual(self.git("symbolic-ref", "HEAD").stdout, branch)
        self.assertEqual(self.git("status", "--porcelain=v1").stdout, status)

        self.assertEqual((audit.ahead, audit.behind), (1, 1))
        self.assertEqual(audit.merge_forecast.status, "clean")
        self.assertEqual(audit.active_operations, ())
        self.assertTrue(audit.safe_for_in_place_sync)

    def test_paused_rebase_in_linked_worktree_is_not_safe(self) -> None:
        self.create_divergence(conflict=False)
        linked = self.repo / "linked"
        self.git("worktree", "add", "-b", "review", str(linked), "fork")
        editor = self.repo / ".git" / "sequence_editor.py"
        editor.write_text(
            "import sys\nfrom pathlib import Path\n"
            "p = Path(sys.argv[1])\n"
            "p.write_text(p.read_text().replace('pick ', 'edit ', 1))\n",
            encoding="utf-8",
        )
        env = {
            **os.environ,
            "GIT_SEQUENCE_EDITOR": (
                f"{shlex.quote(Path(sys.executable).as_posix())} "
                f"{shlex.quote(editor.as_posix())}"
            ),
        }
        subprocess.run(
            ["git", "rebase", "-i", self.base],
            cwd=linked,
            env=env,
            capture_output=True,
            check=True,
            timeout=30,
        )
        self.assertTrue((linked / ".git").is_file())
        self.repo = linked
        before = self.git("status").stdout
        code, payload = self.run_audit("--strict")
        self.assertEqual(code, 1)
        self.assertEqual(payload["worktree"]["changed_paths"], 0)
        self.assertEqual(payload["active_operations"], ["rebase"])
        self.assertFalse(payload["safe_for_in_place_sync"])
        self.assertEqual(self.git("status").stdout, before)

    def test_conflicting_trial_merge_requires_isolated_strategy(self) -> None:
        self.create_divergence(conflict=True)

        head = self.git("rev-parse", "HEAD").stdout
        branch = self.git("symbolic-ref", "HEAD").stdout
        status = self.git("status", "--porcelain=v1").stdout
        audit = kd4_sync_audit.audit_repository(self.repo)
        self.assertEqual(self.git("rev-parse", "HEAD").stdout, head)
        self.assertEqual(self.git("symbolic-ref", "HEAD").stdout, branch)
        self.assertEqual(self.git("status", "--porcelain=v1").stdout, status)

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

    def test_unicode_conflict_paths_round_trip_to_real_files(self) -> None:
        name = "caf\u00e9.txt"
        self.create_divergence(conflict=True, shared_path=name)
        self.git("config", "core.quotePath", "true")
        audit = kd4_sync_audit.audit_repository(self.repo)
        self.assertEqual(audit.merge_forecast.status, "conflicts")
        self.assertEqual(audit.merge_forecast.conflict_paths, (name,))
        self.assertTrue((self.repo / audit.merge_forecast.conflict_paths[0]).is_file())

    def test_unavailable_remote_preserves_local_facts_but_never_reports_safe(
        self,
    ) -> None:
        self.create_divergence(conflict=False)
        self.git("remote", "set-url", "upstream", str(self.repo / "missing-remote"))
        for strict in ((), ("--strict",)):
            with self.subTest(strict=strict):
                code, payload = self.run_audit(*strict)
                self.assertEqual(code, 2)
                self.assertEqual(payload["schema_version"], 2)
                self.assertFalse(payload["ok"])
                self.assertIsNone(payload["upstream_remote_tip"])
                self.assertIsNone(payload["upstream_ref_stale"])
                self.assertTrue(payload["upstream_remote_error"])
                self.assertEqual((payload["ahead"], payload["behind"]), (1, 1))
                self.assertEqual(payload["worktree"]["changed_paths"], 0)
                self.assertEqual(payload["merge_forecast"]["status"], "clean")
                self.assertFalse(payload["safe_for_in_place_sync"])

    def test_nonignored_output_is_rejected_before_auditing_or_writing(self) -> None:
        self.create_divergence(conflict=False)
        target = self.repo / "audit.json"
        with mock.patch.object(kd4_sync_audit, "audit_repository") as audit:
            code, payload = self.run_audit("--strict", "--output", str(target))
        self.assertEqual(code, 2)
        self.assertIn("would change the audited worktree", payload["error"])
        audit.assert_not_called()
        self.assertFalse(target.exists())
        self.assertEqual(self.git("status", "--porcelain=v1").stdout, "")

    def test_ignored_untracked_output_stays_safe_but_tracked_output_is_rejected(
        self,
    ) -> None:
        self.create_divergence(conflict=False)
        (self.repo / ".gitignore").write_text("/.audit/\n", encoding="utf-8")
        self.git("add", ".gitignore")
        self.git("commit", "-m", "ignore local reports")
        target = self.repo / ".audit" / "report.json"
        for _ in range(2):
            code, payload = self.run_audit("--strict", "--output", str(target))
            self.assertEqual(code, 0)
            self.assertTrue(payload["safe_for_in_place_sync"])
            self.assertEqual(json.loads(target.read_text(encoding="utf-8")), payload)
        self.assertEqual(self.git("status", "--porcelain=v1").stdout, "")
        self.git("add", "-f", ".audit/report.json")
        self.git("commit", "-m", "track report")
        before = target.read_bytes()
        code, payload = self.run_audit("--output", str(target))
        self.assertEqual(code, 2)
        self.assertIn("would change the audited worktree", payload["error"])
        self.assertEqual(target.read_bytes(), before)

    def test_output_write_error_preserves_complete_json_diagnostics(self) -> None:
        self.create_divergence(conflict=False)
        with tempfile.TemporaryDirectory() as destination:
            target = Path(destination)
            code, payload = self.run_audit("--output", str(target))
            self.assertEqual(code, 2)
            self.assertFalse(payload["ok"])
            self.assertIn("could not write audit output", payload["output_error"])
            self.assertEqual((payload["ahead"], payload["behind"]), (1, 1))
            self.assertEqual(payload["merge_forecast"]["status"], "clean")
            self.assertTrue(target.is_dir())
            self.assertEqual(list(target.parent.glob(f".{target.name}.*.tmp")), [])

    def test_modify_delete_message_does_not_add_prose_as_conflict_path(self) -> None:
        tree = "a" * 40
        completed = subprocess.CompletedProcess(
            ["git", "merge-tree"],
            1,
            stdout=(
                f"{tree}\0shared.txt\0\0"
                "1\0shared.txt\0CONFLICT (modify/delete)\0"
                "CONFLICT (modify/delete): shared.txt deleted in HEAD and "
                "modified in upstream. Version upstream of shared.txt left in tree.\n\0"
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
            stdout="\0".join(
                (
                    "merge-tree-error",
                    hex_path,
                    "",
                    "1",
                    hex_path,
                    "CONFLICT (contents)",
                    f"CONFLICT (content): Merge conflict in {hex_path}\n",
                    "",
                )
            ),
            stderr="",
        )

        forecast = kd4_sync_audit.parse_merge_forecast(completed)

        self.assertIsNone(forecast.result_tree)
        self.assertEqual(forecast.conflict_paths, (hex_path,))

    def test_malformed_merge_message_is_not_reported_clean(self) -> None:
        completed = subprocess.CompletedProcess(
            ["git", "merge-tree"],
            0,
            stdout="\0".join(("a" * 40, "", "2", "missing-fields", "")),
            stderr="",
        )
        forecast = kd4_sync_audit.parse_merge_forecast(completed)
        self.assertEqual(forecast.status, "error")
        self.assertIn("malformed", forecast.messages[-1])

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
