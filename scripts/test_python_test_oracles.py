import json
from pathlib import Path
import subprocess
import sys
import unittest

from scripts.python_test_oracles import MAX_INPUT_BYTES, analyze


class PythonOracleTests(unittest.TestCase):
    def reasons(self, production, tests):
        report = analyze({"product.py": production, "test_product.py": tests})
        self.assertTrue(report["parse_complete"], report)
        self.assertFalse(report["analysis_complete"], report)
        return {
            (d["test_callable"].rsplit(".", 1)[-1], d["reason"])
            for d in report["diagnostics"]
        }

    def test_constant_fields_absence_and_parameter_flow_have_negative_controls(self):
        reasons = self.reasons(
            """
def report(copies):
    return {"executed": False, "count": copies, "fixed": 7}
def dynamic(copies):
    data = {}
    data[str(copies)] = copies
    return data
""",
            """
import unittest
from product import report, dynamic
class Cases(unittest.TestCase):
    def test_literal(self):
        actual = report(2)
        self.assertEqual(actual["executed"], False)
    def test_behavior(self):
        self.assertEqual(report(2)["count"], 2)
    def test_absent(self):
        self.assertNotIn("totalNs", report(2))
    def test_possible(self):
        self.assertNotIn("executed", report(2))
    def test_dynamic(self):
        self.assertNotIn("totalNs", dynamic(2))
    def test_independent(self):
        self.assertEqual(report(2)["fixed"], report(4)["fixed"])
    def test_dependent(self):
        self.assertNotEqual(report(2)["count"], report(4)["count"])
""",
        )
        self.assertEqual(
            reasons,
            {
                ("test_literal", "literal_output_self_description"),
                ("test_absent", "impossible_absence_oracle"),
                ("test_independent", "compared_value_independent_of_parameter"),
            },
        )

    def test_reflected_defaults_and_imported_constants(self):
        reasons = self.reasons(
            "DEFAULT = 0.0\ndef run(wait=0.0): return wait\n",
            """
import unittest
import inspect
from product import run, DEFAULT
class Cases(unittest.TestCase):
    def test_signature(self):
        self.assertEqual(inspect.signature(run).parameters["wait"].default, 0.0)
    def test_constant(self): self.assertEqual(DEFAULT, 0.0)
    def test_behavior(self): self.assertEqual(run(2), 2)
""",
        )
        self.assertEqual(
            reasons,
            {
                ("test_signature", "constant_only_oracle"),
                ("test_constant", "constant_only_oracle"),
            },
        )

    def test_patched_only_boundary_is_cleared_by_real_reaching_test(self):
        production = """
import subprocess
def run(): return subprocess.run(["tool", "--final"], check=True)
"""
        tests = """
import unittest
from unittest.mock import patch
from product import run
class Cases(unittest.TestCase):
    @patch("product.subprocess.run")
    def test_fake(self, mocked):
        run()
        mocked.assert_called_once_with(["tool", "--final"], check=True)
"""
        self.assertEqual(
            self.reasons(production, tests),
            {
                ("test_fake", "external_boundary_only_mocked"),
                ("test_fake", "mock_call_site_only_oracle"),
            },
        )
        real = (
            tests
            + """
    def test_real(self): self.assertEqual(run().returncode, 0)
"""
        )
        self.assertEqual(
            self.reasons(production, real),
            {("test_fake", "mock_call_site_only_oracle")},
        )

    def test_unpatched_test_on_another_branch_does_not_cover_external_options(self):
        reasons = self.reasons(
            """
import subprocess
def run(enabled):
    if enabled:
        return subprocess.run(["tool", "--success-output", "final"], check=True)
    return None
""",
            """
import unittest
from unittest.mock import patch
from product import run
class Cases(unittest.TestCase):
    @patch("product.subprocess.run")
    def test_fake(self, mocked):
        run(True)
        mocked.assert_called_once_with(["tool", "--success-output", "final"], check=True)
    def test_disabled(self): self.assertIsNone(run(False))
""",
        )
        self.assertIn(("test_fake", "external_boundary_only_mocked"), reasons)

    def test_unstarted_patch_factory_does_not_replace_the_boundary(self):
        reasons = self.reasons(
            "import subprocess\ndef run(): return subprocess.run(['tool'])\n",
            """
import unittest
from unittest.mock import patch
from product import run
class Cases(unittest.TestCase):
    def test_real(self):
        unused = patch("product.subprocess.run")
        self.assertEqual(run().returncode, 0)
""",
        )
        self.assertNotIn(
            "external_boundary_only_mocked", {reason for _, reason in reasons}
        )

    def test_unused_scripted_response_cannot_share_the_expected_answer(self):
        production = (
            "import subprocess\ndef run(): return subprocess.check_output(['tool'])\n"
        )
        tests = """
import unittest
from unittest.mock import patch
from product import run
class Cases(unittest.TestCase):
    @patch("product.subprocess.check_output")
    def test_fallback(self, mocked):
        mocked.side_effect = [b"answer", b"answer"]
        self.assertEqual(run(), b"answer")
        mocked.assert_called_once()
    @patch("product.subprocess.check_output")
    def test_distinct(self, mocked):
        mocked.side_effect = [b"answer", b"wrong path"]
        self.assertEqual(run(), b"answer")
        mocked.assert_called_once()
"""
        reasons = self.reasons(production, tests)
        self.assertIn(("test_fallback", "scripted_fallback_matches_expected"), reasons)
        self.assertNotIn(
            ("test_distinct", "scripted_fallback_matches_expected"), reasons
        )

    def test_worker_never_imports_or_executes_analyzed_modules(self):
        worker = Path(__file__).with_name("python_test_oracles.py")
        result = subprocess.run(
            [sys.executable, "-I", "-B", str(worker)],
            input=json.dumps(
                {"sources": {"product.py": "raise RuntimeError('must not run')\n"}}
            ),
            text=True,
            capture_output=True,
            timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["diagnostics"], [])

    def test_syntax_failure_is_not_a_clean_report(self):
        report = analyze({"broken.py": "def missing("})
        self.assertFalse(report["analysis_complete"])
        self.assertFalse(report["parse_complete"])
        self.assertEqual(report["unknown"][0]["reason"], "parse_error")

    def test_parameter_flow_follows_aliases_and_treats_calls_as_unknown(self):
        reasons = self.reasons(
            """
def alias(copies):
    n = copies + 1
    m = n * 2
    return {"value": m}
def conditional(copies):
    enabled = copies > 1
    if enabled: return {"value": 1}
    return {"value": 2}
def unknown(copies):
    return {"value": external()}
""",
            """
from product import alias, conditional, unknown
import unittest
class Cases(unittest.TestCase):
    def test_alias(self): self.assertEqual(alias(1)["value"], alias(2)["value"])
    def test_control(self): self.assertEqual(conditional(1)["value"], conditional(2)["value"])
    def test_unknown(self): self.assertEqual(unknown(1)["value"], unknown(2)["value"])
""",
        )
        self.assertEqual(reasons, set())

    def test_parameter_independence_does_not_ignore_mutation_or_loop_control(self):
        reasons = self.reasons(
            """
state = 0
def mutating(copies, values):
    values.append(copies)
    return {"value": values}
def global_value(copies):
    update(copies)
    return {"value": state}
def loop(copies):
    for i in range(copies):
        return {"value": 7}
    return {"value": 8}
""",
            """
from product import mutating, global_value, loop
import unittest
class Cases(unittest.TestCase):
    def test_mutation(self): self.assertEqual(mutating(1, [])["value"], mutating(2, [])["value"])
    def test_global(self): self.assertEqual(global_value(1)["value"], global_value(2)["value"])
    def test_loop(self): self.assertEqual(loop(0)["value"], loop(2)["value"])
""",
        )
        self.assertNotIn(
            "compared_value_independent_of_parameter", {reason for _, reason in reasons}
        )

    def test_missing_and_disconnected_oracles_are_not_behavior_coverage(self):
        reasons = self.reasons(
            "def run(value): return value + 1\n",
            """
import unittest
from product import run
class Cases(unittest.TestCase):
    def test_missing(self): run(3)
    def test_disconnected(self):
        run(3)
        expected = [1]
        self.assertEqual(len(expected), 1)
    def test_connected(self): self.assertEqual(run(3), 4)
""",
        )
        self.assertIn(("test_missing", "no_effective_oracle"), reasons)
        self.assertIn(("test_disconnected", "oracle_without_production_flow"), reasons)
        self.assertFalse(any(test == "test_connected" for test, _ in reasons))

    def test_argparse_defaults_are_constant_checks_but_explicit_arguments_are_not(self):
        reasons = self.reasons(
            """
import argparse
def parser():
    p = argparse.ArgumentParser()
    p.add_argument("--wait", type=float, default=0.0)
    return p
""",
            """
import unittest
from product import parser
class Cases(unittest.TestCase):
    def test_default(self): self.assertEqual(parser().get_default("wait"), 0.0)
    def test_empty(self): self.assertEqual(parser().parse_args([]).wait, 0.0)
    def test_explicit(self): self.assertEqual(parser().parse_args(["--wait", "2"]).wait, 2.0)
""",
        )
        self.assertEqual(
            {test for test, reason in reasons if reason == "constant_only_oracle"},
            {"test_default", "test_empty"},
        )

    def test_text_clock_and_copied_expression_have_independent_controls(self):
        reasons = self.reasons(
            """
def select(codes): return next((code for code in codes if code), 0)
""",
            """
import unittest
import time
from pathlib import Path
from unittest.mock import patch
from product import select
class Cases(unittest.TestCase):
    def test_copy(self):
        codes = [0, 2]
        self.assertEqual(select(codes), next((code for code in codes if code), 0))
    def test_literal(self): self.assertEqual(select([0, 2]), 2)
    def test_text(self):
        expected = Path("src/lib.rs").read_text()
        self.assertIn("fn select", expected)
    def test_clock(self):
        start = time.perf_counter()
        select([0, 2])
        self.assertLess(time.perf_counter() - start, .025)
    @patch("time.perf_counter")
    def test_mocked(self, clock):
        self.assertLess(time.perf_counter(), .025)
    def test_generous(self): self.assertLess(time.perf_counter(), 25)
""",
        )
        self.assertEqual(
            reasons,
            {
                ("test_copy", "expectation_derived_from_production"),
                ("test_text", "repository_text_change_detector"),
                ("test_clock", "wall_clock_subsecond_oracle"),
            },
        )

    def test_expected_branch_expression_is_checked_against_production_condition(self):
        reasons = self.reasons(
            """
ERROR = 12029
def retry(allow_auto_logon, first_error):
    if allow_auto_logon and first_error == ERROR:
        return True
    return False
""",
            """
import unittest
from product import retry, ERROR
class Cases(unittest.TestCase):
    def test_copied_condition(self):
        allow_auto_logon = True
        first_error = ERROR
        expected = allow_auto_logon and first_error == ERROR
        self.assertEqual(retry(allow_auto_logon, first_error), expected)
    def test_independent_condition(self):
        allow_auto_logon = True
        first_error = ERROR
        expected = first_error == ERROR
        self.assertEqual(retry(allow_auto_logon, first_error), expected)
""",
        )
        self.assertIn(
            ("test_copied_condition", "expectation_derived_from_production"), reasons
        )
        self.assertNotIn(
            ("test_independent_condition", "expectation_derived_from_production"),
            reasons,
        )


