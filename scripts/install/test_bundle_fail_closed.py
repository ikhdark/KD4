"""Import-only tests of the real local publisher's staging/recovery boundaries.

The stdlib harness confines files to a temporary directory. It never invokes
the publisher entrypoint, activation, builds, Desktop routing, or process stops.
"""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).with_name("publish-local-codex.ps1").resolve()
SHELL = shutil.which("pwsh") or shutil.which("powershell")


@unittest.skipUnless(SHELL, "PowerShell is required for import-only owner calls")
class LocalBundleFailClosedTest(unittest.TestCase):
    def invoke_owner(self, root, command, **values):
        environment = {"KD4_BOUNDARY_SCRIPT": str(SCRIPT)}
        environment.update({f"KD4_BOUNDARY_{name}": str(value) for name, value in values.items()})
        with mock.patch.dict(os.environ, environment):
            result = subprocess.run(
                [SHELL, "-NoProfile", "-NonInteractive", "-Command",
                 ". $env:KD4_BOUNDARY_SCRIPT -ImportOnly; " + command],
                cwd=root,
                env=os.environ.copy(),
                capture_output=True,
                text=True,
                check=False,
                timeout=30,
            )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_staged_digest_mismatch_cannot_change_active_installation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            install = root / "install"
            install.mkdir()
            target = install / "codex.exe"
            target.write_bytes(b"active installation")
            source = root / "candidate.exe"
            source.write_bytes(b"different bytes after verification")
            journal = root / ".install.codex-local-publish.transaction.json"
            backup = root / "backups" / "codex.exe"
            self.invoke_owner(
                root,
                """
                $entry = [pscustomobject]@{
                    Name = 'codex'; SourcePath = $env:KD4_BOUNDARY_SOURCE;
                    ExpectedSha256 = $env:KD4_BOUNDARY_EXPECTED;
                    TargetPath = $env:KD4_BOUNDARY_TARGET;
                    BackupPath = $env:KD4_BOUNDARY_BACKUP;
                    HadPreviousTarget = $true; Changed = $true
                }
                try {
                    $null = New-CodexRuntimeBundleTransaction `
                        -JournalPath $env:KD4_BOUNDARY_JOURNAL `
                        -InstallDir $env:KD4_BOUNDARY_INSTALL -Entries @($entry)
                    throw 'Owner accepted a staged digest mismatch'
                } catch {
                    if ($_.Exception.Message -notlike '*verified SHA-256 mismatch*') { throw }
                }
                """,
                SOURCE=source, EXPECTED=hashlib.sha256(b"verified original bytes").hexdigest(),
                TARGET=target, BACKUP=backup, JOURNAL=journal, INSTALL=install,
            )
            self.assertEqual(target.read_bytes(), b"active installation")
            self.assertEqual(set(install.iterdir()), {target})
            self.assertEqual(set(root.iterdir()), {install, source})

    def test_committed_recovery_rejects_unowned_cleanup_path(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            install = root / "install"
            unrelated = root / "unrelated"
            for path in (install, unrelated):
                path.mkdir()
                (path / "keep.txt").write_bytes(path.name.encode())
            journal = root / ".install.codex-local-publish.transaction.json"
            journal.write_text(json.dumps({
                "SchemaVersion": 1, "TransactionId": "fixture", "Phase": "Committed",
                "JournalPath": str(journal),
                "InstallDir": str(install), "StageRoot": str(unrelated),
                "RollbackRoot": str(root / ".install.rollback.fixture"),
                "HadPreviousInstall": True, "Entries": [],
            }), encoding="utf-8")
            original_journal = journal.read_bytes()
            self.invoke_owner(
                root,
                """
                try {
                    $null = Recover-CodexRuntimeBundleTransaction -JournalPath $env:KD4_BOUNDARY_JOURNAL
                    throw 'Owner accepted an unowned cleanup path'
                } catch {
                    if ($_.Exception.Message -notlike '*unowned StageRoot*') { throw }
                }
                """,
                JOURNAL=journal,
            )
            self.assertEqual(journal.read_bytes(), original_journal)
            for path in (install, unrelated):
                self.assertEqual((path / "keep.txt").read_bytes(), path.name.encode())
                self.assertEqual(set(path.iterdir()), {path / "keep.txt"})
            self.assertEqual(set(root.iterdir()), {install, unrelated, journal})


if __name__ == "__main__":
    unittest.main()
