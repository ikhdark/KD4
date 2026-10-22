from __future__ import annotations

import asyncio
from unittest.mock import AsyncMock, Mock

import pytest

from openai_codex.api import AsyncCodex, AsyncTurnHandle
from openai_codex.async_client import AsyncCodexClient
from openai_codex.client import CodexClient
from openai_codex.errors import TransportClosedError
from openai_codex.generated.v2_all import (
    AgentMessageDeltaNotification,
    Turn,
    TurnCompletedNotification,
    TurnStartResponse,
)
from openai_codex.models import Notification


@pytest.mark.parametrize("asynchronous", [False, True])
@pytest.mark.parametrize(
    "status,message",
    [
        ("completed", None),
        ("interrupted", None),
        ("failed", "provider failed"),
        ("failed", None),
        ("failed", ""),
    ],
)
def test_text_stream_terminal_status_and_route_cleanup(monkeypatch, asynchronous, status, message):
    client = AsyncCodexClient() if asynchronous else CodexClient()
    turn = Turn.model_validate(
        {
            "id": "turn",
            "items": [],
            "status": status,
            "error": {"message": message} if message is not None else None,
        }
    )
    delta = AgentMessageDeltaNotification(
        delta="partial", item_id="item", thread_id="thread", turn_id="turn"
    )
    events = iter(
        [
            Notification(
                "turn/completed",
                TurnCompletedNotification(
                    thread_id="thread", turn=Turn(id="other", items=[], status="completed")
                ),
            ),
            Notification("item/agentMessage/delta", delta),
            Notification(
                "turn/completed", TurnCompletedNotification(thread_id="thread", turn=turn)
            ),
        ]
    )
    started = TurnStartResponse(turn=Turn(id="turn", items=[], status="inProgress"))
    unregister = Mock()
    monkeypatch.setattr(client, "unregister_turn_notifications", unregister)
    monkeypatch.setattr(client, "register_turn_notifications", Mock())
    chunks = []
    if asynchronous:
        monkeypatch.setattr(client, "turn_start", AsyncMock(return_value=started))

        async def receive(_):
            return next(events)

        monkeypatch.setattr(client, "next_turn_notification", receive)

        async def consume():
            async for item in client.stream_text("thread", "input"):
                chunks.append(item.delta)

        def consume_stream():
            return asyncio.run(consume())
    else:
        monkeypatch.setattr(client, "turn_start", Mock(return_value=started))
        monkeypatch.setattr(client, "next_turn_notification", lambda _: next(events))

        def consume_stream():
            chunks.extend(item.delta for item in client.stream_text("thread", "input"))

    if status == "failed":
        with pytest.raises(RuntimeError, match=message or "turn failed"):
            consume_stream()
    else:
        consume_stream()
    assert chunks == ["partial"]
    unregister.assert_called_once_with("turn")


def test_stale_async_handles_do_not_initialize_or_register(monkeypatch):
    async def scenario():
        owner = AsyncCodex()
        owner._initialized = True
        handle = AsyncTurnHandle(owner, "thread", "turn")
        initialize = AsyncMock(side_effect=AssertionError("must not restart"))
        monkeypatch.setattr(owner, "_ensure_initialized", initialize)
        monkeypatch.setattr(owner._client, "close", AsyncMock())
        await owner.close()
        for operation in (
            lambda: handle.steer("more"),
            handle.interrupt,
            lambda: anext(handle.stream()),
        ):
            with pytest.raises(TransportClosedError):
                await operation()
        initialize.assert_not_called()
        assert not handle._router._turn_notifications
        await handle.close()

    asyncio.run(scenario())


def test_async_handle_cleanup_owns_original_router():
    async def scenario():
        owner = AsyncCodex()
        owner._initialized = True
        handle = AsyncTurnHandle(owner, "thread", "turn")
        old_router = handle._router
        old_router.register_turn("turn")
        owner._client._sync._router = owner._client._sync._new_router()
        owner._client._sync._process_epoch += 1
        owner._client._sync._router.register_turn("turn")
        await handle.close()
        assert "turn" not in old_router._turn_notifications
        assert "turn" in owner._client._sync._router._turn_notifications

    asyncio.run(scenario())


def test_closed_async_handle_does_not_claim_process_closed(monkeypatch):
    async def scenario():
        owner = AsyncCodex()
        owner._initialized = True
        handle = AsyncTurnHandle(owner, "thread", "turn")
        rpc = AsyncMock(side_effect=AssertionError("closed handle must not send RPC"))
        monkeypatch.setattr(owner._client, "_run_sync", rpc)
        await handle.close()
        await handle.close()
        assert owner._initialized
        for operation in (
            lambda: handle.steer("more"),
            handle.interrupt,
            lambda: anext(handle.stream()),
        ):
            with pytest.raises(TransportClosedError, match="^turn handle is closed$"):
                await operation()
        rpc.assert_not_called()

    asyncio.run(scenario())


@pytest.mark.parametrize("failed", [False, True])
def test_async_handle_consumes_retained_terminal_after_transport_eof(failed):
    async def scenario():
        owner = AsyncCodex()
        owner._initialized = True
        handle = AsyncTurnHandle(owner, "thread", "turn")
        turn = Turn.model_validate(
            {
                "id": "turn",
                "items": [],
                "status": "failed" if failed else "completed",
                "error": {"message": "provider failure"} if failed else None,
                "surfacedResult": {"adapter": "code_mode_cell", "value": {"answer": 42}},
            }
        )
        handle._router.route_notification(
            Notification("turn/completed", TurnCompletedNotification(thread_id="thread", turn=turn))
        )
        handle._router.fail_all(TransportClosedError("EOF after terminal"))
        if failed:
            with pytest.raises(RuntimeError, match="provider failure"):
                await handle.run()
        else:
            result = await handle.run()
            assert result.surfaced_result == turn.surfaced_result
            assert result.final_response is None
        assert not handle._router._turn_notifications
        assert not handle._router._pending_turn_notifications
        assert handle._router._budget.items == 0

    asyncio.run(scenario())
