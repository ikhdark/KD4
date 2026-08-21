from __future__ import annotations

import subprocess
import unittest

from scripts import check_duplicate_deps


class CheckDuplicateDepsTest(unittest.TestCase):
    def test_empty_duplicate_report_passes(self) -> None:
        def runner(*args: object, **kwargs: object) -> subprocess.CompletedProcess[str]:
            return subprocess.CompletedProcess(
                args=[], returncode=0, stdout="", stderr=""
            )

        self.assertEqual(
            check_duplicate_deps.check_duplicate_deps([], runner=runner), 0
        )

    def test_duplicate_report_fails(self) -> None:
        def runner(*args: object, **kwargs: object) -> subprocess.CompletedProcess[str]:
            return subprocess.CompletedProcess(
                args=[], returncode=0, stdout="foo v1.0.0\nfoo v2.0.0\n", stderr=""
            )

        self.assertEqual(
            check_duplicate_deps.check_duplicate_deps([], runner=runner), 1
        )

    def test_cargo_failure_is_preserved(self) -> None:
        def runner(*args: object, **kwargs: object) -> subprocess.CompletedProcess[str]:
            return subprocess.CompletedProcess(
                args=[], returncode=7, stdout="", stderr="boom\n"
            )

        self.assertEqual(
            check_duplicate_deps.check_duplicate_deps([], runner=runner), 7
        )


if __name__ == "__main__":
    unittest.main()
