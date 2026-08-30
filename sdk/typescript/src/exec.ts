import { spawn } from "node:child_process";
import { statSync } from "node:fs";
import path from "node:path";
import readline from "node:readline";
import { createRequire } from "node:module";

import type { CodexConfigObject, CodexConfigValue } from "./codexOptions";
import { SandboxMode, ModelReasoningEffort, ApprovalMode, WebSearchMode } from "./threadOptions";

export type CodexExecArgs = {
  input: string;

  baseUrl?: string;
  apiKey?: string;
  threadId?: string | null;
  images?: string[];
  // --model
  model?: string;
  // --sandbox
  sandboxMode?: SandboxMode;
  // --cd
  workingDirectory?: string;
  // --add-dir
  additionalDirectories?: string[];
  // --skip-git-repo-check
  skipGitRepoCheck?: boolean;
  // --output-schema
  outputSchemaFile?: string;
  // --config model_reasoning_effort
  modelReasoningEffort?: ModelReasoningEffort;
  // AbortSignal to cancel the execution
  signal?: AbortSignal;
  // --config sandbox_workspace_write.network_access
  networkAccessEnabled?: boolean;
  // --config web_search
  webSearchMode?: WebSearchMode;
  // legacy --config features.web_search_request
  webSearchEnabled?: boolean;
  // --config approval_policy
  approvalPolicy?: ApprovalMode;
};

const INTERNAL_ORIGINATOR_ENV = "CODEX_INTERNAL_ORIGINATOR_OVERRIDE";
const TYPESCRIPT_SDK_ORIGINATOR = "codex_sdk_ts";
const CODEX_NPM_NAME = "@openai/codex";
const CHILD_TERMINATION_GRACE_MS = 1_000;
const STDERR_TAIL_MAX_BYTES = 64 * 1024;
const STDERR_DRAIN_GRACE_MS = 1_000;
const STDERR_TRUNCATION_MARKER = `[stderr truncated; showing last ${STDERR_TAIL_MAX_BYTES} bytes]\n`;

type NativeTarget = {
  targetTriple: string;
  package: string;
  binary: string;
};

type CodexPackageManifest = {
  codexNativeTargets?: Record<string, unknown>;
};

const moduleRequire = createRequire(import.meta.url);

type CodexPathResolution = {
  executablePath: string;
  pathDirs: string[];
};

export class CodexExec {
  private executablePath: string;
  private pathDirs: string[];
  private envOverride?: Record<string, string>;
  private configOverrides?: CodexConfigObject;

  constructor(
    executablePath: string | null = null,
    env?: Record<string, string>,
    configOverrides?: CodexConfigObject,
  ) {
    if (executablePath) {
      this.executablePath = executablePath;
      this.pathDirs = [];
    } else {
      const resolved = findCodexPath();
      this.executablePath = resolved.executablePath;
      this.pathDirs = resolved.pathDirs;
    }
    this.envOverride = env;
    this.configOverrides = configOverrides;
  }

