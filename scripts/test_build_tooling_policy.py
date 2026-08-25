#!/usr/bin/env python3

import contextlib
import io
import json
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


class BuildToolingPolicyTest(unittest.TestCase):
    def test_build_info_script_uses_upstream_git_metadata_fallbacks(
        self,
    ) -> None:
        text = (REPO_ROOT / "codex-rs" / "build_info.rs").read_text(encoding="utf-8")
        self.assertIn('args(["status", "--porcelain"])', text)
        self.assertIn("git_dirty(&workspace_root)", text)
        self.assertIn(
            'cargo:rerun-if-changed={}", git_dir.join("index").display()', text
        )
        self.assertNotIn("SystemTime::now", text)
        self.assertIn('workspace_root.join("build_info.rs").display()', text)

    def test_build_info_scripts_emit_metadata_without_non_windows_linking(self) -> None:
        app_server_build = (
            REPO_ROOT / "codex-rs" / "app-server" / "build.rs"
        ).read_text(encoding="utf-8")
        cli_build = (REPO_ROOT / "codex-rs" / "cli" / "build.rs").read_text(
            encoding="utf-8"
        )

        self.assertIn('#[path = "../build_info.rs"]', app_server_build)
        self.assertIn("build_info::emit();", app_server_build)
        self.assertIn('#[path = "../build_info.rs"]', cli_build)
        self.assertIn("build_info::emit();", cli_build)
        self.assertNotIn("cargo:rustc-link-arg=-ObjC", cli_build)

    def test_skills_build_script_requires_bundled_samples(self) -> None:
        text = (REPO_ROOT / "codex-rs" / "skills" / "build.rs").read_text(
            encoding="utf-8"
        )

        self.assertIn('let samples_dir = Path::new("src/assets/samples");', text)
        self.assertIn("if !samples_dir.exists()", text)

    def test_retired_repo_local_harness_skill_has_no_registration(self) -> None:
        features = load_toml(REPO_ROOT / "kd4_features.toml")["features"]
        feature_ids = {feature["id"] for feature in features}
        workspace_policy = (REPO_ROOT / ".codex" / "AGENTS.md").read_text(
            encoding="utf-8"
        )
        harness_workflow = (REPO_ROOT / ".codex" / "harness" / "workflow.md").read_text(
            encoding="utf-8"
        )

        self.assertNotIn("kd4-harness", feature_ids)
        self.assertFalse((REPO_ROOT / ".codex" / "skills" / "kd4-harness").exists())
        self.assertNotIn("skills/kd4-harness", workspace_policy)
        self.assertNotIn("kd4-harness", harness_workflow)

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
        self.assertIn("\n## Desktop app boundary\n", text)
        section = text.split("\n## Desktop app boundary\n", 1)[1]
        section = section.split("\n## ", 1)[0]
        self.assertIn("Source edits here do not hot-apply", section)
        self.assertIn("rebuilding and updating or replacing", section)

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

    def test_agents_scripts_policy_is_nested_and_discoverable(self) -> None:
        root_text = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")
        scripts_text = (REPO_ROOT / "scripts" / "AGENTS.md").read_text(encoding="utf-8")

        self.assertIn("`scripts/AGENTS.md`", root_text)
        self.assertIn("# Scripts Policy", scripts_text)
        self.assertIn("Root maintenance commands", scripts_text)
        self.assertIn("root_maintenance.py", scripts_text)

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
            ):
                path = windows_package / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.touch()
            (windows_package / "codex-package.json").write_text(
                json.dumps(
                    {
                        "layoutVersion": 1,
                        "version": "1.2.3",
                        "target": "x86_64-pc-windows-msvc",
                        "variant": "codex",
                        "entrypoint": "bin/codex.exe",
                        "resourcesDir": "codex-resources",
                        "pathDir": "codex-path",
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
                    "$function = $ast.FindAll({ param($node) "
                    "$node -is [System.Management.Automation.Language.FunctionDefinitionAst] "
                    "-and $node.Name -eq 'Test-PackageContentsAreComplete' }, $true); "
                    "Invoke-Expression $function[0].Extent.Text; "
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
            complete_windows = run_powershell_probe(True)
            self.assertEqual(
                complete_windows.returncode,
                0,
                f"stdout:\n{complete_windows.stdout}\nstderr:\n{complete_windows.stderr}",
            )

    def test_windows_installer_parses_the_first_nonempty_version_line(self) -> None:
        powershell_installer = (
            REPO_ROOT / "scripts" / "install" / "install.ps1"
        ).read_text(encoding="utf-8")

        self.assertIn("$versionLine = @($versionOutput)", powershell_installer)
        self.assertIn("[regex]::Match($versionLine", powershell_installer)
        self.assertNotIn("$versionOutput -match", powershell_installer)

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

    def test_root_maintenance_script_inventory_covers_owned_script_roots(
        self,
    ) -> None:
        root_maintenance = load_root_maintenance_module()

        expected_kinds = {
            ".codex/environments/setup.py": "python",
            ".codex/hooks/task-continuity.ps1": "powershell",
            "codex-cli/scripts/build_npm_package.py": "python",
            "codex-rs/app-server-test-client/scripts/live_elicitation_hold.ps1": "powershell",
            "codex-rs/config/scripts/generate-proto.ps1": "powershell",
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

        forbidden_cfg_fragments = (
            "cfg!(windows)",
            "cfg!(unix)",
            'target_os = "',
            'target_family = "',
            "#[cfg(windows)]",
            "#[cfg(unix)]",
        )
        violations: list[str] = []
        for rust_path in (REPO_ROOT / "codex-rs").rglob("*.rs"):
            if "target" in rust_path.parts:
                continue
            text = rust_path.read_text(encoding="utf-8")
            for fragment in forbidden_cfg_fragments:
                if fragment in text:
                    violations.append(f"{rust_path.relative_to(REPO_ROOT)}: {fragment}")
        self.assertEqual(violations, [])

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
            "https://developers.openai.com/codex/config-reference", config_docs
        )
        self.assertIn("https://developers.openai.com/codex/mcp", config_docs)

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
        self.assertIn("--others", run.call_args_list[1].args[0])

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

        self.assertIn(
            'RUSTFLAGS="-Ddead_code" just cargo-lane rust-dead-code-matrix cargo check',
            justfile,
        )
        self.assertIn("cargo-workspace-analyzer.ps1", justfile)
        analyzer = (REPO_ROOT / "scripts/cargo-workspace-analyzer.ps1").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            "$ForwardedArgs | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }",
            analyzer,
        )
        self.assertIn('$lane = "rust-dead-code-matrix"', analyzer)
        self.assertIn('$env:RUSTFLAGS = "-Ddead_code"', analyzer)

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

    def test_package_validation_defaults_do_not_expand_to_workspace(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        self.assertIn("Pass a package/filter to 'just clippy'", justfile)
        self.assertIn("clippy-workspace *args:", justfile)
        self.assertIn("-Analyzer clippy @forwarded_args", justfile)
        self.assertIn(
            '($forwarded_args -contains "-p") -or ($forwarded_args -contains "--package")',
            justfile,
        )
        self.assertIn('workspace_arg="--workspace"', justfile)
        self.assertIn('workspace_arg=""', justfile)

    def test_windows_process_suite_cannot_silently_skip_required_coverage(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        sandbox_tests = (
            REPO_ROOT / "codex-rs/windows-sandbox-rs/src/unified_exec/tests.rs"
        ).read_text(encoding="utf-8")
        pty_tests = (REPO_ROOT / "codex-rs/utils/pty/src/windows_tests.rs").read_text(
            encoding="utf-8"
        )

        self.assertIn(
            "SKIP: test-windows-sandbox-processes requires a Windows host; "
            "no Windows process verification was run",
            justfile,
        )
        self.assertGreaterEqual(justfile.count("--no-tests=fail"), 3)
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

    def test_dependency_policy_gate_runs_offline_cargo_deny(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        self.assertIn("cargo deny check bans sources licenses", justfile)
        self.assertIn("cargo tree -d --workspace", justfile)
        self.assertNotIn("--target all", justfile)

    def test_lane_recipes_use_the_canonical_reserved_runner(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        for snippet in (
            'run-lane --lane "{{ package }}" -- cargo nextest run',
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
            "_core-test-helpers-runtime target_dir:",
            "_core-test-helpers-mcp target_dir:",
            "_core-test-helpers-windows-sandbox target_dir:",
            'cargo build --target-dir "{{ target_dir }}" -p codex-cli --bin codex',
            'cargo build --target-dir "{{ target_dir }}" -p codex-code-mode-host --bin codex-code-mode-host',
            "just _test-lane-package-reserved",
            "$target_dir = $env:CODEX_CARGO_LANE_TARGET_DIR",
            '$env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "fast"; cargo nextest run --target-dir $target_dir -p "{{ package }}" @forwarded_args',
            '$text -match "(?i)rmcp|mcp|plugin|test_stdio_server"',
            '$text -match "(?i)windows_sandbox|windows-sandbox|sandbox|codex_command_runner"',
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
