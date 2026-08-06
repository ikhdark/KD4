#!/usr/bin/env bash
set -euo pipefail

readonly -a ALL_PARAMETERS=(
  DryRun SkipBuild AutoSkipBuild NoSccache SkipPreflightCheck BuildOnly TestRun
  Profile PrintBuiltCodexPath RepoRoot SourceExe SourceCodeModeHostExe
  SourceWindowsSandboxSetupExe SourceCommandRunnerExe InstallDir BackupDir
  RunDoctor FastProof DoctorOnNoop FailOnStaleSourceBuild RuntimeProof
  AllowRustyV8Download RustyV8Archive ConfigureDesktopLocalCli RestartDesktop
  DesktopCliEnvironmentTarget LocalCodexHome LocalCodexSqliteHome
  AllowRunningTarget CloseRunningTargetTimeoutSeconds
)
readonly -a PATH_PARAMETERS=(
  RepoRoot SourceExe SourceCodeModeHostExe SourceWindowsSandboxSetupExe
  SourceCommandRunnerExe InstallDir BackupDir RustyV8Archive LocalCodexHome
  LocalCodexSqliteHome
)

main() {
  if ! command -v powershell.exe >/dev/null 2>&1; then
    echo "powershell.exe is required. Run Windows local publish recipes from Windows PowerShell instead." >&2
    return 2
  fi
  if ! command -v wslpath >/dev/null 2>&1; then
    echo "wslpath is required to translate this checkout path for Windows PowerShell." >&2
    return 2
  fi

  local script_dir repo_root windows_repo_root windows_script
  script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
  repo_root="$(cd -- "$script_dir/.." && pwd)"
  windows_repo_root="$(wslpath -w "$repo_root")"
  windows_script="$(wslpath -w "$repo_root/scripts/publish-local-codex.ps1")"

  local -a translated_args
  translate_args translated_args "$@"

  if [[ -n "${CODEX_LOCAL_PUBLISH_DIR:-}" ]]; then
    if [[ "$CODEX_LOCAL_PUBLISH_DIR" == /* ]]; then
      CODEX_LOCAL_PUBLISH_DIR="$(wslpath -w "$CODEX_LOCAL_PUBLISH_DIR")"
    fi
    export CODEX_LOCAL_PUBLISH_DIR
    export_wslenv_publish_dir
  fi

  if has_repo_root_arg "${translated_args[@]}"; then
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$windows_script" "${translated_args[@]}"
  else
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$windows_script" -RepoRoot "$windows_repo_root" "${translated_args[@]}"
  fi
}

has_repo_root_arg() {
  local arg parameter
  for arg in "$@"; do
    parameter="$(resolve_parameter_name "${arg%%[=:]*}")"
    if [[ "${parameter,,}" == "reporoot" ]]; then
      return 0
    fi
  done
  return 1
}

translate_args() {
  local -n out_ref="$1"
  shift
  out_ref=()
  local expect_path=0
  local arg arg_name parameter delimiter value
  for arg in "$@"; do
    if [[ "$expect_path" -eq 1 ]]; then
      out_ref+=("$(translate_path_arg "$arg")")
      expect_path=0
      continue
    fi
    delimiter=""
    if [[ "$arg" == -*=* ]]; then
      delimiter="="
    elif [[ "$arg" == -*:* ]]; then
      delimiter=":"
    fi
    parameter="$(resolve_parameter_name "${arg%%[=:]*}")"
    if is_path_parameter "$parameter"; then
      if [[ -z "$delimiter" ]]; then
        arg_name="${arg#-}"
        if [[ "${arg_name,,}" == "${parameter,,}" ]]; then
          out_ref+=("$arg")
        else
          out_ref+=("-$parameter")
        fi
        expect_path=1
      else
        value="${arg#*"$delimiter"}"
        out_ref+=("-$parameter$delimiter$(translate_path_arg "$value")")
      fi
      continue
    fi
    out_ref+=("$arg")
  done
}

resolve_parameter_name() {
  local raw="${1#-}"
  local raw_lower="${raw,,}"
  local candidate
  local -a matches=()
  for candidate in "${ALL_PARAMETERS[@]}"; do
    if [[ "${candidate,,}" == "$raw_lower"* ]]; then
      matches+=("$candidate")
    fi
  done
  if [[ "${#matches[@]}" -eq 1 ]]; then
    printf '%s\n' "${matches[0]}"
  fi
}

is_path_parameter() {
  local parameter="$1"
  local candidate
  for candidate in "${PATH_PARAMETERS[@]}"; do
    if [[ "$candidate" == "$parameter" ]]; then
      return 0
    fi
  done
  return 1
}

export_wslenv_publish_dir() {
  local entry
  local -a kept=()
  local -a existing=()
  IFS=: read -r -a existing <<< "${WSLENV:-}"
  for entry in "${existing[@]}"; do
    if [[ -n "$entry" && "${entry%%/*}" != "CODEX_LOCAL_PUBLISH_DIR" ]]; then
      kept+=("$entry")
    fi
  done
  if [[ "${#kept[@]}" -eq 0 ]]; then
    export WSLENV="CODEX_LOCAL_PUBLISH_DIR"
  else
    export WSLENV="CODEX_LOCAL_PUBLISH_DIR:$(IFS=:; printf '%s' "${kept[*]}")"
  fi
}

translate_path_arg() {
  local value="$1"
  if [[ "$value" == /* ]]; then
    wslpath -w "$value"
  else
    printf '%s\n' "$value"
  fi
}

main "$@"
