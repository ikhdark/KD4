from __future__ import annotations

from dataclasses import dataclass
from typing import AsyncIterator, Iterator

from .generated.v2_all import (
    AgentMessageThreadItem,
    ItemCompletedNotification,
    MessagePhase,
    SurfacedToolResult,
    ThreadItem,
    ThreadTokenUsage,
    ThreadTokenUsageUpdatedNotification,
    Turn,
    TurnCompletedNotification,
    TurnError,
    TurnStatus,
    TurnTiming,
)
from .models import Notification


@dataclass(slots=True)
class TurnResult:
    """Collected result returned after a turn completes."""

    id: str
    status: TurnStatus
    error: TurnError | None
    started_at: int | None
    completed_at: int | None
    duration_ms: int | None
    final_response: str | None
    items: list[ThreadItem]
    usage: ThreadTokenUsage | None
    surfaced_result: SurfacedToolResult | None = None
    # Terminal host diagnostics; absence is not evidence of verified completion.
    timing: TurnTiming | None = None


def _agent_message_item_from_thread_item(
    item: ThreadItem,
) -> AgentMessageThreadItem | None:
    thread_item = item.root
    if isinstance(thread_item, AgentMessageThreadItem):
        return thread_item
    return None


def _final_assistant_response_from_items(items: list[ThreadItem]) -> str | None:
    last_unknown_phase_response: str | None = None

    for item in reversed(items):
        agent_message = _agent_message_item_from_thread_item(item)
        if agent_message is None:
            continue
        if agent_message.phase == MessagePhase.final_answer:
            return agent_message.text
        if agent_message.phase is None and last_unknown_phase_response is None:
            last_unknown_phase_response = agent_message.text

    return last_unknown_phase_response


class TurnRunError(RuntimeError):
    """A failed/incomplete turn; earlier tool effects may already be committed.

    Observations are retained without fetching history or replaying operations.
    ``terminal`` and ``result`` are absent when completion was not observed.
    """

    def __init__(
        self,
        message: str,
        *,
        turn_id: str,
        terminal: Turn | None = None,
        result: TurnResult | None = None,
        items: list[ThreadItem] | None = None,
        usage: ThreadTokenUsage | None = None,
        thread_id: str | None = None,
    ):
        super().__init__(message)
        self.turn_id = turn_id
        self.thread_id = thread_id
        self.terminal = terminal
        self.result = result
        self.items = result.items if result is not None else (items if items is not None else [])
        self.usage = result.usage if result is not None else usage
        self.error = terminal.error if terminal is not None else None
        self.final_response = (
            result.final_response
            if result is not None
            else _final_assistant_response_from_items(self.items)
        )
        self.surfaced_result = result.surfaced_result if result is not None else None


def _raise_for_failed_turn(
    turn: Turn, *, result: TurnResult | None = None, thread_id: str | None = None
) -> None:
    if turn.status != TurnStatus.failed:
        return
    if turn.error is not None and turn.error.message:
        message = turn.error.message
    else:
        message = f"turn failed with status {turn.status.value}"
    raise TurnRunError(message, turn_id=turn.id, terminal=turn, result=result, thread_id=thread_id)


def _collect_turn_result(stream: Iterator[Notification], *, turn_id: str) -> TurnResult:
    completed: TurnCompletedNotification | None = None
    items: list[ThreadItem] = []
    usage: ThreadTokenUsage | None = None
    thread_id: str | None = None

    try:
        for event in stream:
            payload = event.payload
            if isinstance(payload, ItemCompletedNotification) and payload.turn_id == turn_id:
                thread_id = payload.thread_id
                items.append(payload.item)
            elif (
                isinstance(payload, ThreadTokenUsageUpdatedNotification)
                and payload.turn_id == turn_id
            ):
                thread_id = payload.thread_id
                usage = payload.token_usage
            elif isinstance(payload, TurnCompletedNotification) and payload.turn.id == turn_id:
                completed = payload
    except Exception as error:
        raise TurnRunError(
            str(error),
            turn_id=turn_id,
            terminal=completed.turn if completed else None,
            result=_turn_result(completed, items, usage) if completed else None,
            items=items,
            usage=usage,
            thread_id=completed.thread_id if completed else thread_id,
        ) from error

    return _finish_turn_result(completed, items, usage, turn_id=turn_id, thread_id=thread_id)


def _turn_result(
    completed: TurnCompletedNotification, items: list[ThreadItem], usage: ThreadTokenUsage | None
) -> TurnResult:
    turn = completed.turn
    surfaced_result = completed.surfaced_result or turn.surfaced_result
    return TurnResult(
        id=turn.id,
        status=turn.status,
        error=turn.error,
        started_at=turn.started_at,
        completed_at=turn.completed_at,
        duration_ms=turn.duration_ms,
        final_response=(
            surfaced_result.canonical_message
            if surfaced_result is not None
            else _final_assistant_response_from_items(items)
        ),
        items=items,
        usage=usage,
        surfaced_result=surfaced_result,
        timing=completed.timing if completed.timing is not None else turn.timing,
    )


def _finish_turn_result(
    completed: TurnCompletedNotification | None,
    items: list[ThreadItem],
    usage: ThreadTokenUsage | None,
    *,
    turn_id: str,
    thread_id: str | None = None,
) -> TurnResult:
    if completed is None:
        raise TurnRunError(
            "turn completed event not received",
            turn_id=turn_id,
            items=items,
            usage=usage,
            thread_id=thread_id,
        )
    result = _turn_result(completed, items, usage)
    _raise_for_failed_turn(completed.turn, result=result, thread_id=completed.thread_id)
    return result


async def _collect_async_turn_result(
    stream: AsyncIterator[Notification], *, turn_id: str
) -> TurnResult:
    completed: TurnCompletedNotification | None = None
    items: list[ThreadItem] = []
    usage: ThreadTokenUsage | None = None
    thread_id: str | None = None

    try:
        async for event in stream:
            payload = event.payload
            if isinstance(payload, ItemCompletedNotification) and payload.turn_id == turn_id:
                thread_id = payload.thread_id
                items.append(payload.item)
            elif (
                isinstance(payload, ThreadTokenUsageUpdatedNotification)
                and payload.turn_id == turn_id
            ):
                thread_id = payload.thread_id
                usage = payload.token_usage
            elif isinstance(payload, TurnCompletedNotification) and payload.turn.id == turn_id:
                completed = payload
    except Exception as error:
        raise TurnRunError(
            str(error),
            turn_id=turn_id,
            terminal=completed.turn if completed else None,
            result=_turn_result(completed, items, usage) if completed else None,
            items=items,
            usage=usage,
            thread_id=completed.thread_id if completed else thread_id,
        ) from error

    return _finish_turn_result(completed, items, usage, turn_id=turn_id, thread_id=thread_id)
