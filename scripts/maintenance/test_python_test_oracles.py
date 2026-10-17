import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from scripts.maintenance.python_test_oracles import MAX_INPUT_BYTES, analyze


def semantic_cases():
    """Authored counterexamples shared with the opt-in worker benchmark.

    Expected edges describe possible direct calls, never observed execution.
    Negative cases follow Python binding/control-flow rules, not analyzer output.
    """
    cases = []
    for name, body, edges in (
        ("import_alias", "from product import run as action\ndef test_case(): action()\n", ["product.run"]),
        ("parameter_shadow", "from product import run\ndef test_case(run): run()\n", []),
        ("assigned_shadow", "from product import run\ndef test_case():\n run = lambda: 2\n run()\n", []),
        ("local_import", "from product import run\ndef test_case():\n from unavailable import run\n run()\n", []),
        ("comprehension_shadow", "from product import run\ndef test_case():\n return [run() for run in callbacks]\n", []),
        ("unbound_module", "def test_case(): product.run()\n", []),
        ("rebound_import", "from product import run\nrun = lambda: 2\ndef test_case(): run()\n", []),
        ("decorator_not_body", "from product import run\n@run()\ndef test_case(): pass\n", []),
    ):
        cases.append({"name": name, "sources": {"product.py": "def run(): return 1\n", "test_case.py": body},
                      "test": "test_case.test_case", "edges": edges})
    cases.append({"name": "package_relative_import", "sources": {
        "pkg/__init__.py": "from .impl import run\ndef test_case(): run()\n",
        "pkg/impl.py": "def run(): return 1\n"}, "test": "pkg.test_case", "edges": ["pkg.impl.run"]})
    for name, body in (
        ("match_control", ' match flag:\n  case True: return {"value": 1}\n  case _: return {"value": 2}\n'),
        ("assert_precondition", ' assert flag\n return {"value": 1}\n'),
    ):
        cases.append({"name": name, "sources": {
            "product.py": "def choose(flag):\n" + body,
            "test_case.py": 'from product import choose\ndef test_case(self): self.assertEqual(choose(True)["value"], choose(False)["value"])\n'},
            "forbidden": "compared_value_independent_of_parameter"})
    cases.append({"name": "preceding_failure", "sources": {
        "product.py": "import subprocess\ndef run():\n guard()\n return subprocess.run(['tool'])\n",
        "test_case.py": "from product import run\nfrom unittest.mock import patch\n@patch('product.subprocess.run')\ndef test_fake(mocked): run()\ndef test_real(): run()\n"},
        "required": "external_boundary_only_mocked"})
    return cases


def semantic_case_checks(case, report):
    checks = {"incomplete": report["analysis_complete"] is False,
              "advisory": report["advisory_only"] is True}
    if "edges" in case:
        checks["edges"] = report["tests"][case["test"]]["reached"] == case["edges"]
    reasons = {row["reason"] for row in report["diagnostics"]}
    if "forbidden" in case:
        checks["counterexample"] = case["forbidden"] not in reasons
    if "required" in case:
        checks["failure_path"] = case["required"] in reasons
    return checks


