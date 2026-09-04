#!/usr/bin/env python3

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from datetime import datetime
from pathlib import Path


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
        self.script = Path(__file__).with_name("workflow_preflight.py").resolve()

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

    def run_manifest(
        self,
        manifest: dict[str, object],
        *options: str,
    ) -> subprocess.CompletedProcess[str]:
        assignment = str(manifest.get("assignment_id", "invalid"))
        safe_name = assignment.replace(":", "-").replace("<", "-").replace(">", "-")
        manifest_path = self.repo / f"manifest-{safe_name}.json"
        manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
        return subprocess.run(
            [sys.executable, str(self.script), str(manifest_path), *options],
            check=False,
            capture_output=True,
            text=True,
            encoding="utf-8",
        )

    def receipt_files(self) -> list[Path]:
        registry = self.repo / ".git" / "codex" / "workflow-preflight"
        return sorted(registry.glob("*.json")) if registry.is_dir() else []

    def test_cli_receipt_records_revision_claims_and_owners(self) -> None:
        completed = self.run_manifest(self.manifest())

        self.assertEqual(completed.returncode, 0, completed.stderr)
        resolved = json.loads(completed.stdout)
        self.assertEqual(resolved["assignment_id"], "root:one")
        self.assertEqual(resolved["contract_claims"], ["runtime"])
        self.assertIn("commit", resolved["starting_revision"])
        self.assertIn("workspace_fingerprint", resolved["starting_revision"])
        self.assertTrue(resolved["repository_id"])
        self.assertTrue(resolved["workspace_id"])
        self.assertEqual(resolved["validation_owner"], "root:one")
        self.assertEqual(resolved["generated_output_owner"], "none")
        self.assertEqual(resolved["advisories"], [])
        self.assertTrue(resolved["manifest_fingerprint"])
        self.assertTrue(resolved["expires_at"])

    def test_cli_repairs_empty_identities_and_keeps_them_stable(self) -> None:
        identity_root = self.repo / ".git" / "codex"
        identity_root.mkdir(parents=True)
        repository_id = identity_root / "repository-id"
        workspace_id = identity_root / "workspace-id"
        repository_id.write_text("", encoding="utf-8")
        workspace_id.write_text("", encoding="utf-8")

        first = self.run_manifest(self.manifest())
        second = self.run_manifest(self.manifest())

        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(second.returncode, 0, second.stderr)
        first_receipt = json.loads(first.stdout)
        second_receipt = json.loads(second.stdout)
        self.assertEqual(
            first_receipt["repository_id"], second_receipt["repository_id"]
        )
        self.assertEqual(first_receipt["workspace_id"], second_receipt["workspace_id"])
        self.assertEqual(
            repository_id.read_text(encoding="utf-8").strip(),
            first_receipt["repository_id"],
        )
        self.assertEqual(
            workspace_id.read_text(encoding="utf-8").strip(),
            first_receipt["workspace_id"],
        )

    def test_cli_records_expiry_and_rejects_out_of_bounds_lease(self) -> None:
        accepted = self.run_manifest(self.manifest(), "--lease-seconds", "60")
        rejected = self.run_manifest(self.manifest(), "--lease-seconds", "1")

        self.assertEqual(accepted.returncode, 0, accepted.stderr)
        receipt = json.loads(accepted.stdout)
        recorded = datetime.fromisoformat(receipt["recorded_at"])
        expires = datetime.fromisoformat(receipt["expires_at"])
        self.assertEqual((expires - recorded).total_seconds(), 60)
        self.assertEqual(rejected.returncode, 2)
        self.assertIn("lease_seconds", rejected.stderr)

    def test_cli_rejects_stale_start_revision(self) -> None:
        manifest = self.manifest()
        manifest["starting_revision"] = "0" * 40

        completed = self.run_manifest(manifest)

        self.assertEqual(completed.returncode, 2)
        self.assertIn("starting_revision is stale", completed.stderr)

    def test_cli_reports_claim_and_shared_cargo_lane_overlap(self) -> None:
        first = self.run_manifest(self.manifest("root:first"))
        contender = self.manifest("root:second")
        contender["cargo_lane"] = {
            "target_dir": "target/root-first",
            "cargo_home": ".cargo-home",
        }
        second = self.run_manifest(contender)

        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(second.returncode, 0, second.stderr)
        kinds = [item["kind"] for item in json.loads(second.stdout)["advisories"]]
        self.assertEqual(kinds, ["claim_overlap", "cargo_lane_overlap"])

    def test_cli_detects_case_only_claim_overlap_on_windows(self) -> None:
        first_manifest = self.manifest("root:first")
        first_manifest["path_claims"] = [{"path": "src/Foo", "recursive": True}]
        first_manifest["contract_claims"] = []
        first = self.run_manifest(first_manifest)

        second_manifest = self.manifest("root:second")
        second_manifest["path_claims"] = [
            {"path": "src/foo/child.rs", "recursive": False}
        ]
        second_manifest["contract_claims"] = []
        second = self.run_manifest(second_manifest)

        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(second.returncode, 0, second.stderr)
        advisories = json.loads(second.stdout)["advisories"]
        self.assertEqual(advisories[0]["kind"], "claim_overlap")
        self.assertEqual(advisories[0]["paths"], [["src/foo/child.rs", "src/Foo"]])

    @unittest.skipUnless(
        sys.platform == "win32", "requires Windows per-directory case sensitivity"
    )
    def test_cli_keeps_case_only_claims_distinct_in_case_sensitive_windows_directory(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as case_sensitive_temp:
            case_sensitive_repo = Path(case_sensitive_temp)
            enabled = subprocess.run(
                [
                    "fsutil.exe",
                    "file",
                    "SetCaseSensitiveInfo",
                    str(case_sensitive_repo),
                    "enable",
                ],
                check=False,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
            )
            self.assertEqual(
                enabled.returncode,
                0,
                f"fsutil could not enable case sensitivity: {enabled.stdout}{enabled.stderr}",
            )
            queried = subprocess.run(
                [
                    "fsutil.exe",
                    "file",
                    "QueryCaseSensitiveInfo",
                    str(case_sensitive_repo),
                ],
                check=False,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
            )
            self.assertEqual(
                queried.returncode,
                0,
                f"fsutil could not query case sensitivity: {queried.stdout}{queried.stderr}",
            )
            self.assertRegex(
                queried.stdout.casefold(),
                r"\bcase sensitive attribute\b.*\bis enabled\b",
                f"fsutil did not confirm case sensitivity: {queried.stdout}{queried.stderr}",
            )

            upper_entry = case_sensitive_repo / "CaseProbe"
            lower_entry = case_sensitive_repo / "cASEpROBE"
            upper_entry.write_text("upper\n", encoding="utf-8")
            lower_entry.write_text("lower\n", encoding="utf-8")
            self.assertTrue(upper_entry.is_file())
            self.assertTrue(lower_entry.is_file())
            self.assertFalse(os.path.samefile(upper_entry, lower_entry))

            subprocess.run(
                ["git", "init", "-q", str(case_sensitive_repo)], check=True
            )
            subprocess.run(
                [
                    "git",
                    "-C",
                    str(case_sensitive_repo),
                    "config",
                    "user.email",
                    "test@example.com",
                ],
                check=True,
            )
            subprocess.run(
                [
                    "git",
                    "-C",
                    str(case_sensitive_repo),
                    "config",
                    "user.name",
                    "Test",
                ],
                check=True,
            )
            subprocess.run(
                ["git", "-C", str(case_sensitive_repo), "add", "."], check=True
            )
            subprocess.run(
                [
                    "git",
                    "-C",
                    str(case_sensitive_repo),
                    "commit",
                    "-qm",
                    "baseline",
                ],
                check=True,
            )

            first_manifest = self.manifest("root:case-sensitive-first")
            first_manifest["repository_root"] = str(case_sensitive_repo)
            first_manifest["path_claims"] = [
                {"path": upper_entry.name, "recursive": False}
            ]
            first_manifest["contract_claims"] = []
            first_manifest["cargo_lane"] = {
                "target_dir": "target/shared",
                "cargo_home": ".cargo-home/first",
            }
            first = self.run_manifest(first_manifest)

            second_manifest = self.manifest("root:case-sensitive-second")
            second_manifest["repository_root"] = str(case_sensitive_repo)
            second_manifest["path_claims"] = [
                {"path": lower_entry.name, "recursive": False}
            ]
            second_manifest["contract_claims"] = []
            second_manifest["cargo_lane"] = {
                "target_dir": "target/shared",
                "cargo_home": ".cargo-home/second",
            }
            second = self.run_manifest(second_manifest)

            self.assertEqual(first.returncode, 0, first.stderr)
            self.assertEqual(second.returncode, 0, second.stderr)
            self.assertEqual(
                json.loads(second.stdout)["advisories"],
                [
                    {
                        "kind": "cargo_lane_overlap",
                        "assignment_id": "root:case-sensitive-first",
                        "target_dir": str(
                            (case_sensitive_repo / "target" / "shared").resolve()
                        ),
                    }
                ],
            )

    def test_cli_fingerprint_changes_with_dirty_file_content(self) -> None:
        path = self.repo / "src" / "lib.rs"
        path.write_text("pub fn value() { one(); }\n", encoding="utf-8")
        first = self.run_manifest(self.manifest())
        path.write_text("pub fn value() { two(); }\n", encoding="utf-8")
        second = self.run_manifest(self.manifest())

        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(second.returncode, 0, second.stderr)
        first_fingerprint = json.loads(first.stdout)["starting_revision"][
            "workspace_fingerprint"
        ]
        second_fingerprint = json.loads(second.stdout)["starting_revision"][
            "workspace_fingerprint"
        ]
        self.assertNotEqual(first_fingerprint, second_fingerprint)

    def test_cli_normalizes_cargo_lane_aliases_before_overlap_check(self) -> None:
        first = self.run_manifest(self.manifest("root:first"))
        contender = self.manifest("root:second")
        contender["path_claims"] = [{"path": "docs", "recursive": True}]
        contender["contract_claims"] = ["documentation"]
        contender["cargo_lane"] = {
            "target_dir": "./target/root-first",
            "cargo_home": ".cargo-home",
        }
        second = self.run_manifest(contender)

        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertEqual(
            json.loads(second.stdout)["advisories"],
            [
                {
                    "kind": "cargo_lane_overlap",
                    "assignment_id": "root:first",
                    "target_dir": str((self.repo / "target/root-first").resolve()),
                }
            ],
        )

    def test_cli_allows_isolated_worktree_overlap_with_distinct_lane(self) -> None:
        first_manifest = self.manifest("root:first")
        first_manifest["workspace_strategy"] = "shared"
        first = self.run_manifest(first_manifest)
        self.assertEqual(first.returncode, 0, first.stderr)
        first_receipt = json.loads(first.stdout)

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
                capture_output=True,
            )
            try:
                contender = self.manifest("root:second")
                contender["repository_root"] = str(isolated)
                contender["workspace_strategy"] = "isolated"
                contender["cargo_lane"] = {
                    "target_dir": str(isolated / "target" / "root-second"),
                    "cargo_home": str(isolated / ".cargo-home"),
                }
                second = self.run_manifest(contender)
                self.assertEqual(second.returncode, 0, second.stderr)
                second_receipt = json.loads(second.stdout)
                self.assertEqual(
                    second_receipt["repository_id"], first_receipt["repository_id"]
                )
                self.assertNotEqual(
                    second_receipt["workspace_id"], first_receipt["workspace_id"]
                )
                self.assertEqual(second_receipt["advisories"], [])
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
                    capture_output=True,
                )

    def test_cli_rejects_generated_output_without_claim_and_owner(self) -> None:
        manifest = self.manifest()
        manifest["generated_outputs"] = ["generated/schema.json"]

        completed = self.run_manifest(manifest)

        self.assertEqual(completed.returncode, 2)
        self.assertIn("generated_output_owner", completed.stderr)

    def test_cli_rejects_unresolved_template_placeholder(self) -> None:
        manifest = self.manifest()
        manifest["assignment_id"] = "root:<assignment>"

        completed = self.run_manifest(manifest)

        self.assertEqual(completed.returncode, 2)
        self.assertIn("template placeholder", completed.stderr)

    def test_cli_registers_overlaps_and_release_removes_receipt(self) -> None:
        first = self.run_manifest(self.manifest("root:first"))
        second = self.run_manifest(self.manifest("root:second"))
        released = subprocess.run(
            [
                sys.executable,
                str(self.script),
                "--release",
                "root:first",
                "--repository-root",
                str(self.repo),
            ],
            check=False,
            capture_output=True,
            text=True,
            encoding="utf-8",
        )
        second_after_release = self.run_manifest(self.manifest("root:second"))

        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertTrue(json.loads(second.stdout)["advisories"])
        self.assertEqual(released.returncode, 0, released.stderr)
        self.assertEqual(len(self.receipt_files()), 1)
        self.assertEqual(
            second_after_release.returncode, 0, second_after_release.stderr
        )
        self.assertEqual(json.loads(second_after_release.stdout)["advisories"], [])

    def test_cli_failed_output_write_removes_new_registry_receipt(self) -> None:
        output_directory = self.repo / "receipt-directory"
        output_directory.mkdir()

        completed = self.run_manifest(
            self.manifest("root:output-failure"),
            "--output",
            str(output_directory),
        )

        self.assertEqual(completed.returncode, 2)
        self.assertEqual(self.receipt_files(), [])

    def test_cli_failed_output_write_restores_previous_registry_receipt(self) -> None:
        manifest = self.manifest("root:output-restore")
        initial = self.run_manifest(manifest)
        self.assertEqual(initial.returncode, 0, initial.stderr)
        receipt_path = self.receipt_files()[0]
        previous = receipt_path.read_bytes()
        output_directory = self.repo / "receipt-directory"
        output_directory.mkdir()

        failed = self.run_manifest(manifest, "--output", str(output_directory))

        self.assertEqual(failed.returncode, 2)
        self.assertEqual(receipt_path.read_bytes(), previous)


if __name__ == "__main__":
    unittest.main()
