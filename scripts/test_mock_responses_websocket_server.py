#!/usr/bin/env python3

import asyncio
import contextlib
import io
import json
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

import tomllib

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))

import mock_responses_websocket_server as server


REQUEST_JSON = '{"type":"response.create","model":"gpt-5.2","input":[]}'
TOOL_OUTPUT_JSON = json.dumps(
    {
        "type": "response.create",
        "previous_response_id": "resp-1",
        "input": [
            {
                "type": "function_call_output",
                "call_id": "shell-command-call",
                "output": "websocket\nExit code: 0",
            }
        ],
    }
)


def run_bounded(coroutine):
    async def wait():
        return await asyncio.wait_for(coroutine, timeout=5)

    return asyncio.run(wait())


@contextlib.asynccontextmanager
async def running_server(**options):
    ready = asyncio.get_running_loop().create_future()
    serve = server.websockets.serve

    async def capture_server(*args, **kwargs):
        listening = await serve(*args, **kwargs)
        ready.set_result(listening.sockets[0].getsockname()[1])
        return listening

    with mock.patch.object(server.websockets, "serve", capture_server):
        task = asyncio.create_task(
            server._serve(0, quiet=True, max_sessions=1, **options)
        )
        try:
            port = await ready
            yield f"ws://127.0.0.1:{port}/v1/responses", task
        finally:
            if not task.done():
                task.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await task


class FakeWebSocket:
    def __init__(
        self, messages: list[str | bytes | BaseException], *, path: str = server.PATH
    ) -> None:
        self._messages = list(messages)
        self.request = SimpleNamespace(path=path)
        self.sent: list[str] = []
        self.close_calls: list[tuple[int, str]] = []

    async def recv(self) -> str | bytes:
        message = self._messages.pop(0)
        if isinstance(message, BaseException):
            raise message
        return message

    async def send(self, message: str) -> None:
        self.sent.append(message)

    async def close(self, code: int = 1000, reason: str = "") -> None:
        self.close_calls.append((code, reason))


class FakeSocket:
    def getsockname(self) -> tuple[str, int]:
        return (server.HOST, 65432)


class FakeServer:
    def __init__(self) -> None:
        self.sockets = [FakeSocket()]
        self.closed = False

    def close(self) -> None:
        self.closed = True

    async def wait_closed(self) -> None:
        return None


class FlushTrackingStringIO(io.StringIO):
    def __init__(self) -> None:
        super().__init__()
        self.flush_count = 0

    def flush(self) -> None:
        self.flush_count += 1
        super().flush()


