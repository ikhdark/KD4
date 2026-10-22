#!/usr/bin/env python3
"""Run real unittest selections/discovery; stdout is one JSON document.

Usage: python -B scripts/maintenance/unittest_json.py [-v] module.Class.test_name
       python -B scripts/maintenance/unittest_json.py discover -s scripts -t . -p 'test_*.py'
       python -B scripts/maintenance/unittest_json.py --execution-only module.Class.test_name

The leading --execution-only option omits unused source-body observations, not
execution records. Both modes emit JSON for parent validation.

Diagnostics (including inherited native stdout) go to stderr. This process-only
entrypoint permanently redirects stdout, including atexit output. A killed
process may emit no report; consumers must also check exit status and coverage.
Content hashes observe authored test bodies, not executed bytecode, dependencies,
freshness, or reusable validation receipts.
"""

from __future__ import annotations

from collections import Counter
import hashlib
import inspect
import json
import linecache
import os
from pathlib import Path
import re
import sys
import traceback
import unittest


CONTENT_SCOPE = "authored_test_body_observation_only"
OUTCOMES = {
    "success",
    "failure",
    "error",
    "skipped",
    "expected_failure",
    "unexpected_success",
    "incomplete",
}
BAD_OUTCOMES = {"failure", "error", "unexpected_success", "incomplete"}


def content_identity(test):
    """Read a fresh source observation; deliberately do not hash dependencies."""
    method = getattr(test, getattr(test, "_testMethodName", ""), None)
    try:
        method = inspect.unwrap(method)
        path = inspect.getsourcefile(method)
        if path is None:
            return None
        # inspect's mtime/size cache can miss same-size, restored-mtime edits.
        linecache.cache.pop(path, None)
        source = inspect.getsource(method)
        return {
            "path": str(Path(path).resolve()),
            "sha256": hashlib.sha256(source.encode("utf-8")).hexdigest(),
        }
    except (OSError, TypeError, IndexError, ValueError):
        return None


class JsonResult(unittest.TextTestResult):
    def __init__(self, *args, record_content=True, **kwargs):
        super().__init__(*args, **kwargs)
        self.record_content = record_content
        self.records = []
        self.active = {}
        self.body_methods = {}

    def startTest(self, test):
        super().startTest(test)
        record = {
            "name": test.id(),
            "started": True,
            "body_started": False,
            "outcome": "incomplete",
            "failure_type": None,
            "content_before": content_identity(test) if self.record_content else None,
            "content_after": None,
            "events": [],
        }
        self.active[id(test)] = record
        self.records.append(record)
        # Instrument only this instance's supported lifecycle seam. In
        # IsolatedAsyncioTestCase it is reached after asyncSetUp as well.
        original = test._callTestMethod
        previous = vars(test).get("_callTestMethod")
        self.body_methods[id(test)] = (previous, "_callTestMethod" in vars(test))

        def call_body(method):
            record["body_started"] = True
            return original(method)

        test._callTestMethod = call_body

    def event(self, test, outcome, err=None, reason=None, *, parent=None):
        failure_type = None
        if err is not None:
            failure_type = (
                "assertion"
                if outcome == "failure"
                or (
                    outcome == "expected_failure"
                    and issubclass(err[0], test.failureException)
                )
                else "exception"
            )
        event = {
            "name": test.id(),
            "outcome": outcome,
            "failure_type": failure_type,
            "traceback": self._exc_info_to_string(err, test) if err else None,
            "reason": reason,
            "skip_origin": None,
        }
        owner = parent or getattr(test, "test_case", test)
        record = self.active.get(id(owner))
        if record is None:
            # unittest fixture errors/skips do not call startTest/stopTest.
            record = {
                "name": test.id(),
                "started": False,
                "body_started": False,
                "outcome": outcome,
                "failure_type": failure_type,
                "content_before": None,
                "content_after": None,
                "events": [],
            }
            self.records.append(record)
        if outcome == "skipped":
            method = getattr(owner, getattr(owner, "_testMethodName", ""), None)
            policy = getattr(type(owner), "__unittest_skip__", False) or getattr(
                method, "__unittest_skip__", False
            )
            standard_run = getattr(type(owner), "run", None) in (
                unittest.TestCase.run,
                unittest.IsolatedAsyncioTestCase.run,
            )
            event["skip_origin"] = (
                "policy"
                if policy and not record["body_started"]
                else "after_body_entry"
                if record["body_started"]
                else "setup"
                if not record["started"] or standard_run
                else "unknown"
            )
        record["events"].append(event)
        priority = {
            "incomplete": -1,
            "success": 0,
            "skipped": 1,
            "expected_failure": 1,
            "unexpected_success": 2,
            "failure": 3,
            "error": 4,
        }
        if priority[outcome] >= priority[record["outcome"]]:
            record["outcome"] = outcome
            record["failure_type"] = failure_type

    def addSuccess(self, test):
        super().addSuccess(test)
        self.event(test, "success")

    def addFailure(self, test, err):
        super().addFailure(test, err)
        self.event(test, "failure", err)

    def addError(self, test, err):
        super().addError(test, err)
        self.event(test, "error", err)

    def addSkip(self, test, reason):
        super().addSkip(test, reason)
        self.event(test, "skipped", reason=reason)

    def addExpectedFailure(self, test, err):
        super().addExpectedFailure(test, err)
        self.event(test, "expected_failure", err)

    def addUnexpectedSuccess(self, test):
        super().addUnexpectedSuccess(test)
        self.event(test, "unexpected_success")

    def addSubTest(self, test, subtest, err):
        super().addSubTest(test, subtest, err)
        outcome = (
            "success"
            if err is None
            else ("failure" if issubclass(err[0], test.failureException) else "error")
        )
        self.event(subtest, outcome, err, parent=test)

    def stopTest(self, test):
        record = self.active.pop(id(test))
        previous, existed = self.body_methods.pop(id(test))
        if existed:
            test._callTestMethod = previous
        else:
            del test._callTestMethod
        record["content_after"] = (
            content_identity(test) if self.record_content else None
        )
        super().stopTest(test)


