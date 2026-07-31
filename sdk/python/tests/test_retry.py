from __future__ import annotations

import pytest

from openai_codex.errors import ServerBusyError
from openai_codex.retry import retry_on_overload


@pytest.mark.parametrize(
    ("keyword", "value"),
    [
        ("initial_delay_s", -0.1),
        ("max_delay_s", -0.1),
        ("jitter_ratio", -0.1),
    ],
)
def test_retry_rejects_negative_delay_configuration(keyword: str, value: float) -> None:
    with pytest.raises(ValueError):
        retry_on_overload(lambda: None, **{keyword: value})


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
