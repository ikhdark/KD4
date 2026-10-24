"""Import-only tests of local publisher boundaries and profile handoffs.

The stdlib harness confines files to a temporary directory. It never invokes
the publisher entrypoint, activation, builds, Desktop routing, or installed Codex processes.
"""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest
from unittest import mock


SCRIPT = Path(__file__).with_name("publish-local-codex.ps1").resolve()
SHELL = shutil.which("pwsh") or shutil.which("powershell")


class ReleaseProfileConfigurationTest(unittest.TestCase):
    def test_release_is_the_final_profile_owner(self):
        manifest = SCRIPT.parents[2] / "codex-rs" / "Cargo.toml"
        profiles = tomllib.loads(manifest.read_text(encoding="utf-8"))["profile"]
        release = profiles["release"]
        for name, expected in {
            "opt-level": 3, "lto": "fat", "codegen-units": 1, "incremental": False,
        }.items():
            with self.subTest(setting=name):
                self.assertEqual(release[name], expected)
        self.assertEqual(release["package"]["*"]["codegen-units"], 1)
        self.assertEqual(profiles["local-release"], {"inherits": "release"})
        self.assertEqual(profiles["local-pgo"]["inherits"], "local-release")
        self.assertFalse(profiles["local-pgo"]["incremental"])
        # These descendants deliberately keep their pre-existing build behavior.
        self.assertEqual(profiles["local-fast"]["codegen-units"], 16)
        self.assertEqual(profiles["local-fast"]["package"]["*"]["codegen-units"], 4)
        self.assertEqual(profiles["profiling"]["codegen-units"], 4)
        self.assertEqual(profiles["profiling"]["package"]["*"]["codegen-units"], 4)
        self.assertFalse(profiles["profiling"]["incremental"])


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

    def test_publish_commands_keep_selected_profile(self):
        cases = [
            ("local-fast", "just publish-local-codex-fast"),
            ("local-release", "just publish-local-codex-final"),
        ]
        for profile in ("debug",):
            cases.append((
                profile,
                f'powershell -NoProfile -ExecutionPolicy Bypass -File "{SCRIPT}" '
                f'-Concise -AutoSkipBuild -Profile {profile} '
                '-ConfigureDesktopLocalCli -RestartDesktopIfNeeded',
            ))
        with tempfile.TemporaryDirectory() as directory:
            for profile, expected in cases:
                with self.subTest(profile=profile):
                    self.invoke_owner(
                        directory,
                        """
                        $command = Get-LocalPublishCommand -Profile $env:KD4_BOUNDARY_PROFILE
                        if ($command -cne $env:KD4_BOUNDARY_EXPECTED) {
                            throw "Publish command changed the selected profile: $command"
                        }
                        """,
                        PROFILE=profile, EXPECTED=expected,
                    )

    def test_final_keeps_wrapper_policy(self):
        with tempfile.TemporaryDirectory() as directory:
            self.invoke_owner(
                directory,
                """
                function Assert-RustyV8ArchiveReadyForPublish { }
                $proof = @(Invoke-CodexBuild -RepoRoot $PWD -Profile local-release -DryRun)
                $command = @($proof | Where-Object { $_ -like 'buildCommand:*' })
                $config = @(Get-LocalPublishCargoConfigLines -Profile local-release -DisableSccache $false -Pgo $null)
                if ($command.Count -ne 1 -or $command[0] -notlike '*--config*') {
                    throw "Final build is missing its linker configuration: $proof"
                }
                if (@($config | Where-Object { $_ -match 'rustc(-workspace)?-wrapper' }).Count -ne 0) {
                    throw "Final unexpectedly bypassed wrappers: $proof"
                }
                if (@($proof | Where-Object { $_ -like '*local-final:*fat LTO*codegen-units=1*' }).Count -ne 1) {
                    throw "Final proof does not describe final-publish settings: $proof"
                }
                """,
            )

    def test_pgo_rejects_unbound_or_modified_profile_data(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            data = b"profile fixture"
            digest = hashlib.sha256(data).hexdigest()
            profile = root / f"profile-{digest}.profdata"
            profile.write_bytes(data)
            self.invoke_owner(
                root,
                """
                function Get-LocalPublishBuildTarget { return 'x86_64-pc-windows-msvc' }
                $config = Get-LocalPublishPgoConfiguration -RepoRoot $PWD -Profile local-pgo `
                    -ProfileData $env:KD4_BOUNDARY_PROFILE
                if ($config.sha256 -cne $env:KD4_BOUNDARY_DIGEST -or $config.mode -cne 'use') {
                    throw 'PGO profile contents were not bound to the build configuration'
                }
                [IO.File]::WriteAllText($env:KD4_BOUNDARY_PROFILE, 'changed profile')
                try {
                    $null = Get-LocalPublishPgoConfiguration -RepoRoot $PWD -Profile local-pgo `
                        -ProfileData $env:KD4_BOUNDARY_PROFILE
                    throw 'Owner accepted modified profile contents at the same compiler path'
                } catch {
                    if ($_.Exception.Message -notlike '*content-addressed*') { throw }
                }
                try {
                    $null = Get-LocalPublishPgoConfiguration -RepoRoot $PWD -Profile local-pgo
                    throw 'Owner accepted an optimized build without training data'
                } catch {
                    if ($_.Exception.Message -notlike '*require only -PgoProfileData*') { throw }
                }
                try {
                    $null = Get-LocalPublishPgoConfiguration -RepoRoot $PWD -Profile local-fast `
                        -GenerateDirectory $PWD
                    throw 'Owner accepted instrumentation on a normal profile'
                } catch {
                    if ($_.Exception.Message -notlike '*require a local-pgo profile*') { throw }
                }
                """,
                PROFILE=profile, DIGEST=digest,
            )

    def test_pgo_artifacts_use_explicit_target_subdirectory(self):
        with tempfile.TemporaryDirectory() as directory:
            self.invoke_owner(
                directory,
                """
                function Get-LocalPublishBuildTarget { return 'x86_64-pc-windows-msvc' }
                foreach ($profile in @('local-pgo', 'local-pgo-instrumented')) {
                    if ((Get-LocalPublishCargoProfile $profile) -cne 'local-pgo') {
                        throw 'PGO passes must use the same Cargo profile'
                    }
                    foreach ($getter in @('Get-BuiltCodexPath', 'Get-BuiltCodeModeHostPath',
                        'Get-BuiltWindowsSandboxSetupPath', 'Get-BuiltCommandRunnerPath')) {
                        $path = & $getter -RepoRoot $PWD -Profile $profile
                        $parent = Join-Path $PWD "codex-rs/target/publish-$profile/x86_64-pc-windows-msvc/local-pgo"
                        if ((Split-Path -Parent $path) -cne $parent) {
                            throw "PGO artifact did not use Cargo's explicit-target layout: $path"
                        }
                    }
                }
                """,
            )

    def test_pgo_owner_import_does_not_overwrite_wrapper_parameters(self):
        with tempfile.TemporaryDirectory() as directory:
            self.invoke_owner(
                directory,
                """
                . $env:KD4_BOUNDARY_WRAPPER -ImportOnly -Stage Train -WorkDir $env:KD4_BOUNDARY_ROOT
                $homeBefore = $LocalCodexHome
                $null = Invoke-PgoOwner -Action { Get-LocalPublishCommand -Profile local-fast }
                if ($Stage -cne 'Train' -or $WorkDir -cne $env:KD4_BOUNDARY_ROOT -or
                    $LocalCodexHome -cne $homeBefore) {
                    throw 'Publisher import overwrote wrapper parameters'
                }
                try {
                    $null = Get-PgoRawSnapshot $env:KD4_BOUNDARY_ROOT
                    throw 'PGO accepted a training run with no profiles'
                } catch {
                    if ($_.Exception.Message -notlike '*no .profraw files*') { throw }
                }
                """,
                WRAPPER=SCRIPT.with_name("publish-local-codex-pgo.ps1"), ROOT=directory,
            )

    def test_runtime_proof_parses_doctor_json_and_fails_closed(self):
        def report(readiness="ok", runtime_chain="ok"):
            checks = {
                "local_publish.readiness": {"status": readiness},
                "desktop.runtime_chain": {"status": runtime_chain},
            }
            if runtime_chain is None:
                del checks["desktop.runtime_chain"]
            return json.dumps({"checks": checks}, indent=2)

        cases = (
            ("healthy", report(), 0, True, ""),
            ("missing checks", "{}", 0, False, "missing checks"),
            ("missing runtime chain", report(runtime_chain=None), 0, False,
             "missing desktop.runtime_chain"),
            ("readiness warning", report(readiness="warning"), 0, False,
             "rejected local_publish.readiness"),
            ("runtime failure", report(runtime_chain="fail"), 0, False,
             "rejected desktop.runtime_chain"),
            ("invalid JSON", "not JSON", 0, False, ""),
            ("empty report", "", 0, False, "no JSON report"),
            ("nonzero exit", report(), 1, False, "exited with 1"),
        )
        for name, stdout, exit_code, expected, reason in cases:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                # The owner invokes `<target> doctor --json --summary --runtime-only`.
                # Python executes this temporary `doctor` fixture instead of Codex.
                (root / "doctor").write_text(
                    "import sys\n"
                    "assert sys.argv[1:] == ['--json', '--summary', '--runtime-only']\n"
                    f"sys.stdout.write({stdout!r})\n"
                    "sys.stderr.write('fixture diagnostic on stderr\\n')\n"
                    f"sys.exit({exit_code})\n",
                    encoding="utf-8",
                )
                self.invoke_owner(
                    root,
                    """
                    $failure = 'stale failure'
                    $proven = Test-DesktopRuntimeProof `
                        -TargetPath $env:KD4_BOUNDARY_TARGET `
                        -LocalCodexHome $env:KD4_BOUNDARY_HOME `
                        -FailureReason ([ref]$failure)
                    $expected = $env:KD4_BOUNDARY_EXPECTED -eq 'True'
                    if ($proven -ne $expected) {
                        throw "Unexpected runtime proof result ${proven}: $failure"
                    }
                    if ($expected -and $null -ne $failure) {
                        throw "Successful runtime proof retained a failure: $failure"
                    }
                    if (-not $expected -and [string]::IsNullOrWhiteSpace($failure)) {
                        throw 'Rejected runtime proof did not report its failure'
                    }
                    if ($env:KD4_BOUNDARY_REASON -and
                        $failure -notlike "*$($env:KD4_BOUNDARY_REASON)*") {
                        throw "Unexpected runtime proof failure: $failure"
                    }
                    """,
                    TARGET=sys.executable, HOME=root, EXPECTED=expected, REASON=reason,
                )

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
