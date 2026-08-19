# codex-rs Rust workspace policy

## Scope and routing

- Read the nearest nested `AGENTS.md`. Use
  [`../SOURCEMAP.md`](../SOURCEMAP.md) for Rust owners, generated contracts,
  validation routes, and workflow references; keep crate rules scoped and
  architecture/usage background in READMEs.

## Workspace invariants

- Prefer existing crate boundaries, helpers, and local patterns before adding a
  new abstraction or moving behavior into `codex-core`.
- Do not run normal Cargo/`just test`/`just fix` jobs concurrently against the
  shared target. Use isolated lanes when Rust work is active, and never delete
  caches or targets while jobs may be running.
- Regenerate schemas, snapshots, locks, vendored content, and other generated
  outputs through their owning workflow. When dependencies change, let Cargo
  refresh `Cargo.lock` and include it in the same change.
- Preserve public CLI flags, app-server and protocol contracts, configuration
  loading, sandbox behavior, stored-session and rollout compatibility, and
  installed-binary behavior unless the accepted task changes that surface.
- Rust changes are not Desktop-visible until local publish and restart succeed.

## Validation delta

- Use the smallest documented [validation route](../SOURCEMAP.md#validation-routes)
  and check-only schema route unless changing a generated contract. Inspect
  generated diffs; documentation checks are not runtime proof.

## Rust test conventions

- Prefer sibling `*_tests.rs` modules wired with `#[path = "..."] mod tests;`.
- Prefer whole-object `assert_eq!` comparisons and existing
  `pretty_assertions::assert_eq` patterns over field-by-field assertions.
- Avoid mutating process environment when a passed dependency or flag is
  practical.
- Use `codex_utils_cargo_bin::cargo_bin` and `find_resource!` for first-party
  binaries and resources.
