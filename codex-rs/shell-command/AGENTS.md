# shell-command policy

## Ownership

This crate owns shell command execution plumbing and command-environment
handling used by Codex runtime paths. Treat it as an execution-safety surface.

Construction/environment handling belong here; elevation belongs in
Windows sandbox policy in core/sandbox crates; app-server command
APIs in `app-server`.

## Change Rules

- Do not weaken quoting, shell/path/environment handling, cancellation,
  timeouts, or output capture unless explicitly requested.
- Keep Windows PowerShell/cmd behavior explicit and tested separately from Unix
  shell behavior.
- Avoid string-built command lines when an argv-style API can preserve argument
  boundaries.
- Preserve sandbox and approval semantics; shell-command code must not silently
  bypass runtime policy.
- Keep output decoding and truncation behavior factual and bounded.

## Validation

Run focused `codex-shell-command` validation, plus the nearest sandbox,
escalation, or app-server test when crossing those boundaries.
