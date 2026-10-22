"""Hermetic release-cache identity checks; never invoke installation effects."""

import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("install.ps1")


class ReleaseCacheIdentityTests(unittest.TestCase):
    def test_origin_asset_and_digest_are_part_of_cache_acceptance(self):
        shell = shutil.which("powershell") or shutil.which("pwsh")
        if shell is None:
            self.skipTest("PowerShell is unavailable")
        with tempfile.TemporaryDirectory() as temp:
            # Import only the real owning functions through PowerShell's AST;
            # the installer's top-level junction/PATH/network effects never run.
            script = """
param([string]$Source, [string]$Root)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$tokens = $null; $errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($Source, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw ($errors | Out-String) }
foreach ($function in $ast.FindAll({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] }, $true)) {
    . ([scriptblock]::Create($function.Extent.Text))
}
$InstallMetadataFile = 'codex-install.env'
function Test-PackageContentsAreComplete { return $script:packageValid }
function Get-VersionFromBinary { return '1.2.3' }
$script:packageValid = $true
$ReleaseRepository = 'owner/a'
$package = [pscustomobject]@{ ReleaseId = '12'; AssetId = '34'; Sha256 = ('a' * 64) }
$checksums = [pscustomobject]@{ ReleaseId = '12'; AssetId = '35'; Sha256 = ('b' * 64) }
$a = Get-ReleaseArtifactIdentity -Repository $ReleaseRepository -PackageMetadata $package -ChecksumMetadata $checksums
$same = Get-ReleaseArtifactIdentity -Repository 'OWNER/A' -PackageMetadata $package -ChecksumMetadata $checksums
$b = Get-ReleaseArtifactIdentity -Repository 'owner/b' -PackageMetadata $package -ChecksumMetadata $checksums
$package.AssetId = '36'
$assetChanged = Get-ReleaseArtifactIdentity -Repository $ReleaseRepository -PackageMetadata $package -ChecksumMetadata $checksums
$package.AssetId = '34'; $package.Sha256 = ('c' * 64)
$digestChanged = Get-ReleaseArtifactIdentity -Repository $ReleaseRepository -PackageMetadata $package -ChecksumMetadata $checksums
$package.Sha256 = ('a' * 64)
$target = 'x86_64-pc-windows-msvc'
$release = Join-Path $Root "1.2.3-$target-$a"
[void][IO.Directory]::CreateDirectory($release)
Write-InstallMetadata -ReleaseDir $release -ResolvedVersion '1.2.3' -Target $target -Layout 'Package' -ArtifactIdentity $a -PackageMetadata $package -ChecksumMetadata $checksums
$hit = Test-ReleaseIsComplete -ReleaseDir $release -ExpectedVersion '1.2.3' -ExpectedTarget $target -Layout 'Package' -ArtifactIdentity $a -PackageMetadata $package -ChecksumMetadata $checksums
$ReleaseRepository = 'owner/b'
$wrongOrigin = Test-ReleaseIsComplete -ReleaseDir $release -ExpectedVersion '1.2.3' -ExpectedTarget $target -Layout 'Package' -ArtifactIdentity $b -PackageMetadata $package -ChecksumMetadata $checksums
$ReleaseRepository = 'owner/a'
$script:packageValid = $false
$tamper = Test-ReleaseIsComplete -ReleaseDir $release -ExpectedVersion '1.2.3' -ExpectedTarget $target -Layout 'Package' -ArtifactIdentity $a -PackageMetadata $package -ChecksumMetadata $checksums
$script:packageValid = $true
[IO.File]::WriteAllText((Join-Path $release $InstallMetadataFile), "version=1.2.3`ntarget=$target`nlayout=Package`n")
$legacy = Test-ReleaseIsComplete -ReleaseDir $release -ExpectedVersion '1.2.3' -ExpectedTarget $target -Layout 'Package' -ArtifactIdentity $a -PackageMetadata $package -ChecksumMetadata $checksums
@{ same = ($a -ceq $same); originDistinct = ($a -cne $b); assetDistinct = ($a -cne $assetChanged); digestDistinct = ($a -cne $digestChanged); hit = $hit; wrongOrigin = $wrongOrigin; tamper = $tamper; legacy = $legacy } | ConvertTo-Json -Compress
"""
            driver = Path(temp) / "cache-identity.ps1"
            driver.write_text(script, encoding="utf-8")
            result = subprocess.run(
                [
                    shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    str(driver),
                    str(SCRIPT),
                    temp,
                ],
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(
                json.loads(result.stdout),
                {
                    "same": True,
                    "originDistinct": True,
                    "assetDistinct": True,
                    "digestDistinct": True,
                    "hit": True,
                    "wrongOrigin": False,
                    "tamper": False,
                    "legacy": False,
                },
            )


if __name__ == "__main__":
    unittest.main()
