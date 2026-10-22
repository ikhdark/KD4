from __future__ import annotations

import asyncio

import pytest

from openai_codex import TurnRunError
from openai_codex._run import _collect_async_turn_result, _collect_turn_result
from openai_codex.generated.v2_all import (
    ItemCompletedNotification,
    ThreadItem,
    ThreadTokenUsage,
    ThreadTokenUsageUpdatedNotification,
    Turn,
    TurnCompletedNotification,
)
from openai_codex.models import Notification


@pytest.mark.parametrize("asynchronous", [False, True])
@pytest.mark.parametrize("ending", ["failed", "completed", "interrupted", "missing", "transport"])
def test_buffered_run_preserves_prior_receipts(asynchronous, ending):
    item = ThreadItem.model_validate(
        {
            "type": "commandExecution",
            "id": "committed-command",
            "command": "save",
            "commandActions": [],
            "cwd": "/repo",
            "status": "completed",
            "exitCode": 0,
            "aggregatedOutput": "receipt-123",
            "executionId": "dispatch-123",
        }
    )
    usage = ThreadTokenUsage.model_validate(
        {
            name: {
                "cachedInputTokens": 0,
                "inputTokens": 2,
                "outputTokens": 3,
                "reasoningOutputTokens": 0,
                "totalTokens": 5,
            }
            for name in ("last", "total")
        }
    )
    events = [
        Notification(
            "item/completed",
            ItemCompletedNotification(
                thread_id="thread", turn_id="turn", item=item, completed_at_ms=1
            ),
        ),
        Notification(
            "thread/tokenUsage/updated",
            ThreadTokenUsageUpdatedNotification(
                thread_id="thread", turn_id="turn", token_usage=usage
            ),
        ),
    ]
    terminal = Turn.model_validate(
        {
            "id": "turn",
            "items": [],
            "status": ending if ending in ("failed", "completed", "interrupted") else "inProgress",
            "error": {
                "message": "provider failed",
                "codexErrorInfo": "internalServerError",
                "additionalDetails": "after completed tools",
            }
            if ending == "failed"
            else None,
        }
    )
    if ending in ("failed", "completed", "interrupted"):
        events.append(
            Notification(
                "turn/completed", TurnCompletedNotification(thread_id="thread", turn=terminal)
            )
        )
    transport_error = OSError("transport closed")

    def stream():
        yield from events
        if ending == "transport":
            raise transport_error

    async def async_stream():
        for event in stream():
            yield event

    def collect():
        if asynchronous:
            return asyncio.run(_collect_async_turn_result(async_stream(), turn_id="turn"))
        return _collect_turn_result(stream(), turn_id="turn")

    if ending in ("completed", "interrupted"):
        result = collect()
        assert result.items[0] is item
        assert result.usage is usage
        assert result.status.value == ending
    else:
        with pytest.raises(TurnRunError) as caught:
            collect()
        error = caught.value
        assert isinstance(error, RuntimeError)
        assert error.turn_id == "turn"
        assert error.thread_id == "thread"
        assert error.items[0] is item
        assert error.usage is usage
        if ending == "failed":
            assert error.terminal is terminal
            assert error.error is terminal.error
            assert error.result.items is error.items
            assert str(error) == "provider failed"
        else:
            assert error.terminal is None
            assert error.result is None
        if ending == "transport":
            assert error.__cause__ is transport_error


def test_async_collector_does_not_swallow_cancellation():
    async def stream():
        raise asyncio.CancelledError
        yield  # pragma: no cover

    with pytest.raises(asyncio.CancelledError):
        asyncio.run(_collect_async_turn_result(stream(), turn_id="turn"))
