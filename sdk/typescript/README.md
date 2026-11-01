# Codex SDK

Embed the Codex agent in your workflows and apps.

The TypeScript SDK wraps the local `codex` CLI. It spawns the CLI and exchanges JSONL
events over stdin/stdout. This KD4 checkout is Windows-only; its package names are
inherited from upstream and do not identify a published KD4 SDK.

## Local setup

Requires Windows and Node.js 18+. In an already configured checkout, build the SDK
from the repository root:

```powershell
pnpm --filter @openai/codex-sdk build
```

Use `codexPathOverride` to select your local KD4 `codex.exe` explicitly. Without
an override, the SDK resolves a locally installed `@openai/codex` package and its
matching native package, not a `codex` executable on `PATH`. See the
[local npm packaging guide](../../codex-cli/scripts/README.md) when staging SDK
and CLI tarballs. Installing `@openai/codex-sdk` from the public registry does not
install this checkout.

## Quickstart

```typescript
import { Codex } from "@openai/codex-sdk";

const codex = new Codex({ codexPathOverride: "C:\\path\\to\\codex.exe" });
const thread = codex.startThread();
const turn = await thread.run("Diagnose the compiler failure and propose a fix");

console.log(turn.finalResponse);
console.log(turn.items);
```

Call `run()` repeatedly on the same `Thread` instance to continue that conversation.
Await each turn or close its event iterator before starting another on the same
`Thread`; overlapping consumed runs are rejected. Separate `Thread` objects may
run concurrently. Constructing an unconsumed stream does not reserve a turn.

```typescript
const nextTurn = await thread.run("Implement the fix");
```

### Streaming responses

`run()` buffers events until the turn finishes. To react to intermediate progress—tool calls,
streaming responses, and file change notifications—use `runStreamed()` instead, which returns an
async generator of structured events.

Final text uses the terminal `surfaced_result.canonicalMessage` when present,
otherwise the terminal `timing.finalAnswer` receipt. A present final-answer receipt
with no message suppresses visible-item fallback. Older streams fall back to the
latest uninterrupted final-answer (or phase-less) agent message, then a completed
`plan` item. A Plan is planning content, not verified implementation. `run()` also
returns available `timing`, `finalAnswer`, and `surfacedResult` metadata. Usage is
`null` when no accounting snapshot was received; counters remain cumulative for
the primary thread. Web-search `id` is the exec item identity; `call_id` preserves
the provider identity. Known malformed envelopes and lifecycle mutations after a
terminal are rejected. A fatal finalization error may follow `turn.completed`:
always drain the process and inspect its outcome.
If the stream reaches EOF without `turn.completed` or `turn.failed`, iteration
throws after delivering the events already received; partial output is not completion.

Live model text arrives as `item.progress` with `kind: "agent_message"` and `delta`.
It shares `item_id` with the enclosing `item.started` / `item.completed` records.
Each progress text payload is capped at 16 KiB of UTF-8 before JSON encoding and marks
display reduction with `truncated`. Serialized records also include metadata and may be larger;
progress is provisional, not accepted final text. Replace it with the complete
`item.completed` snapshot rather than appending the snapshot to the deltas. `run()`
still resolves only after terminal completion and retains observed partial text in
`TurnRunError` on failure. Human exec progress is on stderr; canonical stdout and
last-message files still contain only the accepted final answer.

```typescript
const { events } = await thread.runStreamed("Diagnose the compiler failure and propose a fix");

for await (const event of events) {
  switch (event.type) {
    case "item.completed":
      console.log("item", event.item);
      break;
    case "turn.completed":
      console.log("usage", event.usage);
      break;
  }
}
```

### Structured output

The Codex agent can produce a JSON response that conforms to a specified schema. The schema can be
provided for each turn as a plain JSON object.

```typescript
const schema = {
  type: "object",
  properties: {
    summary: { type: "string" },
    status: { type: "string", enum: ["ok", "action_required"] },
  },
  required: ["summary", "status"],
  additionalProperties: false,
} as const;

const turn = await thread.run("Summarize repository status", { outputSchema: schema });
console.log(turn.finalResponse);
```

You can also create a JSON schema from a [Zod schema](https://github.com/colinhacks/zod) using the
[`zod-to-json-schema`](https://www.npmjs.com/package/zod-to-json-schema) package and setting the
`target` to `"openAi"`.

```typescript
const schema = z.object({
  summary: z.string(),
  status: z.enum(["ok", "action_required"]),
});

const turn = await thread.run("Summarize repository status", {
  outputSchema: zodToJsonSchema(schema, { target: "openAi" }),
});
console.log(turn.finalResponse);
```

### Attaching images

Provide structured input entries when you need to include images alongside text. Text entries are
concatenated into the final prompt while image entries are passed to the Codex CLI via the repeatable
literal `--image-path` option. Commas in filenames are preserved; the CLI's legacy `--image` syntax
continues to accept comma-separated paths.

```typescript
const turn = await thread.run([
  { type: "text", text: "Describe these screenshots" },
  { type: "local_image", path: "./ui.png" },
  { type: "local_image", path: "./diagram.jpg" },
]);
```

### Resuming an existing thread

Threads are persisted under `CODEX_HOME/sessions` (normally
`%USERPROFILE%\.codex\sessions`). If you lose the in-memory `Thread` object,
reconstruct it with `resumeThread()` and keep going.

```typescript
const savedThreadId = process.env.CODEX_THREAD_ID!;
const thread = codex.resumeThread(savedThreadId);
await thread.run("Implement the fix");
```

### Working directory controls

Codex runs in the current working directory by default. To avoid unrecoverable errors, Codex
requires the working directory to be a Git repository. You can skip the Git repository check by
passing the `skipGitRepoCheck` option when creating a thread.

```typescript
const thread = codex.startThread({
  workingDirectory: "C:\\path\\to\\project",
  skipGitRepoCheck: true,
});
```

### Controlling the Codex CLI environment

By default, the Codex CLI inherits the Node.js process environment. Provide the optional `env`
parameter when instantiating the
`Codex` client to fully control which variables the CLI receives—useful for sandboxed hosts like
Electron apps.

```typescript
const codex = new Codex({
  codexPathOverride: "C:\\path\\to\\codex.exe",
  env: {
    Path: "C:\\Windows\\System32",
  },
});
```

The SDK still injects its required variables (such as `CODEX_API_KEY`) on top of the environment you
provide. If you set
`baseUrl`, the SDK passes it as a `--config openai_base_url=...` override.

### Passing `--config` overrides

Use the `config` option to provide additional Codex CLI configuration overrides. The SDK accepts a
JSON object, flattens it
into dotted paths, and serializes values as TOML literals before passing them as repeated `--config
key=value` flags.

```typescript
const codex = new Codex({
  codexPathOverride: "C:\\path\\to\\codex.exe",
  config: {
    show_raw_agent_reasoning: true,
    sandbox_workspace_write: { network_access: true },
  },
});
```

Thread options still take precedence for overlapping settings because they are emitted after these
global overrides.

### Intentionally empty final answers

Set the turn-scoped `allowNoProse: true` option only when the request permits an
exactly empty final answer. It does not waive requested work, output schemas, or
completion hooks.

## Development

This checkout retains the SDK source and runnable samples, but no automated test suite or Jest
configuration. Build, lint, and type-check scripts remain available in `package.json`. The sample
dependencies (`ts-node`, `zod`, and `zod-to-json-schema`) are retained.