def test_names(suite):
    for test in suite:
        if isinstance(test, unittest.TestSuite):
            yield from test_names(test)
        else:
            yield test.id()


def problem(kind, message, detail=None):
    return {"kind": kind, "message": message, "traceback": detail}


class JsonProgram(unittest.TestProgram):
    """Reuse unittest's parser and TestLoader, inspecting its actual selection."""

    def __init__(self, argv, *, record_content=True, coalesce_selectors=False):
        self.names = []
        self.problems = []
        self.json_result = None
        self.record_content = record_content
        program = self

        class SelectionLoader(unittest.TestLoader):
            def loadTestsFromNames(self, names, module=None):
                suites = []
                seen = {}

                def leaves(suite):
                    for test in suite:
                        if isinstance(test, unittest.TestSuite):
                            yield from leaves(test)
                        else:
                            yield test

                for name in names:
                    suite = self.loadTestsFromName(name, module)
                    # A deliberate -k filter may exclude whole modules. Without
                    # one, each explicit selection must contribute a test.
                    if not self.testNamePatterns and suite.countTestCases() == 0:
                        program.problems.append(
                            problem(
                                "selection",
                                f"No tests selected by explicit selector: {name}",
                            )
                        )
                    if not coalesce_selectors:
                        suites.append(suite)
                        continue
                    tests = list(leaves(suite))
                    counts = Counter(test.id() for test in tests)
                    duplicates = sorted(
                        key for key, count in counts.items() if count > 1
                    )
                    if duplicates:
                        program.problems.append(
                            problem(
                                "selection",
                                f"Duplicate test identities within {name}: {duplicates}",
                            )
                        )
                    contribution = []
                    for test in tests:
                        identity = (
                            type(test),
                            getattr(
                                type(test), getattr(test, "_testMethodName", ""), None
                            ),
                            vars(test).copy(),
                        )
                        if test.id() in seen:
                            if seen[test.id()] != identity:
                                program.problems.append(
                                    problem(
                                        "selection",
                                        f"Conflicting test identity: {test.id()}",
                                    )
                                )
                        else:
                            seen[test.id()] = identity
                            contribution.append(test)
                    admitted = {id(test) for test in contribution}

                    def retain(node):
                        if not isinstance(node, unittest.TestSuite):
                            return node if id(node) in admitted else None
                        if all(id(test) in admitted for test in leaves(node)):
                            return node  # Preserve custom suite execution semantics.
                        if type(node) is not unittest.TestSuite:
                            program.problems.append(
                                problem(
                                    "selection",
                                    "Cannot coalesce overlapping custom suites; request disjoint selectors",
                                )
                            )
                            return self.suiteClass()
                        return self.suiteClass(
                            child
                            for test in node
                            if (child := retain(test)) is not None
                        )

                    suites.append(retain(suite))
                return self.suiteClass(suites)

        super().__init__(
            module=None,
            argv=[sys.argv[0], *argv],
            testLoader=SelectionLoader(),
            exit=False,
        )

    def runTests(self):
        self.names = list(test_names(self.test))
        if self.testLoader.errors:
            self.problems.extend(
                problem("selection", "unittest could not load a test", detail)
                for detail in self.testLoader.errors
            )
        duplicates = sorted(
            name for name, count in Counter(self.names).items() if count > 1
        )
        if duplicates:
            self.problems.append(
                problem("selection", f"Duplicate test identities: {duplicates}")
            )
        if not self.names:
            self.problems.append(problem("selection", "No tests selected"))
        if self.problems:
            return

        program = self

        class JsonRunner(unittest.TextTestRunner):
            def _makeResult(self):
                result = JsonResult(
                    self.stream,
                    self.descriptions,
                    self.verbosity,
                    record_content=program.record_content,
                )
                program.json_result = result
                return result

        self.testRunner = JsonRunner
        super().runTests()


