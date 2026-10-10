from __future__ import annotations

import asyncio
import gc
import queue
import subprocess
import sys
import threading
import time
from collections import deque

import pytest

from openai_codex import Codex
from openai_codex._limits import _BufferBudget, _NotificationQueue
from openai_codex._message_router import MessageRouter
from openai_codex.api import AsyncCodex
from openai_codex.async_client import AsyncCodexClient
from openai_codex.client import CodexClient, CodexConfig
from openai_codex.errors import CodexError, ServerBusyError, TransportClosedError
from openai_codex.generated.v2_all import ModelListResponse
from openai_codex.models import Notification, UnknownNotification
from openai_codex.retry import retry_on_overload


def config(script: str, **limits) -> CodexConfig:
    return CodexConfig(
        launch_args_override=(sys.executable, "-u", "-c", script),
        **limits,
    )


def event(method: str = "test/event", **params) -> Notification:
    return Notification(method, UnknownNotification(params))


def wait_until(predicate, timeout: float = 3.0) -> None:
    deadline = time.monotonic() + timeout
    while not predicate():
        assert time.monotonic() < deadline, "condition did not become true"
        time.sleep(0.005)


@pytest.mark.parametrize("notification", [False, True])
def test_close_does_not_wait_for_a_full_stdin_pipe(notification) -> None:
    client = CodexClient(config("import time; time.sleep(30)"))
    client.start()
    proc = client._proc
    assert proc is not None and proc.stdin is not None
    entered = threading.Event()
    finished = threading.Event()
    errors: list[BaseException] = []
    stdin = proc.stdin

    class ObservedInput:
        def write(self, text):
            entered.set()
            return stdin.write(text)

        def flush(self):
            return stdin.flush()

        def close(self):
            return stdin.close()

    proc.stdin = ObservedInput()

    def send() -> None:
        try:
            if notification:
                client.notify("blocked", {"text": "x" * (2 * 1024 * 1024)})
            else:
                client._request_raw("blocked", {"text": "x" * (2 * 1024 * 1024)})
        except BaseException as exc:
            errors.append(exc)
        finally:
            finished.set()

    sender = threading.Thread(target=send, daemon=True)
    sender.start()
    try:
        assert entered.wait(3)
        assert not finished.wait(0.05), "test did not fill the OS pipe"
        started = time.monotonic()
        client.close()
        assert time.monotonic() - started < 3
        assert finished.wait(1)
        assert errors
        assert proc.poll() is not None
        assert client._writer_thread is not None and not client._writer_thread.is_alive()
    finally:
        proc.kill() if proc.poll() is None else None
        client.close()
        sender.join(2)


@pytest.mark.parametrize("blocked_write", [False, True])
def test_rpc_deadline_covers_write_and_response_wait(blocked_write) -> None:
    client = CodexClient(config("import time; time.sleep(30)", operation_timeout_s=0.2))
    try:
        client.start()
        proc = client._proc
        started = time.monotonic()
        with pytest.raises(TimeoutError, match="deadline"):
            client._request_raw(
                "blocked", {"text": "x" * (2 * 1024 * 1024)} if blocked_write else {}
            )
        assert time.monotonic() - started < 2
        assert not client._router._response_waiters
        assert proc is not None
        proc.wait(timeout=2)
    finally:
        client.close()


def test_shutdown_fallback_never_uses_an_unbounded_wait() -> None:
    client = CodexClient(CodexConfig(shutdown_timeout_s=0.02))
    waits: list[float] = []

    class Unkillable:
        def terminate(self):
            raise OSError("still running")

        def kill(self):
            pass

        def wait(self, timeout=None):
            assert timeout is not None
            waits.append(timeout)
            raise subprocess.TimeoutExpired("fake", timeout)

        def poll(self):
            return None

    proc = Unkillable()
    client._proc = proc
    client.close()
    assert waits and all(0 <= value <= 0.02 for value in waits)
    assert client._proc is proc, "an unreaped process must remain reachable"
    with pytest.raises(TransportClosedError):
        client._router.next_global_notification(0)


def test_newline_free_oversized_stdout_fails_waiters() -> None:
    client = CodexClient(
        config(
            "import sys,time; sys.stdout.write('x' * 4096); sys.stdout.flush(); time.sleep(30)",
            max_message_bytes=1024,
        )
    )
    try:
        client.start()
        with pytest.raises(CodexError, match="inbound message size"):
            client.next_notification(3)
    finally:
        client.close()


