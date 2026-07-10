#!/bin/sh

set -eu

RELEASE="${CODEX_RELEASE:-latest}"
NON_INTERACTIVE="${CODEX_NON_INTERACTIVE:-false}"

BIN_DIR="${CODEX_INSTALL_DIR:-$HOME/.local/bin}"
BIN_PATH="$BIN_DIR/codex"
CODEX_HOME_DIR="${CODEX_HOME:-$HOME/.codex}"
STANDALONE_ROOT="$CODEX_HOME_DIR/packages/standalone"
RELEASES_DIR="$STANDALONE_ROOT/releases"
CURRENT_LINK="$STANDALONE_ROOT/current"
LOCK_FILE="$STANDALONE_ROOT/install.lock"
LOCK_DIR="$STANDALONE_ROOT/install.lock.d"
LOCK_STALE_AFTER_SECS=600
INSTALL_METADATA_FILE="codex-install.env"

path_action="already"
path_profile=""
conflict_manager=""
conflict_path=""
lock_kind=""
tmp_dir=""
download_cmd=""
sha256_cmd=""
release_json_cache=""
release_json_version=""
resolved_version_result=""
visible_command_preverified="false"

step() {
  printf '==> %s\n' "$1"
}

warn() {
  printf 'WARNING: %s\n' "$1" >&2
}

normalize_version() {
  case "$1" in
    "" | latest)
      printf 'latest\n'
      ;;
    rust-v*)
      printf '%s\n' "${1#rust-v}"
      ;;
    v*)
      printf '%s\n' "${1#v}"
      ;;
    *)
      printf '%s\n' "$1"
      ;;
  esac
}

validate_version() {
  version="$1"

  if [ "$version" = "latest" ]; then
    return
  fi

  if ! printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-(alpha|beta)(\.[0-9]+)?)?$'; then
    echo "Invalid Codex release version: $version. Expected latest or x.y.z[-alpha[.N]|-beta[.N]]." >&2
    exit 1
  fi
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --release)
        if [ "$#" -lt 2 ]; then
          echo "--release requires a value." >&2
          exit 1
        fi
        RELEASE="$2"
        shift
        ;;
      --help | -h)
        cat <<EOF
Usage: install.sh [--release VERSION]

Environment:
  CODEX_RELEASE          Version to install; overridden by --release.
  CODEX_NON_INTERACTIVE  Set to 1, true, or yes to skip prompts.
EOF
        exit 0
        ;;
      *)
        echo "Unknown argument: $1" >&2
        exit 1
        ;;
    esac
    shift
  done
}

download_file() {
  url="$1"
  output="$2"

  ensure_downloader
  case "$download_cmd" in
    curl)
      curl -fsSL "$url" -o "$output"
      ;;
    wget)
      wget -q -O "$output" "$url"
      ;;
  esac
}

download_text() {
  url="$1"

  ensure_downloader
  case "$download_cmd" in
    curl)
      curl -fsSL "$url"
      ;;
    wget)
      wget -q -O - "$url"
      ;;
  esac
}

ensure_downloader() {
  if [ -n "$download_cmd" ]; then
    return
  fi

  if command -v curl >/dev/null 2>&1; then
    download_cmd="curl"
    return
  fi

  if command -v wget >/dev/null 2>&1; then
    download_cmd="wget"
    return
  fi

  echo "curl or wget is required to install Codex." >&2
  exit 1
}

# BEGIN INSTALL RELEASE HELPERS
install_helper_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
. "$install_helper_dir/install_release.sh"
# END INSTALL RELEASE HELPERS

wait_for_download() {
  pid="$1"
  asset="$2"

  if ! wait "$pid"; then
    echo "Failed to download $asset." >&2
    exit 1
  fi
}

require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "$1 is required to install Codex." >&2
    exit 1
  fi
}