def verification_observed(record):
    # Lifecycle entry alone is not body entry. Setup skips and unsupported
    # custom lifecycles cannot certify the selected required work.
    return not any(
        event["skip_origin"] in {"setup", "unknown"} for event in record["events"]
    ) and (
        record["body_started"]
        or record["outcome"] in BAD_OUTCOMES
        or any(event["skip_origin"] == "policy" for event in record["events"])
    )


def report_for(argv, program, problems):
    names = program.names if program is not None else []
    result = program.json_result if program is not None else None
    records = result.records if result is not None else []
    started = [record["name"] for record in records if record["started"]]
    if program is not None:
        problems = [*program.problems, *problems]
    if result is not None and result.shouldStop and result.wasSuccessful():
        problems.append(
            problem("interrupted", "unittest stopped before normal completion")
        )
    if Counter(started) - Counter(names):
        problems.append(
            problem(
                "runner", "Execution included unselected or repeated test identities"
            )
        )
    selection = {
        "requested": list(argv),
        "names": names,
        "selected_count": len(names),
        "duplicates": sorted(
            name for name, count in Counter(names).items() if count > 1
        ),
        "unrun": list((Counter(names) - Counter(started)).elements()),
    }
    tests_run = result.testsRun if result is not None else 0
    complete = bool(
        tests_run > 0
        and tests_run == len(names)
        and Counter(started) == Counter(names)
        and not selection["duplicates"]
        and not problems
        and not (result and result.active)
        and all(record["outcome"] != "incomplete" for record in records)
        and all(verification_observed(record) for record in records)
    )
    successful = bool(
        complete
        and result.wasSuccessful()
        and not any(record["outcome"] in BAD_OUTCOMES for record in records)
    )
    return {
        "schema_version": 2,
        "runner": "unittest",
        "content_identity_scope": CONTENT_SCOPE,
        "reusable_validation_receipt": False,
        "complete": complete,
        "successful": successful,
        "tests_run": tests_run,
        "selection": selection,
        "tests": records,
        "errors": problems,
    }


