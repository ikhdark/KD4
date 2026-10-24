import * as child_process from "node:child_process";
import { EventEmitter } from "node:events";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import path from "node:path";
import { PassThrough } from "node:stream";

import { describe, expect, it } from "@jest/globals";

jest.mock("node:child_process", () => {
  const actual = jest.requireActual<typeof import("node:child_process")>("node:child_process");
  return { ...actual, spawn: jest.fn() };
});

jest.mock("node:module", () => ({ createRequire: jest.fn() }));

// Keep the real manifest reader and filesystem checks; replace only npm lookup.
const packageRequire = Object.assign(
  (manifestPath: string) => JSON.parse(readFileSync(manifestPath, "utf8")),
  { resolve: jest.fn<string, [string]>() },
);
jest.mocked(createRequire).mockReturnValue(packageRequire as unknown as NodeJS.Require);

const _actualChildProcess =
  jest.requireActual<typeof import("node:child_process")>("node:child_process");
const spawnMock = child_process.spawn as jest.MockedFunction<typeof _actualChildProcess.spawn>;
const STDERR_TAIL_MAX_BYTES = 64 * 1024;
const STDERR_TRUNCATION_MARKER = `[stderr truncated; showing last ${STDERR_TAIL_MAX_BYTES} bytes]\n`;

class FakeChildProcess extends EventEmitter {
  pid: number | undefined = 123;
  stdin = new PassThrough();
  stdout = new PassThrough();
  stderr = new PassThrough();
  killed = false;
  killSignals: Array<NodeJS.Signals | number | undefined> = [];
  onKill?: (signal: NodeJS.Signals | number | undefined) => void;

  kill(signal?: NodeJS.Signals | number): boolean {
    this.killed = true;
    this.killSignals.push(signal);
    this.onKill?.(signal);
    return true;
  }
}

function createEarlyExitChild(exitCode = 2): FakeChildProcess {
  const child = new FakeChildProcess();
  setImmediate(() => {
    child.stderr.write("boom");
    child.emit("exit", exitCode, null);
    setImmediate(() => {
      child.stdout.end();
      child.stderr.end();
    });
  });
  return child;
}

function createCompletedChild(
  stderr: Buffer | Buffer[],
  exitCode: number,
  delayStderrUntilAfterExit = false,
): FakeChildProcess {
  const child = new FakeChildProcess();
  const endStderr = () => {
    for (const chunk of Array.isArray(stderr) ? stderr : [stderr]) child.stderr.write(chunk);
    child.stderr.end();
  };
  setImmediate(() => {
    if (delayStderrUntilAfterExit) {
      child.stdout.end();
      child.emit("exit", exitCode, null);
      setImmediate(endStderr);
    } else {
      endStderr();
      child.stdout.end();
      child.emit("exit", exitCode, null);
    }
  });
  return child;
}

function newlineFreeStderrOverCap(): Buffer {
  return Buffer.concat([
    Buffer.from("discard-me"),
    Buffer.from(String.fromCodePoint(0x1f642).repeat(STDERR_TAIL_MAX_BYTES / 4 + 2)),
    Buffer.from("END"),
  ]);
}

const delay = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

