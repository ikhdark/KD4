#!/usr/bin/env node

// Stage the context-meter fix for Desktop 26.924.2738.0 without installing it.
// Usage: node scripts/install/prepare-desktop-context-patch.mjs --asar PATH --out NEW_DIR
// Optional diagnostic: --usage-rollout SAVED_JSONL --usage-line LINE
import { createHash } from "node:crypto";
import {
  closeSync,
  fstatSync,
  mkdirSync,
  openSync,
  readFileSync,
  readSync,
  writeFileSync,
} from "node:fs";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { parseArgs } from "node:util";
import { runInNewContext } from "node:vm";

const asset = "webview/assets/app-primary-2a3f3664ac09.js";
const expectedHash =
  "f94dda2d2248b276da6a1772d969775d3ea7a17329e2cd93f335eb131eed73d2";
const beforeFunction =
  "function KUt(e){let t=e?.modelContextWindow??null,n=e?.last.totalTokens??null;if(t==null||t<=0||n==null||n<0)return{percent:null,usedTokens:null,contextWindow:null,remainingTokens:null};let r=Math.min(n,t),i=r/t*100;return Number.isFinite(i)?{percent:i,usedTokens:r,contextWindow:t,remainingTokens:Math.max(t-r,0)}:{percent:null,usedTokens:null,contextWindow:null,remainingTokens:null}}";
const afterFunction = beforeFunction.replace(
  "n=e?.last.totalTokens??null",
  "n=e?.estimatedContextTokens??e?.last.totalTokens??null",
);
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

function readAsset(archive) {
  const fd = openSync(archive, "r");
  try {
    const size = fstatSync(fd).size;
    const prefix = Buffer.alloc(16);
    if (
      readSync(fd, prefix, 0, prefix.length, 0) !== prefix.length ||
      prefix.readUInt32LE(0) !== 4
    ) {
      throw new Error("Invalid ASAR prefix");
    }
    const headerSize = prefix.readUInt32LE(4);
    const jsonSize = prefix.readUInt32LE(12);
    if (
      jsonSize > 64 * 1024 * 1024 ||
      jsonSize + 8 > headerSize ||
      headerSize + 8 > size
    ) {
      throw new Error("Invalid ASAR header size");
    }
    const headerBytes = Buffer.alloc(jsonSize);
    if (readSync(fd, headerBytes, 0, jsonSize, 16) !== jsonSize)
      throw new Error("Incomplete ASAR header");
    let entry = JSON.parse(headerBytes.toString("utf8"));
    for (const part of asset.split("/")) entry = entry?.files?.[part];
    if (!entry || entry.unpacked || entry.link)
      throw new Error("Expected packed Desktop asset is missing");
    const offset = headerSize + 8 + Number(entry.offset);
    if (
      !Number.isSafeInteger(offset) ||
      offset < headerSize + 8 ||
      !Number.isSafeInteger(entry.size) ||
      entry.size < 0 ||
      entry.size > 8 * 1024 * 1024 ||
      offset + entry.size > size
    ) {
      throw new Error("Invalid ASAR asset range");
    }
    const bytes = Buffer.alloc(entry.size);
    if (readSync(fd, bytes, 0, bytes.length, offset) !== bytes.length)
      throw new Error("Incomplete Desktop asset");
    if (
      sha256(bytes) !== expectedHash ||
      entry.integrity?.algorithm !== "SHA256" ||
      entry.integrity.hash !== expectedHash
    ) {
      throw new Error(
        "Unsupported Desktop asset: its SHA-256 does not match the inspected build",
      );
    }
    return bytes;
  } finally {
    closeSync(fd);
  }
}

function capturedUsage(rollout, lineNumber, originalFunction, patchedFunction) {
  if (!Number.isSafeInteger(lineNumber) || lineNumber < 1)
    throw new Error("--usage-line must be a positive integer");
  const line = readFileSync(rollout, "utf8").split(/\r?\n/)[lineNumber - 1];
  if (!line) throw new Error("The selected rollout line is missing");
  const event = JSON.parse(line);
  const info = event.payload?.info;
  if (
    event.type !== "event_msg" ||
    event.payload?.type !== "token_count" ||
    !info
  ) {
    throw new Error("Select an inline token_count event with usage info");
  }
  // The app-server projection keeps these values unchanged, using camelCase.
  const usage = {
    last: { totalTokens: info.last_token_usage.total_tokens },
    modelContextWindow: info.model_context_window,
  };
  if (info.estimated_context_tokens !== undefined)
    usage.estimatedContextTokens = info.estimated_context_tokens;
  return {
    rollout: resolve(rollout),
    line: lineNumber,
    lineSha256: sha256(line),
    timestamp: event.timestamp,
    usage,
    original: runInNewContext(
      `(${originalFunction})(usage)`,
      { usage },
      { timeout: 1000 },
    ),
    patched: runInNewContext(
      `(${patchedFunction})(usage)`,
      { usage },
      { timeout: 1000 },
    ),
  };
}

function main() {
  const { values } = parseArgs({
    options: {
      "asar": { type: "string" },
      "out": { type: "string" },
      "usage-rollout": { type: "string" },
      "usage-line": { type: "string" },
    },
  });
  if (!values.asar || !values.out)
    throw new Error("Required: --asar PATH --out NEW_DIR");
  if (Boolean(values["usage-rollout"]) !== Boolean(values["usage-line"])) {
    throw new Error("Supply --usage-rollout and --usage-line together");
  }
  const archive = resolve(values.asar);
  const output = resolve(values.out);
  const app = dirname(dirname(archive));
  const fromApp = relative(app, output);
  const insideApp =
    !fromApp ||
    (!isAbsolute(fromApp) &&
      fromApp !== ".." &&
      !fromApp.startsWith(".." + sep));
  if (
    insideApp ||
    output.split(/[\\/]/).some((part) => part.toLowerCase() === "windowsapps")
  ) {
    throw new Error("Output must be outside the installed app and WindowsApps");
  }
  const original = readAsset(archive);
  const source = original.toString("utf8");
  const index = source.indexOf(beforeFunction);
  if (index < 0 || index !== source.lastIndexOf(beforeFunction))
    throw new Error("Expected exactly one inspected context-meter function");
  const patched = Buffer.from(
    source.replace(beforeFunction, afterFunction),
    "utf8",
  );
  const receipt = {
    status: "staged-only-not-installed",
    archive,
    asset,
    originalSha256: sha256(original),
    patchedSha256: sha256(patched),
    function: "KUt",
    changeCount: 1,
    originalExpression: "e?.last.totalTokens??null",
    patchedExpression: "e?.estimatedContextTokens??e?.last.totalTokens??null",
    installedArchiveModified: false,
  };
  if (values["usage-rollout"]) {
    receipt.capturedUsage = capturedUsage(
      values["usage-rollout"],
      Number(values["usage-line"]),
      source.slice(index, index + beforeFunction.length),
      patched.toString("utf8").slice(index, index + afterFunction.length),
    );
  }
  mkdirSync(output); // Refuse an existing output directory; never replace user work.
  for (const [variant, bytes] of [
    ["original", original],
    ["patched", patched],
  ]) {
    const path = join(output, variant, asset);
    mkdirSync(dirname(path), { recursive: true });
    writeFileSync(path, bytes, { flag: "wx" });
  }
  writeFileSync(
    join(output, "receipt.json"),
    JSON.stringify(receipt, null, 2) + "\n",
    { flag: "wx" },
  );
  console.log(JSON.stringify({ output, ...receipt }, null, 2));
}

try {
  main();
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
