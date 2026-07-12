# `rusty_v8` Cargo Artifacts

KD4 consumes the exact `v8` crate version pinned in `codex-rs/Cargo.toml` and
`codex-rs/Cargo.lock`. Cargo uses the upstream prebuilt archives published by
`denoland/rusty_v8` unless `V8_FROM_SOURCE`, `RUSTY_V8_ARCHIVE`, or
`RUSTY_V8_MIRROR` explicitly selects another supported Cargo path.

The upstream release does not provide musl archives for the pinned version.
Musl CI, release, and package builds therefore download the matching
checksum-verified Codex-hosted archive and generated binding, then pass them to
the same Cargo build script through `RUSTY_V8_ARCHIVE` and
`RUSTY_V8_SRC_BINDING_PATH`.

The checked-in `rusty_v8_<version>.sha256` file records upstream archive
checksums used by the Windows local-publish guard before it seeds or accepts a
cached archive. Keep that file synchronized with the exact resolved `v8` crate
version whenever the dependency is updated.

Do not mix archives or generated bindings across crate versions. Normal CI and
release builds intentionally use the `v8` crate's Cargo build script instead of
maintaining a parallel project build graph.
