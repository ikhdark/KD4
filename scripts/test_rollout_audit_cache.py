from __future__ import annotations

import contextlib
import hashlib
import io
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import kd4_turn_latency_audit as audit
from scripts import rollout_audit_cache as cache
from scripts import rollout_snapshot


class RolloutAuditCacheTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / "sessions" / "trace.jsonl"
        self.source.parent.mkdir()
        self.source.write_text(
            '{"type":"session_meta","payload":{"cwd":"one"}}\n', encoding="utf8"
        )
        self.cache = self.root / "cache"

    def run_audit(self, **kwargs):
        return audit.analyze_session_path(
            self.source, self.root, cache_dir=self.cache, **kwargs
        )

    def test_capture_and_analysis_share_one_decode_with_error_coverage(self):
        lines = [b'{"type":"session_meta","payload":{"cwd":"one"}}\n',
                 b'invalid\n', b'[]\n', b'\xff\n']
        self.source.write_bytes(b"".join(lines))
        decode = json.loads
        counts = {line: 0 for line in lines}

        def counted(value, *args, **kwargs):
            if isinstance(value, bytes) and value in counts:
                counts[value] += 1
            return decode(value, *args, **kwargs)

        with mock.patch.object(cache.json, "loads", side_effect=counted):
            first = self.run_audit()
        self.assertEqual(list(counts.values()), [1, 1, 1, 1])
        self.assertEqual(first["coverage"]["lines"], 4)
        self.assertEqual(first["coverage"]["parseErrorCount"], 3)
        direct = audit.analyze_session_path(self.source, self.root)
        self.assertEqual(first["coverage"], direct["coverage"])
        self.source.write_bytes(lines[0])
        changed = self.run_audit()
        self.assertEqual(changed["coverage"]["lines"], 1)
        self.assertEqual(changed["coverage"]["parseErrorCount"], 0)
        self.assertEqual(changed["analysisCache"]["status"], "miss")

    def test_decode_retention_limits_fall_back_without_losing_records(self):
        self.source.write_bytes(b'{}\n{}\n')
        for limit in ("MAX_DECODED_WIRE_BYTES", "MAX_DECODED_RECORDS"):
            with self.subTest(limit=limit), mock.patch.object(cache, limit, 1):
                report = self.run_audit(refresh=True)
            self.assertEqual(report["coverage"]["lines"], 2)
            self.assertEqual(report["coverage"]["parseErrorCount"], 0)

    def test_reuse_preserves_report_identity_and_skips_analysis(self):
        first = self.run_audit()
        path = Path(first["analysisCache"]["report"])
        self.assertEqual(hashlib.sha256(path.read_bytes()).hexdigest(), path.stem)
        with mock.patch.object(
            audit, "_population_report", side_effect=AssertionError("recomputed")
        ):
            second = self.run_audit()
        self.assertEqual(second["analysisCache"]["status"], "hit")
        self.assertEqual(second["observedAt"], first["observedAt"])
        self.assertEqual(second["analysisCache"]["report"], str(path))
        self.assertEqual(second["evidence_lineage"], first["evidence_lineage"])
        self.assertEqual(
            audit.bounded_summary(second)["evidence_lineage"], first["evidence_lineage"]
        )
        first.pop("analysisCache")
        second.pop("analysisCache")
        self.assertEqual(first, second)
        self.assertEqual(
            self.run_audit(refresh=True)["analysisCache"]["status"], "miss"
        )

    def test_same_size_edit_preserved_mtime_append_and_deletion_invalidate(self):
        first = self.run_audit()
        stat = self.source.stat()
        self.source.write_bytes(self.source.read_bytes().replace(b"one", b"two"))
        os.utime(self.source, ns=(stat.st_atime_ns, stat.st_mtime_ns))
        changed = self.run_audit()
        self.assertNotEqual(
            first["inputProvenance"]["sha256"], changed["inputProvenance"]["sha256"]
        )
        self.assertNotEqual(first["evidence_lineage"], changed["evidence_lineage"])
        with self.source.open("a", encoding="utf8") as out:
            out.write('{"type":"event_msg","payload":{}}\n')
        self.assertEqual(self.run_audit()["analysisCache"]["status"], "miss")
        self.source.unlink()
        deleted = self.run_audit()
        self.assertEqual(deleted["coverage"]["files"], 0)
        self.assertEqual(deleted["analysisCache"]["status"], "miss")

    def test_scope_membership_options_and_analyzer_are_inputs(self):
        first = audit.analyze_session_path(
            self.source.parent, self.root, cache_dir=self.cache
        )
        sibling = self.source.with_name("other.jsonl")
        sibling.write_bytes(self.source.read_bytes())
        second = audit.analyze_session_path(
            self.source.parent, self.root, cache_dir=self.cache
        )
        self.assertEqual(second["coverage"]["files"], 2)
        self.assertNotEqual(
            first["inputProvenance"]["sha256"], second["inputProvenance"]["sha256"]
        )
        sibling.unlink()
        self.assertEqual(
            audit.analyze_session_path(
                self.source.parent, self.root, cache_dir=self.cache
            )["analysisCache"]["status"],
            "hit",
        )
        original = self.run_audit()
        self.assertEqual(
            self.run_audit(include_tokens=False)["analysisCache"]["status"], "miss"
        )
        with mock.patch.object(
            cache, "analyzer_identity", return_value={"changed": "implementation"}
        ):
            changed = self.run_audit()
        self.assertNotEqual(
            original["inputProvenance"]["sha256"], changed["inputProvenance"]["sha256"]
        )

    def test_payloads_reauthenticate_on_hits_without_reparsing_them(self):
        data = b'{"type":"event_msg","payload":{"type":"example"}}'
        digest = hashlib.sha256(data).hexdigest()
        directory = rollout_snapshot.rollout_payload_root(self.source)
        directory.mkdir()
        artifact = directory / f"{digest}.json"
        artifact.write_bytes(data)
        self.source.write_text(
            json.dumps(
                {
                    "type": "rollout_payload_artifact",
                    "payload": {
                        "sha256": digest,
                        "bytes": len(data),
                        "item_type": "event_msg",
                    },
                }
            )
            + "\n",
            encoding="utf8",
        )
        with mock.patch.object(
            rollout_snapshot,
            "load_rollout_payload",
            wraps=rollout_snapshot.load_rollout_payload,
        ) as load:
            self.run_audit()
            self.assertEqual(load.call_count, 1)
        with mock.patch.object(
            rollout_snapshot,
            "_hydrate_verified_payload",
            side_effect=AssertionError("reparsed"),
        ):
            self.assertEqual(self.run_audit()["analysisCache"]["status"], "hit")
        stat = artifact.stat()
        artifact.write_bytes(data.replace(b"example", b"changed"))
        os.utime(artifact, ns=(stat.st_atime_ns, stat.st_mtime_ns))
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            self.run_audit()
        artifact.unlink()
        with self.assertRaises(FileNotFoundError):
            self.run_audit()

    def test_corrupt_cache_and_unwritable_cache_never_authorize_reuse(self):
        first = self.run_audit()
        Path(first["analysisCache"]["report"]).write_text("{}", encoding="utf8")
        self.assertEqual(self.run_audit()["analysisCache"]["status"], "miss")
        with mock.patch.object(
            cache, "write_bytes_atomic", side_effect=PermissionError
        ):
            result = self.run_audit(refresh=True)
        self.assertEqual(result["analysisCache"]["status"], "unavailable")
        self.assertEqual(result["coverage"]["files"], 1)

    def test_startup_log_and_diagnostics_are_inputs_and_provider_files_bypass(self):
        startup = self.root / "startup.log"
        startup.write_text("{}\n", encoding="utf8")
        first = self.run_audit(startup_log=startup)
        self.assertEqual(
            self.run_audit(startup_log=startup)["analysisCache"]["status"], "hit"
        )
        startup.write_text("bad\n", encoding="utf8")
        changed = self.run_audit(startup_log=startup)
        self.assertNotEqual(
            first["inputProvenance"]["sha256"], changed["inputProvenance"]["sha256"]
        )
        # The analyzer validates annotation schemas itself; a new value must
        # reach that owner instead of reusing an earlier report.
        with self.assertRaises(ValueError):
            self.run_audit(diagnostic_evidence={"schemaVersion": -1})
        report = self.run_audit(
            runner_evidence={
                "schemaVersion": 1,
                "events": [],
                "providerRequestsPath": str(self.root / "missing"),
            }
        )
        self.assertEqual(report["analysisCache"]["status"], "bypassed")

    def test_cli_exposes_result_address_and_requires_explicit_cache(self):
        for expected in ("miss", "hit"):
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(
                    audit.main(
                        [
                            str(self.source),
                            "--repo-root",
                            str(self.root),
                            "--cache-dir",
                            str(self.cache),
                            "--summary-json",
                        ]
                    ),
                    0,
                )
            value = json.loads(out.getvalue())
            self.assertEqual(value["analysisCache"]["status"], expected)
            self.assertTrue(Path(value["analysisCache"]["report"]).is_file())
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            audit.main([str(self.source), "--refresh"])

    def test_path_spelling_affecting_population_and_coverage_is_in_the_key(self):
        first = self.run_audit()
        with contextlib.chdir(self.root):
            relative = audit.analyze_session_path(
                Path("sessions/trace.jsonl"), Path("."), cache_dir=self.cache
            )
        self.assertNotEqual(
            first["inputProvenance"]["sha256"], relative["inputProvenance"]["sha256"]
        )
        self.assertEqual(relative["analysisCache"]["status"], "miss")

    def test_directory_snapshots_share_authenticated_payload_bytes(self):
        data = b'{"type":"event_msg","payload":{}}'
        digest = hashlib.sha256(data).hexdigest()
        directory = rollout_snapshot.rollout_payload_root(self.source)
        directory.mkdir()
        (directory / f"{digest}.json").write_bytes(data)
        reference = (
            json.dumps(
                {
                    "type": "rollout_payload_artifact",
                    "payload": {
                        "sha256": digest,
                        "bytes": len(data),
                        "item_type": "event_msg",
                    },
                }
            )
            + "\n"
        )
        self.source.write_text(reference, encoding="utf8")
        self.source.with_name("second.jsonl").write_text(reference, encoding="utf8")
        with mock.patch.object(
            rollout_snapshot,
            "load_rollout_payload",
            wraps=rollout_snapshot.load_rollout_payload,
        ) as load:
            report = audit.analyze_session_path(
                self.source.parent, self.root, cache_dir=self.cache
            )
            self.assertEqual(load.call_count, 1)
        self.assertEqual(report["coverage"]["files"], 2)
        self.assertEqual(len(report["inputProvenance"]["inputs"]["payloads"]), 1)

    def test_compressed_snapshots_can_be_replayed_without_changing_identity(self):
        try:
            from compression import zstd
        except ImportError:
            from backports import zstd
        compressed = self.source.with_name(self.source.name + ".zst")
        compressed.write_bytes(zstd.compress(self.source.read_bytes()))
        self.source.unlink()
        self.assertEqual(self.run_audit()["analysisCache"]["status"], "miss")
        self.assertEqual(self.run_audit()["analysisCache"]["status"], "hit")

    def test_compressed_decode_retention_is_bounded_by_expanded_bytes(self):
        try:
            from compression import zstd
        except ImportError:
            from backports import zstd
        compressed = self.source.with_name(self.source.name + ".zst")
        compressed.write_bytes(zstd.compress(b'{}\n' * 1000))
        self.assertLess(compressed.stat().st_size, 100)

        def analyze(*args, **kwargs):
            self.assertEqual(kwargs["_decoded"], {})
            return audit.analyze_session_path(*args, **kwargs)

        with mock.patch.object(cache, "MAX_DECODED_WIRE_BYTES", 100):
            report = cache.analyze_cached(analyze, compressed, self.root, cache_dir=self.cache)
        self.assertEqual(report["coverage"]["lines"], 1000)
        self.assertEqual(report["coverage"]["parseErrorCount"], 0)

    def test_live_append_during_analysis_belongs_to_next_snapshot(self):
        original = audit._population_report
        appended = False

        def append(*args, **kwargs):
            nonlocal appended
            if not appended:
                appended = True
                with self.source.open("a", encoding="utf8") as out:
                    out.write('{"type":"event_msg","payload":{}}\n')
            return original(*args, **kwargs)

        with mock.patch.object(audit, "_population_report", append):
            first = self.run_audit()
        self.assertEqual(first["coverage"]["lines"], 1)
        next_report = self.run_audit()
        self.assertEqual(next_report["coverage"]["lines"], 2)
        self.assertEqual(next_report["analysisCache"]["status"], "miss")
