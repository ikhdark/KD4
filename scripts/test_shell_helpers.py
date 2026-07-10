#!/usr/bin/env python3

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]


def script_text(name: str) -> str:
    return (REPO_ROOT / "scripts" / name).read_text(encoding="utf-8")


def write_executable(path: Path, contents: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(contents, encoding="utf-8", newline="\n")
    path.chmod(0o755)
    return path


class ShellHelperScriptsTest(unittest.TestCase):
    def bash(self) -> str:
        candidates = [
            Path.home() / "scoop" / "apps" / "git" / "current" / "bin" / "bash.exe",
            Path(os.environ.get("ProgramFiles", "")) / "Git" / "bin" / "bash.exe",
        ]
        discovered = shutil.which("bash")
        if discovered is not None:
            candidates.append(Path(discovered))
        bash = next((str(path) for path in candidates if path.is_file()), None)
        if bash is None:
            self.skipTest("bash is not available")
        probe = subprocess.run(
            [bash, "-c", 'test -n "$BASH_VERSION"'],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            timeout=30,
        )
        if probe.returncode != 0:
            self.skipTest("the available bash command did not start Bash")
        return bash

    def copy_script(self, fixture_root: Path, name: str) -> Path:
        return write_executable(
            fixture_root / "scripts" / name,
            script_text(name),
        )

    def run_fixture(
        self,
        fixture_root: Path,
        command: str,
        *args: str,
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [self.bash(), "-c", command, "bash", *args],
            cwd=fixture_root,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            timeout=30,
        )

    def install_fake_bazel(self, fixture_root: Path, body: str) -> Path:
        return write_executable(
            fixture_root / "fake-bin" / "bazel",
            "#!/usr/bin/env bash\n" + body,
        )

    def install_clippy_bazel(self, fixture_root: Path) -> Path:
        return self.install_fake_bazel(
            fixture_root,
            """printf '%s\\n' "$@" > "$BAZEL_LOG"
if [[ -f "$PWD/fake-bazel.stdout" ]]; then
  cat "$PWD/fake-bazel.stdout"
fi
if [[ -f "$PWD/fake-bazel.stderr" ]]; then
  cat "$PWD/fake-bazel.stderr" >&2
fi
if [[ -f "$PWD/fake-bazel.exit" ]]; then
  exit "$(cat "$PWD/fake-bazel.exit")"
fi
""",
        )

    def test_bazel_lock_check_runs_exact_command(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture_root = Path(temp_dir)
            self.copy_script(fixture_root, "check-module-bazel-lock.sh")
            self.install_fake_bazel(
                fixture_root,
                """printf '%s\\n' "$@" > "$BAZEL_LOG"
exit "${FAKE_BAZEL_EXIT:-0}"
""",
            )

            result = self.run_fixture(
                fixture_root,
                """export PATH="$PWD/fake-bin:$PATH"
export BAZEL_LOG="$PWD/bazel.log"
bash scripts/check-module-bazel-lock.sh
""",
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "")
            self.assertEqual(
                (fixture_root / "bazel.log").read_text(encoding="utf-8").splitlines(),
                ["mod", "deps", "--lockfile_mode=error"],
            )

    def test_bazel_lock_check_reports_remediation_after_failure(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture_root = Path(temp_dir)
            self.copy_script(fixture_root, "check-module-bazel-lock.sh")
            self.install_fake_bazel(
                fixture_root,
                """printf '%s\\n' "$@" > "$BAZEL_LOG"
exit "${FAKE_BAZEL_EXIT:-0}"
""",
            )

            result = self.run_fixture(
                fixture_root,
                """export PATH="$PWD/fake-bin:$PATH"
export BAZEL_LOG="$PWD/bazel.log"
export FAKE_BAZEL_EXIT=42
bash scripts/check-module-bazel-lock.sh
""",
            )

            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertEqual(
                result.stdout.splitlines(),
                [
                    "MODULE.bazel.lock is out of date.",
                    "Run 'just bazel-lock-update' and commit the updated lockfile.",
                ],
            )
            self.assertEqual(
                (fixture_root / "bazel.log").read_text(encoding="utf-8").splitlines(),
                ["mod", "deps", "--lockfile_mode=error"],
            )

    def test_debug_codex_uses_existing_windows_binary_and_preserves_args(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture_root = Path(temp_dir)
            self.copy_script(fixture_root, "debug-codex.sh")
            write_executable(
                fixture_root / "codex-rs" / "target" / "debug" / "codex.exe",
                """#!/usr/bin/env bash
printf 'binary\\n' > "$CALL_LOG"
printf 'arg=%s\\n' "$@" >> "$CALL_LOG"
""",
            )
            write_executable(
                fixture_root / "fake-bin" / "cargo",
                """#!/usr/bin/env bash
printf 'cargo\\n' > "$CALL_LOG"
exit 99
""",
            )

            result = self.run_fixture(
                fixture_root,
                """export CALL_LOG="$PWD/calls.log"
export PATH="$PWD/fake-bin:$PATH"
export CODEX_DEBUG_USE_EXISTING_BINARY=1
bash scripts/debug-codex.sh "$@"
""",
                "value with spaces",
                "literal*",
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                (fixture_root / "calls.log").read_text(encoding="utf-8").splitlines(),
                ["binary", "arg=value with spaces", "arg=literal*"],
            )

    def test_debug_codex_cargo_fallback_preserves_exit_and_args(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture_root = Path(temp_dir)
            self.copy_script(fixture_root, "debug-codex.sh")
            (fixture_root / "codex-rs").mkdir()
            write_executable(
                fixture_root / "fake-bin" / "cargo",
                """#!/usr/bin/env bash
printf 'cargo-pwd=%s\\n' "$PWD" > "$CALL_LOG"
printf 'arg=%s\\n' "$@" >> "$CALL_LOG"
exit 37
""",
            )

            result = self.run_fixture(
                fixture_root,
                """export CALL_LOG="$PWD/calls.log"
export PATH="$PWD/fake-bin:$PATH"
unset CODEX_DEBUG_USE_EXISTING_BINARY
bash scripts/debug-codex.sh "$@"
""",
                "value with spaces",
                "literal*",
            )

            self.assertEqual(result.returncode, 37, result.stderr)
            lines = (
                (fixture_root / "calls.log").read_text(encoding="utf-8").splitlines()
            )
            self.assertTrue(lines[0].endswith("/codex-rs"), lines[0])
            self.assertEqual(
                lines[1:],
                [
                    "arg=run",
                    "arg=--quiet",
                    "arg=--bin",
                    "arg=codex",
                    "arg=--",
                    "arg=value with spaces",
                    "arg=literal*",
                ],
            )

    def test_clippy_target_helper_emits_nothing_when_query_fails(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture_root = Path(temp_dir)
            self.copy_script(fixture_root, "list-bazel-clippy-targets.sh")
            self.install_clippy_bazel(fixture_root)
            (fixture_root / "fake-bazel.stdout").write_text(
                "//codex-rs/example:partial\n",
                encoding="utf-8",
            )
            (fixture_root / "fake-bazel.stderr").write_text(
                "query failed\n",
                encoding="utf-8",
            )
            (fixture_root / "fake-bazel.exit").write_text("23\n", encoding="utf-8")

            result = self.run_fixture(
                fixture_root,
                """export PATH="$PWD/fake-bin:$PATH"
export BAZEL_LOG="$PWD/bazel.log"
bash scripts/list-bazel-clippy-targets.sh
""",
            )

            self.assertEqual(result.returncode, 23)
            self.assertEqual(result.stdout, "")
            self.assertIn("query failed", result.stderr)

    def test_clippy_target_helper_filters_windows_cross_target_on_linux(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture_root = Path(temp_dir)
            self.copy_script(fixture_root, "list-bazel-clippy-targets.sh")
            self.install_clippy_bazel(fixture_root)
            (fixture_root / "fake-bazel.stdout").write_text(
                "\n".join(
                    [
                        "//codex-rs/example:unit-tests-bin",
                        "//codex-rs/example:integration-test-bin",
                        "//codex-rs/example:integration-test-windows-cross-bin",
                    ]
                )
                + "\n",
                encoding="utf-8",
            )

            result = self.run_fixture(
                fixture_root,
                """export PATH="$PWD/fake-bin:$PATH"
export BAZEL_LOG="$PWD/bazel.log"
export RUNNER_OS=Linux
bash scripts/list-bazel-clippy-targets.sh
""",
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                result.stdout.splitlines(),
                [
                    "//codex-rs/...",
                    "-//codex-rs/v8-poc:all",
                    "//codex-rs/example:unit-tests-bin",
                    "//codex-rs/example:integration-test-bin",
                ],
            )
            self.assertEqual(
                (fixture_root / "bazel.log").read_text(encoding="utf-8").splitlines(),
                [
                    "--noexperimental_remote_repo_contents_cache",
                    "query",
                    "--output=label",
                    "--",
                    'kind("rust_test rule", attr(tags, "manual", //codex-rs/... except //codex-rs/v8-poc/...))',
                ],
            )

    def test_clippy_target_helper_uses_cache_flags_for_windows_cross(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture_root = Path(temp_dir)
            self.copy_script(fixture_root, "list-bazel-clippy-targets.sh")
            self.install_clippy_bazel(fixture_root)
            (fixture_root / "fake-bazel.stdout").write_text(
                "\n".join(
                    [
                        "//codex-rs/example:unit-tests-bin",
                        "//codex-rs/example:integration-test-bin",
                        "//codex-rs/example:integration-test-windows-cross-bin",
                    ]
                )
                + "\n",
                encoding="utf-8",
            )

            result = self.run_fixture(
                fixture_root,
                """export PATH="$PWD/fake-bin:$PATH"
export BAZEL_LOG="$PWD/bazel.log"
export RUNNER_OS=Windows
export BAZEL_OUTPUT_USER_ROOT=/tmp/bazel-output
export BAZEL_REPO_CONTENTS_CACHE=/tmp/repo-contents
export BAZEL_REPOSITORY_CACHE=/tmp/repository-cache
bash scripts/list-bazel-clippy-targets.sh --windows-cross-compile
""",
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                result.stdout.splitlines(),
                [
                    "//codex-rs/...",
                    "-//codex-rs/v8-poc:all",
                    "//codex-rs/example:unit-tests-bin",
                    "//codex-rs/example:integration-test-windows-cross-bin",
                ],
            )
            self.assertEqual(
                (fixture_root / "bazel.log").read_text(encoding="utf-8").splitlines(),
                [
                    "--output_user_root=/tmp/bazel-output",
                    "--noexperimental_remote_repo_contents_cache",
                    "query",
                    "--repo_contents_cache=/tmp/repo-contents",
                    "--repository_cache=/tmp/repository-cache",
                    "--output=label",
                    "--",
                    'kind("rust_test rule", attr(tags, "manual", //codex-rs/... except //codex-rs/v8-poc/...))',
                ],
            )

    def test_release_target_helper_emits_the_expected_bounded_targets(self) -> None:
        result = subprocess.run(
            [self.bash(), "scripts/list-bazel-release-targets.sh"],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            timeout=30,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.splitlines(),
            ["//codex-rs/...", "-//codex-rs/v8-poc:all"],
        )


if __name__ == "__main__":
    unittest.main()