resolve_version() {
  normalized_version="$(normalize_version "$RELEASE")"
  validate_version "$normalized_version"

  if [ "$normalized_version" != "latest" ]; then
    resolved_version_result="$normalized_version"
    return
  fi

  release_json="$(download_text "https://api.github.com/repos/openai/codex/releases/latest")"
  resolved="$(printf '%s\n' "$release_json" | sed -n 's/.*"tag_name":[[:space:]]*"rust-v\([^"]*\)".*/\1/p' | head -n 1)"

  if [ -z "$resolved" ]; then
    echo "Failed to resolve the latest Codex release version." >&2
    exit 1
  fi

  validate_version "$resolved"
  release_json_cache="$release_json"
  release_json_version="$resolved"
  resolved_version_result="$resolved"
}

pick_profile() {
  # Use the same shell-specific split Homebrew documents because there is no
  # universal startup file across macOS/Linux login and interactive shells.
  case "$os:${SHELL:-}" in
    darwin:*/zsh)
      printf '%s\n' "$HOME/.zprofile"
      ;;
    darwin:*/bash)
      printf '%s\n' "$HOME/.bash_profile"
      ;;
    linux:*/zsh)
      printf '%s\n' "$HOME/.zshrc"
      ;;
    linux:*/bash)
      printf '%s\n' "$HOME/.bashrc"
      ;;
    *)
      printf '%s\n' "$HOME/.profile"
      ;;
  esac
}

add_to_path() {
  path_action="already"
  path_profile=""

  case ":$PATH:" in
    *":$BIN_DIR:"*)
      if [ -z "$conflict_manager" ]; then
        return
      fi
      ;;
  esac

  profile="$(pick_profile)"
  path_profile="$profile"
  begin_marker="# >>> Codex installer >>>"
  end_marker="# <<< Codex installer <<<"
  path_line="export PATH=\"$BIN_DIR:\$PATH\""

  if [ -f "$profile" ] && grep -F "$begin_marker" "$profile" >/dev/null 2>&1; then
    if grep -F "$path_line" "$profile" >/dev/null 2>&1; then
      path_action="configured"
      return
    fi

    if profile_has_equivalent_path_line "$profile" "$BIN_DIR"; then
      path_action="configured"
      return
    fi

    if grep -F "$end_marker" "$profile" >/dev/null 2>&1; then
      rewrite_path_block "$profile" "$begin_marker" "$end_marker" "$path_line"
      path_action="updated"
      return
    fi
  fi

  append_path_block "$profile" "$begin_marker" "$end_marker" "$path_line"
  path_action="added"
}

profile_has_equivalent_path_line() {
  profile="$1"
  expected_dir="$2"

  awk -v expected="$expected_dir" '
    function normalize(path) {
      while (length(path) > 1 && substr(path, length(path), 1) == "/") {
        path = substr(path, 1, length(path) - 1)
      }
      return path
    }
    /^export PATH="/ {
      line = $0
      sub(/^export PATH="/, "", line)
      sub(/:\$PATH"$/, "", line)
      if (normalize(line) == normalize(expected)) {
        found = 1
      }
    }
    END {
      if (found) {
        exit 0
      }
      exit 1
    }
  ' "$profile"
}

append_path_block() {
  profile="$1"
  begin_marker="$2"
  end_marker="$3"
  path_line="$4"

  {
    printf '\n%s\n' "$begin_marker"
    printf '%s\n' "$path_line"
    printf '%s\n' "$end_marker"
  } >>"$profile"
}

rewrite_path_block() {
  profile="$1"
  begin_marker="$2"
  end_marker="$3"
  path_line="$4"
  tmp_profile="$tmp_dir/profile.$$.tmp"

  awk -v begin="$begin_marker" -v end="$end_marker" -v line="$path_line" '
    BEGIN {
      in_block = 0
      replaced = 0
    }
    $0 == begin {
      if (!replaced) {
        print begin
        print line
        print end
        replaced = 1
      }
      in_block = 1
      next
    }
    in_block {
      if ($0 == end) {
        in_block = 0
      }
      next
    }
    {
      print
    }
    END {
      if (in_block != 0) {
        exit 1
      }
    }
  ' "$profile" >"$tmp_profile"
  mv "$tmp_profile" "$profile"
}

mkdir_lock_is_stale() {
  [ -d "$LOCK_DIR" ] || return 1

  pid="$(cat "$LOCK_DIR/pid" 2>/dev/null || true)"
  started_at="$(cat "$LOCK_DIR/started_at" 2>/dev/null || true)"
  now="$(date +%s 2>/dev/null || printf '0')"

  case "$started_at" in
    ''|*[!0-9]*)
      started_at=0
      ;;
  esac

  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    return 1
  fi

  if [ "$started_at" -eq 0 ] || [ "$now" -eq 0 ]; then
    return 0
  fi

  [ $((now - started_at)) -ge "$LOCK_STALE_AFTER_SECS" ]
}

