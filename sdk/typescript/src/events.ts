// based on event types from codex-rs/exec/src/exec_events.rs

import type { ThreadItem } from "./items";

/** Emitted when a new thread is started as the first event. */
export type ThreadStartedEvent = {
  type: "thread.started";
  /** The identifier of the new thread. Can be used to resume the thread later. */
  thread_id: string;
};

/**
 * Emitted when a turn is started by sending a new prompt to the model.
 * A turn encompasses all events that happen while the agent is processing the prompt.
 */
export type TurnStartedEvent = {
  type: "turn.started";
};

/**
 * Token usage for the thread as of the turn's completion. The counts are
 * cumulative, so a resumed thread's usage includes its earlier turns.
 * Numeric fields are known subtotals. Treat a component as complete only when its
 * coverage equals receipts, all expected receipts were reported, and no saturation
 * occurred; absent coverage or absent reported_receipts remains unknown.
 */
export type Usage = {
  /** Absent means legacy unknown, not fully reported. Counts are receipt coverage. */
  coverage?: {
    receipts: number;
    /** Missing on legacy records; this is not inferred from the token sums. */
    reported_receipts?: number;
    /** Known cache-write subtotal; complete only when cache_write_receipts equals receipts. */
    cache_write_input_tokens?: number;
    cache_write_receipts?: number;
    input_tokens: number;
    cached_input_tokens: number;
    output_tokens: number;
    reasoning_output_tokens: number;
    total_tokens: number;
    invalid_receipts: number;
    saturated: boolean;
  };
  /** The number of input tokens used by the thread. */
  input_tokens: number;
  /** The number of cached input tokens used by the thread. */
  cached_input_tokens: number;
  /** The number of output tokens used by the thread. */
  output_tokens: number;
  /** The number of reasoning output tokens used by the thread. */
  reasoning_output_tokens?: number;
};

/** Authoritative owner-authored result, shared by turn and child completion. */
export type SurfacedToolResult = {
  adapter: string;
  value: unknown;
  canonicalMessage?: string;
};

/** Exact answer accepted by core after completion hooks and pending-input admission. */
export type AcceptedFinalMessage = {
  itemId: string;
  text: string;
  origin: "assistant_final" | "legacy_unphased" | "plan";
  completionAttemptId: string;
};

export type TurnTiming = Record<string, unknown> & {
  /** Present with null message means no accepted answer; absence is legacy unknown. */
  finalAnswer?: { message: AcceptedFinalMessage | null };
};

/** Emitted when a turn is completed. Typically right after the assistant's response. */
export type TurnCompletedEvent = {
  type: "turn.completed";
  /** Null means no accounting snapshot was received, not measured zero. */
  usage: Usage | null;
  /** Owner-supplied timing, absent on legacy producers or null when unavailable. */
  timing?: TurnTiming | null;
  /** Authoritative tool result delivered without a final assistant message. */
  surfaced_result?: SurfacedToolResult;
};

/** Indicates that a turn failed with an error. */
export type TurnFailedEvent = {
  type: "turn.failed";
  error: ThreadError;
  disposition?: "failed" | "interrupted" | "transport_lost";
  usage?: Usage | null;
  timing?: TurnTiming | null;
};

/** Bounded incremental evidence; item.completed remains authoritative. */
export type ItemProgressEvent = {
  type: "item.progress";
  item_id: string;
  call_id: string;
} & (
  | {
      /** Noncanonical model text; item.completed is the complete snapshot. */
      kind: "agent_message";
      delta: string;
      truncated: boolean;
    }
  | {
      kind: "command_output";
      delta: string;
      stream: "stdout" | "stderr" | null;
      decoding_lossy: boolean | null;
      truncated: boolean;
    }
  | {
      kind: "mcp";
      message: string;
      progress: number | null;
      total: number | null;
      truncated: boolean;
    }
);

/** Emitted when a new item is added to the thread. Typically the item is initially "in progress". */
export type ItemStartedEvent = {
  type: "item.started";
  item: ThreadItem;
};

/** Emitted when an item is updated. */
export type ItemUpdatedEvent = {
  type: "item.updated";
  item: ThreadItem;
};

/** Signals that an item has reached a terminal state—either success or failure. */
export type ItemCompletedEvent = {
  type: "item.completed";
  item: ThreadItem;
};

/** Fatal error emitted by the stream. */
export type ThreadError = {
  message: string;
  code?: string;
  phase?: string;
};

/** Represents an unrecoverable error emitted directly by the event stream. */
export type ThreadErrorEvent = {
  type: "error";
  message: string;
  code?: string;
  phase?: string;
};

/** Top-level JSONL events emitted by codex exec. */
export type ThreadEvent =
  | ThreadStartedEvent
  | TurnStartedEvent
  | TurnCompletedEvent
  | TurnFailedEvent
  | ItemStartedEvent
  | ItemUpdatedEvent
  | ItemProgressEvent
  | ItemCompletedEvent
  | ThreadErrorEvent;
