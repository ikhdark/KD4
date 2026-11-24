"""Explicit, caller-owned inference admission for asynchronous SDK workflows."""

from __future__ import annotations

import asyncio
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