def test_oversized_outbound_message_does_not_poison_transport() -> None:
    script = """
import json,sys
for line in sys.stdin:
    request = json.loads(line)
    print(json.dumps({"id": request["id"], "result": {}}), flush=True)
"""
    with CodexClient(config(script, max_message_bytes=1024)) as client:
        with pytest.raises(CodexError, match="outbound message size"):
            client._request_raw("too-large", {"text": "x" * 2048})
        assert not client._router._response_waiters
        assert client._request_raw("small") == {}


def test_async_cancellation_stops_real_rpc_and_releases_worker() -> None:
    async def scenario() -> None:
        client = AsyncCodexClient(
            config(
                """
import json,sys,time
request = json.loads(sys.stdin.readline())
print(json.dumps({"method":"test/received","params":{}}), flush=True)
time.sleep(30)
""",
                max_in_flight_requests=1,
            )
        )
        await client.start()
        task = asyncio.create_task(client.model_list())
        try:
            await asyncio.wait_for(client.next_notification(), 3)
            proc = client._sync._proc
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task
            # close has reserved admission even while the cancelled worker exits.
            await asyncio.wait_for(client.close(), 3)
            assert proc is not None and proc.poll() is not None
            deadline = asyncio.get_running_loop().time() + 2
            while not client._worker_slots.acquire(blocking=False):
                assert asyncio.get_running_loop().time() < deadline
                await asyncio.sleep(0.005)
            client._worker_slots.release()
            assert not client._sync._router._response_waiters
        finally:
            task.cancel()
            await client.close()

    asyncio.run(scenario())


def test_cancelled_uncooperative_callback_retains_bounded_admission() -> None:
    async def scenario() -> None:
        client = AsyncCodexClient(CodexConfig(max_in_flight_requests=1))
        entered, release, finished = threading.Event(), threading.Event(), threading.Event()

        def blocked():
            entered.set()
            try:
                release.wait(3)
            finally:
                finished.set()

        task = asyncio.create_task(client._call_sync(blocked))
        try:
            while not entered.is_set():
                await asyncio.sleep(0.005)
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task
            for _ in range(20):
                with pytest.raises(CodexError, match="worker limit"):
                    await client._call_sync(lambda: None)
            assert not finished.is_set()
            await client.close()
        finally:
            release.set()
        assert await asyncio.to_thread(finished.wait, 2)

    asyncio.run(scenario())


@pytest.mark.parametrize("route", ["global", "early-turn", "turn", "early-login", "login", "goal"])
def test_notification_budget_covers_all_routes_and_releases_on_failure(route) -> None:
    router = MessageRouter(max_notifications=1)
    if route in {"turn", "early-turn"}:
        notification = event(turnId="turn")
        if route == "turn":
            router.register_turn("turn")
    elif route in {"login", "early-login"}:
        notification = event("account/login/completed", loginId="login")
        if route == "login":
            router.register_login("login")
    elif route == "goal":
        router.register_goal("thread")
        notification = event(turnId="turn", threadId="thread")
    else:
        notification = event()
    router.route_notification(notification)
    with pytest.raises(CodexError, match="buffer limit"):
        router.route_notification(notification)
    assert router._budget.items == 1
    router.fail_all(TransportClosedError("test failure"))
    assert router._budget.items == router._budget.bytes == 0


def test_notification_bytes_are_bounded_and_consumption_releases_budget() -> None:
    router = MessageRouter(max_buffer_bytes=200)
    notification = event(text="x" * 100)
    router.route_notification(notification)
    with pytest.raises(CodexError, match="buffer limit"):
        router.route_notification(notification)
    assert router.next_global_notification(0) == notification
    assert router._budget.bytes == 0
    router.route_notification(notification)
    assert router.next_global_notification(0) == notification


def test_notification_item_budget_is_shared_across_routes() -> None:
    # Six independently usable routes still share one configured item limit.
    router = MessageRouter(max_notifications=6)
    router.register_turn("turn")
    router.register_login("login")
    goal = router.register_goal("thread")
    notifications = [
        event(),
        event(turnId="early-turn"),
        event(turnId="turn"),
        event("account/login/completed", loginId="early-login"),
        event("account/login/completed", loginId="login"),
        event(turnId="goal-turn", threadId="thread"),
    ]
    for notification in notifications:
        router.route_notification(notification)
    with pytest.raises(CodexError, match="buffer limit"):
        router.route_notification(event(turnId="another-turn"))
    assert router._budget.items == 6

    # Refusal must not evict another route, and consumption frees shared capacity.
    router.register_turn("early-turn")
    router.register_login("early-login")
    assert [
        router.next_global_notification(0),
        router.next_turn_notification("early-turn", 0),
        router.next_turn_notification("turn", 0),
        router.next_login_notification("early-login", 0),
        router.next_login_notification("login", 0),
        goal.next_notification(0),
    ] == notifications
    assert router._budget.items == router._budget.bytes == 0
    router.route_notification(notifications[0])
    assert router.next_global_notification(0) == notifications[0]


