# Codex Package Scripts Policy

This file applies inside `scripts/codex_package/`. It inherits `scripts/AGENTS.md`
and the repository root `AGENTS.md`.

## Ownership

This package owns the canonical Codex package assembly contract for CLI,
app-server, and local packaging proof. Keep package layout, archive contents,
native binary staging, the sibling code-mode host, the patched zsh runtime,
DotSlash metadata, bundled ripgrep, target metadata, version derivation, and
V8/rusty_v8 support synchronized.

## File Context

- `archive.py` and `test_archive.py`: archive creation and packaged artifact
  contents. Preserve deterministic layout and metadata where tests depend on it.
- `cargo.py` and `test_cargo.py`: Cargo build, source-output reuse, and artifact
  fingerprint helpers. Keep the entrypoint and code-mode host in the same source
  build/reuse contract.
- `cli.py` and `test_cli.py`: Codex CLI binary/package entrypoint assembly.
- `dotslash.py` and `test_dotslash.py`: DotSlash manifest parsing, verified
  download caching, and safe executable extraction.
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
- `zsh.py`, `codex-zsh`, and `test_zsh.py`: patched zsh runtime packaging.
  Preserve its installed resource path and caller-selectable manifest behavior.
- `__init__.py`: package marker and public module boundary.
- `__pycache__/` and `*.pyc`: ignored Python bytecode caches. Do not edit or
  use them as source evidence.

## Editing Rules

- Keep generated package contracts stable unless the task explicitly changes the
  package format.
- Keep release package artifacts on the `release` Cargo profile. The
  `local-release` profile is for local publish iteration, not distribution
  package proof, unless the request explicitly asks for an experimental local
  package build.
- Do not hand-edit generated package outputs; change the source helper and
  regenerate through the owning script or just recipe.
- Do not edit `__pycache__/` or bytecode caches. They are regenerated local
  interpreter state.
- Preserve cross-platform targets. If a change affects path separators,
  executable bits, native package names, or archive formats, check every supported
  target in `targets.py`.
- Keep tests close to the helper being changed. Prefer focused package tests over
  broad package staging while iterating when tests are allowed.

## Validation

- If the request says no tests, do not run test commands and do not suggest test
  gates in that turn. Use focused non-test checks only when relevant, such as
  read-back proof, static inspection, dry-run proof, or command/path existence
  checks.
- Use the local command summarizer for supported high-output commands when
  available and the command fits its supported invocation shape. Keep exact
  searches raw and bounded.
- When tests are allowed, run the closest `python -m unittest
  scripts.codex_package.test_<name>` for the touched helper before broader
  staging/package proof.

## Reporting

- Report changed helper modules, affected package contract, selected proof, and
  skipped target platforms or tests.
