from __future__ import annotations

import asyncio
import queue
import threading
from concurrent.futures import Future
from contextlib import nullcontext
from typing import AsyncIterator, Callable, ParamSpec, TypeVar

from pydantic import BaseModel

from ._goal import _GoalOperationState
from .client import CodexClient, CodexConfig
from .errors import CodexError
from .generated.v2_all import (
    AccountLoginCompletedNotification,
    AgentMessageDeltaNotification,
    CancelLoginAccountResponse,
    GetAccountParams as V2GetAccountParams,
    GetAccountResponse,
    LoginAccountParams as V2LoginAccountParams,
    LoginAccountResponse,
    LogoutAccountResponse,
    ModelListResponse,
    ThreadArchiveResponse,
    ThreadCompactStartResponse,
    ThreadForkParams as V2ThreadForkParams,
    ThreadForkResponse,
    ThreadGoalClearResponse,
    ThreadGoalSetResponse,
    ThreadGoalStatus,
    ThreadListParams as V2ThreadListParams,
    ThreadListResponse,
    ThreadReadResponse,
    ThreadResumeParams as V2ThreadResumeParams,
    ThreadResumeResponse,
    ThreadSetNameResponse,
    ThreadStartParams as V2ThreadStartParams,
    ThreadStartResponse,
    ThreadUnarchiveResponse,
    TurnCompletedNotification,
    TurnInterruptResponse,
    TurnStartParams as V2TurnStartParams,
    TurnStartResponse,
    TurnSteerResponse,
)
from .models import InitializeResponse, JsonObject, Notification

ModelT = TypeVar("ModelT", bound=BaseModel)
ParamsT = ParamSpec("ParamsT")
ReturnT = TypeVar("ReturnT")


def _consume_background_future_result(future: Future[ReturnT]) -> None:
    """Retrieve a detached worker result so late failures are not reported as unhandled."""
    try:
        future.result()
    except BaseException:
        pass


