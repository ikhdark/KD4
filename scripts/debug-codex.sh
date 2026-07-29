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
USE_EXISTING_BINARY=false
case "${CODEX_DEBUG_USE_EXISTING_BINARY:-0}" in
  1|[Tt][Rr][Uu][Ee]|[Yy][Ee][Ss]|[Oo][Nn])
    USE_EXISTING_BINARY=true
    ;;
esac
if [[ "$USE_EXISTING_BINARY" == true && -x "$CODEX_BIN" ]]; then
  "$CODEX_BIN" "$@"
else
  if [[ "$USE_EXISTING_BINARY" == true ]]; then
    printf 'CODEX_DEBUG_USE_EXISTING_BINARY is enabled, but %s is unavailable; rebuilding with cargo.\n' "$CODEX_BIN" >&2
  fi
  (cd "$CODEX_RS_DIR" && cargo run --quiet --bin codex -- "$@")
fi
