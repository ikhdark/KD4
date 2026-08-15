#!/bin/sh
set -eu

require_env() {
  eval "value=\${$1-}"
  if [ -z "$value" ]; then
    echo "missing required env var: $1" >&2
    exit 1
  fi
}

require_env APP_SERVER_URL
require_env APP_SERVER_TEST_CLIENT_BIN

thread_id="${CODEX_THREAD_ID:-${THREAD_ID-}}"
if [ -z "$thread_id" ]; then
  echo "missing required env var: CODEX_THREAD_ID" >&2
  exit 1
fi

hold_seconds="${ELICITATION_HOLD_SECONDS:-15}"

echo "[elicitation-hold] increment thread=$thread_id"
"$APP_SERVER_TEST_CLIENT_BIN" --url "$APP_SERVER_URL" \
  thread-hold-elicitation "$thread_id" --hold-seconds "$hold_seconds"

echo "[elicitation-hold] done"
