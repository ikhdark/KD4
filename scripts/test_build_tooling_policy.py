#!/usr/bin/env python3

import contextlib
import io
import json
import importlib.util
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest
from unittest import mock

from scripts import rust_packages


REPO_ROOT = Path(__file__).resolve().parents[1]
CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)


def powershell() -> str | None:
    # Prefer Windows PowerShell 5.1: the justfile invokes these scripts via
    # `powershell -NoProfile -File ...`, so tests should exercise the same
    # host (5.1 has stricter native-stderr and StrictMode semantics).
    return shutil.which("powershell") or shutil.which("pwsh")


def pwsh_only() -> str | None:
    # invoke-rust-perf-env.ps1 runs under pwsh 7.4+ in production (recipes
    # invoke it inline in the just-shell pwsh session), and its -NoSccache
    # proof depends on pwsh's empty-env-var semantics, so its tests must not
    # fall back to Windows PowerShell 5.1.
    return shutil.which("pwsh")


def ps_single_quote(value: str | Path) -> str:
    return "'" + str(value).replace("'", "''") + "'"


def load_just_shell_module():
    path = REPO_ROOT / "scripts" / "just-shell.py"
    spec = importlib.util.spec_from_file_location("just_shell", path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_format_module():
    path = REPO_ROOT / "scripts" / "format.py"
    spec = importlib.util.spec_from_file_location("format_script", path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_root_maintenance_module():
    path = REPO_ROOT / "scripts" / "root_maintenance.py"
    spec = importlib.util.spec_from_file_location("root_maintenance", path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_toml(path: Path):
    return tomllib.loads(path.read_text(encoding="utf-8"))


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

    def test_build_info_scripts_emit_metadata_and_preserve_macos_linking(self) -> None:
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
        self.assertIn("cargo:rustc-link-arg=-ObjC", cli_build)

    def test_bwrap_build_script_tracks_resolved_source_dir(self) -> None:
        text = (REPO_ROOT / "codex-rs" / "bwrap" / "build.rs").read_text(
            encoding="utf-8"
        )

        self.assertIn(
            'println!("cargo:rerun-if-env-changed=CODEX_BWRAP_SOURCE_DIR");', text
        )
        self.assertIn("vendor_dir.join(source).display()", text)

    def test_skills_build_script_requires_bundled_samples(self) -> None:
        text = (REPO_ROOT / "codex-rs" / "skills" / "build.rs").read_text(
            encoding="utf-8"
        )

        self.assertIn('let samples_dir = Path::new("src/assets/samples");', text)
        self.assertIn("if !samples_dir.exists()", text)

    def test_retired_repo_local_harness_skill_has_no_registration(self) -> None:
        features = load_toml(REPO_ROOT / "kd4_features.toml")["features"]
        feature_ids = {feature["id"] for feature in features}
        workspace_readme = (REPO_ROOT / ".codex" / "README.md").read_text(
            encoding="utf-8"
        )
        harness_workflow = (REPO_ROOT / ".codex" / "harness" / "workflow.md").read_text(
            encoding="utf-8"
        )

        self.assertNotIn("kd4-harness", feature_ids)
        self.assertFalse((REPO_ROOT / ".codex" / "skills" / "kd4-harness").exists())
        self.assertNotIn("skills/kd4-harness", workspace_readme)
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
        first_lines = "\n".join(text.splitlines()[:40])

        self.assertIn(r"C:\Users\kuh\Desktop\kd4", first_lines)
        self.assertNotIn(r"C:\Users\kuh\Desktop\codexKD`", text)

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

        self.assertIn("## Validation and local-build proof", text)
        self.assertIn("Rust crates", text)
        self.assertIn("Scripts", text)
        self.assertIn("Local publish", text)
        self.assertIn("do not hand-edit generated locks", normalized)

    def test_agents_scripts_policy_is_nested_and_discoverable(self) -> None:
        root_text = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")
        scripts_text = (REPO_ROOT / "scripts" / "AGENTS.md").read_text(encoding="utf-8")

        self.assertIn("`scripts/AGENTS.md`", root_text)
        self.assertIn("# Scripts Policy", scripts_text)
        self.assertIn("Root maintenance commands", scripts_text)
        self.assertIn("root_maintenance.py", scripts_text)

    def test_installers_require_standalone_metadata(self) -> None:
        shell_installer = (REPO_ROOT / "scripts" / "install" / "install.sh").read_text(
            encoding="utf-8"
        )
        powershell_installer = (
            REPO_ROOT / "scripts" / "install" / "install.ps1"
        ).read_text(encoding="utf-8")

        self.assertIn('INSTALL_METADATA_FILE="codex-install.env"', shell_installer)
        self.assertIn(
            '[ -f "$release_dir/$INSTALL_METADATA_FILE" ] ||', shell_installer
        )
        self.assertIn('"$BIN_PATH" --version >/dev/null', shell_installer)
        self.assertNotIn('visible_command_preverified="true"', shell_installer)
        self.assertIn(
            '[ -x "$release_dir/bin/codex-code-mode-host" ] &&',
            shell_installer,
        )
        self.assertIn(
            '"$stage_release/bin/codex-code-mode-host"',
            shell_installer,
        )

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

    def test_shell_installer_completeness_rejects_package_without_code_mode_host(
        self,
    ) -> None:
        shell = shutil.which("sh")
        if shell is None:
            self.skipTest("shell is required for the Unix installer completeness test")

        shell_installer_path = REPO_ROOT / "scripts" / "install" / "install.sh"
        shell_installer = shell_installer_path.read_text(encoding="utf-8")

        def shell_function(name: str) -> str:
            start = shell_installer.index(f"{name}() {{")
            end = shell_installer.index("\n}\n", start) + 3
            return shell_installer[start:end]

        with tempfile.TemporaryDirectory(dir=REPO_ROOT) as temp_dir:
            root = Path(temp_dir)
            version = "1.2.3"
            target = "test-target"
            release_dir = root / f"{version}-{target}"
            for relative in ("bin/codex", "codex", "codex-path/rg"):
                path = release_dir / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("#!/bin/sh\n", encoding="utf-8", newline="\n")
                path.chmod(0o755)
            (release_dir / "codex-package.json").touch()
            with (release_dir / "codex-install.env").open(
                "w", encoding="utf-8", newline="\n"
            ) as metadata:
                metadata.write(f"version={version}\ntarget={target}\nlayout=package\n")
            shell_probe = root / "probe.sh"
            shell_probe.write_text(
                "\n".join(
                    [
                        "#!/bin/sh",
                        'INSTALL_METADATA_FILE="codex-install.env"',
                        shell_function("install_metadata_field"),
                        shell_function("release_dir_is_complete"),
                        'chmod 0755 "$1/bin/codex" "$1/codex" "$1/codex-path/rg"',
                        'if [ -e "$1/bin/codex-code-mode-host" ]; then',
                        '  chmod 0755 "$1/bin/codex-code-mode-host"',
                        "fi",
                        'release_dir_is_complete "$1" "$2" "$3" package',
                        "",
                    ]
                ),
                encoding="utf-8",
            )

            missing_shell = subprocess.run(
                [
                    shell,
                    shell_probe.relative_to(REPO_ROOT).as_posix(),
                    release_dir.relative_to(REPO_ROOT).as_posix(),
                    version,
                    target,
                ],
                cwd=REPO_ROOT,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertNotEqual(missing_shell.returncode, 0)
            shell_host = release_dir / "bin" / "codex-code-mode-host"
            shell_host.write_text("#!/bin/sh\n", encoding="utf-8", newline="\n")
            shell_host.chmod(0o755)
            complete_shell = subprocess.run(
                [
                    shell,
                    shell_probe.relative_to(REPO_ROOT).as_posix(),
                    release_dir.relative_to(REPO_ROOT).as_posix(),
                    version,
                    target,
                ],
                cwd=REPO_ROOT,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(
                complete_shell.returncode,
                0,
                f"stdout:\n{complete_shell.stdout}\nstderr:\n{complete_shell.stderr}",
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
                "codex-package.json",
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
                    f"{ps_single_quote(windows_package)}; "
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
            for path in (REPO_ROOT / "scripts").rglob("*.py")
            if "__pycache__" not in path.parts and ".venv" not in path.parts
        )
        expected_unittest_targets = sorted(
            path.relative_to(REPO_ROOT).with_suffix("").as_posix().replace("/", ".")
            for path in (REPO_ROOT / "scripts").rglob("test_*.py")
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
        self.assertEqual(
            root_maintenance.test_modules_for_changed_path(
                "scripts/publish-local-codex-wsl.sh"
            ),
            (
                "scripts.test_dev_environment",
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

    def test_root_maintenance_script_audit_plan_covers_every_script_type(self) -> None:
        root_maintenance = load_root_maintenance_module()
        tools = {
            "uv": "uv",
            "pwsh": "pwsh",
            "bash": "bash",
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
        self.assertIn("shell syntax", labels)
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
        shell_command = dict(commands)["shell syntax"]
        self.assertIn("-c", shell_command)
        self.assertNotIn("-lc", shell_command)
        self.assertIn("$'s/\\r$//'", shell_command[-1])

        commands_without_tests, _missing = root_maintenance.script_audit_commands(
            include_tests=True,
            test_targets=[],
            resolve_tool=tools.get,
        )
        self.assertNotIn(
            "script unit tests",
            [label for label, _command in commands_without_tests],
        )

    def test_root_maintenance_script_audit_current_tree_has_no_hard_findings(
        self,
    ) -> None:
        root_maintenance = load_root_maintenance_module()

        errors, _advisories = root_maintenance.script_audit_findings()

        self.assertEqual(errors, [])

    def test_root_maintenance_script_audit_skips_native_sh_tests_on_windows(
        self,
    ) -> None:
        root_maintenance = load_root_maintenance_module()

        targets, skipped = root_maintenance.script_audit_test_targets(
            platform="nt",
            native_sh_available=False,
        )
        self.assertNotIn("scripts.install.test_install_sh", targets)
        self.assertEqual(len(skipped), 1)
        self.assertIn("native /bin/sh is unavailable", skipped[0])

        native_targets, native_skipped = root_maintenance.script_audit_test_targets(
            platform="nt",
            native_sh_available=True,
        )
        self.assertIn("scripts.install.test_install_sh", native_targets)
        self.assertEqual(native_skipped, [])

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

    def test_root_maintenance_empty_changed_selection_is_a_noop(self) -> None:
        root_maintenance = load_root_maintenance_module()

        with (
            mock.patch.object(root_maintenance, "git_changed_paths", return_value=[]),
            mock.patch.object(root_maintenance, "run") as run,
        ):
            self.assertEqual(
                root_maintenance.main(["format-python", "--write", "--changed"]), 0
            )
            self.assertEqual(root_maintenance.main(["test-python", "--changed"]), 0)

        run.assert_not_called()

    def test_root_maintenance_prettier_does_not_scan_script_inventory(self) -> None:
        root_maintenance = load_root_maintenance_module()

        with (
            mock.patch.object(
                root_maintenance,
                "script_inventory",
                side_effect=AssertionError("unexpected script scan"),
            ),
            mock.patch.object(root_maintenance, "run", return_value=0) as run,
        ):
            self.assertEqual(root_maintenance.main(["format-prettier"]), 0)

        run.assert_called_once()

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

    def test_root_maintenance_uv_commands_use_frozen_lock(self) -> None:
        root_maintenance = load_root_maintenance_module()
        calls: list[tuple[str, ...]] = []

        def fake_run(command: list[str]) -> int:
            calls.append(tuple(command))
            return 0

        with mock.patch.object(root_maintenance, "run", side_effect=fake_run):
            self.assertEqual(
                root_maintenance.main(
                    ["format-python", "--changed", "scripts/root_maintenance.py"]
                ),
                0,
            )
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
        root_maintenance = load_root_maintenance_module()

        self.assertEqual(
            package["scripts"]["format"],
            "prettier --check *.json *.md docs/*.md **/*.js",
        )
        self.assertEqual(
            package["scripts"]["format:fix"],
            "prettier --write *.json *.md docs/*.md **/*.js",
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
        for target in root_maintenance.PRETTIER_TARGETS:
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
            package["scripts"]["test:scripts:target"],
            "python scripts/root_maintenance.py test-python --changed",
        )
        self.assertEqual(
            package["scripts"]["test:scripts:changed"],
            "python scripts/root_maintenance.py test-python --changed",
        )

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
            "Tooling success alone does not prove a",
            text,
        )
        self.assertIn("focused failing test or approved final gate", normalized)

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

    def test_windows_process_suite_cannot_silently_skip_required_coverage(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        sandbox_tests = (
            REPO_ROOT / "codex-rs/windows-sandbox-rs/src/unified_exec/tests.rs"
        ).read_text(encoding="utf-8")
        pty_tests = (
            REPO_ROOT / "codex-rs/utils/pty/src/windows_tests.rs"
        ).read_text(encoding="utf-8")

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
        self.assertIn("cargo tree -d --workspace --target all", justfile)

    def test_unix_lane_recipes_mirror_windows_focused_lanes(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        for snippet in (
            'shift; RUST_MIN_STACK={{ rust_min_stack }} NEXTEST_PROFILE=fast cargo nextest run --target-dir "target/lanes/{{ package }}" -p {{ package }} "$@"',
            'shift; cargo check --target-dir "target/lanes/{{ package }}" -p {{ package }} "$@"',
            'shift; cargo clippy --tests --target-dir "target/lanes/{{ package }}" -p {{ package }} "$@"',
            'cargo build --release --target-dir target/lanes/release "$@"',
        ):
            self.assertIn(snippet, justfile)

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
            'cargo watch -x "check -p {{ package }}" @($args | Select-Object -Skip 2)',
            'cargo llvm-cov -p "{{ package }}" @($args | Select-Object -Skip 2)',
            "_core-test-helpers-runtime target_dir:",
            "_core-test-helpers-mcp target_dir:",
            "_core-test-helpers-windows-sandbox target_dir:",
            'cargo build --target-dir "{{ target_dir }}" -p codex-cli --bin codex',
            'cargo build --target-dir "{{ target_dir }}" -p codex-code-mode-host --bin codex-code-mode-host',
            '$forwarded_args = @($args | Select-Object -Skip 2); $target_dir = "target\\lanes\\{{ package }}"',
            '$env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "fast"; cargo nextest run --target-dir $target_dir -p "{{ package }}" @forwarded_args',
            '$text -match "(?i)rmcp|mcp|plugin|test_stdio_server"',
            '$text -match "(?i)windows_sandbox|windows-sandbox|sandbox|codex_command_runner"',
        ):
            self.assertIn(command, justfile)
        self.assertNotIn(
            "test-lane-package package *args:\n    @$forwarded_args",
            justfile,
        )
        self.assertGreaterEqual(justfile.count("scripts\\cargo-lane.ps1"), 3)

    def test_perf_env_recipes_pass_structured_argv(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        perf_env = (REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1").read_text(
            encoding="utf-8"
        )

        self.assertIn("[string]$CargoTargetLane", perf_env)
        self.assertIn("[Parameter(ValueFromRemainingArguments = $true)]", perf_env)
        self.assertIn("[string[]]$ProgramArgs", perf_env)
        self.assertIn("& $program @arguments", perf_env)
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

    def test_remote_env_setup_quotes_container_paths_and_tracks_ownership(self) -> None:
        remote_env = (REPO_ROOT / "scripts" / "test-remote-env.sh").read_text(
            encoding="utf-8"
        )

        self.assertIn("cleanup_remote_env_setup_failure", remote_env)
        self.assertIn("CODEX_TEST_REMOTE_EXEC_SERVER_MANAGED", remote_env)
        self.assertIn('nohup "$remote_codex" exec-server', remote_env)
        self.assertNotIn("nohup ${remote_codex_path} exec-server", remote_env)


if __name__ == "__main__":
    unittest.main()
