"""Test output is diagnostic text, never authority for execution receipts."""

from __future__ import annotations

import contextlib
import io
import json
import subprocess
import unittest

from scripts.build import rust_test_runner
from scripts.build.test_rust_test_runner import RunnerTestCase
from scripts.build.test_rust_test_runner import nextest_run_payload


class NextestReceiptBoundariesTest(RunnerTestCase):
    def test_gate_binds_grouped_ignored_starts_to_its_exact_selection(self):
        # Native Code Mode reporter shape: multiple ignored starts in one
        # fragment, then a separately framed selected completion.
        metadata = {"crate": "codex-core", "test_binary": "codex_core", "kind": "lib"}
        start = {"type": "suite", "event": "started", "test_count": 3, "nextest": metadata}
        terminal = {"type": "suite", "event": "ok", "passed": 0, "failed": 0,
                    "ignored": 2, "measured": 0, "filtered_out": 10, "nextest": metadata}

        def event(kind, name):
            return {"type": "test", "event": kind, "name": "codex-core::codex_core$" + name}

        native = [start, event("started", "skip_one"), event("started", "skip_two"),
                  terminal, start, event("started", "alpha"), event("ok", "alpha"),
                  {**terminal, "passed": 1}]
        for case in ("native", "missing_completion", "duplicate_start", "selected_skip"):
            with self.subTest(case=case):
                events = json.loads(json.dumps(native))
                if case == "missing_completion":
                    del events[6]
                elif case == "duplicate_start":
                    events[2] = event("started", "skip_one")
                elif case == "selected_skip":
                    events[1] = event("started", "alpha")
                manifest = self.manifest(gates={"proof": {"steps": [
                    {"target": "core_lib", "tests": ["alpha"], "helpers": []}
                ]}})
                runner, _ = self.runner(manifest=manifest)
                calls = []

                def execute(args, **kwargs):
                    calls.append(args)
                    return subprocess.CompletedProcess(args, 0, "\n".join(map(json.dumps, events)), "")

                runner.executor = execute
                with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    if case == "native":
                        result = runner.run_gates(["proof"], quiet=True)
                        self.assertEqual(result.completed_tests(), {"codex-core": ["alpha"]})
                    else:
                        with self.assertRaises(rust_test_runner.RunnerError):
                            runner.run_gates(["proof"], quiet=True)
                self.assertEqual(len(calls), 1, "report reconciliation never replays tests")

    def test_exact_selection_reconciles_mixed_execution_and_anonymous_skips(self):
        from dataclasses import replace

        # Native stream from validation 68ed8e37-c715-4621-aee4-fc98c21b871e:
        # ignored starts can share a terminal with completed selected tests,
        # without a corresponding per-test ignored event.
        metadata = {"crate": "codex-core", "test_binary": "codex_core", "kind": "lib"}

        def event(kind, name):
            return {
                "type": "test",
                "event": kind,
                "name": "codex-core::codex_core$" + name,
            }

        native = [
            {"type": "suite", "event": "started", "test_count": 4, "nextest": metadata},
            event("started", "skip_one"),
            event("started", "alpha"),
            event("started", "skip_two"),
            event("ok", "alpha"),
            {"type": "suite", "event": "ok", "passed": 1, "failed": 0,
                "ignored": 3, "measured": 0, "filtered_out": 20, "nextest": metadata},
        ]
        for case in (
            "native", "inflight_selected", "unbound", "missing_completion", "missing_terminal",
            "wrong_count", "too_many_skips", "duplicate_start", "unexpected_pass",
        ):
            with self.subTest(case=case):
                events = json.loads(json.dumps(native))
                if case == "inflight_selected":
                    events[0]["test_count"] = 5
                    events.insert(4, event("started", "beta"))
                    events.extend([
                        dict(events[0]), event("ok", "beta"), dict(events[-1]),
                    ])
                elif case == "missing_completion":
                    del events[4]
                    events[-1]["passed"] = 0
                elif case == "missing_terminal":
                    events.pop()
                elif case == "wrong_count":
                    events[0]["test_count"] = 5
                elif case == "too_many_skips":
                    events[-1]["ignored"] = 1
                elif case == "duplicate_start":
                    events[3] = event("started", "skip_one")
                elif case == "unexpected_pass":
                    events.insert(-1, event("ok", "skip_one"))
                    events[-1]["passed"] = 2
                manifest = self.manifest()
                manifest.targets["core_lib"] = replace(
                    manifest.targets["core_lib"], helpers=(), helpers_by_test_prefix={}
                )
                runner, _ = self.runner(manifest=manifest)
                calls = []

                def execute(args, **kwargs):
                    calls.append(args)
                    return subprocess.CompletedProcess(
                        args, 0, "\n".join(map(json.dumps, events)), ""
                    )

                runner.executor = execute
                selection = "test(alpha)" if case == "unbound" else "test(=alpha)"
                if case == "inflight_selected":
                    selection += " | test(=beta)"
                with (
                    contextlib.redirect_stdout(io.StringIO()),
                    contextlib.redirect_stderr(io.StringIO()),
                ):
                    if case in ("native", "inflight_selected"):
                        self.assertEqual(
                            runner.run_target("core_lib", ["-E", selection]),
                            {"codex-core": ["alpha", "beta"] if case == "inflight_selected" else ["alpha"]},
                        )
                    else:
                        with self.assertRaises(rust_test_runner.RunnerError):
                            runner.run_target("core_lib", ["-E", selection])
                self.assertEqual(len(calls), 1, "reconciliation must not rerun tests")

    def test_exact_selection_reconciles_multiple_anonymous_skips_in_one_fragment(self):
        from dataclasses import replace

        metadata = {"crate": "codex-core", "test_binary": "codex_core", "kind": "lib"}
        start = {
            "type": "suite",
            "event": "started",
            "test_count": 4,
            "nextest": metadata,
        }
        terminal = {
            "type": "suite",
            "event": "ok",
            "passed": 0,
            "failed": 0,
            "ignored": 3,
            "measured": 0,
            "filtered_out": 20,
            "nextest": metadata,
        }

        def event(kind, name):
            return {
                "type": "test",
                "event": kind,
                "name": "codex-core::codex_core$" + name,
            }

        native = [
            start,
            event("started", "skip_one"),
            event("started", "skip_two"),
            terminal,
            start,
            event("started", "alpha"),
            event("ok", "alpha"),
            {**terminal, "passed": 1},
        ]
        for case in (
            "native",
            "selected_skip",
            "too_many_skips",
            "unexpected_pass",
            "duplicate_start",
            "missing_completion",
        ):
            with self.subTest(case=case):
                events = json.loads(json.dumps(native))
                if case == "selected_skip":
                    events[1] = event("started", "alpha")
                elif case == "too_many_skips":
                    events[0]["test_count"] = events[4]["test_count"] = 2
                    events[3]["ignored"] = events[7]["ignored"] = 1
                elif case == "unexpected_pass":
                    events.insert(3, event("ok", "skip_one"))
                    events[4]["passed"] = 1
                elif case == "duplicate_start":
                    events[2] = event("started", "skip_one")
                elif case == "missing_completion":
                    del events[6]
                manifest = self.manifest()
                manifest.targets["core_lib"] = replace(
                    manifest.targets["core_lib"], helpers=(), helpers_by_test_prefix={}
                )
                runner, _ = self.runner(manifest=manifest)
                calls = []

                def execute(args, **kwargs):
                    calls.append(args)
                    return subprocess.CompletedProcess(
                        args, 0, "\n".join(map(json.dumps, events)), ""
                    )

                runner.executor = execute
                with (
                    contextlib.redirect_stdout(io.StringIO()),
                    contextlib.redirect_stderr(io.StringIO()),
                ):
                    if case == "native":
                        self.assertEqual(
                            runner.run_target("core_lib", ["-E", "test(=alpha)"]),
                            {"codex-core": ["alpha"]},
                        )
                    else:
                        with self.assertRaises(rust_test_runner.RunnerError):
                            runner.run_target("core_lib", ["-E", "test(=alpha)"])
                self.assertEqual(
                    len(calls), 1, "report reconciliation never replays tests"
                )

    def test_exact_selection_binds_anonymous_unselected_ignored_counts(self):
        from dataclasses import replace

        metadata = {"crate": "codex-core", "test_binary": "codex_core", "kind": "lib"}
        events = [
            {"type": "suite", "event": "started", "test_count": 3, "nextest": metadata},
            {
                "type": "test",
                "event": "started",
                "name": "codex-core::codex_core$alpha",
            },
            {"type": "test", "event": "ok", "name": "codex-core::codex_core$alpha"},
            {
                "type": "suite",
                "event": "ok",
                "passed": 1,
                "failed": 0,
                "ignored": 2,
                "measured": 0,
                "filtered_out": 0,
                "nextest": metadata,
            },
        ]
        manifest = self.manifest()
        manifest.targets["core_lib"] = replace(
            manifest.targets["core_lib"], helpers=(), helpers_by_test_prefix={}
        )
        runner, _ = self.runner(manifest=manifest)
        calls = []

        def execute(args, **kwargs):
            calls.append(args)
            return subprocess.CompletedProcess(
                args, 0, "\n".join(map(json.dumps, events)), ""
            )

        runner.executor = execute
        with (
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            self.assertEqual(
                runner.run_target("core_lib", ["-E", "test(=alpha)"]),
                {"codex-core": ["alpha"]},
            )
        self.assertEqual(
            len(calls), 1, "exact selection is already independent count evidence"
        )

    def test_ignored_execution_uses_independent_catalog_without_replaying(self):
        from dataclasses import replace

        metadata = {"crate": "codex-core", "test_binary": "codex_core", "kind": "lib"}
        start = {
            "type": "suite",
            "event": "started",
            "test_count": 3,
            "nextest": metadata,
        }
        empty = {
            "type": "suite",
            "event": "ok",
            "passed": 0,
            "failed": 0,
            "ignored": 3,
            "measured": 0,
            "filtered_out": 10,
            "nextest": metadata,
        }

        def event(kind, name="alpha"):
            return {
                "type": "test",
                "event": kind,
                "name": "codex-core::codex_core$" + name,
            }

        native = [
            start,
            event("started"),
            empty,
            start,
            event("ok"),
            {**empty, "passed": 1},
        ]
        listing = {
            "test-count": 3,
            "rust-suites": {
                "codex-core": {
                    "testcases": {
                        name: {
                            "ignored": True,
                            "filter-match": {
                                "status": "matches" if name == "alpha" else "mismatch"
                            },
                        }
                        for name in ("alpha", "unselected_one", "unselected_two")
                    }
                }
            },
        }
        for case in (
            "native",
            "duplicate",
            "unstarted",
            "missing_terminal",
            "wrong_count",
            "wrong_ignored",
            "unexpected",
        ):
            with self.subTest(case=case):
                events = json.loads(json.dumps(native))
                if case == "duplicate":
                    events.insert(-1, event("ok"))
                    events[-1]["passed"] = 2
                elif case == "unstarted":
                    del events[1]
                elif case == "missing_terminal":
                    events.pop()
                elif case == "wrong_count":
                    events[0]["test_count"] = events[3]["test_count"] = 4
                elif case == "wrong_ignored":
                    events[2]["ignored"] = events[5]["ignored"] = 2
                elif case == "unexpected":
                    events[1] = event("started", "foreign")
                    events[4] = event("ok", "foreign")
                manifest = self.manifest()
                manifest.targets["core_lib"] = replace(
                    manifest.targets["core_lib"], helpers=(), helpers_by_test_prefix={}
                )
                runner, _ = self.runner(manifest=manifest)
                calls = []

                def execute(args, **kwargs):
                    calls.append(args)
                    output = (
                        json.dumps(listing)
                        if args[:3] == ["cargo", "nextest", "list"]
                        else "\n".join(map(json.dumps, events))
                    )
                    return subprocess.CompletedProcess(args, 0, output, "")

                runner.executor = execute
                with (
                    contextlib.redirect_stdout(io.StringIO()),
                    contextlib.redirect_stderr(io.StringIO()),
                ):
                    if case == "native":
                        self.assertEqual(
                            runner.run_target(
                                "core_lib",
                                ["-E", "test(=alpha)", "--run-ignored", "all"],
                            ),
                            {"codex-core": ["alpha"]},
                        )
                    else:
                        with self.assertRaises(rust_test_runner.RunnerError):
                            runner.run_target(
                                "core_lib",
                                ["-E", "test(=alpha)", "--run-ignored", "all"],
                            )
                self.assertEqual(
                    sum(c[:3] == ["cargo", "nextest", "run"] for c in calls), 1
                )

    def test_target_and_gate_withhold_malformed_partial_proof(self):
        from dataclasses import replace

        for route in ("target", "gate"):
            for case in (
                "valid",
                "no_start",
                "no_summary",
                "wrong_owner",
                "after_summary",
                "duplicate",
                "wrong_count",
                "wrong_global_count",
                "invalid_global_count",
                "duplicate_summary",
            ):
                with self.subTest(route=route, case=case):
                    events = [
                        json.loads(line)
                        for line in nextest_run_payload([("alpha", "ok")]).splitlines()
                    ]
                    if case == "no_start":
                        del events[1]
                    elif case == "no_summary":
                        events.pop()
                    elif case == "wrong_owner":
                        events[-1]["nextest"]["kind"] = "bin"
                    elif case == "after_summary":
                        events[-1], events[-2] = events[-2], events[-1]
                    elif case == "duplicate":
                        events.insert(-1, dict(events[2]))
                    elif case == "wrong_count":
                        events[-1]["passed"] = 2
                    elif case == "wrong_global_count":
                        events[0]["test_count"] = 2
                    elif case == "invalid_global_count":
                        events[0]["test_count"] = False
                    elif case == "duplicate_summary":
                        events.append(dict(events[-1]))
                    manifest = self.manifest(
                        gates={
                            "proof": {
                                "steps": [
                                    {
                                        "target": "core_lib",
                                        "tests": ["alpha"],
                                        "helpers": [],
                                    }
                                ]
                            }
                        }
                    )
                    manifest.targets["core_lib"] = replace(
                        manifest.targets["core_lib"],
                        helpers=(),
                        helpers_by_test_prefix={},
                    )
                    runner, _ = self.runner(manifest=manifest)
                    calls = []

                    def execute(args, **kwargs):
                        calls.append(args)
                        return subprocess.CompletedProcess(
                            args, 0, "\n".join(map(json.dumps, events)), ""
                        )

                    runner.executor = execute
                    with (
                        contextlib.redirect_stdout(io.StringIO()),
                        contextlib.redirect_stderr(io.StringIO()),
                    ):
                        run = (
                            (
                                lambda: runner.run_target(
                                    "core_lib", ["-E", "test(=alpha)"]
                                )
                            )
                            if route == "target"
                            else (lambda: runner.run_gates(["proof"], quiet=True))
                        )
                        if case == "valid":
                            self.assertTrue(run())
                        else:
                            with self.assertRaises(
                                rust_test_runner.RunnerError
                            ) as caught:
                                run()
                            error = caught.exception
                            receipts = (
                                error.completed_tests
                                if route == "target"
                                else error.completed_gates
                            )
                            facts = (
                                receipts
                                if route == "target"
                                else receipts.completed_tests()
                            )
                            self.assertEqual(error.outcome, "evidence_incomplete")
                            expected = (
                                {"codex-core": ["alpha"]}
                                if case == "wrong_global_count"
                                else {}
                            )
                            self.assertEqual(
                                facts,
                                expected,
                                "only independently framed test results may enter repair unions",
                            )
                            if route == "gate":
                                self.assertEqual(
                                    dict(receipts),
                                    {},
                                    "partial facts cannot certify a complete gate",
                                )
                    self.assertEqual(len(calls), 1)

    def test_partial_receipts_preserve_only_independent_valid_completions(self):
        from dataclasses import replace

        for route in ("target", "gate"):
            for case in (
                "missing_start",
                "duplicate",
                "foreign_incomplete",
                "unfinished_fragment",
                "wrong_fragment_count",
                "abnormal_exit",
            ):
                with self.subTest(route=route, case=case):
                    events = [
                        json.loads(line)
                        for line in nextest_run_payload(
                            [("alpha", "ok"), ("beta", "ok")]
                        ).splitlines()
                    ]
                    if case == "missing_start":
                        del events[3]
                    elif case == "duplicate":
                        events = [
                            json.loads(line)
                            for line in nextest_run_payload(
                                [("alpha", "ok"), ("beta", "ok"), ("beta", "ok")]
                            ).splitlines()
                        ]
                    elif case == "foreign_incomplete":
                        events.extend(
                            json.loads(line)
                            for line in nextest_run_payload(
                                [("unrelated", "ok")], package="other", binary="other"
                            ).splitlines()[:-1]
                        )
                    elif case == "unfinished_fragment":
                        # An ignored test allows the reporter to reopen its
                        # binary. Only the first fragment has a valid terminal.
                        events = [
                            json.loads(line)
                            for line in nextest_run_payload(
                                [("alpha", "ok"), ("ignored", "ignored")]
                            ).splitlines()
                        ]
                        events[0]["test_count"] = 3
                        events.extend(
                            [
                                dict(events[0]),
                                {
                                    "type": "test",
                                    "event": "started",
                                    "name": "codex-core::codex_core$beta",
                                },
                                {
                                    "type": "test",
                                    "event": "ok",
                                    "name": "codex-core::codex_core$beta",
                                },
                            ]
                        )
                    elif case == "wrong_fragment_count":
                        events[-1]["passed"] = 99
                    manifest = self.manifest(
                        gates={
                            "proof": {
                                "steps": [
                                    {
                                        "target": "core_lib",
                                        "tests": ["alpha", "beta"],
                                        "helpers": [],
                                    }
                                ]
                            }
                        }
                    )
                    manifest.targets["core_lib"] = replace(
                        manifest.targets["core_lib"],
                        helpers=(),
                        helpers_by_test_prefix={},
                    )
                    runner, _ = self.runner(manifest=manifest)
                    calls = []

                    def execute(args, **kwargs):
                        calls.append(args)
                        return subprocess.CompletedProcess(
                            args,
                            101 if case == "abnormal_exit" else 0,
                            "\n".join(map(json.dumps, events)),
                            "",
                        )

                    runner.executor = execute
                    with (
                        contextlib.redirect_stdout(io.StringIO()),
                        contextlib.redirect_stderr(io.StringIO()),
                    ):
                        with self.assertRaises(rust_test_runner.RunnerError) as caught:
                            if route == "target":
                                runner.run_target(
                                    "core_lib", ["-E", "test(=alpha) | test(=beta)"]
                                )
                            else:
                                runner.run_gates(["proof"], quiet=True)
                    receipts = (
                        caught.exception.completed_tests
                        if route == "target"
                        else caught.exception.completed_gates
                    )
                    if route == "gate":
                        self.assertEqual(
                            dict(receipts),
                            {},
                            "partial execution cannot certify the gate",
                        )
                    expected = (
                        []
                        if case in ("wrong_fragment_count", "abnormal_exit")
                        else (
                            ["alpha", "beta"]
                            if case == "foreign_incomplete"
                            else ["alpha"]
                        )
                    )
                    self.assertEqual(
                        receipts.completed_tests(),
                        {"codex-core": expected} if expected else {},
                    )
                    published = io.StringIO()
                    with contextlib.redirect_stdout(published):
                        rust_test_runner.emit_execution_receipt(
                            "a" * 64,
                            receipts,
                            ["core_lib" if route == "target" else "proof"],
                            skipped=None,
                            exit_code=2,
                        )
                    payload = json.loads(published.getvalue())
                    self.assertEqual(
                        payload["completed_tests"], receipts.completed_tests()
                    )
                    self.assertEqual(
                        [row["test"] for row in payload["executions"]], expected
                    )
                    self.assertEqual(
                        [row["test"] for row in payload["required_executions"]],
                        ["alpha", "beta"],
                    )
                    self.assertEqual(payload["executed_tests"], len(expected))
                    self.assertEqual(payload["exit_code"], 2)
                    self.assertEqual(
                        len(calls),
                        1,
                        "proof accounting must not discover, rebuild or rerun",
                    )

    def test_exact_target_rejects_an_unrequested_completed_identity(self):
        from dataclasses import replace

        manifest = self.manifest()
        manifest.targets["core_lib"] = replace(
            manifest.targets["core_lib"], helpers=(), helpers_by_test_prefix={}
        )
        runner, _ = self.runner(manifest=manifest)
        runner.executor = lambda args, **kwargs: subprocess.CompletedProcess(
            args, 0, nextest_run_payload([("alpha", "ok"), ("extra", "ok")]), ""
        )
        with self.assertRaises(rust_test_runner.RunnerError):
            runner.run_target("core_lib", ["-E", "test(=alpha)"])

    def test_direct_target_requires_complete_suite_evidence_without_extra_discovery(
        self,
    ):
        from dataclasses import replace

        complete = [
            json.loads(line)
            for line in nextest_run_payload(
                [("alpha", "ok"), ("beta", "ok")]
            ).splitlines()
        ]
        for case in (
            "complete",
            "missing_test",
            "missing_terminal",
            "wrong_count",
            "duplicate",
        ):
            with self.subTest(case=case):
                events = [dict(event) for event in complete]
                if case == "missing_test":
                    events = [
                        event
                        for event in events
                        if not event.get("name", "").endswith("$beta")
                    ]
                elif case == "missing_terminal":
                    events.pop()
                elif case == "wrong_count":
                    events[-1]["passed"] = 3
                elif case == "duplicate":
                    events.insert(-1, dict(events[4]))
                manifest = self.manifest()
                manifest.targets["core_lib"] = replace(
                    manifest.targets["core_lib"], helpers=(), helpers_by_test_prefix={}
                )
                runner, _ = self.runner(manifest=manifest)
                calls = []

                def execute(args, **kwargs):
                    calls.append(args)
                    return subprocess.CompletedProcess(
                        args, 0, "\n".join(json.dumps(event) for event in events), ""
                    )

                runner.executor = execute
                with (
                    contextlib.redirect_stdout(io.StringIO()),
                    contextlib.redirect_stderr(io.StringIO()),
                ):
                    if case == "complete":
                        self.assertEqual(
                            runner.run_target(
                                "core_lib", ["-E", "test(alpha) | test(beta)"]
                            ),
                            {"codex-core": ["alpha", "beta"]},
                        )
                    else:
                        with self.assertRaises(rust_test_runner.RunnerError) as caught:
                            runner.run_target(
                                "core_lib", ["-E", "test(alpha) | test(beta)"]
                            )
                        self.assertIn("reporter evidence", str(caught.exception))
                        self.assertEqual(caught.exception.completed_tests, {})
                self.assertEqual(
                    len(calls), 1, "framing must not add a discovery invocation"
                )

    def test_direct_target_reconciles_fragmented_suite_without_losing_identity_checks(
        self,
    ):
        from dataclasses import replace

        # Native libtest-json-plus shape retained from validation
        # db7afd95-188f-4591-9771-58fb49beb05e: an unmatched ignored case
        # causes a suite terminal/restart while another started test is running.
        metadata = {"crate": "codex-core", "test_binary": "codex_core", "kind": "lib"}
        start = {
            "type": "suite",
            "event": "started",
            "test_count": 3,
            "nextest": metadata,
        }
        terminal = {
            "type": "suite",
            "event": "ok",
            "passed": 1,
            "failed": 0,
            "ignored": 1,
            "measured": 0,
            "filtered_out": 142,
            "nextest": metadata,
        }

        def test_event(event, name):
            return {
                "type": "test",
                "event": event,
                "name": f"codex-core::codex_core${name}",
            }

        complete = [
            start,
            test_event("started", "outside_filter"),
            test_event("started", "alpha"),
            test_event("started", "beta"),
            test_event("ignored", "outside_filter"),
            test_event("ok", "alpha"),
            terminal,
            start,
            test_event("ok", "beta"),
            {**terminal, "filtered_out": 143},
        ]
        for case in (
            "complete",
            "ignored_without_start",
            "missing_test",
            "missing_terminal",
            "duplicate_test",
            "changed_identity",
            "changed_count",
            "float_count",
            "missing_start",
            "missing_ignored",
            "overlapping_start",
            "wrong_fragment_count",
        ):
            with self.subTest(case=case):
                events = json.loads(json.dumps(complete))
                if case == "ignored_without_start":
                    del events[1]
                elif case == "missing_test":
                    del events[8]
                elif case == "missing_terminal":
                    events.pop()
                elif case == "duplicate_test":
                    events[8] = test_event("ok", "alpha")
                elif case == "changed_identity":
                    events[7]["nextest"]["kind"] = "bin"
                elif case == "changed_count":
                    events[7]["test_count"] = 2
                elif case == "float_count":
                    events[7]["test_count"] = 3.0
                elif case == "missing_start":
                    del events[3]
                elif case == "missing_ignored":
                    del events[4]
                elif case == "overlapping_start":
                    events[6], events[7] = events[7], events[6]
                elif case == "wrong_fragment_count":
                    events[6]["passed"] = 2
                manifest = self.manifest()
                manifest.targets["core_lib"] = replace(
                    manifest.targets["core_lib"], helpers=(), helpers_by_test_prefix={}
                )
                runner, _ = self.runner(manifest=manifest)
                calls = []

                def execute(args, **kwargs):
                    calls.append(args)
                    return subprocess.CompletedProcess(
                        args, 0, "\n".join(json.dumps(event) for event in events), ""
                    )

                runner.executor = execute
                with (
                    contextlib.redirect_stdout(io.StringIO()),
                    contextlib.redirect_stderr(io.StringIO()),
                ):
                    # Validation 68ed8e37 also establishes that a native mixed
                    # fragment can omit an unselected test's ignored event.
                    # Exact selected IDs and the repeated ignored count bind
                    # that omission; missing selected completions still fail.
                    if case in ("complete", "ignored_without_start", "missing_ignored"):
                        self.assertEqual(
                            runner.run_target(
                                "core_lib", ["-E", "test(=alpha) | test(=beta)"]
                            ),
                            {"codex-core": ["alpha", "beta"]},
                        )
                    else:
                        with self.assertRaises(rust_test_runner.RunnerError):
                            runner.run_target(
                                "core_lib", ["-E", "test(=alpha) | test(=beta)"]
                            )
                self.assertEqual(
                    len(calls), 1, "report reconciliation must not rerun tests"
                )

    def test_direct_target_reconciles_skip_only_fragments_without_execution_receipts(
        self,
    ):
        from dataclasses import replace

        # Native stream from validation fab29cad-31c0-4270-b737-fdb572b691bc:
        # three skipped fixture names each get a started/empty-suite fragment,
        # not an ignored event; only the final selected test actually completes.
        metadata = {"crate": "codex-core", "test_binary": "codex_core", "kind": "lib"}
        start = {
            "type": "suite",
            "event": "started",
            "test_count": 4,
            "nextest": metadata,
        }
        terminal = {
            "type": "suite",
            "event": "ok",
            "passed": 0,
            "failed": 0,
            "ignored": 3,
            "measured": 0,
            "filtered_out": 102,
            "nextest": metadata,
        }

        def test_event(event, name):
            return {
                "type": "test",
                "event": event,
                "name": f"codex-core::codex_core${name}",
            }

        complete = []
        for name in ("skipped-a", "skipped-b", "skipped-c"):
            complete.extend([start, test_event("started", name), terminal])
        complete.extend(
            [
                start,
                test_event("started", "alpha"),
                test_event("ok", "alpha"),
                {**terminal, "passed": 1},
            ]
        )
        cases = (
            "complete",
            "missing_skip_start",
            "missing_selected_completion",
            "missing_terminal",
            "duplicate_skip",
            "ignored_count_changed",
            "wrong_global_count",
            "unstarted_pass",
            "overlapping_start",
            "multiple_unfinished_starts",
        )
        # Exact IDs bind the count, so an unnamed skip is an anonymous unselected
        # ignored case there; a name filter still needs every skip named.
        for case, selection in [
            (case, selection)
            for case in cases
            for selection in ("test(=alpha)", "test(alpha)")
        ]:
            accepted = case == "complete" or (
                case == "missing_skip_start" and selection == "test(=alpha)"
            )
            with self.subTest(case=case, selection=selection):
                events = json.loads(json.dumps(complete))
                if case == "missing_skip_start":
                    del events[4]
                elif case == "missing_selected_completion":
                    del events[11]
                elif case == "missing_terminal":
                    events.pop()
                elif case == "duplicate_skip":
                    events[4] = test_event("started", "skipped-a")
                elif case == "ignored_count_changed":
                    events[5]["ignored"] = 2
                elif case == "wrong_global_count":
                    for event in events:
                        if (
                            event.get("type") == "suite"
                            and event.get("event") == "started"
                        ):
                            event["test_count"] = 5
                elif case == "unstarted_pass":
                    del events[10]
                elif case == "overlapping_start":
                    events[2], events[3] = events[3], events[2]
                elif case == "multiple_unfinished_starts":
                    events.insert(2, test_event("started", "unexplained"))
                manifest = self.manifest()
                manifest.targets["core_lib"] = replace(
                    manifest.targets["core_lib"], helpers=(), helpers_by_test_prefix={}
                )
                runner, _ = self.runner(manifest=manifest)
                calls = []

                def execute(args, **kwargs):
                    calls.append(args)
                    return subprocess.CompletedProcess(
                        args, 0, "\n".join(json.dumps(event) for event in events), ""
                    )

                runner.executor = execute
                with (
                    contextlib.redirect_stdout(io.StringIO()),
                    contextlib.redirect_stderr(io.StringIO()),
                ):
                    if accepted:
                        self.assertEqual(
                            runner.run_target("core_lib", ["-E", selection]),
                            {"codex-core": ["alpha"]},
                        )
                    else:
                        with self.assertRaises(rust_test_runner.RunnerError):
                            runner.run_target("core_lib", ["-E", selection])
                self.assertEqual(
                    len(calls), 1, "reconcile retained evidence without replay"
                )

    @staticmethod
    def immediate_output(stdout: str, stderr: str = "diagnostic") -> str:
        # Native nextest framing observed in retained
        # rust-test-stderr-3rlycx6k.log, lines 250-264: result header,
        # two-space stream labels, four-space payload, unindented separator.
        # The fixture is inline so validation never depends on retained logs.
        return (
            "        PASS [   0.001s] codex-core alpha\n"
            "  stdout ───\n\n"
            + "\n".join("    " + line for line in stdout.split("\n"))
            + "\n\n  stderr ───\n"
            + "\n".join("    " + line for line in stderr.split("\n"))
            + "\n\n────────────\n"
        )

    def run_output(
        self,
        output: str,
        *required: str,
        artifact: bool = False,
        executed: tuple[str, ...] | None = None,
        structured: bool = True,
        event_output: str | None = None,
    ):
        manifest = self.manifest(
            gates={
                "proof": {
                    "steps": [
                        {
                            "target": "core_lib",
                            "tests": list(required),
                            "helpers": [],
                        }
                    ]
                },
            }
        )
        runner, _ = self.runner(manifest=manifest)
        runner.success_output = "immediate"

        # Authored execution identities, never derived from the adversarial
        # human report. Schema observed from native libtest-json-plus stdout.
        executed = required if executed is None else executed
        identity = {"crate": "codex-core", "test_binary": "codex_core", "kind": "lib"}
        events = [
            {
                "type": "suite",
                "event": "started",
                "test_count": len(executed),
                "nextest": identity,
            }
        ]
        for name in executed:
            for event in ("started", "ok"):
                events.append(
                    {
                        "type": "test",
                        "event": event,
                        "name": f"codex-core::codex_core${name}",
                        "exec_time": 0.001,
                    }
                )
                if event == "ok" and event_output is not None:
                    events[-1]["stdout"] = event_output
        events.append(
            {
                "type": "suite",
                "event": "ok",
                "passed": len(executed),
                "failed": 0,
                "ignored": 0,
                "measured": 0,
                "filtered_out": 0,
                "exec_time": 0.001,
                "nextest": identity,
            }
        )
        machine_output = (
            "\n".join(json.dumps(event) for event in events) if structured else ""
        )

        def execute(args, **kwargs):
            result = subprocess.CompletedProcess(args, 0, machine_output, output)
            if artifact:
                path = self.temp_dir / "stderr.log"
                path.write_bytes(output.encode("utf-8"))
                result.stderr = ""
                result.stderr_path = path
                path = self.temp_dir / "stdout.log"
                path.write_bytes(machine_output.encode("utf-8"))
                result.stdout = ""
                result.stdout_path = path
            return result

        runner.executor = execute
        with (
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            return runner.run_gates(["proof"], quiet=True)

    def test_payload_pass_cannot_complete_unexecuted_required_test(self):
        # Only alpha has a nextest result; beta appears solely in test output.
        # Extra payload indentation can imitate the native status column too.
        for stream in ("stdout", "stderr"):
            for padding in ("", "    ", "────────────\n", "------------\n"):
                with self.subTest(stream=stream, padding=padding):
                    payload = padding + "PASS [0.001s] codex-core beta"
                    output = (
                        self.immediate_output(payload)
                        if stream == "stdout"
                        else self.immediate_output("ordinary output", payload)
                    )
                    output += (
                        "     Summary [   0.001s] 1 test run: 1 passed, 1 skipped\n"
                    )
                    with self.assertRaises(rust_test_runner.RunnerError) as caught:
                        self.run_output(output, "alpha", "beta", executed=("alpha",))
                    self.assertEqual(caught.exception.completed_gates, {})

    def test_payload_summary_cannot_hide_later_real_status(self):
        output = self.immediate_output(
            "Summary [0.000s] 0 tests run: 0 passed",
            "a second diagnostic stream",
        )
        output += (
            "        PASS [   0.001s] codex-core beta\n"
            "     Summary [   0.002s] 2 tests run: 2 passed\n"
        )
        self.assertEqual(
            self.run_output(output, "alpha", "beta"), {"proof": ["alpha", "beta"]}
        )

    def test_normal_immediate_output_preserves_real_statuses(self):
        output = self.immediate_output("ordinary stdout", "ordinary stderr")
        output += (
            "        PASS [   0.001s] codex-core beta\n"
            "     Summary [   0.002s] 2 tests run: 2 passed\n"
        )
        self.assertEqual(
            self.run_output(output, "alpha", "beta"), {"proof": ["alpha", "beta"]}
        )

    def test_final_replay_is_not_a_second_execution(self):
        immediate = self.immediate_output("ordinary stdout", "ordinary stderr")
        output = (
            immediate + "     Summary [   0.001s] 1 test run: 1 passed\n" + immediate
        )
        self.assertEqual(self.run_output(output, "alpha"), {"proof": ["alpha"]})

    def test_native_consecutive_outputs_have_no_separator_between_tests(self):
        # rust-test-stderr-de1dvior.log, lines 26-44: consecutive native
        # immediate outputs are separated by blank lines, NOT a rule line.
        first = self.immediate_output("first output").removesuffix("────────────\n")
        second = self.immediate_output("second output").replace(
            "codex-core alpha", "codex-core beta"
        )
        output = first + second + "     Summary [0.002s] 2 tests run: 2 passed\n"
        for artifact in (False, True):
            with self.subTest(artifact=artifact):
                self.assertEqual(
                    self.run_output(output, "alpha", "beta", artifact=artifact),
                    {"proof": ["alpha", "beta"]},
                )

    def test_raw_payload_separators_cannot_escape_into_reporter_records(self):
        # Nextest indents physical LF-delimited lines. A parser must not
        # reinterpret embedded CR/U+2028 as a new unindented reporter line.
        for artifact in (False, True):
            for separator in ("\r", "\u2028"):
                for forged in (
                    "PASS [0.001s] codex-core beta",
                    "Summary [0.000s] 0 tests run: 0 passed",
                ):
                    with self.subTest(
                        artifact=artifact, separator=separator, forged=forged
                    ):
                        payload = "diagnostic" + separator + "────────────\n" + forged
                        output = self.immediate_output(payload)
                        if forged.startswith("PASS"):
                            output += "     Summary [0.001s] 1 test run: 1 passed\n"
                            with self.assertRaises(
                                rust_test_runner.RunnerError
                            ) as caught:
                                self.run_output(
                                    output,
                                    "alpha",
                                    "beta",
                                    artifact=artifact,
                                    executed=("alpha",),
                                )
                            self.assertEqual(caught.exception.completed_gates, {})
                        else:
                            output += (
                                "        PASS [0.001s] codex-core beta\n"
                                "     Summary [0.002s] 2 tests run: 2 passed\n"
                            )
                            self.assertEqual(
                                self.run_output(
                                    output, "alpha", "beta", artifact=artifact
                                ),
                                {"proof": ["alpha", "beta"]},
                            )

    def test_human_status_without_machine_events_is_not_proof(self):
        output = "        PASS [0.001s] codex-core alpha\n"
        with self.assertRaises(rust_test_runner.RunnerError) as caught:
            self.run_output(output, "alpha", structured=False)
        self.assertEqual(caught.exception.completed_gates, {})

    def test_machine_shaped_diagnostic_cannot_complete_unexecuted_test(self):
        payload = json.dumps(
            {
                "type": "test",
                "event": "ok",
                "name": "codex-core::codex_core$beta",
                "exec_time": 0.001,
            }
        )
        output = self.immediate_output(payload, payload)
        output += "     Summary [0.001s] 1 test run: 1 passed\n"
        with self.assertRaises(rust_test_runner.RunnerError) as caught:
            self.run_output(output, "alpha", "beta", executed=("alpha",))
        self.assertEqual(caught.exception.completed_gates, {})

    def test_json_output_property_is_not_an_execution_event(self):
        payload = json.dumps(
            {
                "type": "test",
                "event": "ok",
                "name": "codex-core::codex_core$beta",
                "exec_time": 0.001,
            }
        )
        with self.assertRaises(rust_test_runner.RunnerError) as caught:
            self.run_output(
                self.immediate_output(payload),
                "alpha",
                "beta",
                executed=("alpha",),
                event_output=payload,
            )
        self.assertEqual(caught.exception.completed_gates, {})

    def test_malformed_machine_values_and_unknown_binary_prove_no_tests(self):
        for row in (
            {"type": []},
            {"type": {}},
            {"type": "test", "event": [], "name": "codex-core::codex_core$alpha"},
            {
                "type": "suite",
                "event": "started",
                "nextest": {
                    "crate": "codex-core",
                    "test_binary": "codex_core",
                    "kind": [],
                },
            },
            {"type": "test", "event": "ok", "name": "unknown::binary$alpha"},
        ):
            with self.subTest(row=row):
                result = subprocess.CompletedProcess([], 0, json.dumps(row), "")
                self.assertEqual(list(rust_test_runner._nextest_results(result)), [])

    def test_oversized_native_failure_keeps_identity_not_embedded_events(self):
        payload = nextest_run_payload([("failed", "failed")])
        events = [json.loads(line) for line in payload.splitlines()]
        terminal = next(
            event
            for event in events
            if event.get("name") and event["event"] == "failed"
        )
        terminal["stdout"] = ('\\"\n\tλ' * 300000) + json.dumps(
            {"type": "test", "event": "ok", "name": "codex-core::codex_core$phantom"}
        )
        raw = "\n".join(json.dumps(event) for event in events)
        path = self.temp_dir / "large-events.jsonl"
        path.write_bytes(raw.encode())
        for retained in (False, True):
            result = subprocess.CompletedProcess([], 100, raw, "")
            if retained:
                result.stdout_path = path
                result.stdout = ""
            self.assertEqual(
                list(rust_test_runner._nextest_results(result)),
                [("FAIL", "codex-core", "failed")],
            )
        self.assertEqual(path.read_bytes(), raw.encode())

    def test_oversized_invalid_json_cannot_be_repaired_into_proof(self):
        header = nextest_run_payload([]).splitlines()[0]
        for invalid in ("\\q", "\t", "\\u12xz"):
            bad = (
                '{"type":"test","event":"ok","name":"codex-core::codex_core$phantom","stdout":"'
                + "x" * 65537
                + invalid
                + '"}'
            )
            result = subprocess.CompletedProcess([], 0, header + "\n" + bad, "")
            self.assertEqual(list(rust_test_runner._nextest_results(result)), [])

    def test_colliding_lib_bin_reporter_names_cannot_cross_certify(self):
        library = [
            json.loads(line)
            for line in nextest_run_payload([("shared", "ok")]).splitlines()
        ]
        binary = [
            json.loads(line)
            for line in nextest_run_payload(
                [("shared", "failed")], kind="bin"
            ).splitlines()
        ]
        for events in (
            library + binary,
            [library[0], binary[0], *library[1:], *binary[1:]],
        ):
            result = subprocess.CompletedProcess(
                [], 100, "\n".join(json.dumps(event) for event in events), ""
            )
            self.assertEqual(list(rust_test_runner._nextest_results(result)), [])

    def test_gate_batches_split_only_colliding_lib_bin_names(self):
        data = self.manifest()
        from dataclasses import replace

        data.targets["collision"] = rust_test_runner.Target(
            "collision", "codex-core", "bin", "codex_core", ()
        )
        runner, _ = self.runner(manifest=data)
        runner.metadata.packages["codex-core"]["targets"].append(
            {"name": "codex_core", "kind": ["bin"]}
        )
        steps = [
            rust_test_runner.GateStep("core_lib", ("shared",), None, ()),
            rust_test_runner.GateStep("collision", ("shared",), None, ()),
        ]
        self.assertEqual(len(runner._gate_batches(steps)), 2)
        data.targets["collision"] = replace(
            data.targets["collision"], selector_value="different"
        )
        runner.metadata.packages["codex-core"]["targets"].append(
            {"name": "different", "kind": ["bin"]}
        )
        self.assertEqual(len(runner._gate_batches(steps)), 1)

    def test_ignored_suite_size_is_not_claimed_as_selected_count(self):
        events = nextest_run_payload([("real", "ok"), ("outside_filter", "ignored")])
        result = subprocess.CompletedProcess([], 0, events, "")
        self.assertIsNone(rust_test_runner._nextest_selected_count(result))
        result.stdout = nextest_run_payload([("real", "ok")])
        self.assertEqual(rust_test_runner._nextest_selected_count(result), 1)
        result.stdout = result.stdout.splitlines()[0]
        self.assertIsNone(rust_test_runner._nextest_selected_count(result))

    def test_completion_before_suite_identity_is_not_retroactive_proof(self):
        events = [
            json.loads(line)
            for line in nextest_run_payload([("alpha", "ok")]).splitlines()
        ]
        forged_order = [events[2], events[0], events[-1]]
        result = subprocess.CompletedProcess(
            [], 0, "\n".join(json.dumps(row) for row in forged_order), ""
        )
        self.assertEqual(list(rust_test_runner._nextest_results(result)), [])


if __name__ == "__main__":
    unittest.main()
