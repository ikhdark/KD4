---
name: no-overlapping
description: Analyze KD4 implementation phases, audit items, plans, trackers, dirty worktrees, branches, and owner paths to determine which work does not overlap and can run safely in parallel. Use when the user asks which phases, items, tasks, Codex threads, branches, or worktrees can run at once; whether workstreams conflict; how to split concurrent implementation; what another task has completed; or to create, update, or reconcile a phase/item tracker with ownership, dependencies, proof state, and parallel-ready or blocked classifications. Inspect the actual source, worktree, and shared tracker before deciding. Do not use for generic harness creation, ordinary implementation, or planning without a concurrency or conflict question.
---

# No Overlapping

Build evidence-backed parallel work maps for KD4 without weakening implementation,
validation, or dirty-worktree policy.

## Required Reading

1. Read the root `AGENTS.md` and `.codex/AGENTS.md`.
2. Read the user-provided report, plan, or phase list and the current canonical
   tracker, if one exists.
3. Read the closest owner `AGENTS.md` only for areas needed to classify overlap.
4. When creating or updating `.codex/harness` state, also read
   `.codex/harness/README.md` and `.codex/harness/workflow.md`.

## Ground The Current State

1. Reload the canonical tracker immediately before analysis. Other Codex tasks
   may have updated it since the last turn.
2. Inspect focused repository evidence with `git status --short`, changed-path
   or diff summaries, and recent task evidence when relevant.
3. Treat dirty paths as evidence of active ownership, not proof of completion.
4. Preserve unrelated changes and do not modify product code during a read-only
   overlap analysis.
5. Distinguish user-reported completion, implemented code, and verified work.

## Map Work Ownership

For every candidate phase or item, record the smallest useful set of:

- stable phase/item ID and short objective;
- primary owner files or subsystem;
- shared types, protocols, registries, generated artifacts, or configuration;
- ordering dependencies and replaced or competing runtime paths;
- focused tests, generators, or validation resources that could collide.

Do not declare work independent merely because its named files differ. Shared
interfaces, schemas, state machines, generated outputs, validation baselines,
and downstream dependencies also count as overlap.

## Classify Concurrency

Use these classifications:

- `parallel_ready`: no shared write owner, contract, generated artifact,
  validation resource, or unfinished dependency was found.
- `coordinate`: work can proceed only with an explicit file/owner split and a
  named integration order.
- `blocked`: an unfinished dependency or competing owner makes concurrent work
  unsafe.
- `unknown`: current evidence is insufficient; inspect further or ask the
  active implementer.

Prefer item-level classifications when any part of a phase overlaps. Call an
entire phase parallel-ready only when every included item qualifies.

## Report Or Update The Tracker

- For a read-only question, report the safe workstreams, blocked work, and exact
  overlap reasons without editing files.
- Create or mutate tracker state only when the user asks to create, update,
  reconcile, or maintain it.
- When a canonical tracker exists, keep one writer responsible for status
  changes and preserve its existing status vocabulary.
- Never promote dirty or compiling code to `verified`; require the audit item's
  stated proof or the nearest sufficient owner validation.
- A phase is complete only when every item is verified or intentionally skipped
  with a reason. Keep user-confirmed completion visibly distinct when receipts
  are unavailable.
- Record which item IDs a parallel task owns before implementation begins.

## Concurrent Implementation Guardrails

- Refresh the tracker and focused dirty-path map immediately before assigning
  new work.
- Give each task a bounded set of item IDs, owner paths, and evidence targets.
- Do not start a downstream timing, protocol, schema, persistence, or generated
  artifact item until its upstream owner is stable and proven.
- Reconcile shared tracker updates after another task reports progress.
- Do not create tasks, branches, worktrees, commits, or subagents unless the
  user explicitly requests them.
- Do not implement the candidate work merely because it is parallel-ready; the
  user's overlap question authorizes analysis, not implementation.

## Output

Lead with the actionable split:

1. work that can start now beside the active item or phase;
2. work that must wait or requires coordination;
3. the tracker/worktree evidence supporting the classification;
4. any status uncertainty that prevents a stronger claim.

Keep simple answers compact. Use a table only when comparing several candidate
items or owners materially improves clarity.
