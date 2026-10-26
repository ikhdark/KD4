# Codex SDK

Embed the Codex agent in your workflows and apps.

The TypeScript SDK wraps the `codex` CLI from `@openai/codex`. It spawns the CLI and exchanges JSONL
events over stdin/stdout.

## Installation

```bash
npm install @openai/codex-sdk
```

Requires Node.js 18+.

## Quickstart

```typescript
import { Codex } from "@openai/codex-sdk";

const codex = new Codex();
const thread = codex.startThread();
const turn = await thread.run("Diagnose the test failure and propose a fix");

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

Final text prefers the latest completed agent message, falling back to a completed
`plan` item. A Plan is planning content, not verified implementation. Usage is
`null` when no accounting snapshot was received; counters remain cumulative for
the primary thread. Web-search `id` is the exec item identity; `call_id` preserves
the provider identity. Known malformed envelopes and lifecycle mutations after a
terminal are rejected. A fatal finalization error may follow `turn.completed`:
always drain the process and inspect its outcome.

```typescript
const { events } = await thread.runStreamed("Diagnose the test failure and propose a fix");

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
concatenated into the final prompt while image entries are passed to the Codex CLI via `--image`.

```typescript
const turn = await thread.run([
  { type: "text", text: "Describe these screenshots" },
  { type: "local_image", path: "./ui.png" },
  { type: "local_image", path: "./diagram.jpg" },
]);
```

### Resuming an existing thread

Threads are persisted in `~/.codex/sessions`. If you lose the in-memory `Thread` object, reconstruct
it with `resumeThread()` and keep going.

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
  workingDirectory: "/path/to/project",
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
  env: {
    PATH: "/usr/local/bin",
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
  config: {
    show_raw_agent_reasoning: true,
    sandbox_workspace_write: { network_access: true },
  },
});
```

Thread options still take precedence for overlapping settings because they are emitted after these
global overrides.

## Development

This checkout retains the SDK source and runnable samples, but no automated test suite or Jest
configuration. Build, lint, and type-check scripts remain available in `package.json`. The sample
dependencies (`ts-node`, `zod`, and `zod-to-json-schema`) are retained.
