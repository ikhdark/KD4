## Project context

- This checkout is the user's local fork of
  [`openai/codex`](https://github.com/openai/codex). Treat the active repository
  root as the checkout location and treat work as fork-local unless the user
  explicitly requests upstream, product-facing, or distribution-ready changes.
- Publishing the fork Codex Desktop sets a separate `CODEX_HOME` value of
  `C:\Users\kuh\Desktop\LOCAL-KD` for the active fork Desktop configuration and
  state. Use that environment variable rather than assuming a user-profile
  `.codex` directory or a machine-specific absolute path.
- [`SOURCEMAP.md`](SOURCEMAP.md) owns repository inventory, runtime entrypoints,
  package and Rust-domain routing, the complete `codex-rs` edit and upstream-sync
  classification, generated contracts, validation routes, and cross-cutting
  change routes. Consult it before changing or upstream-syncing Rust workspace
  paths.
- Known top-level scoped instruction files include `codex-rs/AGENTS.md` and
  `scripts/AGENTS.md`; further nested files apply only where present.
- `.codex/AGENTS.md` documents workspace routing, `.codex/config.toml` owns
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
runtime; do not inject unrelated role bodies. If experiencing agent issues,
fall back to the default agents.

## Operating defaults
* Do not publish unless the user explicitly asks.
* Keep work within the accepted task scope.
* When deleting or renaming a file, update task-relevant references, ownership
  records, manifests, generators, and documentation that would otherwise become
  incorrect.
* For bug checks, never invent findings, when you find a bug, assume there are more and continue until none are found, do not claim a bug simply because you think there is a "better" version, if the code is functional, do not touch it.
* Do not turn a directed fix into a broad fix.
* Avoid blindly overwriting other agents' work, Compare both versions and pick the better version or combine the best compatible behavior.
* When unsure, ask the user questions to collect intention and guidance.
* Do not over-engineer implementations or plans.
* When the user presents a file path or uploaded file, you are required to read the entire file.

## First-pass implementation discipline

- Before the first edit, establish a bounded closure set: the authoritative
  owner from `SOURCEMAP.md`, direct callers and constructors, the listed
  cross-cutting route, compatibility boundaries, and the smallest behavioral
  proof. Stop after those boundaries unless a public, wire, persisted, or
  generated contract requires expanding the set.
- Complete one coherent contract before starting the next: format or lint the
  changed surface, compile its owner, run the exact focused behavioral test, and
  confirm the filter selected at least one test. A successful command that ran
  zero relevant tests is not proof.
- In a shared workspace, inspect the task-relevant diff and reread the exact
  target region immediately before patching. If it changed, reconcile the
  current versions deliberately; do not retry a stale patch or apply duplicate
  patch blocks to the same file.

## Validation failure handling

- Run each task-scoped validation command at most once initially.
- If validation fails because of transient infrastructure—such as sccache failure, lock contention, or an interrupted build—retry at most twice using the existing warmed Cargo lane and an appropriate documented fallback.
- Do not create additional cold Cargo lanes merely to repeat the same validation.
- If the retry reaches unrelated pre-existing or concurrently introduced compilation errors, stop. Report:
  - which task-local validation passed;
  - the exact unrelated blocker;
  - which requested checks could not run.
- Do not repair unrelated compilation failures unless the user explicitly expands the task.
- Do not repeatedly rerun validation while other agents or processes are modifying the same source tree.
- After identifying the same blocking condition twice, perform only non-building checks such as targeted diff inspection, `rustfmt --check`, or `git diff --check`, then finish.
- A failed broad build does not invalidate narrower tests that already passed. Preserve and report those results.
- Do not restart a completed dependency build in a new target directory unless the prior artifacts are unusable and the retry is explicitly justified.

# Sessions

- "C:\Users\kuh\Desktop\LOCAL-KD\sessions" contains the rollouts for the fork which are to be used for auditing and diagnosis. Do not use the default "C:\Users\kuh\.codex\sessions" path.

## Validation

- Validation must stay task-scoped. Do not run full-suite or workspace-wide tests. Run only the narrowest tests that directly cover the changed behavior and its affected contract, unless told otherwise.


# Benchmarking

- Do not label unit-test duration, test-binary startup time, analytical action-count projections, stale compiled binaries, or source-level estimates as runtime benchmarks.
- "C:\Users\kuh\Desktop\LOCAL-KD\backups" contains previous fork binaries that can be used for A/B comparisons of the previous version to the current version.
