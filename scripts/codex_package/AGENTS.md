# Codex Package Scripts Policy

## Ownership

This package owns the canonical Codex package assembly contract for CLI,
app-server, and local packaging proof. Keep package layout, archive contents,
native binary staging, the sibling code-mode host, the patched zsh runtime,
DotSlash metadata, bundled ripgrep, target metadata, version derivation, and
V8/rusty_v8 support synchronized.

## Contracts

- Keep archive/layout metadata deterministic; treat layout changes as package
  contract changes.
- Keep Cargo source reuse/fingerprints shared by the entrypoint and code-mode
  host; keep target triples, native names, and archives aligned with consumers.
- Preserve DotSlash verification/extraction, ripgrep permissions/naming,
  checksum-verified V8 fallback, version semantics, and zsh resource/manifest
  behavior. Nearby `test_*.py` files own focused coverage.
- Ignore `__pycache__/` and `*.pyc`; they are not source evidence.

## Editing Rules

- Keep generated package contracts stable unless the task explicitly changes the
  package format.
- Use `release` for distribution proof; `local-release` is only for local
  iteration unless explicitly requested.
- Do not hand-edit generated package outputs; change the source helper and
  regenerate through the owning script or just recipe.
- For separator, permission, native-name, or archive changes, check every target
  in `targets.py`.
- Keep tests close to the helper being changed. Prefer focused package tests over
  broad package staging while iterating when tests are allowed.

## Validation

- If tests are waived, use focused static/read-back/dry-run/path checks only.
- Use the local summarizer for supported high-output commands; keep exact
  searches raw and bounded.
- When tests are allowed, run the closest `python -m unittest
  scripts.codex_package.test_<name>` for the touched helper before broader
  staging/package proof.

Report changed helpers, affected contracts, proof, and skipped targets/tests.
