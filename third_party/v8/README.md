# `rusty_v8` Cargo Artifacts

KD4 consumes the exact `v8` crate version pinned in `codex-rs/Cargo.toml` and
`codex-rs/Cargo.lock`. Cargo uses the upstream prebuilt archives published by
`denoland/rusty_v8` unless `V8_FROM_SOURCE`, `RUSTY_V8_ARCHIVE`, or
`RUSTY_V8_MIRROR` explicitly selects another supported Cargo path.

This Windows-only checkout uses the local-publish guard in
`scripts/install/publish-local-codex.ps1`. It accepts an existing cached archive,
a local `-RustyV8Archive` input, or `RUSTY_V8_ARCHIVE`. Missing archives do not
trigger a GitHub download unless `-AllowRustyV8Download` is explicitly supplied;
`RUSTY_V8_MIRROR` delegates retrieval to Cargo's build script.

The checked-in `rusty_v8_<version>.sha256` file records upstream archive
checksums used by the Windows local-publish guard before it seeds or accepts a
cached archive. Keep that file synchronized with the exact resolved `v8` crate
version whenever the dependency is updated.

Do not mix archives or generated bindings across crate versions. Local builds
use the `v8` crate's Cargo build script rather than a parallel project build
graph; upstream musl CI/release setup is not this checkout's local workflow.
