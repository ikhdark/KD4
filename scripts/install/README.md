# Install Scripts

Repository instructions live in the root [AGENTS.md](../../AGENTS.md). This README is a
human-facing overview of the installer surface.

## Overview

This directory owns the Windows standalone Codex install entrypoint,
`install.ps1`.

The installer fetches KD4 release artifacts from `ikhdark/KD4`, verifies release
digests, stages standalone package layouts under the Codex home directory,
exposes the selected binary on PATH, and handles conflicts with existing npm,
bun, or older standalone installs. Set `CODEX_RELEASE_REPOSITORY=owner/name` to
use another explicitly selected fork release source.

The standalone package layout is supported from Codex `0.133.0` onward. The
installer rejects older releases before requesting release assets; the retired
platform npm archive layout is not probed or installed.

## Fast repeated local publishes

From the checkout root, run `just publish-local-codex-fast`, or invoke
`scripts/install/publish-local-codex-fast.ps1` directly in PowerShell.
Publisher options are forwarded, for example:

```powershell
just publish-local-codex-fast -BuildOnly
just publish-local-codex-fast
```

The `local-fast` profile inherits release behavior, uses optimization level 2,
disables LTO, enables incremental compilation, and uses 16 codegen units rather
than the final profile's single unit. It targets shorter rebuilds after local
edits, at the cost of some runtime optimization. No speedup has been benchmarked.
Only `icu_calendar` uses optimization level 0 in this profile to work around a
Windows compiler `Missing .seh_endepilogue` error; final builds are unaffected.
The first build warms a separate `codex-rs/target/publish-local-fast` cache;
keep that directory for subsequent builds. Unchanged inputs and artifacts reuse
the existing content-bound build stamp and skip Cargo entirely.

This uses the existing publisher for the CLI, Code Mode host, and both sandbox
helpers, including freshness checks, rollback, Desktop configuration, and restart
safety. Desktop uses the fork home (`C:\Users\kuh\Desktop\LOCAL-KD`), not the
upstream home. Publishing can restart Desktop; `-BuildOnly` only prepares the
bundle. Existing protections may defer activation when invoked inside Codex.
`just publish-local-codex-final` remains unchanged for runtime-optimized builds.
