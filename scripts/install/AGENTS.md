# Install Scripts Policy

This file applies inside `scripts/install/`. It inherits `scripts/AGENTS.md` and
the repository root `AGENTS.md`.

## Ownership

This directory owns the standalone Codex install entrypoints:

- `install.sh`: macOS/Linux shell installer.
- `install.ps1`: Windows PowerShell installer.

The installers fetch OpenAI Codex release artifacts, verify release digests,
stage standalone package layouts under the Codex home directory, expose the
selected binary on PATH, and handle conflicts with existing npm, bun, Homebrew,
or older standalone installs.

## Contract

- Preserve `CODEX_RELEASE`, `CODEX_INSTALL_DIR`, `CODEX_HOME`, and
  `CODEX_NON_INTERACTIVE` semantics.
- Keep `latest`, `rust-v*`, `v*`, and explicit semver release normalization
  aligned across both scripts.
- Preserve SHA-256 verification for downloaded archives. Do not weaken missing
  digest handling.
- Keep install locks, stale-lock cleanup, staging directories, and atomic
  current-release retargeting intact.
- Preserve standalone metadata files and package completeness checks. Layout
  changes must stay synchronized with `scripts/codex_package/`.
- Keep PATH modification bounded to the installer-managed block or visible
  command path for the platform.
- Preserve non-interactive behavior. Do not add prompts that can block
  automation when `CODEX_NON_INTERACTIVE` is enabled.

## Editing Rules

- Treat `install.sh` and `install.ps1` as matching platform implementations of
  one installer contract. If a behavior applies to both platforms, update both
  scripts or document why it is platform-specific.
- Use platform-native primitives: POSIX shell utilities in `install.sh` and
  PowerShell/.NET APIs in `install.ps1`.
- Keep network access limited to release metadata and release asset downloads.
- Do not make the installer depend on repository-local build outputs,
  development virtual environments, or Python bytecode caches.
- Do not hand-edit generated package artifacts to satisfy installer checks.

## Validation

- For shell changes, run a syntax check when available, such as
  `sh -n scripts/install/install.sh`.
- For PowerShell changes, run a parser check against
  `scripts/install/install.ps1`.
- For contract changes shared by both installers, use the narrowest dry-run,
  unit, or static check that exercises the changed branch without replacing a
  user install.
- Do not run install commands that mutate PATH, replace visible binaries, or
  uninstall conflicting managers unless the request explicitly asks for an
  install-flow execution.

## Reporting

- Report which installer path changed, which platform behavior was validated,
  and which platform path was not exercised.
- Call out any runtime restart, shell restart, or PATH reload required for an
  installer behavior to become visible.
