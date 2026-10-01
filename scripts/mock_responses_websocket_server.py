#!/usr/bin/env python3

import argparse
import asyncio
import datetime as dt
import hashlib
import json
import sys
from pathlib import Path
from typing import Any


class MissingWebsocketsError(RuntimeError):
    pass


try:
    import websockets
    from websockets.exceptions import ConnectionClosed
except ModuleNotFoundError:

    class ConnectionClosed(Exception):
        pass

    class _MissingWebsockets:
        @staticmethod
        async def serve(*_args: Any, **_kwargs: Any) -> None:
            raise MissingWebsocketsError(
                "The mock Responses WebSocket server requires the 'websockets' package."
            )

    websockets = _MissingWebsockets()


HOST = "127.0.0.1"
DEFAULT_PORT = 8765
PATH = "/v1/responses"
DEFAULT_MAX_MESSAGE_BYTES = 4 * 1024 * 1024

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
wire_api = "responses"
supports_websockets = true

[profiles.localapi_ws]
model = "gpt-5.2"
model_provider = "localapi_ws"
model_reasoning_effort = "high"


start codex with `codex --profile localapi_ws`
"""


class _ConnectionAbort(Exception):
    pass


TOOL_RESULT_TYPES = frozenset((
    "function_call_output", "custom_tool_call_output", "tool_search_output",
))


def load_rollout_replay(path: Path) -> list[dict[str, Any]]:
    """Load data, never executable code. Incomplete/legacy evidence fails closed.

    One completed turn per replay connection; retries without a completed
    response cannot be reconstructed from response items and are rejected.
    """
    requests = []
    pending_results = []
    hashes = {}
    response_ids = {}
    turn_ids = set()
    for line in path.read_bytes().splitlines():
        if not line.strip():
            continue
        row = json.loads(line)
        payload = row["payload"]
        if row["type"] == "sampling_boundary":
            turn_ids.add(payload.get("turn_id"))
            requests.append({
                "attempt": payload["physical_attempt_id"],
                "outputs": [],
                "tool_results": pending_results,
            })
            pending_results = []
        elif row["type"] == "response_item":
            kind = payload.get("type")
            if kind in TOOL_RESULT_TYPES:
                pending_results.append(payload)
            elif requests and (kind in ("reasoning", "function_call", "custom_tool_call",
                                        "tool_search_call") or
                               kind == "message" and payload.get("role") == "assistant"):
                requests[-1]["outputs"].append(payload)
        elif payload.get("type") == "task_complete":
            for request in payload.get("timing", {}).get("modelRequests", []):
                hashes.update(request.get("requestSha256ByAttempt", {}))
                response_ids.update(request.get("responseIdByAttempt", {}))
    if len(turn_ids) != 1 or not requests or pending_results:
        raise ValueError("replay requires one completed turn with all tool results consumed")
    for request in requests:
        attempt = request["attempt"]
        if attempt not in hashes or attempt not in response_ids or not request["outputs"]:
            raise ValueError(f"attempt {attempt}: missing exact request hash, response ID or outputs")
        request["request_sha256"] = hashes[attempt]
        response_id = response_ids[attempt]
        request["events"] = tuple(_dump_json(event) for event in (
            _event_response_created(response_id),
            *({"type": "response.output_item.done", "item": item}
              for item in request["outputs"]),
            _event_response_completed(response_id),
        ))
    if set(hashes) != {request["attempt"] for request in requests}:
        raise ValueError("request hashes and sampling boundaries do not cover the same attempts")
    return requests


async def _handle_replay(websocket: Any, replay: list[dict[str, Any]], *,
                         quiet: bool, log_json: str) -> bool:
    known_results = {}
    try:
        for index, expected in enumerate(replay):
            while True:
                raw = []
                request = await _recv_json(websocket, f"replay-{index}", quiet=quiet,
                                           log_json=log_json, raw_messages=raw)
                if request.get("generate") is not False:
                    break
                await _send_events(websocket, (
                    _dump_json(_event_response_created("replay-warmup")),
                    _dump_json(_event_response_completed("replay-warmup")),
                ), quiet=quiet)
            actual_results = {
                (item["type"], item.get("call_id")): item for item in request.get("input", [])
                if item.get("type") in TOOL_RESULT_TYPES
            }
            known_results.update({
                (item["type"], item.get("call_id")): item for item in expected["tool_results"]
            })
            required = {(item["type"], item.get("call_id")) for item in expected["tool_results"]}
            if not required.issubset(actual_results) or any(
                known_results.get(key) != value for key, value in actual_results.items()
            ):
                raise ValueError(f"request {index}: tool result mismatch")
            actual_hash = hashlib.sha256(raw[0]).hexdigest()
            if actual_hash != expected["request_sha256"]:
                raise ValueError(f"request {index}: exact request hash mismatch "
                                 f"(expected {expected['request_sha256']}, got {actual_hash})")
            await _send_events(websocket, expected["events"], quiet=quiet)
    except _ConnectionAbort:
        return False
    except ValueError as error:
        sys.stderr.write(f"[replay] {error}\n")
        await websocket.close(code=1008, reason=str(error)[:120])
        return False
    await websocket.close()
    return True


def _utc_iso() -> str:
    return dt.datetime.now(tz=dt.timezone.utc).isoformat(timespec="milliseconds")


def _default_usage() -> dict[str, Any]:
    return dict(DEFAULT_USAGE)


def _event_response_created(response_id: str) -> dict[str, Any]:
    return {"type": "response.created", "response": {"id": response_id}}


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
    _dump_json(_event_response_completed("resp-1")),
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
    sys.stdout.flush()


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
    sys.stdout.flush()


async def _recv_json(
    websocket: Any,
    label: str,
    *,
    quiet: bool,
    log_json: str,
    raw_messages: list[bytes] | None = None,
) -> dict[str, Any]:
    msg = await websocket.recv()
    if raw_messages is not None:
        raw_messages.append(msg if isinstance(msg, bytes) else msg.encode("utf-8"))
    try:
        if isinstance(msg, bytes):
            payload = json.loads(msg.decode("utf-8"))
        else:
            payload = json.loads(msg)
    except (UnicodeDecodeError, json.JSONDecodeError):
        _log_conn("rejecting invalid JSON message", quiet=quiet)
        await websocket.close(code=1007, reason="invalid JSON")
        raise _ConnectionAbort from None
    _print_request(f"[{label}] recv", payload, quiet=quiet, log_json=log_json)
    if not isinstance(payload, dict) or payload.get("type") != "response.create":
        _log_conn("rejecting unexpected request type", quiet=quiet)
        await websocket.close(code=1008, reason="expected response.create")
        raise _ConnectionAbort
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
    replay: list[dict[str, Any]] | None = None,
) -> bool:
    path = websocket.request.path

    _log_conn(f"connected path={path}", quiet=quiet)

    path_no_qs = path.split("?", 1)[0]
    if path_no_qs != expected_path:
        _log_conn(f"rejecting unexpected path (expected {expected_path})", quiet=quiet)
        await websocket.close(code=1008, reason="unexpected websocket path")
        return False

    if replay is not None:
        return await _handle_replay(websocket, replay, quiet=quiet, log_json=log_json)

    # Request 1: provoke a function call (mirrors `codex-rs/core/tests/suite/agent_websocket.rs`).
    try:
        request = await _recv_json(
            websocket,
            "req1",
            quiet=quiet,
            log_json=log_json,
        )
        warmups = 0
        while request.get("generate") is False:
            # Prewarm completes connection setup without consuming the tool turn.
            warmups += 1
            response_id = f"warm-{warmups}"
            await _send_events(
                websocket,
                (
                    _dump_json(_event_response_created(response_id)),
                    _dump_json(_event_response_completed(response_id)),
                ),
                quiet=quiet,
            )
            request = await _recv_json(
                websocket,
                "req1",
                quiet=quiet,
                log_json=log_json,
            )
        await _send_events(websocket, REQUEST_1_EVENT_JSON, quiet=quiet)

        # Request 2: expect appended tool output; send final assistant message.
        request = await _recv_json(
            websocket,
            "req2",
            quiet=quiet,
            log_json=log_json,
        )
        items = request.get("input")
        if (
            request.get("generate") is False
            or not isinstance(items, list)
            or not any(
                isinstance(item, dict)
                and item.get("type") == "function_call_output"
                and item.get("call_id") == CALL_ID
                and isinstance(item.get("output"), (str, list))
                for item in items
            )
        ):
            _log_conn("rejecting missing or invalid tool output", quiet=quiet)
            await websocket.close(code=1008, reason=f"expected output for {CALL_ID}")
            return False
        await _send_events(websocket, REQUEST_2_EVENT_JSON, quiet=quiet)
    except _ConnectionAbort:
        return False

    _log_conn("closing", quiet=quiet)
    await websocket.close()
    return True


def _config_snippet(ws_uri: str) -> str:
    return CONFIG_SNIPPET_TEMPLATE.format(ws_uri=ws_uri)


async def _serve(
    port: int,
    *,
    quiet: bool = False,
    log_json: str = "pretty",
    max_message_bytes: int = DEFAULT_MAX_MESSAGE_BYTES,
    max_sessions: int | None = None,
    replay: list[dict[str, Any]] | None = None,
) -> int:
    if not 0 <= port <= 65535:
        raise ValueError("port must be between 0 and 65535")
    if max_sessions is not None and max_sessions < 1:
        raise ValueError("max_sessions must be >= 1")

    finished = asyncio.Event()
    sessions_seen = 0
    replay_failed = False

    async def handler(ws: Any) -> None:
        nonlocal sessions_seen, replay_failed
        completed = False
        try:
            completed = await _handle_connection(
                ws,
                expected_path=PATH,
                quiet=quiet,
                log_json=log_json,
                replay=replay,
            )
        except ConnectionClosed:
            return
        finally:
            if replay is not None and not completed:
                replay_failed = True
                finished.set()
            if completed and max_sessions is not None:
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
        sys.stderr.flush()
        return 2
    except MissingWebsocketsError as err:
        sys.stderr.write(f"[server] failed to start: {err}\n")
        sys.stderr.flush()
        return 2
    bound_port = server.sockets[0].getsockname()[1]
    ws_uri = f"ws://{HOST}:{bound_port}"

    if not quiet:
        sys.stdout.write("[server] mock Responses WebSocket server running\n")
        sys.stdout.write(_config_snippet(ws_uri))
        sys.stdout.flush()
    try:
        if max_sessions is None and replay is None:
            await asyncio.Future()
        else:
            await finished.wait()
    finally:
        server.close()
        await server.wait_closed()
    return 1 if replay_failed else 0


def _positive_int(value: str) -> int:
    parsed = int(value)
    if parsed < 1:
        raise argparse.ArgumentTypeError("must be >= 1")
    return parsed


def _port(value: str) -> int:
    parsed = int(value)
    if not 0 <= parsed <= 65535:
        raise argparse.ArgumentTypeError("must be between 0 and 65535")
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
        type=_port,
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
            "Set the websockets inbound message cap "
            f"(default: {DEFAULT_MAX_MESSAGE_BYTES})."
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
    parser.add_argument("--replay-rollout", type=Path,
                        help="Replay one completed turn from JSONL; verify exact request hashes "
                             "and tool results. Missing provenance is an error.")
    return parser


def main() -> int:
    parser = _build_arg_parser()
    args = parser.parse_args()
    if args.once and args.max_sessions is not None and args.max_sessions != 1:
        parser.error("--once cannot be combined with --max-sessions other than 1")
    max_sessions = 1 if args.once else args.max_sessions
    replay = None
    if args.replay_rollout:
        try:
            replay = load_rollout_replay(args.replay_rollout)
        except (OSError, ValueError, KeyError) as error:
            parser.error(str(error))
        if max_sessions is None:
            max_sessions = 1

    try:
        return asyncio.run(
            _serve(
                args.port,
                quiet=args.quiet,
                log_json=args.log_json,
                max_message_bytes=args.max_message_bytes,
                max_sessions=max_sessions,
                replay=replay,
            )
        )
    except KeyboardInterrupt:
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
