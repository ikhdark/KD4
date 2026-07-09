#!/usr/bin/env python3

import asyncio
import contextlib
import io
from pathlib import Path
import sys
import unittest
from types import SimpleNamespace
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))

import mock_responses_websocket_server as server


class FakeWebSocket:
    def __init__(self, messages: list[str | bytes], *, path: str = server.PATH) -> None:
        self._messages = list(messages)
        self.request = SimpleNamespace(path=path)
        self.sent: list[str] = []
        self.close_calls: list[tuple[int, str]] = []

    async def recv(self) -> str | bytes:
        return self._messages.pop(0)

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


class MockResponsesWebSocketServerTest(unittest.TestCase):
    def test_scripted_exchange_reuses_cached_event_json(self) -> None:
        websocket = FakeWebSocket(['{"first":true}', '{"second":true}'])

        with mock.patch.object(
            server, "_dump_json", side_effect=AssertionError("event serialization")
        ):
            asyncio.run(
                server._handle_connection(
                    websocket,
                    quiet=True,
                    log_json="off",
                )
            )

        self.assertEqual(websocket.sent, list(server.SCRIPTED_RESPONSE_EVENT_JSON))
        self.assertEqual(websocket.close_calls, [(1000, "")])

    def test_default_usage_returns_fresh_payload(self) -> None:
        first = server._default_usage()
        second = server._default_usage()

        first["input_tokens"] = 123

        self.assertEqual(second["input_tokens"], 0)
        self.assertIsNot(first, second)

    def test_quiet_mode_suppresses_hot_path_logging(self) -> None:
        websocket = FakeWebSocket(['{"first":true}', '{"second":true}'])
        out = io.StringIO()

        with contextlib.redirect_stdout(out):
            asyncio.run(
                server._handle_connection(
                    websocket,
                    quiet=True,
                    log_json="off",
                )
            )

        self.assertEqual(out.getvalue(), "")

    def test_compact_request_logging_avoids_pretty_json(self) -> None:
        websocket = FakeWebSocket(['{"b":2,"a":1}', '{"second":true}'])
        out = io.StringIO()

        with contextlib.redirect_stdout(out):
            asyncio.run(
                server._handle_connection(
                    websocket,
                    quiet=False,
                    log_json="compact",
                )
            )

        logged = out.getvalue()
        self.assertIn('{"b":2,"a":1}', logged)
        self.assertNotIn('\n  "a"', logged)

    def test_oversized_request_closes_before_sending_events(self) -> None:
        websocket = FakeWebSocket(['{"payload":"too large"}', '{"second":true}'])

        asyncio.run(
            server._handle_connection(
                websocket,
                quiet=True,
                log_json="off",
                max_message_bytes=5,
            )
        )

        self.assertEqual(websocket.sent, [])
        self.assertEqual(websocket.close_calls, [(1009, "message too large")])

    def test_serve_disables_compression_caps_messages_and_can_exit(self) -> None:
        captured: dict[str, object] = {}
        fake_server = FakeServer()

        async def fake_serve(handler: object, host: str, port: int, **kwargs: object):
            captured["handler"] = handler
            captured["host"] = host
            captured["port"] = port
            captured["kwargs"] = kwargs
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
                await asyncio.sleep(0)
                handler = captured["handler"]
                await handler(FakeWebSocket(["{}", "{}"]))
                return await task

            rc = asyncio.run(run_once())

        self.assertEqual(rc, 0)
        self.assertTrue(fake_server.closed)
        self.assertEqual(captured["host"], server.HOST)
        self.assertEqual(captured["port"], 0)
        kwargs = captured["kwargs"]
        self.assertIsInstance(kwargs, dict)
        self.assertIsNone(kwargs["compression"])
        self.assertEqual(kwargs["max_size"], 123)

    def test_serve_rejects_non_positive_max_sessions(self) -> None:
        with self.assertRaisesRegex(ValueError, "max_sessions must be >= 1"):
            asyncio.run(server._serve(0, quiet=True, max_sessions=0))

    def test_parser_rejects_non_positive_max_sessions(self) -> None:
        with self.assertRaises(SystemExit):
            server._build_arg_parser().parse_args(["--max-sessions", "0"])

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