class SemanticEvidenceTests(unittest.TestCase):
    def test_above_root_relative_import_is_not_a_sibling_relationship(self):
        from importlib.util import resolve_name

        with self.assertRaises(ImportError):
            resolve_name("..product", "pkg")
        report = analyze({
            "pkg/__init__.py": "from ..product import run\ndef test_case(): run()\n",
            "product.py": "def run(): return 7\n",
        })
        self.assertEqual(report["tests"]["pkg.test_case"]["reached"], [])
        self.assertTrue(any(row["reason"] == "unresolved_call" for row in report["unknown"]))

    def test_definition_time_expressions_are_not_test_body_calls(self):
        report = analyze({
            "product.py": "def run(): return 7\n",
            "test_case.py": "from product import run\ndef test_case(value: run() = run()) -> run(): pass\n",
        })
        self.assertEqual(report["tests"]["test_case.test_case"]["reached"], [])

    def test_compiler_bindings_and_control_flow_counterexamples(self):
        for case in semantic_cases():
            with self.subTest(case=case["name"]):
                checks = semantic_case_checks(case, analyze(case["sources"]))
                self.assertTrue(all(checks.values()), checks)

    def test_authored_behavior_counterexamples_are_real(self):
        # Execute only these two fixed, side-effect-free fixtures, not arbitrary
        # analyzed input. This independently checks the negative oracle.
        for case in semantic_cases():
            if case["name"] not in {"match_control", "assert_precondition"}:
                continue
            namespace = {}
            exec(compile(case["sources"]["product.py"], "fixture", "exec"), namespace)
            choose = namespace["choose"]
            self.assertEqual(choose(True), {"value": 1})
            if case["name"] == "match_control":
                self.assertEqual(choose(False), {"value": 2})
            else:
                with self.assertRaises(AssertionError):
                    choose(False)


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
    def test_patched_call_edges_do_not_establish_real_boundary_coverage(self):
        from scripts.benchmark_scripts.benchmark_semantic_contracts import checks, patched_boundary_cases

        for name, sources in patched_boundary_cases().items():
            with self.subTest(case=name):
                # Exercise the public isolated worker, not a reconstructed
                # reachability helper. Expectations are authored in the fixture.
                result = self.worker(json.dumps({"sources": sources}).encode())
                self.assertEqual(result.returncode, 0, result.stderr)
                outcomes = checks(name, json.loads(result.stdout))
                self.assertTrue(all(outcomes.values()), outcomes)

    def test_patched_call_fixture_runtime_paths(self):
        import types
        from unittest.mock import patch
        from scripts.benchmark_scripts.benchmark_semantic_contracts import patched_boundary_cases

        for name, sources in patched_boundary_cases().items():
            with self.subTest(case=name):
                product = types.ModuleType("product")
                tests = types.ModuleType("test_case")
                with patch.dict(sys.modules, {"product": product, "test_case": tests}):
                    # Only these fixed authored fixtures are executed. The
                    # analyzer itself must never import/execute supplied source.
                    exec(compile(sources["product.py"], "product.py", "exec"), product.__dict__)
                    exec(compile(sources["test_case.py"], "test_case.py", "exec"), tests.__dict__)
                    with patch("subprocess.run") as boundary:
                        tests.test_fake()
                        tests.test_candidate()
                        if hasattr(tests, "test_real"):
                            tests.test_real()
                        expected = int(name in {"unaffected_import_binding", "patched_entrypoint_with_real"})
                        self.assertEqual(boundary.call_count, expected)
                        if expected:
                            boundary.assert_called_once_with(["tool"])

    def test_patched_boundary_receivers_and_object_targets_stay_unknown(self):
        from scripts.benchmark_scripts.benchmark_semantic_contracts import patched_boundary_cases

        sources = patched_boundary_cases()["patched_entrypoint"]
        prefix = sources["test_case.py"].split('@patch("test_case.run")')[0]
        variants = {
            "boundary_receiver": '@patch("product.subprocess")\ndef test_candidate(mocked):\n run()\n mocked.run.assert_called_once()\n',
            "module_receiver": 'import product as api\n@patch("test_case.api")\ndef test_candidate(mocked): api.run()\n',
            "object_target": 'import product as api\n@patch.object(api, "run")\ndef test_candidate(mocked): api.run()\n',
            "opaque_object": 'import product as api\n@patch.object(lookup(), "run")\ndef test_candidate(mocked): api.run()\n',
        }
        for name, body in variants.items():
            with self.subTest(case=name):
                result = self.worker(json.dumps({"sources": {
                    **sources, "test_case.py": prefix + body,
                }}).encode())
                self.assertEqual(result.returncode, 0, result.stderr)
                report = json.loads(result.stdout)
                self.assertIn("external_boundary_only_mocked",
                              {row["reason"] for row in report["diagnostics"]})
                self.assertTrue(any(
                    row["reason"] == "boundary_reachability_unknown"
                    and row.get("test_callable") == "test_case.test_candidate"
                    for row in report["unknown"]
                ))

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

    def test_unexecuted_control_flow_does_not_discharge_mocked_boundary(self):
        bodies = (
            "    if not enabled: return None\n    return subprocess.run(['tool'])\n",
            "    for _ in range(int(enabled)):\n        subprocess.run(['tool'])\n",
            "    while enabled:\n        return subprocess.run(['tool'])\n",
            "    return subprocess.run(['tool']) if enabled else None\n",
            "    return enabled and subprocess.run(['tool'])\n",
            "    return [subprocess.run(['tool']) for _ in range(int(enabled))]\n",
        )
        for body in bodies:
            with self.subTest(body=body):
                # Every disabled call above avoids subprocess.run by Python's
                # control-flow rules. A direct call alone is not real coverage.
                report = analyze({
                    "product.py": "import subprocess\ndef run(enabled):\n" + body,
                    "test_product.py": """
import unittest
from unittest.mock import patch
from product import run
class Cases(unittest.TestCase):
    @patch("product.subprocess.run")
    def test_fake(self, mocked):
        run(True)
        mocked.assert_called_once()
    def test_disabled(self): self.assertFalse(run(False))
""",
                })
                self.assertTrue(report["parse_complete"])
                findings = [row for row in report["diagnostics"]
                            if row["reason"] == "external_boundary_only_mocked"]
                self.assertEqual(len(findings), 1)
                self.assertIn("test_product.Cases.test_disabled",
                              findings[0]["unknown_reaching_tests"])

    def test_unexecuted_test_call_is_not_real_boundary_coverage(self):
        for body in (
            "        if False: run()\n",
            "        for _ in (): run()\n",
            "        return\n        run()\n",
            "        False and run()\n",
        ):
            with self.subTest(body=body):
                report = analyze({
                    "product.py": "import subprocess\ndef run(): return subprocess.run(['tool'])\n",
                    "test_product.py": """
import unittest
from unittest.mock import patch
from product import run
class Cases(unittest.TestCase):
    @patch("product.subprocess.run")
    def test_fake(self, mocked):
        run()
        mocked.assert_called_once()
    def test_disabled(self):
""" + body,
                })
                self.assertTrue(report["parse_complete"])
                self.assertIn("external_boundary_only_mocked",
                              {row["reason"] for row in report["diagnostics"]})

    def test_deferred_function_bodies_are_not_direct_boundary_coverage(self):
        for producer, execute in (
            ("def run(): yield subprocess.run(['tool'])", "next(run())"),
            ("async def run(): return subprocess.run(['tool'])", "asyncio.run(run())"),
        ):
            with self.subTest(producer=producer):
                report = analyze({
                    "product.py": "import subprocess\n" + producer,
                    "test_product.py": f"""
import asyncio, unittest
from unittest.mock import patch
from product import run
class Cases(unittest.TestCase):
    @patch("product.subprocess.run")
    def test_fake(self, mocked):
        {execute}
        mocked.assert_called_once()
    def test_create_only(self):
        deferred = run()
        deferred.close()
""",
                })
                self.assertTrue(report["parse_complete"])
                self.assertIn("external_boundary_only_mocked",
                              {row["reason"] for row in report["diagnostics"]})

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


