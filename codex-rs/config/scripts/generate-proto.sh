#!/usr/bin/env bash
set -euo pipefail

check=0
protoc_path=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --check)
            check=1
            shift
            ;;
        --protoc-path)
            if [[ $# -lt 2 ]]; then
                echo "--protoc-path requires a path" >&2
                exit 2
            fi
            protoc_path="$2"
            shift 2
            ;;
        *)
            echo "Usage: $0 [--check] [--protoc-path <path-to-protoc>]" >&2
            exit 2
            ;;
    esac
done

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../../.." && pwd)"
source_proto_dir="$repo_root/codex-rs/config/src/thread_config/proto"
source_proto="$source_proto_dir/codex.thread_config.v1.proto"
checked_generated="$source_proto_dir/codex.thread_config.v1.rs"
tmpdir="$(mktemp -d)"
proto_dir="$tmpdir/proto"
generated="$proto_dir/codex.thread_config.v1.rs"
install_tmp=""

cleanup() {
    if [[ -n "$install_tmp" ]]; then
        rm -f "$install_tmp"
    fi
    rm -rf "$tmpdir"
}
trap cleanup EXIT

mkdir -p "$proto_dir"
cp "$source_proto" "$proto_dir/"

if [[ -n "$protoc_path" ]]; then
    if [[ ! -x "$protoc_path" ]]; then
        echo "protoc is not executable: $protoc_path" >&2
        exit 1
    fi
    export PROTOC="$protoc_path"
fi

(
    cd "$repo_root/codex-rs"
    CARGO_TARGET_DIR="$tmpdir/target" cargo run --locked \
        -p codex-config \
        --example generate-proto \
        -- "$proto_dir"
)

if ! sed -n '2p' "$generated" | grep -q 'clippy::trivially_copy_pass_by_ref'; then
    {
        sed -n '1p' "$generated"
        printf '#![allow(clippy::trivially_copy_pass_by_ref)]\n'
        sed '1d' "$generated"
    } > "$tmpdir/generated.rs"
    mv "$tmpdir/generated.rs" "$generated"
fi

rustfmt --edition 2024 "$generated"

awk '
    NR == 3 && previous ~ /clippy::trivially_copy_pass_by_ref/ && $0 != "" { print "" }
    { print; previous = $0 }
' "$generated" > "$tmpdir/formatted.rs"
mv "$tmpdir/formatted.rs" "$generated"

if [[ -f "$checked_generated" ]] && cmp -s "$generated" "$checked_generated"; then
    if [[ $check -eq 1 ]]; then
        echo "Config proto is up to date: $checked_generated"
    else
        echo "Config proto is already current: $checked_generated"
    fi
elif [[ $check -eq 1 ]]; then
    echo "Generated config proto is stale. Run: just generate-config-proto" >&2
    exit 1
else
    install_tmp="$(mktemp "$source_proto_dir/.codex.thread_config.v1.rs.XXXXXX")"
    cp "$generated" "$install_tmp"
    mv -f "$install_tmp" "$checked_generated"
    install_tmp=""
    echo "Updated config proto: $checked_generated"
fi
