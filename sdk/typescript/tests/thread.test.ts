import { describe, expect, it } from "@jest/globals";

import type { CodexExec } from "../src/exec";
import type { CodeModeCellItem, CommandExecutionItem, ContextCompactionItem } from "../src/index";
import { Thread } from "../src/thread";

describe("Thread", () => {
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
