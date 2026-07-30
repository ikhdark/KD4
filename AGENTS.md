## Project context

- This checkout is the user's local fork of
  [`openai/codex`](https://github.com/openai/codex) at
  `C:\Users\kuh\Desktop\kd4`. Treat work as fork-local unless the user
  explicitly requests upstream, product-facing, or distribution-ready changes.
- [`SOURCEMAP.md`](SOURCEMAP.md) owns repository inventory, runtime entrypoints,
  package and Rust-domain routing, generated contracts, validation routes, and
  cross-cutting change routes.
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

Budget is not a constraint. Do not reduce investigation quality, skip code reading
needed for correctness, or stop early solely to conserve tokens, context, commands,
or compute.

Read the closest relevant `AGENTS.md`. A nearer file augments this policy and
takes precedence inside its subtree. Never rely on an instruction file that is
absent from the working tree.

When the user presents a file path, read the entire file before changing it.

Use ownership maps and source-of-truth documents to identify the relevant local
owner. Do not expand into unrelated ownership documents after the required owner
and affected routes are clear.

## Operating defaults

Use the fastest safe investigation, implementation, communication, and validation
path that completes the request correctly.

* Keep work within the accepted task scope. Read or modify files outside the
  initial target only when they define, reference, generate, depend on, validate,
  or are directly affected by the requested change.
* Before implementing, gather enough evidence to identify:

  * the owning code or contract;
  * the files and behavior directly affected;
  * important callers, dependents, generated outputs, or compatibility risks;
  * the appropriate validation route.
    Do not continue exploring after these are sufficiently established unless new
    evidence expands the required scope.
* When deleting or renaming a file, update task-relevant references, ownership
  records, manifests, generators, and documentation that would otherwise become
  incorrect.
* You may make directly related implementation improvements when repository
  evidence supports them and they do not materially expand the requested change.
  Do not use a directed task as justification for broad cleanup or redesign.
* Overlapping edits from other agents are expected. Preserve unrelated concurrent
  work. When task-relevant versions compete, compare them once and keep or combine
  the best compatible behavior. Do not repeatedly revisit the same conflict
  without new evidence.
* Read-only agents may be used when they make a broad or multi-file investigation
  faster, clearer, or easier to divide, but they are not required.
* Do not loop on unchanged checks, repeat searches without a new question, or
  repeatedly revisit completed work.
* When checking for bugs, do not stop merely because the first bug was found.
  Survey the accepted scope until relevant candidates are confirmed, rejected,
  identified as duplicates, deferred with a concrete missing fact, blocked,
  classified as out of scope, or judged disproportionately expensive relative
  to their likely value. State the reason for any unresolved task-relevant
  candidate. Never invent or pad findings to satisfy an expected count.
* Do not turn a directed fix into a broad fix.
* After implementation and validation, perform one final self-review of the diff,
  affected contracts, and validation results. Correct any task-relevant defect
  found during that review. Do not repeat unchanged validation solely to satisfy
  this requirement.
* Fix validation, test, or implementation blockers caused by the change or
  necessary to complete the requested task. For unrelated, pre-existing, or
  concurrently introduced blockers, avoid overwriting other agents' work; record
  the blocker and continue where safely possible.
* Ask the user a question when you need clarification, or more context to proper implement.

## Validation and local-build proof

- Rust crates: follow `codex-rs/AGENTS.md` and the closest crate-specific
  guidance. Use the smallest focused test or check that proves the changed
  contract before broader validation.
- Scripts: follow `scripts/AGENTS.md` and use the closest syntax, unit, dry-run,
  or policy test for the edited script.
- Local publish: use the supported local publish and restart proof chain before
  claiming that Codex Desktop is running the changed Rust behavior.
- Regenerate owned artifacts through their documented generator; do not hand-edit
  generated locks or generated protocol/schema outputs.
- Tooling success alone does not prove a behavior or runtime fix. Completion
  requires the focused failing test or approved final gate to pass, and
  desktop-visible changes also require the local publish/restart proof.
