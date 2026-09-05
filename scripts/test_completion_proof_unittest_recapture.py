from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "completion_proof_unittest_recapture.py"
FROZEN_INVENTORY = REPO_ROOT / ".codex" / "validation" / "frozen-test-inventory-v1.json"
BASELINE_COMMIT = "60bb133fa0a4f25e83851ab16d8c462e5f42ff95"
BASELINE_TREE_OBJECT = "c2d4bf589a595f6d451c50333317c675d7781958"
SOURCE_TREE_SHA256 = "654591dd1ddda7a77312172ec7c70e60e80990590c7c74a7c3b08a445279d90e"
FROZEN_INVENTORY_RAW_SHA256 = "df230a7683f0f31f1aae4d3f7644af39cec67b09fadf8f3f1e6c60729d18196a"
PARENT_RECORDS_SHA256 = "a46a941721c872655dcb1c4ca55c070b9f48008a451d0df283f2d69957c2dd07"
EXPECTED_GAP = (
    "scripts.test_completion_proof.CompletionProofCliTest."
    "test_executed_failure_is_confirmed_and_reported",
    "scripts.test_completion_proof.CompletionProofCliTest."
    "test_existing_report_is_rejected_without_reusing_cached_content",
    "scripts.test_completion_proof.CompletionProofCliTest."
    "test_real_cli_records_fresh_execution_for_each_attempt",
    "scripts.test_completion_proof.CompletionProofCliTest."
    "test_unmapped_current_inventory_blocks_before_validation",
    "scripts.test_completion_proof.CompletionProofCliTest."
    "test_zero_selection_is_a_pre_result_error",
)


def _canonical(value: object) -> bytes:
    return json.dumps(
        value, ensure_ascii=False, allow_nan=False, sort_keys=True, separators=(",", ":")
    ).encode("utf-8")


