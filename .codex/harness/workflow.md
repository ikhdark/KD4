# KD4 Harness Workflow

The KD4 harness is an optional durable-artifact layer for substantial work in
this fork. It preserves plans, decisions, evals, evidence, audits, and handoffs
without redefining repository implementation policy.

## Ownership Boundaries

- Root `AGENTS.md` owns repository inspection, implementation discipline,
  validation selection, completion status, and final reporting.
- `.codex/harness` owns optional durable task artifacts and lifecycle guidance.
- This file owns delegated-role and architect-lane procedure; load it only when
  such a workflow is active.
- Give every mutating root task and subagent a durable identity. Path and
  named-contract claims are diagnostic coordination metadata and may overlap.

## Phase 1: Intake

1. Confirm the concrete objective and choose the smallest sufficient workflow.
2. Identify the owner directory, nearest scoped instructions, and validation
   route.
3. Decide whether durable state will materially help.

Use durable artifacts for broad, risky, interrupted, resumable, explicitly
auditable, or multi-agent work. For a focused one-turn task, keep the workflow
in conversation and create no run directory.

## Phase 2: Preflight and Plan

Create `PLAN.md` only when durable planning is useful. Capture the objective,
non-goals, owner scope, validation intent, risks, and a short milestone list.
Add `EVAL.md` before implementation when capability or regression criteria need
to survive later turns.

For concurrent writers or validation lanes, copy `templates/PREFLIGHT.json` and
resolve it with `just workflow-preflight <manifest> <receipt>`. The
preflight publishes the receipt into the repository's locked active-receipt
registry and checks every registered receipt atomically. Use
`just workflow-preflight-release <assignment-id>` when the assignment becomes
terminal. Receipts are leases (one hour by default); long-running assignments
must rerun the same preflight before expiry to renew them, or pass a bounded
`--lease-seconds` value to the script. Expired and legacy non-lease receipts are
removed under the registry lock so stale advisories do not accumulate forever.
The preflight must name the assignment and root task, starting
revision, path and contract claims, dependencies, generated-output owner,
validation owner, exact validation commands, Cargo lane, and shared/isolated
workspace strategy.

Path, contract, and Cargo-lane overlap is returned in the resolved receipt's
`advisories` array. Use isolated worktrees when separation is useful, but overlap
does not block shared-worktree execution.

## Phase 3: Implement

Follow root `AGENTS.md` and any active specialist skill. The harness may record
implementation decisions in `IMPLEMENT.md`, but that artifact does not replace
owner-path inspection or task-scoped validation.

Keep unrelated dirty changes intact. Keep generated output under its owning
workflow. Do not add logs, screenshots, binaries, or large transcripts to
reviewable changes unless requested.

Use supporting reads and current file state to reduce accidental overwrites.
Freshness and ownership mismatches become review risk; they do not reject writes.

## Phase 4: Check

Run the nearest sufficient proof required by root `AGENTS.md`, then record only
the evidence that matters for resumption or audit. Name skipped checks and their
reasons.

Validation is check-only and bound to the revision and covered path/contract
manifest. A relevant mutation supersedes the result. Generated-output
regeneration is a separate, explicitly owner-attributed command serialized by
the repository generation lock.

One workspace epoch that supersedes several proofs counts as one stale event.
After the first event, reconcile once and run one targeted validation. Repeated
staleness pauses the task for root and offers an isolated-worktree restart
instead of beginning another validation loop.

Use the completion-gate status definitions below and the completion discipline
from root `AGENTS.md`. Use `QA_CHECKLIST.md` for broad verification and
`HARNESS_AUDIT.md` for harness-policy or skill changes.

### Completion Gate Status

- `passed`: the objective is implemented, the intended runtime path is wired,
  and the nearest sufficient validation passed with no known task-relevant
  defect remaining.
- `partial`: a useful subset is complete, but an explicitly identified part of
  the accepted scope or its required proof remains unfinished.
- `blocked`: completion cannot proceed without a named external state change,
  authority, dependency, or user decision; the blocker and completed evidence
  are recorded.

## Phase 5: Finish

Summarize the material changes, focused validation, and remaining risk. Write
`HANDOFF.md` before stopping only when unresolved work or important context must
survive. Release any active preflight receipt after its assignment is terminal.

## Optional Multi-Agent Mode

Use `ORCHESTRATOR.md` when multi-agent work is active. Give each agent a bounded
task, durable identity, claim set, and evidence target. Name one owner for each
complete behavioral contract and one owner for final validation; overlaps remain
visible as risk metadata. Before root completion, linked assignments,
validations, and gates must be terminal. Root
completion rechecks sealed receipt evidence so later relevant drift remains a
blocker; unrelated task roots only warn and do not join this barrier.

Investigation agents remain read-only, load root and nearest scoped
instructions, inspect the smallest owner/caller/test/contract surface, separate
evidence from inference, and report dependencies, validation implications, and
a stop condition. Implementation agents reinspect their focused diff before
editing, preserve unrelated work, stop on competing ownership or unfinished
dependencies, and report changed paths, validation, runtime wiring, and risk.
Subagents do not mutate shared harness state or stage, commit, push, or publish.

### Architect-Driven Implementation Lane

For risky work selected under root `AGENTS.md`, use `explorer` as the
read-only contract architect, then copy its completed
`KD4_ARCHITECT_CONTRACT_V1` JSON assignment block into a dependent `worker`
assignment. Preserve every stable obligation ID and copied typed field exactly.
If the receipt is ambiguous or cannot be copied without interpretation, the
coordinator treats the architect assignment as incomplete and does not spawn the
coder. Bind the reviewer and verifier to the coder as their sole evaluation
target, with both architect and coder assignments as dependencies.

The store and runtime enforce active-assignment lifecycle, root-only task control,
independent-review read-only boundaries, successful sealed dependencies, and
cleared gates. Path and named-contract claims are advisory. The receipt format,
transcription fidelity, exact
obligation-ID comparison, and refusal to complete with unresolved copied
obligations are coordinator-policy checks. They are not store validation. The
coder's copied typed assignment is authoritative for review and verification;
tests and other validation remain supporting evidence rather than proof of
completeness. Drift-proof receipt-to-assignment binding would require a future
Rust change and is outside this workflow.