def validate_report(payload):
    """Reject malformed or inconsistent worker reports; no freshness inference."""

    def require(condition, message):
        if not condition:
            raise ValueError(f"Invalid unittest report: {message}")

    def strings(value):
        return isinstance(value, list) and all(isinstance(item, str) for item in value)

    require(isinstance(payload, dict), "expected an object")
    require(
        type(payload.get("schema_version")) is int and payload["schema_version"] == 2,
        "schema_version",
    )
    require(payload.get("runner") == "unittest", "runner")
    require(
        payload.get("content_identity_scope") == CONTENT_SCOPE, "content identity scope"
    )
    require(payload.get("reusable_validation_receipt") is False, "receipt disclaimer")
    for field in ("complete", "successful"):
        require(type(payload.get(field)) is bool, field)
    require(
        type(payload.get("tests_run")) is int and payload["tests_run"] >= 0, "tests_run"
    )
    selection = payload.get("selection")
    require(isinstance(selection, dict), "selection")
    for field in ("requested", "names", "duplicates", "unrun"):
        require(strings(selection.get(field)), f"selection.{field}")
    names = selection["names"]
    require(all(names), "empty test identity")
    require(
        type(selection.get("selected_count")) is int
        and selection["selected_count"] == len(names),
        "selected_count",
    )
    require(
        selection["duplicates"]
        == sorted(name for name, count in Counter(names).items() if count > 1),
        "duplicates",
    )
    require(isinstance(payload.get("errors"), list), "errors")
    for error in payload["errors"]:
        require(isinstance(error, dict), "error entry")
        require(
            isinstance(error.get("kind"), str)
            and error["kind"] in {"selection", "runner", "interrupted"},
            "error kind",
        )
        require(
            isinstance(error.get("message"), str) and bool(error["message"]),
            "error message",
        )
        require(
            error.get("traceback") is None or isinstance(error["traceback"], str),
            "error traceback",
        )
    records = payload.get("tests")
    require(isinstance(records, list), "tests")
    for record in records:
        require(isinstance(record, dict), "test entry")
        require(
            isinstance(record.get("name"), str) and bool(record["name"]), "test name"
        )
        require(type(record.get("started")) is bool, "test started")
        require(type(record.get("body_started")) is bool, "test body_started")
        require(
            not record["body_started"] or record["started"],
            "body without lifecycle start",
        )
        require(
            isinstance(record.get("outcome"), str) and record["outcome"] in OUTCOMES,
            "test outcome",
        )
        require(
            record.get("failure_type") in (None, "assertion", "exception"),
            "test failure_type",
        )
        for field in ("content_before", "content_after"):
            require(field in record, field)
            identity = record[field]
            if identity is not None:
                require(isinstance(identity, dict), field)
                require(
                    isinstance(identity.get("path"), str) and bool(identity["path"]),
                    "content path",
                )
                require(
                    isinstance(identity.get("sha256"), str)
                    and re.fullmatch("[0-9a-f]{64}", identity["sha256"]) is not None,
                    "content sha256",
                )
        events = record.get("events")
        require(isinstance(events, list), "test events")
        require(
            bool(events) or record["outcome"] == "incomplete", "missing terminal event"
        )
        for event in events:
            require(isinstance(event, dict), "event")
            require("skip_origin" in event, "skip provenance missing")
            require(
                event.get("skip_origin")
                in (None, "policy", "setup", "after_body_entry", "unknown"),
                "skip origin",
            )
            require(
                (event.get("skip_origin") is not None)
                == (event.get("outcome") == "skipped"),
                "skip provenance",
            )
            if event.get("skip_origin") == "after_body_entry":
                require(record["body_started"], "skip without body entry")
            if event.get("skip_origin") in {"policy", "setup"}:
                require(not record["body_started"], "pre-body skip after body entry")
            require(
                isinstance(event.get("name"), str) and bool(event["name"]), "event name"
            )
            require(
                isinstance(event.get("outcome"), str)
                and event["outcome"] in OUTCOMES - {"incomplete"},
                "event outcome",
            )
            require(
                event.get("failure_type") in (None, "assertion", "exception"),
                "event failure_type",
            )
            require(
                event.get("reason") is None or isinstance(event["reason"], str),
                "event reason",
            )
            require(
                event.get("traceback") is None or isinstance(event["traceback"], str),
                "event traceback",
            )
            if event["outcome"] in {"failure", "error", "expected_failure"}:
                require(bool(event.get("traceback")), "missing failure traceback")
                require(
                    event["failure_type"] in {"assertion", "exception"},
                    "missing failure type",
                )
            if event["outcome"] == "failure":
                require(
                    event["failure_type"] == "assertion", "assertion classification"
                )
            elif event["outcome"] == "error":
                require(
                    event["failure_type"] == "exception", "exception classification"
                )
            elif event["outcome"] != "expected_failure":
                require(
                    event["failure_type"] is None, "unexpected failure classification"
                )
            if event["outcome"] in BAD_OUTCOMES:
                require(
                    record["outcome"] in BAD_OUTCOMES,
                    "failure event masked by successful outcome",
                )
        if events:
            priority = {
                "success": 0,
                "skipped": 1,
                "expected_failure": 1,
                "unexpected_success": 2,
                "failure": 3,
                "error": 4,
            }
            terminal = max(
                reversed(events), key=lambda event: priority[event["outcome"]]
            )
            require(record["outcome"] == terminal["outcome"], "aggregate test outcome")
            require(
                record["failure_type"] == terminal["failure_type"],
                "aggregate failure type",
            )
    started = [record["name"] for record in records if record["started"]]
    require(payload["tests_run"] == len(started), "started count")
    require(
        selection["unrun"] == list((Counter(names) - Counter(started)).elements()),
        "unrun coverage",
    )
    complete = bool(
        len(started) > 0
        and Counter(started) == Counter(names)
        and not selection["duplicates"]
        and not payload["errors"]
        and all(record["outcome"] != "incomplete" for record in records)
        and all(verification_observed(record) for record in records)
    )
    require(
        payload["complete"] == complete, "complete disagrees with execution coverage"
    )
    successful = complete and not any(
        record["outcome"] in BAD_OUTCOMES for record in records
    )
    require(payload["successful"] == successful, "successful disagrees with outcomes")


