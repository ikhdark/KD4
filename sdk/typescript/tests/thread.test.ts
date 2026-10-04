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
import { Thread } from "../src/thread";

describe("Thread", () => {
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
    const events = [
      { type: "item.started", item },
      { type: "item.completed", item },
    ];
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
