#!/usr/bin/env node

const { spawnSync } = require("node:child_process");

const scriptArgs = process.argv.slice(2);
if (scriptArgs.length === 0) {
  console.error("usage: run-python.js <script> [args...]");
  process.exit(2);
}

const configuredPython = process.env.PYTHON;
const candidates = configuredPython
  ? [[configuredPython, []]]
  : [
      ["py", ["-3"]],
      ["python3", []],
      ["python", []],
    ];

for (const [command, prefixArgs] of candidates) {
  const probe = spawnSync(
    command,
    [
      ...prefixArgs,
      "-c",
      "import sys; raise SystemExit(0 if sys.version_info >= (3, 11) else 1)",
    ],
    { stdio: "ignore" },
  );
  if (probe.error?.code === "ENOENT") {
    continue;
  }
  if (probe.error) {
    console.error(`failed to probe ${command}: ${probe.error.message}`);
    process.exit(1);
  }
  if (probe.status !== 0) {
    continue;
  }

  const result = spawnSync(command, [...prefixArgs, ...scriptArgs], {
    stdio: "inherit",
  });
  if (result.error?.code === "ENOENT") {
    continue;
  }
  if (result.error) {
    console.error(`failed to launch ${command}: ${result.error.message}`);
    process.exit(1);
  }
  process.exit(result.status ?? 1);
}

console.error(
  "Python 3 was not found. Set PYTHON or install a python3/python/py launcher.",
);
process.exit(127);