acquire_install_lock() {
  mkdir -p "$STANDALONE_ROOT"

  if [ "$os" = "darwin" ] && command -v lockf >/dev/null 2>&1; then
    : >>"$LOCK_FILE"
    exec 9<>"$LOCK_FILE"
    lockf 9
    lock_kind="lockf"
    return
  fi

  if command -v flock >/dev/null 2>&1; then
    exec 9>"$LOCK_FILE"
    flock 9
    lock_kind="flock"
    return
  fi

  while ! mkdir "$LOCK_DIR" 2>/dev/null; do
    if mkdir_lock_is_stale; then
      warn "Removing stale installer lock at $LOCK_DIR"
      rm -rf "$LOCK_DIR"
      continue
    fi
    sleep 1
  done

  printf '%s\n' "$$" >"$LOCK_DIR/pid"
  date +%s >"$LOCK_DIR/started_at" 2>/dev/null || true
  lock_kind="mkdir"
}

release_install_lock() {
  if [ "$lock_kind" = "mkdir" ]; then
    rm -rf "$LOCK_DIR" 2>/dev/null || true
  elif [ "$lock_kind" = "flock" ] || [ "$lock_kind" = "lockf" ]; then
    exec 9>&- 2>/dev/null || true
  fi
  lock_kind=""
}

cleanup_stale_install_artifacts() {
  mkdir -p "$RELEASES_DIR" "$STANDALONE_ROOT"

  remove_matching_children "$RELEASES_DIR" '.staging.*' directory
  remove_matching_children "$STANDALONE_ROOT" '.current.*' file

  if [ -d "$BIN_DIR" ]; then
    remove_matching_children "$BIN_DIR" '.codex.*' file
  fi
}

remove_matching_children() {
  directory="$1"
  pattern="$2"
  kind="$3"

  [ -d "$directory" ] || return 0
  found="false"
  for path in "$directory"/$pattern; do
    if [ -e "$path" ] || [ -L "$path" ]; then
      found="true"
      break
    fi
  done
  [ "$found" = "true" ] || return 0

  case "$kind" in
    directory)
      find "$directory" -mindepth 1 -maxdepth 1 -name "$pattern" -exec rm -rf {} +
      ;;
    *)
      find "$directory" -mindepth 1 -maxdepth 1 -name "$pattern" -exec rm -f {} +
      ;;
  esac
}

replace_path_with_symlink() {
  link_path="$1"
  link_target="$2"
  tmp_link="$3"

  rm -f "$tmp_link"
  ln -s "$link_target" "$tmp_link"

  if mv -Tf "$tmp_link" "$link_path" 2>/dev/null; then
    return
  fi

  if mv -hf "$tmp_link" "$link_path" 2>/dev/null; then
    return
  fi

  rm -f "$link_path"
  mv -f "$tmp_link" "$link_path"
}

version_from_binary() {
  codex_path="$1"

  if [ ! -x "$codex_path" ]; then
    return 1
  fi

  "$codex_path" --version 2>/dev/null | sed -n 's/.* \([0-9][0-9A-Za-z.+-]*\)$/\1/p' | head -n 1
}

