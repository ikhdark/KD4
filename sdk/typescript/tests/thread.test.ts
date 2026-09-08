import { describe, expect, it } from "@jest/globals";

import type { CodexExec } from "../src/exec";
import { Thread } from "../src/thread";

describe("Thread", () => {
  it.each(["fatal protocol failure", ""])(
    "rejects a fatal error with message %j",
    async (message) => {
      const exec = {
        async *run(): AsyncGenerator<string> {
          yield JSON.stringify({ type: "error", message });
        },
      } as unknown as CodexExec;
      const thread = new Thread(exec, {}, {});

      await expect(thread.run("hello")).rejects.toThrow(new Error(message));
    },
  );
});
