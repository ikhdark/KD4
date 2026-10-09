from __future__ import annotations

import queue
import threading
from collections import deque

from ._goal import _GoalOperationState
from ._limits import _BufferBudget, _NotificationQueue
from .errors import CodexError, TransportClosedError, map_jsonrpc_error
from .generated.notification_registry import notification_turn_id
from .generated.v2_all import AccountLoginCompletedNotification
from .models import JsonValue, Notification, UnknownNotification

ResponseQueueItem = JsonValue | BaseException
NotificationQueueItem = Notification | BaseException


class MessageRouter:
    """Route reader-thread messages to the SDK operation waiting for them.

    The app-server stdio transport is a single ordered stream, so only the
    reader thread should consume stdout. This router keeps the rest of the SDK
    from competing for that stream by giving each in-flight JSON-RPC request
    and active turn stream its own queue.
    """

    def __init__(
        self,
        *,
        max_notifications: int = 4096,
        max_buffer_bytes: int = 64 * 1024 * 1024,
        max_routes: int = 256,
        max_requests: int = 32,
        operation_timeout_s: float = 300.0,
    ) -> None:
        """Create empty response, turn, and global notification queues."""
        self._lock = threading.Lock()
        self._budget = _BufferBudget(max_notifications, max_buffer_bytes)
        self._max_routes = max_routes
        self._max_requests = max_requests
        self._operation_timeout_s = operation_timeout_s
        self._response_waiters: dict[str, queue.Queue[ResponseQueueItem]] = {}
        self._login_notifications: dict[str, _NotificationQueue] = {}
        self._pending_login_notifications: dict[str, deque[tuple[Notification, int]]] = {}
        self._turn_notifications: dict[str, _NotificationQueue] = {}
        self._pending_turn_notifications: dict[str, deque[tuple[Notification, int]]] = {}
        self._abandoned_turns: set[str] = set()
        self._terminal_turns: set[str] = set()
        self._goal_operations: dict[str, _GoalOperationState] = {}
        self._global_notifications = self._notification_queue(operation_deadline=False)
        self._failure: BaseException | None = None

    def _notification_queue(
        self, *, operation_deadline: bool = True, deadline: float | None = None
    ) -> _NotificationQueue:
        notifications = _NotificationQueue(
            self._budget, self._operation_timeout_s, operation_deadline=operation_deadline
        )
        if deadline is not None:
            notifications.deadline = deadline
        return notifications

    def _check_route_capacity(self, kind: str, key: str) -> None:
        turns = (
            self._turn_notifications.keys()
            | self._pending_turn_notifications.keys()
            | self._abandoned_turns
            | self._terminal_turns
        )
        logins = self._login_notifications.keys() | self._pending_login_notifications.keys()
        if (kind == "turn" and key in turns) or (kind == "login" and key in logins):
            return
        if len(turns) + len(logins) + len(self._goal_operations) >= self._max_routes:
            raise CodexError("notification route limit exceeded")

    def _discard_pending(self, pending: deque[tuple[Notification, int]]) -> None:
        for _, size in pending:
            self._budget.release(size)

    def create_response_waiter(self, request_id: str) -> queue.Queue[ResponseQueueItem]:
        """Register a one-shot queue for a JSON-RPC response id."""

        waiter: queue.Queue[ResponseQueueItem] = queue.Queue(maxsize=1)
        with self._lock:
            if self._failure is not None:
                raise self._failure
            if len(self._response_waiters) >= self._max_requests:
                raise CodexError("in-flight request limit exceeded")
            self._response_waiters[request_id] = waiter
        return waiter

    def discard_response_waiter(self, request_id: str) -> None:
        """Remove a response waiter when the request could not be written."""

        with self._lock:
            self._response_waiters.pop(request_id, None)

    def check_failure(self) -> None:
        with self._lock:
            failure = self._failure
        if failure is not None:
            raise failure

    def next_global_notification(self, timeout_s: float | None = None) -> Notification:
        """Block until the next notification that is not scoped to a turn."""

        with self._lock:
            failure = self._failure
        if failure is not None:
            raise failure
        item = self._global_notifications.get(timeout=timeout_s)
        if isinstance(item, BaseException):
            raise item
        return item

    def register_login(self, login_id: str, *, deadline: float | None = None) -> None:
        """Register a queue for one interactive login attempt."""

        login_queue = self._notification_queue(deadline=deadline)
        with self._lock:
            if self._failure is not None:
                raise self._failure
            if login_id in self._login_notifications:
                return
            self._check_route_capacity("login", login_id)
            pending = self._pending_login_notifications.pop(login_id, deque())
            for notification, size in pending:
                login_queue.put_reserved(notification, size)
            self._login_notifications[login_id] = login_queue

    def unregister_login(self, login_id: str) -> None:
        """Stop routing future notifications for one login attempt."""

        with self._lock:
            login_queue = self._login_notifications.pop(login_id, None)
            self._discard_pending(self._pending_login_notifications.pop(login_id, deque()))
        if login_queue is not None:
            login_queue.fail(TransportClosedError("login notifications unregistered"))

    def next_login_notification(
        self, login_id: str, timeout_s: float | None = None
    ) -> Notification:
        """Block until the next notification for a registered login attempt."""

        with self._lock:
            if self._failure is not None:
                raise self._failure
            login_queue = self._login_notifications.get(login_id)
        if login_queue is None:
            raise RuntimeError(f"login {login_id!r} is not registered for waiting")
        item = login_queue.get(timeout=timeout_s)
        if isinstance(item, BaseException):
            raise item
        return item

    def register_turn(self, turn_id: str, *, deadline: float | None = None) -> None:
        """Register a queue for a turn stream and replay early events."""

        turn_queue = self._notification_queue(deadline=deadline)
        with self._lock:
            if self._failure is not None:
                raise self._failure
            if turn_id in self._turn_notifications:
                return
            if turn_id in self._abandoned_turns:
                raise RuntimeError(f"turn {turn_id!r} was abandoned")
            # A turn can emit events immediately after turn/start, before the
            # caller receives the TurnHandle and starts streaming. Replay them
            # before publishing the queue so newer routed events stay behind.
            self._check_route_capacity("turn", turn_id)
            for notification, size in self._pending_turn_notifications.pop(turn_id, ()):
                turn_queue.put_reserved(notification, size)
            self._turn_notifications[turn_id] = turn_queue

    def unregister_turn(self, turn_id: str) -> None:
        """Stop retaining events for a turn that the caller will not consume."""

        with self._lock:
            turn_queue = self._turn_notifications.pop(turn_id, None)
            self._discard_pending(self._pending_turn_notifications.pop(turn_id, deque()))
            if turn_id in self._terminal_turns:
                self._terminal_turns.remove(turn_id)
            elif turn_id not in self._abandoned_turns:
                self._check_route_capacity("turn", turn_id)
                self._abandoned_turns.add(turn_id)
        if turn_queue is not None:
            turn_queue.fail(TransportClosedError("turn notifications unregistered"))

    def next_turn_notification(self, turn_id: str, timeout_s: float | None = None) -> Notification:
        """Block until the next notification for a registered turn."""

        with self._lock:
            if self._failure is not None:
                raise self._failure
            turn_queue = self._turn_notifications.get(turn_id)
        if turn_queue is None:
            raise RuntimeError(f"turn {turn_id!r} is not registered for streaming")
        item = turn_queue.get(timeout=timeout_s)
        if isinstance(item, BaseException):
            raise item
        return item

    def register_goal(
        self, thread_id: str, *, deadline: float | None = None
    ) -> _GoalOperationState:
        """Register one thread-scoped logical goal operation before it starts."""
        state = _GoalOperationState(
            thread_id=thread_id, _notifications=self._notification_queue(deadline=deadline)
        )
        state.activate_turn_routing()
        return self._register_goal(state)

    def reserve_goal(self, thread_id: str, *, deadline: float | None = None) -> _GoalOperationState:
        """Reserve a thread route without accepting physical turns yet."""
        return self._register_goal(
            _GoalOperationState(
                thread_id=thread_id, _notifications=self._notification_queue(deadline=deadline)
            )
        )

    def _register_goal(self, state: _GoalOperationState) -> _GoalOperationState:
        with self._lock:
            if self._failure is not None:
                raise self._failure
            if state.thread_id in self._goal_operations:
                raise RuntimeError(
                    f"thread {state.thread_id!r} already has an active goal operation"
                )
            self._check_route_capacity("goal", state.thread_id)
            self._goal_operations[state.thread_id] = state
        return state

    def unregister_goal(self, state: _GoalOperationState) -> None:
        """Stop routing notifications to a completed logical goal operation."""
        with self._lock:
            if self._goal_operations.get(state.thread_id) is state:
                self._goal_operations.pop(state.thread_id)

    def has_goal(self, thread_id: str) -> bool:
        """Return whether a logical goal operation owns this thread route."""
        with self._lock:
            return thread_id in self._goal_operations

    def route_response(self, msg: dict[str, JsonValue]) -> None:
        """Deliver a JSON-RPC response or error to its request waiter."""

        request_id = msg.get("id")
        with self._lock:
            if self._failure is not None:
                return
            waiter = self._response_waiters.pop(str(request_id), None)
        if waiter is None:
            return

        if "error" in msg:
            err = msg["error"]
            if isinstance(err, dict):
                waiter.put(
                    map_jsonrpc_error(
                        int(err.get("code", -32000)),
                        str(err.get("message", "unknown")),
                        err.get("data"),
                    )
                )
            else:
                waiter.put(CodexError("Malformed JSON-RPC error response"))
            return

        waiter.put(msg.get("result"))

    def route_notification(self, notification: Notification) -> None:
        """Deliver a notification to a turn queue or the global queue."""

        with self._lock:
            if self._failure is not None:
                return

        login_id = self._notification_login_id(notification)
        if login_id is not None:
            with self._lock:
                if self._failure is not None:
                    return
                login_queue = self._login_notifications.get(login_id)
                if login_queue is None:
                    if login_id not in self._pending_login_notifications:
                        self._check_route_capacity("login", login_id)
                    size = self._budget.reserve(notification)
                    self._pending_login_notifications.setdefault(login_id, deque()).append(
                        (notification, size)
                    )
                    return
            login_queue.put(notification)
            return

        turn_id = self._notification_turn_id(notification)
        thread_id = self._notification_thread_id(notification)
        if thread_id is not None:
            with self._lock:
                if self._failure is not None:
                    return
                goal_state = self._goal_operations.get(thread_id)
            if goal_state is not None and (
                turn_id is not None or notification.method.startswith("thread/goal/")
            ):
                if goal_state.observe(notification):
                    if goal_state.is_finished():
                        self.unregister_goal(goal_state)
                    return
        if turn_id is None:
            self._global_notifications.put(notification)
            return

        with self._lock:
            if self._failure is not None:
                return
            if turn_id in self._abandoned_turns:
                if notification.method == "turn/completed":
                    self._abandoned_turns.remove(turn_id)
                return
            if notification.method == "turn/completed" and turn_id not in self._terminal_turns:
                self._check_route_capacity("turn", turn_id)
                self._terminal_turns.add(turn_id)
            turn_queue = self._turn_notifications.get(turn_id)
            if turn_queue is None:
                if turn_id not in self._pending_turn_notifications:
                    self._check_route_capacity("turn", turn_id)
                size = self._budget.reserve(notification)
                self._pending_turn_notifications.setdefault(turn_id, deque()).append(
                    (notification, size)
                )
                return
        turn_queue.put(notification)

    def fail_all(self, exc: BaseException) -> None:
        """Wake every blocked waiter when the reader thread exits."""

        with self._lock:
            if self._failure is not None:
                return
            self._failure = exc
            response_waiters = list(self._response_waiters.values())
            self._response_waiters.clear()
            login_queues = list(self._login_notifications.values())
            self._login_notifications.clear()
            for pending in self._pending_login_notifications.values():
                self._discard_pending(pending)
            self._pending_login_notifications.clear()
            turn_queues = list(self._turn_notifications.values())
            self._turn_notifications.clear()
            for pending in self._pending_turn_notifications.values():
                self._discard_pending(pending)
            self._pending_turn_notifications.clear()
            self._abandoned_turns.clear()
            self._terminal_turns.clear()
            goal_operations = list(self._goal_operations.values())
            self._goal_operations.clear()
        # Put the same transport failure into every queue so no SDK call blocks
        # forever waiting for a response that cannot arrive.
        for waiter in response_waiters:
            waiter.put(exc)
        for login_queue in login_queues:
            login_queue.put(exc)
        for turn_queue in turn_queues:
            turn_queue.put(exc)
        for goal_operation in goal_operations:
            goal_operation.fail(exc)
        self._global_notifications.put(exc)

    def _notification_turn_id(self, notification: Notification) -> str | None:
        """Extract routing ids from generated metadata or raw unknown payloads."""
        payload = notification.payload
        if isinstance(payload, UnknownNotification):
            raw_turn_id = payload.params.get("turnId")
            if isinstance(raw_turn_id, str):
                return raw_turn_id
            raw_turn = payload.params.get("turn")
            if isinstance(raw_turn, dict):
                raw_nested_turn_id = raw_turn.get("id")
                if isinstance(raw_nested_turn_id, str):
                    return raw_nested_turn_id
            return None
        return notification_turn_id(payload)

    def _notification_thread_id(self, notification: Notification) -> str | None:
        """Extract thread ids from typed payloads or raw unknown payloads."""
        payload = notification.payload
        if isinstance(payload, UnknownNotification):
            raw_thread_id = payload.params.get("threadId")
            return raw_thread_id if isinstance(raw_thread_id, str) else None
        thread_id = getattr(payload, "thread_id", None)
        return thread_id if isinstance(thread_id, str) else None

    def _notification_login_id(self, notification: Notification) -> str | None:
        """Extract the login attempt id from completion notifications."""
        if notification.method != "account/login/completed":
            return None

        payload = notification.payload
        if isinstance(payload, AccountLoginCompletedNotification):
            return payload.login_id
        if isinstance(payload, UnknownNotification):
            raw_login_id = payload.params.get("loginId")
            if isinstance(raw_login_id, str):
                return raw_login_id
        return None
