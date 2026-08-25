# codex-core

This crate implements the business logic for Codex. It is designed to be used by the various Codex UIs written in Rust.

## Dependencies

`codex-core` targets native Windows and uses the Windows sandbox helpers.

Legacy `SandboxPolicy` / `sandbox_mode` configs are still supported on
Windows. Legacy `read-only` and `workspace-write` policies imply full
filesystem read access; exact readable roots are represented by split
filesystem policies instead.

The elevated Windows sandbox also supports:

- legacy `ReadOnly` and `WorkspaceWrite` behavior
- split filesystem policies that need exact readable roots, exact writable
  roots, or extra read-only carveouts under writable roots
- backend-managed system read roots required for basic execution, such as
  `C:\Windows`, `C:\Program Files`, `C:\Program Files (x86)`, and
  `C:\ProgramData`, when a split filesystem policy requests platform defaults

The unelevated restricted-token backend still supports the legacy full-read
Windows model for legacy `ReadOnly` and `WorkspaceWrite` behavior. It also
supports a narrow split-filesystem subset: full-read split policies whose
writable roots still match the legacy `WorkspaceWrite` root set, but add extra
read-only carveouts under those writable roots.

New `[permissions]` / split filesystem policies remain supported on Windows
only when they can be enforced directly by the selected Windows backend or
round-trip through the legacy `SandboxPolicy` model without changing semantics.
Policies that would require direct explicit unreadable carveouts (`none`) or
reopened writable descendants under read-only carveouts still fail closed
instead of running with weaker enforcement.

### All Platforms

Expects the binary containing `codex-core` to simulate the virtual
`apply_patch` CLI when `arg1` is `--codex-run-as-apply-patch`. See the
`codex-arg0` crate for details.
