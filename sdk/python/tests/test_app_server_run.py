from __future__ import annotations

import asyncio

import pytest
from app_server_harness import (
    AppServerHarness,
    ev_assistant_message,
    ev_completed,
    ev_completed_with_usage,
    ev_failed,
    ev_response_created,
    sse,
)
from app_server_helpers import (
    agent_message_texts_from_items,
    assistant_message_with_phase,
)

from openai_codex import AsyncCodex, Codex
from openai_codex.generated.v2_all import MessagePhase


def test_sync_thread_run_uses_mock_responses(
    tmp_path,
) -> None:
    """Drive Thread.run through the pinned app-server and inspect the HTTP request."""
    with AppServerHarness(tmp_path) as harness:
        harness.responses.enqueue_assistant_message("Hello from the mock.", response_id="run-1")

        with Codex(config=harness.app_server_config()) as codex:
            thread = codex.thread_start()
            result = thread.run("hello")

        request = harness.responses.single_request()

    body = request.body_json()
    assert {
        "final_response": result.final_response,
        "agent_messages": agent_message_texts_from_items(result.items),
        "has_usage": result.usage is not None,
        "request_model": body["model"],
        "request_stream": body["stream"],
        "request_user_texts": request.message_input_texts("user")[-1:],
    } == {
        "final_response": "Hello from the mock.",
        "agent_messages": ["Hello from the mock."],
        "has_usage": True,
        "request_model": "mock-model",
        "request_stream": True,
        "request_user_texts": ["hello"],
    }


def test_run_params_and_usage_cross_app_server_boundary(tmp_path) -> None:
    """Thread.run should pass overrides and collect app-server token usage."""
    with AppServerHarness(tmp_path) as harness:
        harness.responses.enqueue_sse(
            sse(
                [
                    ev_response_created("run-overrides"),
                    ev_assistant_message("msg-run-overrides", "overrides applied"),
                    ev_completed_with_usage(
                        "run-overrides",
                        input_tokens=11,
                        cached_input_tokens=3,
                        output_tokens=7,
                        reasoning_output_tokens=5,
                        total_tokens=18,
                    ),
                ]
            )
        )

        with Codex(config=harness.app_server_config()) as codex:
            thread = codex.thread_start()
            result = thread.run(
                "use overrides",
                model="mock-model-override",
            )
            request = harness.responses.single_request()

    usage_payload = None
    if result.usage is not None:
        dumped_usage = result.usage.model_dump(by_alias=True, mode="json")
        usage_payload = {
            "last": dumped_usage["last"],
            "total": dumped_usage["total"],
        }
    assert {
        "final_response": result.final_response,
        "request_model": request.body_json()["model"],
        "usage": usage_payload,
    } == {
        "final_response": "overrides applied",
        "request_model": "mock-model-override",
        "usage": {
            "last": {
                "cachedInputTokens": 3,
                "inputTokens": 11,
                "outputTokens": 7,
                "reasoningOutputTokens": 5,
                "totalTokens": 18,
            },
            "total": {
                "cachedInputTokens": 3,
                "inputTokens": 11,
                "outputTokens": 7,
                "reasoningOutputTokens": 5,
                "totalTokens": 18,
            },
        },
    }


def test_async_thread_run_uses_mock_responses(
    tmp_path,
) -> None:
    """Async Thread.run should exercise the same app-server boundary."""

    async def scenario() -> None:
        """Run the async client against a real app-server process."""
        with AppServerHarness(tmp_path) as harness:
            harness.responses.enqueue_assistant_message(
                "Hello async.",
                response_id="async-run-1",
            )

            async with AsyncCodex(config=harness.app_server_config()) as codex:
                thread = await codex.thread_start()
                result = await thread.run("async hello")

            request = harness.responses.single_request()

        assert {
            "final_response": result.final_response,
            "agent_messages": agent_message_texts_from_items(result.items),
            "request_user_texts": request.message_input_texts("user")[-1:],
        } == {
            "final_response": "Hello async.",
            "agent_messages": ["Hello async."],
            "request_user_texts": ["async hello"],
        }

    asyncio.run(scenario())


@pytest.mark.parametrize("asynchronous", [False, True], ids=["sync", "async"])
@pytest.mark.parametrize(
    ("messages", "expected_final"),
    [
        ([("First message", None), ("Second message", None)], "Second message"),
        ([("First message", None), ("", None)], ""),
        ([("Commentary", MessagePhase.commentary)], None),
        (
            [("Commentary", MessagePhase.commentary), ("Final answer", MessagePhase.final_answer)],
            "Final answer",
        ),
    ],
    ids=["last-message", "empty-last-message", "commentary-only", "final-answer"],
)
def test_turn_result_selects_final_response_through_app_server(
    tmp_path, asynchronous, messages, expected_final
) -> None:
    async def run_async(harness):
        async with AsyncCodex(config=harness.app_server_config()) as codex:
            thread = await codex.thread_start()
            return await thread.run("choose final answer")

    with AppServerHarness(tmp_path) as harness:
        events = [ev_response_created("result-mapping")]
        for index, (text, phase) in enumerate(messages):
            item_id = f"msg-{index}"
            events.append(
                ev_assistant_message(item_id, text)
                if phase is None
                else assistant_message_with_phase(item_id, text, phase)
            )
        events.append(ev_completed("result-mapping"))
        harness.responses.enqueue_sse(sse(events))

        if asynchronous:
            result = asyncio.run(run_async(harness))
        else:
            with Codex(config=harness.app_server_config()) as codex:
                result = codex.thread_start().run("choose final answer")

    assert result.final_response == expected_final
    assert [
        (item.root.text, item.root.phase)
        for item in result.items
        if item.root.type == "agentMessage"
    ] == messages


def test_thread_run_raises_when_real_app_server_reports_failed_turn(tmp_path) -> None:
    """Thread.run should surface the failed turn error emitted by app-server."""
    with AppServerHarness(tmp_path) as harness:
        harness.responses.enqueue_sse(
            sse(
                [
                    ev_response_created("failed-run"),
                    ev_failed("failed-run", "boom from mock model"),
                ]
            )
        )

        with Codex(config=harness.app_server_config()) as codex:
            thread = codex.thread_start()
            with pytest.raises(RuntimeError, match="boom from mock model"):
                thread.run("trigger failure")