class AsyncCodexClient:
    """Async wrapper around CodexClient using thread offloading."""

    def __init__(self, config: CodexConfig | None = None) -> None:
        """Create the wrapped sync client that owns the transport process."""
        self._sync = CodexClient(config=config)
        self._worker_slots = threading.BoundedSemaphore(self._sync.config.max_in_flight_requests)
        self._close_slot = threading.BoundedSemaphore(1)

    @property
    def process_epoch(self) -> int:
        """Return the wrapped transport generation."""
        return self._sync.process_epoch

    async def __aenter__(self) -> "AsyncCodexClient":
        """Start the Codex process when entering an async context."""
        await self.start()
        return self

    async def __aexit__(self, _exc_type, _exc, _tb) -> None:
        """Close the Codex process when leaving an async context."""
        await self.close()

    async def _call_sync(
        self,
        fn: Callable[ParamsT, ReturnT],
        /,
        *args: ParamsT.args,
        **kwargs: ParamsT.kwargs,
    ) -> ReturnT:
        """Run a blocking sync-client operation without blocking the event loop."""
        return await self._run_sync(lambda: fn(*args, **kwargs))

    async def _run_sync(
        self,
        fn: Callable[[], ReturnT],
        on_cancel: Callable[[ReturnT], None] | None = None,
        *,
        shutdown: bool = False,
    ) -> ReturnT:
        slots = self._close_slot if shutdown else self._worker_slots
        if not slots.acquire(blocking=False):
            raise CodexError("async worker limit exceeded")
        operation: Future[ReturnT] = Future()
        exited: Future[None] = Future()
        cancelled = threading.Event()
        delivered = threading.Event()

        def run_operation() -> None:
            try:
                with nullcontext() if shutdown else self._sync._operation(cancelled):
                    result = fn()
                operation.set_result(result)
                if on_cancel is not None:
                    # The same admitted worker owns late-result cleanup. No
                    # unbounded cleanup threads, even if cancellation races delivery.
                    if not delivered.wait(self._sync.config.operation_timeout_s):
                        cancelled.set()
                    if cancelled.is_set():
                        on_cancel(result)
            except BaseException as exc:
                if not operation.done():
                    operation.set_exception(exc)
            finally:
                slots.release()
                exited.set_result(None)

        try:
            threading.Thread(
                target=run_operation,
                name="codex-async-client-rpc",
                daemon=True,
            ).start()
        except BaseException:
            slots.release()
            raise
        wrapped = asyncio.wrap_future(operation)
        worker_exited = asyncio.wrap_future(exited)
        try:
            timeout = (
                self._sync.config.shutdown_timeout_s + 1
                if shutdown
                else self._sync.config.operation_timeout_s
            )
            deadline = asyncio.get_running_loop().time() + timeout
            result = await asyncio.wait_for(asyncio.shield(wrapped), timeout=timeout)
        except (asyncio.CancelledError, asyncio.TimeoutError):
            cancelled.set()
            # SDK pipe/response waits observe this event and abort the captured
            # transport generation. Arbitrary caller callbacks cannot be forcibly
            # interrupted, but continue to occupy their bounded worker slot.
            operation.add_done_callback(_consume_background_future_result)
            wrapped.add_done_callback(lambda done: None if done.cancelled() else done.exception())
            raise
        else:
            # Delivery has committed: return the successful handle rather than
            # losing it to cancellation during the worker's admission handoff.
            delivered.set()
            while True:
                remaining = deadline - asyncio.get_running_loop().time()
                if remaining <= 0 or cancelled.is_set():
                    cancelled.set()
                    raise TimeoutError("operation delivery deadline exceeded")
                try:
                    await asyncio.wait_for(asyncio.shield(worker_exited), remaining)
                    break
                except asyncio.CancelledError:
                    continue
            return result
        finally:
            delivered.set()

    async def start(self) -> None:
        """Start the wrapped sync client in a worker thread."""
        await self._run_sync(self._sync.start, lambda _: self._sync.close())

    async def close(self) -> None:
        """Close the wrapped sync client in a worker thread."""
        await self._run_sync(self._sync.close, shutdown=True)

    async def initialize(self) -> InitializeResponse:
        """Initialize the Codex session."""
        return await self._call_sync(self._sync.initialize)

    def register_turn_notifications(self, turn_id: str) -> None:
        """Register a turn notification queue on the wrapped sync client."""
        self._sync.register_turn_notifications(turn_id)

    def register_login_notifications(self, login_id: str) -> None:
        """Register a login notification queue on the wrapped sync client."""
        self._sync.register_login_notifications(login_id)

    def unregister_login_notifications(self, login_id: str) -> None:
        """Unregister a login notification queue on the wrapped sync client."""
        self._sync.unregister_login_notifications(login_id)

    def unregister_turn_notifications(self, turn_id: str) -> None:
        """Unregister a turn notification queue on the wrapped sync client."""
        self._sync.unregister_turn_notifications(turn_id)

    def register_goal_operation(self, thread_id: str) -> _GoalOperationState:
        """Register a logical goal route on the wrapped sync client."""
        return self._sync.register_goal_operation(thread_id)

    def unregister_goal_operation(self, state: _GoalOperationState) -> None:
        """Release one logical goal route."""
        self._sync.unregister_goal_operation(state)

    async def request(
        self,
        method: str,
        params: JsonObject | None,
        *,
        response_model: type[ModelT],
    ) -> ModelT:
        """Send a typed JSON-RPC request through the wrapped sync client."""
        return await self._call_sync(
            self._sync.request,
            method,
            params,
            response_model=response_model,
        )

    async def account_login_start(
        self,
        params: V2LoginAccountParams | JsonObject,
    ) -> LoginAccountResponse:
        """Start one account login attempt through the wrapped sync client."""
        return await self._call_sync(self._sync.account_login_start, params)

    async def account_login_cancel(self, login_id: str) -> CancelLoginAccountResponse:
        """Cancel one active account login attempt through the wrapped sync client."""
        return await self._call_sync(self._sync.account_login_cancel, login_id)

    async def account_read(
        self,
        params: V2GetAccountParams | JsonObject | None = None,
    ) -> GetAccountResponse:
        """Read current account state through the wrapped sync client."""
        return await self._call_sync(self._sync.account_read, params)

    async def account_logout(self) -> LogoutAccountResponse:
        """Clear the active account session through the wrapped sync client."""
        return await self._call_sync(self._sync.account_logout)

    async def thread_start(
        self, params: V2ThreadStartParams | JsonObject | None = None
    ) -> ThreadStartResponse:
        """Start a thread using the wrapped sync client."""
        return await self._call_sync(self._sync.thread_start, params)

    async def thread_resume(
        self,
        thread_id: str,
        params: V2ThreadResumeParams | JsonObject | None = None,
    ) -> ThreadResumeResponse:
        """Resume a thread using the wrapped sync client."""
        return await self._call_sync(self._sync.thread_resume, thread_id, params)

    async def thread_list(
        self, params: V2ThreadListParams | JsonObject | None = None
    ) -> ThreadListResponse:
        """List threads using the wrapped sync client."""
        return await self._call_sync(self._sync.thread_list, params)

    async def thread_read(self, thread_id: str, include_turns: bool = False) -> ThreadReadResponse:
        """Read a thread using the wrapped sync client."""
        return await self._call_sync(self._sync.thread_read, thread_id, include_turns)

    async def thread_fork(
        self,
        thread_id: str,
        params: V2ThreadForkParams | JsonObject | None = None,
    ) -> ThreadForkResponse:
        """Fork a thread using the wrapped sync client."""
        return await self._call_sync(self._sync.thread_fork, thread_id, params)

    async def thread_archive(self, thread_id: str) -> ThreadArchiveResponse:
        """Archive a thread using the wrapped sync client."""
        return await self._call_sync(self._sync.thread_archive, thread_id)

    async def thread_unarchive(self, thread_id: str) -> ThreadUnarchiveResponse:
        """Unarchive a thread using the wrapped sync client."""
        return await self._call_sync(self._sync.thread_unarchive, thread_id)

    async def thread_set_name(self, thread_id: str, name: str) -> ThreadSetNameResponse:
        """Rename a thread using the wrapped sync client."""
        return await self._call_sync(self._sync.thread_set_name, thread_id, name)

    async def thread_compact(self, thread_id: str) -> ThreadCompactStartResponse:
        """Start thread compaction using the wrapped sync client."""
        return await self._call_sync(self._sync.thread_compact, thread_id)

    async def thread_goal_clear(self, thread_id: str) -> ThreadGoalClearResponse:
        """Clear the persisted goal through the wrapped sync client."""
        return await self._call_sync(self._sync.thread_goal_clear, thread_id)

    async def thread_goal_set(
        self,
        thread_id: str,
        *,
        objective: str | None = None,
        status: ThreadGoalStatus | None = None,
    ) -> ThreadGoalSetResponse:
        """Create or update a persisted goal through the wrapped sync client."""
        return await self._call_sync(
            self._sync.thread_goal_set,
            thread_id,
            objective=objective,
            status=status,
        )

    async def pause_goal(self, thread_id: str) -> ThreadGoalSetResponse:
        """Pause the active goal through the wrapped sync client."""
        return await self._call_sync(self._sync.pause_goal, thread_id)

    async def cancel_goal_operation(self, state: _GoalOperationState) -> None:
        """Stop continuation work after a logical goal operation is cancelled."""
        await self._call_sync(self._sync.cancel_goal_operation, state)

    async def start_goal_operation(
        self,
        thread_id: str,
        objective: str,
    ) -> tuple[_GoalOperationState, str]:
        """Start a logical goal through the wrapped sync client."""

        def cleanup(result: tuple[_GoalOperationState, str]) -> None:
            state, _ = result
            try:
                self._sync.cancel_goal_operation(state)
            finally:
                state.finish()
                self._sync.unregister_goal_operation(state)

        return await self._run_sync(
            lambda: self._sync.start_goal_operation(thread_id, objective),
            cleanup,
        )

    async def turn_start(
        self,
        thread_id: str,
        input_items: list[JsonObject] | JsonObject | str,
        params: V2TurnStartParams | JsonObject | None = None,
    ) -> TurnStartResponse:
        """Start a turn using the wrapped sync client."""

        def cleanup(started: TurnStartResponse) -> None:
            try:
                self._sync.turn_interrupt(thread_id, started.turn.id)
            except BaseException:
                pass
            finally:
                self._sync.unregister_turn_notifications(started.turn.id)

        return await self._run_sync(
            lambda: self._sync.turn_start(thread_id, input_items, params),
            cleanup,
        )

    async def turn_interrupt(self, thread_id: str, turn_id: str) -> TurnInterruptResponse:
        """Interrupt a turn using the wrapped sync client."""
        return await self._call_sync(self._sync.turn_interrupt, thread_id, turn_id)

    async def turn_steer(
        self,
        thread_id: str,
        expected_turn_id: str,
        input_items: list[JsonObject] | JsonObject | str,
    ) -> TurnSteerResponse:
        """Send steering input to a turn using the wrapped sync client."""
        return await self._call_sync(
            self._sync.turn_steer,
            thread_id,
            expected_turn_id,
            input_items,
        )

    async def model_list(self, include_hidden: bool = False) -> ModelListResponse:
        """List models using the wrapped sync client."""
        return await self._call_sync(self._sync.model_list, include_hidden)

    async def request_with_retry_on_overload(
        self,
        method: str,
        params: JsonObject | None,
        *,
        response_model: type[ModelT],
        max_attempts: int = 3,
        initial_delay_s: float = 0.25,
        max_delay_s: float = 2.0,
    ) -> ModelT:
        """Send a typed request with the sync client's overload retry policy."""
        return await self._call_sync(
            self._sync.request_with_retry_on_overload,
            method,
            params,
            response_model=response_model,
            max_attempts=max_attempts,
            initial_delay_s=initial_delay_s,
            max_delay_s=max_delay_s,
        )

    async def _poll_notification(self, receive: Callable[[], Notification]) -> Notification:
        deadline = asyncio.get_running_loop().time() + self._sync.config.operation_timeout_s
        while True:
            try:
                return receive()
            except queue.Empty:
                if asyncio.get_running_loop().time() >= deadline:
                    raise TimeoutError("notification wait deadline exceeded") from None
                await asyncio.sleep(0.01)

    async def next_notification(self) -> Notification:
        """Wait for a global notification within the configured operation budget."""
        return await self._poll_notification(lambda: self._sync.next_notification(0.0))

    async def next_login_notification(self, login_id: str) -> Notification:
        """Wait for the next notification routed to one login attempt."""
        return await self._poll_notification(
            lambda: self._sync.next_login_notification(login_id, 0.0)
        )

    async def next_turn_notification(self, turn_id: str) -> Notification:
        """Wait for the next notification routed to one turn."""
        return await self._poll_notification(
            lambda: self._sync.next_turn_notification(turn_id, 0.0)
        )

    async def next_goal_notification(self, state: _GoalOperationState) -> Notification:
        """Wait for the next notification in a logical goal turn."""
        return await self._poll_notification(lambda: self._sync.next_goal_notification(state, 0.0))

    async def wait_for_login_completed(
        self,
        login_id: str,
    ) -> AccountLoginCompletedNotification:
        """Wait for the completion notification routed to one login attempt."""
        self.register_login_notifications(login_id)
        try:
            while True:
                notification = await self.next_login_notification(login_id)
                if (
                    notification.method == "account/login/completed"
                    and isinstance(notification.payload, AccountLoginCompletedNotification)
                    and notification.payload.login_id == login_id
                ):
                    return notification.payload
        finally:
            self.unregister_login_notifications(login_id)

    async def wait_for_turn_completed(self, turn_id: str) -> TurnCompletedNotification:
        """Wait for the completion notification routed to one turn."""
        self.register_turn_notifications(turn_id)
        try:
            while True:
                notification = await self.next_turn_notification(turn_id)
                if (
                    notification.method == "turn/completed"
                    and isinstance(notification.payload, TurnCompletedNotification)
                    and notification.payload.turn.id == turn_id
                ):
                    return notification.payload
        finally:
            self.unregister_turn_notifications(turn_id)

    async def stream_text(
        self,
        thread_id: str,
        text: str,
        params: V2TurnStartParams | JsonObject | None = None,
    ) -> AsyncIterator[AgentMessageDeltaNotification]:
        """Stream text deltas from one turn without monopolizing the event loop."""
        started = await self.turn_start(thread_id, text, params)
        turn_id = started.turn.id
        try:
            while True:
                notification = await self.next_turn_notification(turn_id)
                if (
                    notification.method == "item/agentMessage/delta"
                    and isinstance(notification.payload, AgentMessageDeltaNotification)
                    and notification.payload.turn_id == turn_id
                ):
                    yield notification.payload
                    continue
                if (
                    notification.method == "turn/completed"
                    and isinstance(notification.payload, TurnCompletedNotification)
                    and notification.payload.turn.id == turn_id
                ):
                    break
        finally:
            self.unregister_turn_notifications(turn_id)
