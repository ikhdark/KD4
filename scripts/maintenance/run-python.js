#!/usr/bin/env node

const { spawnSync } = require("node:child_process");
const { join } = require("node:path");

const args = process.argv.slice(2);
if (!args.length) {
  console.error("usage: run-python.js <script> [args...]");
  process.exit(2);
}
const windows = process.platform === "win32";
const candidates = [];
if (process.env.PYTHON) candidates.push([process.env.PYTHON]);
if (process.env.VIRTUAL_ENV) {
  candidates.push([join(process.env.VIRTUAL_ENV, windows ? "Scripts" : "bin", windows ? "python.exe" : "python")]);
}
if (windows) candidates.push(["py", "-3"]);
candidates.push(["python3"], ["python"]);
const seen = new Set();
for (const [program, ...prefix] of candidates) {
  const key = JSON.stringify([program, ...prefix]);
  if (seen.has(key)) continue;
  seen.add(key);
  const probe = spawnSync(program, [...prefix, "-c", "import sys; sys.exit(0 if sys.version_info >= (3, 11) else 1)"], { stdio: "ignore" });
  if (probe.error || probe.status !== 0) continue;
  const result = spawnSync(program, [...prefix, ...args], { stdio: "inherit" });
  if (result.error) console.error(result.error.message);
  // A script failure is not an interpreter-discovery failure. Never rerun it.
  process.exit(result.status ?? 1);
}
console.error("Python 3.11+ is required; set PYTHON or install a compatible interpreter.");
process.exit(1);
