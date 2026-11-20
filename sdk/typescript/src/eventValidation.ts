import type { ThreadEvent } from "./events";

type ObjectValue = Record<string, unknown>;
const object = (value: unknown): value is ObjectValue =>
  value !== null && typeof value === "object" && !Array.isArray(value);
const text = (value: unknown): value is string => typeof value === "string";
const oneOf = (value: unknown, options: readonly string[]) =>
  text(value) && options.includes(value);
const nullableNumber = (value: unknown) =>
  value === null || (typeof value === "number" && Number.isFinite(value));
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
        oneOf(value.status, ["in_progress", "completed", "failed", "declined"])
      );
    case "code_mode_cell":
      return (
        identity(value.call_id) &&
        identity(value.cell_id) &&
        text(value.error) &&
        oneOf(value.status, ["in_progress", "yielded", "completed", "failed", "terminated"])
      );
    case "code_mode_tool_call":
      return (
        identity(value.call_id) &&
        (value.parent_call_id === null || identity(value.parent_call_id)) &&
        identity(value.parent_cell_id) &&
        identity(value.runtime_tool_call_id) &&
        identity(value.tool) &&
        oneOf(value.status, ["in_progress", "completed", "failed"]) &&
        (value.success === null || typeof value.success === "boolean") &&
        (value.error === null || text(value.error)) &&
        (value.duration_ms === null || count(value.duration_ms)) &&
        (value.content_items === null ||
          (Array.isArray(value.content_items) &&
            value.content_items.every(
              (item) =>
                object(item) &&
                ((item.type === "inputText" && text(item.text)) ||
                  (item.type === "inputImage" && text(item.imageUrl))),
            )))
      );
    case "file_change":
      return (
        oneOf(value.status, ["in_progress", "completed", "failed", "declined"]) &&
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
            object(change) && text(change.path) && oneOf(change.kind, ["add", "delete", "update"]),
        )
      );
    case "mcp_tool_call":
      return (
        text(value.server) &&
        text(value.tool) &&
        oneOf(value.status, ["in_progress", "completed", "failed"]) &&
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
            oneOf(state.status, [
              "pending_init",
              "running",
              "interrupted",
              "completed",
              "errored",
              "shutdown",
              "not_found",
            ]) &&
            (state.message === null || text(state.message)) &&
            surfaced(state.surfaced_result) &&
            optionalText(state.last_agent_message),
        ) &&
        oneOf(value.tool, ["spawn_agent", "send_input", "resume_agent", "wait", "close_agent"]) &&
        oneOf(value.status, ["in_progress", "completed", "failed"])
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
            oneOf(value.disposition, ["failed", "interrupted", "transport_lost"]));
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
        valid = identity(value.item_id) && identity(value.call_id) && identity(value.kind);
        switch (value.kind) {
          case "command_output":
            valid &&=
              text(value.delta) &&
              (value.stream === null || oneOf(value.stream, ["stdout", "stderr"])) &&
              (value.decoding_lossy === null || typeof value.decoding_lossy === "boolean") &&
              typeof value.truncated === "boolean";
            break;
          case "agent_message":
            valid &&= text(value.delta) && typeof value.truncated === "boolean";
            break;
          case "mcp":
            valid &&=
              text(value.message) &&
              nullableNumber(value.progress) &&
              nullableNumber(value.total) &&
              typeof value.truncated === "boolean";
            break;
        }
        break;
    }
    if (value.type === "turn.completed" || value.type === "turn.failed") {
      const timing = value.timing;
      if (timing !== undefined && timing !== null) {
        valid &&= object(timing);
        if (object(timing) && timing.finalAnswer !== undefined) {
          const answer = timing.finalAnswer;
          valid &&=
            object(answer) &&
            (answer.message === null ||
              (object(answer.message) &&
                identity(answer.message.itemId) &&
                text(answer.message.text) &&
                identity(answer.message.completionAttemptId) &&
                oneOf(answer.message.origin, ["assistant_final", "legacy_unphased", "plan"])));
        }
      }
    }
  }
  if (!valid) throw new Error("Invalid turn stream: malformed known event envelope");
  return value as ThreadEvent;
}

/** Match the equivalent UUID spellings accepted by the CLI, or an opaque name. */
export function canonicalThreadId(value: string): string | null {
  if (value.length === 32 && /^[0-9a-f]{32}$/i.test(value)) return value.toLowerCase();
  let hyphenated = value;
  if (value.length === 38 && value.startsWith("{") && value.endsWith("}")) {
    hyphenated = value.slice(1, -1);
  } else if (value.length === 45 && value.slice(0, 9).toLowerCase() === "urn:uuid:") {
    hyphenated = value.slice(9);
  }
  if (
    hyphenated.length !== 36 ||
    !/^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/i.test(hyphenated)
  )
    return null;
  return hyphenated.replaceAll("-", "").toLowerCase();
}
