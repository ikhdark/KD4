#!/usr/bin/env bash

set -euo pipefail

usage() {
  echo "Usage: $0 HOST [RSYNC_OPTION]..." >&2
}

if [[ $# -lt 1 ]]; then
  usage
  exit 2
fi

case "$1" in
  -h|--help)
    usage
    exit 0
    ;;
esac

remote_host="$1"
shift

remote_path='~/code/codex-sync'
local_exec_server_port="${CODEX_REMOTE_EXEC_SERVER_LOCAL_PORT:-8765}"
remote_exec_server_start_timeout_seconds="${CODEX_REMOTE_EXEC_SERVER_START_TIMEOUT_SECONDS:-15}"
remote_exec_server_build_cmd="${CODEX_REMOTE_EXEC_SERVER_BUILD_CMD:-cargo build -p codex-cli --bin codex}"
remote_exec_server_binary="${CODEX_REMOTE_EXEC_SERVER_BINARY:-}"
remote_exec_server_skip_fresh_build="${CODEX_REMOTE_EXEC_SERVER_SKIP_FRESH_BUILD:-1}"
rsync_compress="${CODEX_REMOTE_EXEC_SERVER_RSYNC_COMPRESS:-auto}"

remote_exec_server_pid=''
remote_exec_server_log_path=''
remote_exec_server_pid_path=''
remote_exec_server_ready_path=''
remote_repo_root=''
sync_instance_id="$(date +%s)-$$"
ssh_control_dir="${TMPDIR:-/tmp}/codex-exec-ssh-${sync_instance_id}"
ssh_control_path="${ssh_control_dir}/control"
ssh_opts=(
  -o ControlMaster=auto
  -o ControlPersist=60
  -o "ControlPath=${ssh_control_path}"
)
rsync_ssh_cmd="ssh -o ControlMaster=auto -o ControlPersist=60 -o ControlPath=${ssh_control_path}"

now_seconds() {
  date +%s
}

log_duration() {
  local label="$1"
  local started_at="$2"
  printf '%s: %ss\n' "${label}" "$(( $(now_seconds) - started_at ))" >&2
}

cleanup() {
  local exit_code=$?

  trap - EXIT INT TERM

  if [[ -n "${remote_exec_server_pid_path}" ]]; then
    ssh "${ssh_opts[@]}" "${remote_host}" bash -s -- \
      "${remote_exec_server_pid_path}" \
      "${remote_exec_server_log_path}" \
      "${remote_exec_server_ready_path}" >/dev/null 2>&1 <<'EOF' || true
set -euo pipefail
pid_path="$1"
log_path="$2"
ready_path="$3"
if [[ -r "${pid_path}" ]]; then
  read -r pid <"${pid_path}" || pid=''
  if [[ -n "${pid}" ]]; then
    kill "${pid}" >/dev/null 2>&1 || true
  fi
fi
rm -f "${pid_path}" "${log_path}" "${ready_path}"
EOF
  fi

  ssh "${ssh_opts[@]}" -O exit "${remote_host}" >/dev/null 2>&1 || true
  rm -rf "${ssh_control_dir}" >/dev/null 2>&1 || true

  exit "${exit_code}"
}

trap cleanup EXIT INT TERM

if ! command -v git >/dev/null 2>&1; then
  echo "git is required" >&2
  exit 1
fi

if ! command -v ssh >/dev/null 2>&1; then
  echo "ssh is required" >&2
  exit 1
fi

if ! command -v rsync >/dev/null 2>&1; then
  echo "local rsync is required" >&2
  exit 1
fi

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  echo "run this script from inside a git repository" >&2
  exit 1
}

mkdir -p "${ssh_control_dir}"

setup_started_at="$(now_seconds)"
ssh "${ssh_opts[@]}" "${remote_host}" bash -s -- "${remote_path}" <<'EOF'
  set -e
  remote_path="$1"
  case "${remote_path}" in
    '~')
      remote_path="${HOME}"
      ;;
    '~/'*)
      remote_path="${HOME}/${remote_path#~/}"
      ;;
  esac
  mkdir -p "${remote_path}"
  if ! command -v rsync >/dev/null 2>&1 ||
     ! dpkg-query -W -f='${Status}' libcap-dev 2>/dev/null | grep -q 'install ok installed'; then
    sudo apt-get install -y rsync libcap-dev
  fi
