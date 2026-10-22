import { promises as fs } from "node:fs";
import { describe, expect, it, jest } from "@jest/globals";

import type { CodexExec } from "../src/exec";
import type {
  CodeModeCellItem,
  CommandExecutionItem,
  ContextCompactionItem,
  RunResult,
  ThreadEvent,
} from "../src/index";
import { Thread, TurnRunError } from "../src/thread";

const completed = {
  type: "turn.completed" as const,
  usage: { input_tokens: 0, cached_input_tokens: 0, output_tokens: 0 },
};

describe("Thread", () => {
  it.each([
    { type: "turn.completed" },
    { type: "turn.completed", usage: "invalid" },
    { type: "turn.completed", usage: [] },
    { ...completed, usage: { ...completed.usage, input_tokens: -1 } },
    { ...completed, usage: { ...completed.usage, output_tokens: 1.5 } },
    { type: "item.completed", item: { id: "x", type: "agent_message", text: 17 } },
    { type: "turn.failed", error: "bad" },
    null,
  ])("rejects malformed critical envelopes: %j", async (bad) => {
    const exec = {
      async *run() {
        yield JSON.stringify(bad);
      },
    } as unknown as CodexExec;
    await expect(new Thread(exec, {}, {}).run("hello")).rejects.toBeInstanceOf(TurnRunError);
  });

  it("seals the result but accepts fatal invocation evidence after completion", async () => {
    const item = { id: "before", type: "agent_message", text: "BEFORE" };
    const fatal = {
      type: "error",
      message: "unsaved",
      code: "session_persistence_unconfirmed",
      phase: "finalization",
    };
    for (const late of [
      { type: "item.completed", item: { ...item, id: "late", text: "AFTER" } },
      fatal,
    ]) {
      const exec = {
        async *run() {
          for (const event of [{ type: "item.completed", item }, completed, late])
            yield JSON.stringify(event);
        },
      } as unknown as CodexExec;
      await expect(new Thread(exec, {}, {}).run("hello")).rejects.toMatchObject({
        finalResponse: "BEFORE",
        items: [item],
        terminal: completed,
        ...(late === fatal
          ? { streamError: { message: "unsaved", code: fatal.code, phase: fatal.phase } }
          : {}),
      });
    }
  });

  it("returns plan-only text, preserves agent precedence and honest missing usage", async () => {
    for (const withAgent of [false, true]) {
      const exec = {
        async *run() {
          if (withAgent)
            yield JSON.stringify({
              type: "item.completed",
              item: { id: "a", type: "agent_message", text: "answer" },
            });
          yield JSON.stringify({
            type: "item.completed",
            item: { id: "p", type: "plan", text: "Plan α\nVerify β" },
          });
          yield JSON.stringify({ type: "turn.completed", usage: null });
        },
      } as unknown as CodexExec;
      const turn = await new Thread(exec, {}, {}).run("hello");
      expect(turn.finalResponse).toBe(withAgent ? "answer" : "Plan α\nVerify β");
      expect(turn.usage).toBeNull();
    }
  });

  it("accepts equivalent UUID spellings but rejects identity switches", async () => {
    const id = "12345678-1234-4234-8234-123456789abc";
    for (const returned of [
      id.toUpperCase().replace(/-/g, ""),
      "aaaaaaaa-1234-4234-8234-123456789abc",
    ]) {
      const exec = {
        async *run() {
          yield JSON.stringify({ type: "thread.started", thread_id: returned });
          yield JSON.stringify(completed);
        },
      } as unknown as CodexExec;
      const result = new Thread(exec, {}, {}, id).run("hello");
      if (returned.startsWith("a")) await expect(result).rejects.toThrow("thread identity changed");
      else await expect(result).resolves.toMatchObject({ usage: completed.usage });
    }
  });

  it("passes unknown future events through streamed parsing", async () => {
    const future = { type: "future.notification", version_hint: 42 };
    const exec = {
      async *run() {
        yield JSON.stringify(future);
        yield JSON.stringify(completed);
      },
    } as unknown as CodexExec;
    const { events } = await new Thread(exec, {}, {}).runStreamed("hello");
    const received = [];
    for await (const event of events) received.push(event);
    expect(received).toEqual([future, completed]);
  });

  it.each(["turn.failed", "turn.completed"] as const)(
    "rejects another terminal after %s without replacing its accounting",
    async (type) => {
      const first =
        type === "turn.failed"
          ? {
              type,
              error: { message: "original failure" },
              usage: null,
              timing: { duration_ms: 0 },
            }
          : completed;
      const exec = {
        async *run(): AsyncGenerator<string> {
          yield JSON.stringify(first);
          yield JSON.stringify(completed);
        },
      } as unknown as CodexExec;
      await expect(new Thread(exec, {}, {}).run("hello")).rejects.toMatchObject({
        name: "TurnRunError",
        terminal: first,
        cause: new Error("Invalid turn stream: multiple terminal events received"),
      });
    },
  );

  it.each([undefined, null, { duration_ms: 0 }])(
    "preserves timing availability: %j",
    async (timing) => {
      const exec = {
        async *run() {
          yield JSON.stringify({ ...completed, ...(timing === undefined ? {} : { timing }) });
        },
      } as unknown as CodexExec;
      const result = await new Thread(exec, {}, {}).run("hello");
      expect(result.timing).toEqual(timing);
      expect("timing" in result).toBe(timing !== undefined);
    },
  );

  it("retains partial results and terminal accounting across error then nonzero exit", async () => {
    const item = { id: "partial", type: "agent_message", text: "partial" };
    const terminal = {
      type: "turn.failed",
      error: { message: "lost" },
      disposition: "transport_lost",
      usage: null,
      timing: { duration_ms: 0 },
    };
    const cause = new Error("Codex Exec exited with code 1");
    const exec = {
      async *run() {
        for (const event of [
          { type: "thread.started", thread_id: "failure-thread" },
          { type: "item.completed", item },
          { type: "error", message: "transport" },
          terminal,
        ])
          yield JSON.stringify(event);
        throw cause;
      },
    } as unknown as CodexExec;
    try {
      await new Thread(exec, {}, {}).run("hello");
      throw new Error("expected failure");
    } catch (error) {
      expect(error).toBeInstanceOf(TurnRunError);
      expect(error).toMatchObject({
        threadId: "failure-thread",
        items: [item],
        finalResponse: "partial",
        terminal,
        cause,
        usage: null,
        disposition: "transport_lost",
        timing: { duration_ms: 0 },
      });
    }
  });

  it("aborts a fatal producer that never sends terminal accounting", async () => {
    jest.useFakeTimers();
    let aborted = false;
    const exec = {
      async *run({ signal }: { signal: AbortSignal }) {
        yield JSON.stringify({ type: "error", message: "fatal" });
        await new Promise<void>((resolve) =>
          signal.addEventListener(
            "abort",
            () => {
              aborted = true;
              resolve();
            },
            { once: true },
          ),
        );
      },
    } as unknown as CodexExec;
    try {
      const result = expect(new Thread(exec, {}, {}).run("hello")).rejects.toMatchObject({
        message: "fatal",
        terminal: undefined,
      });
      await jest.advanceTimersByTimeAsync(1_000);
      await result;
      expect(aborted).toBe(true);
    } finally {
      jest.useRealTimers();
    }
  });

  it.each([
    { events: [] },
    { events: [{ type: "turn.started" }] },
    {
      events: [
        { type: "item.completed", item: { id: "partial", type: "agent_message", text: "partial" } },
      ],
    },
  ])("rejects an incomplete stream: %j", async ({ events }) => {
    let schemaPath: string | undefined;
    const exec = {
      async *run(args: { outputSchemaFile?: string }): AsyncGenerator<string> {
        schemaPath = args.outputSchemaFile;
        for (const event of events) yield JSON.stringify(event);
      },
    } as unknown as CodexExec;
    await expect(new Thread(exec, {}, {}).run("hello", { outputSchema: {} })).rejects.toThrow(
      "Incomplete turn stream",
    );
    await expect(fs.stat(schemaPath!)).rejects.toMatchObject({ code: "ENOENT" });
  });

  it.each([
    null,
    [],
    "schema",
    42,
    true,
    new Date("2026-01-01T00:00:00Z"),
    { toJSON: () => undefined },
    { toJSON: () => [] },
    { toJSON: () => null },
    { toJSON: () => "{}" },
  ])("rejects a non-object output schema before starting exec: %j", async (outputSchema) => {
    const exec = { run: jest.fn() };
    const thread = new Thread(exec as unknown as CodexExec, {}, {});

    await expect(thread.run("hello", { outputSchema })).rejects.toThrow(
      "outputSchema must be a plain JSON object",
    );
    expect(exec.run).not.toHaveBeenCalled();
  });

  it.each([{}, Object.assign(Object.create(null), { type: "object" })])(
    "writes and removes a plain output schema: %j",
    async (outputSchema) => {
      let schemaPath: string | undefined;
      let writtenSchema: unknown;
      const exec = {
        async *run(args: { outputSchemaFile?: string }): AsyncGenerator<string> {
          schemaPath = args.outputSchemaFile;
          writtenSchema = JSON.parse(await fs.readFile(schemaPath!, "utf8"));
          yield JSON.stringify({ type: "thread.started", thread_id: "schema-thread" });
          yield JSON.stringify(completed);
        },
      } as unknown as CodexExec;
      const thread = new Thread(exec, {}, {});

      await thread.run("hello", { outputSchema });

      expect(writtenSchema).toEqual(outputSchema);
      expect(schemaPath).toEqual(expect.any(String));
      await expect(fs.stat(schemaPath!)).rejects.toMatchObject({ code: "ENOENT" });
    },
  );

  it("removes the output schema when input normalization fails", async () => {
    const mkdtemp = jest.spyOn(fs, "mkdtemp");
    const exec = { run: jest.fn() };
    const thread = new Thread(exec as unknown as CodexExec, {}, {});
    try {
      await expect(
        thread.run(
          [
            {
              type: "text",
              get text(): string {
                throw new Error("input failed");
              },
            },
          ],
          { outputSchema: { type: "object" } },
        ),
      ).rejects.toThrow("input failed");

      expect(exec.run).not.toHaveBeenCalled();
      expect(mkdtemp).toHaveBeenCalledTimes(1);
      const schemaDir = (await mkdtemp.mock.results[0]!.value) as string;
      await expect(fs.stat(schemaDir)).rejects.toMatchObject({ code: "ENOENT" });
    } finally {
      const created = mkdtemp.mock.results[0];
      mkdtemp.mockRestore();
      if (created?.type === "return") {
        await fs.rm(await created.value, { recursive: true, force: true });
      }
    }
  });

  it.each([null, undefined, "", "Owner-authored final response"])(
    "preserves terminal tool results with canonical message %j",
    async (canonicalMessage) => {
      const surfacedResult =
        canonicalMessage === null
          ? undefined
          : {
              adapter: "owner",
              value: { answer: 42 },
              ...(canonicalMessage === undefined ? {} : { canonicalMessage }),
            };
      const item = { id: "item_0", type: "agent_message" as const, text: "Earlier message" };
      const usage = {
        input_tokens: 10,
        cached_input_tokens: 0,
        output_tokens: 5,
        reasoning_output_tokens: 0,
      };
      const events: ThreadEvent[] = [
        { type: "item.completed", item },
        {
          type: "turn.completed",
          usage,
          ...(surfacedResult ? { surfaced_result: surfacedResult } : {}),
        },
      ];
      const exec = {
        async *run(): AsyncGenerator<string> {
          for (const event of events) yield JSON.stringify(event);
        },
      } as unknown as CodexExec;
      const thread = new Thread(exec, {}, {});
      const result: RunResult = await thread.run("hello");

      expect(result).toEqual({
        items: [item],
        finalResponse: surfacedResult ? (canonicalMessage ?? "") : item.text,
        usage,
        ...(surfacedResult ? { surfacedResult } : {}),
      });
      const streamed = await thread.runStreamed("hello");
      const received = [];
      for await (const event of streamed.events) received.push(event);
      expect(received).toEqual(events);
    },
  );

  it("preserves compaction lifecycle events and collects the completed item once", async () => {
    const item: ContextCompactionItem = { id: "item_0", type: "context_compaction" };
    const events = [{ type: "item.started", item }, { type: "item.completed", item }, completed];
    const exec = {
      async *run(): AsyncGenerator<string> {
        for (const event of events) yield JSON.stringify(event);
      },
    } as unknown as CodexExec;
    const thread = new Thread(exec, {}, {});
    const result = await thread.run("hello");
    expect(result.items).toEqual([item]);
    expect(result.finalResponse).toBe("");
    const streamed = await thread.runStreamed("hello");
    const received = [];
    for await (const event of streamed.events) received.push(event);
    expect(received).toEqual(events);
  });

  it("preserves failed code-mode cells and nested command identifiers", async () => {
    const cell: CodeModeCellItem = {
      id: "item_0",
      type: "code_mode_cell",
      call_id: "exec-call",
      cell_id: "cell-1",
      status: "failed",
      error: "runtime timeout",
    };
    const command: CommandExecutionItem = {
      id: "item_1",
      type: "command_execution",
      command: "echo hello",
      aggregated_output: "hello",
      exit_code: 0,
      status: "completed",
      call_id: "nested-call",
      parent_call_id: cell.call_id,
      parent_cell_id: cell.cell_id,
      runtime_tool_call_id: "runtime-call",
      execution_id: "execution-1",
    };
    const exec = {
      async *run(): AsyncGenerator<string> {
        for (const item of [cell, command]) {
          yield JSON.stringify({ type: "item.completed", item });
        }
        yield JSON.stringify(completed);
      },
    } as unknown as CodexExec;
    const thread = new Thread(exec, {}, {});
    expect((await thread.run("hello")).items).toEqual([cell, command]);
    const streamed = await thread.runStreamed("hello");
    const items = [];
    for await (const event of streamed.events) {
      if (event.type === "item.completed") items.push(event.item);
    }
    expect(items).toEqual([cell, command]);
  });

  it.each(["fatal protocol failure", ""])(
    "rejects a fatal error with message %j",
    async (message) => {
      const exec = {
        async *run(): AsyncGenerator<string> {
          yield JSON.stringify({ type: "error", message });
        },
      } as unknown as CodexExec;
      const thread = new Thread(exec, {}, {});

      await expect(thread.run("hello")).rejects.toThrow(new Error(message));
    },
  );
});