class AdvisoryContractTests(unittest.TestCase):
    def worker(self, payload):
        return subprocess.run(
            [
                sys.executable,
                "-I",
                "-B",
                str(Path(__file__).with_name("python_test_oracles.py")),
            ],
            input=payload,
            capture_output=True,
            timeout=10,
        )

    def test_unknown_wrapper_does_not_discharge_mocked_boundary(self):
        sources = {
            "product.py": """
import subprocess
def run(enabled):
    if enabled: return subprocess.run(["tool"])
    return None
def wrapper(enabled): return run(enabled)
""",
            "test_product.py": """
import unittest
from unittest.mock import patch
from product import run, wrapper
class Cases(unittest.TestCase):
    @patch("product.subprocess.run")
    def test_fake(self, mocked): self.assertEqual(run(True).returncode, 0)
    def test_disabled(self): self.assertIsNone(wrapper(False))
""",
        }
        report = analyze(sources)
        finding = next(
            d
            for d in report["diagnostics"]
            if d["reason"] == "external_boundary_only_mocked"
        )
        self.assertEqual(
            finding["unknown_reaching_tests"], ["test_product.Cases.test_disabled"]
        )
        unknown = [
            u
            for u in report["unknown"]
            if u["reason"] == "boundary_reachability_unknown"
        ]
        self.assertEqual(
            [(u["test_callable"], u["callable"]) for u in unknown],
            [("test_product.Cases.test_disabled", "product.run")],
        )
        self.assertTrue(report["parse_complete"])
        self.assertFalse(report["analysis_complete"])
        # A direct, enabled, unpatched path discharges only the mocked-boundary
        # advisory. The wrapper's unresolved path must still remain visible.
        sources["test_product.py"] += (
            "    def test_real(self): self.assertEqual(run(True).returncode, 0)\n"
        )
        real = analyze(sources)
        self.assertNotIn(
            "external_boundary_only_mocked", {d["reason"] for d in real["diagnostics"]}
        )
        self.assertIn(
            "boundary_reachability_unknown", {u["reason"] for u in real["unknown"]}
        )

    def test_dynamic_loader_and_dispatch_are_explicit_unknowns(self):
        report = analyze(
            {
                "test_dynamic.py": """
import importlib.util
spec = importlib.util.spec_from_file_location("target", "unavailable.py")
spec.loader.exec_module(module)
def test_dispatch(subject):
    subject.run()
    getattr(subject, "run")()
"""
            }
        )
        self.assertTrue(report["parse_complete"])
        self.assertFalse(report["analysis_complete"])
        unknown = {(u["reason"], u.get("target")) for u in report["unknown"]}
        self.assertIn(
            ("dynamic_import", "importlib.util.spec_from_file_location"), unknown
        )
        self.assertIn(("dynamic_import", "spec.loader.exec_module"), unknown)
        self.assertIn(("unresolved_call", "subject.run"), unknown)
        self.assertIn(("unresolved_call", "<dynamic expression>"), unknown)

    def test_clean_syntax_never_claims_behavioral_completeness(self):
        for sources in ({}, {"plain.py": "VALUE = 7\n"}):
            with self.subTest(sources=sources):
                report = analyze(sources)
                self.assertTrue(report["parse_complete"])
                self.assertFalse(report["analysis_complete"])
                self.assertTrue(report["advisory_only"])
                self.assertEqual(report["diagnostics"], [])
                self.assertTrue(report["limitations"])

    def test_cli_reports_advisories_and_parse_errors_without_running_sources(self):
        for source, parse_complete in (
            (
                "raise RuntimeError('never execute')\nVALUE = 7\ndef test_value(): assert VALUE == 7\n",
                True,
            ),
            ("def broken(", False),
        ):
            with self.subTest(source=source):
                result = self.worker(
                    json.dumps({"sources": {"source.py": source}}).encode()
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                report = json.loads(result.stdout)
                self.assertEqual(report["parse_complete"], parse_complete)
                self.assertFalse(report["analysis_complete"])
                self.assertEqual(result.stderr, b"")
        warning = self.worker(
            json.dumps(
                {
                    "sources": {
                        "test_case.py": "import unittest\nVALUE=7\nclass Case(unittest.TestCase):\n def test_value(self): self.assertEqual(VALUE, 7)\n"
                    }
                }
            ).encode()
        )
        self.assertEqual(warning.returncode, 0, warning.stderr)
        self.assertEqual(
            [d["severity"] for d in json.loads(warning.stdout)["diagnostics"]], ["warn"]
        )

    def test_cli_rejects_malformed_or_oversized_input_without_json_success(self):
        for payload in (
            b"{",
            b"[]",
            b"{}",
            b'{"sources":[]}',
            b'{"sources":{"x.py":1}}',
            b" " * (MAX_INPUT_BYTES + 1),
        ):
            with self.subTest(size=len(payload), prefix=payload[:40]):
                result = self.worker(payload)
                self.assertEqual(result.returncode, 2)
                self.assertEqual(result.stdout, b"")
                self.assertIn(b"Python oracle input/analysis error", result.stderr)

    def test_public_api_bounds_file_count_and_utf8_bytes(self):
        invalid = [
            [],
            {1: "text"},
            {"x.py": 1},
            {str(i): "" for i in range(10001)},
            {"x.py": "\u00e9" * (MAX_INPUT_BYTES // 2)},
        ]
        for sources in invalid:
            with self.subTest(kind=type(sources).__name__, entries=len(sources)):
                with self.assertRaises(ValueError):
                    analyze(sources)


if __name__ == "__main__":
    unittest.main()
