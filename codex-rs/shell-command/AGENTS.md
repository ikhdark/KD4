# shell-command policy

This file applies inside `codex-rs/shell-command`. It inherits the Rust
workspace rules from `codex-rs/AGENTS.md`.

## Ownership

This crate owns shell command execution plumbing and command-environment
handling used by Codex runtime paths. Treat it as an execution-safety surface.

Coordinate changes across the owning boundary:

- shell command construction and environment handling belong here;
- privilege/elevation behavior belongs in `codex-rs/shell-escalation`;
- sandbox policy and enforcement belong in `codex-rs/core`,
  `codex-rs/sandboxing`, or platform-specific sandbox crates;
- app-server command APIs belong in `codex-rs/app-server`.

## Change Rules

- Do not weaken command quoting, shell selection, path handling, environment
  propagation, cancellation, timeout, or output-capture behavior unless the user
  explicitly names that surface.
- Keep Windows PowerShell/cmd behavior explicit and tested separately from Unix
  shell behavior.
- Avoid string-built command lines when an argv-style API can preserve argument
  boundaries.
- Preserve sandbox and approval semantics; shell-command code must not silently
  bypass runtime policy.
- Keep output decoding and truncation behavior factual and bounded.

## Validation

For implementation changes, run focused `codex-shell-command` validation. If the
behavior crosses sandbox, escalation, or app-server command APIs, also run the
nearest focused tests for those crates.
