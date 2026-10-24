const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const { runInNewContext } = require("node:vm");
const { test } = require("node:test");
const assert = require("node:assert/strict");

const source = readFileSync(join(__dirname, "run-python.js"), "utf8");
test("compatible Windows launcher fallback runs the script exactly once", () => {
  const calls = [];
  const exit = {};
  let code;
  assert.throws(() => runInNewContext(source, {
    require(name) {
      if (name === "node:path") return require(name);
      return { spawnSync(program, args) {
        calls.push([program, args]);
        return { status: program === "py" ? 0 : 1 };
      }};
    },
    console,
    process: { platform: "win32", env: {}, argv: ["node", "launcher", "format.py"],
      exit(value) { code = value; throw exit; } },
  }), (error) => error === exit);
  assert.equal(code, 0);
  assert.deepEqual(calls.map(([program]) => program), ["py", "py"]);
  assert.deepEqual(Array.from(calls[1][1]), ["-3", "format.py"]);
});

test("an incompatible interpreter is skipped but a script failure is not retried", () => {
  const calls = [];
  const exit = {};
  let code;
  assert.throws(() => runInNewContext(source, {
    require(name) {
      if (name === "node:path") return require(name);
      return { spawnSync(program, args) {
        calls.push([program, args]);
        return { status: program === "old-python" ? 1 : args.includes("-c") ? 0 : 23 };
      }};
    },
    console,
    process: { platform: "win32", env: { PYTHON: "old-python" }, argv: ["node", "launcher", "format.py"],
      exit(value) { code = value; throw exit; } },
  }), (error) => error === exit);
  assert.equal(code, 23);
  assert.deepEqual(calls.map(([program]) => program), ["old-python", "py", "py"]);
});
