from __future__ import annotations

import asyncio
import threading
import time

from openai_codex.async_client import AsyncCodexClient
from openai_codex.generated.v2_all import (
    TurnCompletedNotification,
    TurnInterruptResponse,
    TurnStartResponse,
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


def test_async_cancellation_does_not_wait_for_blocked_rpc() -> None:
    """Cancelling an unbounded sync RPC must not delay asyncio.run shutdown."""

    started = threading.Event()
    release = threading.Event()
    finished = threading.Event()
    runner_done = threading.Event()
    outcome: list[tuple[bool, bool] | BaseException] = []

    async def scenario() -> tuple[bool, bool]:
        client = AsyncCodexClient()

        def blocking_model_list(include_hidden: bool = False) -> bool:
            started.set()
            try:
                release.wait()
            finally:
                finished.set()
            raise RuntimeError(f"late worker failure: {include_hidden}")

        client._sync.model_list = blocking_model_list  # type: ignore[method-assign]
        operation = asyncio.create_task(client.model_list())
        while not started.is_set():
            await asyncio.sleep(0.001)
        operation.cancel()
        done, _ = await asyncio.wait({operation}, timeout=0.5)
        try:
            await operation
        except asyncio.CancelledError:
            pass
        return operation in done, operation.cancelled()

    def run_scenario() -> None:
        try:
            outcome.append(asyncio.run(scenario()))
        except BaseException as exc:
            outcome.append(exc)
        finally:
            runner_done.set()

    runner = threading.Thread(target=run_scenario, daemon=True)
    runner.start()
    try:
        assert runner_done.wait(0.5), "asyncio.run waited for the detached RPC worker"
        assert outcome == [(True, True)]
        assert not finished.is_set()
    finally:
        release.set()
        runner.join(timeout=1)
    assert finished.wait(1)


def test_cancelled_turn_start_cleans_up_after_late_response() -> None:
    async def scenario() -> tuple[bool, list[tuple[str, str]], bool]:
        client = AsyncCodexClient()
        started = threading.Event()
        release = threading.Event()
        cleanup_done = threading.Event()
        calls: list[tuple[str, str]] = []
        response = TurnStartResponse.model_validate(
            {"turn": {"id": "turn-1", "items": [], "status": "completed"}}
        )

        def blocking_turn_start(*_args: object) -> TurnStartResponse:
            started.set()
            release.wait()
            return response

        def interrupt(thread_id: str, turn_id: str) -> TurnInterruptResponse:
            calls.append(("interrupt", f"{thread_id}/{turn_id}"))
            return TurnInterruptResponse()

        def unregister(turn_id: str) -> None:
            calls.append(("unregister", turn_id))
            cleanup_done.set()

        client._sync.turn_start = blocking_turn_start  # type: ignore[method-assign]
        client._sync.turn_interrupt = interrupt  # type: ignore[method-assign]
        client._sync.unregister_turn_notifications = unregister  # type: ignore[method-assign]
        operation = asyncio.create_task(client.turn_start("thread-1", "hello"))
        assert await asyncio.to_thread(started.wait, 1)
        operation.cancel()
        done, _ = await asyncio.wait({operation}, timeout=0.5)
        completed_before_release = operation in done
        release.set()
        try:
            await operation
        except asyncio.CancelledError:
            pass
        cleanup_completed = await asyncio.to_thread(cleanup_done.wait, 1)
        return completed_before_release, calls, cleanup_completed

    assert asyncio.run(scenario()) == (
        True,
        [("interrupt", "thread-1/turn-1"), ("unregister", "turn-1")],
        True,
    )


def test_cancelled_notification_wait_preserves_notification_order() -> None:
    async def scenario() -> tuple[Notification, Notification]:
        client = AsyncCodexClient()
        client.register_turn_notifications("turn-1")
        poll_started = threading.Event()
        original_next = client._sync.next_turn_notification

        def observed_next(turn_id: str, timeout_s: float | None = None) -> Notification:
            poll_started.set()
            return original_next(turn_id, timeout_s)

        client._sync.next_turn_notification = observed_next  # type: ignore[method-assign]
        wait = asyncio.create_task(client.next_turn_notification("turn-1"))
        while not poll_started.is_set():
            await asyncio.sleep(0.001)
        wait.cancel()
        first = Notification(
            method="unknown/first",
            payload=UnknownNotification(params={"turnId": "turn-1"}),
        )
        second = Notification(
            method="unknown/second",
            payload=UnknownNotification(params={"turnId": "turn-1"}),
        )
        client._sync._router.route_notification(first)
        client._sync._router.route_notification(second)
        try:
            await wait
        except asyncio.CancelledError:
            pass

        return (
            await asyncio.wait_for(client.next_turn_notification("turn-1"), 0.5),
            await asyncio.wait_for(client.next_turn_notification("turn-1"), 0.5),
        )

    first, second = asyncio.run(scenario())
    assert (first.method, second.method) == ("unknown/first", "unknown/second")