EOF
log_duration "remote setup" "${setup_started_at}"

rsync_opts=(
  --archive
  --human-readable
  --itemize-changes
  --delete-delay
  --delete-excluded
  --exclude '.git/'
  --exclude 'codex-rs/target/'
  --exclude 'target/'
  --exclude 'node_modules/'
  --exclude '.next/'
  --exclude '.cache/'
  --exclude '.turbo/'
  --exclude 'coverage/'
  --exclude 'dist/'
  --exclude '*.log'
  --filter=':- .gitignore'
)

case "${rsync_compress}" in
  1|true|TRUE|yes|YES|on|ON)
    rsync_opts+=(--compress)
    ;;
  0|false|FALSE|no|NO|off|OFF)
    ;;
  auto)
    case "${remote_host}" in
      localhost|localhost:*|127.*|127.*:*|::1|\[::1\]*)
        ;;
      *)
        rsync_opts+=(--compress)
        ;;
    esac
    ;;
  *)
    echo "CODEX_REMOTE_EXEC_SERVER_RSYNC_COMPRESS must be auto, true, or false" >&2
    exit 2
    ;;
esac

rsync_started_at="$(now_seconds)"
rsync \
  -e "${rsync_ssh_cmd}" \
  "${rsync_opts[@]}" \
  "$@" \
  "${repo_root}/" \
  "${remote_host}:${remote_path}/" \
  >&2
log_duration "rsync" "${rsync_started_at}"

remote_exec_server_log_path="/tmp/codex-exec-server-${sync_instance_id}.log"
remote_exec_server_pid_path="/tmp/codex-exec-server-${sync_instance_id}.pid"
remote_exec_server_ready_path="/tmp/codex-exec-server-${sync_instance_id}.ready"

remote_start_started_at="$(now_seconds)"
remote_start_output="$(
  ssh "${ssh_opts[@]}" "${remote_host}" bash -s -- \
    "${remote_exec_server_log_path}" \
    "${remote_exec_server_pid_path}" \
    "${remote_exec_server_ready_path}" \
    "${remote_exec_server_start_timeout_seconds}" \
    "${remote_path}" \
    "${remote_exec_server_build_cmd}" \
    "${remote_exec_server_binary}" \
    "${remote_exec_server_skip_fresh_build}" <<'EOF'
set -euo pipefail

remote_exec_server_log_path="$1"
remote_exec_server_pid_path="$2"
remote_exec_server_ready_path="$3"
remote_exec_server_start_timeout_seconds="$4"
remote_repo_root="$5"
remote_exec_server_build_cmd="$6"
remote_exec_server_binary="$7"
remote_exec_server_skip_fresh_build="$8"

case "${remote_repo_root}" in
  '~')
    remote_repo_root="${HOME}"
    ;;
  '~/'*)
    remote_repo_root="${HOME}/${remote_repo_root#~/}"
    ;;
esac

remote_codex_rs="$remote_repo_root/codex-rs"
remote_codex_bin="${remote_exec_server_binary:-${remote_codex_rs}/target/debug/codex}"

cd "${remote_codex_rs}"

build_started_at="${SECONDS}"
if [[ -n "${remote_exec_server_binary}" ]]; then
  if [[ ! -x "${remote_codex_bin}" ]]; then
    echo "CODEX_REMOTE_EXEC_SERVER_BINARY is not executable: ${remote_codex_bin}" >&2
    exit 1
  fi
  printf 'remote build: skipped, using CODEX_REMOTE_EXEC_SERVER_BINARY (%ss)\n' \
    "$((SECONDS - build_started_at))" >&2
elif [[ "${remote_exec_server_skip_fresh_build}" == "1" && -x "${remote_codex_bin}" ]] &&
  ! find . -path './target' -prune -o -type f -newer "${remote_codex_bin}" -print -quit | grep -q .; then
  printf 'remote build: skipped, existing binary is fresh (%ss)\n' \
    "$((SECONDS - build_started_at))" >&2
else
  bash -lc "${remote_exec_server_build_cmd}"
  printf 'remote build: completed (%ss)\n' "$((SECONDS - build_started_at))" >&2