  async *run(args: CodexExecArgs): AsyncGenerator<string> {
    const commandArgs: string[] = ["exec", "--experimental-json"];

    if (this.configOverrides) {
      for (const override of serializeConfigOverrides(this.configOverrides)) {
        commandArgs.push("--config", override);
      }
    }

    if (args.baseUrl) {
      commandArgs.push(
        "--config",
        `openai_base_url=${toTomlValue(args.baseUrl, "openai_base_url")}`,
      );
    }

    if (args.model) {
      commandArgs.push("--model", args.model);
    }

    if (args.sandboxMode) {
      commandArgs.push("--sandbox", args.sandboxMode);
    }

    if (args.workingDirectory) {
      commandArgs.push("--cd", args.workingDirectory);
    }

    if (args.additionalDirectories?.length) {
      for (const dir of args.additionalDirectories) {
        commandArgs.push("--add-dir", dir);
      }
    }

    if (args.skipGitRepoCheck) {
      commandArgs.push("--skip-git-repo-check");
    }

    if (args.outputSchemaFile) {
      commandArgs.push("--output-schema", args.outputSchemaFile);
    }

    if (args.modelReasoningEffort) {
      commandArgs.push("--config", `model_reasoning_effort="${args.modelReasoningEffort}"`);
    }

    if (args.networkAccessEnabled !== undefined) {
      commandArgs.push(
        "--config",
        `sandbox_workspace_write.network_access=${args.networkAccessEnabled}`,
      );
    }

    if (args.webSearchMode) {
      commandArgs.push("--config", `web_search="${args.webSearchMode}"`);
    } else if (args.webSearchEnabled === true) {
      commandArgs.push("--config", `web_search="live"`);
    } else if (args.webSearchEnabled === false) {
      commandArgs.push("--config", `web_search="disabled"`);
    }

    if (args.approvalPolicy) {
      commandArgs.push("--config", `approval_policy="${args.approvalPolicy}"`);
    }

    if (args.threadId) {
      commandArgs.push("resume", args.threadId);
    }

    if (args.images?.length) {
      for (const image of args.images) {
        commandArgs.push("--image", image);
      }
    }

    const env: Record<string, string> = {};
    if (this.envOverride) {
      Object.assign(env, this.envOverride);
    } else {
      for (const [key, value] of Object.entries(process.env)) {
        if (value !== undefined) {
          env[key] = value;
        }
      }
    }
    if (!env[INTERNAL_ORIGINATOR_ENV]) {
      env[INTERNAL_ORIGINATOR_ENV] = TYPESCRIPT_SDK_ORIGINATOR;
    }
    if (args.apiKey) {
      env.CODEX_API_KEY = args.apiKey;
    }
    if (this.pathDirs.length > 0) {
      prependPathDirs(env, this.pathDirs);
    }

    const child = spawn(this.executablePath, commandArgs, {
      env,
      signal: args.signal,
    });

    let spawnError: unknown | null = null;
    let processExited = false;
    child.once("error", (err) => (spawnError = err));

    if (!child.stdin) {
      child.kill();
      throw new Error("Child process has no stdin");
    }
    child.stdin.write(args.input);
    child.stdin.end();

    if (!child.stdout) {
      child.kill();
      throw new Error("Child process has no stdout");
    }
    const stderrTail = new StderrTail();
    const stderr = child.stderr;
    const stderrDrainedPromise = stderr
      ? new Promise<void>((resolve) => {
          stderr.once("end", resolve);
          stderr.once("close", resolve);
          stderr.once("error", resolve);
        })
      : Promise.resolve();

    if (stderr) {
      stderr.on("data", (data) => {
        stderrTail.append(data);
      });
    }

    const exitPromise = new Promise<{ code: number | null; signal: NodeJS.Signals | null }>(
      (resolve) => {
        child.once("exit", (code, signal) => {
          processExited = true;
          resolve({ code, signal });
        });
      },
    );

    const rl = readline.createInterface({
      input: child.stdout,
      crlfDelay: Infinity,
    });

    try {
      for await (const line of rl) {
        // `line` is a string (Node sets default encoding to utf8 for readline)
        yield line as string;
      }

      if (spawnError) throw spawnError;
      const { code, signal } = await exitPromise;
      if (code !== 0 || signal) {
        await resolvesBeforeDeadline(stderrDrainedPromise, STDERR_DRAIN_GRACE_MS);
        const detail = signal ? `signal ${signal}` : `code ${code ?? 1}`;
        throw new Error(`Codex Exec exited with ${detail}: ${stderrTail.render()}`);
      }
    } finally {
      rl.close();
      try {
        if (!processExited) child.kill("SIGTERM");
      } catch {
        // ignore
      }
      if (!spawnError && !processExited) {
        const exitedGracefully = await resolvesBeforeDeadline(
          exitPromise,
          CHILD_TERMINATION_GRACE_MS,
        );
        if (!exitedGracefully && !processExited) {
          try {
            child.kill("SIGKILL");
          } catch {
            // ignore
          }
          if (!processExited) {
            await resolvesBeforeDeadline(exitPromise, CHILD_TERMINATION_GRACE_MS);
          }
        }
      }
      child.removeAllListeners();
    }
  }
}

class StderrTail {
  private bytes = Buffer.alloc(0);
  private truncated = false;

