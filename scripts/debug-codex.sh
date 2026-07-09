#!/bin/bash

# Set "chatgpt.cliExecutable": "/Users/<USERNAME>/code/codex/scripts/debug-codex.sh" in VSCode settings to always get the
# latest codex-rs binary when debugging Codex Extension.


set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CODEX_RS_DIR="$(cd "$SCRIPT_DIR/../codex-rs" && pwd)"
CODEX_BIN="$CODEX_RS_DIR/target/debug/codex"
if [[ ! -x "$CODEX_BIN" && -x "$CODEX_BIN.exe" ]]; then
  CODEX_BIN="$CODEX_BIN.exe"
fi
if [[ "${CODEX_DEBUG_USE_EXISTING_BINARY:-0}" == 1 && -x "$CODEX_BIN" ]]; then
  "$CODEX_BIN" "$@"
else
  (cd "$CODEX_RS_DIR" && cargo run --quiet --bin codex -- "$@")
fi
