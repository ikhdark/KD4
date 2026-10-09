"""Bounded notification storage with nonblocking producers and sticky failure."""

from __future__ import annotations

import json
import queue
import threading
import time
from collections import deque

from .errors import CodexError
from .models import Notification, UnknownNotification


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
        size = len(json.dumps({"method": notification.method, "params": params}).encode("utf-8"))
        with self._lock:
            if self.items >= self.max_items or self.bytes + size > self.max_bytes:
                raise CodexError("notification buffer limit exceeded")
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
        self.timeout_s = timeout_s
        self.deadline = time.monotonic() + timeout_s if operation_deadline else None

    def put(self, item: Notification | BaseException) -> None:
        if isinstance(item, BaseException):
            self.fail(item)
            return
        with self._condition:
            if self._failure is not None:
                return  # A route may be unregistered concurrently with delivery.
            size = self._budget.reserve(item)
            self._items.append((item, size))
            self._condition.notify()

    def put_reserved(self, item: Notification, size: int) -> None:
        """Transfer an already charged early event without releasing its budget."""
        with self._condition:
            self._items.append((item, size))
            self._condition.notify()

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
                if self._failure is not None:
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

    def fail(self, exc: BaseException) -> None:
        with self._condition:
            if self._failure is not None:
                return
            self._failure = exc
            while self._items:
                _, size = self._items.popleft()
                self._budget.release(size)
            self._condition.notify_all()
