from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "completion_proof_unittest_recapture.py"


def _canonical(value: object) -> bytes:
    return json.dumps(
        value, ensure_ascii=False, allow_nan=False, sort_keys=True, separators=(",", ":")
    ).encode("utf-8")


def _run(command: list[str], cwd: Path) -> subprocess.CompletedProcess[bytes]:
    environment = os.environ.copy()
    for name in (
        "ALL_PROXY", "HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY",
        "all_proxy", "https_proxy", "http_proxy", "no_proxy",
    ):
        environment.pop(name, None)
    environment["CODEX_NETWORK_ALLOW_LOCAL_BINDING"] = "1"
    return subprocess.run(
        command,
        cwd=cwd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=environment,
        check=False,
    )


class WorkerFixture:
    def __init__(self, root: Path, source: str, methods: list[str]) -> None:
        self.root = root
        package = root / "fixture_tests"
        package.mkdir(parents=True)
        (package / "__init__.py").write_text("", encoding="utf-8")
        (package / "test_sample.py").write_text(textwrap.dedent(source), encoding="utf-8")
        (root / "data.txt").write_text("fixture\n", encoding="utf-8")
        for command in (
            ["git", "init", "--quiet"],
            ["git", "config", "user.email", "fixture@example.invalid"],
            ["git", "config", "user.name", "Fixture"],
            ["git", "config", "core.autocrlf", "false"],
            ["git", "add", "fixture_tests", "data.txt"],
            ["git", "commit", "--quiet", "-m", "fixture"],
        ):
            completed = _run(command, root)
            if completed.returncode:
                raise AssertionError(completed.stderr.decode("utf-8", "replace"))
        self.head = _run(["git", "rev-parse", "HEAD"], root).stdout.decode().strip()
        tree = _run(["git", "ls-tree", "-r", "--full-tree", "HEAD"], root).stdout
        self.tree_sha256 = hashlib.sha256(tree).hexdigest()
        records = []
        for method in methods:
            native = f"fixture_tests.test_sample.SampleTests.{method}"
            records.append(
                {
                    "baseline_id": "fixture::python-unittest::" + native,
                    "native_id": native,
                    "predecessor_entry_sha256": hashlib.sha256(native.encode()).hexdigest(),
                }
            )
        self.manifest = {
            "baseline_commit": self.head,
            "format_id": "kd4.unittest-parent-manifest.v1",
            "frozen_inventory_raw_sha256": hashlib.sha256(b"fixture").hexdigest(),
            "parent_records": records,
            "schema_version": 1,
            "source_tree_sha256": self.tree_sha256,
        }
        self.manifest_path = root / ".git" / "fixture-manifest.json"
        self.manifest_raw = _canonical(self.manifest)
        self.manifest_path.write_bytes(self.manifest_raw)
        self.output = root / ".git" / "worker-report.json"

    def worker_command(self) -> list[str]:
        return [
            sys.executable,
            str(SCRIPT),
            "_worker",
            "--source-root",
            str(self.root),
            "--manifest",
            str(self.manifest_path),
            "--output",
            str(self.output),
            "--expected-head",
            self.head,
            "--expected-tree",
            self.tree_sha256,
            "--manifest-sha256",
            hashlib.sha256(self.manifest_raw).hexdigest(),
            "--worker-sha256",
            hashlib.sha256(SCRIPT.read_bytes()).hexdigest(),
        ]


