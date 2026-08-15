## Project context

- This checkout is the user's local fork of
  [`openai/codex`](https://github.com/openai/codex). Treat the active repository
  root as the checkout location and treat work as fork-local unless the user
  explicitly requests upstream, product-facing, or distribution-ready changes.
- Publishing the fork Codex Desktop sets a seperate `CODEX_HOME` which is "C:\Users\kuh\Desktop\LOCAL-KD" for the active fork Desktop configuration and state. Use that environment variable rather than assuming a user-profile
  `.codex` directory or a machine-specific absolute path.
- [`SOURCEMAP.md`](SOURCEMAP.md) owns repository inventory, runtime entrypoints,
  package and Rust-domain routing, generated contracts, validation routes, and
  cross-cutting change routes.
- [`do-not-code.md`](do-not-code.md) owns the complete `codex-rs` top-level
  classification, exact upstream-mirror paths, and generator-managed exceptions.
  Consult it before changing or upstream-syncing Rust workspace paths.
- Known top-level scoped instruction files include `codex-rs/AGENTS.md` and
  `scripts/AGENTS.md`; further nested files apply only where present.
- `.codex/README.md` documents workspace routing, `.codex/config.toml` owns
  optional repo-local runtime configuration, and `.codex/skills` owns fork-local
  skills and validation workflows.

## Desktop app boundary

- The repository contains the Rust CLI and app-server components used by Codex
  Desktop, but not the native Windows desktop shell source.
- Source edits here do not hot-apply to the installed app. Desktop-visible
  completion requires rebuilding and updating or replacing the local binary,
  then restarting the Desktop app.

## Delegated and durable workflows

Root sessions do not need inactive role procedures. When delegation, durable
artifacts, or the architect-driven implementation lane is actually selected,
load [`.codex/harness/workflow.md`](.codex/harness/workflow.md). Child agents
receive their selected role policy plus the compact shared rules supplied by the
runtime; do not inject unrelated role bodies. If experiancing agent issues, fall back to the default agents.

## Operating defaults
* Do not publish unless the user explicitly asks.
* Keep work within the accepted task scope.
* When deleting or renaming a file, update task-relevant references, ownership
  records, manifests, generators, and documentation that would otherwise become
  incorrect.
* For bug checks, never invent findings, when you find a bug, assume there are more and continue until none are found, do not claim a bug simply because you think there is a "better" verison, if the code is functional, do not touch it. 
* Do not turn a directed fix into a broad fix.
* Avoid blindly overwriting other agents' work, Compare both verisons and pick the better verison or combine the best compatible behavior.
* When unsure, ask the user questions to collect intention and guidance.
* Do not over-engineer implementations or plans.
* When the user presents a file path or uploaded file, you are required to read the entire file.

# Sessions

- "C:\Users\kuh\Desktop\LOCAL-KD\sessions" contains the rollouts for the fork which are to be used for auditing and diagnosis. Do not use the default "C:\Users\kuh\.codex\sessions" path.

## Validation

- Validation must stay task-scoped. Do not run full-suite or workspace-wide tests. Run only the narrowest tests that directly cover the changed behavior and its affected contract, unless told otherwise.


# Benchmarking

- Do not label unit-test duration, test-binary startup time, analytical action-count projections, stale compiled binaries, or source-level estimates as runtime benchmarks.
- "C:\Users\kuh\Desktop\LOCAL-KD\backups" contains previous fork binarys that can be used for the A/B comparisons of the previous version to the current verison.
