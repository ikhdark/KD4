#!/usr/bin/env python3

from __future__ import annotations

import json
import subprocess
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest import mock

from scripts import workflow_preflight


class WorkflowPreflightTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.repo = Path(self.temp.name)
        subprocess.run(["git", "init", "-q", str(self.repo)], check=True)
        subprocess.run(
            ["git", "-C", str(self.repo), "config", "user.email", "test@example.com"],
            check=True,
        )
        subprocess.run(
            ["git", "-C", str(self.repo), "config", "user.name", "Test"],
            check=True,
        )
        (self.repo / "src").mkdir()
        (self.repo / "src" / "lib.rs").write_text(
            "pub fn value() {}\n", encoding="utf-8"
        )
        subprocess.run(["git", "-C", str(self.repo), "add", "."], check=True)
        subprocess.run(
            ["git", "-C", str(self.repo), "commit", "-qm", "baseline"],
            check=True,
        )
        self.manifest_path = self.repo / "preflight.json"

    def tearDown(self) -> None:
        self.temp.cleanup()

    def manifest(self, assignment_id: str = "root:one") -> dict[str, object]:
        return {
            "schema_version": 1,
            "assignment_id": assignment_id,
            "root_task_id": "root-task",
            "repository_root": str(self.repo),
            "starting_revision": "auto",
            "path_claims": [{"path": "src", "recursive": True}],
            "contract_claims": ["runtime"],
            "dependencies": [],
            "generated_outputs": [],
            "generated_output_owner": "none",
            "validation_owner": assignment_id,
            "validation_commands": ["cargo test -p example"],
            "cargo_lane": {
                "target_dir": f"target/{assignment_id.replace(':', '-')}",
                "cargo_home": ".cargo-home",
            },
            "workspace_strategy": "auto",
        }

    def resolve(
        self,
        value: dict[str, object],
        against: tuple[dict[str, object], ...] = (),
    ) -> dict[str, object]:
        return workflow_preflight.resolve_manifest(
            value,
            manifest_path=self.manifest_path,
            against=against,
        )

    def test_resolved_receipt_records_revision_claims_and_owners(self) -> None:
        resolved = self.resolve(self.manifest())
        self.assertEqual(resolved["assignment_id"], "root:one")
        self.assertEqual(resolved["contract_claims"], ["runtime"])
        self.assertIn("commit", resolved["starting_revision"])
        self.assertIn("workspace_fingerprint", resolved["starting_revision"])
        self.assertIn("repository_id", resolved)
        self.assertIn("workspace_id", resolved)
        self.assertEqual(resolved["validation_owner"], "root:one")
        self.assertEqual(resolved["generated_output_owner"], "none")
        self.assertEqual(resolved["advisories"], [])
        self.assertIn("manifest_fingerprint", resolved)
        self.assertIn("expires_at", resolved)

    def test_empty_identity_is_repaired_and_then_stable(self) -> None:
        identity = self.repo / ".git" / "codex" / "test-id"
        identity.parent.mkdir(parents=True, exist_ok=True)
        identity.write_text("", encoding="utf-8")

        first = workflow_preflight.persistent_identity(identity)
        second = workflow_preflight.persistent_identity(identity)

        self.assertEqual(first, second)
        self.assertEqual(identity.read_text(encoding="utf-8").strip(), first)

    def test_receipt_expiry_and_lease_bounds(self) -> None:
        now = datetime(2026, 7, 31, tzinfo=timezone.utc)
        receipt = self.resolve(self.manifest())
        receipt["expires_at"] = (now + timedelta(seconds=1)).isoformat()

        self.assertTrue(workflow_preflight.receipt_is_active(receipt, now))
        self.assertFalse(
            workflow_preflight.receipt_is_active(receipt, now + timedelta(seconds=2))
        )
        receipt.pop("expires_at")
        self.assertFalse(workflow_preflight.receipt_is_active(receipt, now))
        with self.assertRaisesRegex(workflow_preflight.PreflightError, "lease_seconds"):
            workflow_preflight.resolve_manifest(
                self.manifest(),
                manifest_path=self.manifest_path,
                lease_seconds=1,
                now=now,
            )

    def test_stale_start_revision_is_rejected(self) -> None:
        value = self.manifest()
        value["starting_revision"] = "0" * 40
        with self.assertRaisesRegex(workflow_preflight.PreflightError, "stale"):
            self.resolve(value)

    def test_overlap_and_shared_cargo_lane_are_advisory(self) -> None:
        active = self.resolve(self.manifest("root:first"))
        contender = self.manifest("root:second")
        contender["cargo_lane"] = {
            "target_dir": "target/root-first",
            "cargo_home": ".cargo-home",
        }
        resolved = self.resolve(contender, (active,))
        self.assertEqual(
            [advisory["kind"] for advisory in resolved["advisories"]],
            ["claim_overlap", "cargo_lane_overlap"],
        )

    def test_case_only_claim_aliases_overlap_on_case_insensitive_filesystems(
        self,
    ) -> None:
        left = {"path": "src/Foo", "recursive": True}
        right = {"path": "src/foo/child.rs", "recursive": False}
        self.assertTrue(
            workflow_preflight.claims_overlap(left, right, case_insensitive=True)
        )

    def test_case_detection_reports_case_sensitive_when_alias_is_absent(
        self,
    ) -> None:
        with mock.patch.object(Path, "exists", return_value=False):
            self.assertFalse(
                workflow_preflight.repository_paths_are_case_insensitive(self.repo)
            )

    def test_workspace_fingerprint_hashes_dirty_content_not_only_status_shape(
        self,
    ) -> None:
        path = self.repo / "src" / "lib.rs"
        path.write_text("pub fn value() { one(); }\n", encoding="utf-8")
        first = workflow_preflight.workspace_fingerprint(self.repo)
        path.write_text("pub fn value() { two(); }\n", encoding="utf-8")
        second = workflow_preflight.workspace_fingerprint(self.repo)
        self.assertNotEqual(first, second)

    def test_cargo_lane_aliases_are_advisory(self) -> None:
        active = self.resolve(self.manifest("root:first"))
        contender = self.manifest("root:second")
        contender["path_claims"] = [{"path": "docs", "recursive": True}]
        contender["contract_claims"] = ["documentation"]
        contender["cargo_lane"] = {
            "target_dir": "./target/root-first",
            "cargo_home": ".cargo-home",
        }
        resolved = self.resolve(contender, (active,))
        self.assertEqual(
            resolved["advisories"],
            [
                {
                    "kind": "cargo_lane_overlap",
                    "assignment_id": "root:first",
                    "target_dir": str((self.repo / "target/root-first").resolve()),
                }
            ],
        )

    def test_isolated_worktree_allows_intentional_overlap_with_distinct_lane(
        self,
    ) -> None:
        active_value = self.manifest("root:first")
        active_value["workspace_strategy"] = "shared"
        active = self.resolve(active_value)
        with tempfile.TemporaryDirectory() as worktree_parent:
            isolated = Path(worktree_parent) / "isolated"
            subprocess.run(
                [
                    "git",
                    "-C",
                    str(self.repo),
                    "worktree",
                    "add",
                    "--detach",
                    str(isolated),
                    "HEAD",
                ],
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            try:
                contender = self.manifest("root:second")
                contender["repository_root"] = str(isolated)
                contender["workspace_strategy"] = "isolated"
                contender["cargo_lane"] = {
                    "target_dir": str(isolated / "target" / "root-second"),
                    "cargo_home": str(isolated / ".cargo-home"),
                }
                resolved = self.resolve(contender, (active,))
                self.assertEqual(resolved["repository_id"], active["repository_id"])
                self.assertNotEqual(resolved["workspace_id"], active["workspace_id"])
            finally:
                subprocess.run(
                    [
                        "git",
                        "-C",
                        str(self.repo),
                        "worktree",
                        "remove",
                        "--force",
                        str(isolated),
                    ],
                    check=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                )

    def test_generated_output_requires_claim_and_owner(self) -> None:
        value = self.manifest()
        value["generated_outputs"] = ["generated/schema.json"]
        with self.assertRaisesRegex(
            workflow_preflight.PreflightError, "generated_output_owner"
        ):
            self.resolve(value)

    def test_unresolved_template_placeholder_is_rejected(self) -> None:
        value = self.manifest()
        value["assignment_id"] = "root:<assignment>"
        with self.assertRaisesRegex(
            workflow_preflight.PreflightError, "template placeholder"
        ):
            self.resolve(value)

    def test_main_registers_overlaps_with_advisories_and_release_removes_receipts(
        self,
    ) -> None:
        first = self.manifest("root:first")
        first_path = self.repo / "first.json"
        first_path.write_text(json.dumps(first), encoding="utf-8")
        self.assertEqual(workflow_preflight.main([str(first_path)]), 0)

        second = self.manifest("root:second")
        second_path = self.repo / "second.json"
        second_path.write_text(json.dumps(second), encoding="utf-8")
        self.assertEqual(workflow_preflight.main([str(second_path)]), 0)

        self.assertEqual(
            workflow_preflight.main(
                [
                    "--release",
                    "root:first",
                    "--repository-root",
                    str(self.repo),
                ]
            ),
            0,
        )
        self.assertEqual(workflow_preflight.main([str(second_path)]), 0)

    def test_failed_output_write_removes_new_registry_receipt(self) -> None:
        manifest = self.manifest("root:output-failure")
        manifest_path = self.repo / "output-failure.json"
        manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
        output_directory = self.repo / "receipt-directory"
        output_directory.mkdir()

        self.assertEqual(
            workflow_preflight.main(
                [str(manifest_path), "--output", str(output_directory)]
            ),
            2,
        )
        self.assertFalse(
            workflow_preflight.registry_receipt_path(
                self.repo, "root:output-failure"
            ).exists()
        )

    def test_failed_output_write_restores_previous_registry_receipt(self) -> None:
        manifest = self.manifest("root:output-restore")
        manifest_path = self.repo / "output-restore.json"
        manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
        self.assertEqual(workflow_preflight.main([str(manifest_path)]), 0)
        receipt_path = workflow_preflight.registry_receipt_path(
            self.repo, "root:output-restore"
        )
        previous = receipt_path.read_bytes()
        output_directory = self.repo / "receipt-directory"
        output_directory.mkdir()

        self.assertEqual(
            workflow_preflight.main(
                [str(manifest_path), "--output", str(output_directory)]
            ),
            2,
        )
        self.assertEqual(receipt_path.read_bytes(), previous)


if __name__ == "__main__":
    unittest.main()
