#!/usr/bin/env bash
set -euo pipefail

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

  if [[ -n "${CODEX_LOCAL_PUBLISH_DIR:-}" && "$CODEX_LOCAL_PUBLISH_DIR" == /* ]]; then
    export CODEX_LOCAL_PUBLISH_DIR
    CODEX_LOCAL_PUBLISH_DIR="$(wslpath -w "$CODEX_LOCAL_PUBLISH_DIR")"
  fi

  if has_repo_root_arg "${translated_args[@]}"; then
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$windows_script" "${translated_args[@]}"
  else
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$windows_script" -RepoRoot "$windows_repo_root" "${translated_args[@]}"
  fi
}

has_repo_root_arg() {
  local arg arg_lower
  for arg in "$@"; do
    arg_lower="${arg,,}"
    if [[ "$arg_lower" == "-reporoot" || "$arg_lower" == "-reporoot="* ]]; then
      return 0
    fi
  done
  return 1
}

translate_args() {
  local -n out_ref="$1"
  shift
  out_ref=()
  local path_flags=(
    -RepoRoot
    -SourceExe
    -SourceCodeModeHostExe
    -InstallDir
    -BackupDir
    -RustyV8Archive
    -LocalCodexHome
    -LocalCodexSqliteHome
  )
  local expect_path=0
  local arg arg_lower flag flag_lower value
  for arg in "$@"; do
    if [[ "$expect_path" -eq 1 ]]; then
      out_ref+=("$(translate_path_arg "$arg")")
      expect_path=0
      continue
    fi
    arg_lower="${arg,,}"
    for flag in "${path_flags[@]}"; do
      flag_lower="${flag,,}"
      if [[ "$arg_lower" == "$flag_lower" ]]; then
        out_ref+=("$arg")
        expect_path=1
        continue 2
      fi
      if [[ "$arg_lower" == "$flag_lower="* ]]; then
        value="${arg#*=}"
        out_ref+=("$flag=$(translate_path_arg "$value")")
        continue 2
      fi
    done
    out_ref+=("$arg")
  done
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