current_installed_version() {
  version="$(install_metadata_field "$CURRENT_LINK" version || true)"
  if [ -n "$version" ]; then
    printf '%s\n' "$version"
    return 0
  fi

  version="$(version_from_binary "$CURRENT_LINK/bin/codex" || true)"
  if [ -n "$version" ]; then
    printf '%s\n' "$version"
    return 0
  fi

  version="$(version_from_binary "$CURRENT_LINK/codex" || true)"
  if [ -n "$version" ]; then
    printf '%s\n' "$version"
    return 0
  fi

  return 0
}

install_metadata_field() {
  release_dir="$1"
  field="$2"
  metadata_path="$release_dir/$INSTALL_METADATA_FILE"

  [ -f "$metadata_path" ] || return 1
  awk -F= -v field="$field" '
    $1 == field {
      print substr($0, length(field) + 2)
      found = 1
      exit
    }
    END {
      if (found) {
        exit 0
      }
      exit 1
    }
  ' "$metadata_path"
}

resolve_existing_codex() {
  command -v codex 2>/dev/null || true
}

classify_existing_codex() {
  existing_path="$1"

  if [ -z "$existing_path" ] || [ "$existing_path" = "$BIN_PATH" ]; then
    return 1
  fi

  case "$existing_path" in
    /opt/homebrew/* | /usr/local/*)
      if [ "$os" = "darwin" ]; then
        printf 'brew\n'
        return 0
      fi
      ;;
  esac

  if [ -f "$existing_path" ] && grep -F "#!/usr/bin/env node" "$existing_path" >/dev/null 2>&1; then
    case "$existing_path" in
      *".bun"*)
        printf 'bun\n'
        ;;
      *)
        printf 'npm\n'
        ;;
    esac
    return 0
  fi

  return 1
}

prompt_yes_no() {
  prompt="$1"

  case "$NON_INTERACTIVE" in
    1 | [Tt][Rr][Uu][Ee] | [Yy][Ee][Ss])
      return 1
      ;;
  esac

  if ( : </dev/tty ) 2>/dev/null; then
    printf '%s [y/N] ' "$prompt" >/dev/tty
    if ! IFS= read -r answer </dev/tty; then
      return 1
    fi
  elif [ -t 0 ]; then
    printf '%s [y/N] ' "$prompt"
    if ! IFS= read -r answer; then
      return 1
    fi
  else
    return 1
  fi

  case "$answer" in
    y | Y | yes | YES)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

print_launch_instructions() {
  case "$path_action" in
    added)
      step "Current terminal: export PATH=\"$BIN_DIR:\$PATH\" && codex"
      step "Future terminals: open a new terminal and run: codex"
      step "PATH was added to $path_profile"
      ;;
    updated)
      step "Current terminal: export PATH=\"$BIN_DIR:\$PATH\" && codex"
      step "Future terminals: open a new terminal and run: codex"
      step "PATH was updated in $path_profile"
      ;;
    configured)
      step "Current terminal: export PATH=\"$BIN_DIR:\$PATH\" && codex"
      step "Future terminals: open a new terminal and run: codex"
      step "PATH is already configured in $path_profile"
      ;;
    *)
      step "Current terminal: codex"
      step "Future terminals: open a new terminal and run: codex"
      ;;
  esac
}

maybe_launch_codex_now() {
  if prompt_yes_no "Start Codex now?"; then
    step "Launching Codex"
    "$BIN_PATH"
  fi
}

detect_conflicting_install() {
  existing_path="$(resolve_existing_codex)"
  manager="$(classify_existing_codex "$existing_path" || true)"

  if [ -z "$manager" ]; then
    return
  fi

  conflict_manager="$manager"
  conflict_path="$existing_path"
  step "Detected existing $manager-managed Codex at $existing_path"
  warn "Multiple managed Codex installs can be ambiguous because PATH order decides which one runs."
}

handle_conflicting_install() {
  if [ -z "$conflict_manager" ]; then
    return
  fi

  case "$conflict_manager" in
    brew)
      uninstall_cmd="brew uninstall --cask codex"
      ;;
    bun)
      uninstall_cmd="bun remove -g @openai/codex"
      ;;
    *)
      uninstall_cmd="npm uninstall -g @openai/codex"
      ;;
  esac

  if prompt_yes_no "Uninstall the existing $conflict_manager-managed Codex now?"; then
    step "Running: $uninstall_cmd"
    if ! sh -c "$uninstall_cmd"; then
      warn "Failed to uninstall the existing $conflict_manager-managed Codex. Continuing with the standalone install."
    fi
  else
    warn "Leaving the existing $conflict_manager-managed Codex installed. PATH order will determine which codex runs."
  fi
}

install_package_release() {
  release_dir="$1"
  archive_path="$2"
  resolved_version="$3"
  target="$4"
  layout="$5"
  stage_release="$RELEASES_DIR/.staging.$(basename "$release_dir").$$"

  mkdir -p "$RELEASES_DIR"
  rm -rf "$stage_release"
  mkdir -p "$stage_release"
  tar -xzf "$archive_path" -C "$stage_release"
  chmod 0755 "$stage_release/bin/codex" "$stage_release/codex-path/rg"
  if [ -f "$stage_release/codex-resources/bwrap" ]; then
    chmod 0755 "$stage_release/codex-resources/bwrap"
  fi
  if [ -f "$stage_release/codex-resources/zsh/bin/zsh" ]; then
    chmod 0755 "$stage_release/codex-resources/zsh/bin/zsh"
  fi
  ln -sf "bin/codex" "$stage_release/codex"
  write_install_metadata "$stage_release" "$resolved_version" "$target" "$layout"

  if [ -e "$release_dir" ] || [ -L "$release_dir" ]; then
    rm -rf "$release_dir"
  fi
  mv "$stage_release" "$release_dir"
}

install_legacy_platform_npm_release() {
  release_dir="$1"
  archive_path="$2"
  target="$3"
  resolved_version="$4"
  layout="$5"
  stage_release="$RELEASES_DIR/.staging.$(basename "$release_dir").$$"
  vendor_root="package/vendor/$target"

  mkdir -p "$RELEASES_DIR"
  rm -rf "$stage_release"
  mkdir -p "$stage_release/codex-resources"
  tar -xzf "$archive_path" -C "$stage_release" \
    "$vendor_root/codex/codex" \
    "$vendor_root/path/rg"

  mv "$stage_release/$vendor_root/codex/codex" "$stage_release/codex"
  mv "$stage_release/$vendor_root/path/rg" "$stage_release/codex-resources/rg"
  chmod 0755 "$stage_release/codex" "$stage_release/codex-resources/rg"
  if tar -tzf "$archive_path" "$vendor_root/codex-resources/bwrap" >/dev/null 2>&1; then
    tar -xzf "$archive_path" -C "$stage_release" "$vendor_root/codex-resources/bwrap"
    mv "$stage_release/$vendor_root/codex-resources/bwrap" "$stage_release/codex-resources/bwrap"
    chmod 0755 "$stage_release/codex-resources/bwrap"
  fi
  rm -rf "$stage_release/package"
  write_install_metadata "$stage_release" "$resolved_version" "$target" "$layout"

  if [ -e "$release_dir" ] || [ -L "$release_dir" ]; then
    rm -rf "$release_dir"
  fi
  mv "$stage_release" "$release_dir"
}

write_install_metadata() {
  release_dir="$1"
  resolved_version="$2"
  target="$3"
  layout="$4"
  tmp_metadata="$release_dir/$INSTALL_METADATA_FILE.$$"

  {
    printf 'version=%s\n' "$resolved_version"
    printf 'target=%s\n' "$target"
    printf 'layout=%s\n' "$layout"
  } >"$tmp_metadata"
  mv "$tmp_metadata" "$release_dir/$INSTALL_METADATA_FILE"
}

release_dir_is_complete() {
  release_dir="$1"
  expected_version="$2"
  expected_target="$3"
  layout="$4"

  [ -d "$release_dir" ] &&
    [ "$(basename "$release_dir")" = "$expected_version-$expected_target" ] ||
    return 1

  [ -f "$release_dir/$INSTALL_METADATA_FILE" ] ||
    return 1
  [ "$(install_metadata_field "$release_dir" version || true)" = "$expected_version" ] &&
    [ "$(install_metadata_field "$release_dir" target || true)" = "$expected_target" ] &&
    [ "$(install_metadata_field "$release_dir" layout || true)" = "$layout" ] ||
    return 1

  case "$layout" in
    package)
      [ -f "$release_dir/codex-package.json" ] &&
        [ -x "$release_dir/bin/codex" ] &&
        [ -x "$release_dir/codex" ] &&
        [ -x "$release_dir/codex-path/rg" ] ||
        return 1
      ;;
    legacy-platform-npm)
      [ -x "$release_dir/codex" ] &&
        [ -x "$release_dir/codex-resources/rg" ] ||
        return 1
      ;;
    *)
      return 1
      ;;
  esac

  case "$layout:$expected_target" in
    package:*linux* | legacy-platform-npm:*linux*) [ -x "$release_dir/codex-resources/bwrap" ] || return 1 ;;
    *) true ;;
  esac

  case "$layout:$expected_target" in
    package:*linux* | package:*apple-darwin) [ -x "$release_dir/codex-resources/zsh/bin/zsh" ] || return 1 ;;
    *) true ;;
  esac
}

update_current_link() {
  release_dir="$1"
  tmp_link="$STANDALONE_ROOT/.current.$$"

  replace_path_with_symlink "$CURRENT_LINK" "$release_dir" "$tmp_link"
}

release_codex_relative_path() {
  release_dir="$1"

  if [ -x "$release_dir/bin/codex" ]; then
    printf 'bin/codex\n'
  else
    printf 'codex\n'
  fi
}

update_visible_command() {
  release_dir="$1"
  mkdir -p "$BIN_DIR"
  tmp_link="$BIN_DIR/.codex.$$"
  codex_relative_path="$(release_codex_relative_path "$release_dir")"

  replace_path_with_symlink "$BIN_PATH" "$CURRENT_LINK/$codex_relative_path" "$tmp_link"
}

verify_visible_command() {
  "$BIN_PATH" --version >/dev/null
}

parse_args "$@"

require_command mktemp
require_command tar

case "$(uname -s)" in
  Darwin)
    os="darwin"
    ;;
  Linux)
    os="linux"
    ;;
  *)
    echo "install.sh supports macOS and Linux. Use install.ps1 on Windows." >&2
    exit 1
    ;;
esac

case "$(uname -m)" in
  x86_64 | amd64)
    arch="x86_64"
    ;;
  arm64 | aarch64)
    arch="aarch64"
    ;;
  *)
    echo "Unsupported architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

if [ "$os" = "darwin" ] && [ "$arch" = "x86_64" ]; then
  if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = "1" ]; then
    arch="aarch64"
  fi
fi

if [ "$os" = "darwin" ]; then
  if [ "$arch" = "aarch64" ]; then
    npm_tag="darwin-arm64"
    vendor_target="aarch64-apple-darwin"
    platform_label="macOS (Apple Silicon)"
  else
    npm_tag="darwin-x64"
    vendor_target="x86_64-apple-darwin"
    platform_label="macOS (Intel)"
  fi
else
  if [ "$arch" = "aarch64" ]; then
    npm_tag="linux-arm64"
    vendor_target="aarch64-unknown-linux-musl"
    platform_label="Linux (ARM64)"
  else
    npm_tag="linux-x64"
    vendor_target="x86_64-unknown-linux-musl"
    platform_label="Linux (x64)"
  fi
fi

resolve_version
resolved_version="$resolved_version_result"
package_asset="codex-package-$vendor_target.tar.gz"
checksum_asset="codex-package_SHA256SUMS"
release_name="$resolved_version-$vendor_target"
release_dir="$RELEASES_DIR/$release_name"
install_layout=""
asset=""

if release_dir_is_complete "$release_dir" "$resolved_version" "$vendor_target" "package"; then
  install_layout="package"
  asset="$package_asset"
elif release_dir_is_complete "$release_dir" "$resolved_version" "$vendor_target" "legacy-platform-npm"; then
  install_layout="legacy-platform-npm"
  asset="codex-npm-$npm_tag-$resolved_version.tgz"
else
  if release_asset_exists "$package_asset" "$resolved_version" &&
    release_asset_exists "$checksum_asset" "$resolved_version"; then
    install_layout="package"
    asset="$package_asset"
  elif release_asset_exists "codex-npm-$npm_tag-$resolved_version.tgz" "$resolved_version"; then
    install_layout="legacy-platform-npm"
    asset="codex-npm-$npm_tag-$resolved_version.tgz"
  else
    echo "Could not find Codex package or platform npm release assets for Codex $resolved_version." >&2
    exit 1
  fi
fi
download_url="$(release_url_for_asset "$asset" "$resolved_version")"
checksum_url="$(release_url_for_asset "$checksum_asset" "$resolved_version")"
current_version="$(current_installed_version)"

if [ -n "$current_version" ] && [ "$current_version" != "$resolved_version" ]; then
  step "Updating Codex CLI from $current_version to $resolved_version"
elif [ -n "$current_version" ]; then
  step "Updating Codex CLI"
else
  step "Installing Codex CLI"
fi
step "Detected platform: $platform_label"
step "Resolved version: $resolved_version"

detect_conflicting_install

tmp_dir="$(mktemp -d)"
cleanup() {
  release_install_lock
  if [ -n "$tmp_dir" ]; then
    rm -rf "$tmp_dir"
  fi
}
trap cleanup EXIT INT TERM

acquire_install_lock
cleanup_stale_install_artifacts

if ! release_dir_is_complete "$release_dir" "$resolved_version" "$vendor_target" "$install_layout"; then
  if [ -e "$release_dir" ] || [ -L "$release_dir" ]; then
    warn "Found incomplete existing release at $release_dir; reinstalling."
  fi

  archive_path="$tmp_dir/$asset"
  checksum_path="$tmp_dir/$checksum_asset"

  step "Downloading Codex CLI"
  if [ "$install_layout" = "package" ]; then
    checksum_digest="$(release_asset_digest "$checksum_asset" "$resolved_version")"
    ensure_downloader
    download_file "$checksum_url" "$checksum_path" &
    checksum_download_pid="$!"
    download_file "$download_url" "$archive_path" &
    archive_download_pid="$!"
    wait_for_download "$checksum_download_pid" "$checksum_asset"
    verify_archive_digest "$checksum_path" "$checksum_digest"
    expected_digest="$(package_archive_digest "$asset" "$checksum_path")"
    wait_for_download "$archive_download_pid" "$asset"
  else
    expected_digest="$(release_asset_digest "$asset" "$resolved_version")"
    download_file "$download_url" "$archive_path" &
    archive_download_pid="$!"
    wait_for_download "$archive_download_pid" "$asset"
  fi
  verify_archive_digest "$archive_path" "$expected_digest"

  step "Installing standalone package to $release_dir"
  if [ "$install_layout" = "package" ]; then
    install_package_release "$release_dir" "$archive_path" "$resolved_version" "$vendor_target" "$install_layout"
  else
    install_legacy_platform_npm_release "$release_dir" "$archive_path" "$vendor_target" "$resolved_version" "$install_layout"
  fi
fi
update_current_link "$release_dir"
update_visible_command "$release_dir"
add_to_path
verify_visible_command
release_install_lock
handle_conflicting_install

case "$path_action" in
  added)
    print_launch_instructions
    ;;
  updated)
    print_launch_instructions
    ;;
  configured)
    print_launch_instructions
    ;;
  *)
    step "$BIN_DIR is already on PATH"
    print_launch_instructions
    ;;
esac

printf 'Codex CLI %s installed successfully.\n' "$resolved_version"
maybe_launch_codex_now
