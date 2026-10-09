from __future__ import annotations

import math
import random
import threading
import time
from typing import Callable, TypeVar

from .errors import is_retryable_error

T = TypeVar("T")


def retry_on_overload(
    op: Callable[[], T],
    *,
    max_attempts: int = 3,
    initial_delay_s: float = 0.25,
    max_delay_s: float = 2.0,
    jitter_ratio: float = 0.2,
    timeout_s: float = 300.0,
    _cancelled: threading.Event | None = None,
) -> T:
    """Retry overload errors within one wall-clock budget.

    SDK operations enforce this budget during I/O; arbitrary callables must
    themselves be interruptible. A late callable result is never accepted.
    """

    if not isinstance(max_attempts, int) or isinstance(max_attempts, bool):
        raise ValueError("max_attempts must be a finite integer")
    if max_attempts < 1:
        raise ValueError("max_attempts must be >= 1")
    for name, value in (
        ("initial_delay_s", initial_delay_s),
        ("max_delay_s", max_delay_s),
        ("jitter_ratio", jitter_ratio),
        ("timeout_s", timeout_s),
    ):
        if (
            not isinstance(value, (int, float))
            or isinstance(value, bool)
            or not math.isfinite(value)
        ):
            raise ValueError(f"{name} must be finite")
    if initial_delay_s < 0:
        raise ValueError("initial_delay_s must be >= 0")
    if max_delay_s < 0:
        raise ValueError("max_delay_s must be >= 0")
    if jitter_ratio < 0:
        raise ValueError("jitter_ratio must be >= 0")
    if timeout_s <= 0:
        raise ValueError("timeout_s must be > 0")

    deadline = time.monotonic() + timeout_s

    def remaining() -> float:
        if _cancelled is not None and _cancelled.is_set():
            raise TimeoutError("retry operation cancelled")
        budget = deadline - time.monotonic()
        if budget <= 0:
            raise TimeoutError("retry operation deadline exceeded")
        return budget

    delay = initial_delay_s
    attempt = 0
    while True:
        remaining()
        attempt += 1
        try:
            result = op()
            remaining()
            return result
        except Exception as exc:
            if attempt >= max_attempts:
                raise
            if not is_retryable_error(exc):
                raise

            base_delay = min(max_delay_s, delay)
            # Clamp the factor first: finite inputs must not overflow when multiplied.
            jitter = base_delay * min(jitter_ratio, max_delay_s / base_delay) if base_delay else 0.0
            sleep_for = min(
                max_delay_s,
                max(0.0, base_delay + random.uniform(-1.0, 1.0) * jitter),
            )
            if sleep_for > 0:
                sleep_for = min(sleep_for, remaining())
                if _cancelled is None:
                    time.sleep(sleep_for)
                else:
                    _cancelled.wait(sleep_for)
            delay = min(max_delay_s, delay * 2)
