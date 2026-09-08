from __future__ import annotations

import asyncio
import sys

import pytest
from app_server_harness import AppServerHarness

import openai_codex.api as public_api_module
from openai_codex.api import (
    ApprovalMode,
    AsyncCodex,
    Codex,
    Sandbox,
)
from openai_codex.client import CodexClient, CodexConfig
from openai_codex.models import InitializeResponse


@pytest.mark.parametrize("asynchronous", [False, True], ids=["sync", "async"])
def test_public_client_derives_metadata_from_legacy_initialize_response(asynchronous: bool) -> None:
    script = """
import json
import sys
for line in sys.stdin:
    request = json.loads(line)
    if request.get("method") == "initialize":
        print(json.dumps({"id": request["id"], "result": {"userAgent": "codex-cli/1.2.3"}}), flush=True)
"""
    config = CodexConfig(launch_args_override=(sys.executable, "-u", "-c", script))

    async def async_metadata():
        async with AsyncCodex(config=config) as codex:
            return codex.metadata

    if asynchronous:
        metadata = asyncio.run(async_metadata())
    else:
        with Codex(config=config) as codex:
            metadata = codex.metadata

    assert metadata.userAgent == "codex-cli/1.2.3"
    assert metadata.serverInfo is not None
    assert metadata.serverInfo.name == "codex-cli"
    assert metadata.serverInfo.version == "1.2.3"


def test_codex_init_failure_closes_client(monkeypatch: pytest.MonkeyPatch) -> None:
    closed: list[bool] = []

    class FakeClient:
        def __init__(self, config=None) -> None:  # noqa: ANN001,ARG002
            self._closed = False

        def start(self) -> None:
            return None

        def initialize(self) -> InitializeResponse:
            return InitializeResponse.model_validate({})

        def close(self) -> None:
            self._closed = True
            closed.append(True)

    monkeypatch.setattr(public_api_module, "CodexClient", FakeClient)

    with pytest.raises(RuntimeError, match="missing required metadata"):
        Codex()

    assert closed == [True]


def test_async_codex_init_failure_closes_client() -> None:
    async def scenario() -> None:
        codex = AsyncCodex()
        close_calls = 0

        async def fake_start() -> None:
            return None

        async def fake_initialize() -> InitializeResponse:
            return InitializeResponse.model_validate({})

        async def fake_close() -> None:
            nonlocal close_calls
            close_calls += 1

        codex._client.start = fake_start  # type: ignore[method-assign]
        codex._client.initialize = fake_initialize  # type: ignore[method-assign]
        codex._client.close = fake_close  # type: ignore[method-assign]

        with pytest.raises(RuntimeError, match="missing required metadata"):
            await codex.models()

        assert close_calls == 1
        assert codex._initialized is False
        assert codex._init is None

    asyncio.run(scenario())


def test_async_codex_initializes_only_once_under_concurrency() -> None:
    async def scenario() -> None:
        codex = AsyncCodex()
        start_calls = 0
        initialize_calls = 0
        ready = asyncio.Event()
        release_initialization = asyncio.Event()
        second_started = asyncio.Event()

        async def fake_start() -> None:
            nonlocal start_calls
            start_calls += 1

        async def fake_initialize() -> InitializeResponse:
            nonlocal initialize_calls
            initialize_calls += 1
            ready.set()
            await release_initialization.wait()
            return InitializeResponse.model_validate(
                {
                    "userAgent": "codex-cli/1.2.3",
                    "serverInfo": {"name": "codex-cli", "version": "1.2.3"},
                }
            )

        async def fake_model_list(include_hidden: bool = False):  # noqa: ANN202,ARG001
            await ready.wait()
            return object()

        codex._client.start = fake_start  # type: ignore[method-assign]
        codex._client.initialize = fake_initialize  # type: ignore[method-assign]
        codex._client.model_list = fake_model_list  # type: ignore[method-assign]

        async def second_request():
            second_started.set()
            return await codex.models()

        first = asyncio.create_task(codex.models())
        await ready.wait()
        second = asyncio.create_task(second_request())
        await second_started.wait()
        release_initialization.set()
        await asyncio.gather(first, second)

        assert start_calls == 1
        assert initialize_calls == 1

    asyncio.run(asyncio.wait_for(scenario(), timeout=5))


