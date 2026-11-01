# OpenAI Codex Python SDK (Beta) - API Reference

Public surface of the `openai_codex` source in this local Windows-only KD4 checkout.
See [local setup](getting-started.md) to select a KD4 runtime rather than the
upstream package from the public index.

This SDK is in beta. Public APIs may change before `1.0`. Turn streams are routed by turn ID so one
client can consume multiple active turns concurrently.
Thread starts default to `ApprovalMode.deny_all`; turn starts accept an optional `approval_mode`
override.

## Package Entry

```python
from openai_codex import (
    Codex,
    AsyncCodex,
    CodexConfig,
    TurnRunError,
    ApprovalMode,
    Sandbox,
    ChatgptLoginHandle,
    DeviceCodeLoginHandle,
    AsyncChatgptLoginHandle,
    AsyncDeviceCodeLoginHandle,
    Thread,
    AsyncThread,
    TurnHandle,
    AsyncTurnHandle,
    TurnResult,
    Input,
    InputItem,
    RunInput,
    TextInput,
    ImageInput,
    LocalImageInput,
    SkillInput,
    MentionInput,
)
from openai_codex.types import (
    Account,
    AccountLoginCompletedNotification,
    CancelLoginAccountResponse,
    CancelLoginAccountStatus,
    GetAccountResponse,
    InitializeResponse,
    ThreadItem,
    ThreadTokenUsage,
    TurnError,
    TurnStatus,
)
```

- Version: `openai_codex.__version__`
- Requires Windows and Python >= 3.10
- Public Codex protocol value and event types live in `openai_codex.types`

## CodexConfig

