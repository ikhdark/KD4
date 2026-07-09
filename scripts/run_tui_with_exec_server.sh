#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cargo_root="$repo_root/codex-rs"
listen_url="${CODEX_EXEC_SERVER_LISTEN_URL:-ws://127.0.0.1:0}"
start_timeout_seconds="${CODEX_EXEC_SERVER_START_TIMEOUT_SECONDS:-120}"
log_max_lines="${CODEX_EXEC_SERVER_LOG_MAX_LINES:-80}"
build_missing_binaries="${CODEX_BUILD_MISSING_BINARIES:-1}"
build_profile="${CODEX_BUILD_PROFILE:-debug}"
ready_file="${CODEX_EXEC_SERVER_READY_FILE:-}"
reuse_ready_file="${CODEX_EXEC_SERVER_REUSE_READY_FILE:-1}"
tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/codex-tui-with-exec-server.XXXXXX")"
stdout_log="$tmp_dir/exec-server.stdout"
stderr_log="$tmp_dir/exec-server.stderr"
server_pid=""
server_stdout_fd=""
server_process_group=0
started_server=0
exec_server_url="${CODEX_EXEC_SERVER_URL:-}"

cleanup() {
  if [[ -n "$server_pid" ]]; then
    if [[ "$server_process_group" == 1 ]]; then
      kill -- "-$server_pid" >/dev/null 2>&1 || true
    else
      kill "$server_pid" >/dev/null 2>&1 || true
    fi
    wait "$server_pid" >/dev/null 2>&1 || true
  fi
  rm -rf "$tmp_dir"
}

trap cleanup EXIT INT TERM HUP

