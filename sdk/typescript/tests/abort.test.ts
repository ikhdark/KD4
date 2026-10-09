import { describe, expect, it } from "@jest/globals";

import {
  assistantMessage,
  responseCompleted,
  responseStarted,
  shell_call as shellCall,
  sse,
  SseResponseBody,
  startResponsesTestProxy,
} from "./responsesProxy";
import { createMockClient } from "./testCodex";

function* infiniteShellCall(): Generator<SseResponseBody> {
  while (true) {
    yield sse(responseStarted(), shellCall(), responseCompleted());
  }
}

describe("AbortSignal support", () => {
  it("aborts run() when signal is aborted", async () => {
    const { url, close } = await startResponsesTestProxy({
      statusCode: 200,
      responseBodies: infiniteShellCall(),
    });
    const { client, cleanup } = createMockClient(url);

    try {
      const thread = client.startThread();

      // Create an abort controller and abort it immediately
      const controller = new AbortController();
      controller.abort("Test abort");

      // The operation should fail because the signal is already aborted
      await expect(
        thread.run("Hello, world!", { signal: controller.signal }),
      ).rejects.toMatchObject({
        name: "AbortError",
        code: "ABORT_ERR",
        cause: "Test abort",
      });
    } finally {
      cleanup();
      await close();
    }
  });

  it("aborts runStreamed() when signal is aborted", async () => {
    const { url, close } = await startResponsesTestProxy({
      statusCode: 200,
      responseBodies: infiniteShellCall(),
    });
    const { client, cleanup } = createMockClient(url);

    try {
      const thread = client.startThread();

      // Create an abort controller and abort it immediately
      const controller = new AbortController();
      controller.abort("Test abort");

      const { events } = await thread.runStreamed("Hello, world!", { signal: controller.signal });

      try {
        await expect(events.next()).rejects.toMatchObject({
          name: "AbortError",
          code: "ABORT_ERR",
          cause: "Test abort",
        });
      } finally {
        await events.return(undefined);
      }
    } finally {
      cleanup();
      await close();
    }
  });

  it("aborts run() when signal is aborted during execution", async () => {
    const controller = new AbortController();
    let reachedModel = false;
    function* abortAfterRequest(): Generator<SseResponseBody> {
      // Synchronize with the real HTTP request rather than racing process startup.
      reachedModel = true;
      controller.abort("Aborted during execution");
      yield sse(responseStarted(), assistantMessage("unused"), responseCompleted());
    }
    const { url, close } = await startResponsesTestProxy({
      statusCode: 200,
      responseBodies: abortAfterRequest(),
    });
    const { client, cleanup } = createMockClient(url);

    try {
      const thread = client.startThread();

      const runPromise = thread.run("Hello, world!", { signal: controller.signal });
      await expect(runPromise).rejects.toMatchObject({
        name: "AbortError",
        code: "ABORT_ERR",
        cause: "Aborted during execution",
      });
      expect(reachedModel).toBe(true);
    } finally {
      cleanup();
      await close();
    }
  });

  it("aborts runStreamed() when signal is aborted during iteration", async () => {
    const { url, close } = await startResponsesTestProxy({
      statusCode: 200,
      responseBodies: infiniteShellCall(),
    });
    const { client, cleanup } = createMockClient(url);

    try {
      const thread = client.startThread();

      const controller = new AbortController();

      const { events } = await thread.runStreamed("Hello, world!", { signal: controller.signal });

      // Abort during iteration
      let eventCount = 0;
      await expect(
        (async () => {
          for await (const event of events) {
            void event; // Consume the event
            eventCount++;
            // Abort after first event
            if (eventCount === 1) {
              controller.abort("Aborted during iteration");
            }
            // Continue iterating - should eventually throw
          }
        })(),
      ).rejects.toMatchObject({
        name: "AbortError",
        code: "ABORT_ERR",
        cause: "Aborted during iteration",
      });
      expect(eventCount).toBeGreaterThan(0);
    } finally {
      cleanup();
      await close();
    }
  });

  it("completes normally when signal is not aborted", async () => {
    const { url, close } = await startResponsesTestProxy({
      statusCode: 200,
      responseBodies: [sse(responseStarted(), assistantMessage("Hi!"), responseCompleted())],
    });
    const { client, cleanup } = createMockClient(url);

    try {
      const thread = client.startThread();

      const controller = new AbortController();

      // Don't abort - should complete successfully
      const result = await thread.run("Hello, world!", { signal: controller.signal });

      expect(result.finalResponse).toBe("Hi!");
      expect(result.items).toHaveLength(1);
    } finally {
      cleanup();
      await close();
    }
  });
});
