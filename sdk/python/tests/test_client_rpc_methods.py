from __future__ import annotations

import json
import queue
import sys
from pathlib import Path
from types import ModuleType

import pytest

from openai_codex.api import TurnHandle
from openai_codex.client import (
    CodexClient,
    CodexConfig,
)
from openai_codex.errors import CodexError, TransportClosedError
from openai_codex.generated.v2_all import (
    AccountUpdatedNotification,
    AgentMessageDeltaNotification,
    ApprovalsReviewer,
    ChatgptAccount,
    PlanType,
    ThreadListParams,
    ThreadResumeResponse,
    ThreadTokenUsageUpdatedNotification,
    TurnCompletedNotification,
    WarningNotification,
)
from openai_codex.models import UnknownNotification

_STDERR_TAIL_MAX_BYTES = 64 * 1024
_STDERR_TRUNCATION_MARKER = f"[stderr truncated; showing last {_STDERR_TAIL_MAX_BYTES} bytes]\n"
_WRITE_NEWLINE_FREE_STDERR = f"""
payload = b"discard-me" + chr(0x1F642).encode("utf-8") * ({_STDERR_TAIL_MAX_BYTES} // 4 + 2) + b"END"
sys.stderr.buffer.write(payload)
sys.stderr.buffer.flush()
"""


def _client_for_script(script: str) -> CodexClient:
    return CodexClient(
        CodexConfig(launch_args_override=(sys.executable, "-c", f"import sys\n{script}"))
    )


def test_over_cap_newline_free_stderr_is_drained_on_success() -> None:
    client = _client_for_script(
        _WRITE_NEWLINE_FREE_STDERR
        + 'sys.stdout.write(\'{"method":"diagnostic/noisy","params":{}}\\n\')\n'
        + "sys.stdout.flush()\n"
    )
    try:
        client.start()
        notification = client.next_notification(timeout_s=5)
        assert notification.method == "diagnostic/noisy"
    finally:
        client.close()


def test_over_cap_newline_free_stderr_is_reported_on_failure() -> None:
    client = _client_for_script(_WRITE_NEWLINE_FREE_STDERR + "import time\ntime.sleep(0.1)\n")
    try:
        client.start()
        with pytest.raises(TransportClosedError) as exc_info:
            client.next_notification(timeout_s=5)

        message = str(exc_info.value)
        assert _STDERR_TRUNCATION_MARKER in message
        assert message.endswith("END")
        assert "discard-me" not in message
        assert "\ufffd" not in message
    finally:
        client.close()


def test_start_rejects_runtime_missing_required_path_symbol(monkeypatch) -> None:
    runtime_module = ModuleType("codex_cli_bin")
    runtime_module.bundled_codex_path = lambda: Path(sys.executable)
    monkeypatch.setitem(sys.modules, "codex_cli_bin", runtime_module)

    client = CodexClient()
    try:
        with pytest.raises(ImportError, match="bundled_path_dir"):
            client.start()
    finally:
        client.close()


def test_account_and_notification_decode_the_same_public_plan_enum() -> None:
    account = ChatgptAccount.model_validate({"email": None, "planType": "pro", "type": "chatgpt"})
    update = AccountUpdatedNotification.model_validate({"planType": "pro"})
    assert account.plan_type is update.plan_type is PlanType.pro


def test_thread_resume_response_accepts_auto_review_reviewer() -> None:
    """Generated response models should keep accepting the auto review enum value."""
    response = ThreadResumeResponse.model_validate(
        {
            "approvalPolicy": "on-request",
            "approvalsReviewer": "auto_review",
            "cwd": "/tmp",
            "model": "gpt-5",
            "modelProvider": "openai",
            "sandbox": {"type": "dangerFullAccess"},
            "thread": {
                "cliVersion": "1.0.0",
                "createdAt": 1,
                "cwd": "/tmp",
                "ephemeral": False,
                "id": "thread-1",
                "modelProvider": "openai",
                "preview": "",
                # The pinned runtime schema requires the session id on threads.
                "sessionId": "session-1",
                "source": "cli",
                "status": {"type": "idle"},
                "turns": [],
                "updatedAt": 1,
            },
        }
    )

    assert response.approvals_reviewer is ApprovalsReviewer.auto_review