def _run(
    command: list[str],
    cwd: Path,
    *,
    environment_overrides: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[bytes]:
    environment = os.environ.copy()
    for name in (
        "ALL_PROXY", "HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY",
        "all_proxy", "https_proxy", "http_proxy", "no_proxy",
    ):
        environment.pop(name, None)
    environment["CODEX_NETWORK_ALLOW_LOCAL_BINDING"] = "1"
    if environment_overrides:
        environment.update(environment_overrides)
    return subprocess.run(
        command,
        cwd=cwd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=environment,
        check=False,
    )


def _proof_hash(domain: str, value: object) -> str:
    return hashlib.sha256(domain.encode("ascii") + b"\0" + _canonical(value)).hexdigest()


def _write_frozen_parent_manifest(path: Path) -> None:
    inventory_raw = FROZEN_INVENTORY.read_bytes()
    if hashlib.sha256(inventory_raw).hexdigest() != FROZEN_INVENTORY_RAW_SHA256:
        raise AssertionError("frozen V1 inventory raw identity changed")
    inventory = json.loads(inventory_raw)
    rows = sorted(
        (
            row for row in inventory["tests"]
            if row["framework"] == "python-unittest"
            and not row["baseline_id"].startswith("hidden-at-freeze-v1::")
        ),
        key=lambda row: row["baseline_id"],
    )
    records = [
        {
            "baseline_id": row["baseline_id"],
            "native_id": row["native_id"],
            "predecessor_entry_sha256": _proof_hash(
                "kd4.frozen-v1-inventory-entry.v1", row
            ),
        }
        for row in rows
    ]
    if len(records) != 893:
        raise AssertionError("frozen unittest parent count changed")
    if _proof_hash(
        "kd4.unittest-recapture-parent-record-set.v1", records
    ) != PARENT_RECORDS_SHA256:
        raise AssertionError("frozen unittest parent record identity changed")
    path.write_bytes(
        _canonical(
            {
                "baseline_commit": BASELINE_COMMIT,
                "format_id": "kd4.unittest-parent-manifest.v1",
                "frozen_inventory_raw_sha256": FROZEN_INVENTORY_RAW_SHA256,
                "parent_records": records,
                "schema_version": 1,
                "source_tree_sha256": SOURCE_TREE_SHA256,
            }
        )
    )


def _controller_command(repo_root: Path, manifest: Path, output: Path) -> list[str]:
    return [
        sys.executable,
        str(SCRIPT),
        "controller",
        "--repo-root",
        str(repo_root),
        "--manifest",
        str(manifest),
        "--output",
        str(output),
    ]


def _write_git_wrapper(root: Path, mode: str) -> tuple[Path, dict[str, str]]:
    real_git = shutil.which("git")
    if real_git is None:
        raise AssertionError("Git is required for controller integration tests")
    driver = root / "git-wrapper.py"
    driver.write_text(
        textwrap.dedent(
            """
            import os
            from pathlib import Path
            import subprocess
            import sys

            arguments = sys.argv[1:]
            mode = os.environ["KD4_GIT_WRAPPER_MODE"]
            real_git = os.environ["KD4_REAL_GIT"]
            if (
                mode == "wrong-tree"
                and "cat-file" in arguments
                and "-p" in arguments
                and "60bb133fa0a4f25e83851ab16d8c462e5f42ff95" in arguments
            ):
                sys.stdout.write(
                    "tree 0000000000000000000000000000000000000000\\n"
                    "author Fixture <fixture@example.invalid> 0 +0000\\n\\nwrong\\n"
                )
                raise SystemExit(0)
            if mode == "object-audit-failure" and "fsck" in arguments:
                sys.stderr.write("injected object audit failure\\n")
                raise SystemExit(91)
            if mode == "clone-failure" and "clone" in arguments:
                sys.stderr.write("injected clone failure\\n")
                raise SystemExit(92)
            if mode == "checkout-failure" and "clone" in arguments:
                destination = Path(arguments[-1])
                completed = subprocess.run(
                    [real_git, "init", "--quiet", str(destination)], check=False
                )
                raise SystemExit(completed.returncode)
            if mode == "checkout-failure" and "checkout" in arguments:
                sys.stderr.write("injected checkout failure\\n")
                raise SystemExit(93)
            completed = subprocess.run([real_git, *arguments], check=False)
            raise SystemExit(completed.returncode)
            """
        ),
        encoding="utf-8",
    )
    if os.name == "nt":
        launcher = root / "git.cmd"
        launcher.write_text(
            f'@"{sys.executable}" "{driver}" %*\r\n', encoding="utf-8"
        )
    else:
        launcher = root / "git"
        launcher.write_text(
            f'#!{sys.executable}\nexec(compile(open({str(driver)!r}, "rb").read(), '
            f'{str(driver)!r}, "exec"))\n',
            encoding="utf-8",
        )
        launcher.chmod(0o755)
    environment = {
        "KD4_GIT_WRAPPER_MODE": mode,
        "KD4_REAL_GIT": real_git,
        "PATH": str(root) + os.pathsep + os.environ.get("PATH", ""),
    }
    return launcher, environment


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
            from unittest import mock

            class SampleTests(unittest.TestCase):
                @mock.patch("time.time", return_value=0)
                def test_nested(self, clock):
                    repository_path = Path(__file__).resolve().parents[1] / "data.txt"
                    for i in (1, 1):
                        with self.subTest("outer", i=i, ratio=1.5, negative_zero=-0.0, repository_path=repository_path):
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
            self.assertIn('"bits":"3ff8000000000000","kind":"float64"', encoded)
            self.assertIn('"bits":"8000000000000000","kind":"float64"', encoded)
            self.assertNotIn("repr", encoded)

    def test_worker_entrypoint_fails_closed_for_unsupported_parameter(self) -> None:
        source = """
            import unittest

            class SampleTests(unittest.TestCase):
                def test_bad(self):
                    with self.subTest(value=object()):
                        pass

                def test_nonfinite(self):
                    with self.subTest(value=float("inf")):
                        pass
        """
        with tempfile.TemporaryDirectory(prefix="kd4-unittest-worker-test-") as name:
            fixture = WorkerFixture(Path(name), source, ["test_bad", "test_nonfinite"])
            sentinel = b"preserve-existing-worker-output\n"
            fixture.output.write_bytes(sentinel)
            completed = _run(fixture.worker_command(), fixture.root)
            self.assertNotEqual(completed.returncode, 0)
            self.assertEqual(fixture.output.read_bytes(), sentinel)
            self.assertIn(b"unsupported subtest parameter type", completed.stderr)
            self.assertIn(b"non-finite", completed.stderr)

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

    def test_controller_rejects_unapproved_exception_extension_before_execution(self) -> None:
        approved = json.loads((REPO_ROOT / ".codex/validation/frozen-test-inventory-v2-unittest-source-exceptions.json").read_bytes())
        with tempfile.TemporaryDirectory(prefix="kd4-unittest-controller-test-") as name:
            root = Path(name)
            manifest = root / "parents.json"
            output = root / "public-report.json"
            exception_path = root / "exceptions.json"
            _write_frozen_parent_manifest(manifest)
            sentinel = b"existing-authority-must-survive\n"
            output.write_bytes(sentinel)
            tampered = json.loads(_canonical(approved))
            tampered["historical_execution_extension"]["baseline_ids"].append(
                "python-unittest::unapproved.thirtieth.parent"
            )
            exception_path.write_bytes(_canonical(tampered))
            completed = _run(
                _controller_command(REPO_ROOT, manifest, output)
                + ["--source-exceptions", str(exception_path), "--codex-executable", "unexecuted-codex.exe"],
                REPO_ROOT,
            )
            self.assertNotEqual(completed.returncode, 0)
            self.assertIn(b"exact approved five-source and 29-parent amendments", completed.stderr)
            self.assertEqual(output.read_bytes(), sentinel)
            self.assertFalse(list(root.glob("unittest-attempt-*")))

    def test_controller_reconstructs_exact_clean_baseline_and_reports_five_parent_gap(self) -> None:
        with tempfile.TemporaryDirectory(prefix="kd4-unittest-controller-test-") as name:
            root = Path(name)
            manifest = root / "frozen-parent-manifest.json"
            output = root / "public-report.json"
            sentinel = b"existing-authority-must-survive\n"
            _write_frozen_parent_manifest(manifest)
            output.write_bytes(sentinel)

            completed = _run(
                _controller_command(REPO_ROOT, manifest, output), REPO_ROOT
            )

            self.assertNotEqual(completed.returncode, 0)
            self.assertEqual(output.read_bytes(), sentinel)
            self.assertIn(b"clean baseline reconstruction contains 888 parents", completed.stderr)
            self.assertIn(b"recapture worker was not launched", completed.stderr)
            for native_id in EXPECTED_GAP:
                self.assertIn(native_id.encode("utf-8"), completed.stderr)

    def test_controller_rejects_proxy_environment_before_clone(self) -> None:
        with tempfile.TemporaryDirectory(prefix="kd4-unittest-controller-test-") as name:
            root = Path(name)
            manifest = root / "frozen-parent-manifest.json"
            output = root / "public-report.json"
            sentinel = b"existing-authority-must-survive\n"
            _write_frozen_parent_manifest(manifest)
            output.write_bytes(sentinel)

            completed = _run(
                _controller_command(REPO_ROOT, manifest, output),
                REPO_ROOT,
                environment_overrides={"HTTPS_PROXY": "http://127.0.0.1:9"},
            )

            self.assertNotEqual(completed.returncode, 0)
            self.assertEqual(output.read_bytes(), sentinel)
            self.assertIn(b"controller proxy environment must be cleared", completed.stderr)

    def test_controller_rejects_repository_without_exact_baseline_commit(self) -> None:
        with tempfile.TemporaryDirectory(prefix="kd4-unittest-controller-test-") as name:
            root = Path(name)
            repository = root / "wrong-repository"
            repository.mkdir()
            for command in (
                ["git", "init", "--quiet"],
                ["git", "config", "user.email", "fixture@example.invalid"],
                ["git", "config", "user.name", "Fixture"],
                ["git", "commit", "--quiet", "--allow-empty", "-m", "wrong"],
            ):
                completed = _run(command, repository)
                self.assertEqual(
                    completed.returncode, 0, completed.stderr.decode("utf-8", "replace")
                )
            manifest = root / "frozen-parent-manifest.json"
            output = root / "public-report.json"
            sentinel = b"existing-authority-must-survive\n"
            _write_frozen_parent_manifest(manifest)
            output.write_bytes(sentinel)

            completed = _run(
                _controller_command(repository, manifest, output), REPO_ROOT
            )

            self.assertNotEqual(completed.returncode, 0)
            self.assertEqual(output.read_bytes(), sentinel)
            self.assertIn(BASELINE_COMMIT.encode("ascii"), completed.stderr)

    def test_controller_preserves_output_for_tree_clone_checkout_and_object_failures(self) -> None:
        for mode, expected in (
            ("wrong-tree", b"frozen baseline tree object mismatch"),
            ("object-audit-failure", b"injected object audit failure"),
            ("clone-failure", b"injected clone failure"),
            ("checkout-failure", b"injected checkout failure"),
        ):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory(
                prefix="kd4-unittest-controller-test-"
            ) as name:
                root = Path(name)
                manifest = root / "frozen-parent-manifest.json"
                output = root / "public-report.json"
                sentinel = b"existing-authority-must-survive\n"
                _write_frozen_parent_manifest(manifest)
                output.write_bytes(sentinel)
                _, environment = _write_git_wrapper(root, mode)

                completed = _run(
                    _controller_command(REPO_ROOT, manifest, output),
                    REPO_ROOT,
                    environment_overrides=environment,
                )

                self.assertNotEqual(completed.returncode, 0)
                self.assertEqual(output.read_bytes(), sentinel)
                self.assertIn(expected, completed.stderr)


if __name__ == "__main__":
    unittest.main()
