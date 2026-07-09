#!/usr/bin/env bash

# Remote-env setup script for codex-rs integration tests.
#
# Usage (source-only):
#   source scripts/test-remote-env.sh
#   cd codex-rs
#   just test -p codex-core --test all remote_test_env_can_connect_and_use_filesystem
#   codex_remote_env_cleanup
#
# Fast-path knobs:
#   CODEX_TEST_REMOTE_ENV_CODEX_BINARY=/path/to/codex
#   CODEX_TEST_REMOTE_ENV_SKIP_BUILD=1
#   CODEX_TEST_REMOTE_ENV_REUSE=1
#   CODEX_TEST_REMOTE_ENV_REUSE_SERVER=1

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

is_sourced() {
  [[ "${BASH_SOURCE[0]}" != "$0" ]]
}

is_truthy() {
  case "${1:-}" in
    1 | true | TRUE | yes | YES | on | ON)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

resolve_codex_binary_path() {
  if [[ -n "${CODEX_TEST_REMOTE_ENV_CODEX_BINARY:-}" ]]; then
    echo "${CODEX_TEST_REMOTE_ENV_CODEX_BINARY}"
  else
    echo "${REPO_ROOT}/codex-rs/target/debug/codex"
  fi
}

codex_binary_needs_rebuild() {
  local binary_path="$1"
  local newer_input

  [[ -f "${binary_path}" ]] || return 0

  newer_input="$(
    find "${REPO_ROOT}/codex-rs" \
      \( -path "${REPO_ROOT}/codex-rs/target" -o -path "${REPO_ROOT}/codex-rs/vendor" \) -prune -o \
      -type f \( -name '*.rs' -o -name 'Cargo.toml' -o -name 'Cargo.lock' -o -name 'build.rs' \) \
      -newer "${binary_path}" -print -quit
  )"
  [[ -n "${newer_input}" ]]
}

ensure_codex_binary() {
  local binary_path="$1"

  if [[ -n "${CODEX_TEST_REMOTE_ENV_CODEX_BINARY:-}" ]]; then
    [[ -f "${binary_path}" ]] || {
      echo "codex binary not found at ${binary_path}" >&2
      return 1
    }
    return 0
  fi

  if ! is_truthy "${CODEX_TEST_REMOTE_ENV_SKIP_BUILD:-0}"; then
    if is_truthy "${CODEX_TEST_REMOTE_ENV_FORCE_BUILD:-0}" || codex_binary_needs_rebuild "${binary_path}"; then
      if ! command -v cargo >/dev/null 2>&1; then
        echo "cargo is required to build codex" >&2
        return 1
      fi
      if ! (
        cd "${REPO_ROOT}/codex-rs"
        cargo build -p codex-cli --bin codex
      ); then
        return 1
      fi
    fi
  fi

  if [[ ! -f "${binary_path}" ]]; then
    echo "codex binary not found at ${binary_path}" >&2
    return 1
  fi
}

