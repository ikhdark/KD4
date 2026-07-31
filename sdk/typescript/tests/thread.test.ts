import { describe, expect, it } from "@jest/globals";

import type { CodexExec } from "../src/exec";
import { Thread } from "../src/thread";

describe("Thread", () => {
  it("rejects run when the exec stream emits a fatal error event", async () => {
    const exec = {
      async *run(): AsyncGenerator<string> {
        yield JSON.stringify({ type: "error", message: "fatal protocol failure" });
      },
    } as unknown as CodexExec;
    const thread = new Thread(exec, {}, {});

    await expect(thread.run("hello")).rejects.toThrow("fatal protocol failure");
  });

  it("rejects an error event even when its message is empty", async () => {
    const exec = {
      async *run(): AsyncGenerator<string> {
        yield JSON.stringify({ type: "error", message: "" });
      },
    } as unknown as CodexExec;
    const thread = new Thread(exec, {}, {});

    await expect(thread.run("hello")).rejects.toThrow();
  });
});
