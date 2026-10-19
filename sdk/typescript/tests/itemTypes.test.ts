import { expect, it } from "@jest/globals";
import type { CollabAgentState, SurfacedToolResult } from "../src/index";

import type { CodexExec } from "../src/exec";
import type { ThreadEvent } from "../src/events";
import { Thread } from "../src/thread";
import type {
  CommandExecutionItem,
  FileChangeItem,
  McpToolCallItem,
  WebSearchAction,
  WebSearchItem,
} from "../src/items";

it("preserves nullable outcomes and search actions through the event stream", async () => {
  const command: CommandExecutionItem = {
    id: "command",
    type: "command_execution",
    command: "false",
    aggregated_output: "",
    exit_code: null,
    status: "declined",
  };
  const patch: FileChangeItem = {
    id: "patch",
    type: "file_change",
    changes: [],
    status: "in_progress",
  };
  const mcp: McpToolCallItem = {
    id: "mcp",
    type: "mcp_tool_call",
    server: "server",
    tool: "tool",
    arguments: {},
    result: null,
    error: null,
    status: "in_progress",
  };
  const actions: WebSearchAction[] = [
    { type: "search", query: "codex" },
    { type: "open_page", url: "https://example.com" },
    { type: "find_in_page", pattern: "needle" },
    { type: "other" },
  ];

  const searches: WebSearchItem[] = actions.map((action, index) => ({
    id: `search-${index}`,
    type: "web_search",
    query: "codex",
    action,
  }));
  const events: ThreadEvent[] = [
    { type: "item.completed", item: command },
    { type: "item.started", item: patch },
    { type: "item.completed", item: { ...patch, status: "declined" } },
    { type: "item.started", item: mcp },
    ...searches.map((item) => ({ type: "item.completed" as const, item })),
    { type: "turn.completed", usage: { input_tokens: 0, cached_input_tokens: 0, output_tokens: 0, reasoning_output_tokens: 0 } },
  ];
  const exec = {
    async *run(): AsyncGenerator<string> {
      for (const event of events) {
        yield JSON.stringify(event);
      }
    },
  } as unknown as CodexExec;
  const thread = new Thread(exec, {}, {});

  const streamed = await thread.runStreamed("search and run tools");
  const received = [];
  for await (const event of streamed.events) {
    received.push(event);
  }
  expect(received).toEqual(events);
  expect((await thread.run("search and run tools")).items).toEqual([
    command,
    { ...patch, status: "declined" },
    ...searches,
  ]);
});

// @ts-expect-error exit_code is required even while it is null.
const missingExitCode: CommandExecutionItem = {
  id: "command",
  type: "command_execution",
  command: "true",
  aggregated_output: "",
  status: "in_progress",
};
void missingExitCode;

// @ts-expect-error result and error are required nullable fields.
const missingMcpOutcomes: McpToolCallItem = {
  id: "mcp",
  type: "mcp_tool_call",
  server: "server",
  tool: "tool",
  arguments: {},
  status: "in_progress",
};
void missingMcpOutcomes;

it("exposes optional authoritative child results through public types", () => {
  const surface: SurfacedToolResult = {
    adapter: "clock", value: { utc: "2026-10-18T00:00:00Z" }, canonicalMessage: "midnight",
  };
  const current: CollabAgentState = {
    status: "completed", message: null, surfaced_result: surface, last_agent_message: "working",
  };
  const legacy: CollabAgentState = { status: "completed", message: "done" };
  expect(current.surfaced_result?.value).toEqual({ utc: "2026-10-18T00:00:00Z" });
  expect(current.surfaced_result?.canonicalMessage).toBe("midnight");
  expect(current.last_agent_message).toBe("working");
  expect(legacy.surfaced_result).toBeUndefined();
});
