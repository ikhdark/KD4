# Install Scripts Policy

## Ownership

This directory owns the Windows standalone Codex install entrypoint,
`install.ps1`.

They fetch releases, verify digests, stage standalone layouts, expose PATH, and
handle npm/bun/older-install conflicts.

## Contract

- Preserve `CODEX_RELEASE`, `CODEX_INSTALL_DIR`, `CODEX_HOME`, and
  `CODEX_NON_INTERACTIVE` semantics.
- Preserve `latest`, `rust-v*`, `v*`, and semver normalization.
- Preserve SHA-256 verification for downloaded archives. Do not weaken missing
  digest handling.
- Preserve locks/stale cleanup, staging, and atomic release retargeting.
- Preserve standalone metadata files and package completeness checks. Layout
  changes must stay synchronized with `scripts/codex_package/`.
- Bound PATH edits to the installer-managed block/visible platform path.
- Preserve non-interactive behavior. Do not add prompts that can block
  automation when `CODEX_NON_INTERACTIVE` is enabled.

## Editing Rules

- Use PowerShell/.NET APIs for platform integration.
- Keep network access limited to release metadata and release asset downloads.
- Do not depend on repo-local builds, dev environments, or bytecode caches.
- Do not hand-edit generated package artifacts to satisfy installer checks.

## Validation

- Run a PowerShell parser check for `install.ps1` changes and the narrowest
  dry-run, unit, or static check that exercises the changed branch without
  replacing a user install.
- Do not run install commands that mutate PATH, replace visible binaries, or
  uninstall conflicting managers unless the request explicitly asks for an
  install-flow execution.

Report validation and any restart or PATH reload.
