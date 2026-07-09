# Codex Package Scripts

Agent workflow rules for this directory live in `AGENTS.md`. This README is a
human-facing overview of the package-script surface.

## Overview

This package owns the canonical Codex package assembly contract for CLI,
app-server, and local packaging proof. It covers package layout, archive
contents, native binary staging, shell completions, DotSlash metadata, bundled
ripgrep, target metadata, version derivation, and V8/rusty_v8 support.

## File Context

- `archive.py` and `test_archive.py`: archive creation and packaged artifact
  contents. Preserve deterministic layout and metadata where tests depend on it.
- `cargo.py` and `test_cargo.py`: Cargo metadata/build artifact helpers. Prefer
  `cargo metadata` over hand-maintained package lists.
- `cli.py` and `test_cli.py`: Codex CLI binary/package entrypoint assembly.
- `dotslash.py` and `test_dotslash.py`: DotSlash metadata generation and
  validation. Keep emitted JSON/paths stable for release tooling.
- `layout.py` and `test_layout.py`: canonical package directory structure,
  metadata, and Windows `apply_patch` alias validation. Treat layout changes as
  packaging contract changes.
- `ripgrep.py`, `rg`, and `test_ripgrep.py`: bundled ripgrep discovery/staging
  and checked-in DotSlash manifest coverage. Preserve executable permissions and
  platform-specific naming.
- `targets.py` and `test_targets.py`: package target matrix and artifact naming.
  Keep target triples, native package names, and archive names aligned with
  release and installer consumers.
- `v8.py` and `test_v8.py`: V8/rusty_v8 packaging support and release-pair logic.
- `version.py` and `test_version.py`: package version derivation. Avoid changing
  version semantics without checking downstream package consumers.
- `zsh.py`, `codex-zsh`, and `test_zsh.py`: zsh completion packaging. Preserve
  installed completion names and script behavior.
- `__init__.py`: package marker and public module boundary.
