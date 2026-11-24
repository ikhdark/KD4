"""Explicit, caller-owned inference admission for asynchronous SDK workflows."""

from __future__ import annotations

import asyncio
import math
from collections.abc import Awaitable, Callable, Sequence
from typing import Any, TypeVar

_Input = TypeVar("_Input")
_Result = TypeVar("_Result")


class InferenceSlotsExhausted(RuntimeError):
    """A caller's local submission allowance cannot admit the selected phase."""


class InferenceWorkflow:
    """Gate asynchronous turn starts on declared evidence and a local allowance.

    Use one instance from one event loop. Slots count calls to the supplied
    submitter, not provider requests, transport retries, tokens, or charges.
    """

    def __init__(self, *, slots: int | None = None, reserve: int = 0) -> None:
        if slots is not None and (type(slots) is not int or slots < 0):
            raise ValueError("slots must be a nonnegative integer or None")
        if type(reserve) is not int or reserve < 0 or reserve > (slots or 0):
            raise ValueError("reserve must be a nonnegative integer within a finite allowance")
        self._remaining = slots
        self._reserve = reserve

    @property
    def remaining(self) -> int | None:
        """The unspent local allowance, or None when unbounded."""
        return self._remaining

    async def run_phase(
        self,
        prepare: Callable[[tuple[Any, ...]], _Input | None],
        submit: Callable[[_Input], Awaitable[_Result]],
        *,
        ready: Sequence[asyncio.Future[Any]] = (),
        essential: bool = False,
    ) -> _Result | None:
        """Prepare and submit one phase after its exact caller-owned prerequisites.

        A prepare decision of None skips submission without spending a slot.
        An essential phase can use a reserved slot. Failed or interrupted
        submissions retain their debit; admission might have reached the peer.
        Cancelling the wait does not cancel the prerequisite futures.
        """
        dependencies = tuple(ready)
        results = tuple(await asyncio.gather(*(asyncio.shield(dep) for dep in dependencies))) if dependencies else ()
        request = prepare(results)
        if request is None:
            return None
        minimum = 0 if essential else self._reserve
        if self._remaining is not None:
            if self._remaining <= minimum:
                raise InferenceSlotsExhausted("no local submission slot available for this phase")
            # No await between checking and charging. A caller must not retry
            # an indeterminate admission merely because its await failed.
            self._remaining -= 1
        return await submit(request)


def prepare_keyed_evidence(
    rows: list[dict[str, str]],
    required_keys: frozenset[str],
    *,
    fields: frozenset[str],
    key_field: str,
    render: Callable[[list[dict[str, str]]], str],
) -> str:
    """Render complete evidence for an explicit, independently keyed assignment.

    The caller supplies a complete table and a pure renderer that changes
    only the evidence rows. All rows for every required key are preserved in
    order. Unknown shape, missing keys or less than 20% UTF-8 byte savings
    leave the ordinary request unchanged. The helper neither submits work
    nor infers which keys are required.
    """
    ordinary = render(rows)
    if (
        type(rows) is not list
        or not required_keys
        or key_field not in fields
        or any(
            type(row) is not dict
            or row.keys() != fields
            or any(type(value) is not str for value in row.values())
            for row in rows
        )
    ):
        return ordinary

    selected = [row for row in rows if row[key_field] in required_keys]
    if len(selected) == len(rows) or {row[key_field] for row in selected} != required_keys:
        return ordinary

    projected = render(selected)
    return (
        projected
        if ordinary and len(projected.encode("utf-8")) * 5 <= len(ordinary.encode("utf-8")) * 4
        else ordinary
    )


def exact_keyed_values(
    rows: list[dict[str, str]],
    required_keys: tuple[str, ...],
    *,
    key_field: str,
    value_field: str,
) -> dict[str, str] | None:
    """Resolve an explicit lookup in a complete two-column string table.

    Return values in assignment order only when each requested key has exactly
    one row. Return None on unknown shape, missing or ambiguous keys, or an
    invalid assignment. The caller owns table completeness and fallback.
    This helper does not infer intent or submit a turn.
    """
    if (
        type(rows) is not list
        or type(required_keys) is not tuple
        or not required_keys
        or any(type(key) is not str or not key for key in required_keys)
        or len(set(required_keys)) != len(required_keys)
        or type(key_field) is not str
        or type(value_field) is not str
        or not key_field
        or not value_field
        or key_field == value_field
    ):
        return None

    fields = {key_field, value_field}
    wanted = set(required_keys)
    candidates: dict[str, str] = {}
    for row in rows:
        if (
            type(row) is not dict
            or row.keys() != fields
            or type(row[key_field]) is not str
            or not row[key_field]
            or type(row[value_field]) is not str
        ):
            return None
        key = row[key_field]
        if key not in wanted:
            continue
        if key in candidates:
            return None
        candidates[key] = row[value_field]
    if candidates.keys() != wanted:
        return None
    return {key: candidates[key] for key in required_keys}


async def latest_in_window(source: asyncio.Queue[_Input], *, window: float) -> _Input:
    """Consume the first event plus its bounded successors; return the latest.

    Use a dedicated, single-consumer queue of replaceable events, not distinct
    requests or durable work. A later event does not extend the window. The
    caller owns the queue and any urgent or independent submissions.
    """
    if not math.isfinite(window) or window <= 0:
        raise ValueError("window must be finite and positive")
    async def take() -> _Input:
        event = await source.get()
        source.task_done()
        return event

    latest = await take()
    loop = asyncio.get_running_loop()
    deadline = loop.time() + window
    while (remaining := deadline - loop.time()) > 0:
        receiver = asyncio.create_task(take())
        try:
            done, _ = await asyncio.wait((receiver,), timeout=remaining)
            if not done:
                break
            latest = receiver.result()
        finally:
            if not receiver.done():
                receiver.cancel()
                await asyncio.gather(receiver, return_exceptions=True)
    return latest
