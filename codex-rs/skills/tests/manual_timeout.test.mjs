import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fetchCodexManual } from "../src/assets/samples/openai-docs/scripts/fetch-codex-manual.mjs";

test("body deadline aborts stalled transfers without replacing cached bytes", async () => {
  const proxy = Object.fromEntries(["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"].map(k => [k, process.env[k]]));
  for (const key of Object.keys(proxy)) delete process.env[key];
  const cacheDir = await mkdtemp(path.join(os.tmpdir(), "manual-deadline-"));
  const manual = "# Manual\n\n## Safe operation\nKeep user work.\n";
  const hash = createHash("sha256").update(manual).digest("hex");
  let delay = false;
  let gets = 0;
  const server = http.createServer((req, res) => {
    res.writeHead(200, {"x-content-sha256": hash});
    res.flushHeaders();
    if (req.method === "HEAD") return res.end();
    gets++;
    const timer = setTimeout(() => res.end(manual), delay ? 5000 : 0);
    res.on("close", () => clearTimeout(timer));
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  const options = {manualUrl: `http://127.0.0.1:${server.address().port}/manual`, cacheDir, timeoutMs: 100};
  try {
    const first = await fetchCodexManual(options);
    assert.equal(await readFile(first.status.manualPath, "utf8"), manual);
    delay = true;
    assert.equal((await fetchCodexManual(options)).status.cacheStatus, "hit");
    assert.equal(gets, 1, "a valid cached body needs no GET");
    await writeFile(first.status.manualPath, "older cached manual");
    const start = performance.now();
    await assert.rejects(fetchCodexManual(options), /could not be fetched/);
    assert(performance.now() - start < 4000, "neither transport may wait for the stalled body");
    assert.equal(await readFile(first.status.manualPath, "utf8"), "older cached manual");
  } finally {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
    await rm(cacheDir, {recursive: true});
    for (const [key, value] of Object.entries(proxy)) {
      if (value === undefined) delete process.env[key]; else process.env[key] = value;
    }
  }
});
