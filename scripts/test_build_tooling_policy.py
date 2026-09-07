#!/usr/bin/env python3

import contextlib
import hashlib
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import rust_packages
from scripts.build_tooling_test_support import REPO_ROOT
from scripts.build_tooling_test_support import load_format_module
from scripts.build_tooling_test_support import load_root_maintenance_module
from scripts.build_tooling_test_support import load_toml
from scripts.build_tooling_test_support import powershell
from scripts.build_tooling_test_support import ps_single_quote


def repository_owned_paths() -> list[Path]:
    result = subprocess.run(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
        cwd=REPO_ROOT,
        capture_output=True,
        check=True,
    )
    return [
        REPO_ROOT / relative_path
        for relative_path in result.stdout.decode("utf-8").split("\0")
        if relative_path and (REPO_ROOT / relative_path).is_file()
    ]


class BuildToolingPolicyTest(unittest.TestCase):
    def run_workspace_analyzer(
        self,
        analyzer: str,
        *forwarded_args: str,
        rustflags: str = "",
        os_name: str | None = None,
    ) -> tuple[subprocess.CompletedProcess[str], list[dict[str, object]]]:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is required for workspace analyzer tests")
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_root = Path(temp_dir)
            analyzer_path = temp_root / "cargo-workspace-analyzer.ps1"
            shutil.copyfile(
                REPO_ROOT / "scripts" / "cargo-workspace-analyzer.ps1",
                analyzer_path,
            )
            (temp_root / "cargo-lane.ps1").write_text(
                "param(\n"
                "    [string]$Lane,\n"
                "    [Parameter(ValueFromRemainingArguments = $true)]\n"
                "    [string[]]$Command\n"
                ")\n"
                "[ordered]@{ lane = $Lane; args = @($Command); rustflags = $env:RUSTFLAGS } "
                "| ConvertTo-Json -Compress | Add-Content -LiteralPath "
                "$env:CODEX_ANALYZER_TEST_OUTPUT\n",
                encoding="utf-8",
            )
            output_path = temp_root / "calls.jsonl"
            env = {
                **os.environ,
                "RUSTFLAGS": rustflags,
                "CODEX_ANALYZER_TEST_OUTPUT": str(output_path),
            }
            if os_name is not None:
                env["OS"] = os_name
            result = subprocess.run(
                [
                    shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    str(analyzer_path),
                    "-Analyzer",
                    analyzer,
                    *forwarded_args,
                ],
                cwd=REPO_ROOT,
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            output = (
                output_path.read_text(encoding="utf-8") if output_path.exists() else ""
            )
        payloads = [json.loads(line) for line in output.splitlines() if line.strip()]
        return result, payloads

    def test_build_metadata_is_owned_by_the_compiling_utility_crate(self) -> None:
        rust_root = REPO_ROOT / "codex-rs"
        for retired_path in (
            rust_root / "build_info.rs",
            rust_root / "app-server" / "build.rs",
            rust_root / "cli" / "build.rs",
            rust_root / "rollout" / "build.rs",
        ):
            self.assertFalse(
                retired_path.exists(), f"retired build input: {retired_path}"
            )

        cli_manifest = load_toml(rust_root / "cli" / "Cargo.toml")
        self.assertNotIn("build", cli_manifest["package"])

        build_info = (rust_root / "utils" / "build-info" / "src" / "lib.rs").read_text(
            encoding="utf-8"
        )
        publisher = (REPO_ROOT / "scripts" / "publish-local-codex.ps1").read_text(
            encoding="utf-8"
        )
        for variable in (
            "CODEX_BUILD_COMMIT",
            "CODEX_BUILD_DIRTY",
            "CODEX_BUILD_PROFILE",
            "CODEX_BUILD_TIMESTAMP",
        ):
            self.assertIn(f'option_env!("{variable}")', build_info)
            self.assertIn(
                f'Set-ProcessEnvironmentVariable -Name "{variable}"', publisher
            )

    def test_confirmed_dead_rust_inputs_do_not_return(self) -> None:
        rust_root = REPO_ROOT / "codex-rs"
        rmcp = load_toml(rust_root / "rmcp-client" / "Cargo.toml")
        state = load_toml(rust_root / "state" / "Cargo.toml")
        tui = load_toml(rust_root / "tui" / "Cargo.toml")

        self.assertNotIn("codex-utils-home-dir", rmcp["dependencies"])
        self.assertNotIn("hmac", state["dependencies"])
        self.assertNotIn("rand", state["dependencies"])
        self.assertNotIn("core_test_support", tui["dev-dependencies"])

        diff_render = (rust_root / "tui" / "src" / "diff_render.rs").read_text(
            encoding="utf-8"
        )
        responses_stream = (
            rust_root / "codex-api" / "src" / "responses_stream.rs"
        ).read_text(encoding="utf-8")
        self.assertNotRegex(
            diff_render,
            r"#\[allow\(dead_code\)\]\s*path: PathBuf",
        )
        self.assertNotRegex(
            responses_stream,
            r"#\[allow\(dead_code\)\]\s*struct ResponseCompleted",
        )
        self.assertRegex(
            responses_stream,
            r"#\[allow\(dead_code\)\]\s*struct Error",
        )

    def test_skills_build_script_requires_bundled_samples(self) -> None:
        text = (REPO_ROOT / "codex-rs" / "skills" / "build.rs").read_text(
            encoding="utf-8"
        )

        self.assertIn('let samples_dir = Path::new("src/assets/samples");', text)
        self.assertIn("if !samples_dir.exists()", text)

    def test_retired_repo_local_harness_skill_has_no_registration(self) -> None:
        features = load_toml(REPO_ROOT / "kd4_features.toml")["features"]
        feature_ids = {feature["id"] for feature in features}
        root_policy = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")
        harness_workflow = (REPO_ROOT / ".codex" / "harness" / "workflow.md").read_text(
            encoding="utf-8"
        )

        self.assertNotIn("kd4-harness", feature_ids)
        self.assertFalse((REPO_ROOT / ".codex" / "skills" / "kd4-harness").exists())
        self.assertNotIn("skills/kd4-harness", root_policy)
        self.assertNotIn("kd4-harness", harness_workflow)

    def test_harness_workflow_orchestrator_reference_resolves(self) -> None:
        harness_dir = REPO_ROOT / ".codex" / "harness"
        workflow = (harness_dir / "workflow.md").read_text(encoding="utf-8")
        relative_path = "templates/ORCHESTRATOR.md"

        self.assertIn(
            f"[`{relative_path}`]({relative_path})",
            workflow,
        )
        self.assertTrue((harness_dir / relative_path).is_file())

    def test_harness_local_markdown_links_resolve(self) -> None:
        harness_dir = REPO_ROOT / ".codex" / "harness"

        durable_markdown = (
            path
            for path in harness_dir.rglob("*.md")
            if "runs" not in path.relative_to(harness_dir).parts
        )
        for markdown_path in sorted(durable_markdown):
            markdown = markdown_path.read_text(encoding="utf-8")
            for target in re.findall(r"\[[^]]*\]\(([^)]+)\)", markdown):
                relative_path = target.split("#", 1)[0]
                if not relative_path or "://" in relative_path:
                    continue
                with self.subTest(source=markdown_path, target=target):
                    self.assertTrue(
                        (markdown_path.parent / relative_path).is_file(),
                        f"broken local Markdown link in {markdown_path}: {target}",
                    )

    def test_repo_local_skill_frontmatter_names_match_folders(self) -> None:
        skills_dir = REPO_ROOT / ".codex" / "skills"
        if not skills_dir.exists():
            self.skipTest("repo-local skills directory is not materialized")
        skill_dirs = [path for path in skills_dir.iterdir() if path.is_dir()]
        self.assertTrue(skill_dirs, "skills directory exists but contains no skills")
        frontmatter_names: list[str] = []
        for skill_dir in skill_dirs:
            skill_path = skill_dir / "SKILL.md"
            # A skill directory without SKILL.md is a broken skill, not an
            # ignorable one.
            self.assertTrue(
                skill_path.exists(),
                f"skill '{skill_dir.name}' is missing SKILL.md",
            )
            skill = skill_path.read_text(encoding="utf-8")

            name_lines = [
                line for line in skill.splitlines() if line.startswith("name: ")
            ]
            self.assertEqual(len(name_lines), 1, f"invalid skill name in {skill_path}")
            frontmatter_names.append(name_lines[0].removeprefix("name: ").strip())
        self.assertEqual(len(frontmatter_names), len(set(frontmatter_names)))

    def test_agents_skill_inventory_matches_local_build_tree(self) -> None:
        agents = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")
        normalized = " ".join(agents.split())
        skills_dir = REPO_ROOT / ".codex" / "skills"
        if not skills_dir.exists():
            # Skip visibly instead of silently dropping the central
            # inventory assertion.
            self.skipTest("repo-local skills directory is not materialized")
        skill_names = sorted(
            path.name for path in skills_dir.iterdir() if path.is_dir()
        )
        self.assertNotIn("kd4-harness", skill_names)
        self.assertIn("`.codex/skills`", agents)
        for phrase in ("fork-local skills", "validation workflows"):
            self.assertIn(phrase, normalized)

    def test_agents_mentions_current_checkout_not_stale_codexkd_path(self) -> None:
        text = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")
        first_lines = " ".join(" ".join(text.splitlines()[:40]).split())

        self.assertIn(
            "Treat the active repository root as the checkout location", first_lines
        )
        self.assertNotIn(r"C:\Users\kuh\Desktop\kd4", text)
        self.assertNotIn(r"C:\Users\kuh\Desktop\codexKD`", text)

    def test_agents_bootstraps_bounded_routing_before_broad_source_map(self) -> None:
        text = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")
        normalized = " ".join(text.split())
        slice_command = (
            "python scripts/source_owners.py slice --owner <owner-id> "
            '--focus "<task description>" --max-relationships 32'
        )

        self.assertIn(slice_command, normalized)
        self.assertLess(
            normalized.index(slice_command),
            normalized.index("[`SOURCEMAP.md`](SOURCEMAP.md)"),
            "the bounded owner query must be available before the broad map route",
        )
        self.assertIn("Require an untruncated result", normalized)
        self.assertIn("no omitted relationships or material unknowns", normalized)

    def test_agents_desktop_boundary_is_top_level_guidance(self) -> None:
        text = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")

        # Assert the section exists as top-level (H2) guidance and carries the
        # rebuild contract, without pinning it to a line window that breaks
        # whenever earlier sections grow.
        self.assertIn("\n## Repository identity and runtime boundary\n", text)
        section = text.split("\n## Repository identity and runtime boundary\n", 1)[1]
        section = section.split("\n## ", 1)[0]
        self.assertIn(
            "Source changes become Desktop-visible only after rebuilding", section
        )
        self.assertIn("replacing or updating the local binary", section)

    def test_agents_validation_map_matches_current_layout(self) -> None:
        text = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")
        normalized = " ".join(text.split())

        self.assertIn("## Shared operating policy", text)
        self.assertIn("### Implementation and validation", text)
        self.assertIn("## Rust and script validation", text)
        self.assertIn(
            "Do not publish, deploy, or modify upstream state unless the user "
            "explicitly requests that action",
            normalized,
        )
        self.assertIn("do not hand-edit generated output", normalized)

    def test_agents_scripts_policy_is_root_owned(self) -> None:
        root_text = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")
        normalized = " ".join(root_text.split())

        self.assertFalse((REPO_ROOT / "scripts" / "AGENTS.md").exists())
        self.assertIn(
            "For a script edit, follow the validation route named in `SOURCEMAP.md`",
            normalized,
        )

    def test_windows_installer_requires_standalone_metadata(self) -> None:
        powershell_installer = (
            REPO_ROOT / "scripts" / "install" / "install.ps1"
        ).read_text(encoding="utf-8")

        self.assertIn(
            '$InstallMetadataFile = "codex-install.env"', powershell_installer
        )
        self.assertIn("function Write-InstallMetadata", powershell_installer)
        self.assertIn("function Get-InstallMetadataField", powershell_installer)
        self.assertIn(
            "Write-InstallMetadata -ReleaseDir $stagingDir", powershell_installer
        )
        self.assertIn(
            'Get-InstallMetadataField -ReleaseDir $ReleaseDir -Name "version"',
            powershell_installer,
        )
        self.assertIn('"bin\\codex-code-mode-host.exe"', powershell_installer)

    def test_windows_installer_cleans_failed_metadata_temporary_file(self) -> None:
        ps = powershell()
        if ps is None:
            self.skipTest("PowerShell is required for the Windows installer test")
        installer = REPO_ROOT / "scripts" / "install" / "install.ps1"
        with tempfile.TemporaryDirectory() as temp_dir:
            command = (
                "$tokens=$null; $errors=$null; "
                f"$ast=[Management.Automation.Language.Parser]::ParseFile({ps_single_quote(installer)},[ref]$tokens,[ref]$errors); "
                "$fn=$ast.FindAll({param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq 'Write-InstallMetadata'},$true)[0]; "
                "Invoke-Expression $fn.Extent.Text; $InstallMetadataFile='codex-install.env'; "
                "function Move-Item { throw 'injected move failure' }; "
                f"try {{ Write-InstallMetadata -ReleaseDir {ps_single_quote(Path(temp_dir))} -ResolvedVersion '1' -Target 't' -Layout 'Package' }} catch {{ }}; "
                f"if (@(Get-ChildItem -LiteralPath {ps_single_quote(Path(temp_dir))} -Filter 'codex-install.env.*').Count -ne 0) {{ exit 9 }}"
            )
            completed = subprocess.run(
                [ps, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", command],
                text=True,
                capture_output=True,
                check=False,
            )
        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_windows_installer_detects_unknown_external_codex_conflict(self) -> None:
        ps = powershell()
        if ps is None:
            self.skipTest("PowerShell is required for the Windows installer test")
        installer = REPO_ROOT / "scripts" / "install" / "install.ps1"
        command = (
            "$tokens=$null; $errors=$null; "
            f"$ast=[Management.Automation.Language.Parser]::ParseFile({ps_single_quote(installer)},[ref]$tokens,[ref]$errors); "
            "$names=@('Get-ExistingCodexManager','Get-ConflictingInstall'); "
            "$ast.FindAll({param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $names -contains $n.Name},$true) | ForEach-Object { Invoke-Expression $_.Extent.Text }; "
            "function Test-PathIsEqualOrDescendant { return $false }; function Get-ExistingCodexCommand { 'C:\\Tools\\codex.exe' }; function Write-Step {}; function Write-WarningStep {}; "
            "$conflict=Get-ConflictingInstall -VisibleBinDir 'C:\\KD4\\bin'; "
            "if ($null -eq $conflict -or $null -ne $conflict.Manager -or $conflict.Path -cne 'C:\\Tools\\codex.exe') { exit 9 }"
        )
        completed = subprocess.run(
            [ps, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", command],
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_windows_installer_defaults_to_fork_release_artifacts(self) -> None:
        powershell_installer = (
            REPO_ROOT / "scripts" / "install" / "install.ps1"
        ).read_text(encoding="utf-8")

        self.assertIn(
            "[string]$ReleaseRepository = $env:CODEX_RELEASE_REPOSITORY",
            powershell_installer,
        )
        self.assertIn('$ReleaseRepository = "ikhdark/KD4"', powershell_installer)
        self.assertIn(
            '$ReleaseApiBase = "https://api.github.com/repos/$ReleaseRepository/releases"',
            powershell_installer,
        )
        self.assertNotIn("api.github.com/repos/openai/codex", powershell_installer)

        ps = powershell()
        if ps is None:
            self.skipTest("PowerShell is required for the Windows installer test")
        installer_path = REPO_ROOT / "scripts" / "install" / "install.ps1"
        command = (
            "$tokens = $null; $errors = $null; "
            f"$ast = [System.Management.Automation.Language.Parser]::ParseFile("
            f"{ps_single_quote(installer_path)}, [ref]$tokens, [ref]$errors); "
            "$function = $ast.FindAll({ param($node) "
            "$node -is [System.Management.Automation.Language.FunctionDefinitionAst] "
            "-and $node.Name -eq 'Get-ReleaseApiUri' }, $true); "
            "Invoke-Expression $function[0].Extent.Text; "
            "$ReleaseApiBase = 'https://api.github.com/repos/ikhdark/KD4/releases'; "
            "$actual = Get-ReleaseApiUri -RelativePath 'latest'; "
            "if ($actual -cne 'https://api.github.com/repos/ikhdark/KD4/releases/latest') "
            "{ exit 9 }"
        )
        completed = subprocess.run(
            [ps, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", command],
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(
            completed.returncode,
            0,
            f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
        )

    def test_powershell_installer_completeness_rejects_package_without_code_mode_host(
        self,
    ) -> None:
        ps = powershell()
        if ps is None:
            self.skipTest("PowerShell is required for the Windows installer test")

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            windows_package = root / "windows-package"
            for relative in (
                "bin/codex.exe",
                "codex-path/apply_patch.bat",
                "codex-path/applypatch.bat",
                "codex-path/rg.exe",
                "codex-resources/codex-command-runner.exe",
                "codex-resources/codex-windows-sandbox-setup.exe",
                "LICENSE",
                "NOTICE",
            ):
                path = windows_package / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.touch()
            package_files = [
                {
                    "path": path.relative_to(windows_package).as_posix(),
                    "role": "test",
                    "size": path.stat().st_size,
                    "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
                }
                for path in sorted(windows_package.rglob("*"))
                if path.is_file() and path.name != "codex-package.json"
            ]
            (windows_package / "codex-package.json").write_text(
                json.dumps(
                    {
                        "layoutVersion": 2,
                        "version": "1.2.3",
                        "target": "x86_64-pc-windows-msvc",
                        "variant": "codex",
                        "entrypoint": "bin/codex.exe",
                        "resourcesDir": "codex-resources",
                        "pathDir": "codex-path",
                        "bundleId": hashlib.sha256(
                            json.dumps(
                                package_files,
                                sort_keys=True,
                                separators=(",", ":"),
                            ).encode()
                        ).hexdigest(),
                        "buildIdentity": {"status": "test-fixture"},
                        "files": package_files,
                    }
                ),
                encoding="utf-8",
            )

            powershell_installer = REPO_ROOT / "scripts" / "install" / "install.ps1"

            def run_powershell_probe(
                expected: bool,
            ) -> subprocess.CompletedProcess[str]:
                command = (
                    "$tokens = $null; $errors = $null; "
                    f"$ast = [System.Management.Automation.Language.Parser]::ParseFile("
                    f"{ps_single_quote(powershell_installer)}, "
                    "[ref]$tokens, [ref]$errors); "
                    "$names = @('Get-FileSha256', 'Get-PeMachine', "
                    "'Test-PackageContentsAreComplete'); "
                    "$functions = $ast.FindAll({ param($node) "
                    "$node -is [System.Management.Automation.Language.FunctionDefinitionAst] "
                    "-and $names -contains $node.Name }, $true); "
                    "$functions | ForEach-Object { Invoke-Expression $_.Extent.Text }; "
                    f"$actual = Test-PackageContentsAreComplete -PackageDir "
                    f"{ps_single_quote(windows_package)} -ExpectedVersion '1.2.3' "
                    "-ExpectedTarget 'x86_64-pc-windows-msvc'; "
                    f"if ($actual -ne ${str(expected).lower()}) {{ exit 9 }}"
                )
                return subprocess.run(
                    [
                        ps,
                        "-NoProfile",
                        "-ExecutionPolicy",
                        "Bypass",
                        "-Command",
                        command,
                    ],
                    text=True,
                    capture_output=True,
                    check=False,
                )

            missing_windows = run_powershell_probe(False)
            self.assertEqual(
                missing_windows.returncode,
                0,
                f"stdout:\n{missing_windows.stdout}\nstderr:\n{missing_windows.stderr}",
            )
            (windows_package / "bin" / "codex-code-mode-host.exe").touch()
            metadata_path = windows_package / "codex-package.json"
            metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
            code_mode_host = windows_package / "bin" / "codex-code-mode-host.exe"
            metadata["files"].append(
                {
                    "path": "bin/codex-code-mode-host.exe",
                    "role": "code-mode-host",
                    "size": code_mode_host.stat().st_size,
                    "sha256": hashlib.sha256(code_mode_host.read_bytes()).hexdigest(),
                }
            )
            metadata["bundleId"] = hashlib.sha256(
                json.dumps(
                    metadata["files"], sort_keys=True, separators=(",", ":")
                ).encode()
            ).hexdigest()
            metadata_path.write_text(json.dumps(metadata), encoding="utf-8")
            complete_windows = run_powershell_probe(True)
            self.assertEqual(
                complete_windows.returncode,
                0,
                f"stdout:\n{complete_windows.stdout}\nstderr:\n{complete_windows.stderr}",
            )

            metadata["bundleId"] = "f" * 64
            metadata_path.write_text(json.dumps(metadata), encoding="utf-8")
            mismatched_bundle = run_powershell_probe(False)
            self.assertEqual(
                mismatched_bundle.returncode,
                0,
                f"stdout:\n{mismatched_bundle.stdout}\nstderr:\n{mismatched_bundle.stderr}",
            )

            metadata["bundleId"] = hashlib.sha256(
                json.dumps(
                    metadata["files"], sort_keys=True, separators=(",", ":")
                ).encode()
            ).hexdigest()
            metadata_path.write_text(json.dumps(metadata), encoding="utf-8")
            (windows_package / "unexpected.dll").touch()
            extra_file = run_powershell_probe(False)
            self.assertEqual(
                extra_file.returncode,
                0,
                f"stdout:\n{extra_file.stdout}\nstderr:\n{extra_file.stderr}",
            )

    def test_windows_installer_parses_the_first_nonempty_version_line(self) -> None:
        powershell_installer = (
            REPO_ROOT / "scripts" / "install" / "install.ps1"
        ).read_text(encoding="utf-8")

        self.assertIn("$versionLine = @($versionOutput)", powershell_installer)
        self.assertIn("[regex]::Match($versionLine", powershell_installer)
        self.assertNotIn("$versionOutput -match", powershell_installer)

    def test_windows_installer_uninstall_removes_only_its_path_entry(self) -> None:
        ps = powershell()
        if ps is None:
            self.skipTest("PowerShell is required for the Windows installer test")
        installer = REPO_ROOT / "scripts" / "install" / "install.ps1"
        command = (
            "$tokens=$null; $errors=$null; "
            f"$ast=[Management.Automation.Language.Parser]::ParseFile({ps_single_quote(installer)},[ref]$tokens,[ref]$errors); "
            "$fn=$ast.FindAll({param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq 'Remove-PathEntry'},$true)[0]; "
            "Invoke-Expression $fn.Extent.Text; "
            "$actual=Remove-PathEntry -PathValue 'C:\\Tools;C:\\KD4\\bin\\;C:\\Other' -Entry 'c:\\kd4\\BIN'; "
            "if ($actual -cne 'C:\\Tools;C:\\Other') { Write-Error $actual; exit 9 }"
        )
        completed = subprocess.run(
            [ps, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", command],
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_windows_installer_retains_active_and_two_previous_releases(self) -> None:
        ps = powershell()
        if ps is None:
            self.skipTest("PowerShell is required for the Windows installer test")
        installer = REPO_ROOT / "scripts" / "install" / "install.ps1"
        with tempfile.TemporaryDirectory() as temp_dir:
            releases = Path(temp_dir) / "releases"
            active = releases / "active"
            incomplete = releases / "incomplete"
            completed_releases = [releases / f"previous-{index}" for index in range(3)]
            for release in (active, incomplete, *completed_releases):
                release.mkdir(parents=True)
            for index, release in enumerate(completed_releases):
                (release / "codex-install.env").write_text(
                    "version=1\n", encoding="utf-8"
                )
                os.utime(release, (index + 1, index + 1))

            command = (
                "$tokens=$null; $errors=$null; "
                f"$ast=[Management.Automation.Language.Parser]::ParseFile({ps_single_quote(installer)},[ref]$tokens,[ref]$errors); "
                "$fn=$ast.FindAll({param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq 'Remove-OldCompletedReleases'},$true)[0]; "
                "Invoke-Expression $fn.Extent.Text; $InstallMetadataFile='codex-install.env'; "
                f"Remove-OldCompletedReleases -ReleasesDir {ps_single_quote(releases)} -ActiveReleaseDir {ps_single_quote(active)} -RetainPrevious 2"
            )
            completed = subprocess.run(
                [ps, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", command],
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertTrue(active.is_dir())
            self.assertTrue(incomplete.is_dir())
            self.assertFalse(completed_releases[0].exists())
            self.assertTrue(completed_releases[1].is_dir())
            self.assertTrue(completed_releases[2].is_dir())

    def test_root_maintenance_covers_current_script_tooling_tests(self) -> None:
        root_maintenance = load_root_maintenance_module()

        expected_ruff_targets = sorted(
            path.relative_to(REPO_ROOT).as_posix()
            for root in root_maintenance.SCRIPT_AUDIT_ROOTS
            for path in root.rglob("*.py")
            if "__pycache__" not in path.parts and ".venv" not in path.parts
        )
        expected_unittest_targets = sorted(
            (
                path.relative_to(REPO_ROOT).with_suffix("").as_posix().replace("/", ".")
                if root_maintenance.SCRIPTS_ROOT in path.parents
                else path.relative_to(REPO_ROOT).as_posix()
            )
            for root in root_maintenance.SCRIPT_AUDIT_ROOTS
            for path in root.rglob("test_*.py")
            if "__pycache__" not in path.parts and ".venv" not in path.parts
        )

        self.assertEqual(
            root_maintenance.python_source_targets(), expected_ruff_targets
        )
        self.assertEqual(
            root_maintenance.python_unittest_targets(), expected_unittest_targets
        )
        self.assertEqual(
            root_maintenance.python_test_targets(
                ["scripts.test_build_tooling_policy"], []
            ),
            ["scripts.test_build_tooling_policy"],
        )
        self.assertEqual(
            root_maintenance.python_test_targets([], ["scripts/root_maintenance.py"]),
            ["scripts.test_build_tooling_policy"],
        )
        with mock.patch.object(
            root_maintenance,
            "git_changed_paths",
            return_value=["scripts/root_maintenance.py", "docs/example.md"],
        ):
            self.assertEqual(
                root_maintenance.expand_changed_paths([None]),
                ["scripts/root_maintenance.py", "docs/example.md"],
            )
        with mock.patch.object(
            root_maintenance,
            "git_changed_paths",
            return_value=["scripts/root_maintenance.py"],
        ):
            self.assertEqual(
                root_maintenance.python_test_targets(
                    [], root_maintenance.expand_changed_paths([None])
                ),
                ["scripts.test_build_tooling_policy"],
            )
        self.assertEqual(
            root_maintenance.test_module_for_changed_path("docs/example.md"),
            None,
        )
        self.assertEqual(
            root_maintenance.test_modules_for_changed_path(
                "scripts/publish-local-codex.ps1"
            ),
            (
                "scripts.test_publish_local_codex",
                "scripts.test_publish_local_codex_apply",
                "scripts.test_publish_local_codex_build",
                "scripts.test_publish_local_codex_dry_run",
                "scripts.test_publish_local_codex_freshness",
            ),
        )
        self.assertEqual(root_maintenance.python_lint_targets(["docs/example.md"]), [])
        self.assertEqual(
            root_maintenance.python_test_targets([], ["docs/example.md"]), []
        )
        self.assertEqual(
            root_maintenance.test_modules_for_changed_path(
                "Scripts/Test_Asciicheck.PY"
            ),
            ("scripts.test_asciicheck",),
        )

    def test_root_maintenance_routes_aggregate_python_script_tests(self) -> None:
        root_maintenance = load_root_maintenance_module()

        self.assertEqual(
            root_maintenance.python_test_targets(
                [],
                [
                    "scripts/investigation_eval/score_results.py",
                    "scripts/investigation_eval/validate_cases.py",
                    "scripts/kd4_model_attempt_analysis.py",
                ],
            ),
            [
                "scripts.investigation_eval.test_investigation_eval",
                "scripts.test_kd4_perf_snapshot",
            ],
        )

    def test_python_sdk_gate_and_publish_routing_match_source_map(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        sdk_recipe = justfile.split("\nsdk-python-check:\n", 1)[1].split("\n\n", 1)[0]
        source_map = (REPO_ROOT / "SOURCEMAP.md").read_text(encoding="utf-8")

        self.assertIn("--group dev ruff check .", sdk_recipe)
        self.assertIn("--group dev pytest", sdk_recipe)
        self.assertIn("| Windows local publish |", source_map)
        self.assertIn("`scripts/publish-local-codex.ps1`", source_map)
        self.assertIn("`just publish-local-codex-final`", source_map)

    def test_root_maintenance_script_audit_plan_covers_every_script_type(self) -> None:
        root_maintenance = load_root_maintenance_module()
        tools = {
            "uv": "uv",
            "pwsh": "pwsh",
            "node": "node",
        }

        commands, missing = root_maintenance.script_audit_commands(
            include_tests=True,
            test_targets=["scripts.test_asciicheck"],
            resolve_tool=tools.get,
        )

        self.assertEqual(missing, [])
        labels = [label for label, _command in commands]
        self.assertIn("Python format", labels)
        self.assertIn("Python lint", labels)
        self.assertIn("PowerShell syntax", labels)
        self.assertIn("justfile PowerShell syntax", labels)
        self.assertIn("justfile Python syntax", labels)
        self.assertIn(
            "Parser]::ParseInput",
            dict(commands)["justfile PowerShell syntax"][-1],
        )
        javascript_targets = [
            target
            for target, kind in root_maintenance.script_kind_map().items()
            if kind == "javascript"
        ]
        self.assertEqual(
            any(label.startswith("JavaScript syntax:") for label in labels),
            bool(javascript_targets),
        )
        self.assertIn("script unit tests", labels)
        unit_command = dict(commands)["script unit tests"]
        self.assertIn("scripts.test_asciicheck", unit_command)
        self.assertEqual(
            root_maintenance.test_modules_for_changed_path(
                "scripts/common-rust-env.ps1"
            ),
            ("scripts.test_build_tooling_performance",),
        )
        self.assertEqual(
            root_maintenance.test_modules_for_changed_path(
                "scripts/rust_build_status.py"
            ),
            ("scripts.test_build_tooling_storage",),
        )
        commands_without_tests, _missing = root_maintenance.script_audit_commands(
            include_tests=True,
            test_targets=[],
            resolve_tool=tools.get,
        )
        self.assertNotIn(
            "script unit tests",
            [label for label, _command in commands_without_tests],
        )

    def test_root_maintenance_parses_every_just_recipe_as_powershell(self) -> None:
        ps = powershell()
        if ps is None:
            self.skipTest("PowerShell is required for justfile syntax validation")
        root_maintenance = load_root_maintenance_module()
        commands, missing = root_maintenance.script_audit_commands(
            include_tests=False,
            resolve_tool={"uv": "uv", "pwsh": ps, "node": "node"}.get,
        )
        self.assertEqual(missing, [])
        result = subprocess.run(
            dict(commands)["justfile PowerShell syntax"],
            cwd=REPO_ROOT,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_justfile_script_recipes_are_checked_by_their_own_interpreter(self) -> None:
        root_maintenance = load_root_maintenance_module()

        powershell_sources = root_maintenance.just_powershell_sources()
        python_sources = root_maintenance.just_python_sources()

        self.assertTrue(any("cargo run" in source for _, source in powershell_sources))
        self.assertFalse(
            any("import runpy" in source for _, source in powershell_sources)
        )
        self.assertTrue(any("import runpy" in source for _, source in python_sources))
        for name, source in python_sources:
            compile(source, name, "exec")

    def test_root_maintenance_script_inventory_covers_owned_script_roots(
        self,
    ) -> None:
        root_maintenance = load_root_maintenance_module()

        expected_kinds = {
            ".codex/environments/setup.py": "python",
            "codex-cli/bin/codex.js": "javascript",
            "codex-cli/scripts/build_npm_package.py": "python",
            "codex-rs/app-server-test-client/scripts/live_elicitation_hold.ps1": "powershell",
            "codex-rs/config/scripts/generate-proto.ps1": "powershell",
            "codex-rs/responses-api-proxy/npm/bin/codex-responses-api-proxy.js": "javascript",
            "codex-rs/scripts/nextest_windows_stack.py": "python",
            "codex-rs/skills/src/assets/samples/imagegen/scripts/image_gen.py": "python",
            "sdk/python/scripts/update_sdk_artifacts.py": "python",
            "tools/argument-comment-lint/run.py": "python",
        }
        kind_by_target = root_maintenance.script_kind_map()

        for target, expected_kind in expected_kinds.items():
            with self.subTest(target=target):
                self.assertEqual(kind_by_target.get(target), expected_kind)
        self.assertIn(
            "tools/argument-comment-lint/test_wrapper_common.py",
            root_maintenance.python_unittest_targets(),
        )

    def test_root_maintenance_does_not_route_retired_task_continuity_paths(self) -> None:
        root_maintenance = load_root_maintenance_module()

        for target in (
            ".codex/hooks.json",
            ".codex/hooks/task-continuity-entry.ps1",
            ".codex/hooks/task-continuity-fast-basic.ps1",
            ".codex/hooks/task-continuity-fast-compact.ps1",
            ".codex/hooks/task-continuity-fast-session.ps1",
            ".codex/hooks/task-continuity.ps1",
        ):
            with self.subTest(target=target):
                self.assertNotIn(target, root_maintenance.SCRIPT_TEST_MODULES)

    def test_obsolete_developer_tooling_residue_is_absent(self) -> None:
        obsolete_paths = (
            ".devcontainer",
            "default.nix",
            "flake.lock",
            "flake.nix",
            "codex-cli/scripts/init_firewall.sh",
            "codex-cli/scripts/run_in_container.sh",
            "codex-rs/bwrap",
            "codex-rs/linux-sandbox",
            "codex-rs/shell-escalation",
            "codex-rs/vendor/bubblewrap",
            "scripts/install/install.sh",
            "scripts/test-remote-env.sh",
            "codex-rs/vendor/BUILD.bazel",
            "codex-rs/codex-backend-openapi-models/BUILD.bazel",
            "codex-rs/backend-client/BUILD.bazel",
            "codex-rs/login/BUILD.bazel",
        )

        for relative_path in obsolete_paths:
            with self.subTest(path=relative_path):
                self.assertFalse((REPO_ROOT / relative_path).exists())

        extensions = (REPO_ROOT / ".vscode" / "extensions.json").read_text(
            encoding="utf-8"
        )
        self.assertNotIn("BazelBuild.vscode-bazel", extensions)

        deny_config = (REPO_ROOT / "codex-rs" / "deny.toml").read_text(encoding="utf-8")
        self.assertNotIn('"webrtc-sys-build"', deny_config)

        repository_paths = repository_owned_paths()
        posix_launchers = sorted(
            str(path.relative_to(REPO_ROOT))
            for path in repository_paths
            if path.suffix.lower() in {".bash", ".sh", ".zsh"}
        )
        self.assertEqual(posix_launchers, [])

        retired_asset_pattern = re.compile(
            r"(?:^|/)(?:docker|linux|macos|darwin|mosh|tmux|wsl|wine|zellij)"
            r"(?:[-_.]|/)|(?:apple-darwin|pc-linux|unknown-linux)",
            re.IGNORECASE,
        )
        retired_assets = sorted(
            relative_path
            for path in repository_paths
            if retired_asset_pattern.search(
                relative_path := path.relative_to(REPO_ROOT).as_posix()
            )
        )
        self.assertEqual(retired_assets, [])

    def test_windows_only_rust_policy_has_no_host_platform_branches(self) -> None:
        cargo = load_toml(REPO_ROOT / "codex-rs" / "Cargo.toml")
        self.assertEqual(cargo["workspace"]["dependencies"]["arboard"], "3")

        deny = load_toml(REPO_ROOT / "codex-rs" / "deny.toml")
        self.assertEqual(
            set(deny["graph"]["targets"]),
            {"x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"},
        )

        schema = (REPO_ROOT / "codex-rs" / "core" / "config.schema.json").read_text(
            encoding="utf-8"
        )
        for retired_key in ("use_legacy_landlock", "use_linux_sandbox_bwrap"):
            with self.subTest(schema_key=retired_key):
                self.assertNotIn(retired_key, schema)

        conditional_dependencies: list[str] = []
        for manifest_path in (REPO_ROOT / "codex-rs").rglob("Cargo.toml"):
            if "target" in manifest_path.parts:
                continue
            manifest = manifest_path.read_text(encoding="utf-8")
            if "[target.'cfg(" in manifest:
                conditional_dependencies.append(
                    str(manifest_path.relative_to(REPO_ROOT))
                )
        self.assertEqual(conditional_dependencies, [])

        response_proxy_launcher = (
            REPO_ROOT
            / "codex-rs"
            / "responses-api-proxy"
            / "npm"
            / "bin"
            / "codex-responses-api-proxy.js"
        ).read_text(encoding="utf-8")
        for retired_platform in ("linux", "android", "darwin", "SIGHUP"):
            with self.subTest(response_proxy_platform=retired_platform):
                self.assertNotIn(retired_platform, response_proxy_launcher)

        codex_launcher = (REPO_ROOT / "codex-cli" / "bin" / "codex.js").read_text(
            encoding="utf-8"
        )
        self.assertNotIn("SIGHUP", codex_launcher)

        dotslash_manifest = (
            REPO_ROOT / "tools" / "argument-comment-lint" / "argument-comment-lint"
        ).read_text(encoding="utf-8")
        for retired_platform in ("macos-", "linux-", "apple-darwin", "unknown-linux"):
            with self.subTest(dotslash_platform=retired_platform):
                self.assertNotIn(retired_platform, dotslash_manifest)

        repository_paths = repository_owned_paths()
        rust_paths = [path for path in repository_paths if path.suffix == ".rs"]
        host_cfg_pattern = re.compile(
            r"(?:#\s*\[\s*cfg(?:_attr)?|cfg!)\s*\([^)]{0,500}"
            r"\b(?:target_family|target_os|unix|windows)\b"
        )
        host_cfg_branches: list[str] = []
        unix_imports: list[str] = []
        retired_platform_test_residue: list[str] = []
        retired_platform_test_pattern = re.compile(
            r"(?m)\b(?:async\s+)?fn\s+(?:linux|macos)_[A-Za-z0-9_]+\s*\(|"
            r"\b_unix_script\b|\bconst\s+IS_(?:MACOS|WINDOWS)\s*:|"
            r"#\[ignore\s*=\s*[\"'][^\"']*(?:linux|macos|unix|windows)[^\"']*[\"']\]"
        )
        for path in rust_paths:
            relative_path = path.relative_to(REPO_ROOT).as_posix()
            source = path.read_text(encoding="utf-8")
            if host_cfg_pattern.search(source):
                host_cfg_branches.append(relative_path)
            if "std::os::unix" in source:
                unix_imports.append(relative_path)
            if retired_platform_test_pattern.search(source):
                retired_platform_test_residue.append(relative_path)
        self.assertEqual(sorted(host_cfg_branches), [])
        self.assertEqual(sorted(unix_imports), [])
        self.assertEqual(sorted(retired_platform_test_residue), [])

        source_suffixes = {".js", ".md", ".ps1", ".py", ".rs", ".toml", ".ts"}
        policy_path = Path(__file__).resolve()
        retired_harness_pattern = re.compile(
            r"\b(?:DOCKER_CERT_PATH|DOCKER_HOST|DOCKER_TLS_VERIFY|WINEDEBUG|"
            r"WINEPREFIX|WSL_DISTRO_NAME|WSLENV|CODEX_[A-Z0-9_]*(?:DOCKER|WINE|WSL)"
            r"[A-Z0-9_]*)\b"
        )
        retired_harness_variables: list[str] = []
        for path in repository_paths:
            if (
                path.resolve() == policy_path
                or path.suffix.lower() not in source_suffixes
            ):
                continue
            source = path.read_text(encoding="utf-8")
            if retired_harness_pattern.search(source):
                retired_harness_variables.append(path.relative_to(REPO_ROOT).as_posix())
        self.assertEqual(sorted(retired_harness_variables), [])

        compatibility_parser_roots = (
            (REPO_ROOT / "codex-rs" / "apply-patch").resolve(),
            (REPO_ROOT / "codex-rs" / "shell-command").resolve(),
        )
        posix_runtime_launcher_pattern = re.compile(
            r"(?m)^\s*#!\s*/(?:usr/bin/env\s+)?(?:ba|z|)sh\b|"
            r"Command::new\s*\(\s*[\"'](?:bash|zsh|sh|/bin/(?:bash|zsh|sh))[\"']|"
            r"(?:exec|execFile|spawn)\s*\(\s*[\"']"
            r"(?:bash|zsh|sh|/bin/(?:bash|zsh|sh))[\"']|"
            r"Start-Process\s+(?:-FilePath\s+)?[\"']?(?:bash|zsh|sh)\b|"
            r"subprocess\.(?:Popen|call|check_call|check_output|run)\s*\(\s*"
            r"[\[(]?\s*[\"'](?:bash|zsh|sh|/bin/(?:bash|zsh|sh))[\"']"
        )
        posix_runtime_launchers: list[str] = []
        for path in repository_paths:
            resolved_path = path.resolve()
            if (
                resolved_path == policy_path
                or path.suffix.lower() not in source_suffixes
            ):
                continue
            if any(
                resolved_path == root or root in resolved_path.parents
                for root in compatibility_parser_roots
            ):
                continue
            source = path.read_text(encoding="utf-8")
            if posix_runtime_launcher_pattern.search(source):
                posix_runtime_launchers.append(path.relative_to(REPO_ROOT).as_posix())
        self.assertEqual(sorted(posix_runtime_launchers), [])

        runtime_docs = "\n".join(
            (REPO_ROOT / relative_path).read_text(encoding="utf-8")
            for relative_path in (
                "codex-rs/app-server/README.md",
                "codex-rs/exec-server/README.md",
            )
        )
        for retired_runtime_example in (
            "/Users/",
            "/usr/bin:/bin",
            "file:///tmp",
        ):
            with self.subTest(runtime_example=retired_runtime_example):
                self.assertNotIn(retired_runtime_example, runtime_docs)

    def test_ignore_rules_have_single_owners_for_generated_artifacts(self) -> None:
        root_ignore = (REPO_ROOT / ".gitignore").read_text(encoding="utf-8")
        codex_ignore = (REPO_ROOT / ".codex" / ".gitignore").read_text(encoding="utf-8")
        rust_ignore = (REPO_ROOT / "codex-rs" / ".gitignore").read_text(
            encoding="utf-8"
        )

        self.assertNotIn(".codex/evals/", root_ignore)
        self.assertIn("/evals/", codex_ignore.splitlines())
        self.assertIn("*.pdb", rust_ignore.splitlines())

    def test_dependency_roles_match_published_consumers(self) -> None:
        sdk_package = json.loads(
            (REPO_ROOT / "sdk" / "typescript" / "package.json").read_text(
                encoding="utf-8"
            )
        )
        self.assertIn("@modelcontextprotocol/sdk", sdk_package["dependencies"])
        self.assertNotIn("@modelcontextprotocol/sdk", sdk_package["devDependencies"])

    def test_sdk_build_owns_cleanup(self) -> None:
        sdk_package = json.loads(
            (REPO_ROOT / "sdk" / "typescript" / "package.json").read_text(
                encoding="utf-8"
            )
        )
        self.assertNotIn("clean", sdk_package["scripts"])
        tsup_config = (REPO_ROOT / "sdk" / "typescript" / "tsup.config.ts").read_text(
            encoding="utf-8"
        )
        self.assertIn("clean: true", tsup_config)

    def test_config_documentation_uses_current_public_destinations(self) -> None:
        config_docs = (REPO_ROOT / "codex-rs" / "config.md").read_text(encoding="utf-8")

        self.assertNotIn("../docs/config.md", config_docs)
        self.assertIn(
            "https://developers.openai.com/codex/config-file/config-reference",
            config_docs,
        )
        self.assertIn("https://developers.openai.com/codex/extend/mcp", config_docs)

    def test_root_maintenance_script_audit_current_tree_has_no_hard_findings(
        self,
    ) -> None:
        root_maintenance = load_root_maintenance_module()

        errors, _advisories = root_maintenance.script_audit_findings()

        self.assertEqual(errors, [])

    def test_root_maintenance_script_audit_context_matches_current_routes(
        self,
    ) -> None:
        root_maintenance = load_root_maintenance_module()

        self.assertEqual(root_maintenance.script_audit_context_issues(), [])

    def test_root_maintenance_script_audit_has_no_platform_skips(self) -> None:
        root_maintenance = load_root_maintenance_module()

        targets = root_maintenance.script_audit_test_targets()

        self.assertNotIn("scripts.install.test_install_sh", targets)
        self.assertFalse(any(target.endswith("_sh") for target in targets))

    def test_root_maintenance_script_audit_success_has_no_stale_skip_summary(
        self,
    ) -> None:
        root_maintenance = load_root_maintenance_module()
        stdout = io.StringIO()

        with (
            mock.patch.object(
                root_maintenance,
                "script_source_targets",
                return_value=["scripts/example.py"],
            ),
            mock.patch.object(
                root_maintenance,
                "script_kind_map",
                return_value={"scripts/example.py": "python"},
            ),
            mock.patch.object(
                root_maintenance, "script_audit_context_issues", return_value=[]
            ),
            mock.patch.object(
                root_maintenance, "script_audit_findings", return_value=([], [])
            ),
            mock.patch.object(
                root_maintenance, "script_audit_test_targets", return_value=[]
            ),
            mock.patch.object(
                root_maintenance, "script_audit_commands", return_value=([], [])
            ),
            mock.patch.object(
                root_maintenance, "git_context_label", return_value="test"
            ),
            contextlib.redirect_stdout(stdout),
        ):
            self.assertEqual(
                root_maintenance.run_script_audit(
                    include_tests=True,
                    strict=False,
                ),
                0,
            )

        self.assertIn(
            "SCRIPT AUDIT PASSED: 1 script artifact(s), 0 command group(s), "
            "0 advisory item(s).",
            stdout.getvalue(),
        )
        self.assertNotIn("platform test skip", stdout.getvalue())

    def test_root_maintenance_git_paths_use_nul_delimiters(self) -> None:
        root_maintenance = load_root_maintenance_module()
        tracked = subprocess.CompletedProcess(
            ["git"],
            0,
            stdout="scripts/line\nbreak.py\0",
            stderr="",
        )
        untracked = subprocess.CompletedProcess(
            ["git"],
            0,
            stdout="scripts/ trailing .py\0",
            stderr="",
        )

        with mock.patch.object(
            root_maintenance.subprocess, "run", side_effect=[tracked, untracked]
        ) as run:
            paths = root_maintenance.git_changed_paths()

        self.assertEqual(
            paths,
            ["scripts/line\nbreak.py", "scripts/ trailing .py"],
        )
        self.assertEqual(run.call_count, 2)
        self.assertIn("-z", run.call_args_list[0].args[0])
        self.assertIn("--diff-filter=ACDMRTUXB", run.call_args_list[0].args[0])
        self.assertIn("--others", run.call_args_list[1].args[0])

    def test_changed_production_script_without_tests_is_unverified(self) -> None:
        root_maintenance = load_root_maintenance_module()

        with mock.patch.object(root_maintenance, "run") as run:
            self.assertEqual(
                root_maintenance.main(
                    [
                        "test-python",
                        "--changed",
                        "scripts/unmapped_audit189_helper.py",
                    ]
                ),
                2,
            )
            self.assertEqual(
                root_maintenance.main(["test-python", "--changed", "docs/example.md"]),
                0,
            )

        run.assert_not_called()

    def test_format_empty_changed_selection_is_a_noop(self) -> None:
        format_script = load_format_module()

        with (
            mock.patch.object(format_script, "resolved_changed_paths", return_value=[]),
            mock.patch.object(format_script, "run_formatter_group") as run,
        ):
            self.assertEqual(
                format_script.main(
                    ["--write", "--only", "python-scripts", "--changed"]
                ),
                0,
            )

        run.assert_not_called()

    def test_format_default_python_scope_stays_with_internal_scripts(self) -> None:
        format_script = load_format_module()
        group = format_script.FormatterGroup("Python scripts", ())

        with (
            mock.patch.object(
                format_script, "resolved_changed_paths"
            ) as resolve_changed,
            mock.patch.object(
                format_script, "formatter_groups", return_value=(group,)
            ) as groups,
            mock.patch.object(
                format_script,
                "run_formatter_group",
                return_value=format_script.FormatterResult("Python scripts", "", 0),
            ),
        ):
            self.assertEqual(
                format_script.main(["--check", "--only", "python-scripts"]), 0
            )

        resolve_changed.assert_not_called()
        self.assertEqual(groups.call_args.kwargs["python_script_targets"], ("scripts",))

    def test_root_maintenance_missing_command_is_reported(self) -> None:
        root_maintenance = load_root_maintenance_module()
        stderr = io.StringIO()

        with (
            mock.patch.object(
                root_maintenance.subprocess,
                "run",
                side_effect=FileNotFoundError("missing"),
            ),
            contextlib.redirect_stderr(stderr),
        ):
            self.assertEqual(root_maintenance.run(["missing-tool"]), 127)

        self.assertIn("Could not run missing-tool", stderr.getvalue())

    def test_root_maintenance_does_not_duplicate_formatter_commands(self) -> None:
        root_maintenance = load_root_maintenance_module()
        subcommands = root_maintenance.build_parser()._subparsers._group_actions[0]

        self.assertNotIn("format-prettier", subcommands.choices)
        self.assertNotIn("format-python", subcommands.choices)

    def test_root_maintenance_uv_commands_use_frozen_lock(self) -> None:
        root_maintenance = load_root_maintenance_module()
        calls: list[tuple[str, ...]] = []

        def fake_run(command: list[str]) -> int:
            calls.append(tuple(command))
            return 0

        with mock.patch.object(root_maintenance, "run", side_effect=fake_run):
            self.assertEqual(
                root_maintenance.main(
                    ["lint-python", "--changed", "scripts/root_maintenance.py"]
                ),
                0,
            )
            self.assertEqual(
                root_maintenance.main(
                    ["test-python", "--module", "scripts.test_build_tooling_policy"]
                ),
                0,
            )

        for command in calls:
            self.assertEqual(command[:4], ("uv", "run", "--frozen", "--project"))

    def test_codex_cli_launcher_parses_under_node(self) -> None:
        node = shutil.which("node")
        if node is None:
            self.skipTest("node is not available")

        result = subprocess.run(
            [node, "--check", str(REPO_ROOT / "codex-cli" / "bin" / "codex.js")],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
        )

        self.assertEqual(result.returncode, 0, result.stderr)

    def test_run_python_enforces_the_supported_interpreter_version(self) -> None:
        node = shutil.which("node")
        if node is None:
            self.skipTest("node is not available")
        launcher = REPO_ROOT / "scripts" / "run-python.js"
        self.assertIn(
            "sys.version_info >= (3, 11)", launcher.read_text(encoding="utf-8")
        )

        with tempfile.TemporaryDirectory() as temp_dir:
            marker = Path(temp_dir) / "selected.txt"
            script = Path(temp_dir) / "selected.py"
            script.write_text(
                "from pathlib import Path\n"
                f"Path({str(marker)!r}).write_text('ok', encoding='utf-8')\n",
                encoding="utf-8",
            )
            result = subprocess.run(
                [node, str(launcher), str(script)],
                cwd=REPO_ROOT,
                env={**os.environ, "PYTHON": sys.executable},
                capture_output=True,
                text=True,
                check=False,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(marker.read_text(encoding="utf-8"), "ok")

    def test_formatting_commands_only_target_existing_repository_sources(
        self,
    ) -> None:
        package = json.loads((REPO_ROOT / "package.json").read_text(encoding="utf-8"))
        format_script = load_format_module()

        self.assertEqual(
            package["scripts"]["format"],
            "node scripts/run-python.js scripts/format.py --check --only prettier",
        )
        self.assertEqual(
            package["scripts"]["format:fix"],
            "node scripts/run-python.js scripts/format.py --write --only prettier",
        )
        self.assertEqual(
            package["scripts"]["format:python"],
            "node scripts/run-python.js scripts/format.py --check --only python-scripts",
        )
        self.assertEqual(
            package["scripts"]["format:python:fix"],
            "node scripts/run-python.js scripts/format.py --write --only python-scripts",
        )
        tracked_paths = set(
            subprocess.run(
                ["git", "ls-files"],
                cwd=REPO_ROOT,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                check=True,
            ).stdout.splitlines()
        )
        for target in format_script.PRETTIER_TARGETS:
            with self.subTest(target=target):
                local_matches = {
                    path.relative_to(REPO_ROOT).as_posix()
                    for path in REPO_ROOT.glob(target)
                    if path.is_file()
                }
                self.assertTrue(
                    local_matches & tracked_paths,
                    f"Prettier target does not match repository files: {target}",
                )
        self.assertEqual(
            package["scripts"]["test:scripts:changed"],
            "node scripts/run-python.js scripts/root_maintenance.py test-python --changed",
        )
        self.assertNotIn("test:scripts:target", package["scripts"])

    def test_justfile_only_exposes_canonical_developer_tooling_recipes(self) -> None:
        justfile = "\n" + (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        package = json.loads((REPO_ROOT / "package.json").read_text(encoding="utf-8"))

        self.assertIn("\ncargo-lane-isolated-home lane *args:", justfile)
        self.assertIn("\nconfig-schema-check:", justfile)
        self.assertIn("\nconfig-schema-regenerate owner:", justfile)
        self.assertIn("\napp-server-schema-check:", justfile)
        self.assertIn('\napp-server-schema-regenerate owner experimental="":', justfile)
        self.assertIn("\nwrite-hooks-schema:", justfile)
        self.assertNotIn("write-hooks-schema", package["scripts"])
        for obsolete_recipe in (
            "cargo-lane-home",
            "cargo-lane-main",
            "test-github-scripts",
            "write-config-schema",
            "config-schema-check-force",
            "write-app-server-schema",
            "app-server-schema-check-force",
            "app-server-schema-runtime-check",
            "app-server-schema-runtime-check-with-runtime",
            "app-server-schema-runtime-check-force",
            "source-owners-slice-focused",
        ):
            with self.subTest(recipe=obsolete_recipe):
                self.assertNotIn(f"\n{obsolete_recipe}", justfile)
        self.assertNotIn("\ndead-code *args:", justfile)
        self.assertNotIn("\ntest-full *args:", justfile)
        for obsolete_path in (
            "scripts/run-powershell-script.ps1",
            "scripts/test_run_powershell_script.py",
        ):
            with self.subTest(path=obsolete_path):
                self.assertFalse((REPO_ROOT / obsolete_path).exists())

        result = subprocess.run(
            [
                "just",
                "--dry-run",
                "app-server-schema-regenerate",
                "policy-test",
                "--experimental",
            ],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        rendered = result.stdout + result.stderr
        self.assertIn('--owner "policy-test" -- --experimental', rendered)

        result = subprocess.run(
            [
                "just",
                "source-owners-slice",
                "source-owner-index",
                "--focus",
                "canonical tooling command",
            ],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        source_owner_slice = json.loads(result.stdout)
        self.assertFalse(source_owner_slice["truncated"])
        self.assertEqual(source_owner_slice["omitted_relationships"], 0)
        self.assertEqual(source_owner_slice["material_unknowns"], [])
        self.assertEqual(justfile.count("\nsource-owners-slice "), 1)

        result = subprocess.run(
            ["just", "--dry-run", "cargo-lane", "main", "cargo", "--version"],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('run-lane --lane "main"', result.stdout + result.stderr)

        canonical_command_sources = {
            "codex-rs/app-server/README.md": "app-server-schema-regenerate <owner>",
            "codex-rs/app-server-protocol/tests/schema_fixtures.rs": (
                "app-server-schema-regenerate <owner>"
            ),
            "codex-rs/core/src/config/schema.md": "config-schema-regenerate <owner>",
            "codex-rs/core/src/config/schema_tests.rs": (
                "config-schema-regenerate <owner>"
            ),
            "codex-rs/core/src/tools/handlers/shell_tests.rs": (
                '"config-schema-regenerate", "validation-test"'
            ),
        }
        for relative_path, canonical_command in canonical_command_sources.items():
            with self.subTest(path=relative_path):
                source = (REPO_ROOT / relative_path).read_text(encoding="utf-8")
                self.assertIn(canonical_command, source)
                self.assertNotIn("just write-config-schema", source)
                self.assertNotIn("just write-app-server-schema", source)

    def test_rust_package_search_start_keeps_existing_dotted_directories(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            package_dir = repo_root / "codex-rs" / "crate.with.dot"
            package_dir.mkdir(parents=True)
            (package_dir / "Cargo.toml").write_text(
                '[package]\nname = "crate-with-dot"\n',
                encoding="utf-8",
            )

            self.assertEqual(
                rust_packages.package_search_start(package_dir), package_dir
            )
            self.assertEqual(
                rust_packages.nearest_package_root(package_dir, repo_root=repo_root),
                package_dir,
            )

    def test_rust_package_search_does_not_escape_repo_root(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            parent = Path(temp_dir)
            (parent / "Cargo.toml").write_text(
                '[package]\nname = "external"\n',
                encoding="utf-8",
            )
            repo_root = parent / "kd4"
            script = repo_root / "scripts" / "format.py"
            script.parent.mkdir(parents=True)
            script.write_text("", encoding="utf-8")

            self.assertIsNone(
                rust_packages.nearest_package_root(
                    script,
                    repo_root=repo_root,
                    assume_file=True,
                )
            )

    def test_rust_package_search_reuses_cached_ancestor(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            package_root = repo_root / "codex-rs" / "crate"
            nested = package_root / "src" / "nested"
            nested.mkdir(parents=True)
            cache = {package_root: package_root}

            self.assertEqual(
                rust_packages.nearest_package_root(
                    nested,
                    repo_root=repo_root,
                    package_root_cache=cache,
                ),
                package_root,
            )
            self.assertEqual(cache[nested], package_root)

    def test_rust_package_search_skips_virtual_workspace_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            repo_root = Path(temp_dir)
            codex_rs_root = repo_root / "codex-rs"
            source = codex_rs_root / "workspace-file.rs"
            codex_rs_root.mkdir()
            (codex_rs_root / "Cargo.toml").write_text(
                '[workspace]\nmembers = ["crate"]\n',
                encoding="utf-8",
            )
            source.write_text("", encoding="utf-8")

            self.assertIsNone(
                rust_packages.nearest_package_root(
                    source,
                    repo_root=repo_root,
                    assume_file=True,
                )
            )

    def test_formatter_group_decodes_command_output_as_utf8(self) -> None:
        format_script = load_format_module()
        group = format_script.FormatterGroup(
            "Test",
            (
                format_script.Command(
                    (
                        sys.executable,
                        "-c",
                        "import sys; sys.stdout.buffer.write(b'check \\xf0\\x9f\\x9b\\xa0 done')",
                    )
                ),
            ),
        )

        result = format_script.run_formatter_group(group)

        self.assertEqual(result.returncode, 0)
        self.assertIn("check \U0001f6e0 done", result.output)

    def test_agents_validation_tooling_does_not_prove_runtime_fix(self) -> None:
        text = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")
        normalized = " ".join(text.split())

        self.assertIn(
            "A formatter, linter, build, applied patch, or successful command "
            "selecting zero relevant tests is not runtime proof",
            normalized,
        )
        self.assertIn(
            "Runtime proof requires a direct contract test or a user-approved "
            "end-to-end gate",
            normalized,
        )

    def test_local_rust_loop_recipes_are_discoverable(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        for recipe in (
            "cargo-fetch:",
            "codex-fast *args:",
            "codex-lane *args:",
            "codex-stale-ok *args:",
            "fix-lane",
            "watch-lane package *args:",
            "coverage-lane package *args:",
            "rust-build-doctor:",
            "target-disk:",
            "target-prune *args:",
            "target-optimize *args:",
            "target-optimize-dry-run *args:",
            "build-dev-small package:",
            "run-dev-small package *args:",
            "local-release package:",
            "build-for-release *args:",
            "bench-workspace *args:",
            "test-lane-fast lane *args:",
            "test-windows-sandbox-processes *args:",
            "deps-duplicates-workspace *args:",
            "deps-policy-check *args:",
        ):
            self.assertIn(recipe, justfile)

    def test_dead_code_matrix_uses_dedicated_cargo_lane(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        self.assertIn("cargo-workspace-analyzer.ps1", justfile)
        analyzer = (REPO_ROOT / "scripts/cargo-workspace-analyzer.ps1").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            "$ForwardedArgs | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }",
            analyzer,
        )
        self.assertIn('$lane = "rust-dead-code-matrix"', analyzer)
        result, payloads = self.run_workspace_analyzer(
            "dead-code",
            "--package=codex-core",
            rustflags="-C target-cpu=native --cfg existing",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(payloads), 1, result.stdout)
        self.assertEqual(
            payloads[0]["rustflags"],
            "-C target-cpu=native --cfg existing -Ddead_code",
        )
        self.assertNotIn("--workspace", payloads[0]["args"])

    def test_workspace_analyzer_recognizes_equals_form_selectors(self) -> None:
        for selector in (
            "--package=codex-core",
            "--manifest-path=codex-rs/core/Cargo.toml",
        ):
            with self.subTest(selector=selector):
                result, payloads = self.run_workspace_analyzer("dead-code", selector)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(len(payloads), 1, result.stdout)
                self.assertIn(selector, payloads[0]["args"])
                self.assertNotIn("--workspace", payloads[0]["args"])

    def test_windows_v8_fallback_tracks_remaining_forwarding_packages(self) -> None:
        analyzer = (REPO_ROOT / "scripts/cargo-workspace-analyzer.ps1").read_text(
            encoding="utf-8"
        )

        self.assertIn('$v8SandboxPackage = "codex-code-mode"', analyzer)
        self.assertNotIn("codex-v8-poc", analyzer)
        self.assertIn(
            '$workspaceArgs = $cargoArgs + @("--exclude", $v8SandboxPackage)',
            analyzer,
        )
        self.assertIn('$packageArgs += @("--package", $v8SandboxPackage)', analyzer)
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        workspace_recipe = justfile.split("clippy-workspace *args:", 1)[1].split(
            "\n\n", 1
        )[0]
        self.assertIn("-Analyzer clippy --workspace @forwarded_args", workspace_recipe)

        result, payloads = self.run_workspace_analyzer(
            "clippy", "--workspace", "--all-features", os_name="Windows_NT"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(payloads), 2, result.stdout)
        self.assertIn("--workspace", payloads[0]["args"])
        self.assertIn("--exclude", payloads[0]["args"])
        self.assertIn("--package", payloads[1]["args"])

    def test_package_validation_defaults_do_not_expand_to_workspace(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        self.assertIn("Pass a package/filter to 'just clippy'", justfile)
        self.assertIn("clippy-workspace *args:", justfile)
        clippy_recipe = justfile.split("clippy *args:", 1)[1].split("\n\n", 1)[0]
        workspace_recipe = justfile.split("clippy-workspace *args:", 1)[1].split(
            "\n\n", 1
        )[0]
        self.assertIn("cargo clippy --tests @forwarded_args", clippy_recipe)
        self.assertNotIn("--workspace", clippy_recipe)
        self.assertIn("-Analyzer clippy --workspace @forwarded_args", workspace_recipe)

    def test_windows_process_suite_cannot_silently_skip_required_coverage(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        sandbox_tests = (
            REPO_ROOT / "codex-rs/windows-sandbox-rs/src/unified_exec/tests.rs"
        ).read_text(encoding="utf-8")
        pty_tests = (REPO_ROOT / "codex-rs/utils/pty/src/windows_tests.rs").read_text(
            encoding="utf-8"
        )

        self.assertIn("[windows]\ntest-windows-sandbox-processes *args:", justfile)
        # The two non-core legs still force the zero-test policy inline. The
        # codex-core leg gets it from its named gate, which forces
        # `--no-tests=fail` inside `scripts/rust_test_runner.py`.
        sandbox_recipe = justfile.split("test-windows-sandbox-processes *args:", 1)[
            1
        ].split("\n\n", 1)[0]
        self.assertEqual(sandbox_recipe.count("--no-tests=fail"), 2)
        self.assertIn("just core-gate windows-sandbox-core-exec", sandbox_recipe)
        self.assertIn("-p codex-utils-pty", justfile)
        self.assertIn("CODEX_REQUIRE_WINDOWS_SANDBOX_PROCESS_TESTS", justfile)
        self.assertIn("CODEX_REQUIRE_WINDOWS_SANDBOX_PROCESS_TESTS", sandbox_tests)
        self.assertIn("CODEX_REQUIRE_WINDOWS_SANDBOX_PROCESS_TESTS", pty_tests)
        self.assertIn(
            "required legacy sandbox prerequisite is",
            sandbox_tests,
        )
        self.assertIn(
            "Windows process verification was not run: required prerequisite",
            pty_tests,
        )
        self.assertIn("Python executable (`python3` or `python`)", pty_tests)
        self.assertIn(
            "PowerShell executable (`pwsh.exe` or `powershell.exe`)",
            pty_tests,
        )
        self.assertNotIn("python not found; skipping", pty_tests)

    def test_local_setup_recipes_avoid_stale_or_unlocked_dependency_state(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        # `codex-fast` must actually be fast: reuse the built binary instead of
        # duplicating the plain `codex` recipe.
        self.assertIn("codex-fast *args:\n    just codex-stale-ok {args}", justfile)
        # Install/setup paths must not quietly re-resolve the lockfile.
        self.assertIn("cargo fetch --locked", justfile)
        self.assertNotIn("cargo fetch\n", justfile)

    def test_high_frequency_python_recipes_bypass_the_powershell_adapter(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        for recipe in ("fmt", "fmt-check-fast", "fmt-full", "fmt-check"):
            marker = f'[script("python")]\n{recipe}:'
            self.assertIn(marker, justfile)
        self.assertIn(
            '[no-cd]\n[script("python")]\ncheck-kd4-features *args:', justfile
        )
        feature_recipe = subprocess.run(
            ["just", "--show", "check-kd4-features"],
            cwd=REPO_ROOT,
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
        )
        self.assertEqual(feature_recipe.returncode, 0, feature_recipe.stderr)
        self.assertIn('[script("python")]', feature_recipe.stdout)
        self.assertIn("forwarded = sys.argv[1:]", feature_recipe.stdout)
        self.assertIn("sys.argv = [script, *forwarded]", feature_recipe.stdout)

    def test_direct_python_recipe_preserves_argv_and_exit_code(self) -> None:
        help_result = subprocess.run(
            ["just", "check-kd4-features", "--help"],
            cwd=REPO_ROOT,
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
        )
        self.assertEqual(help_result.returncode, 0, help_result.stderr)
        self.assertIn("usage:", help_result.stdout.lower())

        unicode_argument = "--unknown-KD4-λ-path with spaces"
        rejected = subprocess.run(
            ["just", "check-kd4-features", unicode_argument],
            cwd=REPO_ROOT,
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
        )
        self.assertEqual(rejected.returncode, 2, rejected.stderr)
        rendered_argument = unicode_argument.encode("unicode_escape").decode("ascii")
        self.assertIn(rendered_argument, rejected.stderr)

    def test_release_packaging_policy_is_explicit_and_pinned(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        cargo_manifest = (REPO_ROOT / "codex-rs" / "Cargo.toml").read_text(
            encoding="utf-8"
        )
        npm_manifest = json.loads(
            (REPO_ROOT / "codex-cli" / "package.json").read_text(encoding="utf-8")
        )

        self.assertIn('rust_parallelism := "8"', justfile)
        self.assertIn('$requiredPwshVersion = [version]"7.5.2"', justfile)
        self.assertIn("just test-release-tooling", justfile)
        self.assertIn("prepare-codex-release version:", justfile)
        self.assertIn("cosign verify-blob", justfile)
        self.assertIn('strip = "symbols"', cargo_manifest)
        self.assertTrue(npm_manifest["private"])
        self.assertEqual(
            npm_manifest["repository"]["url"],
            "git+https://github.com/ikhdark/KD4.git",
        )

    def test_dependency_policy_gate_runs_offline_cargo_deny(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        self.assertIn("cargo deny check bans sources licenses", justfile)
        self.assertIn("cargo tree -d --workspace", justfile)
        self.assertNotIn("--target all", justfile)

    def test_lane_recipes_use_the_canonical_reserved_runner(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        for snippet in (
            'run-lane --lane "{{ package }}" -- just _test-lane-package-reserved',
            'cargo nextest run --target-dir $target_dir -p "{{ package }}"',
            'run-lane --lane "{{ package }}" -- cargo check',
            'run-lane --lane "{{ package }}" -- cargo clippy',
            "run-lane --lane release -- cargo build --release",
            "run-lane --lane app-server-test-client -- just _app-server-test-client-reserved",
        ):
            self.assertIn(snippet, justfile)
        self.assertNotIn("target/lanes/", justfile)
        self.assertEqual(justfile.count("scripts\\cargo-lane.ps1"), 1)
        self.assertIn('cargo-lane.ps1" -Lane "{{ lane }}" -IsolateCargoHome', justfile)

    def test_high_contention_just_recipes_use_cargo_lanes_on_windows(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        for command in (
            "cargo clippy --fix --tests --allow-dirty @forwarded_args",
            "cargo clippy --tests @forwarded_args",
            "fix-workspace *args:",
            "clippy-workspace *args:",
            "Pass a package/filter to 'just fix'",
            "Pass a package/filter to 'just clippy'",
            "cargo nextest run --no-run @forwarded_args",
            'cargo watch -x "check --target-dir $target_dir -p {{ package }}" @forwarded_args',
            'cargo llvm-cov -p "{{ package }}" @($args | Select-Object -Skip 2)',
            "just _test-lane-package-reserved",
            "$target_dir = $env:CODEX_CARGO_LANE_TARGET_DIR",
            '$env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "fast"; cargo nextest run --target-dir $target_dir -p "{{ package }}" @forwarded_args',
        ):
            self.assertIn(command, justfile)
        self.assertNotIn(
            "test-lane-package package *args:\n    @$forwarded_args",
            justfile,
        )
        self.assertGreaterEqual(
            justfile.count('scripts\\rust_build_status.py" run-lane'), 10
        )
        self.assertNotIn('$target_dir = "target\\lanes\\', justfile)

    def test_core_tests_only_run_through_named_targets_and_gates(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        manifest = load_toml(
            REPO_ROOT / "codex-rs" / ".config" / "kd4-rust-tests.toml"
        )

        # Every generic nextest recipe refuses a codex-core selection instead of
        # inferring helper binaries from the forwarded arguments.
        for recipe in (
            "test *args:",
            "test-fast *args:",
            "test-fast-nosccache *args:",
            "test-compile *args:",
            "test-timings *args:",
            "_test-lane-local-reserved *args:",
            "_test-lane-fast-reserved *args:",
            "_test-lane-package-reserved package *args:",
        ):
            body = justfile.split(recipe, 1)[1].split("\n\n", 1)[0]
            self.assertIn("rust_test_runner.py\" _guard-generic --", body, recipe)

        # The named recipes replace the removed helper-inference recipes.
        for recipe in (
            "core-test target *args:",
            "core-test-fast target *args:",
            "core-test-lane target *args:",
            "_core-test-lane-reserved target *args:",
            "core-gate gate:",
            "core-test-list:",
            "core-test-manifest-check:",
        ):
            self.assertIn(recipe, justfile)
        self.assertNotIn("_core-test-helpers", justfile)
        self.assertNotIn("(?i)rmcp|mcp|plugin|test_stdio_server", justfile)
        self.assertNotIn("(?i)windows_sandbox|windows-sandbox|sandbox", justfile)

        # Helper builds moved into the manifest, so no recipe may build them.
        self.assertNotIn("--bin test_stdio_server", justfile)
        self.assertNotIn("--bin codex-windows-sandbox-setup", justfile)

        # Repository-owned codex-core invocations go through named gates.
        self.assertNotIn("nextest run -p codex-core", justfile)
        self.assertNotIn("-p codex-core", justfile)
        for gate in (
            "adaptive-reasoning-contract",
            "config-schema-protocol",
            "windows-sandbox-core-exec",
        ):
            self.assertIn(f"just core-gate {gate}", justfile)
            self.assertIn(gate, manifest["gates"])

        # The app-server thread-status recipe kept only its two real contracts.
        thread_status = justfile.split("_app-server-thread-status-tests:", 1)[1].split(
            "\n\n", 1
        )[0]
        self.assertNotIn("validated_invalidated_tracker_still_requests_diff_fallback", thread_status)
        self.assertIn("stale_active_running_thread_resume_clears_watch_status", thread_status)
        self.assertIn("stale_active_repair_preserves_pending_approval_status", thread_status)

    def test_perf_env_recipes_pass_structured_argv(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        perf_env = (REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1").read_text(
            encoding="utf-8"
        )

        self.assertIn("[string]$CargoTargetLane", perf_env)
        self.assertIn("[Parameter(ValueFromRemainingArguments = $true)]", perf_env)
        self.assertIn("[string[]]$ProgramArgs", perf_env)
        self.assertIn("& $program @arguments", perf_env)
        self.assertIn('"run-lane"', perf_env)
        self.assertIn('"--lane"', perf_env)
        self.assertIn("-ProgramArgs $forwarded_args", justfile)
        self.assertIn("-ProgramArgs $command_args", justfile)
        self.assertIn('"--release"', justfile)
        self.assertIn(
            '& "{{ justfile_directory() }}\\scripts\\invoke-rust-perf-env.ps1"',
            justfile,
        )
        self.assertGreaterEqual(justfile.count("; exit $LASTEXITCODE"), 3)
        self.assertIn('-CargoTargetLane "perf-nextest-nosccache"', justfile)
        self.assertIn('-CargoTargetLane "release-cli"', justfile)
        self.assertNotIn(
            'pwsh -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\\scripts\\invoke-rust-perf-env.ps1"',
            justfile,
        )
        self.assertNotIn('-CommandLine (("cargo', justfile)
        self.assertNotIn("[string]$CommandLine", perf_env)
        self.assertNotIn("cmd.exe /d /s /c", perf_env)


if __name__ == "__main__":
    unittest.main()
