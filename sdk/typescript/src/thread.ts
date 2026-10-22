import { CodexOptions } from "./codexOptions";
import { ThreadEvent, ThreadError, TurnCompletedEvent, TurnFailedEvent, Usage } from "./events";
import { CodexExec } from "./exec";
import { ThreadItem } from "./items";
import { ThreadOptions } from "./threadOptions";
import { TurnOptions } from "./turnOptions";
import { createOutputSchemaFile } from "./outputSchemaFile";
import { canonicalThreadId, validateThreadEvent } from "./eventValidation";

/** Completed turn. */
export type Turn = {
  items: ThreadItem[];
  finalResponse: string;
  usage: Usage | null;
  /** Owner timing; absence, unavailable (null), and measured zero remain distinct. */
  timing?: TurnCompletedEvent["timing"];
  /** Authoritative typed result when a tool directly completes the turn. */
  surfacedResult?: TurnCompletedEvent["surfaced_result"];
  finalAnswer?: NonNullable<TurnCompletedEvent["timing"]>["finalAnswer"];
};

/** Alias for `Turn` to describe the result of `run()`. */
export type RunResult = Turn;

/** Failed or incomplete turn, including all observations collected before failure. */
export class TurnRunError extends Error {
  readonly name = "TurnRunError";
  constructor(
    message: string,
    public readonly threadId: string | null,
    public readonly items: ThreadItem[],
    public readonly finalResponse: string,
    public readonly terminal: TurnFailedEvent | TurnCompletedEvent | undefined,
    public readonly streamError: ThreadError | undefined,
    options?: ErrorOptions,
  ) {
    super(message, options);
  }

  get usage() {
    return this.terminal?.usage;
  }
  get timing() {
    return this.terminal?.timing;
  }
  get disposition() {
    return this.terminal?.type === "turn.failed" ? this.terminal.disposition : undefined;
  }
}

/** The result of the `runStreamed` method. */
export type StreamedTurn = {
  events: AsyncGenerator<ThreadEvent>;
};

/** Alias for `StreamedTurn` to describe the result of `runStreamed()`. */
export type RunStreamedResult = StreamedTurn;

/** An input to send to the agent. */
export type UserInput =
  | {
      type: "text";
      text: string;
    }
  | {
      type: "local_image";
      path: string;
    };

export type Input = string | UserInput[];

/** Represent a thread of conversation with the agent. One thread can have multiple consecutive turns. */
export class Thread {
  private _exec: CodexExec;
  private _options: CodexOptions;
  private _id: string | null;
  private _threadOptions: ThreadOptions;
  private _running = false;

  /** Returns the ID of the thread. Populated after the first turn starts. */
  public get id(): string | null {
    return this._id;
  }

  /* @internal */
  constructor(
    exec: CodexExec,
    options: CodexOptions,
    threadOptions: ThreadOptions,
    id: string | null = null,
  ) {
    this._exec = exec;
    this._options = options;
    this._id = id;
    this._threadOptions = threadOptions;
  }

  /** Provides the input to the agent and streams events as they are produced during the turn. */
  async runStreamed(input: Input, turnOptions: TurnOptions = {}): Promise<StreamedTurn> {
    return { events: this.runStreamedInternal(input, turnOptions) };
  }

  private async *runStreamedInternal(
    input: Input,
    turnOptions: TurnOptions = {},
  ): AsyncGenerator<ThreadEvent> {
    if (this._running) {
      throw new Error(
        "A turn is already running on this Thread; finish or close its stream before starting another.",
      );
    }
    this._running = true;
    try {
      const { schemaPath, cleanup } = await createOutputSchemaFile(turnOptions.outputSchema);
      try {
        const options = this._threadOptions;
        const { prompt, images } = normalizeInput(input);
        const generator = this._exec.run({
          input: prompt,
          baseUrl: this._options.baseUrl,
          apiKey: this._options.apiKey,
          threadId: this._id,
          images,
          model: options?.model,
          sandboxMode: options?.sandboxMode,
          workingDirectory: options?.workingDirectory,
          skipGitRepoCheck: options?.skipGitRepoCheck,
          outputSchemaFile: schemaPath,
          modelReasoningEffort: options?.modelReasoningEffort,
          signal: turnOptions.signal,
          networkAccessEnabled: options?.networkAccessEnabled,
          webSearchMode: options?.webSearchMode,
          webSearchEnabled: options?.webSearchEnabled,
          approvalPolicy: options?.approvalPolicy,
          additionalDirectories: options?.additionalDirectories,
        });
        let terminalSeen = false;
        let establishedId = this._id === null ? null : canonicalThreadId(this._id);
        for await (const item of generator) {
          let parsed: ThreadEvent;
          try {
            parsed = validateThreadEvent(JSON.parse(item));
          } catch (error) {
            throw new Error(`Failed to parse item: ${item}`, { cause: error });
          }
          if (
            terminalSeen &&
            (parsed.type === "thread.started" ||
              parsed.type === "turn.started" ||
              parsed.type === "item.started" ||
              parsed.type === "item.updated" ||
              parsed.type === "item.completed" ||
              parsed.type === "item.progress" ||
              parsed.type === "turn.completed" ||
              parsed.type === "turn.failed")
          ) {
            throw new Error(
              parsed.type === "turn.completed" || parsed.type === "turn.failed"
                ? "Invalid turn stream: multiple terminal events received"
                : "Invalid turn stream: lifecycle mutation after terminal",
            );
          }
          if (parsed.type === "thread.started") {
            const identity = canonicalThreadId(parsed.thread_id) ?? parsed.thread_id;
            if (establishedId !== null && establishedId !== identity) {
              throw new Error("Invalid turn stream: thread identity changed");
            }
            establishedId = identity;
            this._id = parsed.thread_id;
          }
          terminalSeen ||= parsed.type === "turn.completed" || parsed.type === "turn.failed";
          yield parsed;
        }
      } finally {
        await cleanup();
      }
    } finally {
      this._running = false;
    }
  }