@pytest.mark.parametrize(
    ("approval_mode", "approval_settings"),
    [
        (ApprovalMode.deny_all, {"approvalPolicy": "never"}),
        (
            ApprovalMode.auto_review,
            {"approvalPolicy": "on-request", "approvalsReviewer": "auto_review"},
        ),
    ],
    ids=["deny-all", "auto-review"],
)
@pytest.mark.parametrize(
    ("sandbox", "thread_sandbox", "turn_sandbox"),
    [
        (Sandbox.read_only, "read-only", {"type": "readOnly", "networkAccess": False}),
        (
            Sandbox.workspace_write,
            "workspace-write",
            {
                "type": "workspaceWrite",
                "networkAccess": False,
                "writableRoots": [],
                "excludeSlashTmp": False,
                "excludeTmpdirEnvVar": False,
            },
        ),
        (Sandbox.full_access, "danger-full-access", {"type": "dangerFullAccess"}),
    ],
    ids=["read-only", "workspace-write", "full-access"],
)
def test_public_presets_reach_thread_and_turn_requests(
    tmp_path, monkeypatch, approval_mode, approval_settings, sandbox, thread_sandbox, turn_sandbox
) -> None:
    requests = []
    write_message = CodexClient._write_message

    def capture_request(self, payload, **kwargs):
        requests.append(payload)
        return write_message(self, payload, **kwargs)

    monkeypatch.setattr(CodexClient, "_write_message", capture_request)
    with AppServerHarness(tmp_path) as harness:
        harness.responses.enqueue_assistant_message("presets accepted", response_id="presets")
        with Codex(config=harness.app_server_config()) as codex:
            thread = codex.thread_start(approval_mode=approval_mode, sandbox=sandbox)
            result = thread.run("use these presets", approval_mode=approval_mode, sandbox=sandbox)

    assert result.final_response == "presets accepted"
    for method, sandbox_key, expected_sandbox in (
        ("thread/start", "sandbox", thread_sandbox),
        ("turn/start", "sandboxPolicy", turn_sandbox),
    ):
        params = [request["params"] for request in requests if request["method"] == method]
        assert len(params) == 1, method
        assert {
            key: value
            for key, value in params[0].items()
            if key in {"approvalPolicy", "approvalsReviewer", sandbox_key}
        } == {**approval_settings, sandbox_key: expected_sandbox}


@pytest.mark.parametrize(
    ("kwargs", "message"),
    [
        ({"approval_mode": "allow_all"}, "deny_all, auto_review"),
        ({"sandbox": "workspace"}, r"Sandbox\.workspace_write"),
    ],
    ids=["unknown-approval-mode", "raw-sandbox-string"],
)
def test_invalid_public_presets_are_rejected_before_sending_requests(
    tmp_path, monkeypatch, kwargs, message
) -> None:
    with AppServerHarness(tmp_path) as harness:
        with Codex(config=harness.app_server_config()) as codex:
            thread = codex.thread_start()
            requests = []
            write_message = codex._client._write_message

            def capture_request(payload, **options):
                requests.append(payload)
                return write_message(payload, **options)

            monkeypatch.setattr(codex._client, "_write_message", capture_request)
            for operation in (
                lambda: codex.thread_start(**kwargs),
                lambda: codex.thread_resume(thread.id, **kwargs),
                lambda: codex.thread_fork(thread.id, **kwargs),
                lambda: thread.run("invalid presets", **kwargs),
            ):
                with pytest.raises(ValueError, match=message):
                    operation()
            assert requests == []