def test_early_replay_transfers_budget_without_duplicate_charges() -> None:
    router = MessageRouter(max_notifications=1)
    notification = event(turnId="turn")
    router.route_notification(notification)
    router.register_turn("turn")
    assert router._budget.items == 1
    assert router.next_turn_notification("turn", 0) == notification
    assert router._budget.items == router._budget.bytes == 0


def test_login_registration_replays_before_concurrent_delivery() -> None:
    router = MessageRouter()
    first = event("account/login/completed", loginId="login", order=1)
    second = event("account/login/completed", loginId="login", order=2)
    router.route_notification(first)
    threads = []

    class Interleaved(deque):
        def __iter__(self):
            thread = threading.Thread(target=router.route_notification, args=(second,))
            threads.append(thread)
            thread.start()
            thread.join(0.05)
            return super().__iter__()

    router._pending_login_notifications["login"] = Interleaved(
        router._pending_login_notifications["login"]
    )
    router.register_login("login")
    for thread in threads:
        thread.join(2)
        assert not thread.is_alive()
    assert router.next_login_notification("login", 0) == first
    assert router.next_login_notification("login", 0) == second


def test_route_and_request_maps_reject_growth() -> None:
    router = MessageRouter(max_routes=2, max_requests=1)
    router.route_notification(event(turnId="one"))
    router.route_notification(event(turnId="two"))
    with pytest.raises(CodexError, match="route limit"):
        router.route_notification(event(turnId="three"))
    assert len(router._pending_turn_notifications) == 2
    router.create_response_waiter("one")
    with pytest.raises(CodexError, match="request limit"):
        router.create_response_waiter("two")
    assert len(router._response_waiters) == 1


