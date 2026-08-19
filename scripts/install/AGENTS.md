# Install Scripts Policy

## Ownership

This directory owns the standalone Codex install entrypoints:

- `install.sh`: macOS/Linux shell installer.
- `install.ps1`: Windows PowerShell installer.

They fetch releases, verify digests, stage standalone layouts, expose PATH, and
handle npm/bun/Homebrew/older-install conflicts.

## Contract

- Preserve `CODEX_RELEASE`, `CODEX_INSTALL_DIR`, `CODEX_HOME`, and
  `CODEX_NON_INTERACTIVE` semantics.
- Align `latest`, `rust-v*`, `v*`, and semver normalization across both scripts.
- Preserve SHA-256 verification for downloaded archives. Do not weaken missing
  digest handling.
- Preserve locks/stale cleanup, staging, and atomic release retargeting.
- Preserve standalone metadata files and package completeness checks. Layout
  changes must stay synchronized with `scripts/codex_package/`.
- Bound PATH edits to the installer-managed block/visible platform path.
- Preserve non-interactive behavior. Do not add prompts that can block
  automation when `CODEX_NON_INTERACTIVE` is enabled.

## Editing Rules

- Treat both scripts as one contract; update both or document platform scope.
- Use platform-native primitives: POSIX shell utilities in `install.sh` and
  PowerShell/.NET APIs in `install.ps1`.
- Keep network access limited to release metadata and release asset downloads.
- Do not depend on repo-local builds, dev environments, or bytecode caches.
- Do not hand-edit generated package artifacts to satisfy installer checks.

## Validation

- Run `sh -n scripts/install/install.sh` for shell changes and a parser check for
  `install.ps1` changes.
- For contract changes shared by both installers, use the narrowest dry-run,
  unit, or static check that exercises the changed branch without replacing a
  user install.
- Do not run install commands that mutate PATH, replace visible binaries, or
  uninstall conflicting managers unless the request explicitly asks for an
  install-flow execution.

Report changed/validated/unexercised platforms and any restart or PATH reload.
