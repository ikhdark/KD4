from __future__ import annotations

import asyncio
import sys
import threading

from openai_codex.async_client import AsyncCodexClient
from openai_codex.client import CodexConfig
from openai_codex.generated.v2_all import (
    TurnInterruptResponse,
    TurnStartResponse,
)
from openai_codex.models import Notification, UnknownNotification


def test_async_client_allows_concurrent_transport_calls() -> None:
    """Both RPCs must reach the peer before either response can be returned."""
    script = """
import json
import os
import sys
import threading

# A synchronous regression can block the event loop's timeout as well.
watchdog = threading.Timer(10, lambda: os._exit(2))
watchdog.start()
requests = [json.loads(sys.stdin.readline()), json.loads(sys.stdin.readline())]
for request in reversed(requests):
    print(json.dumps({"id": request["id"], "result": {
        "data": [], "nextCursor": str(request["params"]["includeHidden"]),
    }}), flush=True)
watchdog.cancel()
sys.stdin.read()
"""

    async def scenario() -> None:
        config = CodexConfig(launch_args_override=(sys.executable, "-u", "-c", script))
        async with AsyncCodexClient(config) as client:
            visible, hidden = await asyncio.wait_for(
                asyncio.gather(client.model_list(), client.model_list(include_hidden=True)),
                timeout=5,
            )
            assert (visible.next_cursor, hidden.next_cursor) == ("False", "True")

    asyncio.run(scenario())


def test_async_client_routes_turn_and_global_notifications_from_transport() -> None:
    """A pending RPC must not consume turn events or unrelated global events."""
    script = """
import json
import sys

for line in sys.stdin:
    request = json.loads(line)
    for event in [
        {"method": "unknown/global", "params": {}},
        {"method": "unknown/direct", "params": {"turnId": "turn-1"}},
        {"method": "turn/completed", "params": {
            "threadId": "thread-1",
            "turn": {"id": "turn-1", "items": [], "status": "completed"},
        }},
        {"id": request["id"], "result": {}},
    ]:
        print(json.dumps(event), flush=True)
"""

    async def scenario() -> None:
        config = CodexConfig(launch_args_override=(sys.executable, "-u", "-c", script))
        async with AsyncCodexClient(config) as client:
            client.register_turn_notifications("turn-1")
            await client.turn_interrupt("thread-1", "turn-1")
            event = await client.next_turn_notification("turn-1")
            assert event == Notification(
                method="unknown/direct",
                payload=UnknownNotification(params={"turnId": "turn-1"}),
            )
            completed = await client.wait_for_turn_completed("turn-1")
            assert completed.thread_id == "thread-1"
            assert completed.turn.id == "turn-1"
            assert completed.turn.status.value == "completed"
            assert await client.next_notification() == Notification(
                method="unknown/global", payload=UnknownNotification(params={})
            )

    asyncio.run(asyncio.wait_for(scenario(), timeout=5))


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