def test_turn_handle_close_discards_late_events_and_releases_completed_turn() -> None:
    client = _client_for_script("""
import json
sys.stdin.readline()
for message in [
    {"method": "item/agentMessage/delta", "params": {
        "delta": "ignored", "itemId": "item-1", "threadId": "thread-1", "turnId": "turn-1",
    }},
    {"method": "turn/completed", "params": {
        "threadId": "thread-1", "turn": {"id": "turn-1", "items": [], "status": "completed"},
    }},
    {"method": "test/drained", "params": {}},
]:
    print(json.dumps(message), flush=True)
sys.stdin.read()
""")
    try:
        client.start()
        client.register_turn_notifications("turn-1")
        handle = TurnHandle(client, "thread-1", "turn-1")
        handle.close()
        with pytest.raises(RuntimeError, match="abandoned"):
            client.register_turn_notifications("turn-1")
        client.notify("test/release")
        assert client.next_notification(timeout_s=5).method == "test/drained"
        client.register_turn_notifications("turn-1")
        with pytest.raises(queue.Empty):
            client.next_turn_notification("turn-1", timeout_s=0.01)
    finally:
        client.close()


def test_thread_list_serializes_public_params_on_the_wire() -> None:
    client = _client_for_script("""
import json
request = json.loads(sys.stdin.readline())
print(json.dumps({"method": "test/request", "params": request}), flush=True)
print(json.dumps({"id": request["id"], "result": {"data": [], "nextCursor": None}}), flush=True)
sys.stdin.read()
""")
    try:
        client.start()
        result = client.thread_list(ThreadListParams(search_term="needle", limit=5))
        request = client.next_notification(timeout_s=5)
        assert isinstance(request.payload, UnknownNotification)
        assert request.payload.params["method"] == "thread/list"
        assert request.payload.params["params"] == {"searchTerm": "needle", "limit": 5}
        assert result.data == []
        assert result.next_cursor is None
    finally:
        client.close()


@pytest.mark.parametrize("register_late", [False, True], ids=["registered", "buffered"])
def test_reader_routes_interleaved_typed_and_unknown_notifications(register_late) -> None:
    """Exercise decoding, routing, buffering and per-turn order through real pipes."""
    messages = [
        {
            "method": "item/agentMessage/delta",
            "params": {
                "delta": delta,
                "itemId": f"item-{index}",
                "threadId": "thread-1",
                "turnId": turn_id,
            },
        }
        for index, (turn_id, delta) in enumerate(
            [
                ("turn-1", "one-a"),
                ("turn-2", "two-a"),
                ("turn-1", "one-b"),
                ("turn-2", "two-b"),
            ]
        )
    ]
    messages.extend(
        [
            {"method": "unknown/direct", "params": {"turnId": "turn-1"}},
            {"method": "unknown/nested", "params": {"turn": {"id": "turn-2"}}},
            {
                "method": "turn/completed",
                "params": {
                    "threadId": "thread-1",
                    "turn": {"id": "turn-1", "items": [], "status": "completed"},
                },
            },
            {"method": "warning", "params": {"message": "heads up"}},
        ]
    )
    client = _client_for_script(
        "sys.stdin.readline()\n"
        + f"sys.stdout.write({''.join(json.dumps(message) + chr(10) for message in messages)!r})\n"
        + "sys.stdout.flush()\nsys.stdin.read()\n"
    )
    try:
        client.start()
        if not register_late:
            client.register_turn_notifications("turn-1")
            client.register_turn_notifications("turn-2")
        client.notify("test/release")
        # The unscoped warning follows every turn event, so receiving it also
        # proves that the late-registration case has actually buffered them.
        warning = client.next_notification(timeout_s=5)
        assert isinstance(warning.payload, WarningNotification)
        assert warning.payload.message == "heads up"
        if register_late:
            client.register_turn_notifications("turn-1")
            client.register_turn_notifications("turn-2")
        for turn_id, expected_deltas, unknown_method in [
            ("turn-1", ["one-a", "one-b"], "unknown/direct"),
            ("turn-2", ["two-a", "two-b"], "unknown/nested"),
        ]:
            for delta in expected_deltas:
                event = client.next_turn_notification(turn_id, timeout_s=5)
                assert isinstance(event.payload, AgentMessageDeltaNotification)
                assert (event.payload.turn_id, event.payload.delta) == (turn_id, delta)
            unknown = client.next_turn_notification(turn_id, timeout_s=5)
            assert unknown.method == unknown_method
            assert isinstance(unknown.payload, UnknownNotification)
        completion = client.next_turn_notification("turn-1", timeout_s=5)
        assert isinstance(completion.payload, TurnCompletedNotification)
        assert completion.payload.turn.id == "turn-1"
    finally:
        client.close()


