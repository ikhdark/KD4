#!/usr/bin/env python3
"""Run real unittest selections/discovery; stdout is one JSON document.

Usage: python -B scripts/unittest_json.py [-v] module.Class.test_name
       python -B scripts/unittest_json.py discover -s scripts -p 'test_*.py'

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
    except (OSError, TypeError, IndexError):
        return None


class JsonResult(unittest.TextTestResult):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.records = []
        self.active = {}

    def startTest(self, test):
        super().startTest(test)
        record = {
            "name": test.id(),
            "started": True,
            "outcome": "incomplete",
            "failure_type": None,
            "content_before": content_identity(test),
            "content_after": None,
            "events": [],
        }
        self.active[id(test)] = record
        self.records.append(record)

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
        }
        owner = parent or getattr(test, "test_case", test)
        record = self.active.get(id(owner))
        if record is None:
            # unittest fixture errors/skips do not call startTest/stopTest.
            record = {
                "name": test.id(),
                "started": False,
                "outcome": outcome,
                "failure_type": failure_type,
                "content_before": None,
                "content_after": None,
                "events": [],
            }
            self.records.append(record)
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
        record["content_after"] = content_identity(test)
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

    def __init__(self, argv):
        self.names = []
        self.problems = []
        self.json_result = None
        super().__init__(
            module=None,
            argv=[sys.argv[0], *argv],
            testLoader=unittest.TestLoader(),
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
                result = JsonResult(self.stream, self.descriptions, self.verbosity)
                program.json_result = result
                return result

        self.testRunner = JsonRunner
        super().runTests()


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
    )
    successful = bool(
        complete
        and result.wasSuccessful()
        and not any(record["outcome"] in BAD_OUTCOMES for record in records)
    )
    return {
        "schema_version": 1,
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
        type(payload.get("schema_version")) is int and payload["schema_version"] == 1,
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
            program.__init__(argv)
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
