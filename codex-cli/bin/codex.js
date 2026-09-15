#!/usr/bin/env node
// Unified entry point for the Codex CLI.

import { spawn } from "node:child_process";
import { existsSync, realpathSync } from "fs";
import { createRequire } from "node:module";
import { constants } from "node:os";
import path from "path";
import { fileURLToPath } from "url";

// __dirname equivalent in ESM
const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const require = createRequire(import.meta.url);
async function main() {
  const codexPackageRoot = realpathSync(path.join(__dirname, ".."));
  const { platform, arch } = process;
  const packageJson = require(path.join(codexPackageRoot, "package.json"));
  const nativeTarget = packageJson.codexNativeTargets?.[`${platform}-${arch}`];

  if (nativeTarget === undefined) {
    throw new Error(`Unsupported platform: ${platform} (${arch})`);
  }

  if (
    !nativeTarget ||
    ["targetTriple", "package", "binary"].some(
      (field) =>
        typeof nativeTarget[field] !== "string" ||
        nativeTarget[field].trim().length === 0,
    )
  ) {
    throw new Error(
      `Invalid native target metadata for ${platform} (${arch}). Reinstall this KD4 package from the same fork release artifact.`,
    );
  }

  const targetTriple = nativeTarget.targetTriple;
  const platformPackage = nativeTarget.package;

  function findCodexExecutable() {
    let vendorRoot;
    try {
      const packageJsonPath = require.resolve(
        `${platformPackage}/package.json`,
      );
      vendorRoot = path.join(path.dirname(packageJsonPath), "vendor");
    } catch (error) {
      if (error.code !== "MODULE_NOT_FOUND") {
        throw error;
      }
      vendorRoot = path.join(__dirname, "..", "vendor");
    }

    const codexExecutable = path.join(
      vendorRoot,
      targetTriple,
      "bin",
      nativeTarget.binary,
    );
    if (existsSync(codexExecutable)) {
      return codexExecutable;
    }

    throw new Error(
      `Missing optional dependency ${platformPackage}. Reinstall this KD4 package from the same fork release artifact.`,
    );
  }

  const binaryPath = findCodexExecutable();

  // Use an asynchronous spawn instead of spawnSync so that Node is able to
  // respond to signals (e.g. Ctrl-C / SIGINT) while the native binary is
  // executing. This allows us to forward those signals to the child process
  // and guarantees that when either the child terminates or the parent
  // receives a fatal signal, both processes exit in a predictable manner.

  function isPnpmOwnedCodexInstall(nodeModulesDir) {
    if (!existsSync(path.join(nodeModulesDir, ".modules.yaml"))) {
      return false;
    }

    try {
      return (
        realpathSync(path.join(nodeModulesDir, "@openai", "codex")) ===
        codexPackageRoot
      );
    } catch {
      return false;
    }
  }

  /**
   * Use heuristics to detect the package manager that was used to install Codex
   * in order to give the user a hint about how to update it.
   */
  function detectPackageManager() {
    // pnpm's owning node_modules directory can be several parents above the
    // package in isolated global layouts. Search ancestors of both the canonical
    // package root and lexical entrypoint because pnpm may link either path.
    const entrypointDir = path.dirname(
      path.resolve(process.argv[1] ?? __filename),
    );
    const visited = new Set();
    for (const startDir of new Set([codexPackageRoot, entrypointDir])) {
      for (
        let currentDir = startDir;
        !visited.has(currentDir);
        currentDir = path.dirname(currentDir)
      ) {
        // Once the two walks meet, every remaining ancestor was already checked.
        // The filesystem root is its own parent, so it is checked exactly once.
        visited.add(currentDir);
        if (isPnpmOwnedCodexInstall(path.join(currentDir, "node_modules"))) {
          return "pnpm";
        }
      }
    }

    const userAgent = process.env.npm_config_user_agent || "";
    if (/\bbun\//.test(userAgent)) {
      return "bun";
    }

    const execPath = process.env.npm_execpath || "";
    if (execPath.includes("bun")) {
      return "bun";
    }

    if (
      __dirname.includes(".bun/install/global") ||
      __dirname.includes(".bun\\install\\global")
    ) {
      return "bun";
    }

    return userAgent ? "npm" : null;
  }

  const packageManager = detectPackageManager();
  const packageManagerEnvVar =
    packageManager === "bun"
      ? "CODEX_MANAGED_BY_BUN"
      : packageManager === "pnpm"
        ? "CODEX_MANAGED_BY_PNPM"
        : "CODEX_MANAGED_BY_NPM";
  const env = {
    ...process.env,
    CODEX_MANAGED_PACKAGE_ROOT: codexPackageRoot,
  };
  delete env.CODEX_MANAGED_BY_NPM;
  delete env.CODEX_MANAGED_BY_BUN;
  delete env.CODEX_MANAGED_BY_PNPM;
  env[packageManagerEnvVar] = "1";

  const startupError = (error) =>
    new Error(
      `Unable to start ${binaryPath}: ${error.message ?? String(error)}. Reinstall this KD4 package from the same fork release artifact.`,
    );
  let child;
  try {
    child = spawn(binaryPath, process.argv.slice(2), {
      stdio: "inherit",
      env,
    });
  } catch (error) {
    throw startupError(error);
  }

  // Forward common termination signals to the child so that it shuts down
  // gracefully. In the handler we temporarily disable the default behavior of
  // exiting immediately; once the child has been signaled we simply wait for
  // its exit event which will in turn terminate the parent (see below).
  const forwardSignal = (signal) => {
    try {
      child.kill(signal);
    } catch {
      /* ignore */
    }
  };

  const forwardedSignals = ["SIGINT", "SIGTERM"];
  const signalHandlers = new Map(
    forwardedSignals.map((signal) => [signal, () => forwardSignal(signal)]),
  );
  for (const [signal, handler] of signalHandlers) {
    process.on(signal, handler);
  }

  const removeSignalHandlers = () => {
    for (const [signal, handler] of signalHandlers) {
      process.off(signal, handler);
    }
  };

  // When the child exits, mirror its termination reason in the parent so that
  // shell scripts and other tooling observe the correct exit status.
  // Wrap the lifetime of the child process in a Promise so that we can await
  // its termination in a structured way. The Promise resolves with an object
  // describing how the child exited: either via exit code or due to a signal.
  let childResult;
  try {
    childResult = await new Promise((resolve, reject) => {
      child.once("error", (error) => {
        reject(startupError(error));
      });
      child.once("exit", (code, signal) => {
        if (signal) {
          resolve({ type: "signal", signal });
        } else {
          resolve({ type: "code", exitCode: code ?? 1 });
        }
      });
    });
  } finally {
    removeSignalHandlers();
  }

  if (childResult.type === "signal") {
    // Windows does not preserve POSIX signal exit status when signaling self.
    if (platform === "win32") {
      const signalNumber = constants.signals[childResult.signal];
      process.exit(signalNumber === undefined ? 1 : 128 + signalNumber);
    }
    // On POSIX, preserve signal termination for the invoking shell.
    process.kill(process.pid, childResult.signal);
  } else {
    process.exit(childResult.exitCode);
  }
}

await main().catch((error) => {
  // eslint-disable-next-line no-console
  console.error(`Codex could not start: ${error.message ?? String(error)}`);
  process.exitCode = 1;
});
