import { expect, it } from "@jest/globals";

import type {
  CommandExecutionItem,
  FileChangeItem,
  McpToolCallItem,
  WebSearchAction,
} from "../src/items";

it("represents every audited exec item edge", () => {
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

  expect([command.status, patch.status, mcp.result, actions.length]).toEqual([
    "declined",
    "in_progress",
    null,
    4,
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
