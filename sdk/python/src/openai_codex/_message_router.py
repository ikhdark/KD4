from __future__ import annotations

import queue
import threading
import time
from collections import OrderedDict, deque
from weakref import WeakSet, WeakValueDictionary

from ._goal import _GoalOperationState
from ._limits import _BufferBudget, _NotificationBufferFull, _NotificationQueue
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
        max_control_requests: int = 2,
        operation_timeout_s: float = 300.0,
        notification_timeout_s: float = 300.0,
        turn_timeout_s: float | None = None,
    ) -> None:
        """Create empty response, turn, and global notification queues."""
        self._lock = threading.Lock()
        self._budget = _BufferBudget(max_notifications, max_buffer_bytes)
        self._max_routes = max_routes
        self._max_requests = max_requests
        self._max_control_requests = max_control_requests
        self._operation_timeout_s = operation_timeout_s
        self._notification_timeout_s = notification_timeout_s
        self._turn_timeout_s = turn_timeout_s
        self._response_waiters: dict[str, queue.Queue[ResponseQueueItem]] = {}
        self._control_waiters: set[str] = set()
        self._login_notifications: dict[str, _NotificationQueue] = {}
        self._pending_login_notifications: dict[str, deque[tuple[Notification, int]]] = {}
        self._terminal_logins: set[str] = set()
        self._detached_logins: WeakValueDictionary[str, _NotificationQueue] = WeakValueDictionary()
        self._completed_detached_logins: WeakSet[_NotificationQueue] = WeakSet()
        self._recent_cancelled_logins: OrderedDict[str, _NotificationQueue] = OrderedDict()
        self._login_starts = 0
        self._turn_notifications: dict[str, _NotificationQueue] = {}
        self._pending_turn_notifications: dict[str, deque[tuple[Notification, int]]] = {}
        self._abandoned_turns: set[str] = set()
        self._terminal_turns: set[str] = set()
        self._goal_operations: dict[str, _GoalOperationState] = {}
        # Includes handle-owned completed queues and routes being unregistered,
        # so their still-charged backlog cannot be blamed on an unrelated sender.
        self._notification_queues: WeakSet[_NotificationQueue] = WeakSet()
        self._notification_queues_lock = threading.Lock()
        self._global_notifications = self._notification_queue(operation_deadline=False)
        self._failure: BaseException | None = None

    def _notification_queue(
        self, *, operation_deadline: bool = True, deadline: float | None = None
    ) -> _NotificationQueue:
        notifications = _NotificationQueue(self._budget, self._notification_timeout_s)
        if operation_deadline:
            notifications.deadline = time.monotonic() + self._operation_timeout_s
        if deadline is not None:
            notifications.deadline = deadline
        with self._notification_queues_lock:
            self._notification_queues.add(notifications)
        return notifications

    def _check_route_capacity(self, kind: str, key: str) -> None:
        if kind == "turn" and (
            key in self._turn_notifications
            or key in self._pending_turn_notifications
            or key in self._abandoned_turns
            or key in self._terminal_turns
        ):
            return
        if kind == "login" and (
            key in self._login_notifications or key in self._pending_login_notifications
        ):
            return
        turns = (
            self._turn_notifications.keys()
            | self._pending_turn_notifications.keys()
            | self._abandoned_turns
            | self._terminal_turns
        )
        logins = self._login_notifications.keys() | self._pending_login_notifications.keys()
        if len(turns) + len(logins) + len(self._goal_operations) >= self._max_routes:
            raise CodexError("notification route limit exceeded")

    def _discard_pending(self, pending: deque[tuple[Notification, int]]) -> None:
        for _, size in pending:
            self._budget.release(size)

    def create_response_waiter(
        self, request_id: str, *, control: bool = False
    ) -> queue.Queue[ResponseQueueItem]:
        """Register a one-shot queue for a JSON-RPC response id."""

        waiter: queue.Queue[ResponseQueueItem] = queue.Queue(maxsize=1)
        with self._lock:
            if self._failure is not None:
                raise self._failure
            count = len(self._control_waiters) if control else (
                len(self._response_waiters) - len(self._control_waiters)
            )
            limit = self._max_control_requests if control else self._max_requests
            if count >= limit:
                raise CodexError("in-flight request limit exceeded")
            self._response_waiters[request_id] = waiter
            if control:
                self._control_waiters.add(request_id)
        return waiter

    def discard_response_waiter(self, request_id: str) -> None:
        """Remove a response waiter when the request could not be written."""

        with self._lock:
            self._response_waiters.pop(request_id, None)
            self._control_waiters.discard(request_id)

    def check_failure(self) -> None:
        with self._lock:
            failure = self._failure
        if failure is not None:
            raise failure

    def next_global_notification(self, timeout_s: float | None = None) -> Notification:
        """Block until the next notification that is not scoped to a turn."""
        return self.global_notification_queue().get(timeout=timeout_s)

    def global_notification_queue(self) -> _NotificationQueue:
        with self._lock:
            failure = self._failure
        if failure is not None:
            raise failure
        return self._global_notifications

    def register_login(self, login_id: str, *, deadline: float | None = None) -> None:
        """Register a queue for one interactive login attempt."""

        login_queue = self._notification_queue(deadline=deadline)
        with self._lock:
            detached = self._detached_logins.get(login_id)
            if detached is not None:
                if self._failure is not None and detached not in self._completed_detached_logins:
                    raise self._failure
                return
            if self._failure is not None and login_id not in self._terminal_logins:
                raise self._failure
            if login_id in self._login_notifications:
                return
            self._check_route_capacity("login", login_id)
            pending = self._pending_login_notifications.pop(login_id, deque())
            for notification, size in pending:
                login_queue.put_reserved(notification, size)
            self._login_notifications[login_id] = login_queue
            if login_id in self._terminal_logins:
                login_queue.deadline = None
            if self._failure is not None:
                login_queue.fail(self._failure, drain=True)

    def unregister_login(
        self, login_id: str, *, expected: _NotificationQueue | None = None
    ) -> None:
        """Stop routing future notifications for one login attempt."""

        with self._lock:
            if expected is not None and expected is not (
                self._login_notifications.get(login_id) or self._detached_logins.get(login_id)
            ):
                return
            login_queue = self._login_notifications.pop(login_id, None)
            detached = self._detached_logins.pop(login_id, None)
            self._recent_cancelled_logins.pop(login_id, None)
            login_queue = login_queue if login_queue is not None else detached
            if login_queue is not None:
                self._completed_detached_logins.discard(login_queue)
            self._terminal_logins.discard(login_id)
            self._discard_pending(self._pending_login_notifications.pop(login_id, deque()))
        if login_queue is not None:
            login_queue.fail(TransportClosedError("login notifications unregistered"))

    def retain_login(self, login_id: str) -> _NotificationQueue:
        """Give a handle ownership independent of the live route budget."""
        with self._lock:
            notifications = self._login_notifications.get(login_id)
            if notifications is None:
                notifications = self._detached_logins.get(login_id)
            if notifications is None:
                raise RuntimeError("login notifications are no longer available")
            return notifications

    def retire_login(self, login_id: str) -> None:
        """Retire an acknowledged cancellation without inventing its completion."""
        with self._lock:
            notifications = self._login_notifications.pop(login_id, None)
            if notifications is None:
                return
            if login_id in self._terminal_logins:
                self._completed_detached_logins.add(notifications)
            self._terminal_logins.discard(login_id)
            self._detached_logins[login_id] = notifications
            self._recent_cancelled_logins[login_id] = notifications
            while len(self._recent_cancelled_logins) > self._max_routes:
                self._recent_cancelled_logins.popitem(last=False)

    def begin_login_start(self) -> None:
        with self._lock:
            self._login_starts += 1

    def end_login_start(self) -> None:
        with self._lock:
            self._login_starts -= 1
            if self._login_starts == 0:
                for login_id, pending in self._pending_login_notifications.items():
                    self._discard_pending(pending)
                    self._terminal_logins.discard(login_id)
                self._pending_login_notifications.clear()

    def next_login_notification(
        self, login_id: str, timeout_s: float | None = None
    ) -> Notification:
        """Block until the next notification for a registered login attempt."""
        return self.login_notification_queue(login_id).get(timeout=timeout_s)

    def login_notification_queue(self, login_id: str) -> _NotificationQueue:
        with self._lock:
            login_queue = self._login_notifications.get(login_id)
            if login_queue is None:
                login_queue = self._detached_logins.get(login_id)
            if (
                self._failure is not None
                and login_id not in self._terminal_logins
                and login_queue not in self._completed_detached_logins
            ):
                raise self._failure
        if login_queue is None:
            raise RuntimeError(f"login {login_id!r} is not registered for waiting")
        return login_queue

    def register_turn(self, turn_id: str, *, deadline: float | None = None) -> None:
        """Register a queue for a turn stream and replay early events."""

        turn_queue = self._notification_queue(operation_deadline=False, deadline=deadline)
        if deadline is None and self._turn_timeout_s is not None:
            turn_queue.deadline = time.monotonic() + self._turn_timeout_s
        with self._lock:
            if self._failure is not None and turn_id not in self._terminal_turns:
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
            if self._failure is not None:
                turn_queue.fail(self._failure, drain=True)

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
        return self.turn_notification_queue(turn_id).get(timeout=timeout_s)

    def turn_notification_queue(self, turn_id: str) -> _NotificationQueue:
        with self._lock:
            if self._failure is not None and turn_id not in self._terminal_turns:
                raise self._failure
            turn_queue = self._turn_notifications.get(turn_id)
        if turn_queue is None:
            raise RuntimeError(f"turn {turn_id!r} is not registered for streaming")
        return turn_queue

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
            self._control_waiters.discard(str(request_id))
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

        while True:
            try:
                self._route_notification(notification)
                return
            except _NotificationBufferFull as exc:
                # Only storage pressure is recoverable here. Decode, lifecycle
                # and route-admission errors still fail the transport closed.
                with self._lock:
                    if self._failure is not None:
                        return
                    victim = None if exc.oversized else self._largest_backlog(exc.item_limit)
                    if victim is None:
                        victim = self._notification_route(notification)
                    # Do not retain a producer traceback (and its decoded frame)
                    # in every failed route outside the notification byte budget.
                    self._fail_notification_route(
                        *victim, CodexError("notification buffer limit exceeded; route failed")
                    )
                    # A single oversized event cannot fit even an empty budget.
                    # Its own stream is now explicitly failed, never completed.
                    if exc.oversized:
                        return

    def _notification_route(self, notification: Notification) -> tuple[str, str]:
        login_id = self._notification_login_id(notification)
        if login_id is not None:
            return "login", login_id
        thread_id = self._notification_thread_id(notification)
        turn_id = self._notification_turn_id(notification)
        if thread_id is not None and (state := self._goal_operations.get(thread_id)) is not None:
            if notification.method.startswith("thread/goal/") or (
                turn_id is not None and state._turn_routing_active
            ):
                return "goal", thread_id
        return ("turn", turn_id) if turn_id is not None else ("global", "")

    def _largest_backlog(self, items: bool) -> tuple[str, str | _NotificationQueue] | None:
        """Choose the pressure owner under the router lock, not the new sender."""
        candidates: list[tuple[int, str, str | _NotificationQueue]] = []
        with self._notification_queues_lock:
            unrouted = set(self._notification_queues)
        for kind, routes in (
            ("turn", self._turn_notifications),
            ("login", self._login_notifications),
            ("login", self._detached_logins),
        ):
            for key, notifications in list(routes.items()):
                candidates.append((notifications.buffered_size(items=items), kind, key))
                unrouted.discard(notifications)
        for kind, routes in (
            ("turn", self._pending_turn_notifications),
            ("login", self._pending_login_notifications),
        ):
            for key, pending in routes.items():
                pressure = len(pending) if items else sum(size for _, size in pending)
                candidates.append((pressure, kind, key))
        for key, state in self._goal_operations.items():
            candidates.append((state._notifications.buffered_size(items=items), "goal", key))
            unrouted.discard(state._notifications)
        unrouted.discard(self._global_notifications)
        for notifications in unrouted:
            candidates.append((
                notifications.buffered_size(items=items), "detached", notifications
            ))
        candidates.append((self._global_notifications.buffered_size(items=items), "global", ""))
        pressure, kind, key = max(candidates, key=lambda candidate: candidate[0])
        return (kind, key) if pressure else None

    def _fail_notification_route(
        self, kind: str, key: str | _NotificationQueue, exc: BaseException
    ) -> None:
        """Release a congested route's budget and publish its sticky failure."""
        if isinstance(key, _NotificationQueue):
            key.fail(exc)
        elif kind == "global":
            self._global_notifications.fail(exc)
        elif kind == "goal":
            self._goal_operations[key].fail(exc)
        elif kind == "turn":
            self._discard_pending(self._pending_turn_notifications.pop(key, deque()))
            notifications = self._turn_notifications.get(key)
            if notifications is None:
                self._check_route_capacity("turn", key)
                notifications = self._notification_queue(operation_deadline=False)
                self._turn_notifications[key] = notifications
            self._terminal_turns.discard(key)
            notifications.fail(exc)
        else:
            self._discard_pending(self._pending_login_notifications.pop(key, deque()))
            notifications = self._login_notifications.get(key) or self._detached_logins.get(key)
            if notifications is None:
                self._check_route_capacity("login", key)
                notifications = self._notification_queue(operation_deadline=False)
                self._login_notifications[key] = notifications
            self._terminal_logins.discard(key)
            self._completed_detached_logins.discard(notifications)
            notifications.fail(exc)

    def _route_notification(self, notification: Notification) -> None:
        with self._lock:
            if self._failure is not None:
                return

        login_id = self._notification_login_id(notification)
        if login_id is not None:
            with self._lock:
                if self._failure is not None:
                    return
                login_queue = self._login_notifications.get(login_id)
                detached = self._detached_logins.get(login_id)
                if login_queue is None:
                    login_queue = detached
                if login_queue is None:
                    # Unknown IDs can be early only while login/start is in flight.
                    # A late completion for a discarded attempt must not recreate a route.
                    if self._login_starts == 0:
                        return
                    if login_id not in self._pending_login_notifications:
                        self._check_route_capacity("login", login_id)
                    size = self._budget.reserve(notification)
                    self._pending_login_notifications.setdefault(login_id, deque()).append(
                        (notification, size)
                    )
                else:
                    if not login_queue.put(notification):
                        return
                # Only validated completions admitted to the byte budget are terminal.
                if isinstance(notification.payload, AccountLoginCompletedNotification):
                    if login_queue is not None:
                        login_queue.deadline = None
                    if detached is not None:
                        self._completed_detached_logins.add(detached)
                    else:
                        self._terminal_logins.add(login_id)
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
            turn_queue = self._turn_notifications.get(turn_id)
            if turn_queue is None:
                if turn_id not in self._pending_turn_notifications:
                    self._check_route_capacity("turn", turn_id)
                size = self._budget.reserve(notification)
                self._pending_turn_notifications.setdefault(turn_id, deque()).append(
                    (notification, size)
                )
            else:
                if not turn_queue.put(notification):
                    return
            # Mark terminal only after successful bounded retention, atomically
            # with failure. Overflow must never certify a missing completion.
            if notification.method == "turn/completed":
                self._terminal_turns.add(turn_id)

    def fail_all(self, exc: BaseException, *, discard_completed: bool = False) -> None:
        """Wake every blocked waiter when the reader thread exits."""

        with self._lock:
            if self._failure is not None and not discard_completed:
                return
            self._failure = exc
            response_waiters = list(self._response_waiters.values())
            self._response_waiters.clear()
            self._control_waiters.clear()
            retained_logins = set() if discard_completed else self._terminal_logins
            login_queues = [
                (notifications, login_id in retained_logins)
                for login_id, notifications in self._login_notifications.items()
            ]
            login_queues.extend(
                (
                    notifications,
                    not discard_completed and notifications in self._completed_detached_logins,
                )
                for notifications in list(self._detached_logins.values())
            )
            if discard_completed:
                self._detached_logins.clear()
                self._completed_detached_logins.clear()
                self._recent_cancelled_logins.clear()
            self._login_notifications = {
                login_id: notifications
                for login_id, notifications in self._login_notifications.items()
                if login_id in retained_logins
            }
            for login_id in list(self._pending_login_notifications):
                if login_id not in retained_logins:
                    self._discard_pending(self._pending_login_notifications.pop(login_id))
            if discard_completed:
                self._terminal_logins.clear()
            retained = set() if discard_completed else self._terminal_turns
            turn_queues = [
                (notifications, turn_id in retained)
                for turn_id, notifications in self._turn_notifications.items()
            ]
            self._turn_notifications = {
                turn_id: notifications
                for turn_id, notifications in self._turn_notifications.items()
                if turn_id in retained
            }
            for turn_id in list(self._pending_turn_notifications):
                if turn_id not in retained:
                    self._discard_pending(self._pending_turn_notifications.pop(turn_id))
            self._abandoned_turns.clear()
            if discard_completed:
                self._terminal_turns.clear()
            goal_operations = list(self._goal_operations.values())
            self._goal_operations.clear()
        # Put the same transport failure into every queue so no SDK call blocks
        # forever waiting for a response that cannot arrive.
        for waiter in response_waiters:
            waiter.put(exc)
        for login_queue, drain in login_queues:
            login_queue.fail(exc, drain=drain)
        for turn_queue, drain in turn_queues:
            turn_queue.fail(exc, drain=drain)
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