  append(data: unknown): void {
    const chunk =
      typeof data === "string"
        ? Buffer.from(data, "utf8")
        : Buffer.isBuffer(data)
          ? data
          : Buffer.from(data as Uint8Array);
    if (chunk.length === 0) {
      return;
    }

    if (chunk.length >= STDERR_TAIL_MAX_BYTES) {
      this.truncated ||= this.bytes.length > 0 || chunk.length > STDERR_TAIL_MAX_BYTES;
      this.bytes = Buffer.from(chunk.subarray(chunk.length - STDERR_TAIL_MAX_BYTES));
      return;
    }

    const overflow = this.bytes.length + chunk.length - STDERR_TAIL_MAX_BYTES;
    if (overflow > 0) {
      this.truncated = true;
      this.bytes = Buffer.concat([this.bytes.subarray(overflow), chunk], STDERR_TAIL_MAX_BYTES);
    } else {
      this.bytes = Buffer.concat([this.bytes, chunk]);
    }
  }

  render(): string {
    let start = 0;
    if (this.truncated) {
      while (start < Math.min(3, this.bytes.length) && (this.bytes[start]! & 0xc0) === 0x80) {
        start += 1;
      }
    }
    const text = this.bytes.subarray(start).toString("utf8");
    return this.truncated ? STDERR_TRUNCATION_MARKER + text : text;
  }
}

function resolvesBeforeDeadline<T>(promise: Promise<T>, timeoutMs: number): Promise<boolean> {
  return new Promise((resolve) => {
    const timeout = setTimeout(() => resolve(false), timeoutMs);
    void promise.then(() => {
      clearTimeout(timeout);
      resolve(true);
    });
  });
}

function serializeConfigOverrides(configOverrides: CodexConfigObject): string[] {
  const overrides: string[] = [];
  flattenConfigOverrides(configOverrides, "", overrides);
  return overrides;
}

function flattenConfigOverrides(
  value: CodexConfigValue,
  prefix: string,
  overrides: string[],
): void {
  if (!isPlainObject(value)) {
    if (prefix) {
      overrides.push(`${prefix}=${toTomlValue(value, prefix)}`);
      return;
    } else {
      throw new Error("Codex config overrides must be a plain object");
    }
  }

  const entries = Object.entries(value);
  if (!prefix && entries.length === 0) {
    return;
  }

  if (prefix && entries.length === 0) {
    overrides.push(`${prefix}={}`);
    return;
  }

  for (const [key, child] of entries) {
    if (!key) {
      throw new Error("Codex config override keys must be non-empty strings");
    }
    if (child === undefined) {
      continue;
    }
    const path = prefix ? `${prefix}.${key}` : key;
    if (isPlainObject(child)) {
      flattenConfigOverrides(child, path, overrides);
    } else {
      overrides.push(`${path}=${toTomlValue(child, path)}`);
    }
  }
}

function toTomlValue(value: CodexConfigValue, path: string): string {
  if (typeof value === "string") {
    return JSON.stringify(value);
  } else if (typeof value === "number") {
    if (!Number.isFinite(value)) {
      throw new Error(`Codex config override at ${path} must be a finite number`);
    }
    return `${value}`;
  } else if (typeof value === "boolean") {
    return value ? "true" : "false";
  } else if (Array.isArray(value)) {
    const rendered = value.map((item, index) => toTomlValue(item, `${path}[${index}]`));
    return `[${rendered.join(", ")}]`;
  } else if (isPlainObject(value)) {
    const parts: string[] = [];
    for (const [key, child] of Object.entries(value)) {
      if (!key) {
        throw new Error("Codex config override keys must be non-empty strings");
      }
      if (child === undefined) {
        continue;
      }
      parts.push(`${formatTomlKey(key)} = ${toTomlValue(child, `${path}.${key}`)}`);
    }
    return `{${parts.join(", ")}}`;
  } else if (value === null) {
    throw new Error(`Codex config override at ${path} cannot be null`);
  } else {
    const typeName = typeof value;
    throw new Error(`Unsupported Codex config override value at ${path}: ${typeName}`);
  }
}