ensure_remote_env_image() {
  local image_name="$1"
  local default_image="codex-remote-test-env:ubuntu-24.04"
  local base_image="${CODEX_TEST_REMOTE_ENV_BASE_IMAGE:-ubuntu:24.04}"

  if docker image inspect "${image_name}" >/dev/null 2>&1; then
    return 0
  fi

  if [[ "${image_name}" != "${default_image}" ]] && ! is_truthy "${CODEX_TEST_REMOTE_ENV_BUILD_IMAGE:-0}"; then
    echo "remote env image ${image_name} does not exist; set CODEX_TEST_REMOTE_ENV_BUILD_IMAGE=1 to build it" >&2
    return 1
  fi

  docker build -t "${image_name}" - <<EOF
FROM ${base_image}
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends python3 zsh bubblewrap \
    && rm -rf /var/lib/apt/lists/*
EOF
}

start_remote_env_container() {
  local container_name="$1"
  local image_name="$2"
  local binary_path="$3"
  local remote_port="$4"
  local host_bind="${CODEX_TEST_REMOTE_ENV_HOST_BIND:-127.0.0.1}"
  local host_port="${CODEX_TEST_REMOTE_ENV_HOST_PORT:-}"
  local binary_dir
  local mount_dir="${CODEX_TEST_REMOTE_ENV_BINARY_MOUNT_DIR:-/tmp/codex-remote-env/host-bin}"

  if is_truthy "${CODEX_TEST_REMOTE_ENV_REUSE:-0}" && docker container inspect "${container_name}" >/dev/null 2>&1; then
    if [[ "$(docker inspect -f '{{.State.Running}}' "${container_name}")" != "true" ]]; then
      docker start "${container_name}" >/dev/null || return 1
    fi
    return 0
  fi

  docker rm -f "${container_name}" >/dev/null 2>&1 || true
  binary_dir="$(cd "$(dirname "${binary_path}")" && pwd)" || return 1

  if is_truthy "${CODEX_TEST_REMOTE_ENV_MOUNT_BINARY:-1}"; then
    if docker run -d \
      --name "${container_name}" \
      --privileged \
      --security-opt seccomp=unconfined \
      -p "${host_bind}:${host_port}:${remote_port}" \
      -v "${binary_dir}:${mount_dir}:ro" \
      "${image_name}" sleep infinity >/dev/null; then
      return 0
    fi
    docker rm -f "${container_name}" >/dev/null 2>&1 || true
  fi

  docker run -d \
    --name "${container_name}" \
    --privileged \
    --security-opt seccomp=unconfined \
    -p "${host_bind}:${host_port}:${remote_port}" \
    "${image_name}" sleep infinity >/dev/null
}

mounted_codex_path() {
  local container_name="$1"
  local binary_path="$2"
  local mount_dir="${CODEX_TEST_REMOTE_ENV_BINARY_MOUNT_DIR:-/tmp/codex-remote-env/host-bin}"
  local candidate="${mount_dir}/$(basename "${binary_path}")"

  if docker exec "${container_name}" test -x "${candidate}" >/dev/null 2>&1; then
    echo "${candidate}"
    return 0
  fi

  return 1
}

stage_codex_binary() {
  local container_name="$1"
  local binary_path="$2"
  local remote_codex_path="/tmp/codex-remote-env/bin/codex"
  local mounted_path

  if mounted_path="$(mounted_codex_path "${container_name}" "${binary_path}")"; then
    echo "${mounted_path}"
    return 0
  fi

  docker exec -i "${container_name}" sh -lc \
    "mkdir -p /tmp/codex-remote-env/bin && cat > ${remote_codex_path} && chmod +x ${remote_codex_path}" <"${binary_path}" || return 1
  echo "${remote_codex_path}"
}

stop_remote_exec_server() {
  local container_name="$1"
  local pid_path="/tmp/codex-remote-env/exec-server.pid"

  docker exec "${container_name}" sh -lc \
    "if [ -r ${pid_path} ]; then read pid < ${pid_path}; kill \"\$pid\" >/dev/null 2>&1 || true; rm -f ${pid_path}; fi" >/dev/null 2>&1 || true
}

start_remote_exec_server() {
  local container_name="$1"
  local remote_codex_path="$2"
  local port="$3"
  local stdout_path="$4"
  local pid_path="/tmp/codex-remote-env/exec-server.pid"

  docker exec "${container_name}" sh -lc \
    "mkdir -p /tmp/codex-remote-env; rm -f ${stdout_path} ${pid_path}; nohup ${remote_codex_path} exec-server --listen ws://0.0.0.0:${port} > ${stdout_path} 2>&1 & pid=\$!; echo \$pid > ${pid_path}; echo \$pid"
}

remote_exec_server_pid() {
  local container_name="$1"
  local pid_path="/tmp/codex-remote-env/exec-server.pid"

  docker exec "${container_name}" sh -lc \
    "if [ -r ${pid_path} ]; then read pid < ${pid_path}; echo \$pid; fi" 2>/dev/null || true
}

remote_exec_server_url() {
  local container_name="$1"
  local port="$2"
  local published_port
  local host="${CODEX_TEST_REMOTE_ENV_HOST:-127.0.0.1}"
  local container_ip

  published_port="$(docker port "${container_name}" "${port}/tcp" 2>/dev/null | awk -F: 'NR == 1 {print $NF}')"
  if [[ -n "${published_port}" ]]; then
    echo "ws://${host}:${published_port}"
    return 0
  fi

  container_ip="$(
    docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "${container_name}"
  )"
  if [[ -z "${container_ip}" ]]; then
    echo "container ${container_name} has no IP address" >&2
    return 1
  fi
  echo "ws://${container_ip}:${port}"
}

setup_remote_env() {
  local container_name
  local codex_binary_path
  local image_name
  local remote_codex_path
  local remote_exec_server_pid
  local remote_exec_server_port
  local remote_exec_server_stdout_path

  if is_truthy "${CODEX_TEST_REMOTE_ENV_REUSE:-0}"; then
    container_name="${CODEX_TEST_REMOTE_ENV_CONTAINER_NAME:-codex-remote-test-env-local}"
  else
    container_name="${CODEX_TEST_REMOTE_ENV_CONTAINER_NAME:-codex-remote-test-env-local-$(date +%s)-${RANDOM}}"
  fi
  codex_binary_path="$(resolve_codex_binary_path)"
  image_name="${CODEX_TEST_REMOTE_ENV_IMAGE:-codex-remote-test-env:ubuntu-24.04}"
  remote_exec_server_port="${CODEX_TEST_REMOTE_ENV_REMOTE_PORT:-31987}"

  if ! command -v docker >/dev/null 2>&1; then
    echo "docker is required (Colima or Docker Desktop)" >&2
    return 1
  fi

  if ! docker info >/dev/null 2>&1; then
    echo "docker daemon is not reachable; for Colima run: colima start" >&2
    return 1
  fi

  ensure_codex_binary "${codex_binary_path}" || return 1
  ensure_remote_env_image "${image_name}" || return 1

  # bubblewrap needs mount propagation inside the remote test container.
  if ! start_remote_env_container "${container_name}" "${image_name}" "${codex_binary_path}" "${remote_exec_server_port}"; then
    docker rm -f "${container_name}" >/dev/null 2>&1 || true
    return 1
  fi

  if [[ -z "${CODEX_TEST_REMOTE_EXEC_SERVER_URL:-}" ]]; then
    remote_exec_server_stdout_path="/tmp/codex-remote-env/exec-server.stdout"
    if ! is_truthy "${CODEX_TEST_REMOTE_ENV_REUSE_SERVER:-0}" || ! wait_for_remote_exec_server_port "${container_name}" "${remote_exec_server_port}" "${remote_exec_server_stdout_path}" 1; then
      stop_remote_exec_server "${container_name}"
      remote_codex_path="$(stage_codex_binary "${container_name}" "${codex_binary_path}")" || return 1
      remote_exec_server_pid="$(start_remote_exec_server "${container_name}" "${remote_codex_path}" "${remote_exec_server_port}" "${remote_exec_server_stdout_path}")" || return 1
      wait_for_remote_exec_server_port "${container_name}" "${remote_exec_server_port}" "${remote_exec_server_stdout_path}" || return 1
    else
      remote_exec_server_pid="$(remote_exec_server_pid "${container_name}")"
    fi
    export CODEX_TEST_REMOTE_EXEC_SERVER_PID="${remote_exec_server_pid}"
    CODEX_TEST_REMOTE_EXEC_SERVER_URL="$(remote_exec_server_url "${container_name}" "${remote_exec_server_port}")" || return 1
    export CODEX_TEST_REMOTE_EXEC_SERVER_URL
  fi

  export CODEX_TEST_REMOTE_ENV="${container_name}"
}

wait_for_remote_exec_server_port() {
  local container_name="$1"
  local port="$2"
  local stdout_path="$3"
  local quiet="${4:-0}"

  if docker exec "${container_name}" python3 - "${port}" >/dev/null 2>&1 <<'PY'; then
import socket
import sys
import time

port = int(sys.argv[1])
deadline = time.monotonic() + 5
delay = 0.0
while time.monotonic() < deadline:
    if delay:
        time.sleep(delay)
    try:
        socket.create_connection(("127.0.0.1", port), timeout=0.2).close()
        raise SystemExit(0)
    except OSError:
        delay = 0.025 if delay == 0.0 else min(delay * 2, 0.2)
raise SystemExit(1)
PY
    return 0
  fi

  if ! is_truthy "${quiet}"; then
    echo "timed out waiting for remote exec-server on ${container_name}:${port}" >&2
    docker exec "${container_name}" sh -lc "cat ${stdout_path} 2>/dev/null || true" >&2 || true
  fi
  return 1
}

codex_remote_env_cleanup() {
  if [[ -n "${CODEX_TEST_REMOTE_ENV:-}" ]]; then
    if ! is_truthy "${CODEX_TEST_REMOTE_ENV_REUSE_SERVER:-0}"; then
      stop_remote_exec_server "${CODEX_TEST_REMOTE_ENV}"
    fi
    if ! is_truthy "${CODEX_TEST_REMOTE_ENV_REUSE:-0}"; then
      docker rm -f "${CODEX_TEST_REMOTE_ENV}" >/dev/null 2>&1 || true
    fi
    unset CODEX_TEST_REMOTE_ENV
  fi
  unset CODEX_TEST_REMOTE_EXEC_SERVER_PID
  unset CODEX_TEST_REMOTE_EXEC_SERVER_URL
}

if ! is_sourced; then
  echo "source this script instead of executing it: source scripts/test-remote-env.sh" >&2
  exit 1
fi

old_shell_options="$(set +o)"
set -euo pipefail
if setup_remote_env; then
  status=0
  echo "CODEX_TEST_REMOTE_ENV=${CODEX_TEST_REMOTE_ENV}"
  echo "CODEX_TEST_REMOTE_EXEC_SERVER_URL=${CODEX_TEST_REMOTE_EXEC_SERVER_URL}"
  echo "Remote env ready. Run your command, then call: codex_remote_env_cleanup"
else
  status=$?
fi
eval "${old_shell_options}"
return "${status}"