describe("CodexExec", () => {
  it("bounds stdout drain after owner exit and rejects an incomplete capture", async () => {
    const { CodexExec } = await import("../src/exec");
    jest.useFakeTimers();
    const child = new FakeChildProcess();
    spawnMock.mockReturnValue(child as unknown as child_process.ChildProcess);
    try {
      const lines: string[] = [];
      let settled = false;
      const run = (async () => {
        for await (const line of new CodexExec("codex").run({ input: "hi" })) {
          lines.push(line);
        }
        settled = true;
      })().catch((error: unknown) => {
        settled = true;
        return error;
      });
      child.stdout.write("before exit\n");
      await jest.advanceTimersByTimeAsync(5_000);
      expect(settled).toBe(false);
      child.emit("exit", 0, null);
      child.stdout.write("trailing complete record\n");
      child.stderr.end();
      await jest.advanceTimersByTimeAsync(999);
      expect(settled).toBe(false);
      await jest.advanceTimersByTimeAsync(1);
      expect(await run).toMatchObject({
        message: expect.stringContaining("output capture did not reach EOF"),
      });
      expect(lines).toEqual(["before exit", "trailing complete record"]);
      expect(child.stdout.destroyed).toBe(true);
      expect(child.killSignals).toEqual([]);
    } finally {
      jest.useRealTimers();
    }
  });

  it.each([
    { completed: false, exitCode: 0 },
    { completed: true, exitCode: 0 },
    { completed: true, exitCode: 2 },
  ])(
    "keeps turn completion evidence authoritative after stdout drain: %j",
    async ({ completed, exitCode }) => {
      const { CodexExec } = await import("../src/exec");
      const { Thread, TurnRunError } = await import("../src/thread");
      jest.useFakeTimers();
      const child = new FakeChildProcess();
      const spawned = new Promise<void>((resolve) => {
        spawnMock.mockImplementationOnce(() => {
          resolve();
          return child as unknown as child_process.ChildProcess;
        });
      });
      const item = { id: "partial", type: "agent_message", text: "observed answer" };
      const terminal = {
        type: "turn.completed",
        usage: { input_tokens: 3, cached_input_tokens: 0, output_tokens: 2 },
      };
      let settled = false;
      try {
        const result = new Thread(new CodexExec("codex"), {}, {}).run("hello").then(
          (turn) => {
            settled = true;
            return { turn, error: undefined };
          },
          (error: unknown) => {
            settled = true;
            return { turn: undefined, error };
          },
        );
        await spawned;
        child.stdout.write(
          JSON.stringify({ type: "thread.started", thread_id: "drain-thread" }) + "\n",
        );
        child.stdout.write(JSON.stringify({ type: "item.completed", item }) + "\n");
        child.emit("exit", exitCode, null);
        // The terminal arrives after owner exit, while a descendant still holds
        // stdout open. EOF/drain expiry itself must never manufacture success.
        if (completed) child.stdout.write(JSON.stringify(terminal) + "\n");
        child.stderr.end();
        await jest.advanceTimersByTimeAsync(999);
        expect(settled).toBe(false);
        await jest.advanceTimersByTimeAsync(1);
        const outcome = await result;
        {
          expect(outcome.turn).toBeUndefined();
          expect(outcome.error).toBeInstanceOf(TurnRunError);
          expect(outcome.error).toMatchObject({
            threadId: "drain-thread",
            items: [item],
            finalResponse: item.text,
            terminal: completed ? terminal : undefined,
            message: expect.stringContaining(
              exitCode === 2 ? "exited with code 2" : "output capture did not reach EOF",
            ),
          });
        }
        expect(child.stdout.destroyed).toBe(true);
        expect(child.killSignals).toEqual([]);
      } finally {
        child.stdout.destroy();
        child.stderr.destroy();
        jest.useRealTimers();
      }
    },
  );

  it("interrupts an active stdout read on abort even when the root ignores termination", async () => {
    const { CodexExec } = await import("../src/exec");
    jest.useFakeTimers();
    const child = new FakeChildProcess();
    const controller = new AbortController();
    spawnMock.mockReturnValue(child as unknown as child_process.ChildProcess);
    try {
      const run = (async () => {
        for await (const _line of new CodexExec("codex").run({
          input: "hi",
          signal: controller.signal,
        })) {
          /* drain */
        }
      })().catch((error: unknown) => error);
      child.stdout.write("partial line without newline");
      controller.abort();
      await jest.advanceTimersByTimeAsync(2_000);
      expect(await run).toMatchObject({ name: "AbortError", code: "ABORT_ERR" });
      expect(child.stdout.destroyed).toBe(true);
      expect(child.stderr.destroyed).toBe(true);
      expect(child.killSignals).toEqual(["SIGTERM", "SIGKILL"]);
    } finally {
      jest.useRealTimers();
    }
  });

  it("closes failed-spawn readers without waiting for an exit event that cannot arrive", async () => {
    const { CodexExec } = await import("../src/exec");
    const child = new FakeChildProcess();
    child.pid = undefined;
    spawnMock.mockReturnValue(child as unknown as child_process.ChildProcess);
    const failed = Object.assign(new Error("executable not found"), { code: "ENOENT" });
    const run = (async () => {
      for await (const _line of new CodexExec("missing").run({ input: "hi" })) {
        /* drain */
      }
    })().catch((error: unknown) => error);
    child.emit("error", failed);
    expect(await run).toBe(failed);
    expect(child.stdout.destroyed).toBe(true);
    expect(child.stderr.destroyed).toBe(true);
    expect(child.killSignals).toEqual([]);
  });

  it("consumes a rejected deadline input and releases its timer", async () => {
    const { resolvesBeforeDeadline } = await import("../src/exec");
    jest.useFakeTimers();
    try {
      await expect(
        resolvesBeforeDeadline(Promise.reject(new Error("input failed")), 1_000),
      ).resolves.toBe(false);
      expect(jest.getTimerCount()).toBe(0);
      await expect(resolvesBeforeDeadline(Promise.resolve(), 1_000)).resolves.toBe(true);
      expect(jest.getTimerCount()).toBe(0);
    } finally {
      jest.useRealTimers();
    }
  });

  it("continues consuming rejection after the deadline has expired", async () => {
    const { resolvesBeforeDeadline } = await import("../src/exec");
    jest.useFakeTimers();
    try {
      let reject!: (error: Error) => void;
      const pending = new Promise<void>((_resolve, fail) => {
        reject = fail;
      });
      const result = resolvesBeforeDeadline(pending, 1_000);
      jest.advanceTimersByTime(1_000);
      await expect(result).resolves.toBe(false);
      reject(new Error("late failure"));
      // Drain the production rejection handler; Jest also rejects unhandled promises.
      await Promise.resolve();
      expect(jest.getTimerCount()).toBe(0);
    } finally {
      jest.useRealTimers();
    }
  });

  it.each([
    {
      value: "high",
      literal: '"high"',
    },
    {
      value: "focused",
      literal: '"focused"',
    },
    {
      value: 'x"\\\nflag=true\t',
      literal: String.raw`"x\"\\\nflag=true\t"`,
    },
    {
      // TOML forbids a literal DEL even though JSON permits it.
      value: "x\u007fy",
      literal: String.raw`"x\u007fy"`,
    },
  ])("escapes public thread config arguments for $literal", async ({ value, literal }) => {
    const { Codex } = await import("../src/codex");
    spawnMock.mockClear();
    const child = createCompletedChild(Buffer.alloc(0), 0);
    child.stdout.write(
      '{"type":"turn.completed","usage":{"input_tokens":0,"cached_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0}}\n',
    );
    spawnMock.mockReturnValue(child as unknown as child_process.ChildProcess);
    const thread = new Codex({ codexPathOverride: "codex" }).startThread({
      // JavaScript callers can supply strings outside the TypeScript unions.
      modelReasoningEffort: value,
      webSearchMode: value as "live",
      approvalPolicy: value as "never",
    });

    await thread.run("hello");

    expect(spawnMock).toHaveBeenCalledTimes(1);
    expect(spawnMock.mock.calls[0]?.[1]).toEqual([
      "exec",
      "--experimental-json",
      "--config",
      `model_reasoning_effort=${literal}`,
      "--config",
      `web_search=${literal}`,
      "--config",
      `approval_policy=${literal}`,
    ]);
  });

  it("escapes TOML strings and quoted keys inside config arrays", async () => {
    const { Codex } = await import("../src/codex");
    spawnMock.mockClear();
    const child = createCompletedChild(Buffer.alloc(0), 0);
    child.stdout.write(
      '{"type":"turn.completed","usage":{"input_tokens":0,"cached_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0}}\n',
    );
    spawnMock.mockReturnValue(child as unknown as child_process.ChildProcess);
    await new Codex({
      codexPathOverride: "codex",
      config: { rules: [{ "key\u007f": "value\u007f" }] },
    })
      .startThread()
      .run("hello");

    expect(spawnMock.mock.calls[0]?.[1]).toEqual([
      "exec",
      "--experimental-json",
      "--config",
      String.raw`rules=[{"key\u007f" = "value\u007f"}]`,
    ]);
  });

  it("preserves literal config key segments in CLI overrides", async () => {
    const { Codex } = await import("../src/codex");
    spawnMock.mockClear();
    const child = createCompletedChild(Buffer.alloc(0), 0);
    child.stdout.write(
      '{"type":"turn.completed","usage":{"input_tokens":0,"cached_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0}}\n',
    );
    spawnMock.mockReturnValue(child as unknown as child_process.ChildProcess);
    await new Codex({
      codexPathOverride: "codex",
      config: {
        mcp_servers: { "docs.v1": { enabled: false } },
        projects: { "C:\\Users\\Mark\\repo.v1": { trust_level: "trusted" } },
        "a.b": true,
        a: { b: false },
        'space \"猫\\\u007f': {},
      },
    })
      .startThread()
      .run("hello");
    expect(spawnMock.mock.calls[0]?.[1]).toEqual([
      "exec",
      "--experimental-json",
      "--config",
      'mcp_servers."docs.v1".enabled=false',
      "--config",
      String.raw`projects."C:\\Users\\Mark\\repo.v1".trust_level="trusted"`,
      "--config",
      '"a.b"=true',
      "--config",
      "a.b=false",
      "--config",
      String.raw`"space \"猫\\\u007f"={}`,
    ]);
  });

  it("rejects the public turn when the child closes stdin before accepting the prompt", async () => {
    const { Codex } = await import("../src/codex");
    spawnMock.mockClear();
    const child = new FakeChildProcess();
    const inputError = Object.assign(new Error("write EPIPE"), { code: "EPIPE" });
    spawnMock.mockReturnValue(child as unknown as child_process.ChildProcess);
    setImmediate(() => {
      child.stdin.destroy(inputError);
      child.stdout.end();
      child.stderr.end();
      child.emit("exit", 0, null);
    });

    const codex = new Codex({ codexPathOverride: "codex" });
    await expect(codex.startThread().run("unread prompt")).rejects.toMatchObject({
      name: "TurnRunError",
      message: "write EPIPE",
      cause: inputError,
    });
    expect(spawnMock).toHaveBeenCalledTimes(1);
  });

  it("preserves the child failure when it exits during a large prompt write", async () => {
    const { Codex } = await import("../src/codex");
    spawnMock.mockImplementationOnce((_file, _args, options) =>
      _actualChildProcess.spawn(
        process.execPath,
        ["-e", "process.stderr.write('prompt rejected'); process.exit(2)"],
        options,
      ),
    );

    const thread = new Codex({ codexPathOverride: process.execPath }).startThread();
    await expect(thread.run("x".repeat(8 * 1024 * 1024))).rejects.toThrow(
      "Codex Exec exited with code 2: prompt rejected",
    );
  });

  it("retains terminal accounting when the real producer exits nonzero", async () => {
    const { Codex } = await import("../src/codex");
    const terminal = {
      type: "turn.failed",
      error: { message: "connection lost" },
      disposition: "transport_lost",
      usage: null,
      timing: { duration_ms: 0 },
    };
    const events = [
      { type: "thread.started", thread_id: "failed-thread" },
      { type: "error", message: "transport error" },
      terminal,
    ];
    spawnMock.mockImplementationOnce((_file, _args, options) =>
      _actualChildProcess.spawn(
        process.execPath,
        [
          "-e",
          `process.stdin.resume(); process.stdin.on('end', () => { process.stdout.write(${JSON.stringify(events.map((event) => JSON.stringify(event)).join("\n") + "\n")}, () => process.exit(1)); });`,
        ],
        options,
      ),
    );
    await expect(
      new Codex({ codexPathOverride: process.execPath }).startThread().run("hello"),
    ).rejects.toMatchObject({
      name: "TurnRunError",
      threadId: "failed-thread",
      terminal,
      cause: expect.objectContaining({ message: expect.stringContaining("exited with code 1") }),
    });
  });

  it.each([
    "",
    JSON.stringify({
      type: "item.updated",
      item: { id: "partial", type: "agent_message", text: "unfinished" },
    }) + "\n",
  ])("rejects clean producer EOF without completion: %j", async (output) => {
    const { Codex } = await import("../src/codex");
    spawnMock.mockImplementationOnce((_file, _args, options) =>
      _actualChildProcess.spawn(
        process.execPath,
        [
          "-e",
          `process.stdin.resume(); process.stdin.on('end', () => process.stdout.write(${JSON.stringify(output)}));`,
        ],
        options,
      ),
    );
    await expect(
      new Codex({ codexPathOverride: process.execPath }).startThread().run("hello"),
    ).rejects.toMatchObject({
      name: "TurnRunError",
      terminal: undefined,
      finalResponse: output ? "unfinished" : "",
      message: expect.stringContaining("Incomplete turn stream"),
    });
  });

  it("rejects when exit happens before stdout closes", async () => {
    const { CodexExec } = await import("../src/exec");
    const child = createEarlyExitChild();
    spawnMock.mockReturnValue(child as unknown as child_process.ChildProcess);

    const exec = new CodexExec("codex");
    const runPromise = (async () => {
      for await (const _ of exec.run({ input: "hi" })) {
        // no-op
      }
    })().then(
      () => ({ status: "resolved" as const }),
      (error) => ({ status: "rejected" as const, error }),
    );

    const result = await Promise.race([
      runPromise,
      delay(500).then(() => ({ status: "timeout" as const })),
    ]);

    expect(result.status).toBe("rejected");
    if (result.status === "rejected") {
      expect(result.error).toBeInstanceOf(Error);
      expect(result.error.message).toMatch(/Codex Exec exited/);
    }
  });

  it.each(["single", "chunked"])(
    "bounds and renders a newline-free stderr tail on failure (%s)",
    async (delivery) => {
      const { CodexExec } = await import("../src/exec");
      spawnMock.mockClear();
      const stderr = newlineFreeStderrOverCap();
      const chunks =
        delivery === "single"
          ? stderr
          : Array.from({ length: Math.ceil(stderr.length / 4093) }, (_, index) =>
              stderr.subarray(index * 4093, (index + 1) * 4093),
            );
      const child = createCompletedChild(chunks, 2, true);
      spawnMock.mockReturnValue(child as unknown as child_process.ChildProcess);

      const exec = new CodexExec("codex");
      let error: unknown;
      try {
        for await (const _ of exec.run({ input: "hi" })) {
          // no-op
        }
      } catch (caught) {
        error = caught;
      }

      expect(error).toBeInstanceOf(Error);
      const message = (error as Error).message;
      expect(message).toBe(
        "Codex Exec exited with code 2: " +
          STDERR_TRUNCATION_MARKER +
          String.fromCodePoint(0x1f642).repeat(16_383) +
          "END",
      );
    },
  );

  it("drains over-cap newline-free stderr when the child succeeds", async () => {
    const { CodexExec } = await import("../src/exec");
    spawnMock.mockClear();
    const child = createCompletedChild(newlineFreeStderrOverCap(), 0);
    spawnMock.mockReturnValue(child as unknown as child_process.ChildProcess);

    const exec = new CodexExec("codex");
    const result = await Promise.race([
      (async () => {
        for await (const _ of exec.run({ input: "hi" })) {
          // no-op
        }
        return "resolved" as const;
      })(),
      delay(500).then(() => "timeout" as const),
    ]);

    expect(result).toBe("resolved");
  });

  it("escalates to SIGKILL when an early-returned child ignores SIGTERM", async () => {
    const { CodexExec } = await import("../src/exec");
    spawnMock.mockClear();
    const child = new FakeChildProcess();
    child.onKill = (signal) => {
      if (signal === "SIGKILL") {
        setImmediate(() => child.emit("exit", null, "SIGKILL"));
      }
    };
    spawnMock.mockReturnValue(child as unknown as child_process.ChildProcess);

    const exec = new CodexExec("codex");
    const iterator = exec.run({ input: "hi" });
    child.stdout.write("first line\n");
    await expect(iterator.next()).resolves.toEqual({ done: false, value: "first line" });

    await iterator.return(undefined);
    expect(child.killSignals).toEqual(["SIGTERM", "SIGKILL"]);
  });

  it("bounds cleanup even when the child never emits exit", async () => {
    const { CodexExec } = await import("../src/exec");
    spawnMock.mockClear();
    const child = new FakeChildProcess();
    spawnMock.mockReturnValue(child as unknown as child_process.ChildProcess);

    const exec = new CodexExec("codex");
    const iterator = exec.run({ input: "hi" });
    child.stdout.write("first line\n");
    await expect(iterator.next()).resolves.toEqual({ done: false, value: "first line" });

    const result = await Promise.race([
      iterator.return(undefined).then(() => "returned" as const),
      delay(2_500).then(() => "timeout" as const),
    ]);
    expect(result).toBe("returned");
    expect(child.killSignals).toEqual(["SIGTERM", "SIGKILL"]);
  });

  it("places resume args before image args", async () => {
    const { CodexExec } = await import("../src/exec");
    spawnMock.mockClear();
    const child = new FakeChildProcess();
    spawnMock.mockReturnValue(child as unknown as child_process.ChildProcess);

    setImmediate(() => {
      child.stdout.end();
      child.stderr.end();
      child.emit("exit", 0, null);
    });

    const exec = new CodexExec("codex");
    for await (const _ of exec.run({ input: "hi", images: ["img.png"], threadId: "thread-id" })) {
      // no-op
    }

    const commandArgs = spawnMock.mock.calls[0]?.[1] as string[] | undefined;
    expect(commandArgs).toBeDefined();
    const resumeIndex = commandArgs!.indexOf("resume");
    const imageIndex = commandArgs!.indexOf("--image");
    expect(resumeIndex).toBeGreaterThan(-1);
    expect(imageIndex).toBeGreaterThan(-1);
    expect(resumeIndex).toBeLessThan(imageIndex);
    expect(commandArgs!.slice(resumeIndex)).toEqual(["resume", "thread-id", "--image", "img.png"]);
  });

  it("allows overriding the env passed to the Codex CLI", async () => {
    const { CodexExec } = await import("../src/exec");
    spawnMock.mockClear();
    const child = new FakeChildProcess();
    spawnMock.mockReturnValue(child as unknown as child_process.ChildProcess);

    setImmediate(() => {
      child.stdout.end();
      child.stderr.end();
      child.emit("exit", 0, null);
    });

    process.env.CODEX_ENV_SHOULD_NOT_LEAK = "leak";

    try {
      const exec = new CodexExec("codex", {
        CODEX_HOME: "/tmp/codex-home",
        CUSTOM_ENV: "custom",
      });

      for await (const _ of exec.run({
        input: "custom env",
        apiKey: "test",
        baseUrl: "https://example.test",
      })) {
        // no-op
      }

      const commandArgs = spawnMock.mock.calls[0]?.[1] as string[] | undefined;
      expect(commandArgs).toBeDefined();
      const spawnOptions = spawnMock.mock.calls[0]?.[2] as child_process.SpawnOptions | undefined;
      const spawnEnv = spawnOptions?.env as Record<string, string> | undefined;
      expect(spawnEnv).toBeDefined();
      if (!spawnEnv || !commandArgs) {
        throw new Error("Spawn args missing");
      }

      expect(spawnEnv.CODEX_HOME).toBe("/tmp/codex-home");
      expect(spawnEnv.CUSTOM_ENV).toBe("custom");
      expect(spawnEnv.CODEX_ENV_SHOULD_NOT_LEAK).toBeUndefined();
      expect(spawnEnv.CODEX_API_KEY).toBe("test");
      expect(spawnEnv.CODEX_INTERNAL_ORIGINATOR_OVERRIDE).toBe("codex_sdk_ts");
      expect(commandArgs).toContain("--config");
      expect(commandArgs).toContain(`openai_base_url=${JSON.stringify("https://example.test")}`);
    } finally {
      delete process.env.CODEX_ENV_SHOULD_NOT_LEAK;
    }
  });

  describe("installed runtime", () => {
    let packageRoot: string;
    let nativeRoot: string;
    let pathDir: string;
    let manifestPath: string;
    let nativeManifestPath: string;
    const targetTriple = "x86_64-pc-windows-msvc";

    beforeEach(() => {
      spawnMock.mockClear();
      packageRoot = mkdtempSync(path.join(tmpdir(), "codex-sdk-vendor-"));
      nativeManifestPath = path.join(packageRoot, "native", "package.json");
      nativeRoot = path.join(packageRoot, "native", "vendor", targetTriple);
      pathDir = path.join(nativeRoot, "codex-path");
      manifestPath = path.join(packageRoot, "package.json");
      mkdirSync(path.join(nativeRoot, "bin"), { recursive: true });
      mkdirSync(pathDir);
      writeFileSync(path.join(nativeRoot, "codex-package.json"), "{}");
      writeFileSync(path.join(nativeRoot, "bin", "codex.exe"), "");
      writeFileSync(nativeManifestPath, JSON.stringify({ version: "0.149.0-kd4" }));
      writeFileSync(
        manifestPath,
        JSON.stringify({
          optionalDependencies: { "@openai/codex-test-platform": "0.149.0-kd4" },
          codexNativeTargets: {
            [`${process.platform}-${process.arch}`]: {
              targetTriple,
              package: "@openai/codex-test-platform",
              binary: "codex.exe",
            },
          },
        }),
      );
      packageRequire.resolve.mockImplementation((id) => {
        if (id === "@openai/codex/package.json") return manifestPath;
        if (id === "@openai/codex-test-platform/package.json") return nativeManifestPath;
        throw new Error(`Unexpected package lookup: ${id}`);
      });
    });

    afterEach(() => {
      rmSync(packageRoot, { recursive: true, force: true });
      packageRequire.resolve.mockReset();
    });

    it.each(["0.148.0", null])("rejects native version %j before spawning", async (version) => {
      const { Codex } = await import("../src/codex");
      writeFileSync(nativeManifestPath, JSON.stringify({ version }));
      expect(() => new Codex()).toThrow(/Native package version mismatch/);
      expect(spawnMock).not.toHaveBeenCalled();
    });

    it.each([undefined, "", 42])("rejects invalid native pin %j", async (pin) => {
      const { Codex } = await import("../src/codex");
      const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
      manifest.optionalDependencies["@openai/codex-test-platform"] = pin;
      writeFileSync(manifestPath, JSON.stringify(manifest));
      expect(() => new Codex()).toThrow(/Invalid native dependency metadata/);
      expect(spawnMock).not.toHaveBeenCalled();
    });

    it("accepts an alias pin matching the native manifest", async () => {
      const { Codex } = await import("../src/codex");
      const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
      manifest.optionalDependencies["@openai/codex-test-platform"] =
        "npm:@openai/codex@0.149.0-kd4";
      writeFileSync(manifestPath, JSON.stringify(manifest));
      expect(() => new Codex()).not.toThrow();
      expect(spawnMock).not.toHaveBeenCalled();
    });

    it.each(["PATH", "Path"])(
      "launches the manifest binary and deduplicates %s",
      async (pathKey) => {
        const { Codex } = await import("../src/codex");
        const child = new FakeChildProcess();
        spawnMock.mockReturnValue(child as unknown as child_process.ChildProcess);
        setImmediate(() => {
          child.stdout.end(
            JSON.stringify({
              type: "item.completed",
              item: { id: "item-1", type: "agent_message", text: "runtime launched" },
            }) +
              "\n" +
              JSON.stringify({
                type: "turn.completed",
                usage: { input_tokens: 0, cached_input_tokens: 0, output_tokens: 0 },
              }) +
              "\n",
          );
          child.stderr.end();
          child.emit("exit", 0, null);
        });
        const existingPath = path.join(packageRoot, "existing-tools");
        const env = {
          PATH: "discard duplicate key",
          [pathKey]: `${existingPath}${path.delimiter}${pathDir}`,
        };
        const result = await new Codex({ env }).startThread().run("hello");

        expect(result.finalResponse).toBe("runtime launched");
        expect(spawnMock).toHaveBeenCalledTimes(1);
        const [executable, , options] = spawnMock.mock.calls[0]!;
        expect(executable).toBe(path.join(nativeRoot, "bin", "codex.exe"));
        const pathEntries = Object.entries(options!.env!).filter(
          ([key]) => key.toLowerCase() === "path",
        );
        expect(pathEntries).toEqual([[pathKey, `${pathDir}${path.delimiter}${existingPath}`]]);
      },
    );

    it.each(["legacy-layout", "missing-platform"])(
      "rejects %s before spawning",
      async (problem) => {
        const { Codex } = await import("../src/codex");
        if (problem === "legacy-layout") {
          rmSync(path.join(nativeRoot, "bin", "codex.exe"));
          mkdirSync(path.join(nativeRoot, "codex"));
          writeFileSync(path.join(nativeRoot, "codex", "codex.exe"), "");
        } else {
          writeFileSync(manifestPath, JSON.stringify({ codexNativeTargets: {} }));
        }

        expect(() => new Codex()).toThrow(
          problem === "legacy-layout"
            ? /Unable to locate Codex CLI binaries/
            : /Unsupported platform/,
        );
        expect(spawnMock).not.toHaveBeenCalled();
      },
    );
  });
});

