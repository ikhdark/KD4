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
| `operation_timeout_s` | 300 seconds per RPC (including retries) |
| `notification_timeout_s` | 300 seconds per notification wait |
| `turn_timeout_s` | `None` (no whole-turn cap; set explicitly for a hard cap) |
| `shutdown_timeout_s` | 2 seconds |
| `max_message_bytes` | 8 MiB per JSON-RPC line |
| `max_buffered_notifications` | 4096 across all routes |
| `max_buffer_bytes` | 64 MiB of serialized notification payloads |
| `max_notification_routes` | 256 tracked login, turn, and goal IDs |
| `max_in_flight_requests` | 32 operations / async workers |
| `max_control_requests` | 2 reserved interrupt/steer operations / workers / response waiters |

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
Increase them explicitly for workloads that need larger results or longer waits.
RPC deadlines include writing, response waiting, and the entire SDK retry sequence,
not a fresh budget for each retry. Turn streams use independent per-wait deadlines;
active or queued output does not expire at the RPC deadline. Set `turn_timeout_s`
to impose an absolute deadline from route registration; that explicit hard cap is
not extended by incoming events, even while events remain queued. Login/legacy-goal
routes retain their start operation's absolute deadline. Timeout failures use
`TimeoutError`; admission and buffer limit failures use `CodexError`.

Notifications are never silently evicted to make room. Buffer pressure explicitly
fails the largest congested route and releases its backlog, not the shared transport.
An event larger than the entire byte budget fails only its own route. Failed routes
cannot certify completion; consumers must handle the error before retrying. Route
admission remains bounded. Malformed display-only mirrors/progress are exposed as
raw `UnknownNotification` payloads; authoritative items, deltas, usage and lifecycle
events still fail closed on validation errors.

Cancelled or timed-out RPC response waits detach without stopping unrelated turns;
late responses are ignored. Queued, unwritten frames are skipped. An ambiguous
in-progress pipe write or broken I/O still aborts the transport generation: close
it before restarting. Cancellation of a notification-only wait leaves its events
available. A cancelled async turn start retains its bounded worker until a response
or the original RPC deadline, then interrupts a received late start result. A start
with no acknowledged result has unknown server-side outcome; do not blindly retry.
Arbitrary user callbacks cannot be forcibly stopped by Python; blocked callbacks
keep their admission slot rather than spawning unlimited work.

Turn-handle `close()` / `abandon()` detach without interrupting by default. Use
`close(cancel=True)` (await it for async handles), or `stream(cancel_on_close=True)`
to request interruption on early stream exit. Explicitly close the iterator when
breaking iteration; closing a never-started generator does not execute its cleanup.
Low-level `stream_text(..., cancel_on_close=True)` has the same opt-in policy.
Interruption errors propagate, but notification routes are released in `finally`.

Shutdown terminates the process before pipe cleanup and bounds process waits and
thread joins. Restart is refused while old pipe workers remain alive, including
when a descendant retains inherited pipe handles. Low-level approval handling
denies command/file requests by default, matching the curated `deny_all` policy.

This checkout does not include an automated Python test suite. The
[runnable examples](examples/README.md) exercise SDK workflows using your
configured Codex runtime and may make live model requests.

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
