#!/usr/bin/env node

const { spawnSync } = require("node:child_process");
const { join } = require("node:path");

const args = process.argv.slice(2);
if (!args.length) {
  console.error("usage: run-python.js <script> [args...]");
  process.exit(2);
}
const windows = process.platform === "win32";
const python = process.env.PYTHON || (process.env.VIRTUAL_ENV
  ? join(process.env.VIRTUAL_ENV, windows ? "Scripts" : "bin", windows ? "python.exe" : "python")
  : windows ? "python" : "python3");
const result = spawnSync(python, args, { stdio: "inherit" });
if (result.error) {
  console.error(result.error.message);
}
process.exit(result.status ?? 1);
