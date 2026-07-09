# Quality Gates Reference

Use this reference when selecting validation or finishing a harnessed task.

## General Gate

Every implementation claim needs evidence from the relevant owner path. The
nearest sufficient proof is preferred over broad unrelated checks.

## Implementation Discipline Gate

For implementation, debugging, refactoring, integration, migration, or
repo-behavior work, apply
`.codex/skills/kd4-crosscheck-and-finish/SKILL.md` before finish claims. The
harness can record planning and evidence, but that skill owns the crosscheck,
full-path implementation, validation, and reporting standard.

## Wiring Proof Gate

For implementation changes, use Wiring Guard/KDWG as the static reachability
proof layer when the plugin is active. Declare intent before edits, then run the
check before finish claims. Use `--no-wiring-targets` only for docs, templates,
planning, or config-only changes with no runtime call-site wiring target.

When Wiring Guard/KDWG, `wire-implementations`, or static wiring proof is
explicitly in scope for a KD4 implementation task, treat the KD4 harness as
triggered in lightweight mode. The harness records or reports the evidence
expectations; Wiring Guard remains the static reachability proof.

## Docs And Skill Changes

Use:

- focused diff review;
- `git diff --check` for whitespace and patch hygiene;
- targeted read-back of changed files when useful.

## Rust Changes

Use focused checks or tests from the relevant crate under `codex-rs`. Prefer
local `just` recipes when they match the touched surface.

## Schema Or Protocol Changes

Identify the owning contract before editing implementation code. Use the owning
schema check or generator. Do not hand-edit generated schema files.

## Script Changes

Run syntax checks and the closest script tests when available. Do not hand-edit
generated locks.

## Publish Or Desktop Changes

For local publish paths, use the KD4 local publish recipes. Desktop-visible
claims require:

- local binary replacement or publish proof;
- Codex Desktop restart when required;
- process path/hash or equivalent runtime-chain evidence;
- user-visible screenshot or equivalent runtime proof when relevant.

## Final Answer Gate

The final answer should not overstate validation. If checks were skipped, name
the skipped check and why.

## Eval Gate

Use `.codex/harness/templates/EVAL.md` when a workflow or skill change can
regress. Prefer command or rule graders. Use manual or model graders only when a
deterministic check cannot judge the outcome.

## Harness Audit Gate

Use `.codex/harness/templates/HARNESS_AUDIT.md` when the user asks whether the
harness is reliable, too costly, too broad, or ready for more automation. Score
only categories backed by repository evidence.