class ModuleIdentityTests(unittest.TestCase):
    def test_module_package_collision_uses_import_oracle_and_never_merges_owners(self):
        # Only this fixed fixture is executed. Python independently selects the
        # package, whose namespace cannot acquire definitions from pkg.py.
        sources = {
            "pkg.py": "def run(): return {'status': 'shadowed'}\n",
            "pkg/__init__.py": "value = 2\n",
            "test_case.py": "from pkg import run\ndef test_case(): run()\n",
        }
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for path, source in sources.items():
                target = root / path
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(source, encoding="utf-8")
            observed = subprocess.run(
                [sys.executable, "-B", "-c",
                 "import importlib.util, json; s = importlib.util.find_spec('pkg'); "
                 "print(json.dumps({'package': s.submodule_search_locations is not None})); "
                 "from pkg import run"],
                cwd=root, capture_output=True, text=True, timeout=5,
            )
        self.assertEqual(json.loads(observed.stdout), {"package": True})
        self.assertNotEqual(observed.returncode, 0)
        self.assertIn("ImportError", observed.stderr)
        worker = subprocess.run(
            [sys.executable, "-I", "-B", str(Path(__file__).with_name("python_test_oracles.py"))],
            input=json.dumps({"sources": sources}).encode(), capture_output=True, timeout=10,
        )
        self.assertEqual(worker.returncode, 0, worker.stderr)
        report = json.loads(worker.stdout)
        self.assertEqual(report["tests"]["test_case.test_case"]["reached"], [])
        collisions = [row for row in report["unknown"] if row["reason"] == "ambiguous_module_identity"]
        self.assertEqual({row["path"] for row in collisions}, {"pkg.py", "pkg/__init__.py"})
        self.assertFalse(report["analysis_complete"])
        self.assertEqual(report["diagnostics"], [])

    def test_colliding_path_spellings_are_unknown_and_unique_modules_still_resolve(self):
        for paths in [("pkg/item.py", "pkg\\item.py"), ("pkg/item.py", "pkg.item.py")]:
            with self.subTest(paths=paths):
                report = analyze({
                    paths[0]: "def run(): return 1\n",
                    paths[1]: "def other(): return 2\n",
                    "safe.py": "def run(): return 3\n",
                    "test_case.py": "from pkg.item import run\nimport safe\ndef test_case(): run(); safe.run()\n",
                })
                self.assertEqual(report["tests"]["test_case.test_case"]["reached"], ["safe.run"])
                self.assertEqual({row["path"] for row in report["unknown"]
                                  if row["reason"] == "ambiguous_module_identity"}, set(paths))


class SemanticContractCounterexamples(unittest.TestCase):
    def test_benchmark_changes_select_the_authored_accuracy_oracles(self):
        from scripts.maintenance.root_maintenance import test_modules_for_changed_path

        # Authored fixtures remain; the retired worker benchmark has no route.
        self.assertIn("scripts.maintenance.test_python_test_oracles.SemanticContractCounterexamples",
                      test_modules_for_changed_path(
                          "scripts/benchmark_scripts/benchmark_semantic_contracts.py"))
        self.assertEqual(test_modules_for_changed_path(
            "scripts/benchmark_scripts/benchmark_semantic_evidence.py"), ())

    def test_semantic_fixtures_through_real_analyzer(self):
        from scripts.benchmark_scripts.benchmark_semantic_contracts import cases, checks

        for name, sources in cases().items():
            with self.subTest(case=name):
                result = checks(name, analyze(sources))
                self.assertTrue(all(result.values()), result)


if __name__ == "__main__":
    unittest.main()
