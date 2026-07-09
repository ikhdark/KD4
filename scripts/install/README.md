# Install Scripts

Agent workflow rules for this directory live in `AGENTS.md`. This README is a
human-facing overview of the installer surface.

## Overview

This directory owns the standalone Codex install entrypoints:

- `install.sh`: macOS/Linux shell installer.
- `install.ps1`: Windows PowerShell installer.

The installers fetch OpenAI Codex release artifacts, verify release digests,
stage standalone package layouts under the Codex home directory, expose the
selected binary on PATH, and handle conflicts with existing npm, bun, Homebrew,
or older standalone installs.
