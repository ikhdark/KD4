#!/bin/sh
# Release metadata and checksum helpers sourced by install.sh.
# The release workflow bundles these functions back into standalone install.sh.

release_url_for_asset() {
  asset="$1"
  resolved_version="$2"

  printf 'https://github.com/openai/codex/releases/download/rust-v%s/%s\n' "$resolved_version" "$asset"
}

release_metadata_url() {
  resolved_version="$1"

  printf 'https://api.github.com/repos/openai/codex/releases/tags/rust-v%s\n' "$resolved_version"
}

release_asset_digest_or_empty() {
  asset="$1"
  resolved_version="$2"
  ensure_release_metadata "$resolved_version"

  digest="$(printf '%s\n' "$release_json_cache" | awk -v asset="$asset" '
    /"name":[[:space:]]*"[^"]+"/ {
      name = $0
      sub(/^.*"name":[[:space:]]*"/, "", name)
      sub(/".*$/, "", name)
      if (name == asset) {
        in_asset = 1
        asset_depth = depth
      }
    }

    in_asset && /"digest":[[:space:]]*"[^"]+"/ {
      digest = $0
      sub(/^.*"digest":[[:space:]]*"/, "", digest)
      sub(/".*$/, "", digest)
    }

    {
      line = $0
      opens = gsub(/\{/, "{", line)
      closes = gsub(/\}/, "}", line)
      depth += opens - closes

      if (in_asset && depth < asset_depth) {
        in_asset = 0
      }
    }

    END {
      if (digest != "") {
        print digest
      }
    }
  ')"

  case "$digest" in
    sha256:????????????????????????????????????????????????????????????????)
      printf '%s\n' "${digest#sha256:}"
      ;;
    *)
      return 1
      ;;
  esac
}

ensure_release_metadata() {
  resolved_version="$1"

  if [ "$release_json_version" = "$resolved_version" ] && [ -n "$release_json_cache" ]; then
    return
  fi

  if ! release_json_cache="$(
    download_text "$(release_metadata_url "$resolved_version")"
  )"; then
    echo "Could not fetch GitHub release metadata for Codex $resolved_version." >&2
    exit 1
  fi
  release_json_version="$resolved_version"
}

release_asset_exists() {
  asset="$1"
  resolved_version="$2"

  release_asset_digest_or_empty "$asset" "$resolved_version" >/dev/null
}

release_asset_digest() {
  asset="$1"
  resolved_version="$2"

  digest="$(release_asset_digest_or_empty "$asset" "$resolved_version" || true)"
  if [ -z "$digest" ]; then
    echo "Could not find SHA-256 digest for release asset $asset." >&2
    exit 1
  fi

  printf '%s\n' "$digest"
}

package_archive_digest() {
  asset="$1"
  manifest_path="$2"

  digest="$(awk -v asset="$asset" '
    $2 == asset && $1 ~ /^[0-9a-fA-F]{64}$/ {
      print tolower($1)
      found = 1
      exit
    }
    END {
      if (!found) {
        exit 1
      }
    }
  ' "$manifest_path" 2>/dev/null || true)"

  if [ -z "$digest" ]; then
    echo "Could not find SHA-256 digest for $asset in codex-package_SHA256SUMS." >&2
    exit 1
  fi

  printf '%s\n' "$digest"
}

file_sha256() {
  path="$1"

  ensure_sha256
  case "$sha256_cmd" in
    sha256sum)
      sha256sum "$path" | awk '{print $1}'
      ;;
    shasum)
      shasum -a 256 "$path" | awk '{print $1}'
      ;;
    openssl)
      openssl dgst -sha256 "$path" | sed 's/^.*= //'
      ;;
  esac
}

ensure_sha256() {
  if [ -n "$sha256_cmd" ]; then
    return
  fi

  if command -v sha256sum >/dev/null 2>&1; then
    sha256_cmd="sha256sum"
    return
  fi

  if command -v shasum >/dev/null 2>&1; then
    sha256_cmd="shasum"
    return
  fi

  if command -v openssl >/dev/null 2>&1; then
    sha256_cmd="openssl"
    return
  fi

  echo "sha256sum, shasum, or openssl is required to verify the Codex download." >&2
  exit 1
}

verify_archive_digest() {
  archive_path="$1"
  expected_digest="$2"
  actual_digest="$(file_sha256 "$archive_path")"

  if [ "$actual_digest" != "$expected_digest" ]; then
    echo "Downloaded Codex archive checksum did not match expected digest." >&2
    echo "expected: $expected_digest" >&2
    echo "actual:   $actual_digest" >&2
    exit 1
  fi
}
