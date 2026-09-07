#!/usr/bin/env python3

from __future__ import annotations

import contextlib
import io
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from scripts import check_blob_size
from scripts.check_blob_size import ChangedBlob


class CheckBlobSizeTest(unittest.TestCase):
    def test_run_git_is_anchored_at_repo_root(self) -> None:
        with mock.patch.object(check_blob_size.subprocess, "run") as run:
            run.return_value.stdout = "ok\n"

            output = check_blob_size.run_git("status", "--short")

        self.assertEqual(output, "ok\n")
        run.assert_called_once()
        self.assertEqual(run.call_args.kwargs["cwd"], check_blob_size.REPO_ROOT)

    def test_collect_changed_blobs_batches_diff_and_cat_file(self) -> None:
        calls: list[tuple[tuple[str, ...], str | None]] = []

        def fake_git(*args: str, input_text: str | None = None) -> str:
            calls.append((args, input_text))
            if args[:2] == ("diff", "--numstat"):
                return "10\t2\ttext.txt\0-\t-\timage.bin\0"
            if args[:3] == (
                "cat-file",
                "-Z",
                "--batch-check=%(objecttype) %(objectsize)",
            ):
                self.assertEqual(input_text, "HEAD:text.txt\0HEAD:image.bin\0")
                return "blob 12\x00blob 600000\x00"
            raise AssertionError(f"unexpected git call: {args}")

        blobs = check_blob_size.collect_changed_blobs(
            "BASE",
            "HEAD",
            {"image.bin"},
            include_kind=True,
            run_git_func=fake_git,
        )

        self.assertEqual(
            blobs,
            [
                ChangedBlob("text.txt", 12, False, False),
                ChangedBlob("image.bin", 600000, True, True),
            ],
        )
        self.assertEqual(len(calls), 2)

    def test_explicit_paths_skip_diff_when_kind_is_not_requested(self) -> None:
        calls: list[tuple[tuple[str, ...], str | None]] = []

        def fake_git(*args: str, input_text: str | None = None) -> str:
            calls.append((args, input_text))
            if args[:3] == (
                "cat-file",
                "-Z",
                "--batch-check=%(objecttype) %(objectsize)",
            ):
                return "blob 1\x00blob 2\x00"
            raise AssertionError(f"unexpected git call: {args}")

        blobs = check_blob_size.collect_changed_blobs(
            "BASE",
            "HEAD",
            set(),
            paths=["a.txt", "b.txt"],
            include_kind=False,
            run_git_func=fake_git,
        )

        self.assertEqual(
            blobs,
            [
                ChangedBlob("a.txt", 1, False, False),
                ChangedBlob("b.txt", 2, False, False),
            ],
        )
        self.assertEqual(len(calls), 1)

    def test_explicit_paths_with_kind_preserve_paths_missing_from_diff(self) -> None:
        def fake_git(*args: str, input_text: str | None = None) -> str:
            if args[:2] == ("diff", "--numstat"):
                self.assertNotIn("a.txt", args)
                self.assertNotIn("b.bin", args)
                return "-\t-\tb.bin\0"
            if args[:3] == (
                "cat-file",
                "-Z",
                "--batch-check=%(objecttype) %(objectsize)",
            ):
                self.assertEqual(input_text, "HEAD:a.txt\0HEAD:b.bin\0")
                return "blob 1\x00blob 2\x00"
            raise AssertionError(f"unexpected git call: {args}")

        blobs = check_blob_size.collect_changed_blobs(
            "BASE",
            "HEAD",
            set(),
            paths=["a.txt", "b.bin"],
            include_kind=True,
            run_git_func=fake_git,
        )

        self.assertEqual(
            blobs,
            [
                ChangedBlob("a.txt", 1, False, False),
                ChangedBlob("b.bin", 2, False, True),
            ],
        )

    def test_explicit_paths_with_kind_do_not_expand_git_command_line(self) -> None:
        paths = [f"generated/path-{index:05d}.bin" for index in range(10_000)]

        def fake_git(*args: str, input_text: str | None = None) -> str:
            self.assertLess(len(args), 10)
            self.assertIsNone(input_text)
            return ""

        changed = check_blob_size.get_changed_paths(
            "BASE",
            "HEAD",
            include_kind=True,
            paths=paths,
            run_git_func=fake_git,
        )

        self.assertEqual(len(changed), len(paths))
        self.assertFalse(any(path.is_binary for path in changed))

    def test_parse_paths_accepts_newline_or_nul_delimiters(self) -> None:
        self.assertEqual(check_blob_size.parse_paths("a\n\nb\n"), ["a", "b"])
        self.assertEqual(check_blob_size.parse_paths("a\0\0b\0"), ["a", "b"])

    def test_batch_blob_sizes_preserves_newlines_in_paths(self) -> None:
        def fake_git(*args: str, input_text: str | None = None) -> str:
            self.assertEqual(
                args,
                (
                    "cat-file",
                    "-Z",
                    "--batch-check=%(objecttype) %(objectsize)",
                ),
            )
            self.assertEqual(input_text, "HEAD:docs/a\nb.txt\0HEAD:c.txt\0")
            return "blob 123\x00blob 456\x00"

        sizes = check_blob_size.batch_blob_sizes(
            "HEAD", ["docs/a\nb.txt", "c.txt"], run_git_func=fake_git
        )

        self.assertEqual(sizes, {"docs/a\nb.txt": 123, "c.txt": 456})

    def test_batch_blob_sizes_reports_missing_path_cleanly(self) -> None:
        def fake_git(*args: str, input_text: str | None = None) -> str:
            return "HEAD:missing.txt missing\0"

        with self.assertRaisesRegex(
            check_blob_size.BlobLookupError,
            "'missing.txt' does not exist as a blob at 'HEAD'",
        ):
            check_blob_size.batch_blob_sizes(
                "HEAD", ["missing.txt"], run_git_func=fake_git
            )

    def test_batch_blob_sizes_rejects_non_blob_object(self) -> None:
        def fake_git(*args: str, input_text: str | None = None) -> str:
            return "commit 245\0"

        with self.assertRaisesRegex(
            check_blob_size.BlobLookupError,
            "'vendor/submodule' is not a blob at 'HEAD'",
        ):
            check_blob_size.batch_blob_sizes(
                "HEAD", ["vendor/submodule"], run_git_func=fake_git
            )

    def test_main_reports_blob_lookup_error_without_traceback(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            allowlist = Path(temp_dir) / "allowlist.txt"
            allowlist.write_text("", encoding="utf-8")
            stderr = io.StringIO()
            with (
                mock.patch.object(
                    check_blob_size,
                    "collect_changed_blobs",
                    side_effect=check_blob_size.BlobLookupError(
                        "'missing.txt' does not exist as a blob at 'HEAD'"
                    ),
                ),
                contextlib.redirect_stderr(stderr),
            ):
                exit_code = check_blob_size.main(
                    [
                        "--base",
                        "BASE",
                        "--head",
                        "HEAD",
                        "--allowlist",
                        str(allowlist),
                    ]
                )

        self.assertEqual(exit_code, 2)
        self.assertIn("missing.txt", stderr.getvalue())
        self.assertNotIn("Traceback", stderr.getvalue())

    def test_allowlist_preserves_hash_in_paths(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            allowlist = Path(temp_dir) / "allowlist.txt"
            allowlist.write_text(
                "# comment\nassets/icon#dark.png\nassets/large.bin # explanation\n",
                encoding="utf-8",
            )

            loaded = check_blob_size.load_allowlist(allowlist)

        self.assertEqual(loaded, {"assets/icon#dark.png", "assets/large.bin"})

    def test_step_summary_escapes_markdown_table_cells(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            summary_path = Path(temp_dir) / "summary.md"
            old_summary = os.environ.get("GITHUB_STEP_SUMMARY")
            os.environ["GITHUB_STEP_SUMMARY"] = str(summary_path)
            try:
                check_blob_size.write_step_summary(
                    10,
                    [ChangedBlob("docs/a|`b`.txt", 20, False, False)],
                    [ChangedBlob("docs/a|`b`.txt", 20, False, False)],
                    include_kind=False,
                )
            finally:
                if old_summary is None:
                    os.environ.pop("GITHUB_STEP_SUMMARY", None)
                else:
                    os.environ["GITHUB_STEP_SUMMARY"] = old_summary

            summary = summary_path.read_text(encoding="utf-8")

        self.assertIn("<code>docs/a&#124;`b`.txt</code>", summary)
        self.assertIn("| Path | Size | Status |", summary)
        self.assertNotIn("| Kind |", summary)


if __name__ == "__main__":
    unittest.main()
