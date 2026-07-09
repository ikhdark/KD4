#!/usr/bin/env python3

import argparse
import asyncio
import datetime as dt
import json
import sys
from typing import Any

try:
    import websockets
except ModuleNotFoundError:
    class _MissingWebsockets:
        @staticmethod
        async def serve(*_args: Any, **_kwargs: Any) -> None:
            raise RuntimeError(
                "The mock Responses WebSocket server requires the 'websockets' package."
            )

    websockets = _MissingWebsockets()


HOST = "127.0.0.1"
DEFAULT_PORT = 8765
PATH = "/v1/responses"
DEFAULT_MAX_MESSAGE_BYTES = 256 * 1024

CALL_ID = "shell-command-call"
FUNCTION_NAME = "shell_command"
FUNCTION_ARGS_JSON = json.dumps({"command": "echo websocket"}, separators=(",", ":"))

ASSISTANT_TEXT = "done"
LOG_JSON_CHOICES = ("pretty", "compact", "off")

DEFAULT_USAGE: dict[str, Any] = {
    "input_tokens": 0,
    "input_tokens_details": None,
    "output_tokens": 0,
    "output_tokens_details": None,
    "total_tokens": 0,
}

CONFIG_SNIPPET_TEMPLATE = """Add this to your config.toml:


[model_providers.localapi_ws]
base_url = "{ws_uri}/v1"
name = "localapi_ws"
wire_api = "responses_websocket"
env_key = "OPENAI_API_KEY_STAGING"

[profiles.localapi_ws]
model = "gpt-5.2"
model_provider = "localapi_ws"
model_reasoning_effort = "high"


start codex with `codex --profile localapi_ws`
"""


class _ConnectionAbort(Exception):
    pass


def _utc_iso() -> str:
    return dt.datetime.now(tz=dt.timezone.utc).isoformat(timespec="milliseconds")


def _default_usage() -> dict[str, Any]:
    return dict(DEFAULT_USAGE)


def _event_response_created(response_id: str) -> dict[str, Any]:
    return {"type": "response.created", "response": {"id": response_id}}


def _event_response_done() -> dict[str, Any]:
    return {"type": "response.done", "response": {"usage": _default_usage()}}


def _event_response_completed(response_id: str) -> dict[str, Any]:
    return {
        "type": "response.completed",
        "response": {"id": response_id, "usage": _default_usage()},
    }


def _event_function_call(
    call_id: str, name: str, arguments_json: str
) -> dict[str, Any]:
    return {
        "type": "response.output_item.done",
        "item": {
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": arguments_json,
        },
    }


def _event_assistant_message(message_id: str, text: str) -> dict[str, Any]:
    return {
        "type": "response.output_item.done",
        "item": {
            "type": "message",
            "role": "assistant",
            "id": message_id,
            "content": [{"type": "output_text", "text": text}],
        },
    }


def _dump_json(payload: Any) -> str:
    return json.dumps(payload, ensure_ascii=False, separators=(",", ":"))


REQUEST_1_EVENT_JSON = (
    _dump_json(_event_response_created("resp-1")),
    _dump_json(_event_function_call(CALL_ID, FUNCTION_NAME, FUNCTION_ARGS_JSON)),
    _dump_json(_event_response_done()),
)

REQUEST_2_EVENT_JSON = (
    _dump_json(_event_response_created("resp-2")),
    _dump_json(_event_assistant_message("msg-1", ASSISTANT_TEXT)),
    _dump_json(_event_response_completed("resp-2")),
)

SCRIPTED_RESPONSE_EVENT_JSON = REQUEST_1_EVENT_JSON + REQUEST_2_EVENT_JSON


def _log_conn(message: str, *, quiet: bool) -> None:
    if quiet:
        return
    sys.stdout.write(f"[conn] {_utc_iso()} {message}\n")


def _print_request(
    prefix: str,
    payload: Any,
    *,
    quiet: bool = False,
    log_json: str = "pretty",
) -> None:
    if quiet or log_json == "off":
        return
    if log_json == "compact":
        body = _dump_json(payload)
    else:
        body = json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True)
    sys.stdout.write(f"{prefix} {_utc_iso()}\n{body}\n")


def _message_size(message: str | bytes) -> int:
    if isinstance(message, bytes):
        return len(message)
    return len(message.encode("utf-8"))


async def _recv_json(
    websocket: Any,
    label: str,
    *,
    quiet: bool,
    log_json: str,
    max_message_bytes: int,
) -> Any:
    msg = await websocket.recv()
    if _message_size(msg) > max_message_bytes:
        _log_conn(
            f"rejecting oversized message ({_message_size(msg)} > {max_message_bytes})",
            quiet=quiet,
        )
        await websocket.close(code=1009, reason="message too large")
        raise _ConnectionAbort
    if isinstance(msg, bytes):
        payload = json.loads(msg.decode("utf-8"))
    else:
        payload = json.loads(msg)
    _print_request(f"[{label}] recv", payload, quiet=quiet, log_json=log_json)
    return payload


async def _send_event_json(
    websocket: Any,
    event_json: str,
    *,
    quiet: bool,
) -> None:
    _log_conn(f"send {event_json}", quiet=quiet)
    await websocket.send(event_json)


async def _send_events(
    websocket: Any,
    events: tuple[str, ...],
    *,
    quiet: bool,
) -> None:
    for event_json in events:
        await _send_event_json(websocket, event_json, quiet=quiet)


