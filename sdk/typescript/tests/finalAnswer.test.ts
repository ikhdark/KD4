import { describe, expect, it } from "@jest/globals";
import { CodexExec } from "../src/exec";
import { Thread, TurnRunError } from "../src/thread";

const usage = {
  input_tokens: 10,
  cached_input_tokens: 0,
  output_tokens: 20,
  reasoning_output_tokens: 7,
};
const message = (text: string, phase?: "commentary" | "final_answer") => ({
  type: "item.completed",
  item: { id: text, type: "agent_message", text, ...(phase ? { phase } : {}) },
});
const complete = { type: "turn.completed", usage };
function thread(events: unknown[]) {
  return new Thread(
    {
      run: async function* () {
        for (const event of events) yield JSON.stringify(event);
      },
    } as unknown as CodexExec,
    {},
    {},
  );
}

describe("accepted final answer identity", () => {
  it.each([
    [[message("answer", "final_answer"), message("progress", "commentary")], "answer"],
    [[message("progress", "commentary"), message("answer", "final_answer")], "answer"],
    [[message("progress", "commentary")], ""],
    [[message("legacy")], "legacy"],
    [[message("answer", "final_answer"), message("legacy")], "answer"],
  ])("does not promote commentary over final output", async (items, expected) => {
    const turn = await thread([...(items as unknown[]), complete]).run("finish");
    expect(turn.finalResponse).toBe(expected);
  });

  it("uses a recovered receipt over a rejected earlier final and preserves exact schema bytes", async () => {
    const accepted = {
      itemId: "accepted",
      text: '{"answer":"α"}\n',
      origin: "legacy_unphased",
      completionAttemptId: "turn:2",
    };
    const turn = await thread([
      message("rejected", "final_answer"),
      { ...complete, timing: { finalAnswer: { message: accepted } } },
    ]).run("finish");
    expect(turn.finalResponse).toBe(accepted.text);
    expect(turn.finalAnswer?.message).toEqual(accepted);
  });

  it("does not fall back to activity when core explicitly accepted no answer", async () => {
    const turn = await thread([
      message("rejected", "final_answer"),
      { ...complete, timing: { finalAnswer: { message: null } } },
    ]).run("finish");
    expect(turn.finalResponse).toBe("");
  });

  it("rejects a post-terminal message rather than contradicting a canonical result", async () => {
    const terminal = {
      ...complete,
      surfaced_result: { adapter: "owner", value: {}, canonicalMessage: "accepted" },
    };
    await expect(thread([terminal, message("late")]).run("finish")).rejects.toBeInstanceOf(
      TurnRunError,
    );
    await expect(thread([terminal, message("late")]).run("finish")).rejects.toThrow(
      "lifecycle mutation after terminal",
    );
  });

  it.each([
    [{ type: "error", message: "late process failure" }],
    [complete],
    [{ type: "turn.failed", error: { message: "late failure" } }],
  ])("still rejects trailing errors and duplicate terminals", async (late) => {
    await expect(thread([complete, late]).run("finish")).rejects.toBeInstanceOf(TurnRunError);
  });
});
