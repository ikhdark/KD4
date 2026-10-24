from __future__ import annotations

import io
import json
import subprocess
from unittest.mock import Mock

import pytest

from openai_codex.client import CodexClient, CodexConfig
from openai_codex.errors import CodexError, TransportClosedError
from openai_codex.generated.v2_all import (
    AgentMessageDeltaNotification,
    TurnCompletedNotification,
)


@pytest.fixture
def pipe_client():
    """Replace only the process and pipe; keep decoding, routing, and budgets real."""
    pipes = []

    def make(wire: str, **limits):
        client = CodexClient(CodexConfig(**limits))
        proc = Mock(spec=subprocess.Popen)
        proc.stdout = io.StringIO(wire)
        client._proc = proc
        pipes.append(proc.stdout)
        return client, proc

    yield make
    for pipe in pipes:
        pipe.close()


@pytest.mark.parametrize(
    ("frame", "message"),
    [
        ("not JSON\n", "Invalid JSON-RPC line"),
        (
            '{"method":"item/agentMessage/delta","params":{"delta":"missing IDs"}}\n',
            "Invalid payload for known notification",
        ),
    ],
    ids=["invalid-json", "invalid-known-notification"],
)
def test_reader_failure_wakes_pending_rpc_and_discards_unfinished_events(
    pipe_client, frame, message
) -> None:
    early = {"method": "future/update", "params": {"turnId": "pending-turn", "text": "partial"}}
    client, proc = pipe_client(json.dumps(early) + "\n" + frame)
    router = client._router
    waiter = router.create_response_waiter("pending-rpc")
    client.register_turn_notifications("pending-turn")

    client._reader_loop(proc, router)

    failure = waiter.get_nowait()
    assert isinstance(failure, CodexError)
    assert message in str(failure)
    assert failure.__cause__ is not None
    with pytest.raises(CodexError) as stream_failure:
        client.next_turn_notification("pending-turn", 0)
    assert stream_failure.value is failure
    assert router._budget.items == router._budget.bytes == 0
    proc.terminate.assert_called_once_with()
    assert proc.stdout.closed


def test_reader_retains_typed_completed_stream_for_late_registration_at_eof(pipe_client) -> None:
    thread_id, turn_id = "thread", "turn"
    frames = [
        {
            "method": "item/agentMessage/delta",
            "params": {"threadId": thread_id, "turnId": turn_id, "itemId": "message", "delta": "hi"},
        },
        {
            "method": "turn/completed",
            "params": {
                "threadId": thread_id,
                "turn": {"id": turn_id, "items": [], "status": "completed"},
            },
        },
    ]
    client, proc = pipe_client("".join(json.dumps(frame) + "\n" for frame in frames))

    client._reader_loop(proc, client._router)
    client.register_turn_notifications(turn_id)

    delta = client.next_turn_notification(turn_id, 0)
    terminal = client.next_turn_notification(turn_id, 0)
    assert isinstance(delta.payload, AgentMessageDeltaNotification)
    assert delta.payload.delta == "hi"
    assert isinstance(terminal.payload, TurnCompletedNotification)
    assert terminal.payload.turn.id == turn_id
    assert client._router._budget.items == client._router._budget.bytes == 0
    with pytest.raises(TransportClosedError, match="closed stdout"):
        client.next_turn_notification(turn_id, 0)
    proc.terminate.assert_called_once_with()
    assert proc.stdout.closed


def test_message_limit_counts_utf8_bytes_including_the_newline(pipe_client) -> None:
    payload = {"id": "result", "result": {"text": "\U0001f642"}}
    wire = json.dumps(payload, ensure_ascii=False) + "\n"
    byte_count = len(wire.encode("utf-8"))
    assert byte_count > len(wire)

    exact, exact_proc = pipe_client(wire, max_message_bytes=byte_count)
    assert exact._read_message(exact_proc) == payload

    exceeded, exceeded_proc = pipe_client(wire, max_message_bytes=byte_count - 1)
    with pytest.raises(CodexError, match="inbound message size limit exceeded"):
        exceeded._read_message(exceeded_proc)