describe("CodexExec mocked framing and parse failures", () => {
  it("decodes split UTF-8 and CRLF frames, including the final record without a newline", async () => {
    const { CodexExec } = await import("../src/exec");
    const { Thread } = await import("../src/thread");
    const child = new FakeChildProcess();
    const spawned = new Promise<void>((resolve) => {
      spawnMock.mockImplementationOnce(() => {
        resolve();
        return child as unknown as child_process.ChildProcess;
      });
    });
    const item = { id: "answer", type: "agent_message", text: "hello \u{1f642}" };
    const usage = { input_tokens: 3, cached_input_tokens: 0, output_tokens: 2 };
    const frames = [
      { type: "thread.started", thread_id: "fragmented-thread" },
      { type: "item.completed", item },
      { type: "turn.completed", usage },
    ];
    const wire = Buffer.from(frames.map((frame) => JSON.stringify(frame)).join("\r\n"));
    const crlf = wire.indexOf("\r\n");
    const emoji = wire.indexOf(Buffer.from("\u{1f642}"));
    const run = new Thread(new CodexExec("codex"), {}, {}).run("hello");
    await spawned;
    child.stdout.write(wire.subarray(0, crlf + 1));
    child.stdout.write(wire.subarray(crlf + 1, emoji + 2));
    child.stdout.end(wire.subarray(emoji + 2));
    child.stderr.end();
    child.emit("exit", 0, null);

    await expect(run).resolves.toMatchObject({ items: [item], finalResponse: item.text, usage });
    expect(child.killSignals).toEqual([]);
    expect(child.stdout.destroyed).toBe(true);
    expect(child.stderr.destroyed).toBe(true);
  });

  it("retains partial evidence on a malformed envelope, reaps its child, and permits resume", async () => {
    const { CodexExec } = await import("../src/exec");
    const { Thread, TurnRunError } = await import("../src/thread");
    const child = new FakeChildProcess();
    child.onKill = (signal) => child.emit("exit", null, signal);
    const spawned = new Promise<void>((resolve) => {
      spawnMock.mockImplementationOnce(() => {
        resolve();
        return child as unknown as child_process.ChildProcess;
      });
    });
    const thread = new Thread(new CodexExec("codex"), {}, {});
    const item = { id: "partial", type: "agent_message", text: "observed answer" };
    const result = thread.run("first").catch((error: unknown) => error);
    await spawned;
    child.stdout.write(
      [
        { type: "thread.started", thread_id: "parse-thread" },
        { type: "item.completed", item },
        { type: "item.completed", item: { id: "bad", type: "agent_message", text: 42 } },
      ]
        .map((frame) => JSON.stringify(frame) + "\n")
        .join(""),
    );

    const error = await result;
    expect(error).toBeInstanceOf(TurnRunError);
    expect(error).toMatchObject({
      threadId: "parse-thread",
      items: [item],
      finalResponse: item.text,
      cause: { message: expect.stringContaining("Failed to parse item") },
    });
    expect(child.killSignals).toEqual(["SIGTERM"]);
    expect(child.stdout.destroyed).toBe(true);
    expect(child.stderr.destroyed).toBe(true);

    const resumed = new FakeChildProcess();
    spawnMock.mockImplementationOnce(() => {
      setImmediate(() => {
        resumed.stdout.end(JSON.stringify({ type: "turn.completed", usage: null }) + "\n");
        resumed.stderr.end();
        resumed.emit("exit", 0, null);
      });
      return resumed as unknown as child_process.ChildProcess;
    });
    await expect(thread.run("retry")).resolves.toMatchObject({
      items: [],
      finalResponse: "",
      usage: null,
    });
    expect(spawnMock.mock.calls.at(-1)?.[1]).toEqual(expect.arrayContaining(["resume", "parse-thread"]));
    expect(resumed.killSignals).toEqual([]);
  });
});