is_ws_url() {
  [[ "${1:-}" == ws://* || "${1:-}" == wss://* ]]
}

probe_exec_server_url() {
  local url="$1"
  local python_bin=""
  if command -v python3 >/dev/null 2>&1; then
    python_bin="python3"
  elif command -v python >/dev/null 2>&1; then
    python_bin="python"
  else
    return 1
  fi

  "$python_bin" - "$url" <<'PY'
import socket
import sys
from urllib.parse import urlparse

parsed = urlparse(sys.argv[1])
if parsed.scheme not in {"ws", "wss"} or not parsed.hostname:
    raise SystemExit(1)
port = parsed.port or (443 if parsed.scheme == "wss" else 80)
try:
    with socket.create_connection((parsed.hostname, port), timeout=1.0):
        pass
except OSError:
    raise SystemExit(1)
PY
}

candidate_bin() {
  local name="$1"
  local profile
  for profile in release debug; do
    if [[ -x "$cargo_root/target/$profile/$name" ]]; then
      printf '%s\n' "$cargo_root/target/$profile/$name"
      return 0
    fi
    if [[ -x "$cargo_root/target/$profile/$name.exe" ]]; then
      printf '%s\n' "$cargo_root/target/$profile/$name.exe"
      return 0
    fi
  done
  return 1
}

target_bin() {
  local name="$1"
  if [[ -x "$cargo_root/target/$build_profile/$name" ]]; then
    printf '%s\n' "$cargo_root/target/$build_profile/$name"
  else
    printf '%s\n' "$cargo_root/target/$build_profile/$name.exe"
  fi
}

resolve_bin() {
  local override="$1"
  local name="$2"
  if [[ -n "$override" ]]; then
    printf '%s\n' "$override"
    return 0
  fi
  candidate_bin "$name"
}

build_cargo_packages() {
  if [[ "$build_missing_binaries" == 0 ]]; then
    return 0
  fi

  local cargo_args=(build --quiet)
  if [[ "$build_profile" == "release" ]]; then
    cargo_args+=(--release)
  elif [[ "$build_profile" != "debug" ]]; then
    cargo_args+=(--profile "$build_profile")
  fi
  cargo_args+=("$@")

  (
    cd "$cargo_root"
    cargo "${cargo_args[@]}"
  )
}

build_cli_binary() {
  build_cargo_packages -p codex-cli --bin codex
}

build_tui_binary() {
  build_cargo_packages -p codex-tui --bin codex-tui
}

build_both_binaries() {
  build_cargo_packages -p codex-cli --bin codex -p codex-tui --bin codex-tui
}

dump_file_tail() {
  local label="$1"
  local path="$2"
  if [[ ! -s "$path" ]]; then
    return
  fi

  local line_count
  line_count="$(wc -l <"$path" | tr -d ' ')"
  if [[ "$line_count" -gt "$log_max_lines" ]]; then
    echo "---- $label: output truncated to last $log_max_lines of $line_count lines ----" >&2
    tail -n "$log_max_lines" "$path" >&2 || true
  else
    echo "---- $label ----" >&2
    cat "$path" >&2 || true
  fi
}

dump_startup_logs() {
  dump_file_tail "exec-server stderr" "$stderr_log"
  dump_file_tail "exec-server stdout" "$stdout_log"
}

load_cached_url() {
  if [[ -z "$ready_file" || "$reuse_ready_file" == 0 || ! -s "$ready_file" ]]; then
    return 1
  fi
  local cached
  IFS= read -r cached <"$ready_file" || return 1
  cached="${cached%$'\r'}"
  if is_ws_url "$cached" && probe_exec_server_url "$cached"; then
    exec_server_url="$cached"
    return 0
  fi
  return 1
}

persist_ready_url() {
  if [[ -n "$ready_file" ]]; then
    mkdir -p "$(dirname "$ready_file")"
    printf '%s\n' "$exec_server_url" >"$ready_file"
  fi
}

resolve_binaries() {
  codex_bin="$(resolve_bin "${CODEX_CLI_BIN:-}" codex || true)"
  tui_bin="$(resolve_bin "${CODEX_TUI_BIN:-}" codex-tui || true)"

  if [[ -z "$codex_bin" || -z "$tui_bin" ]]; then
    if [[ -z "$codex_bin" && -z "$tui_bin" ]]; then
      build_both_binaries
    elif [[ -z "$codex_bin" ]]; then
      build_cli_binary
    else
      build_tui_binary
    fi
    codex_bin="${codex_bin:-$(target_bin codex)}"
    tui_bin="${tui_bin:-$(target_bin codex-tui)}"
  fi
}

resolve_tui_binary() {
  tui_bin="$(resolve_bin "${CODEX_TUI_BIN:-}" codex-tui || true)"
  if [[ -z "$tui_bin" ]]; then
    build_tui_binary
    tui_bin="$(target_bin codex-tui)"
  fi
}

start_exec_server() {
  local server_cmd=("$codex_bin" exec-server --listen "$listen_url")
  if [[ ! -x "$codex_bin" ]]; then
    server_cmd=(cargo run --quiet -p codex-cli --bin codex -- exec-server --listen "$listen_url")
  fi

  if command -v setsid >/dev/null 2>&1; then
    coproc EXEC_SERVER { setsid "${server_cmd[@]}" 2>"$stderr_log"; }
    server_process_group=1
  else
    coproc EXEC_SERVER { "${server_cmd[@]}" 2>"$stderr_log"; }
    server_process_group=0
  fi

  server_pid="$EXEC_SERVER_PID"
  server_stdout_fd="${EXEC_SERVER[0]}"
  started_server=1
}

wait_for_exec_server_url() {
  local deadline=$((SECONDS + start_timeout_seconds))
  local line

  while (( SECONDS < deadline )); do
    if IFS= read -r -t 0.25 -u "$server_stdout_fd" line; then
      line="${line%$'\r'}"
      printf '%s\n' "$line" >>"$stdout_log"
      if is_ws_url "$line"; then
        exec_server_url="$line"
        persist_ready_url
        return 0
      fi
      continue
    fi

    if ! kill -0 "$server_pid" >/dev/null 2>&1; then
      dump_startup_logs
      echo "failed to start codex exec-server" >&2
      exit 1
    fi
  done

  dump_startup_logs
  echo "timed out waiting ${start_timeout_seconds}s for codex exec-server to report its websocket URL" >&2
  exit 1
}

run_tui() {
  export CODEX_EXEC_SERVER_URL="$exec_server_url"
  echo "Starting codex-tui with CODEX_EXEC_SERVER_URL=$CODEX_EXEC_SERVER_URL" >&2

  if [[ -x "$tui_bin" ]]; then
    "$tui_bin" -c mcp_oauth_credentials_store=file "$@"
    return
  fi

  cd "$cargo_root"
  cargo run --quiet -p codex-tui --bin codex-tui -- -c mcp_oauth_credentials_store=file "$@"
}

if ! is_ws_url "$exec_server_url"; then
  if ! load_cached_url; then
    resolve_binaries
    start_exec_server
    wait_for_exec_server_url
  else
    resolve_tui_binary
  fi
else
  resolve_tui_binary
fi

run_tui "$@"
