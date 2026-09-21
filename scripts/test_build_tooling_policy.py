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
    def test_dead_code_preserves_target_config_and_rejects_unused_code(self):
        shell = powershell()
        if shell is None or shutil.which("cargo") is None:
            self.skipTest("PowerShell and Cargo are required")
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            analyzer = root / "cargo-workspace-analyzer.ps1"
            shutil.copyfile(REPO_ROOT / "scripts" / analyzer.name, analyzer)
            shutil.copyfile(
                REPO_ROOT / "scripts" / "common-rust-env.ps1",
                root / "common-rust-env.ps1",
            )
            (root / "cargo-lane.ps1").write_text(
                "$cargoArgs = @($args | Select-Object -Skip 3)\n"
                "& cargo @cargoArgs\nexit $LASTEXITCODE\n",
                encoding="utf-8",
            )
            (root / "Cargo.toml").write_text(
                '[package]\nname="lint-probe"\nversion="0.1.0"\nedition="2021"\n'
                '[lib]\npath="lib.rs"\n',
                encoding="utf-8",
            )
            config = root / ".cargo" / "config.toml"
            config.parent.mkdir()
            config.write_text(
                "[target.'cfg(all())']\nrustflags=[\"--cfg=normal_flags\"]\n",
                encoding="utf-8",
            )
            source = root / "lib.rs"
            prefix = '#[cfg(not(normal_flags))]\ncompile_error!("lost target flags");\n'
            env = os.environ.copy()
            for key in list(env):
                if key.startswith("CARGO_") or key in (
                    "RUSTFLAGS",
                    "RUSTC_WRAPPER",
                    "RUSTC_WORKSPACE_WRAPPER",
                ):
                    env.pop(key)
            env["CARGO_HOME"] = str(root / "cargo-home")
            command = [
                shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                str(analyzer),
                "-Analyzer",
                "dead-code",
                "--offline",
                "-p",
                "lint-probe",
            ]
            source.write_text(prefix + "fn unused_probe() {}\n", encoding="utf-8")
            rejected = subprocess.run(
                command,
                cwd=root,
                env=env,
                capture_output=True,
                text=True,
                timeout=60,
                check=False,
            )
            self.assertNotEqual(rejected.returncode, 0)
            self.assertIn("function `unused_probe` is never used", rejected.stderr)
            self.assertNotIn("lost target flags", rejected.stderr)
            source.write_text(prefix + "pub fn used_probe() {}\n", encoding="utf-8")
            accepted = subprocess.run(
                command,
                cwd=root,
                env=env,
                capture_output=True,
                text=True,
                timeout=60,
                check=False,
            )
            self.assertEqual(accepted.returncode, 0, accepted.stdout + accepted.stderr)

    def test_advisory_ignores_match_between_audit_and_deny(self) -> None:
        audit = load_toml(REPO_ROOT / "codex-rs" / ".cargo" / "audit.toml")
        deny = load_toml(REPO_ROOT / "codex-rs" / "deny.toml")
        audit_ids = audit["advisories"]["ignore"]
        deny_ids = [
            entry if isinstance(entry, str) else entry["id"]
            for entry in deny["advisories"]["ignore"]
        ]
        self.assertCountEqual(
            audit_ids,
            deny_ids,
            "cargo audit and cargo deny must use the same advisory exceptions",
        )

    def test_python_launcher_bounds_only_the_capability_probe(self) -> None:
        node = shutil.which("node")
        if node is None:
            self.skipTest("Node is not available")
        harness = r"""
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync(process.argv[1], 'utf8');
const results = [];
for (const timeout of [false, true]) {
  const calls = [], errors = [];
  let exitCode;
  try {
    vm.runInNewContext(source, {
      require: () => ({spawnSync: (command, args, options) => {
        calls.push({command, args, options});
        return timeout ? {error: {code: 'ETIMEDOUT'}} : {status: 0};
      }}),
      process: {argv: ['node', 'run-python.js', 'maintenance.py'], env: {PYTHON: 'chosen-python'}, exit: code => {exitCode = code; throw new Error('exit');}},
      console: {error: message => errors.push(message)},
    });
  } catch (error) { if (error.message !== 'exit') throw error; }
  results.push({calls, errors, exitCode});
}
console.log(JSON.stringify(results));
"""
        result = subprocess.run(
            [node, "-e", harness, str(REPO_ROOT / "scripts" / "run-python.js")],
            capture_output=True,
            text=True,
            check=True,
            timeout=15,
        )
        success, timeout = json.loads(result.stdout)
        self.assertEqual(success["exitCode"], 0)
        self.assertEqual(len(success["calls"]), 2)
        self.assertEqual(success["calls"][0]["options"]["timeout"], 10000)
        self.assertNotIn("timeout", success["calls"][1]["options"])
        self.assertEqual(success["calls"][1]["args"], ["maintenance.py"])
        self.assertEqual(timeout["exitCode"], 1)
        self.assertEqual(len(timeout["calls"]), 1)
        self.assertIn("probe timed out", timeout["errors"][0])

    def test_mixed_changed_scripts_report_uncovered_path(self):
        maintenance = load_root_maintenance_module()
        with mock.patch.object(maintenance, "run") as run:
            self.assertEqual(
                maintenance.main(
                    [
                        "test-python",
                        "--changed",
                        "scripts/readme_toc.py",
                        "--changed",
                        "scripts/unmapped_audit189_helper.py",
                    ]
                ),
                2,
            )
            run.assert_not_called()
        with mock.patch.object(
            maintenance,
            "script_inventory",
            side_effect=AssertionError("broad discovery"),
        ):
            self.assertIn(
                "scripts.test_readme_toc",
                maintenance.test_modules_for_changed_path("scripts/readme_toc.py"),
            )

    def test_explicit_package_bound_allows_nested_codex_rs_package(self):
        from scripts.rust_packages import nearest_package_root

        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)
            package = repo / "codex-rs" / "nested" / "codex-rs"
            package.mkdir(parents=True)
            (package / "Cargo.toml").write_text('[package]\nname = "nested"\n')
            self.assertEqual(
                nearest_package_root(
                    package / "src" / "lib.rs", repo_root=repo, assume_file=True
                ),
                package,
            )

    def test_analyzer_preserves_child_exit_and_streams_progress(self):
        result, calls = self.run_workspace_analyzer(
            "clippy",
            "--workspace",
            "--all-features",
            child_exit=7,
            os_name="Windows_NT",
        )
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertEqual(len(calls), 1)
        self.assertIn("child progress", result.stdout)

    def test_analyzer_keeps_excludes_before_compiler_separator(self):
        result, calls = self.run_workspace_analyzer(
            "clippy",
            "--workspace",
            "--all-features",
            "--",
            "-Dwarnings",
            os_name="Windows_NT",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        self.assertEqual(
            calls[0]["args"][-4:], ["--exclude", "codex-code-mode", "--", "-Dwarnings"]
        )
        self.assertEqual(calls[1]["args"][-2:], ["--", "-Dwarnings"])

    def test_analyzer_respects_explicit_sandbox_exclusion(self):
        for exclusion in (
            ("--exclude", "codex-code-mode"),
            ("--exclude=codex-code-mode",),
        ):
            result, calls = self.run_workspace_analyzer(
                "clippy",
                "--workspace",
                "--all-features",
                *exclusion,
                os_name="Windows_NT",
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(len(calls), 1)

    def test_dead_code_preserves_authoritative_encoded_flags(self):
        result, calls = self.run_workspace_analyzer(
            "dead-code", "--package=codex-core", encoded_flags="--cfg\x1fexisting"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls[0]["encoded"], "--cfg\x1fexisting")
        self.assertEqual(
            calls[0]["args"][-4:], ["--", "-A", "clippy::all", "-Ddead_code"]
        )

    def test_dead_code_package_spellings_remain_package_scoped(self):
        for selection in (
            ["-p", "codex-core"],
            ["--package", "codex-core"],
            ["--package=codex-core"],
            ["-pcodex-core"],
            ["-p=codex-core"],
        ):
            with self.subTest(selection=selection):
                result, calls = self.run_workspace_analyzer("dead-code", *selection)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(len(calls), 1)
                self.assertEqual(
                    calls[0]["args"],
                    [
                        "cargo",
                        "clippy",
                        "--all-targets",
                        *selection,
                        "--",
                        "-A",
                        "clippy::all",
                        "-Ddead_code",
                    ],
                )

    def run_just_recipe(
        self,
        *args: str,
        missing: str = "",
        from_subdirectory: bool = False,
        fail_program: str = "",
        child_exit: int = 0,
    ) -> tuple[subprocess.CompletedProcess[str], list[dict[str, object]]]:
        if os.name != "nt" or not shutil.which("just") or not shutil.which("pwsh"):
            self.skipTest("Windows, just, and pwsh are required for recipe tests")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "codex-rs").mkdir()
            (root / "scripts").mkdir()
            shutil.copyfile(REPO_ROOT / "justfile", root / "justfile")
            shutil.copyfile(
                REPO_ROOT / "scripts" / "common-rust-env.ps1",
                root / "scripts" / "common-rust-env.ps1",
            )
            # Keep real just dispatch and PowerShell argument handling; record
            # external build/sign/publish commands without executing them.
            prefix = r"""
function Record-Call($program, $arguments) {
    @{ program = $program; args = @($arguments); cwd = (Get-Location).Path } |
        ConvertTo-Json -Compress | Add-Content -LiteralPath $env:RECIPE_CALLS
    $global:LASTEXITCODE = if ($program -eq $env:RECIPE_FAIL_PROGRAM) { [int]$env:RECIPE_CHILD_EXIT } else { 0 }
    if ($global:LASTEXITCODE -ne 0) { exit $global:LASTEXITCODE }
}
function python { Record-Call 'python' $args }
function cargo { Record-Call 'cargo' $args }
function cosign { Record-Call 'cosign' $args }
function gh {
    Record-Call 'gh' $args
    if ($args[1] -eq 'view') { 'artifact.zip' }
}
function Get-Command($Name) {
    if ($Name -ne $env:RECIPE_MISSING) {
        Microsoft.PowerShell.Core\Get-Command $Name -ErrorAction SilentlyContinue
    }
}
"""
            (root / "scripts" / "just-shell.py").write_text(
                "import runpy, sys\n"
                f"adapter = runpy.run_path({str(REPO_ROOT / 'scripts' / 'just-shell.py')!r})\n"
                f"raise SystemExit(adapter['run_powershell']({prefix!r} + sys.argv[1], "
                "sys.argv[2], sys.argv[3:]))\n",
                encoding="utf-8",
            )
            release = root / "_build" / "release" / "test-version"
            release.mkdir(parents=True)
            (release / "artifact.zip").write_bytes(b"fixture")
            calls_path = root / "calls.jsonl"
            env = {
                **os.environ,
                "RECIPE_CALLS": str(calls_path),
                "RECIPE_MISSING": missing,
                "RECIPE_FAIL_PROGRAM": fail_program,
                "RECIPE_CHILD_EXIT": str(child_exit),
                "CODEX_RELEASE_CERTIFICATE_IDENTITY": "fixture-identity",
                "CODEX_RELEASE_OIDC_ISSUER": "fixture-issuer",
            }
            if missing == "identity":
                env.pop("CODEX_RELEASE_CERTIFICATE_IDENTITY")
            result = subprocess.run(
                ["just", *args],
                cwd=root / "codex-rs" if from_subdirectory else root,
                env=env,
                text=True,
                encoding="utf-8",
                errors="replace",
                capture_output=True,
                check=False,
                timeout=45,
            )
            calls = (
                [
                    json.loads(line)
                    for line in calls_path.read_text(encoding="utf-8-sig").splitlines()
                ]
                if calls_path.exists()
                else []
            )
            for call in calls:
                call["cwd"] = Path(call["cwd"]).relative_to(root).as_posix()
            return result, calls

    def run_workspace_analyzer(
        self,
        analyzer: str,
        *forwarded_args: str,
        rustflags: str = "",
        os_name: str | None = None,
        encoded_flags: str | None = None,
        child_exit: int = 0,
    ) -> tuple[subprocess.CompletedProcess[str], list[dict[str, object]]]:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is required for workspace analyzer tests")
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_root = Path(temp_dir)
            analyzer_path = temp_root / "cargo-workspace-analyzer.ps1"
            shutil.copyfile(
                REPO_ROOT / "scripts" / "common-rust-env.ps1",
                temp_root / "common-rust-env.ps1",
            )
            shutil.copyfile(
                REPO_ROOT / "scripts" / "cargo-workspace-analyzer.ps1",
                analyzer_path,
            )
            (temp_root / "cargo-lane.ps1").write_text(
                "$Lane = $args[1]\n"
                "$Command = @($args | Select-Object -Skip 2)\n"
                "[ordered]@{ lane = $Lane; args = @($Command); rustflags = $env:RUSTFLAGS; encoded = $env:CARGO_ENCODED_RUSTFLAGS } "
                "| ConvertTo-Json -Compress | Add-Content -LiteralPath "
                "$env:CODEX_ANALYZER_TEST_OUTPUT\n"
                "Write-Output 'child progress'\n"
                f"exit {child_exit}\n",
                encoding="utf-8",
            )
            output_path = temp_root / "calls.jsonl"
            env = {
                **os.environ,
                "RUSTFLAGS": rustflags,
                "CODEX_ANALYZER_TEST_OUTPUT": str(output_path),
            }
            env.pop("CARGO_ENCODED_RUSTFLAGS", None)
            if encoded_flags is not None:
                env["CARGO_ENCODED_RUSTFLAGS"] = encoded_flags
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
        self.assertNotRegex(responses_stream, r"#\[allow\(dead_code\)\]\s*struct Error")

    def test_skills_build_script_requires_bundled_samples(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            executable = root / (
                "skills-build.exe" if os.name == "nt" else "skills-build"
            )
            compile_result = subprocess.run(
                [
                    "rustc",
                    str(REPO_ROOT / "codex-rs/skills/build.rs"),
                    "-o",
                    str(executable),
                ],
                capture_output=True,
                text=True,
                check=False,
                timeout=60,
            )
            self.assertEqual(compile_result.returncode, 0, compile_result.stderr)
            missing = subprocess.run(
                [str(executable)],
                cwd=root,
                capture_output=True,
                text=True,
                check=False,
                timeout=10,
            )
            self.assertNotEqual(missing.returncode, 0)
            self.assertIn(
                "bundled skills directory src/assets/samples is missing", missing.stderr
            )
            (root / "src/assets/samples").mkdir(parents=True)
            present = subprocess.run(
                [str(executable)],
                cwd=root,
                capture_output=True,
                text=True,
                check=False,
                timeout=10,
            )
            self.assertEqual(present.returncode, 0, present.stderr)
            self.assertIn("cargo:rerun-if-changed=src/assets/samples", present.stdout)

    def test_retired_repo_local_harness_has_no_registration(self) -> None:
        features = load_toml(REPO_ROOT / "kd4_features.toml")["features"]
        feature_ids = {feature["id"] for feature in features}
        root_policy = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")
        self.assertFalse((REPO_ROOT / ".codex" / "harness").exists())
        self.assertNotIn("kd4-harness", feature_ids)
        self.assertFalse((REPO_ROOT / ".codex" / "skills" / "kd4-harness").exists())
        self.assertNotIn("skills/kd4-harness", root_policy)
        self.assertNotIn(".codex/harness", root_policy)
        for path in (
            "scripts/workflow_preflight.py",
            "scripts/test_workflow_preflight.py",
        ):
            self.assertFalse((REPO_ROOT / path).exists(), path)

    def test_retired_harness_recipes_are_unavailable(self) -> None:
        just = shutil.which("just")
        if just is None:
            self.skipTest("just is required to check recipe dispatch")
        for recipe in ("workflow-preflight", "workflow-preflight-release"):
            with self.subTest(recipe=recipe):
                result = subprocess.run(
                    [just, "--dry-run", recipe],
                    cwd=REPO_ROOT,
                    text=True,
                    capture_output=True,
                    check=False,
                    timeout=30,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("does not contain recipe", result.stderr)
                self.assertIn(recipe, result.stderr)

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
            parsed_name = name_lines[0].removeprefix("name: ").strip()
            self.assertEqual(parsed_name, skill_dir.name)
            frontmatter_names.append(parsed_name)
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

    def test_source_map_documents_bounded_routing_before_broad_lookup(self) -> None:
        slice_command = (
            "python scripts/source_owners.py slice --owner <owner-id> "
            '--focus "<task description>" --max-relationships 32'
        )

        source_map = (REPO_ROOT / "SOURCEMAP.md").read_text(encoding="utf-8")
        section = source_map.split("## How to use this map\n", 1)[1].split("\n## ", 1)[
            0
        ]
        guidance = " ".join(section.split())
        self.assertIn(slice_command, guidance)
        self.assertIn("Expand truncated or omitted relationships", guidance)
        self.assertIn("Resolve material unknowns", guidance)
        self.assertIn(
            "Use this broad map only when no owner matches",
            guidance,
        )

    def test_agents_desktop_boundary_is_top_level_guidance(self) -> None:
        text = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")

        # Assert the section exists as top-level (H2) guidance and carries the
        # rebuild contract, without pinning it to a line window that breaks
        # whenever earlier sections grow.
        heading = re.search(
            r"(?m)^## Repository identity and runtime boundary\s*$", text
        )
        self.assertIsNotNone(heading)
        section = re.split(r"(?m)^## ", text[heading.end() :], maxsplit=1)[0]
        self.assertIn(
            "Source changes become Desktop-visible only after rebuilding", section
        )
        self.assertIn("replacing or updating the local binary", section)

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

    def test_windows_installer_validates_release_repository_before_effects(
        self,
    ) -> None:
        ps = powershell()
        if ps is None:
            self.skipTest("PowerShell is required for the Windows installer test")
        installer = REPO_ROOT / "scripts" / "install" / "install.ps1"
        for repository, accepted in [
            ("ikhdark/KD4", True),
            ("a/.github", True),
            ("my-org/repo_name-1.2", True),
            ("../..", False),
            ("owner/.", False),
            ("owner/..", False),
            ("-x/-y", False),
            ("owner/repo/extra", False),
            ("owner/repo\nextra", False),
        ]:
            with self.subTest(repository=repository):
                # Execute the actual parameter/validation preamble. Stop before
                # function definitions so accepted cases cannot install anything.
                command = (
                    "$tokens=$null; $errors=$null; "
                    f"$ast=[Management.Automation.Language.Parser]::ParseFile({ps_single_quote(installer)},[ref]$tokens,[ref]$errors); "
                    "$first=$ast.FindAll({param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst]},$true)[0]; "
                    "$preamble=$ast.Extent.Text.Substring(0,$first.Extent.StartOffset); "
                    "$probe=[scriptblock]::Create($preamble + '; $ReleaseApiBase'); "
                    f"try {{ & $probe -ReleaseRepository {ps_single_quote(repository)} }} "
                    "catch { [Console]::Error.WriteLine($_.Exception.Message); exit 7 }"
                )
                completed = subprocess.run(
                    [ps, "-NoProfile", "-NonInteractive", "-Command", command],
                    text=True,
                    capture_output=True,
                    check=False,
                )
                self.assertEqual(
                    completed.returncode, 0 if accepted else 7, completed.stderr
                )
                if accepted:
                    self.assertEqual(
                        completed.stdout.strip(),
                        f"https://api.github.com/repos/{repository}/releases",
                    )
                else:
                    self.assertIn("Invalid Codex release repository:", completed.stderr)
                    self.assertEqual(completed.stdout, "")

    def test_windows_installer_warns_when_native_uninstall_fails(self) -> None:
        ps = powershell()
        if ps is None:
            self.skipTest("PowerShell is required for the Windows installer test")
        installer = REPO_ROOT / "scripts" / "install" / "install.ps1"
        for manager, exit_code, approved in [
            ("npm", 23, True),
            ("bun", 23, True),
            ("npm", 0, True),
            ("bun", 0, True),
            ("npm", 23, False),
        ]:
            with self.subTest(manager=manager, exit_code=exit_code, approved=approved):
                command = (
                    "$ErrorActionPreference='Stop'; $tokens=$null; $errors=$null; "
                    f"$ast=[Management.Automation.Language.Parser]::ParseFile({ps_single_quote(installer)},[ref]$tokens,[ref]$errors); "
                    "$fn=$ast.FindAll({param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq 'Maybe-HandleConflictingInstall'},$true)[0]; "
                    "Invoke-Expression $fn.Extent.Text; "
                    "$script:calledArgs=@(); $warnings=[Collections.Generic.List[string]]::new(); "
                    "function Write-Step {}; function Write-WarningStep { param($Message) $warnings.Add($Message) }; "
                    f"function Prompt-YesNo {{ return ${str(approved).lower()} }}; "
                    f"function {manager} {{ $script:calledArgs=@($args); "
                    f"& {ps_single_quote(Path(sys.executable))} -c 'import sys; sys.exit({exit_code})' }}; "
                    f"Maybe-HandleConflictingInstall -Conflict ([pscustomobject]@{{Manager='{manager}'}}); "
                    "@{warnings=@($warnings.ToArray()); arguments=$script:calledArgs; completed=$true} | ConvertTo-Json -Compress"
                )
                completed = subprocess.run(
                    [ps, "-NoProfile", "-NonInteractive", "-Command", command],
                    text=True,
                    capture_output=True,
                    check=False,
                )
                self.assertEqual(completed.returncode, 0, completed.stderr)
                result = json.loads(completed.stdout)
                self.assertTrue(result["completed"])
                self.assertEqual(
                    result["arguments"],
                    [
                        "remove" if manager == "bun" else "uninstall",
                        "-g",
                        "@openai/codex",
                    ]
                    if approved
                    else [],
                )
                if approved and exit_code:
                    self.assertEqual(
                        result["warnings"],
                        [
                            f"Failed to uninstall the existing {manager}-managed Codex: "
                            f"{manager} exited with code {exit_code}. Continuing with the standalone install."
                        ],
                    )
                elif approved:
                    self.assertEqual(result["warnings"], [])
                else:
                    self.assertEqual(
                        result["warnings"],
                        [
                            f"Leaving the existing {manager}-managed Codex installed. PATH order will determine which codex runs."
                        ],
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

        source_paths = []
        for root in root_maintenance.SCRIPT_AUDIT_ROOTS:
            for directory, dirs, files in os.walk(root):
                dirs[:] = [
                    name for name in dirs if name not in {".venv", "__pycache__"}
                ]
                source_paths.extend(
                    Path(directory) / name for name in files if name.endswith(".py")
                )
        expected_ruff_targets = sorted(
            path.relative_to(REPO_ROOT).as_posix() for path in source_paths
        )
        expected_unittest_targets = sorted(
            path.relative_to(REPO_ROOT).with_suffix("").as_posix().replace("/", ".")
            if root_maintenance.SCRIPTS_ROOT in path.parents
            else path.relative_to(REPO_ROOT).as_posix()
            for path in source_paths
            if path.name.startswith("test_")
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
                    "scripts/generated_output_lock.py",
                    "scripts/kd4_model_attempt_analysis.py",
                ],
            ),
            [
                "scripts.test_dev_environment",
                "scripts.test_kd4_perf_snapshot",
            ],
        )

    def test_python_sdk_gate_runs_lint_and_tests(self) -> None:
        just = shutil.which("just")
        if just is None:
            self.skipTest("just is required to inspect the SDK recipe")
        result = subprocess.run(
            [just, "--dry-run", "sdk-python-check"],
            cwd=REPO_ROOT,
            text=True,
            capture_output=True,
            check=False,
            timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        commands = result.stdout + result.stderr
        self.assertIn("--group dev ruff check .", commands)
        self.assertIn("--group dev pytest", commands)

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

    def test_windows_nextest_setup_isolates_desktop_codex_environment(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            nextest_env = Path(temp) / "nextest.env"
            env = {
                name: value
                for name, value in os.environ.items()
                if not name.startswith("CODEX_")
            }
            env["NEXTEST_ENV"] = str(nextest_env)
            env["CODEX_HOME"] = str(Path(temp) / "desktop-home")
            env["CODEX_PERMISSION_PROFILE"] = ":danger-full-access"
            env["CODEX_SQLITE_HOME"] = str(Path(temp) / "desktop-sqlite")

            result = subprocess.run(
                [sys.executable, "codex-rs/scripts/nextest_windows_stack.py"],
                cwd=REPO_ROOT,
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                nextest_env.read_text(encoding="utf-8"),
                "CODEX_HOME=\n"
                "CODEX_PERMISSION_PROFILE=\n"
                "CODEX_SQLITE_HOME=\n"
                "RUST_MIN_STACK=8388608\n",
            )

    def test_root_maintenance_does_not_route_retired_task_continuity_paths(
        self,
    ) -> None:
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
            if path.suffix
            != ".rs"  # Platform library modules are not retired deployment assets.
            and retired_asset_pattern.search(
                relative_path := path.relative_to(REPO_ROOT).as_posix()
            )
        )
        self.assertEqual(retired_assets, [])

    def test_windows_distribution_keeps_library_platform_dependencies_gated(
        self,
    ) -> None:
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

        # The product ships on Windows; libraries may retain foreign-platform
        # implementations. Ask Cargo which dependencies each shipped target
        # actually selects instead of banning cfg syntax across every source.
        for target in sorted(deny["graph"]["targets"]):
            with self.subTest(target=target):
                result = subprocess.run(
                    [
                        "cargo",
                        "metadata",
                        "--offline",
                        "--locked",
                        "--format-version",
                        "1",
                        "--filter-platform",
                        target,
                    ],
                    cwd=REPO_ROOT / "codex-rs",
                    capture_output=True,
                    text=True,
                    encoding="utf-8",
                    timeout=60,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                metadata = json.loads(result.stdout)
                packages = {p["id"]: p["name"] for p in metadata["packages"]}
                dependencies = {
                    packages[node["id"]]: {packages[d["pkg"]] for d in node["deps"]}
                    for node in metadata["resolve"]["nodes"]
                }
                self.assertIn("windows-sys", dependencies["codex-http-client"])
                self.assertNotIn(
                    "system-configuration", dependencies["codex-http-client"]
                )
                self.assertIn("winapi", dependencies["codex-utils-pty"])
                self.assertNotIn("close_fds", dependencies["codex-utils-pty"])

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

        self.assertEqual(
            calls,
            [
                (
                    "uv",
                    "run",
                    "--frozen",
                    "--project",
                    "scripts",
                    "ruff",
                    "check",
                    "scripts/root_maintenance.py",
                ),
                (
                    "uv",
                    "run",
                    "--frozen",
                    "--project",
                    "scripts",
                    "python",
                    "-m",
                    "unittest",
                    "scripts.test_build_tooling_policy",
                    "-v",
                ),
            ],
        )

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

    @unittest.skipUnless(sys.platform == "win32", "Windows native launcher")
    def test_codex_cli_launcher_handles_native_startup_failures(self) -> None:
        node = shutil.which("node")
        if node is None:
            self.skipTest("node is not available")
        target = subprocess.check_output(
            [node, "-p", 'process.platform + "-" + process.arch'], text=True
        ).strip()
        with tempfile.TemporaryDirectory(prefix="codex-launcher-") as temp_dir:
            root = Path(temp_dir)
            launcher = root / "bin" / "codex.js"
            launcher.parent.mkdir()
            shutil.copy2(REPO_ROOT / "codex-cli" / "bin" / "codex.js", launcher)
            manifest = root / "package.json"
            manifest.write_text(
                json.dumps(
                    {
                        "type": "module",
                        "codexNativeTargets": {
                            target: {
                                "targetTriple": "fixture",
                                "package": "@codex-test/native",
                                "binary": "native.exe",
                            }
                        },
                    }
                ),
                encoding="utf-8",
            )

            def run(*args: str) -> subprocess.CompletedProcess[str]:
                return subprocess.run(
                    [node, *args], input="", capture_output=True, text=True, timeout=15
                )

            native = root / "vendor" / "fixture" / "bin" / "native.exe"
            native.parent.mkdir(parents=True)
            for invalid_executable in [False, True]:
                with self.subTest(invalid_executable=invalid_executable):
                    if invalid_executable:
                        native.write_bytes(b"not a Windows executable")
                    result = run(str(launcher))
                    self.assertEqual(result.returncode, 1, result)
                    self.assertIn("Reinstall this KD4 package", result.stderr)
                    self.assertIn(
                        "Unable to start"
                        if invalid_executable
                        else "Missing optional dependency",
                        result.stderr,
                    )
                    self.assertNotIn("\n    at ", result.stderr)

            # Use a real native process to verify argument and exit-code forwarding.
            shutil.copy2(node, native)
            result = run(
                str(launcher),
                "-e",
                'process.stdout.write("forwarded"); process.exitCode = 7',
            )
            self.assertEqual(result.returncode, 7, result)
            self.assertEqual(result.stdout, "forwarded")
            result = run(
                "--input-type=module",
                "-e",
                f"await import({json.dumps(launcher.as_uri())})",
            )
            self.assertEqual(result.returncode, 0, result)
            self.assertEqual(result.stderr, "")

            manifest.write_text(json.dumps({"type": "module"}), encoding="utf-8")
            result = run(str(launcher))
            self.assertEqual(result.returncode, 1, result)
            self.assertIn("Unsupported platform", result.stderr)
            self.assertNotIn("\n    at ", result.stderr)

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

    def test_gate_for_routes_repository_paths_from_another_workdir(self) -> None:
        just = shutil.which("just")
        if just is None:
            self.skipTest("just is unavailable")
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.run(
                [
                    just,
                    "--justfile",
                    str(REPO_ROOT / "justfile"),
                    "gate-for",
                    "scripts/rust_test_runner.py",
                ],
                cwd=directory,
                capture_output=True,
                text=True,
                check=False,
                timeout=30,
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        routes = json.loads(result.stdout)
        self.assertEqual(routes["status"], "declared")
        self.assertEqual(routes["unowned_paths"], [])
        self.assertEqual(
            [route["owner"] for route in routes["validation"]],
            ["rust-test-routing", "rust-test-routing"],
        )
        self.assertEqual(
            [route["argv"] for route in routes["validation"]],
            [
                ["python", "-m", "unittest", "scripts.test_rust_test_runner"],
                ["just", "core-test-manifest-check"],
            ],
        )

    def test_hooks_schema_check_selects_the_fixture_comparison(self) -> None:
        result = subprocess.run(
            ["just", "--dry-run", "hooks-schema-check"],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            (result.stdout + result.stderr).strip(),
            "cargo nextest run --profile local --no-tests=fail -p codex-hooks --lib "
            "-E 'test(=schema::tests::generated_hook_schemas_match_fixtures)'",
        )

    @unittest.skipUnless(sys.platform == "win32", "Windows protobuf wrapper")
    def test_protos_check_runs_both_freshness_checks(self) -> None:
        result = subprocess.run(
            ["just", "--dry-run", "protos-check"],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        commands = (result.stdout + result.stderr).strip().splitlines()
        self.assertEqual(len(commands), 2, commands)
        self.assertIn("config\\scripts\\generate-proto.ps1", commands[0])
        self.assertTrue(commands[0].endswith(" -Check"), commands[0])
        self.assertIn(
            "-p codex-exec-server --example generate-relay-proto", commands[1]
        )
        self.assertTrue(commands[1].endswith(" -- --check"), commands[1])

    def test_justfile_only_exposes_canonical_developer_tooling_recipes(self) -> None:
        justfile = "\n" + (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        package = json.loads((REPO_ROOT / "package.json").read_text(encoding="utf-8"))

        self.assertIn("\ncargo-lane-isolated-home lane *args:", justfile)
        self.assertIn("\nconfig-schema-check:", justfile)
        self.assertIn("\nconfig-schema-regenerate owner:", justfile)
        self.assertIn("\napp-server-schema-check *args:", justfile)
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
            "SOURCEMAP.md": "app-server-schema-regenerate <owner>",
            "codex-rs/app-server-protocol/tests/schema_fixtures.rs": (
                "app-server-schema-regenerate <owner>"
            ),
            "codex-rs/core/src/config/schema.md": "config-schema-regenerate <owner>",
            "codex-rs/core/src/config/schema_tests.rs": (
                "config-schema-regenerate <owner>"
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
            "-C target-cpu=native --cfg existing",
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
        rejected = (
            (),
            ("--quiet",),
            ("--tests",),
            ("-p",),
            ("--package=",),
            ("--", "-p", "codex-cli"),
            ("-p", "codex-cli", "--workspace"),
            ("--all", "--package=codex-cli"),
        )
        accepted = (
            ("-p", "codex-cli"),
            ("--package", "codex-cli"),
            ("--package=codex-cli",),
            ("-pcodex-cli",),
            ("-p=codex-cli",),
            ("-p=codex-cli@0.0.0",),
            ("-p=codex-utils-*",),
            ("-p", "codex-cli", "-p", "codex-utils-pty", "--", "-Dwarnings"),
        )
        for recipe in ("clippy", "fix"):
            for args in rejected:
                with self.subTest(recipe=recipe, args=args):
                    result, calls = self.run_just_recipe(recipe, *args)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn("Pass a package selection", result.stderr)
                    self.assertEqual(
                        calls, [], "unscoped invocation reached the lane runner"
                    )
            for args in accepted:
                with self.subTest(recipe=recipe, args=args):
                    result, calls = self.run_just_recipe(recipe, *args)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(len(calls), 1)
                    command = calls[0]["args"]
                    self.assertEqual(
                        # PowerShell consumes the literal -- when calling the
                        # recording function instead of a native executable.
                        command[1:6],
                        ["run-lane", "--lane", "auto", "cargo", "clippy"],
                    )
                    self.assertEqual(command[-len(args) :], list(args))
                    self.assertEqual("--fix" in command, recipe == "fix")

    def test_release_prerequisites_fail_before_preparation(self) -> None:
        for recipe, missing, diagnostic in (
            ("sign-codex-release", "cosign", "cosign is required"),
            (
                "publish-codex-release",
                "identity",
                "Set CODEX_RELEASE_CERTIFICATE_IDENTITY",
            ),
            ("publish-codex-release", "gh", "gh is required"),
            ("publish-codex-release", "cosign", "cosign is required"),
        ):
            with self.subTest(recipe=recipe, missing=missing):
                result, calls = self.run_just_recipe(
                    recipe, "test-version", missing=missing
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(diagnostic, result.stderr)
                self.assertEqual(
                    calls, [], "missing prerequisite allowed preparation or signing"
                )
        result, calls = self.run_just_recipe("publish-codex-release", "test-version")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            [call["program"] for call in calls],
            ["python", "python", "cosign", "cosign", "gh", "gh"],
        )
        self.assertEqual(
            [call["args"][0] for call in calls[2:4]], ["sign-blob", "verify-blob"]
        )
        self.assertEqual(
            [call["args"][:2] for call in calls[4:]],
            [["release", "create"], ["release", "view"]],
        )

    def test_app_server_runtime_check_batches_the_existing_test_selections(
        self,
    ) -> None:
        result, calls = self.run_just_recipe("app-server-runtime-check")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([call["program"] for call in calls], ["python", "cargo"])
        self.assertEqual(
            calls[0]["args"][1:],
            [
                "run-lane",
                "--lane",
                "core-tests",
                "just",
                "_core-gate-reserved",
                "app-server-command-exec",
                "app-server-process-exec",
                "app-server-thread-status",
            ],
        )
        self.assertEqual(calls[1]["args"], ["check", "-p", "codex-app-server"])

    def test_release_tooling_recipe_runs_from_repository_root(self) -> None:
        result, calls = self.run_just_recipe(
            "test-release-tooling", from_subdirectory=True
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0]["cwd"], ".")
        self.assertEqual(
            calls[0]["args"],
            [
                "-m",
                "unittest",
                "scripts.test_build_tooling_policy",
                "scripts.test_check_blob_size",
                "scripts.test_stage_npm_packages",
            ],
        )

    def test_windows_process_suite_cannot_silently_skip_required_coverage(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        sandbox_tests = (
            REPO_ROOT / "codex-rs/windows-sandbox-rs/src/unified_exec/tests.rs"
        ).read_text(encoding="utf-8")
        pty_tests = (REPO_ROOT / "codex-rs/utils/pty/src/windows_tests.rs").read_text(
            encoding="utf-8"
        )

        self.assertIn("[windows]\ntest-windows-sandbox-processes *args:", justfile)
        sandbox_recipe = justfile.split("test-windows-sandbox-processes *args:", 1)[
            1
        ].split("\n\n", 1)[0]
        self.assertIn(
            "just core-gate windows-process windows-sandbox-core-exec", sandbox_recipe
        )
        manifest = load_toml(REPO_ROOT / "codex-rs/.config/kd4-rust-tests.toml")
        steps = manifest["gates"]["windows-process"]["steps"]
        self.assertEqual(
            {test for step in steps for test in step["tests"]},
            {
                "tests::windows_tests::terminate_kills_descendants_for_best_effort_pipe_and_atomic_conpty",
                "tests::windows_tests::normal_exit_preserves_descendants_for_pipe_and_conpty",
                "tests::windows_tests::conpty_delivers_input_to_foreground_children",
                "tests::windows_tests::conpty_ctrl_c_interrupts_powershell_foreground_child",
                "tests::windows_tests::required_process_test_prerequisites_report_unverified_coverage",
                "unified_exec::tests::legacy_capture_cancellation_terminates_descendants_without_timeout",
                "windows_impl::tests::process_wait_failure_is_not_treated_as_exit",
                "win::tests::controlling_ipc_eof_terminates_process_tree",
                "win::tests::invalid_process_wait_is_not_treated_as_exit",
            },
        )
        self.assertIn("CODEX_REQUIRE_WINDOWS_SANDBOX_PROCESS_TESTS", justfile)
        # Sandbox tests require their prerequisite unconditionally. The flag
        # controls only PTY tests that can also run as ordinary developer tests.
        self.assertIn("fn require_legacy_process_sandbox()", sandbox_tests)
        self.assertIn("CODEX_REQUIRE_WINDOWS_SANDBOX_PROCESS_TESTS", pty_tests)
        self.assertIn(
            "Windows sandbox process test prerequisite unavailable",
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
        # Pin the child's stderr encoding: on a legacy code page Python falls
        # back to backslashreplace and reports the argument as an escape
        # sequence, which says nothing about whether argv survived.
        rejected = subprocess.run(
            ["just", "check-kd4-features", unicode_argument],
            cwd=REPO_ROOT,
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
            env={**os.environ, "PYTHONIOENCODING": "utf-8"},
        )
        self.assertEqual(rejected.returncode, 2, rejected.stderr)
        self.assertIn(unicode_argument, rejected.stderr)

    def test_dependency_audit_prerequisite_runs_and_gates_cargo_audit(self) -> None:
        for fail_program in ("", "python", "cargo"):
            with self.subTest(fail_program=fail_program):
                result, calls = self.run_just_recipe(
                    "deps-audit",
                    fail_program=fail_program,
                    child_exit=7,
                )
                if fail_program:
                    self.assertNotEqual(result.returncode, 0, result.stderr)
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(
                    [call["program"] for call in calls],
                    ["python"] if fail_program == "python" else ["python", "cargo"],
                )
                self.assertEqual(
                    calls[0]["args"],
                    [
                        "-m",
                        "unittest",
                        "scripts.test_build_tooling_policy.BuildToolingPolicyTest.test_advisory_ignores_match_between_audit_and_deny",
                    ],
                )
                if fail_program != "python":
                    self.assertEqual(calls[1]["args"], ["audit"])

    def test_install_rejects_old_powershell_before_running_setup(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        body = justfile.split("\ninstall:\n", 1)[1].split("\n\n", 1)[0]
        body = "\n".join(line[4:] for line in body.splitlines()[1:])
        body = body.replace("{{ python }}", "python").replace(
            "{{ justfile_directory() }}", str(REPO_ROOT)
        )
        prefix = r"""
function Test-PwshVersion { $env:TEST_PWSH_VERSION }
function Get-Command { @{ Source = 'Test-PwshVersion' } }
function Record-Setup($program, $arguments) {
    @{program=$program; args=@($arguments)} | ConvertTo-Json -Compress |
        Add-Content -LiteralPath $env:TEST_SETUP_CALLS
    $global:LASTEXITCODE = 0
}
function rustup { Record-Setup 'rustup' $args }
function cargo { Record-Setup 'cargo' $args }
function python { Record-Setup 'python' $args; 'test-toolchain' }
"""
        for version, accepted in (
            ("7.4.9", False),
            ("7.5", True),
            ("7.5.2", True),
            ("7.6", True),
        ):
            with (
                self.subTest(version=version),
                tempfile.TemporaryDirectory() as directory,
            ):
                script = Path(directory) / "setup.ps1"
                calls = Path(directory) / "calls.jsonl"
                script.write_text(prefix + body, encoding="utf-8")
                result = subprocess.run(
                    [powershell(), "-NoProfile", "-File", str(script)],
                    env={
                        **os.environ,
                        "TEST_PWSH_VERSION": version,
                        "TEST_SETUP_CALLS": str(calls),
                    },
                    capture_output=True,
                    text=True,
                    encoding="utf-8",
                    timeout=20,
                )
                self.assertEqual(result.returncode, 0 if accepted else 2, result.stderr)
                observed = (
                    [
                        json.loads(line)
                        for line in calls.read_text(encoding="utf-8-sig").splitlines()
                    ]
                    if calls.exists()
                    else []
                )
                self.assertEqual(
                    [call["program"] for call in observed],
                    ["rustup", "python", "rustup", "cargo"] if accepted else [],
                )
                if accepted:
                    self.assertEqual(observed[-1]["args"], ["fetch", "--locked"])
                    self.assertEqual(
                        observed[2]["args"],
                        [
                            "toolchain",
                            "install",
                            "test-toolchain",
                            "--profile",
                            "minimal",
                            "--component",
                            "rustfmt",
                        ],
                    )
                else:
                    self.assertIn("7.5 or newer is required", result.stderr)

    def test_release_packaging_policy_is_explicit_and_pinned(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        cargo_manifest = (REPO_ROOT / "codex-rs" / "Cargo.toml").read_text(
            encoding="utf-8"
        )
        npm_manifest = json.loads(
            (REPO_ROOT / "codex-cli" / "package.json").read_text(encoding="utf-8")
        )

        self.assertIn('rust_parallelism := "8"', justfile)
        self.assertIn('$requiredPwshVersion = [version]"7.5"', justfile)
        self.assertIn("\ntest-release-tooling:\n", justfile)
        self.assertIn(
            "scripts.test_build_tooling_policy scripts.test_check_blob_size scripts.test_stage_npm_packages",
            justfile,
        )
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

    def test_compile_and_test_recipes_share_automatic_package_lanes(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        for recipe in ("test", "test-fast", "test-compile"):
            body = justfile.split(f"\n{recipe} *args:\n", 1)[1].split("\n\n", 1)[0]
            self.assertIn(
                'rust_build_status.py" run-lane --lane auto -- cargo nextest run',
                body,
            )
            self.assertIn("@forwarded_args", body)

    def test_gate_recipes_forward_all_requested_gates(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        for recipe, command in (
            ("core-gate", "-- just _core-gate-reserved @forwarded_args"),
            ("_core-gate-reserved", "run-gate @forwarded_args"),
        ):
            body = justfile.split(f"\n{recipe} +gates:\n", 1)[1].split("\n\n", 1)[0]
            self.assertIn("$forwarded_args = @($args | Select-Object -Skip 1)", body)
            self.assertIn(command, body)

    def test_high_contention_just_recipes_use_cargo_lanes_on_windows(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        for command in (
            "cargo clippy --fix --tests --allow-dirty @forwarded_args",
            "cargo clippy --tests @forwarded_args",
            "fix-workspace *args:",
            "clippy-workspace *args:",
            "Pass a package selection (-p/--package) to 'just fix'",
            "Pass a package selection (-p/--package) to 'just clippy'",
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
        manifest = load_toml(REPO_ROOT / "codex-rs" / ".config" / "kd4-rust-tests.toml")

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
            self.assertIn('rust_test_runner.py" _guard-generic --', body, recipe)

        # The named recipes replace the removed helper-inference recipes.
        for recipe in (
            "core-test target *args:",
            "core-test-fast target *args:",
            "core-test-lane target *args:",
            "_core-test-reserved profile target *args:",
            "core-gate +gates:",
            "_core-gate-reserved +gates:",
            "core-test-parity legacy *args:",
            "_core-parity-reserved legacy *args:",
            "core-test-list:",
            "core-test-manifest-check:",
        ):
            self.assertIn(recipe, justfile)

        # Every recipe that compiles a named target or gate reserves a Cargo
        # lane first: a build sharing codex-rs/target would otherwise invalidate
        # the whole graph between runs.
        for recipe, reserved in (
            ("core-test target *args:", "just _core-test-reserved local"),
            ("core-test-fast target *args:", "just _core-test-reserved fast"),
            ("core-test-lane target *args:", "just _core-test-reserved fast"),
            ("core-gate +gates:", "just _core-gate-reserved"),
            ("core-test-parity legacy *args:", "just _core-parity-reserved"),
        ):
            body = justfile.split(f"\n{recipe}\n", 1)[1].split("\n\n", 1)[0]
            self.assertIn('rust_build_status.py" run-lane --lane', body, recipe)
            self.assertIn(reserved, body, recipe)
            self.assertNotIn('rust_test_runner.py" run-', body, recipe)

        # Shared-lane recipes must name the one lane variable, and only the
        # explicit per-target escape hatch may open a lane of its own.
        for recipe in (
            "core-test target *args:",
            "core-test-fast target *args:",
            "core-gate +gates:",
            "core-test-parity legacy *args:",
        ):
            body = justfile.split(f"\n{recipe}\n", 1)[1].split("\n\n", 1)[0]
            self.assertIn('--lane "{{ core_test_lane }}"', body, recipe)
        lane_body = justfile.split("\ncore-test-lane target *args:\n", 1)[1].split(
            "\n\n", 1
        )[0]
        self.assertIn('--lane "{{ target }}"', lane_body)

        # Each reserved body must consume the reservation instead of falling
        # back to the default target directory.
        for reserved in (
            "_core-test-reserved profile target *args:",
            "_core-gate-reserved +gates:",
            "_core-parity-reserved legacy *args:",
        ):
            body = justfile.split(f"\n{reserved}\n", 1)[1].split("\n\n", 1)[0]
            self.assertIn("$target_dir = $env:CODEX_CARGO_LANE_TARGET_DIR", body)
            self.assertIn("missing Cargo lane reservation", body)
            self.assertIn(
                'rust_test_runner.py" --target-dir $target_dir', body, reserved
            )
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
            "config-schema-protocol",
            "windows-sandbox-core-exec",
        ):
            self.assertRegex(justfile, rf"just core-gate [^\n]*\b{gate}\b")
            self.assertIn(gate, manifest["gates"])

        thread_status = justfile.split("_app-server-thread-status-tests:", 1)[1].split(
            "\n\n", 1
        )[0]
        self.assertIn("just core-gate app-server-thread-status", thread_status)
        self.assertEqual(
            manifest["gates"]["app-server-thread-status"]["steps"][0]["tests"],
            [
                "thread_status::tests::stale_guards_cannot_clear_requests_from_a_new_lifecycle",
                "thread_status::tests::stale_guard_drop_does_not_reload_removed_or_shutdown_thread",
            ],
        )

    def test_core_test_recipes_forward_only_the_caller_arguments(self) -> None:
        # `set positional-arguments` puts the shell's own argument first, then
        # one slot per named parameter, ahead of the variadic tail. Skipping the
        # wrong count silently drops a caller's filter or forwards the recipe's
        # own profile or target name as one, so pin it per recipe signature.
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        signature = re.compile(
            r"^(core-test|core-test-fast|core-test-lane|core-test-parity"
            r"|core-gate|_core-test-reserved|_core-gate-reserved"
            r"|_core-parity-reserved)"
            r"((?: [^:\n]*)?):[ \t]*$"
        )
        skip = re.compile(r"\$args \| Select-Object -Skip (\d+)")
        checked: dict[str, int] = {}
        current: tuple[str, int] | None = None
        for line in justfile.splitlines():
            match = signature.match(line)
            if match is not None:
                named = [
                    token
                    for token in match.group(2).split()
                    if not token.startswith(("*", "+"))
                ]
                current = (match.group(1), len(named))
                continue
            found = skip.search(line)
            if found is None or current is None:
                continue
            recipe, named_count = current
            checked[recipe] = int(found.group(1))
            self.assertEqual(int(found.group(1)), named_count + 1, recipe)
            current = None
        self.assertEqual(
            checked,
            {
                "core-test": 2,
                "core-test-fast": 2,
                "core-test-lane": 2,
                "core-test-parity": 2,
                # A `+`/`*` variadic occupies no slot of its own.
                "core-gate": 1,
                "_core-test-reserved": 3,
                "_core-gate-reserved": 1,
                "_core-parity-reserved": 2,
            },
        )

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
