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
- `read_file_range(path, *, start, length, expected_revision=None) -> FsReadFileRangeResponse` ([native file reads](#native-file-reads))
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

## Native file reads

Use `read_file_range` when your application already knows which file to read.
It calls the [app-server filesystem endpoint](../../../codex-rs/app-server-protocol-types/src/protocol/v2/fs.rs),
not a model or a thread. Use an absolute path on the SDK runtime's filesystem.
Offsets are bytes from zero; `length` is 1–65,536. A short response can be
the end of a file, and an empty file is not an error. For subsequent ranges
of the same file, pass the first result's `revision` as `expected_revision`
so a changed file fails rather than mixing revisions. Errors, including
an unavailable endpoint on an older runtime, propagate without model fallback.

```python
from base64 import b64decode

page = codex.read_file_range(r"C:\work\receipt.json", start=0, length=65_536)
data = b64decode(page.data_base64, validate=True)
if len(data) != page.file_bytes:
    raise ValueError("Expected the complete receipt; read more ranges or stop")
```

For a small, known-format receipt, parse and validate the complete bytes in the
caller. To finish a task that also needs judgment, pass only the relevant
validated fields to a normal turn; don't ask it to fetch or reconstruct the
same evidence. Use the normal turn directly when the path, fields or operation
must be inferred. `AsyncCodex.read_file_range` has the same contract.

## Model selection for an independent operation

If your application explicitly selects a model for a self-contained operation,
start a separate ephemeral thread and pass only the input that operation needs:

```python
operation = codex.thread_start(ephemeral=True, model="gpt-6.2-sol")
result = operation.run(prompt, output_schema=operation_schema)
```

This leaves any existing conversation's model unchanged. A `model` override on
`Thread.run()` selects that model for the turn **and subsequent turns on that
thread**; don't use a temporary override on a shared conversation to isolate
an operation. Keep dependent turns on the original thread when they require
its history or configuration.

## Explicit inference workflow

Use `InferenceWorkflow` when **your application** knows which results are
required and which branch should run. It does not change sampling inside a
Codex turn, inspect task semantics, or launch autonomous work.

For a complete, independently keyed evidence table, the application can use
`prepare_keyed_evidence` before it submits a turn. Supply the exact required
keys from the assignment, including every key for exhaustive work. Use this
only when each row applies solely to its declared key and the renderer changes
only which rows are sent; do not use it for partial reads, cross-key evidence
or free-form intent inference.

```python
import json

from openai_codex.inference import prepare_keyed_evidence

body = prepare_keyed_evidence(
    rows,
    frozenset(assignment),
    fields=frozenset({"requirement", "receipt", "revision", "observation"}),
    key_field="requirement",
    render=lambda evidence: json.dumps({"assignment": assignment, "evidence": evidence}),
)
```

All relevant revisions, duplicates and contradictions stay in original order.
Unknown table shape, a missing required key, or less than 20% rendered UTF-8
byte savings keeps the original body. This is not a token limit, automatic
routing, an answer cache, or a server retry; the ordinary submission still
owns its errors and its configured inference settings.

If the assignment is only an exact lookup (not interpretation), use
[`exact_keyed_values`](../src/openai_codex/inference.py) instead. It returns
a mapping in requested order without inference only for a complete,
caller-supplied two-column string table with exactly one match per requested
key. Unknown shape, missing keys and duplicate requested rows (even with equal
values) return `None`. The caller must prove table completeness and keep its
ordinary inference path for those cases; this does not route arbitrary prompts.

```python
from openai_codex.inference import exact_keyed_values

answer = exact_keyed_values(
    rows, ("invoice", "receipt"), key_field="id", value_field="value"
)
# If answer is None, handle the original request rather than a partial map.
```

```python
import asyncio

from openai_codex import InferenceSlotsExhausted, InferenceWorkflow

# selection and tail are caller-owned asyncio tasks. Only selection is needed
# to choose the branch. Both the branch result and tail are needed for the final.
workflow = InferenceWorkflow(slots=2, reserve=1)
branch = asyncio.create_task(workflow.run_phase(
    lambda results: branch_prompt_for(results[0]),
    branch_thread.run,
    ready=[selection],
))
final = await workflow.run_phase(
    lambda results: synthesis_prompt_for(*results),
    final_thread.run,
    ready=[branch, tail],
    essential=True,
)
```

- Supply only the tasks that gate each phase, in the order its pure
  `prepare(results)` callback needs. Failed prerequisites propagate without
  submitting. Cancelling the readiness wait leaves those tasks owned by their
  original caller. Once submission begins, the SDK submitter owns cancellation.
  Use distinct threads for concurrent turns or await prior completion.
- Return `None` from `prepare` to skip a phase without spending a slot. For
  completion preflight, check the exact current result first (for example,
  `await thread.recorded_answer(turn_id)` without fallback), retain that result
  when it satisfies the task, and prepare a fresh turn only if still needed.
  Do not treat missing evidence as completion.
- `slots=None` (default) is unbounded. A finite `slots` counts calls to the
  submitter within this workflow, not transport retries, physical provider
  requests, hosted quota, or tokens. `reserve` protects local slots for phases
  marked `essential=True`. A denied phase raises `InferenceSlotsExhausted`
  without contacting the runtime. Never refund or auto-retry an uncertain
  submission; inspect the owning receipt. This helper runs on one event loop.
- For a queue, use `thread/queue/delete` only for the ID explicitly identified
  as obsolete and check the deletion result. Submit jointly intended inputs as
  one turn; do not merge independent requests based on text or timing. The
  [local queue contract](../../../codex-rs/app-server/THREAD_QUEUE.md) describes
  acceptance receipts and replay identity.
- To retire an already-started turn, use its handle's `interrupt()` rather
  than launching a replacement blindly. Read terminal and partial-output
  events separately; absence of a reported usage receipt is not zero usage.
  Provider-side work retirement was measured only on the qualified local
  Responses route.

### Coalesce replaceable refresh events

For a burst of revisions where **only the latest value matters**, feed a
dedicated `asyncio.Queue` from that one source. The first queued event opens a
fixed window; later events update the selected value without extending the
window. Urgent actions and ordinary user turns should use their own direct
admission path.

```python
from openai_codex import latest_in_window

latest = await latest_in_window(refresh_events, window=0.300)
result = await workflow.run_phase(lambda _: latest, refresh_thread.run)
```

Choose the window for your application's latency tolerance. The helper
intentionally discards superseded values and delays even a single refresh on
that queue. It neither deduplicates independent turns nor cancels a submitted
turn. A queue item is acknowledged when consumed, not when inference succeeds.
On cancellation the helper stops receiving; the caller owns any durable
records and subsequent attempts. Use one consumer per queue and do not put
auditable user input or irreversible work in this lane.

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
- `read_file_range(path, *, start, length, expected_revision=None) -> Awaitable[FsReadFileRangeResponse]`
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
attempt. The async `wait()` closes the notification route when it exits,
including on cancellation, and discards unread events. This does not cancel
the server-side login attempt; use `cancel()` to request that separately.
API-key login completes synchronously and does not return a handle.

## Thread / AsyncThread

`Thread` and `AsyncThread` share the same shape and intent.

Use `thread.fork(*, last_turn_id=None, ephemeral=None)` to branch an
independent investigation from the existing history without injecting later
branch turns into the parent. It keeps the original working scope. To change
the instruction scope without bringing earlier directory-specific history,
use `codex.start_scoped_thread(cwd, *, ephemeral=None)` instead.
It takes an absolute working directory (a string or `Path`) and uses the
server's ordinary project instruction discovery. Put local guidance in the
nested `AGENTS.md` and workspace-wide guidance at the root. Both helpers
have async equivalents. Existing `thread_start` and `thread_fork` remain
available for more configuration options.

### Thread

- `run(input: str | Input, *, approval_mode=None, allow_no_prose=None, cwd=None, effort=None, model=None,
  output_schema=None, personality=None, sandbox: Sandbox | None = None, service_tier=UNSET,
  summary=None) -> TurnResult`
- `turn(input: str | Input, *, approval_mode=None, allow_no_prose=None, cwd=None, effort=None, model=None,
  output_schema=None, personality=None, run_independently=None, sandbox: Sandbox | None = None, service_tier=UNSET,
  summary=None) -> TurnHandle`
- `read(*, include_turns: bool = False) -> ThreadReadResponse`
- `recorded_answer(turn_id: str, *, fallback: RunInput | None = None) -> str | None`
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
- `recorded_answer(turn_id: str, *, fallback: RunInput | None = None) -> Awaitable[str | None]`
- `set_name(name: str) -> Awaitable[ThreadSetNameResponse]`
- `compact() -> Awaitable[ThreadCompactStartResponse]`

`recorded_answer(turn_id)` reads history and returns the exact text of a unique,
nonempty, uninterrupted final answer in the specified completed turn, without
starting a new turn. Missing, incomplete or ambiguous history returns `None`.
Pass `fallback=...` only if a miss should start an ordinary turn on that same
thread; read failures and thread ID mismatches propagate. Free-form
`run(...)` always starts a turn.

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

The `sandbox=` keyword remains accepted on thread lifecycle methods and turns:

```python
from openai_codex import Codex, Sandbox

with Codex() as codex:
    thread = codex.thread_start(sandbox=Sandbox.full_access)
    result = thread.run("Review the diff only.")
```

Local KD4 always runs with full filesystem and network access. The legacy
`Sandbox.read_only` and `Sandbox.workspace_write` values are accepted but do not
enforce restrictions; omission also uses full access. Approval policy and
project/hook trust remain independent. Other runtimes, including the upstream
bundled executable, may retain sandbox enforcement.

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

Handle close/abandon and early stream exit detach by default. Cancelling an active
async `stream()` or `run()` discards unread events when the route closes. Opt into
interruption with `close(cancel=True)` or `stream(cancel_on_close=True)`;
explicitly close/aclose the iterator on early exit. Interruption failures
propagate after route cleanup.
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

`Codex()` uses the installed pinned runtime. To select a local KD4 build, see
[local setup](../README.md#local-setup).

```python
from openai_codex import Codex

with Codex() as codex:
    thread = codex.thread_start(model="gpt-5.4", config={"model_reasoning_effort": "high"})
    result = thread.run("Say hello in one sentence.")
    print(result.final_response)
```