const TOML_BARE_KEY = /^[A-Za-z0-9_-]+$/;
function formatTomlKey(key: string): string {
  return TOML_BARE_KEY.test(key) ? key : JSON.stringify(key);
}

function isPlainObject(value: unknown): value is CodexConfigObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function findCodexPath(): CodexPathResolution {
  const { platform, arch } = process;
  let codexPackageJsonPath: string;
  try {
    codexPackageJsonPath = moduleRequire.resolve(`${CODEX_NPM_NAME}/package.json`);
  } catch {
    throw new Error(
      `Unable to locate Codex CLI binaries. Ensure ${CODEX_NPM_NAME} is installed with optional dependencies.`,
    );
  }

  const manifest = moduleRequire(codexPackageJsonPath) as CodexPackageManifest;
  const nativeTarget = resolveNativeTarget(manifest, platform, arch);
  if (!nativeTarget) {
    throw new Error(`Unsupported platform: ${platform} (${arch})`);
  }

  let vendorRoot: string;
  try {
    const codexRequire = createRequire(codexPackageJsonPath);
    const platformPackageJsonPath = codexRequire.resolve(`${nativeTarget.package}/package.json`);
    vendorRoot = path.join(path.dirname(platformPackageJsonPath), "vendor");
  } catch {
    throw new Error(
      `Unable to locate Codex CLI binaries. Ensure ${CODEX_NPM_NAME} is installed with optional dependencies.`,
    );
  }

  const nativePackage = resolveNativePackage(
    vendorRoot,
    nativeTarget.targetTriple,
    nativeTarget.binary,
  );
  if (!nativePackage) {
    throw new Error(
      `Unable to locate Codex CLI binaries for ${nativeTarget.targetTriple}. Ensure ${CODEX_NPM_NAME} is installed with optional dependencies.`,
    );
  }

  return nativePackage;
}

export function resolveNativeTarget(
  manifest: CodexPackageManifest,
  platform: string,
  arch: string,
): NativeTarget | null {
  const candidate = manifest.codexNativeTargets?.[`${platform}-${arch}`];
  if (
    typeof candidate !== "object" ||
    candidate === null ||
    !("targetTriple" in candidate) ||
    typeof candidate.targetTriple !== "string" ||
    !("package" in candidate) ||
    typeof candidate.package !== "string" ||
    !("binary" in candidate) ||
    typeof candidate.binary !== "string"
  ) {
    return null;
  }
  return candidate as NativeTarget;
}

export function resolveNativePackage(
  vendorRoot: string,
  targetTriple: string,
  codexBinaryName: string,
): CodexPathResolution | null {
  const packageRoot = path.join(vendorRoot, targetTriple);
  const packageBinaryPath = path.join(packageRoot, "bin", codexBinaryName);
  if (isFile(packageBinaryPath) && isFile(path.join(packageRoot, "codex-package.json"))) {
    return {
      executablePath: packageBinaryPath,
      pathDirs: existingDirs(path.join(packageRoot, "codex-path")),
    };
  }

  return null;
}

function existingDirs(...dirs: string[]): string[] {
  return dirs.filter(isDirectory);
}

export function prependPathDirs(env: Record<string, string>, pathDirs: string[]): void {
  const pathKey = pathEnvKey(env);
  for (const key of Object.keys(env)) {
    if (key.toLowerCase() === "path" && key !== pathKey) {
      delete env[key];
    }
  }

  const existingEntries = (env[pathKey] ?? "")
    .split(path.delimiter)
    .filter((entry) => entry.length > 0 && !pathDirs.includes(entry));
  env[pathKey] = [...pathDirs, ...existingEntries].join(path.delimiter);
}

function pathEnvKey(env: Record<string, string>): string {
  const matchingKeys = Object.keys(env).filter((key) => key.toLowerCase() === "path");
  return matchingKeys.includes("Path") ? "Path" : (matchingKeys.at(-1) ?? "PATH");
}

function isFile(filePath: string): boolean {
  try {
    return statSync(filePath).isFile();
  } catch {
    return false;
  }
}

function isDirectory(filePath: string): boolean {
  try {
    return statSync(filePath).isDirectory();
  } catch {
    return false;
  }
}