def test_failure_wakes_all_consumers_even_with_full_buffer() -> None:
    budget = _BufferBudget(max_items=1)
    notifications = _NotificationQueue(budget)
    notifications.put(event())
    failure = TransportClosedError("closed")
    notifications.fail(failure)
    assert budget.items == budget.bytes == 0
    outcomes = []

    def receive():
        try:
            notifications.get()
        except BaseException as exc:
            outcomes.append(exc)

    threads = [threading.Thread(target=receive, daemon=True) for _ in range(3)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(1)
        assert not thread.is_alive()
    assert outcomes == [failure] * 3


def test_failure_wakes_already_blocked_consumers_with_full_shared_budget(monkeypatch) -> None:
    budget = _BufferBudget(max_items=1)
    full_queue = _NotificationQueue(budget)
    full_queue.put(event())
    notifications = _NotificationQueue(budget)
    failure = TransportClosedError("closed")
    waiting = threading.Event()
    waiting_threads = set()
    original_wait = notifications._condition.wait
    outcomes = []

    def observed_wait(timeout=None):
        # Called under the queue condition, so fail() cannot race the last wait.
        waiting_threads.add(threading.get_ident())
        if len(waiting_threads) == 3:
            waiting.set()
        return original_wait(timeout)

    def receive():
        try:
            notifications.get()
        except BaseException as exc:
            outcomes.append(exc)

    monkeypatch.setattr(notifications._condition, "wait", observed_wait)
    threads = [threading.Thread(target=receive, daemon=True) for _ in range(3)]
    for thread in threads:
        thread.start()
    try:
        assert waiting.wait(2), "consumers did not block before failure"
        assert budget.items == 1
        notifications.fail(failure)
        for thread in threads:
            thread.join(1)
            assert not thread.is_alive()
        assert outcomes == [failure] * 3
        assert budget.items == 1, "failing another queue must not release this queue's item"
    finally:
        # Even a missing notify_all regression must not leave test workers behind.
        notifications.fail(failure)
        with notifications._condition:
            notifications._condition.notify_all()
        for thread in threads:
            thread.join(1)
        full_queue.fail(failure)
    assert budget.items == budget.bytes == 0


def test_abandoned_queue_returns_its_shared_budget() -> None:
    budget = _BufferBudget()
    notifications = _NotificationQueue(budget)
    notifications.put(event())
    del notifications
    gc.collect()
    assert budget.items == budget.bytes == 0


def test_stream_deadline_is_not_reset_by_events_or_async_polling(monkeypatch) -> None:
    clock = [10.0]
    monkeypatch.setattr("openai_codex._limits.time.monotonic", lambda: clock[0])
    router = MessageRouter(operation_timeout_s=1)
    router.register_turn("turn")
    with pytest.raises(queue.Empty):
        router.next_turn_notification("turn", 0)
    clock[0] = 10.5
    router.route_notification(event(turnId="turn"))
    router.next_turn_notification("turn", 0)
    clock[0] = 11.0
    router.route_notification(event(turnId="turn"))
    with pytest.raises(TimeoutError, match="operation deadline"):
        router.next_turn_notification("turn", 0)


@pytest.mark.parametrize(
    "method",
    [
        "item/commandExecution/requestApproval",
        "item/fileChange/requestApproval",
    ],
)
def test_default_approval_denies_through_real_transport(method) -> None:
    script = f"""
import json,sys
print(json.dumps({{"id":"approval", "method":{method!r}, "params":{{}}}}), flush=True)
response = json.loads(sys.stdin.readline())
print(json.dumps({{"method":"test/decision", "params":response["result"]}}), flush=True)
sys.stdin.read()
"""
    with CodexClient(config(script)) as client:
        assert client.next_notification(3).payload.params == {"decision": "decline"}


@pytest.mark.parametrize("failure", [KeyboardInterrupt(), SystemExit(7), RuntimeError("init")])
def test_sync_initialization_preserves_failure_when_cleanup_also_fails(
    monkeypatch, failure
) -> None:
    calls = []

    class Broken:
        def __init__(self, config=None):
            pass

        def start(self):
            calls.append("start")

        def initialize(self):
            raise failure

        def close(self):
            calls.append("close")
            raise RuntimeError("cleanup")

    monkeypatch.setattr("openai_codex.api.CodexClient", Broken)
    with pytest.raises(type(failure)) as caught:
        Codex()
    assert caught.value is failure
    assert calls == ["start", "close"]


def test_async_initialization_preserves_failure_when_cleanup_also_fails() -> None:
    async def scenario():
        codex = AsyncCodex()
        failure = RuntimeError("init")

        async def start():
            raise failure

        async def close():
            raise RuntimeError("cleanup")

        codex._client.start = start
        codex._client.close = close
        with pytest.raises(RuntimeError) as caught:
            await codex.models()
        assert caught.value is failure
        assert not codex._initialized and codex._init is None

    asyncio.run(scenario())


@pytest.mark.parametrize(
    "name", ["initial_delay_s", "max_delay_s", "jitter_ratio", "max_attempts", "timeout_s"]
)
@pytest.mark.parametrize("value", [float("nan"), float("inf"), -float("inf")])
def test_retry_rejects_nonfinite_limits_before_calling_operation(name, value) -> None:
    with pytest.raises(ValueError, match=name):
        retry_on_overload(lambda: pytest.fail("called with invalid limits"), **{name: value})


@pytest.mark.parametrize("value", [1.5, True, "3"])
def test_retry_requires_integer_attempt_count(value) -> None:
    with pytest.raises(ValueError, match="max_attempts"):
        retry_on_overload(
            lambda: pytest.fail("called with invalid attempt limit"), max_attempts=value
        )


def test_retry_budget_spans_attempts_and_sleep(monkeypatch) -> None:
    clock = [0.0]
    attempts = []
    sleeps = []

    def overloaded():
        attempts.append(clock[0])
        clock[0] += 0.4
        raise ServerBusyError(-32001, "busy")

    def sleep(delay):
        sleeps.append(delay)
        clock[0] += delay

    monkeypatch.setattr("openai_codex.retry.time.monotonic", lambda: clock[0])
    monkeypatch.setattr("openai_codex.retry.time.sleep", sleep)
    with pytest.raises(TimeoutError, match="deadline"):
        retry_on_overload(overloaded, timeout_s=1, initial_delay_s=2, jitter_ratio=0)
    assert attempts == [0.0]
    assert sleeps == [0.6]


def test_sdk_retry_deadline_includes_blocked_retry_response() -> None:
    script = """
import json,sys,time
request = json.loads(sys.stdin.readline())
print(json.dumps({"id":request["id"],"error":{"code":-32001,"message":"busy","data":{
    "reason":"serializedRequestQueue","retryable":True}}}), flush=True)
sys.stdin.readline()
time.sleep(30)
"""
    with CodexClient(config(script, operation_timeout_s=0.3)) as client:
        started = time.monotonic()
        with pytest.raises(TimeoutError, match="deadline"):
            client.request_with_retry_on_overload(
                "model/list",
                {},
                response_model=ModelListResponse,
                initial_delay_s=0,
            )
        assert time.monotonic() - started < 2


def test_approval_reply_has_reserved_admission_when_rpc_limit_is_full() -> None:
    script = """
import json,sys
request = json.loads(sys.stdin.readline())
print(json.dumps({"id":"approval","method":"item/commandExecution/requestApproval","params":{}}), flush=True)
reply = json.loads(sys.stdin.readline())
print(json.dumps({"id":request["id"],"result":reply["result"]}), flush=True)
sys.stdin.read()
"""
    with CodexClient(config(script, max_in_flight_requests=1)) as client:
        assert client._request_raw("test/approval") == {"decision": "decline"}


def test_explicit_low_level_approval_handler_remains_supported() -> None:
    client = CodexClient(approval_handler=lambda method, params: {"decision": "accept"})
    assert client._handle_server_request(
        {
            "method": "item/fileChange/requestApproval",
            "params": {},
        }
    ) == {"decision": "accept"}
    with pytest.raises(CodexError, match="Unsupported server request"):
        CodexClient()._handle_server_request({"method": "unknown/approval", "params": {}})
    assert CodexClient()._handle_server_request(
        {
            "method": "item/permissions/requestApproval",
            "params": {},
        }
    ) == {"permissions": {}, "scope": "turn"}


def test_route_capacity_allows_existing_route_replay_and_completion() -> None:
    router = MessageRouter(max_routes=1)
    first = event(turnId="turn")
    router.route_notification(first)
    router.register_turn("turn")
    last = event("turn/completed", turnId="turn")
    router.route_notification(last)
    assert router.next_turn_notification("turn", 0) == first
    assert router.next_turn_notification("turn", 0) == last
    router.unregister_turn("turn")
    router.register_login("login")


@pytest.mark.parametrize("login", [False, True])
def test_failure_racing_notification_routing_cannot_repopulate_pending_maps(
    monkeypatch, login
) -> None:
    router = MessageRouter()
    entered, release = threading.Event(), threading.Event()
    selector = "_notification_login_id" if login else "_notification_turn_id"
    original = getattr(router, selector)

    def blocked(notification):
        entered.set()
        assert release.wait(2)
        return original(notification)

    monkeypatch.setattr(router, selector, blocked)
    notification = (
        event("account/login/completed", loginId="login") if login else event(turnId="turn")
    )
    thread = threading.Thread(target=router.route_notification, args=(notification,), daemon=True)
    thread.start()
    try:
        assert entered.wait(2)
        router.fail_all(TransportClosedError("closed"))
    finally:
        release.set()
        thread.join(2)
    assert not thread.is_alive()
    assert not router._pending_login_notifications and not router._pending_turn_notifications
    assert router._budget.items == 0


def test_old_operation_cannot_send_on_restarted_transport() -> None:
    script = """
import json,sys
for line in sys.stdin:
    request = json.loads(line)
    print(json.dumps({"id":request["id"],"result":{}}), flush=True)
"""
    client = CodexClient(config(script))
    try:
        client.start()
        with client._operation():
            client.close()
            client.start()
            with pytest.raises(TransportClosedError, match="previous transport"):
                client._request_raw("stale")
        assert client._request_raw("current") == {}
    finally:
        client.close()


def test_slow_pipe_cleanup_does_not_block_close_or_allow_worker_accumulation() -> None:
    client = CodexClient(config("import sys; sys.stdin.read()", shutdown_timeout_s=0.1))
    client.start()
    proc = client._proc
    assert proc is not None and proc.stdin is not None
    stdin = proc.stdin
    closing, release = threading.Event(), threading.Event()

    class SlowClose:
        def close(self):
            closing.set()
            assert release.wait(3)
            stdin.close()

    proc.stdin = SlowClose()
    try:
        started = time.monotonic()
        client.close()
        assert time.monotonic() - started < 1
        assert closing.wait(1)
        with pytest.raises(TransportClosedError, match="previous transport workers"):
            client.start()
    finally:
        release.set()
        assert client._writer_thread is not None
        client._writer_thread.join(2)
        client.close()
    assert not client._writer_thread.is_alive()


def test_async_global_notification_wait_has_a_deadline() -> None:
    async def scenario():
        client = AsyncCodexClient(CodexConfig(operation_timeout_s=0.02))
        with pytest.raises(TimeoutError, match="notification wait deadline"):
            await asyncio.wait_for(client.next_notification(), 1)

    asyncio.run(scenario())


@pytest.mark.parametrize("name", ["operation_timeout_s", "shutdown_timeout_s"])
@pytest.mark.parametrize("value", [0, -1, float("nan"), float("inf"), -float("inf"), True])
def test_config_rejects_invalid_time_budgets(name, value) -> None:
    with pytest.raises(ValueError, match=name):
        CodexConfig(**{name: value})


@pytest.mark.parametrize(
    "name",
    [
        "max_message_bytes",
        "max_buffered_notifications",
        "max_buffer_bytes",
        "max_notification_routes",
        "max_in_flight_requests",
    ],
)
@pytest.mark.parametrize("value", [0, -1, 1.5, float("nan"), float("inf"), True])
def test_config_rejects_invalid_capacity_limits(name, value) -> None:
    with pytest.raises(ValueError, match=name):
        CodexConfig(**{name: value})


def test_notification_routes_inherit_the_start_operation_deadline(monkeypatch) -> None:
    clock = [10.0]
    monkeypatch.setattr("openai_codex.client.time.monotonic", lambda: clock[0])
    client = CodexClient(CodexConfig(operation_timeout_s=1))
    with client._operation():
        clock[0] = 10.75
        client.register_turn_notifications("turn")
        client.register_login_notifications("login")
        goal = client.reserve_goal_operation("thread")
    assert client._router._turn_notifications["turn"].deadline == 11
    assert client._router._login_notifications["login"].deadline == 11
    assert goal._notifications.deadline == 11
    clock[0] = 11
    for receive in (
        lambda: client.next_turn_notification("turn", 0),
        lambda: client.next_login_notification("login", 0),
        lambda: client.next_goal_notification(goal, 0),
    ):
        with pytest.raises(TimeoutError, match="operation deadline"):
            receive()


def test_async_worker_handoff_cannot_extend_operation_deadline() -> None:
    release, finished = threading.Event(), threading.Event()

    async def scenario():
        client = AsyncCodexClient(CodexConfig(operation_timeout_s=0.02))
        slots = client._worker_slots

        class DelayedRelease:
            def acquire(self, **kwargs):
                return slots.acquire(**kwargs)

            def release(self):
                try:
                    release.wait(2)
                finally:
                    slots.release()
                    finished.set()

        client._worker_slots = DelayedRelease()
        operation = asyncio.create_task(client._call_sync(lambda: "completed"))
        try:
            # The watchdog must not supply the TimeoutError this test expects.
            done, _ = await asyncio.wait({operation}, timeout=0.5)
            assert operation in done, "SDK operation exceeded its delivery deadline"
            with pytest.raises(TimeoutError):
                await operation
        finally:
            release.set()
            assert await asyncio.to_thread(finished.wait, 1)
            if not operation.done():
                operation.cancel()
            await asyncio.gather(operation, return_exceptions=True)

    asyncio.run(scenario())


def test_async_goal_start_cancellation_stops_waiting_for_first_turn(monkeypatch) -> None:
    from types import SimpleNamespace

    from openai_codex.generated.v2_all import IdleThreadStatus

    async def scenario():
        client = AsyncCodexClient()
        activated = threading.Event()
        thread = SimpleNamespace(
            status=SimpleNamespace(root=IdleThreadStatus(type="idle")),
            ephemeral=False,
            path="saved",
        )
        monkeypatch.setattr(client._sync, "thread_read", lambda _: SimpleNamespace(thread=thread))
        monkeypatch.setattr(client._sync, "thread_goal_clear", lambda _: None)
        monkeypatch.setattr(
            client._sync, "thread_goal_set", lambda *args, **kwargs: activated.set()
        )
        operation = asyncio.create_task(client.start_goal_operation("thread", "objective"))
        assert await asyncio.to_thread(activated.wait, 1)
        operation.cancel()
        with pytest.raises(asyncio.CancelledError):
            await operation
        deadline = asyncio.get_running_loop().time() + 1
        while client._sync._router.has_goal("thread"):
            assert asyncio.get_running_loop().time() < deadline
            await asyncio.sleep(0.005)
        assert not client._sync._thread_start_locks

    asyncio.run(scenario())
