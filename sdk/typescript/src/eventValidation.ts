import type { ThreadEvent } from "./events";

type ObjectValue = Record<string, unknown>;
const object = (value: unknown): value is ObjectValue =>
  value !== null && typeof value === "object" && !Array.isArray(value);
const text = (value: unknown): value is string => typeof value === "string";
const identity = (value: unknown) => text(value) && value.trim().length > 0;
const count = (value: unknown) =>
  typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
const optionalText = (value: unknown) => value === undefined || text(value);
const error = (value: unknown) =>
  object(value) && text(value.message) && optionalText(value.code) && optionalText(value.phase);
const usage = (value: unknown) =>
  value === null ||
  (object(value) &&
    count(value.input_tokens) &&
    count(value.cached_input_tokens) &&
    count(value.output_tokens) &&
    (value.reasoning_output_tokens === undefined || count(value.reasoning_output_tokens)) &&
    (value.coverage === undefined ||
      (object(value.coverage) &&
        [
          "receipts",
          "input_tokens",
          "cached_input_tokens",
          "output_tokens",
          "reasoning_output_tokens",
          "total_tokens",
          "invalid_receipts",
        ].every((key) => count((value.coverage as ObjectValue)[key])) &&
        typeof value.coverage.saturated === "boolean")));
const surfaced = (value: unknown) =>
  value === undefined ||
  (object(value) &&
    text(value.adapter) &&
    "value" in value &&
    optionalText(value.canonicalMessage));

function validItem(value: unknown): boolean {
  if (!object(value) || !identity(value.id) || !text(value.type)) return false;
  switch (value.type) {
    case "agent_message":
      return (
        text(value.text) &&
        (value.phase === undefined ||
          value.phase === "commentary" ||
          value.phase === "partial_answer" ||
          value.phase === "final_answer")
      );
    case "plan":
    case "reasoning":
      return text(value.text);
    case "error":
      return text(value.message);
    case "command_execution":
      return (
        text(value.command) &&
        text(value.aggregated_output) &&
        (value.exit_code === null ||
          (typeof value.exit_code === "number" && Number.isSafeInteger(value.exit_code))) &&
        ["in_progress", "completed", "failed", "declined"].includes(String(value.status))
      );
    case "code_mode_cell":
      return (
        identity(value.call_id) &&
        identity(value.cell_id) &&
        text(value.error) &&
        ["in_progress", "yielded", "completed", "failed", "terminated"].includes(
          String(value.status),
        )
      );
    case "file_change":
      return (
        ["in_progress", "completed", "failed", "declined"].includes(String(value.status)) &&
        (value.effects === undefined ||
          (object(value.effects) &&
            typeof value.effects.exact === "boolean" &&
            Array.isArray(value.effects.changes) &&
            value.effects.changes.every(
              (change) =>
                object(change) &&
                identity(change.path) &&
                (change.before === null || text(change.before)) &&
                (change.after === null || text(change.after)),
            ))) &&
        Array.isArray(value.changes) &&
        value.changes.every(
          (change) =>
            object(change) &&
            text(change.path) &&
            ["add", "delete", "update"].includes(String(change.kind)),
        )
      );
    case "mcp_tool_call":
      return (
        text(value.server) &&
        text(value.tool) &&
        ["in_progress", "completed", "failed"].includes(String(value.status)) &&
        (value.error === null || error(value.error)) &&
        (value.result === null || (object(value.result) && Array.isArray(value.result.content)))
      );
    case "collab_tool_call":
      return (
        identity(value.sender_thread_id) &&
        Array.isArray(value.receiver_thread_ids) &&
        value.receiver_thread_ids.every(identity) &&
        object(value.agents_states) &&
        (value.prompt === null || text(value.prompt)) &&
        Object.values(value.agents_states).every(
          (state) =>
            object(state) &&
            [
              "pending_init",
              "running",
              "interrupted",
              "completed",
              "errored",
              "shutdown",
              "not_found",
            ].includes(String(state.status)) &&
            (state.message === null || text(state.message)) &&
            surfaced(state.surfaced_result) &&
            optionalText(state.last_agent_message),
        ) &&
        ["spawn_agent", "send_input", "resume_agent", "wait", "close_agent"].includes(
          String(value.tool),
        ) &&
        ["in_progress", "completed", "failed"].includes(String(value.status))
      );
    case "web_search":
      return (
        text(value.query) &&
        object(value.action) &&
        text(value.action.type) &&
        optionalText(value.call_id) &&
        optionalText(value.action.query) &&
        optionalText(value.action.url) &&
        optionalText(value.action.pattern) &&
        (value.action.queries === undefined ||
          (Array.isArray(value.action.queries) && value.action.queries.every(text)))
      );
    case "todo_list":
      return (
        Array.isArray(value.items) &&
        value.items.every(
          (item) => object(item) && text(item.text) && typeof item.completed === "boolean",
        )
      );
    default:
      return true; // Additive item kinds remain forward-compatible.
  }
}

/** Validate known critical envelopes, without rejecting future event names. */
export function validateThreadEvent(value: unknown): ThreadEvent {
  let valid = object(value) && text(value.type);
  if (valid && object(value)) {
    switch (value.type) {
      case "thread.started":
        valid = identity(value.thread_id);
        break;
      case "turn.completed":
        valid = "usage" in value && usage(value.usage) && surfaced(value.surfaced_result);
        break;
      case "turn.failed":
        valid =
          error(value.error) &&
          (value.usage === undefined || usage(value.usage)) &&
          (value.disposition === undefined ||
            ["failed", "interrupted", "transport_lost"].includes(String(value.disposition)));
        break;
      case "error":
        valid = error(value);
        break;
      case "item.started":
      case "item.updated":
      case "item.completed":
        valid = validItem(value.item);
        break;
      case "item.progress":
        valid =
          identity(value.item_id) &&
          identity(value.call_id) &&
          (value.kind === "command_output" || value.kind === "agent_message"
            ? text(value.delta)
            : value.kind === "mcp"
              ? text(value.message)
              : true);
        if (value.kind === "agent_message") valid &&= typeof value.truncated === "boolean";
        break;
    }
    if (
      (value.type === "turn.completed" || value.type === "turn.failed") &&
      value.timing !== undefined &&
      value.timing !== null &&
      !object(value.timing)
    )
      valid = false;
    if (object(value.timing) && value.timing.finalAnswer !== undefined) {
      const answer = value.timing.finalAnswer;
      valid &&=
        object(answer) &&
        (answer.message === null ||
          (object(answer.message) &&
            identity(answer.message.itemId) &&
            text(answer.message.text) &&
            identity(answer.message.completionAttemptId) &&
            ["assistant_final", "legacy_unphased", "plan"].includes(
              String(answer.message.origin),
            )));
    }
  }
  if (!valid) throw new Error("Invalid turn stream: malformed known event envelope");
  return value as ThreadEvent;
}

/** Match the equivalent UUID spellings accepted by the CLI, or an opaque name. */
export function canonicalThreadId(value: string): string | null {
  const stripped = value
    .replace(/^urn:uuid:/i, "")
    .replace(/^\{(.*)\}$/, "$1")
    .replace(/-/g, "");
  return /^[0-9a-f]{32}$/i.test(stripped) ? stripped.toLowerCase() : null;
}
