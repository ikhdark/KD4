#!/usr/bin/env python3

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

    def test_bazel_build_scripts_include_shared_build_info_source(
        self,
    ) -> None:
        macro = (REPO_ROOT / "defs.bzl").read_text(encoding="utf-8")
        workspace_build = (
            REPO_ROOT / "codex-rs" / "BUILD.bazel"
        ).read_text(encoding="utf-8")
        cli_build = (REPO_ROOT / "codex-rs" / "cli" / "BUILD.bazel").read_text(
            encoding="utf-8"
        )
        app_server_build = (
            REPO_ROOT / "codex-rs" / "app-server" / "BUILD.bazel"
        ).read_text(encoding="utf-8")

        self.assertIn("MACOS_WEBRTC_RUSTC_LINK_FLAGS", cli_build)
        self.assertIn("extra_binaries_non_windows", app_server_build)
        self.assertIn('"build_info.rs"', workspace_build)
        self.assertIn("build_script_srcs = []", macro)
        self.assertIn('srcs = ["build.rs"] + build_script_srcs', macro)
        for crate_build in (cli_build, app_server_build):
            self.assertIn(
                'build_script_srcs = ["//codex-rs:build_info.rs"]',
                crate_build,
            )

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

    def test_repo_local_skill_tree_is_local_build_focused(self) -> None:
        required = {
            "kd4-crosscheck-and-finish",
            "kd4-harness",
        }
        skills_dir = REPO_ROOT / ".codex" / "skills"
        if not skills_dir.exists():
            self.skipTest("repo-local skills directory is not materialized")
        actual = {path.name for path in skills_dir.iterdir() if path.is_dir()}

        self.assertTrue(required.issubset(actual))

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
        self.assertIn("kd4-crosscheck-and-finish", skill_names)
        self.assertIn("kd4-harness", skill_names)
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

        self.assertEqual(root_maintenance.PYTHON_RUFF_TARGETS, expected_ruff_targets)
        self.assertEqual(
            root_maintenance.PYTHON_UNITTEST_TARGETS, expected_unittest_targets
        )
        self.assertEqual(
            root_maintenance.python_test_targets(["scripts.test_verify_local"], []),
            ["scripts.test_verify_local"],
        )
        self.assertEqual(
            root_maintenance.python_test_targets([], ["scripts/verify_local.py"]),
            ["scripts.test_verify_local"],
        )
        with mock.patch.object(
            root_maintenance,
            "git_changed_paths",
            return_value=["scripts/verify_local.py", "docs/example.md"],
        ):
            self.assertEqual(
                root_maintenance.expand_changed_paths([None]),
                ["scripts/verify_local.py", "docs/example.md"],
            )
        with mock.patch.object(
            root_maintenance,
            "git_changed_paths",
            return_value=["scripts/verify_local.py"],
        ):
            self.assertEqual(
                root_maintenance.python_test_targets(
                    [], root_maintenance.expand_changed_paths([None])
                ),
                ["scripts.test_verify_local"],
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
            for target in root_maintenance.SCRIPT_AUDIT_TARGETS
            if root_maintenance.script_kind_for_path(
                root_maintenance.REPO_ROOT / target
            )
            == "javascript"
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
        completed = subprocess.CompletedProcess(
            ["git"],
            0,
            stdout="scripts/line\nbreak.py\0scripts/ trailing .py\0",
            stderr="",
        )

        with mock.patch.object(
            root_maintenance.subprocess, "run", return_value=completed
        ) as run:
            paths = root_maintenance.git_changed_paths()

        self.assertEqual(
            paths,
            ["scripts/line\nbreak.py", "scripts/ trailing .py"],
        )
        self.assertIn("-z", run.call_args.args[0])

    def test_root_maintenance_uv_commands_use_frozen_lock(self) -> None:
        root_maintenance = load_root_maintenance_module()
        calls: list[tuple[str, ...]] = []

        def fake_run(command: list[str]) -> int:
            calls.append(tuple(command))
            return 0

        with mock.patch.object(root_maintenance, "run", side_effect=fake_run):
            self.assertEqual(
                root_maintenance.main(
                    ["format-python", "--changed", "scripts/verify_local.py"]
                ),
                0,
            )
            self.assertEqual(
                root_maintenance.main(
                    ["lint-python", "--changed", "scripts/verify_local.py"]
                ),
                0,
            )
            self.assertEqual(
                root_maintenance.main(
                    ["test-python", "--module", "scripts.test_verify_local"]
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

    def test_package_json_exposes_focused_script_test_alias(self) -> None:
        package = json.loads((REPO_ROOT / "package.json").read_text(encoding="utf-8"))

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
            "bazel-test-changed *targets:",
            "bench-workspace *args:",
            "test-lane-fast lane *args:",
            "test-windows-sandbox-processes *args:",
            "deps-duplicates-workspace *args:",
            "deps-policy-check *args:",
        ):
            self.assertIn(recipe, justfile)

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
        rules = (REPO_ROOT / "scripts" / "verify_local_rules.toml").read_text(
            encoding="utf-8"
        )

        self.assertIn("cargo deny check bans sources licenses", justfile)
        self.assertIn("cargo tree -d --workspace --target all", justfile)
        self.assertIn('validation_command = ["just", "deps-policy-check"]', rules)

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
            "_core-test-helpers-mcp:",
            "_core-test-helpers-windows-sandbox:",
            '$text -match "(?i)rmcp|mcp|plugin|test_stdio_server"',
            '$text -match "(?i)windows_sandbox|windows-sandbox|sandbox|codex_command_runner"',
        ):
            self.assertIn(command, justfile)
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
