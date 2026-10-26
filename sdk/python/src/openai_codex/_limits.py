"""Bounded notification storage with nonblocking producers and sticky failure."""

from __future__ import annotations

import asyncio
import json
import queue
import threading
import time
from collections import deque

from .errors import CodexError
from .models import Notification, UnknownNotification


class _NotificationBufferFull(CodexError):
    def __init__(self, *, item_limit: bool = False, oversized: bool = False) -> None:
        super().__init__("notification buffer limit exceeded")
        self.item_limit = item_limit
        self.oversized = oversized


class _BufferBudget:
    def __init__(self, max_items: int = 4096, max_bytes: int = 64 * 1024 * 1024) -> None:
        self.max_items = max_items
        self.max_bytes = max_bytes
        self.items = 0
        self.bytes = 0
        self._lock = threading.Lock()

    def reserve(self, notification: Notification) -> int:
        payload = notification.payload
        params = (
            payload.params
            if isinstance(payload, UnknownNotification)
            else payload.model_dump(mode="json", warnings=False)
        )
        # Default ensure_ascii=True makes character and UTF-8 byte counts equal.
        size = len(json.dumps({"method": notification.method, "params": params}))
        with self._lock:
            if size > self.max_bytes:
                raise _NotificationBufferFull(oversized=True)
            if self.items >= self.max_items or self.bytes + size > self.max_bytes:
                raise _NotificationBufferFull(item_limit=self.items >= self.max_items)
            self.items += 1
            self.bytes += size
        return size

    def release(self, size: int) -> None:
        with self._lock:
            self.items -= 1
            self.bytes -= size


class _NotificationQueue:
    def __init__(
        self,
        budget: _BufferBudget | None = None,
        timeout_s: float = 300.0,
        *,
        operation_deadline: bool = False,
    ) -> None:
        self._budget = budget if budget is not None else _BufferBudget()
        self._items: deque[tuple[Notification, int]] = deque()
        self._condition = threading.Condition()
        self._failure: BaseException | None = None
        self._async_waiters: set[tuple[asyncio.AbstractEventLoop, asyncio.Future[None]]] = set()
        self.timeout_s = timeout_s
        self.deadline = time.monotonic() + timeout_s if operation_deadline else None

    def put(self, item: Notification | BaseException) -> bool:
        if isinstance(item, BaseException):
            self.fail(item)
            return False
        with self._condition:
            if self._failure is not None:
                return False  # A route may be unregistered concurrently with delivery.
            size = self._budget.reserve(item)
            self._items.append((item, size))
            self._condition.notify()
            self._notify_async_locked()
            return True

    def buffered_size(self, *, items: bool = False) -> int:
        """Return retained pressure for router-owned overflow isolation."""
        with self._condition:
            return len(self._items) if items else sum(size for _, size in self._items)

    def put_reserved(self, item: Notification, size: int) -> None:
        """Transfer an already charged early event without releasing its budget."""
        with self._condition:
            self._items.append((item, size))
            self._condition.notify()
            self._notify_async_locked()

    def __del__(self) -> None:
        # A completed goal may outlive its router, then be abandoned by its caller.
        for _, size in self._items:
            self._budget.release(size)

    def get(self, timeout: float | None = None) -> Notification:
        if timeout is not None and timeout < 0:
            raise ValueError("timeout must be nonnegative")
        now = time.monotonic()
        deadline = now + (self.timeout_s if timeout is None else timeout)
        with self._condition:
            while True:
                if self._failure is not None and not self._items:
                    raise self._failure
                now = time.monotonic()
                if self.deadline is not None and now >= self.deadline:
                    raise TimeoutError("notification operation deadline exceeded")
                if self._items:
                    item, size = self._items.popleft()
                    self._budget.release(size)
                    return item
                remaining = deadline - now
                if remaining <= 0:
                    if timeout is None:
                        raise TimeoutError("notification wait deadline exceeded")
                    raise queue.Empty
                if self.deadline is not None:
                    remaining = min(remaining, self.deadline - now)
                self._condition.wait(remaining)

    @staticmethod
    def _complete_async_waiter(waiter: asyncio.Future[None]) -> None:
        if not waiter.done():
            waiter.set_result(None)

    def _notify_async_locked(self) -> None:
        # Detach before scheduling so a burst coalesces to one wake per waiter.
        waiters, self._async_waiters = self._async_waiters, set()
        for loop, waiter in waiters:
            try:
                loop.call_soon_threadsafe(self._complete_async_waiter, waiter)
            except RuntimeError:
                pass  # Its event loop has already closed.

    async def get_async(self) -> Notification:
        """Wait on publication without polling or retaining a blocked OS thread."""
        loop = asyncio.get_running_loop()
        deadline = time.monotonic() + self.timeout_s
        while True:
            with self._condition:
                try:
                    return self.get(0.0)
                except queue.Empty:
                    pass
                remaining = deadline - time.monotonic()
                if self.deadline is not None:
                    remaining = min(remaining, self.deadline - time.monotonic())
                if remaining <= 0:
                    raise TimeoutError("notification wait deadline exceeded")
                waiter = loop.create_future()
                registration = (loop, waiter)
                # Empty-check and subscription share the producer's lock.
                self._async_waiters.add(registration)
            try:
                await asyncio.wait_for(waiter, remaining)
            except asyncio.TimeoutError:
                pass  # Recheck sticky failure, queued data and route deadline.
            finally:
                with self._condition:
                    self._async_waiters.discard(registration)

    def fail(self, exc: BaseException, *, drain: bool = False) -> None:
        with self._condition:
            if self._failure is None:
                self._failure = exc
            while self._items and not drain:
                _, size = self._items.popleft()
                self._budget.release(size)
            self._condition.notify_all()
            self._notify_async_locked()