`CodexConfig(codex_bin=r"C:\path\to\codex.exe")` explicitly selects a local
runtime. Without it, the SDK resolves its pinned `openai-codex-cli-bin` package,
not `PATH`. `cwd`, `env`, `config_overrides`, and `launch_args_override` configure
the subprocess; see [transport limits](../README.md#transport-safety-and-limits)
for the bounded-operation defaults.

## Codex (sync)

```python
Codex(config: CodexConfig | None = None)
```

Properties/methods:

- `metadata -> InitializeResponse`
- `next_notification(timeout_s: float | None = None) -> Notification`
- `close() -> None`
- `login_api_key(api_key: str) -> None`
- `login_chatgpt() -> ChatgptLoginHandle`
- `login_chatgpt_device_code() -> DeviceCodeLoginHandle`
- `account(*, refresh_token: bool = False) -> GetAccountResponse`
- `logout() -> None`
- `thread_start(*, approval_mode=ApprovalMode.deny_all, base_instructions=None, config=None,
  cwd=None, developer_instructions=None, ephemeral=None, model=None, model_provider=None,
  personality=None, sandbox: Sandbox | None = None, service_name=None, service_tier=UNSET,
  session_start_source=None, thread_source=None) -> Thread`
- `thread_list(*, archived=None, cursor=None, cwd=None, limit=None, model_providers=None,
  search_term=None, sort_direction=None, sort_key=None, source_kinds=None, use_state_db_only=None)
  -> ThreadListResponse`
- `thread_resume(thread_id: str, *, approval_mode=None, base_instructions=None, config=None,
  cwd=None, developer_instructions=None, model=None, model_provider=None, personality=None, sandbox:
  Sandbox | None = None, service_tier=UNSET) -> Thread`
- `thread_fork(thread_id: str, *, approval_mode=None, base_instructions=None, config=None, cwd=None,
  developer_instructions=None, ephemeral=None, last_turn_id=None, model=None, model_provider=None, sandbox: Sandbox |
  None = None, service_tier=UNSET, thread_source=None) -> Thread`
- `thread_archive(thread_id: str) -> ThreadArchiveResponse`
- `thread_unarchive(thread_id: str) -> Thread`
- `models(*, include_hidden: bool = False) -> ModelListResponse`

Context manager:

```python
with Codex() as codex:
    ...
```

`UNSET` in the signatures denotes the internal omitted-argument default for
`service_tier`. Omit the keyword to preserve the runtime's configured value;
pass `None` to send an explicit null. Do not replace omission with `None` when
forwarding optional settings.

## AsyncCodex (async parity)

```python
AsyncCodex(config: CodexConfig | None = None)
```

Preferred usage:

```python
async with AsyncCodex() as codex:
    ...
```

`AsyncCodex` initializes lazily. Context entry is the standard path because it
ensures startup and shutdown are paired explicitly.

Properties/methods:

- `metadata -> InitializeResponse`
- `next_notification() -> Awaitable[Notification]`
- `close() -> Awaitable[None]`
- `login_api_key(api_key: str) -> Awaitable[None]`
- `login_chatgpt() -> Awaitable[AsyncChatgptLoginHandle]`
- `login_chatgpt_device_code() -> Awaitable[AsyncDeviceCodeLoginHandle]`
- `account(*, refresh_token: bool = False) -> Awaitable[GetAccountResponse]`
- `logout() -> Awaitable[None]`
- `thread_start(*, approval_mode=ApprovalMode.deny_all, base_instructions=None, config=None,
  cwd=None, developer_instructions=None, ephemeral=None, model=None, model_provider=None,
  personality=None, sandbox: Sandbox | None = None, service_name=None, service_tier=UNSET,
  session_start_source=None, thread_source=None) -> Awaitable[AsyncThread]`
- `thread_list(*, archived=None, cursor=None, cwd=None, limit=None, model_providers=None,
  search_term=None, sort_direction=None, sort_key=None, source_kinds=None, use_state_db_only=None)
  -> Awaitable[ThreadListResponse]`
- `thread_resume(thread_id: str, *, approval_mode=None, base_instructions=None, config=None,
  cwd=None, developer_instructions=None, model=None, model_provider=None, personality=None, sandbox:
  Sandbox | None = None, service_tier=UNSET) -> Awaitable[AsyncThread]`
- `thread_fork(thread_id: str, *, approval_mode=None, base_instructions=None, config=None, cwd=None,
  developer_instructions=None, ephemeral=None, last_turn_id=None, model=None, model_provider=None, sandbox: Sandbox |
  None = None, service_tier=UNSET, thread_source=None) -> Awaitable[AsyncThread]`
- `thread_archive(thread_id: str) -> Awaitable[ThreadArchiveResponse]`
- `thread_unarchive(thread_id: str) -> Awaitable[AsyncThread]`
- `models(*, include_hidden: bool = False) -> Awaitable[ModelListResponse]`

Async context manager:

```python
async with AsyncCodex() as codex:
    ...
```

## Login handles

### ChatgptLoginHandle / AsyncChatgptLoginHandle

- `login_id: str`
- `auth_url: str`
- `wait() -> AccountLoginCompletedNotification`
- `cancel() -> CancelLoginAccountResponse`

Async handle methods return awaitables.

### DeviceCodeLoginHandle / AsyncDeviceCodeLoginHandle

- `login_id: str`
- `verification_url: str`
- `user_code: str`
- `wait() -> AccountLoginCompletedNotification`
- `cancel() -> CancelLoginAccountResponse`

Async handle methods return awaitables.

`wait()` consumes only the completion notification for its matching login
attempt. API-key login completes synchronously and does not return a handle.

## Thread / AsyncThread

`Thread` and `AsyncThread` share the same shape and intent.

### Thread

- `run(input: str | Input, *, approval_mode=None, allow_no_prose=None, cwd=None, effort=None, model=None,
  output_schema=None, personality=None, sandbox: Sandbox | None = None, service_tier=UNSET,
  summary=None) -> TurnResult`
- `turn(input: str | Input, *, approval_mode=None, allow_no_prose=None, cwd=None, effort=None, model=None,
  output_schema=None, personality=None, run_independently=None, sandbox: Sandbox | None = None, service_tier=UNSET,
  summary=None) -> TurnHandle`
- `read(*, include_turns: bool = False) -> ThreadReadResponse`
- `set_name(name: str) -> ThreadSetNameResponse`
- `compact() -> ThreadCompactStartResponse`

### AsyncThread

- `run(input: str | Input, *, approval_mode=None, allow_no_prose=None, cwd=None, effort=None, model=None,
  output_schema=None, personality=None, sandbox: Sandbox | None = None, service_tier=UNSET,
  summary=None) -> Awaitable[TurnResult]`
- `turn(input: str | Input, *, approval_mode=None, allow_no_prose=None, cwd=None, effort=None, model=None,
  output_schema=None, personality=None, run_independently=None, sandbox: Sandbox | None = None, service_tier=UNSET,
  summary=None) -> Awaitable[AsyncTurnHandle]`
- `read(*, include_turns: bool = False) -> Awaitable[ThreadReadResponse]`
- `set_name(name: str) -> Awaitable[ThreadSetNameResponse]`
- `compact() -> Awaitable[ThreadCompactStartResponse]`

`run(...)` is the common-case convenience path. It accepts plain strings, starts
the turn, consumes notifications until completion, and returns a small result
object with:

- `id: str`
- `status: TurnStatus`
- `error: TurnError | None`
- `started_at: int | None`
- `completed_at: int | None`
- `duration_ms: int | None`
- `final_response: str | None`
- `items: list[ThreadItem]`
- `usage: ThreadTokenUsage | None`
- `surfaced_result: SurfacedToolResult | None`
- `timing: TurnTiming | None`

`final_response` uses the terminal surfaced tool result's `canonical_message`,
then the terminal `timing.final_answer` receipt. A present receipt without a
message suppresses visible-item fallback. Older runtimes fall back to an
uninterrupted final-answer or phase-less assistant message; without one the value
is `None`. Timing diagnostics alone do not certify completion.

Failed or incomplete buffered runs raise `TurnRunError`, retaining observed
`items`, `usage`, `turn_id`, `thread_id`, and any `terminal` / collected `result`.
Inspect those receipts before retrying because prior tool effects may be committed.
Async cancellation continues to propagate `CancelledError`.

`allow_no_prose=True` explicitly permits an exactly empty final answer for that
request only; it does not waive work, schemas, or completion hooks.
`run_independently` is available on `turn(...)`, not on the buffered `run(...)`
shortcut, and is forwarded to the runtime.

Use `turn(...)` when you need low-level turn control (`stream()`, `steer()`,
`interrupt()`) before collecting the turn result.

## Sandbox

Use `sandbox=` consistently on thread lifecycle methods and turns:

```python
from openai_codex import Codex, Sandbox

with Codex() as codex:
    thread = codex.thread_start(sandbox=Sandbox.workspace_write)
    result = thread.run("Review the diff only.", sandbox=Sandbox.read_only)
```

Presets:

- `Sandbox.read_only`: read files without allowing writes.
- `Sandbox.workspace_write`: the normal default for projects with a recorded trust decision; read
  files and write inside the workspace and configured writable roots.
- `Sandbox.full_access`: run without filesystem access restrictions.

When `sandbox=` is omitted, Codex uses its configured default. A sandbox
passed to `run(...)` or `turn(...)` applies to that turn and subsequent turns.

## TurnHandle / AsyncTurnHandle

### TurnHandle

- `close(*, cancel=False) -> None` / `abandon(*, cancel=False) -> None`
- `steer(input: str | Input) -> TurnSteerResponse`
- `interrupt() -> TurnInterruptResponse`
- `stream(*, cancel_on_close=False) -> Iterator[Notification]`
- `run() -> TurnResult`

Behavior notes:

- `stream()` and `run()` consume only notifications for their own turn ID
- one `Codex` instance can stream multiple active turns concurrently

### AsyncTurnHandle

- `close(*, cancel=False) -> Awaitable[None]` / `abandon(*, cancel=False) -> Awaitable[None]`
- `steer(input: str | Input) -> Awaitable[TurnSteerResponse]`
- `interrupt() -> Awaitable[TurnInterruptResponse]`
- `stream(*, cancel_on_close=False) -> AsyncIterator[Notification]`
- `run() -> Awaitable[TurnResult]`

Behavior notes:

- `stream()` and `run()` consume only notifications for their own turn ID
- one `AsyncCodex` instance can stream multiple active turns concurrently

Handle close/abandon and early stream exit detach by default. Opt into cancellation
with `close(cancel=True)` or `stream(cancel_on_close=True)`; explicitly close/aclose
the iterator on early exit. Interruption failures propagate after route cleanup.
Closing a generator before its first iteration does not run its cleanup; close the
handle directly in that case. `CodexConfig.turn_timeout_s` optionally sets a hard
whole-turn cap; `notification_timeout_s` bounds each wait independently of the RPC
`operation_timeout_s`. Interrupt/steer use bounded reserved control capacity.

## Inputs

```python
@dataclass class TextInput: text: str
@dataclass class ImageInput: url: str
@dataclass class LocalImageInput: path: str
@dataclass class SkillInput: name: str; path: str
@dataclass class MentionInput: name: str; path: str

InputItem = TextInput | ImageInput | LocalImageInput | SkillInput | MentionInput
Input = list[InputItem] | InputItem
RunInput = Input | str
```

Use `ImageInput` with a base64-encoded `data:image/...` URL. HTTP and HTTPS image URLs are
deprecated; download remote images and pass their local paths with `LocalImageInput` instead.

Use a plain `str` as shorthand for `TextInput(...)` anywhere a turn input is accepted:
`thread.run("...")`, `thread.turn("...")`, and `turn.steer("...")`.

## Public Types

The SDK wrappers return and accept public Codex protocol models wherever possible:

```python
from openai_codex.types import (
    Account,
    AccountLoginCompletedNotification,
    CancelLoginAccountResponse,
    CancelLoginAccountStatus,
    GetAccountResponse,
    ThreadReadResponse,
    Turn,
    TurnStatus,
)
```

## Retry + errors

```python
from openai_codex import (
    retry_on_overload,
    JsonRpcError,
    MethodNotFoundError,
    InvalidParamsError,
    ServerBusyError,
    is_retryable_error,
)
```

- `retry_on_overload(...)` retries transient overload errors with exponential backoff + jitter.
- `is_retryable_error(exc)` checks if an exception is transient/overload-like.

## Example

```python
from openai_codex import Codex

with Codex() as codex:
    thread = codex.thread_start(model="gpt-5.4", config={"model_reasoning_effort": "high"})
    result = thread.run("Say hello in one sentence.")
    print(result.final_response)
```