def test_goal_notifications_arriving_on_stdout_route_by_thread() -> None:
    message = {
        "method": "item/agentMessage/delta",
        "params": {
            "delta": "continued",
            "itemId": "item-1",
            "threadId": "thread-1",
            "turnId": "turn-2",
        },
    }
    client = _client_for_script(
        "sys.stdin.readline()\n" + f"print({json.dumps(message)!r}, flush=True)\nsys.stdin.read()\n"
    )
    try:
        client.start()
        state = client.register_goal_operation("thread-1")
        client.notify("test/release")
        event = client.next_goal_notification(state, timeout_s=5)
        assert isinstance(event.payload, AgentMessageDeltaNotification)
        assert (event.payload.turn_id, event.payload.delta) == ("turn-2", "continued")
    finally:
        client.close()


@pytest.mark.parametrize("method", ["thread/tokenUsage/updated", "turn/completed"])
def test_invalid_wire_notification_fails_current_and_late_waiters(method) -> None:
    message = {"method": method, "params": {"threadId": "missing"}}
    client = _client_for_script(f"print({json.dumps(message)!r}, flush=True)\nsys.stdin.read()\n")
    try:
        client.start()
        with pytest.raises(CodexError, match="Invalid payload for known notification"):
            client.next_notification(timeout_s=5)
        with pytest.raises(CodexError, match="Invalid payload for known notification"):
            client.register_turn_notifications("turn-1")
    finally:
        client.close()


def test_turn_start_replays_completion_received_before_rpc_response() -> None:
    client = _client_for_script("""
import json
request = json.loads(sys.stdin.readline())
turn = {"id": "turn-1", "items": [], "status": "completed"}
print(json.dumps({"method": "turn/completed", "params": {
    "threadId": request["params"]["threadId"], "turn": turn,
}}), flush=True)
print(json.dumps({"id": request["id"], "result": {"turn": turn}}), flush=True)
sys.stdin.read()
""")
    try:
        client.start()
        started = client.turn_start("thread-1", "hello")
        event = client.next_turn_notification(started.turn.id, timeout_s=5)
        assert isinstance(event.payload, TurnCompletedNotification)
        assert event.payload.turn.id == started.turn.id == "turn-1"
    finally:
        client.close()


@pytest.mark.parametrize("known", [True, False], ids=["typed-usage", "unknown-global"])
def test_stdout_notifications_preserve_typed_and_unknown_payloads(known) -> None:
    usage = {
        "cachedInputTokens": 0,
        "inputTokens": 1,
        "outputTokens": 2,
        "reasoningOutputTokens": 0,
        "totalTokens": 3,
    }
    message = (
        {
            "method": "thread/tokenUsage/updated",
            "params": {
                "threadId": "thread-1",
                "turnId": "turn-1",
                "tokenUsage": {"last": usage, "total": usage},
            },
        }
        if known
        else {
            "method": "unknown/notification",
            "params": {
                "id": "evt-1",
                "conversationId": "thread-1",
                "msg": {"type": "turn_aborted"},
            },
        }
    )
    client = _client_for_script(f"print({json.dumps(message)!r}, flush=True)\nsys.stdin.read()\n")
    try:
        client.start()
        if known:
            client.register_turn_notifications("turn-1")
            event = client.next_turn_notification("turn-1", timeout_s=5)
            assert isinstance(event.payload, ThreadTokenUsageUpdatedNotification)
            assert event.payload.turn_id == "turn-1"
            assert event.payload.token_usage.last.total_tokens == 3
        else:
            event = client.next_notification(timeout_s=5)
            assert isinstance(event.payload, UnknownNotification)
            assert event.payload.params == message["params"]
        assert event.method == message["method"]
    finally:
        client.close()
