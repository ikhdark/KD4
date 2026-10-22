from __future__ import annotations

import asyncio
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor

import pytest

from openai_codex import Codex
from openai_codex.api import AsyncCodex
from openai_codex.client import CodexClient, CodexConfig

PEER = r"""
import json, sys
opt_outs = set()
callback_id = 0
def send(value): print(json.dumps(value), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    if "method" not in request:
        send({"method":"fixture/reply", "params":request})
        continue
    if "id" not in request: continue
    method = request["method"]
    if method == "initialize":
        opt_outs = set(request["params"]["capabilities"].get("optOutNotificationMethods", []))
        result = {"userAgent":"fixture/1", "serverInfo":{"name":"fixture", "version":"1"}}
    elif method == "model/list":
        if "thread/status/changed" not in opt_outs:
            send({"method":"thread/status/changed", "params":{"threadId":"thread", "status":{"type":"idle"}}})
        result = {"data":[], "nextCursor":None}
    elif method == "fixture/callback":
        callback_id += 1
        send({"id":f"callback-{callback_id}", "method":"fixture/approval", "params":{}})
        result = {}
    elif method == "fixture/important":
        send({"method":"fixture/important", "params":{"proof":True}})
        result = {}
    else: result = {"ready":True}
    send({"id":request["id"], "result":result})
"""


def config() -> CodexConfig:
    return CodexConfig(
        launch_args_override=(sys.executable, "-u", "-c", PEER),
        max_buffered_notifications=4,
        operation_timeout_s=3,
    )


@pytest.mark.parametrize("asynchronous", [False, True])
def test_facade_owns_global_subscriptions_and_exposes_remaining_events(asynchronous) -> None:
    from pydantic import BaseModel

    class Reply(BaseModel):
        pass

    async def scenario() -> None:
        async with AsyncCodex(config()) as codex:
            for _ in range(100):
                assert (await codex.models()).data == []
            await codex._client.request("fixture/important", {}, response_model=Reply)
            notification = await codex.next_notification()
            assert notification.method == "fixture/important"
            assert notification.payload.params == {"proof": True}

    if asynchronous:
        asyncio.run(scenario())
    else:
        with Codex(config()) as codex:
            for _ in range(100):
                assert codex.models().data == []
            codex._client.request("fixture/important", {}, response_model=Reply)
            notification = codex.next_notification(1)
            assert notification.method == "fixture/important"
            assert notification.payload.params == {"proof": True}


def test_held_callback_does_not_block_unrelated_response_decoding() -> None:
    entered, release = threading.Event(), threading.Event()

    def handler(method, params):
        entered.set()
        assert release.wait(3)
        return {"decision": "accept"}

    with CodexClient(config(), approval_handler=handler) as client:
        with ThreadPoolExecutor(max_workers=1) as worker:
            first = worker.submit(client._request_raw, "fixture/callback")
            try:
                assert entered.wait(1)
                assert client._request_raw("fixture/ready") == {"ready": True}
                assert first.result(timeout=1) == {}
            finally:
                release.set()
        reply = client.next_notification(1).payload.params
        assert reply == {"id": "callback-1", "result": {"decision": "accept"}}


def test_callback_slower_than_operation_budget_still_replies() -> None:
    # A human approval may take longer than one RPC budget; the late reply must
    # not hit an expired deadline and tear down the healthy transport.
    def handler(method, params):
        time.sleep(1.5)
        return {"decision": "accept"}

    with CodexClient(config(), approval_handler=handler) as client:
        assert client._request_raw("fixture/ready") == {"ready": True}
        client.config.operation_timeout_s = 1.0
        assert client._request_raw("fixture/callback") == {}
        reply = client.next_notification(5).payload.params
        assert reply == {"id": "callback-1", "result": {"decision": "accept"}}
        assert client._request_raw("fixture/still-healthy") == {"ready": True}


def test_nested_callback_request_and_callback_failure_are_request_scoped() -> None:
    client: CodexClient
    fail = False

    def handler(method, params):
        if fail:
            raise RuntimeError("callback failure sentinel")
        return client._request_raw("fixture/nested")

    with CodexClient(config(), approval_handler=handler) as client:
        assert client._request_raw("fixture/callback") == {}
        assert client.next_notification(1).payload.params == {
            "id": "callback-1",
            "result": {"ready": True},
        }
        fail = True
        assert client._request_raw("fixture/callback") == {}
        reply = client.next_notification(1).payload.params
        assert reply["error"]["code"] == -32603
        assert "callback failure sentinel" in reply["error"]["message"]
        assert client._request_raw("fixture/still-healthy") == {"ready": True}


def test_blocked_callback_budget_survives_restart_and_late_replies_stay_old() -> None:
    entered, release = threading.Event(), threading.Event()
    lock = threading.Lock()
    old_calls = 0
    phase = "old"

    def handler(method, params):
        nonlocal old_calls
        with lock:
            generation = phase
            if generation == "old":
                old_calls += 1
                if old_calls == 4:
                    entered.set()
        if generation == "old":
            assert release.wait(5)
        return {"generation": generation}

    client = CodexClient(config(), approval_handler=handler)
    client.start()
    try:
        for _ in range(4):
            assert client._request_raw("fixture/callback") == {}
        assert entered.wait(1)
        client.close()
        phase = "new"
        client.start()
        assert client._request_raw("fixture/callback") == {}
        exhausted = client.next_notification(1).payload.params
        assert exhausted["error"]["code"] == -32603
        assert "capacity exhausted" in exhausted["error"]["message"]
        assert old_calls == 4, "restart must not mint new slots for blocked old callbacks"
        release.set()
        deadline = time.monotonic() + 2
        while not client._server_request_slots.acquire(blocking=False):
            assert time.monotonic() < deadline
            time.sleep(0.005)
        client._server_request_slots.release()
        assert client._request_raw("fixture/callback") == {}
        reply = client.next_notification(1).payload.params
        assert reply["id"] == "callback-2"
        assert reply["result"] == {"generation": "new"}, (
            "old callback results must not enter the replacement reader"
        )
    finally:
        release.set()
        client.close()


def test_old_callback_cannot_issue_nested_rpc_after_process_restart() -> None:
    from openai_codex.errors import TransportClosedError

    entered, release, rejected = threading.Event(), threading.Event(), threading.Event()
    client: CodexClient

    def handler(method, params):
        entered.set()
        assert release.wait(3)
        try:
            client._request_raw("fixture/old-generation-nested")
        except TransportClosedError:
            rejected.set()
        return {}

    client = CodexClient(config(), approval_handler=handler)
    client.start()
    try:
        assert client._request_raw("fixture/callback") == {}
        assert entered.wait(1)
        client.close()
        client.start()
        release.set()
        assert rejected.wait(1), "nested RPCs must inherit the callback's old transport generation"
        assert client._request_raw("fixture/new-generation") == {"ready": True}
    finally:
        release.set()
        client.close()


@pytest.mark.parametrize("invalid", [[], {"bad": object()}, {"bad": float("nan")}])
def test_invalid_callback_output_is_a_request_error_not_transport_failure(invalid) -> None:
    with CodexClient(config(), approval_handler=lambda method, params: invalid) as client:
        assert client._request_raw("fixture/callback") == {}
        reply = client.next_notification(1).payload.params
        assert reply["error"]["code"] == -32603
        assert client._request_raw("fixture/still-healthy") == {"ready": True}