async def _handle_connection(
    websocket: Any,
    *,
    expected_path: str = PATH,
    quiet: bool = False,
    log_json: str = "pretty",
    max_message_bytes: int = DEFAULT_MAX_MESSAGE_BYTES,
) -> None:
    # websockets v15 exposes the request path here.
    path = getattr(getattr(websocket, "request", None), "path", None)
    if path is None:
        # Older handler signatures could pass `path` separately; accept if unavailable.
        path = "(unknown)"

    _log_conn(f"connected path={path}", quiet=quiet)

    path_no_qs = path.split("?", 1)[0] if path != "(unknown)" else path
    if path_no_qs != "(unknown)" and path_no_qs != expected_path:
        _log_conn(f"rejecting unexpected path (expected {expected_path})", quiet=quiet)
        await websocket.close(code=1008, reason="unexpected websocket path")
        return

    # Request 1: provoke a function call (mirrors `codex-rs/core/tests/suite/agent_websocket.rs`).
    try:
        await _recv_json(
            websocket,
            "req1",
            quiet=quiet,
            log_json=log_json,
            max_message_bytes=max_message_bytes,
        )
        await _send_events(websocket, REQUEST_1_EVENT_JSON, quiet=quiet)

        # Request 2: expect appended tool output; send final assistant message.
        await _recv_json(
            websocket,
            "req2",
            quiet=quiet,
            log_json=log_json,
            max_message_bytes=max_message_bytes,
        )
        await _send_events(websocket, REQUEST_2_EVENT_JSON, quiet=quiet)
    except _ConnectionAbort:
        return

    _log_conn("closing", quiet=quiet)
    await websocket.close()


def _config_snippet(ws_uri: str) -> str:
    return CONFIG_SNIPPET_TEMPLATE.format(ws_uri=ws_uri)


async def _serve(
    port: int,
    *,
    quiet: bool = False,
    log_json: str = "pretty",
    max_message_bytes: int = DEFAULT_MAX_MESSAGE_BYTES,
    max_sessions: int | None = None,
) -> int:
    if max_sessions is not None and max_sessions < 1:
        raise ValueError("max_sessions must be >= 1")

    finished = asyncio.Event()
    sessions_seen = 0

    async def handler(ws: Any) -> None:
        nonlocal sessions_seen
        try:
            await _handle_connection(
                ws,
                expected_path=PATH,
                quiet=quiet,
                log_json=log_json,
                max_message_bytes=max_message_bytes,
            )
        except websockets.exceptions.ConnectionClosedOK:
            return
        finally:
            if max_sessions is not None:
                sessions_seen += 1
                if sessions_seen >= max_sessions:
                    finished.set()

    try:
        server = await websockets.serve(
            handler,
            HOST,
            port,
            compression=None,
            max_size=max_message_bytes,
        )
    except OSError as err:
        sys.stderr.write(f"[server] failed to bind ws://{HOST}:{port}: {err}\n")
        return 2
    bound_port = server.sockets[0].getsockname()[1]
    ws_uri = f"ws://{HOST}:{bound_port}"

    if not quiet:
        sys.stdout.write("[server] mock Responses WebSocket server running\n")
        sys.stdout.write(_config_snippet(ws_uri))
        sys.stdout.flush()
    try:
        if max_sessions is None:
            await asyncio.Future()
        else:
            await finished.wait()
    finally:
        server.close()
        await server.wait_closed()
    return 0


def _positive_int(value: str) -> int:
    parsed = int(value)
    if parsed < 1:
        raise argparse.ArgumentTypeError("must be >= 1")
    return parsed


def _build_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Mock a minimal Responses API WebSocket endpoint for the `test_codex` flow.\n"
            f"Binds to {HOST}:{DEFAULT_PORT} by default and logs incoming JSON requests to stdout."
        ),
        formatter_class=argparse.RawTextHelpFormatter,
    )
    parser.add_argument(
        "--port",
        type=int,
        default=DEFAULT_PORT,
        help=f"Bind port (default: {DEFAULT_PORT}; use 0 for random free port).",
    )
    parser.add_argument(
        "--quiet",
        action="store_true",
        help="Suppress startup, request, response, and connection logs.",
    )
    parser.add_argument(
        "--log-json",
        choices=LOG_JSON_CHOICES,
        default="pretty",
        help="Request JSON logging format (default: pretty).",
    )
    parser.add_argument(
        "--max-message-bytes",
        type=_positive_int,
        default=DEFAULT_MAX_MESSAGE_BYTES,
        help=(
            "Reject inbound messages above this size and pass the same cap to "
            f"websockets (default: {DEFAULT_MAX_MESSAGE_BYTES})."
        ),
    )
    parser.add_argument(
        "--max-sessions",
        type=_positive_int,
        default=None,
        help="Exit after serving this many websocket sessions.",
    )
    parser.add_argument(
        "--once",
        action="store_true",
        help="Exit after one websocket session.",
    )
    return parser


def main() -> int:
    parser = _build_arg_parser()
    args = parser.parse_args()
    if args.once and args.max_sessions is not None and args.max_sessions != 1:
        parser.error("--once cannot be combined with --max-sessions other than 1")
    max_sessions = 1 if args.once else args.max_sessions

    try:
        return asyncio.run(
            _serve(
                args.port,
                quiet=args.quiet,
                log_json=args.log_json,
                max_message_bytes=args.max_message_bytes,
                max_sessions=max_sessions,
            )
        )
    except KeyboardInterrupt:
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