def diagnostic_stdout():
    """Reserve original stdout for JSON; permanently route other output to stderr."""
    sys.stdout.flush()
    report_stream = os.fdopen(
        os.dup(sys.stdout.fileno()), "w", encoding="utf-8", newline="\n"
    )
    os.dup2(sys.stderr.fileno(), sys.stdout.fileno())
    if os.name == "nt":
        # Native children use the Windows standard handle, not just CRT fd 1.
        import ctypes
        import msvcrt

        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.SetStdHandle.argtypes = [ctypes.c_ulong, ctypes.c_void_p]
        kernel32.SetStdHandle.restype = ctypes.c_int
        if not kernel32.SetStdHandle(-11 & 0xFFFFFFFF, msvcrt.get_osfhandle(1)):
            report_stream.close()
            raise ctypes.WinError(ctypes.get_last_error())
    sys.stdout = sys.stderr
    return report_stream


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    execution_only = argv[:1] == ["--execution-only"]
    if execution_only:
        argv = argv[1:]
    coalesce_selectors = argv[:1] == ["--coalesce-selectors"]
    if coalesce_selectors:
        argv = argv[1:]
    output = diagnostic_stdout()
    sys.path.insert(0, os.getcwd())
    program = None
    problems = []
    try:
        if not argv:
            problems.append(
                problem("selection", "Provide unittest selections or discover")
            )
        else:
            # Retain the program even if loading/execution raises.
            program = JsonProgram.__new__(JsonProgram)
            program.__init__(
                argv,
                record_content=not execution_only,
                coalesce_selectors=coalesce_selectors,
            )
    except KeyboardInterrupt:
        problems.append(
            problem(
                "interrupted", "Unittest execution interrupted", traceback.format_exc()
            )
        )
    except SystemExit as error:
        executing = program is not None and program.json_result is not None
        problems.append(
            problem(
                "runner" if executing else "selection",
                f"Unittest exited: {error.code}",
                traceback.format_exc(),
            )
        )
    except BaseException as error:
        problems.append(
            problem(
                "runner", f"{type(error).__name__}: {error}", traceback.format_exc()
            )
        )
    for error in problems:
        print(error["traceback"] or error["message"], file=sys.stderr)
    report = report_for(argv, program, problems)
    with output:
        json.dump(report, output, ensure_ascii=True, sort_keys=True)
        output.write("\n")
    if report["successful"]:
        return 0
    return 2 if report["errors"] else 1


if __name__ == "__main__":
    sys.exit(main())
