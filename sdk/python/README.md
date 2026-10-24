# OpenAI Codex Python SDK (Beta)

Build Python applications that start Codex threads, run turns, stream progress,
and control workspace access.

Historical `thread/goal/*` wrappers require an older compatible runtime; current
KD4 uses ordinary thread/turn operations. The `externalAgentConfig/*` compatibility
API supports detection, config/environment, MCP, skills, AGENTS.md, plugins, hooks,
subagents, commands, sessions, and persistent import histories. Imports preserve
existing target configuration and report unsupported source controls as per-item
failures. Imported session tool activity is inert history, not executable tool calls.
A runtime missing the API raises `MethodNotFoundError` without retrying.

## Install

Install the SDK:

```bash
pip install openai-codex
```

## Quickstart

The SDK reuses your existing Codex authentication when one is already
available:

```python
from openai_codex import Codex

with Codex() as codex:
    thread = codex.thread_start()
    result = thread.run("Explain this repository in three bullets.")
    print(result.final_response)
```

`thread.run(...)` returns a `TurnResult` containing the final response,
collected items, and token usage.

Failed or incomplete buffered runs raise `TurnRunError` (a `RuntimeError`).
It retains `items`, `usage`, `turn_id`, the observed `terminal` and `result`
when available, and the original stream exception as `__cause__`.
Earlier tools may already have completed: inspect these receipts before retrying.
Async cancellation still propagates `CancelledError`.

## Authentication

Existing Codex authentication is reused automatically. To start ChatGPT
browser login explicitly:

```python
from openai_codex import Codex

with Codex() as codex:
    login = codex.login_chatgpt()
    print(login.auth_url)
    print(login.wait().success)
```

For device-code login:

```python
with Codex() as codex:
    login = codex.login_chatgpt_device_code()
    print(login.verification_url, login.user_code)
    login.wait()
```

For API-key login:

```python
with Codex() as codex:
    codex.login_api_key("sk-...")
```

## Transport safety and limits

`CodexConfig` bounds local transport work by default:

| Setting | Default |
| --- | --- |
| `operation_timeout_s` | 300 seconds |
| `shutdown_timeout_s` | 2 seconds |
| `max_message_bytes` | 8 MiB per JSON-RPC line |
| `max_buffered_notifications` | 4096 across all routes |
| `max_buffer_bytes` | 64 MiB of serialized notification payloads |
| `max_notification_routes` | 256 tracked login, turn, and goal IDs |
| `max_in_flight_requests` | 32 operations / async workers |

The high-level `Codex` and `AsyncCodex` facades opt out of redundant
`thread/started` and `thread/status/changed` mirrors. Turn streams and creation
responses remain authoritative. Consume other global/account/warning events with
`next_notification()` (sync accepts `timeout_s`); an undrained important-event
stream still fails explicitly at the configured buffer limit. Low-level clients
retain their default subscriptions and pull ownership.

Server-request callbacks run in a separate four-worker, nonqueueing lane. A held
callback does not block stdout decoding; saturation and callback exceptions return
per-request errors. Callbacks cannot be forcibly stopped: uncooperative callbacks
keep their slots, including across process restart, and late replies cannot cross
transport generations.

Timeouts must be finite and positive; capacity limits must be positive integers.
Increase them explicitly for workloads that need larger results or longer turns.
RPC deadlines include writing, response waiting, and the entire SDK retry sequence,
not a fresh budget for each retry. Turn/login/goal notification deadlines inherit
their start operation's remaining budget (or start at explicit registration)
and are not extended by incoming events. Global notification waits
have a per-wait deadline. Timeout failures use `TimeoutError`; admission and buffer
limit failures use `CodexError`.

Notifications are never silently evicted to make room. Incoming overflow fails
the transport and wakes its consumers. Cancelling or timing out an in-flight async
RPC aborts its transport generation, so other calls on that connection also fail;
close it before restarting. Cancellation of a notification-only wait leaves its
events available. Completed start results are delivered or cleaned up by the same
bounded worker. Arbitrary user callbacks cannot be forcibly stopped by Python;
blocked callbacks keep their admission slot rather than spawning unlimited work.

Shutdown terminates the process before pipe cleanup and bounds process waits and
thread joins. Restart is refused while old pipe workers remain alive, including
when a descendant retains inherited pipe handles. Low-level approval handling
denies command/file requests by default, matching the curated `deny_all` policy.

Local transport regression tests require only the existing Python test environment:

```bash
python -m pytest tests/test_transport_limits.py tests/test_client_rpc_methods.py tests/test_async_client_behavior.py tests/test_retry.py
```

The real app-server integration tests install packages and depend on the pinned
runtime and a live model; they are not part of these local checks.

## Built-In Help

Use Python's standard `help(openai_codex)`, `help(Codex)`, or
`python -m pydoc openai_codex` documentation tools.

## Documentation

- [Getting started](https://github.com/openai/codex/blob/main/sdk/python/docs/getting-started.md)
- [API reference](https://github.com/openai/codex/blob/main/sdk/python/docs/api-reference.md)
- [FAQ](https://github.com/openai/codex/blob/main/sdk/python/docs/faq.md)
- [Examples](https://github.com/openai/codex/blob/main/sdk/python/examples/README.md)

The package is licensed under the
[repository Apache License 2.0](https://github.com/openai/codex/blob/main/LICENSE).