  /** Provides the input to the agent and returns the completed turn. */
  async run(input: Input, turnOptions: TurnOptions = {}): Promise<Turn> {
    const controller = new AbortController();
    const abort = () => controller.abort(turnOptions.signal?.reason);
    if (turnOptions.signal?.aborted) abort();
    else turnOptions.signal?.addEventListener("abort", abort, { once: true });
    const generator = this.runStreamedInternal(input, {
      ...turnOptions,
      signal: controller.signal,
    });
    const items: ThreadItem[] = [];
    const partialItems = new Map<string, ThreadItem>();
    let partialResponse = "";
    let finalResponse: string = "";
    let agentMessageSeen = false;
    let explicitFinalSeen = false;
    let usage: Usage | null = null;
    let surfacedResult: Turn["surfacedResult"];
    let terminal: TurnFailedEvent | TurnCompletedEvent | undefined;
    let streamError: ThreadError | undefined;
    let cause: unknown;
    let failureTimer: ReturnType<typeof setTimeout> | undefined;
    let trailingEvents = 0;
    const boundFailureDrain = () => {
      // The CLI emits error before its terminal accounting, then exits nonzero.
      // Give that evidence a bounded drain, not an unbounded new model phase.
      failureTimer ??= setTimeout(() => controller.abort(), 1_000);
    };
    try {
      for await (const event of generator) {
        if (failureTimer && ++trailingEvents > 128) {
          controller.abort();
          break;
        }
        if (terminal && (event.type === "turn.completed" || event.type === "turn.failed")) {
          // Preserve the first terminal's accounting; a later success cannot
          // erase an already observed failure while we drain trailing output.
          throw new Error("Invalid turn stream: multiple terminal events received");
        }
        if (
          event.type === "item.started" ||
          event.type === "item.updated" ||
          event.type === "item.completed"
        ) {
          partialItems.set(event.item.id, event.item);
          if (event.item.type === "agent_message") partialResponse = event.item.text;
        }
        if (event.type === "item.completed") {
          if (
            event.item.type === "agent_message" &&
            event.item.phase !== "commentary" &&
            (!explicitFinalSeen || event.item.phase === "final_answer")
          ) {
            agentMessageSeen = true;
            explicitFinalSeen ||= event.item.phase === "final_answer";
            finalResponse = event.item.text;
          } else if (event.item.type === "plan" && !agentMessageSeen) {
            finalResponse = event.item.text;
            partialResponse = event.item.text;
          }
          items.push(event.item);
        } else if (event.type === "turn.completed") {
          terminal = event;
          usage = event.usage;
          if (event.surfaced_result) {
            surfacedResult = event.surfaced_result;
            finalResponse = surfacedResult.canonicalMessage ?? "";
          } else if (event.timing?.finalAnswer !== undefined) {
            finalResponse = event.timing.finalAnswer.message?.text ?? "";
          }
        } else if (event.type === "turn.failed") {
          terminal = event;
          boundFailureDrain();
        } else if (event.type === "error") {
          const { type: _type, ...error } = event;
          streamError ??= error;
          boundFailureDrain();
        }
      }
    } catch (error) {
      cause = error;
    } finally {
      clearTimeout(failureTimer);
      turnOptions.signal?.removeEventListener("abort", abort);
    }
    if (streamError || terminal?.type !== "turn.completed" || cause !== undefined) {
      const message =
        terminal?.type === "turn.failed"
          ? terminal.error.message
          : (streamError?.message ??
            (cause &&
            typeof cause === "object" &&
            "message" in cause &&
            typeof cause.message === "string"
              ? cause.message
              : cause !== undefined
                ? String(cause)
                : "Incomplete turn stream: turn.completed event not received"));
      throw new TurnRunError(
        message,
        this._id,
        [...partialItems.values()],
        partialResponse || finalResponse,
        terminal,
        streamError,
        { cause },
      );
    }
    return {
      items,
      finalResponse,
      usage,
      ...("timing" in terminal ? { timing: terminal.timing } : {}),
      ...(terminal.timing?.finalAnswer !== undefined
        ? { finalAnswer: terminal.timing.finalAnswer }
        : {}),
      ...(surfacedResult ? { surfacedResult } : {}),
    };
  }
}

function normalizeInput(input: Input): { prompt: string; images: string[] } {
  if (typeof input === "string") {
    return { prompt: input, images: [] };
  }
  const promptParts: string[] = [];
  const images: string[] = [];
  for (const item of input) {
    if (item.type === "text") {
      promptParts.push(item.text);
    } else if (item.type === "local_image") {
      images.push(item.path);
    }
  }
  return { prompt: promptParts.join("\n\n"), images };
}