fi

if [[ ! -x "${remote_codex_bin}" ]]; then
  echo "remote codex binary is not executable: ${remote_codex_bin}" >&2
  exit 1
fi

rm -f "${remote_exec_server_log_path}" "${remote_exec_server_pid_path}" "${remote_exec_server_ready_path}"
mkfifo "${remote_exec_server_ready_path}"
: >"${remote_exec_server_log_path}"

server_started_at="${SECONDS}"
nohup "${remote_codex_bin}" exec-server --listen ws://127.0.0.1:0 > >(
  first_line_written=0
  while IFS= read -r line; do
    printf '%s\n' "${line}" >>"${remote_exec_server_log_path}"
    if (( first_line_written == 0 )); then
      printf '%s\n' "${line}" >"${remote_exec_server_ready_path}"
      first_line_written=1
    fi
  done
) 2>&1 &
remote_exec_server_pid="$!"
echo "${remote_exec_server_pid}" >"${remote_exec_server_pid_path}"

exec 3<>"${remote_exec_server_ready_path}"
if IFS= read -r -t "${remote_exec_server_start_timeout_seconds}" listen_url <&3; then
  if [[ "${listen_url}" == ws://* ]]; then
    printf 'remote exec-server readiness: %ss\n' "$((SECONDS - server_started_at))" >&2
    printf 'remote_exec_server_pid=%s\n' "${remote_exec_server_pid}"
    printf 'remote_exec_server_log_path=%s\n' "${remote_exec_server_log_path}"
    printf 'remote_repo_root=%s\n' "${remote_repo_root}"
    printf 'listen_url=%s\n' "${listen_url}"
    exit 0
  fi
  cat "${remote_exec_server_log_path}" >&2 || true
  echo "remote exec server reported an invalid listen URL: ${listen_url}" >&2
  exit 1
fi

if ! kill -0 "${remote_exec_server_pid}" >/dev/null 2>&1; then
  cat "${remote_exec_server_log_path}" >&2 || true
  echo "remote exec server exited before reporting a listen URL" >&2
  exit 1
fi

cat "${remote_exec_server_log_path}" >&2 || true
echo "timed out waiting for remote exec server listen URL" >&2
exit 1
EOF
)"
log_duration "remote exec-server startup" "${remote_start_started_at}"

listen_url=''
while IFS='=' read -r key value; do
  case "${key}" in
    remote_exec_server_pid)
      remote_exec_server_pid="${value}"
      ;;
    remote_exec_server_log_path)
      remote_exec_server_log_path="${value}"
      ;;
    remote_repo_root)
      remote_repo_root="${value}"
      ;;
    listen_url)
      listen_url="${value}"
      ;;
  esac
done <<< "${remote_start_output}"

if [[ -z "${remote_exec_server_pid}" || -z "${listen_url}" || -z "${remote_repo_root}" ]]; then
  echo "failed to parse remote exec server startup output" >&2
  exit 1
fi

remote_exec_server_port="${listen_url##*:}"
if [[ -z "${remote_exec_server_port}" || "${remote_exec_server_port}" == "${listen_url}" ]]; then
  echo "failed to parse remote exec server port from ${listen_url}" >&2
  exit 1
fi

echo "Remote exec server: ${listen_url}"
echo "Remote exec server log: ${remote_exec_server_log_path}"
echo "Press Ctrl-C to stop the SSH tunnel and remote exec server."
echo "Start codex via: "
printf '  CODEX_EXEC_SERVER_URL=ws://127.0.0.1:%s codex -C %q\n' \
  "${local_exec_server_port}" \
  "${remote_repo_root}"

tunnel_started_at="$(now_seconds)"
set +e
ssh \
  "${ssh_opts[@]}" \
  -nNT \
  -o ExitOnForwardFailure=yes \
  -o ServerAliveInterval=30 \
  -o ServerAliveCountMax=3 \
  -L "${local_exec_server_port}:127.0.0.1:${remote_exec_server_port}" \
  "${remote_host}"
tunnel_exit_code=$?
set -e
log_duration "ssh tunnel lifetime" "${tunnel_started_at}"
exit "${tunnel_exit_code}"
