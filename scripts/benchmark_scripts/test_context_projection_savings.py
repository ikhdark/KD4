from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from scripts.benchmark_scripts.context_projection_savings import (
    audit,
    derived_file_uri,
    project,
)


class DerivedFileUriTest(unittest.TestCase):
    def test_only_unreserved_rooted_paths_are_treated_as_derivable(self):
        # Mirrors output_projection::tests::read_display_omits_canonical_uri_*
        self.assertEqual(
            derived_file_uri(r"C:\repo\src\main.rs"),
            "file:///C:/repo/src/main.rs",
        )
        self.assertEqual(
            derived_file_uri("/home/alice/src/main.rs"),
            "file:///home/alice/src/main.rs",
        )
        # A space, a reserved byte and a non-ASCII byte each need escaping, so
        # none of them may be claimed as a pure re-encoding.
        for path in [
            r"C:\repo\Alice Smith\main.rs",
            r"C:\repo\a?b\main.rs",
            r"C:\repo\100%\main.rs",
            "/home/alice/λ.rs",
        ]:
            self.assertIsNone(derived_file_uri(path), path)
        # Host-owned locators, relative paths and UNC shares are not file URIs
        # of this shape.
        for path in ["skill:reviewer", "context:desktop", "src/main.rs", r"\\srv\share\a.rs"]:
            self.assertIsNone(derived_file_uri(path), path)


class ProjectTest(unittest.TestCase):
    def test_canonical_uri_survives_when_it_is_not_a_re_encoding(self):
        header = {
            "path": r"C:\repo\src\main.rs",
            "canonical_uri": "vscode-remote://host/repo/src/main.rs",
        }
        self.assertEqual(project(header), header)

    def test_resolved_selectors_survive_merge_sort_and_syntax_error_cases(self):
        selector = {"kind": "lines", "start": 2, "end": 2}
        delivered = {"selector": selector, "status": "ok", "complete": True}
        binding = {
            "selector_index": 0,
            "qualified_name": "A::b",
            "kind": "fn",
            "resolved_selectors": [selector],
        }
        exact = {"results": [delivered], "selector_bindings": [binding]}
        projected = project(exact)
        self.assertNotIn("resolved_selectors", projected["selector_bindings"][0])
        # Identity is never derivable from a result and always stays.
        for field in ("selector_index", "qualified_name", "kind"):
            self.assertEqual(projected["selector_bindings"][0][field], binding[field])

        # A merged delivered range no longer shows the item's real extent.
        merged = {
            "results": [{**delivered, "selector": {"kind": "lines", "start": 1, "end": 121}}],
            "selector_bindings": [binding],
        }
        self.assertEqual(
            project(merged)["selector_bindings"][0]["resolved_selectors"], [selector]
        )
        # Undelivered and parser-unreliable bindings keep their range.
        for results, bound in [
            ([{**delivered, "status": "invalid_selector"}], binding),
            ([{**delivered, "complete": False}], binding),
            ([delivered], {**binding, "syntax_error": True}),
        ]:
            kept = project({"results": results, "selector_bindings": [bound]})
            self.assertEqual(
                kept["selector_bindings"][0]["resolved_selectors"], [selector]
            )


class AuditTest(unittest.TestCase):
    def test_audit_counts_only_savings_it_can_justify(self):
        selector = {"kind": "lines", "start": 1, "end": 1}
        header = {
            "path": r"C:\repo\a.rs",
            "canonical_uri": "file:///C:/repo/a.rs",
            "results": [{"selector": selector, "status": "ok", "complete": True}],
            "selector_bindings": [
                {"selector_index": 0, "qualified_name": "a", "resolved_selectors": [selector]}
            ],
        }
        opaque = {"path": "skill:x", "canonical_uri": "skill:x"}
        records = [
            {
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "output": json.dumps(header) + "\nbody",
                },
            },
            {
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "output": json.dumps(opaque) + "\nbody",
                },
            },
            # Rollout-only records must not be counted as model context.
            {"type": "event_msg", "payload": {"type": "token_count"}},
            {"type": "tool_manifest", "payload": {"tools": ["read_file"]}},
        ]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollout.jsonl"
            path.write_text(
                "\n".join(json.dumps(record) for record in records) + "\n",
                encoding="utf-8",
            )
            report = audit(path)
        self.assertEqual(report["enveloped_outputs"], 2)
        self.assertEqual(report["canonical_uri_omissions"], 1)
        self.assertEqual(report["resolved_selectors_omissions"], 1)
        self.assertGreater(report["header_tokens_saved"], 0)
        self.assertEqual(
            report["header_tokens_saved"],
            report["header_tokens_before"] - report["header_tokens_after"],
        )

    def test_malformed_lines_fail_instead_of_publishing_partial_totals(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollout.jsonl"
            path.write_text("{not json\n\n", encoding="utf-8")
            # A partial corpus must not be presented as a zero-output audit.
            with self.assertRaisesRegex(ValueError, "invalid rollout record"):
                audit(path)


if __name__ == "__main__":
    unittest.main()
