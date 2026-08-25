# Install Scripts

Agent workflow rules for this directory live in `AGENTS.md`. This README is a
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
