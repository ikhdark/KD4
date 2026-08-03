# codex-rs Rust workspace policy

This file applies inside `codex-rs` and inherits the root `AGENTS.md`.

## Scope and routing

- Read the nearest nested `AGENTS.md` that is actually present; it augments this
  policy for its subtree.
- Use [`../SOURCEMAP.md`](../SOURCEMAP.md) for the Rust package inventory,
  runtime owners, generated contracts, validation routes, and the on-demand
  Rust workflow/tool reference. Do not duplicate that inventory here.
- Keep this parent policy limited to Rust-wide invariants. Put crate-only
  operational rules in the nearest scoped `AGENTS.md` and architecture or usage
  background in README files.

## Workspace invariants

- Prefer existing crate boundaries, helpers, and local patterns before adding a
  new abstraction or moving behavior into `codex-core`.
- Do not run multiple normal Cargo, `just test`, or `just fix` commands against
  the shared `codex-rs/target` concurrently. Use `just test-lane`, `just
  cargo-lane`, or another isolated lane when Rust work is already active.
- Do not delete package caches, lane caches, or target directories while Rust
  jobs may be running. Use the repository's target diagnostics and pruning
  recipes.
- Regenerate schemas, snapshots, locks, vendored content, and other generated
  outputs through their owning workflow. When dependencies change, let Cargo
  refresh `Cargo.lock` and include it in the same change.
- Preserve public CLI flags, app-server and protocol contracts, configuration
  loading, sandbox behavior, stored-session and rollout compatibility, and
  installed-binary behavior unless the accepted task changes that surface.
- Rust source changes are not visible in Codex Desktop until the root local
  publish and restart proof succeeds.

## Validation delta

- Choose the smallest focused proof identified by the
  [validation routes](../SOURCEMAP.md#validation-routes). Use the documented
  check-only schema routes unless the task intentionally changes a generated
  contract.
- Keep generated diffs tied to their source change and inspect them before
  completion. Documentation-only changes require a focused diff check and must
  not be reported as runtime proof.

## Rust test conventions

- New Rust test modules should usually use sibling `*_tests.rs` files wired with
  `#[path = "..."] mod tests;`.
- Prefer whole-object `assert_eq!` comparisons and existing
  `pretty_assertions::assert_eq` patterns over field-by-field assertions.
- Avoid mutating process environment when a passed dependency or flag is
  practical.
- Use `codex_utils_cargo_bin::cargo_bin` and `find_resource!` for first-party
  binaries and resources.
