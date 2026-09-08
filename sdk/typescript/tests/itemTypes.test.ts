import { expect, it } from "@jest/globals";

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
    { type: "item.started", item: mcp },
    ...searches.map((item) => ({ type: "item.completed" as const, item })),
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
  expect((await thread.run("search and run tools")).items).toEqual([command, ...searches]);
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