class UnittestRecaptureEntrypointTests(unittest.TestCase):
    """Exercise the private worker as instrumentation, never as proof authority."""

    def test_worker_entrypoint_captures_nested_repeated_subtests_and_exact_sites(self) -> None:
        source = """
            from pathlib import Path
            import unittest

            class SampleTests(unittest.TestCase):
                def test_nested(self):
                    repository_path = Path(__file__).resolve().parents[1] / "data.txt"
                    for i in (1, 1):
                        with self.subTest("outer", i=i, repository_path=repository_path):
                            for j in (2, 3):
                                with self.subTest(j=j, payload=(b"x", {"flags": [True, None]})):
                                    self.assertGreater(j, 0)

                def test_plain(self):
                    self.assertTrue(True)
        """
        with tempfile.TemporaryDirectory(prefix="kd4-unittest-worker-test-") as name:
            fixture = WorkerFixture(Path(name), source, ["test_nested", "test_plain"])
            completed = _run(fixture.worker_command(), fixture.root)
            self.assertEqual(completed.returncode, 0, completed.stderr.decode("utf-8", "replace"))
            raw = fixture.output.read_bytes()
            report = json.loads(raw)
            self.assertEqual(raw, _canonical(report))
            self.assertEqual(report["format_id"], "kd4.unittest-execution-report.v1")
            self.assertNotIn("socket_policy", report)
            self.assertEqual(
                report["untrusted_observations"]["socket_policy"], "loopback-only"
            )
            self.assertEqual(
                report["untrusted_observations"]["checkout"][
                    "execution_working_directory"
                ],
                ".",
            )
            self.assertEqual(len(report["parent_results"]), 2)
            self.assertEqual(len(report["source_site_manifest"]), 2)
            self.assertEqual(report["total_counts"]["subtest_occurrence_count"], 6)
            occurrences = report["subtest_occurrences"]
            groups: dict[str, list[int]] = {}
            for occurrence in occurrences:
                groups.setdefault(occurrence["declared_site_id"], []).append(
                    occurrence["occurrence_ordinal"]
                )
            self.assertEqual(sorted(groups.values()), [[0, 1], [0, 1, 2, 3]])
            projections = [item["canonical_context_projection"] for item in occurrences]
            self.assertTrue(all(item["kind"] == "tuple" for item in projections))
            encoded = raw.decode("utf-8")
            self.assertIn('"kind":"repository-path"', encoded)
            self.assertIn('"value":"data.txt"', encoded)
            self.assertNotIn("repr", encoded)

    def test_worker_entrypoint_fails_closed_for_unsupported_parameter(self) -> None:
        source = """
            import unittest

            class SampleTests(unittest.TestCase):
                def test_bad(self):
                    with self.subTest(value=object()):
                        pass
        """
        with tempfile.TemporaryDirectory(prefix="kd4-unittest-worker-test-") as name:
            fixture = WorkerFixture(Path(name), source, ["test_bad"])
            sentinel = b"preserve-existing-worker-output\n"
            fixture.output.write_bytes(sentinel)
            completed = _run(fixture.worker_command(), fixture.root)
            self.assertNotEqual(completed.returncode, 0)
            self.assertEqual(fixture.output.read_bytes(), sentinel)
            self.assertIn(b"unsupported subtest parameter type", completed.stderr)

    def test_worker_entrypoint_blocks_non_loopback_socket(self) -> None:
        source = """
            import socket
            import unittest

            class SampleTests(unittest.TestCase):
                def test_network(self):
                    sock = socket.socket()
                    try:
                        sock.connect(("8.8.8.8", 53))
                    finally:
                        sock.close()
        """
        with tempfile.TemporaryDirectory(prefix="kd4-unittest-worker-test-") as name:
            fixture = WorkerFixture(Path(name), source, ["test_network"])
            sentinel = b"preserve-existing-worker-output\n"
            fixture.output.write_bytes(sentinel)
            completed = _run(fixture.worker_command(), fixture.root)
            self.assertNotEqual(completed.returncode, 0)
            self.assertEqual(fixture.output.read_bytes(), sentinel)
            self.assertIn(b"unittest parent error", completed.stderr)

    def test_worker_entrypoint_guards_non_ip_socket_during_module_import(self) -> None:
        source = """
            import socket
            import unittest

            sock = socket.socket.__new__(socket.socket)
            try:
                sock.connect(("127.0.0.1", 1))
            finally:
                sock.close()

            class SampleTests(unittest.TestCase):
                def test_network(self):
                    pass
        """
        with tempfile.TemporaryDirectory(prefix="kd4-unittest-worker-test-") as name:
            fixture = WorkerFixture(Path(name), source, ["test_network"])
            sentinel = b"preserve-existing-worker-output\n"
            fixture.output.write_bytes(sentinel)
            completed = _run(fixture.worker_command(), fixture.root)
            self.assertNotEqual(completed.returncode, 0)
            self.assertEqual(fixture.output.read_bytes(), sentinel)
            self.assertIn(b"non-loopback socket connect blocked", completed.stderr)

    def test_worker_entrypoint_rejects_cwd_other_than_source_root(self) -> None:
        source = """
            import unittest

            class SampleTests(unittest.TestCase):
                def test_ok(self):
                    pass
        """
        with tempfile.TemporaryDirectory(prefix="kd4-unittest-worker-test-") as name:
            fixture = WorkerFixture(Path(name), source, ["test_ok"])
            sentinel = b"preserve-existing-worker-output\n"
            fixture.output.write_bytes(sentinel)
            completed = _run(fixture.worker_command(), REPO_ROOT)
            self.assertNotEqual(completed.returncode, 0)
            self.assertEqual(fixture.output.read_bytes(), sentinel)
            self.assertIn(b"current directory does not equal source root", completed.stderr)

    def test_worker_entrypoint_rejects_import_time_cwd_mutation(self) -> None:
        source = """
            import os
            from pathlib import Path
            import unittest

            os.chdir(Path(__file__).resolve().parent)

            class SampleTests(unittest.TestCase):
                def test_ok(self):
                    pass
        """
        with tempfile.TemporaryDirectory(prefix="kd4-unittest-worker-test-") as name:
            fixture = WorkerFixture(Path(name), source, ["test_ok"])
            sentinel = b"preserve-existing-worker-output\n"
            fixture.output.write_bytes(sentinel)
            completed = _run(fixture.worker_command(), fixture.root)
            self.assertNotEqual(completed.returncode, 0)
            self.assertEqual(fixture.output.read_bytes(), sentinel)
            self.assertIn(b"current directory does not equal source root", completed.stderr)

    def test_controller_entrypoint_rejects_nonfrozen_manifest_without_touching_output(self) -> None:
        source = """
            import unittest
            class SampleTests(unittest.TestCase):
                def test_ok(self):
                    pass
        """
        with tempfile.TemporaryDirectory(prefix="kd4-unittest-controller-test-") as name:
            fixture = WorkerFixture(Path(name), source, ["test_ok"])
            public_output = fixture.root / "public-report.json"
            sentinel = b"existing-authority-must-survive\n"
            public_output.write_bytes(sentinel)
            completed = _run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "controller",
                    "--repo-root",
                    str(fixture.root),
                    "--manifest",
                    str(fixture.manifest_path),
                    "--output",
                    str(public_output),
                ],
                REPO_ROOT,
            )
            self.assertNotEqual(completed.returncode, 0)
            self.assertEqual(public_output.read_bytes(), sentinel)
            self.assertIn(b"frozen V1 inventory authority mismatch", completed.stderr)


if __name__ == "__main__":
    unittest.main()