class MockResponsesWebSocketServerTest(unittest.TestCase):
    def assert_scripted_response_events(self, events: list[dict]) -> None:
        usage = {
            "input_tokens": 0,
            "input_tokens_details": None,
            "output_tokens": 0,
            "output_tokens_details": None,
            "total_tokens": 0,
        }
        self.assertEqual(
            events,
            [
                {"type": "response.created", "response": {"id": "resp-1"}},
                {
                    "type": "response.output_item.done",
                    "item": {
                        "type": "function_call",
                        "call_id": "shell-command-call",
                        "name": "shell_command",
                        "arguments": '{"command":"echo websocket"}',
                    },
                },
                {
                    "type": "response.completed",
                    "response": {"id": "resp-1", "usage": usage},
                },
                {"type": "response.created", "response": {"id": "resp-2"}},
                {
                    "type": "response.output_item.done",
                    "item": {
                        "type": "message",
                        "role": "assistant",
                        "id": "msg-1",
                        "content": [{"type": "output_text", "text": "done"}],
                    },
                },
                {
                    "type": "response.completed",
                    "response": {"id": "resp-2", "usage": usage},
                },
            ],
        )

    def test_scripted_exchange_reuses_cached_event_json(self) -> None:
        websocket = FakeWebSocket([REQUEST_JSON, TOOL_OUTPUT_JSON])

        with mock.patch.object(
            server, "_dump_json", side_effect=AssertionError("event serialization")
        ):
            run_bounded(
                server._handle_connection(
                    websocket,
                    quiet=True,
                    log_json="off",
                )
            )

        self.assert_scripted_response_events(
            [json.loads(event) for event in websocket.sent]
        )
        self.assertEqual(websocket.close_calls, [(1000, "")])

    def test_prewarm_does_not_consume_the_scripted_exchange(self) -> None:
        warmup = '{"type":"response.create","generate":false}'
        websocket = FakeWebSocket([warmup, warmup, REQUEST_JSON, TOOL_OUTPUT_JSON])

        self.assertTrue(run_bounded(server._handle_connection(websocket, quiet=True)))

        events = [json.loads(event) for event in websocket.sent]
        self.assertEqual(
            [event["type"] for event in events[:4]],
            ["response.created", "response.completed"] * 2,
        )
        self.assertEqual(events[0]["response"]["id"], events[1]["response"]["id"])
        self.assertEqual(events[2]["response"]["id"], events[3]["response"]["id"])
        self.assertNotEqual(events[1]["response"]["id"], events[3]["response"]["id"])
        self.assert_scripted_response_events(events[4:])

    def test_invalid_request_envelope_is_rejected_without_output(self) -> None:
        for request in ("null", "[]", "{}", '{"type":"other"}'):
            with self.subTest(request=request):
                websocket = FakeWebSocket([request])
                self.assertFalse(
                    run_bounded(server._handle_connection(websocket, quiet=True))
                )
                self.assertEqual(websocket.sent, [])
                self.assertEqual(
                    websocket.close_calls, [(1008, "expected response.create")]
                )

    def test_invalid_tool_continuation_cannot_report_success(self) -> None:
        valid = json.loads(TOOL_OUTPUT_JSON)
        output = valid["input"][0]
        for continuation in (
            {},
            {"type": "response.create"},
            {"type": "response.create", "input": None},
            {"type": "response.create", "input": []},
            {"type": "response.create", "input": [None]},
            {**valid, "input": [{**output, "call_id": "wrong"}]},
            {
                **valid,
                "input": [
                    {"type": "function_call_output", "call_id": "shell-command-call"}
                ],
            },
            {**valid, "input": [{**output, "output": None}]},
            {**valid, "generate": False},
        ):
            with self.subTest(continuation=continuation):
                websocket = FakeWebSocket([REQUEST_JSON, json.dumps(continuation)])
                self.assertFalse(
                    run_bounded(server._handle_connection(websocket, quiet=True))
                )
                self.assertEqual(len(websocket.sent), 3)
                self.assertEqual(websocket.close_calls[0][0], 1008)
                self.assertFalse(any('"resp-2"' in event for event in websocket.sent))

    @unittest.skipUnless(
        hasattr(server.websockets, "connect"), "websockets is not installed"
    )
    def test_real_exchange_with_prewarm_and_large_input(self) -> None:
        async def run_exchange() -> None:
            async with running_server() as (uri, task):
                async with server.websockets.connect(
                    uri, proxy=None, compression=None
                ) as websocket:
                    requests = [
                        {"type": "response.create", "generate": False},
                        {
                            "type": "response.create",
                            "model": "gpt-5.2",
                            "input": [{"role": "user", "content": "x" * (300 * 1024)}],
                        },
                        {
                            "type": "response.create",
                            "input": [
                                {
                                    "type": "function_call_output",
                                    "call_id": "shell-command-call",
                                    "output": [
                                        {"type": "input_text", "text": "websocket"}
                                    ],
                                }
                            ],
                        },
                    ]
                    responses = []
                    for request in requests:
                        await websocket.send(json.dumps(request))
                        events = []
                        while True:
                            event = json.loads(await websocket.recv())
                            events.append(event)
                            if event["type"] == "response.completed":
                                break
                        responses.append(events)
                    self.assertEqual(
                        [event["type"] for event in responses[0]],
                        ["response.created", "response.completed"],
                    )
                    self.assert_scripted_response_events(responses[1] + responses[2])
                self.assertEqual(await task, 0)

        run_bounded(run_exchange())

    @unittest.skipUnless(
        hasattr(server.websockets, "connect"), "websockets is not installed"
    )
    def test_real_default_and_overridden_limits_reject_oversized_messages(self) -> None:
        async def run_limits() -> None:
            for options, size in (
                ({}, 4 * 1024 * 1024),
                ({"max_message_bytes": 128}, 129),
            ):
                with self.subTest(options=options):
                    async with running_server(**options) as (uri, task):
                        async with server.websockets.connect(
                            uri, proxy=None, compression=None
                        ) as websocket:
                            await websocket.send(
                                json.dumps(
                                    {"type": "response.create", "input": "x" * size}
                                )
                            )
                            with self.assertRaises(server.ConnectionClosed) as caught:
                                await websocket.recv()
                            self.assertEqual(caught.exception.rcvd.code, 1009)
                        self.assertFalse(task.done())

        run_bounded(run_limits())

    def test_printed_config_enables_current_websocket_transport(self) -> None:
        snippet = server._config_snippet("ws://127.0.0.1:54321")
        config = tomllib.loads(
            snippet.partition("Add this to your config.toml:")[2].partition(
                "start codex"
            )[0]
        )
        profile = config["profiles"]["localapi_ws"]
        provider = config["model_providers"][profile["model_provider"]]
        schema_path = (
            Path(__file__).resolve().parents[1] / "codex-rs/core/config.schema.json"
        )
        schema = json.loads(schema_path.read_text(encoding="utf-8"))
        wire_api_variants = schema["definitions"]["WireApi"]["oneOf"]
        self.assertTrue(
            any(
                provider["wire_api"] in variant.get("enum", [])
                for variant in wire_api_variants
            )
        )
        self.assertIs(provider["supports_websockets"], True)
        self.assertEqual(provider["base_url"], "ws://127.0.0.1:54321/v1")
        self.assertNotIn("env_key", provider)
        self.assertIn("codex --profile localapi_ws", snippet)

    def test_default_usage_returns_fresh_payload(self) -> None:
        first = server._default_usage()
        second = server._default_usage()

        first["input_tokens"] = 123

        self.assertEqual(second["input_tokens"], 0)
        self.assertIsNot(first, second)

    def test_quiet_mode_suppresses_hot_path_logging(self) -> None:
        websocket = FakeWebSocket([REQUEST_JSON, TOOL_OUTPUT_JSON])
        out = io.StringIO()

        with contextlib.redirect_stdout(out):
            run_bounded(
                server._handle_connection(
                    websocket,
                    quiet=True,
                    log_json="off",
                )
            )

        self.assertEqual(out.getvalue(), "")

    def test_compact_request_logging_avoids_pretty_json(self) -> None:
        websocket = FakeWebSocket(
            ['{"type":"response.create","b":2,"a":1}', TOOL_OUTPUT_JSON]
        )
        out = io.StringIO()

        with contextlib.redirect_stdout(out):
            run_bounded(
                server._handle_connection(
                    websocket,
                    quiet=False,
                    log_json="compact",
                )
            )

        logged = out.getvalue()
        self.assertIn('{"type":"response.create","b":2,"a":1}', logged)
        self.assertNotIn('\n  "a"', logged)

    def test_connection_and_request_logs_are_flushed(self) -> None:
        out = FlushTrackingStringIO()

        with contextlib.redirect_stdout(out):
            server._log_conn("connected", quiet=False)
            server._print_request("[req] recv", {"ok": True})

        self.assertEqual(out.flush_count, 2)

    def test_connection_requires_current_request_path_api(self) -> None:
        websocket = FakeWebSocket(["{}", "{}"], path="/unexpected")
        websocket.path = websocket.request.path
        del websocket.request

        with self.assertRaises(AttributeError):
            run_bounded(
                server._handle_connection(websocket, quiet=True, log_json="off")
            )

    def test_invalid_json_closes_with_invalid_payload_code(self) -> None:
        websocket = FakeWebSocket([b"\xff"])

        run_bounded(
            server._handle_connection(
                websocket,
                quiet=True,
                log_json="off",
            )
        )

        self.assertEqual(websocket.sent, [])
        self.assertEqual(websocket.close_calls, [(1007, "invalid JSON")])

    def test_serve_disables_compression_caps_messages_and_can_exit(self) -> None:
        captured: dict[str, object] = {}
        fake_server = FakeServer()
        ready = asyncio.Event()

        async def fake_serve(handler: object, host: str, port: int, **kwargs: object):
            captured["handler"] = handler
            captured["host"] = host
            captured["port"] = port
            captured["kwargs"] = kwargs
            ready.set()
            return fake_server

        with (
            mock.patch.object(server.websockets, "serve", side_effect=fake_serve),
            contextlib.redirect_stdout(io.StringIO()),
        ):

            async def run_once() -> int:
                task = asyncio.create_task(
                    server._serve(
                        0,
                        quiet=True,
                        max_sessions=1,
                        max_message_bytes=123,
                    )
                )
                await ready.wait()
                handler = captured["handler"]
                await handler(FakeWebSocket([REQUEST_JSON, TOOL_OUTPUT_JSON]))
                return await task

            rc = run_bounded(run_once())

        self.assertEqual(rc, 0)
        self.assertTrue(fake_server.closed)
        self.assertEqual(captured["host"], server.HOST)
        self.assertEqual(captured["port"], 0)
        kwargs = captured["kwargs"]
        self.assertIsInstance(kwargs, dict)
        self.assertIsNone(kwargs["compression"])
        self.assertEqual(kwargs["max_size"], 123)

    def test_serve_ignores_aborted_connections_for_session_limit(self) -> None:
        captured: dict[str, object] = {}
        fake_server = FakeServer()
        ready = asyncio.Event()

        async def fake_serve(handler: object, host: str, port: int, **kwargs: object):
            captured["handler"] = handler
            ready.set()
            return fake_server

        with (
            mock.patch.object(server.websockets, "serve", side_effect=fake_serve),
            contextlib.redirect_stdout(io.StringIO()),
        ):

            async def run_connections() -> int:
                task = asyncio.create_task(server._serve(0, quiet=True, max_sessions=1))
                await ready.wait()
                handler = captured["handler"]
                await handler(FakeWebSocket([server.ConnectionClosed(None, None)]))
                await handler(FakeWebSocket(["{}", "{}"], path="/health"))
                await handler(FakeWebSocket([REQUEST_JSON, "{}"]))
                await handler(
                    FakeWebSocket(
                        [
                            '{"type":"response.create","generate":false}',
                            server.ConnectionClosed(None, None),
                        ]
                    )
                )
                self.assertFalse(task.done())
                await handler(FakeWebSocket([REQUEST_JSON, TOOL_OUTPUT_JSON]))
                return await task

            rc = run_bounded(run_connections())

        self.assertEqual(rc, 0)
        self.assertTrue(fake_server.closed)

    def test_missing_websockets_dependency_is_reported_cleanly(self) -> None:
        stderr = io.StringIO()

        with (
            mock.patch.object(
                server.websockets,
                "serve",
                side_effect=server.MissingWebsocketsError("dependency missing"),
            ),
            contextlib.redirect_stderr(stderr),
        ):
            rc = run_bounded(server._serve(0, quiet=True, max_sessions=1))

        self.assertEqual(rc, 2)
        self.assertIn("dependency missing", stderr.getvalue())

    def test_serve_rejects_non_positive_max_sessions(self) -> None:
        with self.assertRaisesRegex(ValueError, "max_sessions must be >= 1"):
            run_bounded(server._serve(0, quiet=True, max_sessions=0))

    def test_parser_rejects_non_positive_max_sessions(self) -> None:
        with self.assertRaises(SystemExit):
            server._build_arg_parser().parse_args(["--max-sessions", "0"])

    def test_parser_and_serve_reject_invalid_ports(self) -> None:
        for value in ("-1", "65536"):
            with self.subTest(value=value), self.assertRaises(SystemExit):
                server._build_arg_parser().parse_args(["--port", value])
        with self.assertRaisesRegex(ValueError, "port must be between"):
            run_bounded(server._serve(65536, quiet=True, max_sessions=1))

    def test_parser_accepts_performance_flags(self) -> None:
        args = server._build_arg_parser().parse_args(
            [
                "--port",
                "0",
                "--quiet",
                "--log-json",
                "compact",
                "--max-message-bytes",
                "123",
                "--once",
            ]
        )

        self.assertEqual(args.port, 0)
        self.assertTrue(args.quiet)
        self.assertEqual(args.log_json, "compact")
        self.assertEqual(args.max_message_bytes, 123)
        self.assertTrue(args.once)


if __name__ == "__main__":
    unittest.main()
