import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";

import { afterEach, beforeEach } from "@jest/globals";

const originalCodexHome = process.env.CODEX_HOME;
const originalCodexSqliteHome = process.env.CODEX_SQLITE_HOME;
let currentCodexHome: string | undefined;

beforeEach(async () => {
  currentCodexHome = await fs.mkdtemp(path.join(os.tmpdir(), "codex-sdk-test-"));
  process.env.CODEX_HOME = currentCodexHome;
  process.env.CODEX_SQLITE_HOME = path.join(currentCodexHome, "sqlite");
});

afterEach(async () => {
  const codexHomeToDelete = currentCodexHome;
  currentCodexHome = undefined;

  if (originalCodexHome === undefined) {
    delete process.env.CODEX_HOME;
  } else {
    process.env.CODEX_HOME = originalCodexHome;
  }

  if (originalCodexSqliteHome === undefined) {
    delete process.env.CODEX_SQLITE_HOME;
  } else {
    process.env.CODEX_SQLITE_HOME = originalCodexSqliteHome;
  }

  if (codexHomeToDelete) {
    await fs.rm(codexHomeToDelete, { recursive: true, force: true });
  }
});
