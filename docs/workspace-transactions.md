# Retired task workspaces

The `workspace_transaction`, `semantic_context`, and `workspace_validation` tools
and their packaged workspace worker have been removed. Commands, reads, and edits
use the configured checkout without automatic transaction routing, validation
snapshots, or snapshot-specific Cargo output overrides. Use normal command tools
for Rust analysis and focused Cargo checks.

Existing `CODEX_HOME/workspace-transactions` folders are not deleted or reconciled
by this change. They may contain unmerged edits: preserve and inspect them before
any manual recovery or cleanup. The removed tool is not available for recovery in
a newly built binary.

These source changes take effect in Desktop only after an explicitly requested
local binary rebuild/replacement and Desktop restart.
