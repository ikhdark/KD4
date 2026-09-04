#!/usr/bin/env python3

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class CheckBlobSizeTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.repo = Path(self.temp.name) / "repo"
        subprocess.run(["git", "init", "-q", str(self.repo)], check=True)
        self.git("config", "user.email", "test@example.com")
        self.git("config", "user.name", "Test")
        self.git("config", "core.autocrlf", "false")
        self.git("commit", "--allow-empty", "-qm", "baseline")
        self.base = self.git("rev-parse", "HEAD").stdout.strip()
        self.script = Path(__file__).with_name("check_blob_size.py").resolve()
        self.allowlist = Path(self.temp.name) / "allowlist.txt"
        self.allowlist.write_text("", encoding="utf-8")

    def tearDown(self) -> None:
        self.temp.cleanup()

    def git(
        self,
        *arguments: str,
        input_text: str | None = None,
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["git", "-C", str(self.repo), *arguments],
            check=True,
            capture_output=True,
            text=True,
            encoding="utf-8",
            input=input_text,
        )

    def commit_files(
        self, files: dict[str, str | bytes], message: str = "files"
    ) -> str:
        for relative, contents in files.items():
            path = self.repo / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            if isinstance(contents, bytes):
                path.write_bytes(contents)
            else:
                path.write_text(contents, encoding="utf-8", newline="")
        self.git("add", "-A")
        self.git("commit", "-qm", message)
        return self.git("rev-parse", "HEAD").stdout.strip()

    def commit_tree_blobs(self, files: dict[str, bytes], message: str) -> str:
        entries: list[bytes] = []
        for path, contents in sorted(files.items()):
            blob = (
                subprocess.run(
                    ["git", "-C", str(self.repo), "hash-object", "-w", "--stdin"],
                    check=True,
                    capture_output=True,
                    input=contents,
                )
                .stdout.decode("ascii")
                .strip()
            )
            entries.append(f"100644 blob {blob}\t{path}\0".encode())
        tree = (
            subprocess.run(
                ["git", "-C", str(self.repo), "mktree", "-z"],
                check=True,
                capture_output=True,
                input=b"".join(entries),
            )
            .stdout.decode("ascii")
            .strip()
        )
        return subprocess.run(
            [
                "git",
                "-C",
                str(self.repo),
                "commit-tree",
                tree,
                "-p",
                "HEAD",
                "-m",
                message,
            ],
            check=True,
            capture_output=True,
            text=True,
            encoding="utf-8",
        ).stdout.strip()

    def run_cli(
        self,
        *,
        base: str,
        head: str,
        max_bytes: int = 512000,
        include_kind: bool = False,
        stdin_paths: str | None = None,
        paths_file: Path | None = None,
        summary_path: Path | None = None,
        use_temp_repository: bool = True,
        cwd: Path | None = None,
    ) -> subprocess.CompletedProcess[str]:
        command = [
            sys.executable,
            str(self.script),
            "--base",
            base,
            "--head",
            head,
            "--max-bytes",
            str(max_bytes),
            "--allowlist",
            str(self.allowlist),
        ]
        if include_kind:
            command.append("--include-kind")
        if stdin_paths is not None:
            command.append("--stdin-paths")
        if paths_file is not None:
            command.extend(["--paths-file", str(paths_file)])

        environment = os.environ.copy()
        if use_temp_repository:
            environment["GIT_DIR"] = str(self.repo / ".git")
            environment["GIT_WORK_TREE"] = str(self.repo)
        if summary_path is not None:
            environment["GITHUB_STEP_SUMMARY"] = str(summary_path)
        else:
            environment.pop("GITHUB_STEP_SUMMARY", None)
        return subprocess.run(
            command,
            cwd=cwd,
            env=environment,
            input=stdin_paths,
            check=False,
            capture_output=True,
            text=True,
            encoding="utf-8",
        )

    def test_cli_anchors_git_at_repository_root_from_unrelated_cwd(self) -> None:
        completed = self.run_cli(
            base="HEAD",
            head="HEAD",
            use_temp_repository=False,
            cwd=Path(self.temp.name),
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("No changed files were detected", completed.stdout)

    def test_cli_collects_real_diff_and_blob_sizes_in_one_run(self) -> None:
        head = self.commit_files(
            {"text.txt": "hello\n", "image.bin": b"\0" + b"x" * 599},
        )
        self.allowlist.write_text("image.bin\n", encoding="utf-8")

        completed = self.run_cli(
            base=self.base,
            head=head,
            max_bytes=100,
            include_kind=True,
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("Checked 2 changed file(s)", completed.stdout)
        self.assertIn("text.txt: 6 bytes (0.0 KiB) [non-binary, ok]", completed.stdout)
        self.assertIn(
            "image.bin: 600 bytes (0.6 KiB) [binary, allowlisted]", completed.stdout
        )

    def test_cli_explicit_paths_skip_diff_when_kind_is_not_requested(self) -> None:
        head = self.commit_files({"a.txt": "a", "b.txt": "bb"})

        completed = self.run_cli(
            base="not-a-real-revision",
            head=head,
            stdin_paths="a.txt\nb.txt\n",
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("Checked 2 changed file(s)", completed.stdout)
        self.assertIn("a.txt: 1 bytes", completed.stdout)
        self.assertIn("b.txt: 2 bytes", completed.stdout)

    def test_cli_explicit_kind_preserves_path_absent_from_diff(self) -> None:
        first = self.commit_files({"a.txt": "a", "b.bin": b"\0old"}, "first")
        head = self.commit_files({"b.bin": b"\0new-binary"}, "second")

        completed = self.run_cli(
            base=first,
            head=head,
            include_kind=True,
            stdin_paths="a.txt\nb.bin\n",
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("a.txt: 1 bytes (0.0 KiB) [non-binary, ok]", completed.stdout)
        self.assertIn("b.bin: 11 bytes (0.0 KiB) [binary, ok]", completed.stdout)

    def test_cli_handles_ten_thousand_paths_without_command_line_expansion(
        self,
    ) -> None:
        head = self.commit_files({"a.txt": "a"})
        paths_file = Path(self.temp.name) / "many-paths.bin"
        paths_file.write_bytes(("a.txt\0" * 10_000).encode("utf-8"))

        completed = self.run_cli(
            base=self.base,
            head=head,
            include_kind=True,
            paths_file=paths_file,
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("Checked 10000 changed file(s)", completed.stdout)
        self.assertEqual(completed.stdout.count("- a.txt:"), 10_000)

    def test_cli_paths_file_accepts_newline_and_nul_delimiters(self) -> None:
        head = self.commit_files({"a.txt": "a", "b.txt": "bb"})
        newline_file = Path(self.temp.name) / "newline-paths.txt"
        newline_file.write_text("a.txt\n\nb.txt\n", encoding="utf-8")
        nul_file = Path(self.temp.name) / "nul-paths.bin"
        nul_file.write_bytes(b"a.txt\0\0b.txt\0")

        for paths_file in (newline_file, nul_file):
            with self.subTest(paths_file=paths_file.name):
                completed = self.run_cli(
                    base="not-a-real-revision",
                    head=head,
                    paths_file=paths_file,
                )
                self.assertEqual(completed.returncode, 0, completed.stderr)
                self.assertIn("Checked 2 changed file(s)", completed.stdout)
                self.assertIn("a.txt: 1 bytes", completed.stdout)
                self.assertIn("b.txt: 2 bytes", completed.stdout)

    def test_cli_preserves_newlines_in_paths_during_blob_lookup(self) -> None:
        newline_path = "a\nb.txt"
        head = self.commit_tree_blobs(
            {newline_path: b"a" * 123, "c.txt": b"c" * 456},
            "newline path",
        )
        paths_file = Path(self.temp.name) / "paths.bin"
        paths_file.write_bytes(f"{newline_path}\0c.txt\0".encode())

        completed = self.run_cli(
            base="not-a-real-revision",
            head=head,
            paths_file=paths_file,
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn(f"{newline_path}: 123 bytes", completed.stdout)
        self.assertIn("c.txt: 456 bytes", completed.stdout)

    def test_cli_reports_missing_blob_without_traceback(self) -> None:
        completed = self.run_cli(
            base="not-a-real-revision",
            head=self.base,
            stdin_paths="missing.txt\n",
        )

        self.assertEqual(completed.returncode, 2)
        self.assertIn("'missing.txt' does not exist as a blob", completed.stderr)
        self.assertNotIn("Traceback", completed.stderr)

    def test_cli_rejects_non_blob_gitlink(self) -> None:
        self.git(
            "update-index",
            "--add",
            "--cacheinfo",
            f"160000,{self.base},vendor/submodule",
        )
        self.git("commit", "-qm", "gitlink")
        head = self.git("rev-parse", "HEAD").stdout.strip()

        completed = self.run_cli(
            base="not-a-real-revision",
            head=head,
            stdin_paths="vendor/submodule\n",
        )

        self.assertEqual(completed.returncode, 2)
        self.assertIn("'vendor/submodule' is not a blob", completed.stderr)
        self.assertNotIn("Traceback", completed.stderr)

    def test_cli_allowlist_preserves_hash_in_paths_and_inline_comments(self) -> None:
        head = self.commit_files(
            {"assets/icon#dark.png": b"x" * 20, "assets/large.bin": b"y" * 30}
        )
        self.allowlist.write_text(
            "# comment\nassets/icon#dark.png\nassets/large.bin # explanation\n",
            encoding="utf-8",
        )

        completed = self.run_cli(
            base="not-a-real-revision",
            head=head,
            max_bytes=10,
            stdin_paths="assets/icon#dark.png\nassets/large.bin\n",
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn(
            "assets/icon#dark.png: 20 bytes (0.0 KiB) [allowlisted]", completed.stdout
        )
        self.assertIn(
            "assets/large.bin: 30 bytes (0.0 KiB) [allowlisted]", completed.stdout
        )

    def test_cli_step_summary_escapes_markdown_table_cells(self) -> None:
        unusual_path = "a|`b`.txt"
        head = self.commit_tree_blobs({unusual_path: b"x" * 20}, "unusual path")
        summary = Path(self.temp.name) / "summary.md"

        completed = self.run_cli(
            base="not-a-real-revision",
            head=head,
            max_bytes=10,
            stdin_paths=f"{unusual_path}\n",
            summary_path=summary,
        )

        self.assertEqual(completed.returncode, 1, completed.stderr)
        contents = summary.read_text(encoding="utf-8")
        self.assertIn("<code>a&#124;`b`.txt</code>", contents)
        self.assertIn("| Path | Size | Status |", contents)
        self.assertNotIn("| Kind |", contents)


if __name__ == "__main__":
    unittest.main()
