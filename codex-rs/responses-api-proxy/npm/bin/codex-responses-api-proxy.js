#!/usr/bin/env node
// Entry point for the Codex responses API proxy binary.

import { spawn } from "node:child_process";
import { constants } from "node:os";
import path from "path";
import { fileURLToPath } from "url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

function determineTargetTriple(platform, arch) {
  switch (platform) {
    case "linux":
    case "android":
      if (arch === "x64") {
        return "x86_64-unknown-linux-musl";
      }
      if (arch === "arm64") {
        return "aarch64-unknown-linux-musl";
      }
      break;
    case "darwin":
      if (arch === "x64") {
        return "x86_64-apple-darwin";
      }
      if (arch === "arm64") {
        return "aarch64-apple-darwin";
      }
      break;
    case "win32":
      if (arch === "x64") {
        return "x86_64-pc-windows-msvc";
      }
      if (arch === "arm64") {
        return "aarch64-pc-windows-msvc";
      }
      break;
    default:
      break;
  }
  return null;
}

const targetTriple = determineTargetTriple(process.platform, process.arch);
if (!targetTriple) {
  throw new Error(
    `Unsupported platform: ${process.platform} (${process.arch})`,
  );
}

const vendorRoot = path.join(__dirname, "..", "vendor");
const archRoot = path.join(vendorRoot, targetTriple);
const binaryBaseName = "codex-responses-api-proxy";
const binaryPath = path.join(
  archRoot,
  binaryBaseName,
  process.platform === "win32" ? `${binaryBaseName}.exe` : binaryBaseName,
);

// Keep these lifecycle rules synchronized with codex-cli/bin/codex.js. The
// proxy is staged independently, so it cannot import the main package's files.
async function runNative() {
  const child = spawn(binaryPath, process.argv.slice(2), {
    stdio: "inherit",
  });

  const signalHandlers = new Map(
    ["SIGINT", "SIGTERM", "SIGHUP"].map((signal) => [
      signal,
      () => forwardSignal(signal),
    ]),
  );
  const removeSignalHandlers = () => {
    for (const [signal, handler] of signalHandlers) {
      process.off(signal, handler);
    }
  };
  const exitForSignal = (signal) => {
    removeSignalHandlers();
    if (process.platform === "win32") {
      const signalNumber = constants.signals[signal];
      process.exit(signalNumber === undefined ? 1 : 128 + signalNumber);
    }
    process.kill(process.pid, signal);
  };
  const forwardSignal = (signal) => {
    // Windows console Ctrl-C already reaches the native child. TerminateProcess
    // would prevent its own handler from completing; mirror its eventual exit.
    if (process.platform === "win32" && signal === "SIGINT") {
      return;
    }
    let detail = "the child process may still be running";
    try {
      if (child.kill(signal)) {
        return;
      }
    } catch (error) {
      detail = error.message ?? String(error);
    }
    console.error(
      `Unable to forward ${signal} to responses-api-proxy: ${detail}.`,
    );
    exitForSignal(signal);
  };
  for (const [signal, handler] of signalHandlers) {
    process.on(signal, handler);
  }

  let childResult;
  try {
    childResult = await new Promise((resolve, reject) => {
      child.once("error", reject);
      child.once("exit", (code, signal) => {
        resolve(signal ? { signal } : { exitCode: code ?? 1 });
      });
    });
  } finally {
    removeSignalHandlers();
  }

  if (childResult.signal) {
    exitForSignal(childResult.signal);
  } else {
    process.exit(childResult.exitCode);
  }
}

await runNative().catch((error) => {
  console.error(
    `responses-api-proxy could not start: ${error.message ?? String(error)}`,
  );
  process.exitCode = 1;
});
