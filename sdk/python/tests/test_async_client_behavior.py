from __future__ import annotations

import asyncio
import threading
import time

from openai_codex.async_client import AsyncCodexClient
from openai_codex.generated.v2_all import (
    TurnCompletedNotification,
)
from openai_codex.models import Notification, UnknownNotification


def test_async_client_allows_concurrent_transport_calls() -> None:
    """Async wrappers should offload sync calls so concurrent awaits can overlap."""

    async def scenario() -> int:
        """Run two blocking sync calls and report peak overlap."""
        client = AsyncCodexClient()
        active = 0
        max_active = 0

        def fake_model_list(include_hidden: bool = False) -> bool:
            """Simulate a blocking sync transport call."""
            nonlocal active, max_active
            active += 1
            max_active = max(max_active, active)
            time.sleep(0.05)
            active -= 1
            return include_hidden

        client._sync.model_list = fake_model_list  # type: ignore[method-assign]
        await asyncio.gather(client.model_list(), client.model_list())
        return max_active

    assert asyncio.run(scenario()) == 2


def test_async_client_turn_notification_methods_delegate_to_sync_client() -> None:
    """Async turn routing methods should preserve sync-client registration semantics."""

    async def scenario() -> tuple[list[tuple[str, str]], Notification, str]:
        """Record the sync-client calls made by async turn notification wrappers."""
        client = AsyncCodexClient()
        event = Notification(
            method="unknown/direct",
            payload=UnknownNotification(params={"turnId": "turn-1"}),
        )
        completed = TurnCompletedNotification.model_validate(
            {
                "threadId": "thread-1",
                "turn": {"id": "turn-1", "items": [], "status": "completed"},
            }
        )
        calls: list[tuple[str, str]] = []
        events = [
            event,
            Notification(method="turn/completed", payload=completed),
        ]

        def fake_register(turn_id: str) -> None:
            """Record turn registration through the wrapped sync client."""
            calls.append(("register", turn_id))

        def fake_unregister(turn_id: str) -> None:
            """Record turn unregistration through the wrapped sync client."""
            calls.append(("unregister", turn_id))

        def fake_next(turn_id: str, _timeout_s: float | None = None) -> Notification:
            """Return one routed notification through the wrapped sync client."""
            calls.append(("next", turn_id))
            return events.pop(0)

        client._sync.register_turn_notifications = fake_register  # type: ignore[method-assign]
        client._sync.unregister_turn_notifications = fake_unregister  # type: ignore[method-assign]
        client._sync.next_turn_notification = fake_next  # type: ignore[method-assign]

        client.register_turn_notifications("turn-1")
        next_event = await client.next_turn_notification("turn-1")
        client.unregister_turn_notifications("turn-1")
        completed_event = await client.wait_for_turn_completed("turn-1")

        return calls, next_event, completed_event.turn.id

    calls, next_event, completed_turn_id = asyncio.run(scenario())

    assert (
        calls,
        next_event,
        completed_turn_id,
    ) == (
        [
            ("register", "turn-1"),
            ("next", "turn-1"),
            ("unregister", "turn-1"),
            ("register", "turn-1"),
            ("next", "turn-1"),
            ("unregister", "turn-1"),
        ],
        Notification(
            method="unknown/direct",
            payload=UnknownNotification(params={"turnId": "turn-1"}),
        ),
        "turn-1",
    )


def test_async_cancellation_reconciles_finite_worker() -> None:
    """Cancelling a finite to_thread call must not leave its worker alive."""

    async def scenario() -> bool:
        client = AsyncCodexClient()
        started = threading.Event()
        finished = threading.Event()

        def blocking_model_list(include_hidden: bool = False) -> bool:
            started.set()
            time.sleep(0.05)
            finished.set()
            return include_hidden

        client._sync.model_list = blocking_model_list  # type: ignore[method-assign]
        operation = asyncio.create_task(client.model_list())
        await asyncio.to_thread(started.wait, 1)
        operation.cancel()
        try:
            await operation
        except asyncio.CancelledError:
            pass
        return finished.is_set()

    assert asyncio.run(scenario()) is True


def test_cancelled_notification_wait_leaves_no_background_consumer() -> None:
    async def scenario() -> Notification:
        client = AsyncCodexClient()
        client.register_turn_notifications("turn-1")
        wait = asyncio.create_task(client.next_turn_notification("turn-1"))
        await asyncio.sleep(0.01)
        wait.cancel()
        try:
            await wait
        except asyncio.CancelledError:
            pass

        expected = Notification(
            method="unknown/direct",
            payload=UnknownNotification(params={"turnId": "turn-1"}),
        )
        client._sync._router.route_notification(expected)
        return await client.next_turn_notification("turn-1")

    assert asyncio.run(scenario()).method == "unknown/direct"
