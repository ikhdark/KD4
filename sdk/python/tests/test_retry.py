from __future__ import annotations

import pytest

from openai_codex.errors import (
    CodexRpcError,
    JsonRpcError,
    ServerBusyError,
    is_retryable_error,
    map_jsonrpc_error,
)
from openai_codex.retry import retry_on_overload


@pytest.mark.parametrize(
    ("keyword", "value"),
    [
        ("initial_delay_s", -0.1),
        ("max_delay_s", -0.1),
        ("jitter_ratio", -0.1),
        ("max_attempts", 0),
    ],
)
def test_retry_rejects_negative_delay_configuration(keyword: str, value: float) -> None:
    def must_not_run() -> None:
        pytest.fail("invalid retry configuration reached the operation")

    minimum = 1 if keyword == "max_attempts" else 0
    with pytest.raises(ValueError, match=f"{keyword} must be >= {minimum}"):
        retry_on_overload(must_not_run, **{keyword: value})


def test_retry_clamps_jittered_delay_to_maximum(monkeypatch: pytest.MonkeyPatch) -> None:
    attempts = 0
    sleeps: list[float] = []

    def overloaded_once() -> str:
        nonlocal attempts
        attempts += 1
        if attempts == 1:
            raise ServerBusyError(-32001, "busy")
        return "ok"

    monkeypatch.setattr("openai_codex.retry.random.uniform", lambda _low, high: high)
    monkeypatch.setattr("openai_codex.retry.time.sleep", sleeps.append)

    assert (
        retry_on_overload(
            overloaded_once,
            initial_delay_s=10,
            max_delay_s=1,
            jitter_ratio=100,
        )
        == "ok"
    )
    assert sleeps == [1]


def test_retry_handles_structured_overload_error(monkeypatch: pytest.MonkeyPatch) -> None:
    error = map_jsonrpc_error(
        -32001,
        "request queue is full",
        {"reason": "serializedRequestQueue", "retryable": True},
    )
    assert isinstance(error, ServerBusyError)
    assert is_retryable_error(error)

    attempts = 0
    sleeps: list[float] = []

    def overloaded_three_times() -> str:
        nonlocal attempts
        attempts += 1
        if attempts <= 3:
            raise error
        return "ok"

    monkeypatch.setattr("openai_codex.retry.time.sleep", sleeps.append)

    assert (
        retry_on_overload(
            overloaded_three_times,
            max_attempts=4,
            initial_delay_s=0.25,
            max_delay_s=0.6,
            jitter_ratio=0,
        )
        == "ok"
    )
    assert attempts == 4
    assert sleeps == [0.25, 0.5, 0.6]


@pytest.mark.parametrize("max_attempts", [1, 3])
def test_retry_stops_at_attempt_limit(monkeypatch: pytest.MonkeyPatch, max_attempts: int) -> None:
    error = ServerBusyError(-32001, "still busy")
    attempts = 0
    sleeps: list[float] = []

    def overloaded() -> None:
        nonlocal attempts
        attempts += 1
        raise error

    monkeypatch.setattr("openai_codex.retry.time.sleep", sleeps.append)
    with pytest.raises(ServerBusyError) as caught:
        retry_on_overload(
            overloaded, max_attempts=max_attempts, initial_delay_s=0.25, jitter_ratio=0
        )
    assert caught.value is error
    assert attempts == max_attempts
    assert sleeps == ([] if max_attempts == 1 else [0.25, 0.5])


@pytest.mark.parametrize("completed_at", [0.5, 1.0, 1.5])
def test_retry_rejects_results_at_or_after_deadline(
    monkeypatch: pytest.MonkeyPatch, completed_at: float
) -> None:
    # One wall-clock budget includes the callable, not only retry sleeps.
    clock = [0.0]
    result = object()
    attempts = 0

    def complete() -> object:
        nonlocal attempts
        attempts += 1
        clock[0] = completed_at
        return result

    monkeypatch.setattr("openai_codex.retry.time.monotonic", lambda: clock[0])
    monkeypatch.setattr("openai_codex.retry.time.sleep", lambda _: pytest.fail("unexpected retry"))
    if completed_at < 1.0:
        assert retry_on_overload(complete, timeout_s=1) is result
    else:
        with pytest.raises(TimeoutError, match="deadline"):
            retry_on_overload(complete, timeout_s=1)
    assert attempts == 1


@pytest.mark.parametrize(
    ("message", "extra_data"),
    [
        ("request queue rejected", {}),
        ("retry limit exceeded", {}),
        ("too many failed attempts", {}),
        ("request queue rejected", {"codexErrorInfo": "server_overloaded"}),
    ],
)
def test_structured_overload_honors_retryable_flag(
    monkeypatch: pytest.MonkeyPatch, message: str, extra_data: dict[str, str]
) -> None:
    # The protocol's explicit retry permission is authoritative; legacy text
    # and error-info hints must not turn a refusal into another attempt.
    error = map_jsonrpc_error(
        -32001,
        message,
        {"reason": "serializedRequestQueue", "retryable": False, **extra_data},
    )

    assert type(error) is CodexRpcError
    assert not is_retryable_error(error)
    assert not is_retryable_error(JsonRpcError(error.code, error.message, error.data))

    attempts = 0

    def rejected() -> None:
        nonlocal attempts
        attempts += 1
        raise error

    monkeypatch.setattr("openai_codex.retry.time.sleep", lambda _: pytest.fail("unexpected retry"))
    with pytest.raises(CodexRpcError) as caught:
        retry_on_overload(rejected)
    assert caught.value is error
    assert attempts == 1
