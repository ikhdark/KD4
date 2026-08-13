## Project context

- This checkout is the user's local fork of
  [`openai/codex`](https://github.com/openai/codex). Treat the active repository
  root as the checkout location and treat work as fork-local unless the user
  explicitly requests upstream, product-facing, or distribution-ready changes.
- Codex Desktop sets `CODEX_HOME` for the active Desktop configuration and
  state. Use that environment variable rather than assuming a user-profile
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

## Instruction scope

Read the closest relevant `AGENTS.md`. A nearer file augments this policy and
takes precedence inside its subtree. Never rely on an instruction file that is
absent from the working tree.

When the user presents a file path, read the entire file before changing it.

Use ownership maps and source-of-truth documents to identify the relevant local
owner. Do not expand into unrelated ownership documents after the required owner
and affected routes are clear.

## Delegated and durable workflows

Root sessions do not need inactive role procedures. When delegation, durable
artifacts, or the architect-driven implementation lane is actually selected,
load [`.codex/harness/workflow.md`](.codex/harness/workflow.md). Child agents
receive their selected role policy plus the compact shared rules supplied by the
runtime; do not inject unrelated role bodies.

## Operating defaults

Use the fastest safe path that completes the accepted request correctly.

* Keep work within the accepted task scope. Read or modify files outside the
  initial target only when they define, reference, generate, depend on, validate,
  or are directly affected by the requested change.
* Before implementing, identify the owner, affected behavior and callers,
  compatibility or generated-output risks, and validation route. Stop exploring
  once those are clear unless new evidence expands the scope.
* When deleting or renaming a file, update task-relevant references, ownership
  records, manifests, generators, and documentation that would otherwise become
  incorrect.
* You may make directly related implementation improvements when repository
  evidence supports them and they do not materially expand the requested change.
  Do not use a directed task as justification for broad cleanup or redesign.
* Preserve unrelated concurrent work. When task-relevant versions compete,
  compare them once and keep or combine the best compatible behavior.
* Do not loop on unchanged checks, repeat searches without a new question, or
  repeatedly revisit completed work.
* For audits or bug checks, survey the accepted scope; classify unresolved
  candidates and never invent findings. Do not turn a directed fix into a broad
  fix.
* Fix validation, test, or implementation blockers caused by the change or
  necessary to complete the requested task. For unrelated, pre-existing, or
  concurrently introduced blockers, avoid overwriting other agents' work; record
  the blocker and continue where safely possible.
* Ask the user only when a material requirement cannot be discovered safely.

## Validation and local-build proof

- Rust crates: follow `codex-rs/AGENTS.md` and the closest crate-specific
  guidance. Use the smallest focused test or check that proves the changed
  contract before broader validation.
- Scripts: follow `scripts/AGENTS.md` and use the closest syntax, unit, dry-run,
  or policy test for the edited script.
- Do not publish unless the user explicitly asks.
- Regenerate owned artifacts through their documented generator; do not hand-edit
  generated locks or generated protocol/schema outputs.
- Prefer targeted tests; avoid full crate or workspace runs unless asked.
- Tooling success alone does not prove a behavior or runtime fix. Completion
  requires the focused failing test or approved final gate to pass, and
  desktop-visible changes also require the local publish/restart proof.
