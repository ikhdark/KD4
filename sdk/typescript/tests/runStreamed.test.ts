import { describe, expect, it } from "@jest/globals";

import { ThreadEvent } from "../src/index";

import {
  assistantMessage,
  responseCompleted,
  responseStarted,
  sse,
  startResponsesTestProxy,
} from "./responsesProxy";
import { createMockClient } from "./testCodex";

describe("Codex", () => {
  it("returns thread events", async () => {
    const { url, close } = await startResponsesTestProxy({
      statusCode: 200,
      responseBodies: [sse(responseStarted(), assistantMessage("Hi!"), responseCompleted())],
    });
    const { client, cleanup } = createMockClient(url);

    try {
      const thread = client.startThread();
      const result = await thread.runStreamed("Hello, world!");

      const events: ThreadEvent[] = [];
      for await (const event of result.events) {
        events.push(event);
      }

      expect(events).toEqual([
        {
          type: "thread.started",
          thread_id: expect.any(String),
        },
        {
          type: "turn.started",
        },
        {
          type: "item.completed",
          item: {
            id: "item_0",
            type: "agent_message",
            text: "Hi!",
          },
        },
        expect.objectContaining({
          type: "turn.completed",
          usage: expect.objectContaining({
            cached_input_tokens: 12,
            input_tokens: 42,
            output_tokens: 5,
            reasoning_output_tokens: 0,
          }),
        }),
      ]);
      expect(thread.id).toBe((events[0] as { thread_id: string }).thread_id);
      expect(thread.id).toEqual(expect.any(String));
    } finally {
      cleanup();
      await close();
    }
  });

  it("sends previous items when runStreamed is called twice", async () => {
    const { url, close, requests } = await startResponsesTestProxy({
      statusCode: 200,
      responseBodies: [
        sse(
          responseStarted("response_1"),
          assistantMessage("First response", "item_1"),
          responseCompleted("response_1"),
        ),
        sse(
          responseStarted("response_2"),
          assistantMessage("Second response", "item_2"),
          responseCompleted("response_2"),
        ),
      ],
    });
    const { client, cleanup } = createMockClient(url);

    try {
      const thread = client.startThread();
      const first = await thread.runStreamed("first input");
      await drainEvents(first.events);

      const second = await thread.runStreamed("second input");
      const secondEvents = await drainEvents(second.events);
      expect(secondEvents).toContainEqual({
        type: "item.completed",
        item: expect.objectContaining({ type: "agent_message", text: "Second response" }),
      });
      expect(secondEvents.at(-1)).toEqual(expect.objectContaining({ type: "turn.completed" }));

      // Check second request continues the same thread
      expect(requests).toHaveLength(2);
      const secondRequest = requests[1];
      expect(secondRequest).toBeDefined();
      const payload = secondRequest!.json;
      expect(payload.input.at(-1)).toEqual(expect.objectContaining({
        role: "user",
        content: [{ type: "input_text", text: "second input" }],
      }));

      const assistantEntry = payload.input.find(
        (entry: { role: string }) => entry.role === "assistant",
      );
      expect(assistantEntry).toBeDefined();
      const assistantText = assistantEntry?.content?.find(
        (item: { type: string; text: string }) => item.type === "output_text",
      )?.text;
      expect(assistantText).toBe("First response");
    } finally {
      cleanup();
      await close();
    }
  });

  it("resumes thread by id when streaming", async () => {
    const { url, close, requests } = await startResponsesTestProxy({
      statusCode: 200,
      responseBodies: [
        sse(
          responseStarted("response_1"),
          assistantMessage("First response", "item_1"),
          responseCompleted("response_1"),
        ),
        sse(
          responseStarted("response_2"),
          assistantMessage("Second response", "item_2"),
          responseCompleted("response_2"),
        ),
      ],
    });
    const { client, cleanup } = createMockClient(url);

    try {
      const originalThread = client.startThread();
      const first = await originalThread.runStreamed("first input");
      await drainEvents(first.events);

      const resumedThread = client.resumeThread(originalThread.id!);
      const second = await resumedThread.runStreamed("second input");
      const secondEvents = await drainEvents(second.events);
      expect(secondEvents).toContainEqual({
        type: "item.completed",
        item: expect.objectContaining({ type: "agent_message", text: "Second response" }),
      });
      expect(secondEvents.at(-1)).toEqual(expect.objectContaining({ type: "turn.completed" }));

      expect(resumedThread.id).toBe(originalThread.id);

      expect(requests).toHaveLength(2);
      const secondRequest = requests[1];
      expect(secondRequest).toBeDefined();
      const payload = secondRequest!.json;
      expect(payload.input.at(-1)).toEqual(expect.objectContaining({
        role: "user",
        content: [{ type: "input_text", text: "second input" }],
      }));

      const assistantEntry = payload.input.find(
        (entry: { role: string }) => entry.role === "assistant",
      );
      expect(assistantEntry).toBeDefined();
      const assistantText = assistantEntry?.content?.find(
        (item: { type: string; text: string }) => item.type === "output_text",
      )?.text;
      expect(assistantText).toBe("First response");
    } finally {
      cleanup();
      await close();
    }
  });

  it("applies output schema turn options when streaming", async () => {
    const { url, close, requests } = await startResponsesTestProxy({
      statusCode: 200,
      responseBodies: [
        sse(
          responseStarted("response_1"),
          assistantMessage("Structured response", "item_1"),
          responseCompleted("response_1"),
        ),
      ],
    });
    const { client, cleanup } = createMockClient(url);

    const schema = {
      type: "object",
      properties: {
        answer: { type: "string" },
      },
      required: ["answer"],
      additionalProperties: false,
    } as const;

    try {
      const thread = client.startThread();
      const streamed = await thread.runStreamed("structured", { outputSchema: schema });
      await drainEvents(streamed.events);

      expect(requests.length).toBeGreaterThanOrEqual(1);
      const payload = requests[0];
      expect(payload).toBeDefined();
      const text = payload!.json.text;
      expect(text).toBeDefined();
      expect(text?.format).toEqual({
        name: "codex_output_schema",
        type: "json_schema",
        strict: true,
        schema,
      });
    } finally {
      cleanup();
      await close();
    }
  });
});

async function drainEvents(events: AsyncGenerator<ThreadEvent>): Promise<ThreadEvent[]> {
  const collected: ThreadEvent[] = [];
  for await (const event of events) {
    collected.push(event);
  }
  return collected;
}
