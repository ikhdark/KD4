import { describe, expect, it } from "@jest/globals";

import { CodexExec, CodexExecArgs } from "../src/exec";
import { Thread } from "../src/thread";

const threadId = "019f0000-0000-7000-8000-000000000001";
const completed = JSON.stringify({
  type: "turn.completed",
  usage: { input_tokens: 0, cached_input_tokens: 0, output_tokens: 0 },
});

function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

function threadWith(run: (args: CodexExecArgs) => AsyncGenerator<string>) {
  return new Thread({ run } as CodexExec, {}, {});
}

describe("Thread operation ownership", () => {
  it("rejects overlap before a fresh thread identity is resolved and resumes sequentially", async () => {
    const started = deferred();
    const release = deferred();
    const requestedIds: (string | null | undefined)[] = [];
    const thread = threadWith(async function* (args) {
      requestedIds.push(args.threadId);
      started.resolve();
      await release.promise;
      yield JSON.stringify({ type: "thread.started", thread_id: threadId });
      yield completed;
    });
    const first = thread.run("first");
    try {
      await started.promise;
      await expect(thread.run("overlap")).rejects.toThrow("already running");
      const overlap = await thread.runStreamed("stream overlap");
      await expect(overlap.events.next()).rejects.toThrow("already running");
      expect(requestedIds).toEqual([null]);
    } finally {
      release.resolve();
      await first;
    }
    await thread.run("next");
    expect(requestedIds).toEqual([null, threadId]);
  });

  it("does not reserve lazy streams and releases ownership after early return", async () => {
    const requestedIds: (string | null | undefined)[] = [];
    let cleaned = 0;
    const thread = threadWith(async function* (args) {
      requestedIds.push(args.threadId);
      try {
        yield JSON.stringify({ type: "thread.started", thread_id: threadId });
        yield completed;
      } finally {
        cleaned++;
      }
    });
    const lazy = await thread.runStreamed("lazy");
    await thread.run("first consumed");
    expect(requestedIds).toEqual([null]);
    await lazy.events.next();
    await expect(thread.run("overlap")).rejects.toThrow("already running");
    await lazy.events.return(undefined);
    await thread.run("after close");
    expect(requestedIds).toEqual([null, threadId, threadId]);
    expect(cleaned).toBe(3);
  });

  it("releases ownership after malformed output and failed schema preparation", async () => {
    let calls = 0;
    const thread = threadWith(async function* () {
      if (++calls === 1) yield "not JSON";
      else yield completed;
    });
    await expect(thread.run("malformed")).rejects.toThrow("Failed to parse");
    const cyclic: Record<string, unknown> = {};
    cyclic.self = cyclic;
    await expect(thread.run("invalid schema", { outputSchema: cyclic })).rejects.toThrow();
    await thread.run("recovered");
    expect(calls).toBe(2);
  });

  it("allows independent Thread objects to run concurrently", async () => {
    const release = deferred();
    const bothStarted = deferred();
    let calls = 0;
    const run = async function* () {
      if (++calls === 2) bothStarted.resolve();
      await release.promise;
      yield completed;
    };
    const first = threadWith(run).run("one");
    const second = threadWith(run).run("two");
    try {
      await bothStarted.promise;
      expect(calls).toBe(2);
    } finally {
      release.resolve();
      await Promise.all([first, second]);
    }
  });
});
