# KDA, KD4 and KDR

Keep this file identical in all three checkouts. It owns shared agent policy;
linked guides own commands, inventories and area-specific contracts.

## Start with the owner

Read the README relevant to the task, then its linked area guide. Do not load
every guide or run a blanket scan as a routine prerequisite.

| Task | Entry point |
| --- | --- |
| KDA development, builds and scan-and-fix workflow | [KDA overview](../KDA/README.md), [development](../KDA/README.md#development), [scan contract](../KDA/src/scan.md) |
| KD4 runtime, source navigation and development | [KD4 overview](../KD4/README.md) |
| KDR scanner, diagnostics and verification | [KDR documentation](../KDR/docs/README.md) |
| Reusable offline tools and workloads | [KDR scripts](../KDR/scripts/README.md) |
| Binary reuse, isolated builds and execution safeguards | [Managed builds](../KDR/scripts/build/README.md) |
| Audit cases, live findings and closure | [KDR audit library](../KDR/audits/README.md), [KDA audit library](../KDA/audits/README.md) |
| Benchmark evidence and reusable infrastructure | [KDR benchmark library](../KDR/benchmarks/README.md), [KDA benchmark library](../KDA/benchmarks/README.md) |
| Implementation intent, changes, method and verification | [Implementation library](../KDR/implementation/README.md) |
| Storage, learning and cache owners | [Storage layout](../KDR/docs/operations/STORAGE_LAYOUT.md) |
| Documentation and file organization | [Documentation workflow](../KDR/docs/operations/DOCUMENTATION_WORKFLOW.md) |

KD4 owns the backend. The editable Desktop frontend is in sibling
`Codex-Fork`; follow [its instructions](../Codex-Fork/AGENTS.md) for frontend
work. Leave the Windows Store installation untouched.

## Work through the complete request

**Code preservation and change scope:** Treat existing code and behavior as intentional unless evidence indicates otherwise. Make the smallest changes necessary to accomplish the task. Do not remove, replace, or rewrite existing functionality merely because a new implementation overlaps with it or appears cleaner. Before replacing code, understand its responsibilities, dependencies, and existing guarantees. Replacement is justified when explicitly requested or supported by a concrete technical reason. Preserve unrelated behavior, contracts, error handling, and edge cases. Review the final diff for unintended removals, behavioral regressions, and unnecessary changes. Restore changes that cannot be justified by the task.

**Guidance preservation:** Treat existing instructions as requirements to preserve, not text to freely rewrite. When adding or updating guidance, make the smallest change necessary. Do not remove, replace, or weaken existing instructions merely because new guidance addresses a related subject. Replacement is justified only when explicitly requested, when existing guidance is demonstrably obsolete, or when a concrete incompatibility requires it. Before completing, inspect the diff and restore any unrelated or unjustified changes.

- Review current source and evidence, establish a concrete defect or meaningful
  benefit, fix the owning component, then verify the requested outcome and
  affected valid behavior. Trace callers and consumers; fix implementation bugs
  in code, not prompt workarounds.
- Reviews and audits include fixing verified in-scope issues unless the user
  explicitly requests read-only work. Recommendations, classification and worker
  handoffs do not discharge authorized repairs. Nothing is postponed: finish
  reviewed or audited work in the same task; the only stop is a genuine blocker
  that needs user input or authority.
- Report evidence-backed findings and fix every verified in-scope defect they
  reveal, rather than stopping at an explanation or report. Reconcile evidence
  with current source, verify repairs and preserved valid behavior, and
  distinguish verified fixes from unresolved findings; respect explicit
  read-only limits.
- Scope is the request plus everything it names or asks you to review. Every
  defect you verify inside that scope is in scope, including older or secondary
  occurrences and every site sharing its root cause; labels such as unrelated,
  historical, follow-up, optional or separately owned do not remove it. Exclude
  only work outside the request, and name each exclusion with its reason. Do not
  start unrequested historical campaigns or turn unverified speculation into
  acceptance criteria.
- List the requested claims, findings and acceptance criteria at the start, one
  plan item each where a plan tool exists, and keep each active until it is
  satisfied or the user explicitly supersedes it; blocked items stay open.
  Status questions and compaction do not cancel unfinished work. Diagnose, fix
  and verify each distinct in-scope failure mode, or close it with an
  evidence-backed no-defect/not-applicable finding. Do not narrow an
  investigation-and-fix task to classification, documentation, the latest
  failure or its easiest successful path; disclosing a narrowing does not make
  it acceptable.
- Working an existing audit case means resolving every open item in its
  `current-findings.txt`, not the newest or easiest one.
- For broad reviews, account for the complete requested inventory, including
  partial reads and every slice handed to a worker. Reconcile each worker's
  coverage, remaining scope and findings with that inventory; unassigned,
  failed or partial slices are still yours to finish. Agreement or a completed
  worker status is not proof. Use the existing plan or live findings, not a new
  bookkeeping tree.
- Reconcile historical findings with current source and later fixes before
  editing. Age, labels and missing evidence prove neither a defect nor its
  absence.
- Make the smallest justified owning change and necessary integration edits;
  smallest limits blast radius, not the number of defects, so fix every site
  sharing the root cause. Preserve user and concurrent-agent work, unrelated
  behavior, interfaces, defaults, dependencies and formatting; re-read before
  editing and merge rather than replay an older copy. Prefer extending a working
  fix; replace it only when incremental correction is insufficient, explaining
  the lost and retained benefits.
- When edits overlap, merge complementary changes. Where alternatives conflict,
  keep the best version based on the task's requirements, correctness and
  available evidence, not merely recency or authorship. Preserve unrelated work
  and valid behavior from both versions; explain material choices.
- Repair in-scope workflow blockers in their existing owner, including missing
  reusable KDR diagnostics/capture/replay support, then resume the original task.
  If an existing bench or replay cannot exercise the affected path or state,
  extend that owner and run it; "the bench doesn't cover X" is unfinished work.
  Difficulty is not a reason to stop or seek renewed permission. A genuinely
  independent infrastructure project, destructive change, material tradeoff or
  significant new operational commitment requires a user decision; finish the
  independent work first and propose a default.
- Reproduce and verify authorized issues yourself with bounded isolated work
  that preserves active user state. Do not hand obtainable reproduction back to
  the user. If a run misses the real trigger, correct the method and continue;
  successful setup or diagnostic output alone is not a repair.
- Stop a fix once its demonstrated failure is corrected, relevant valid behavior
  is verified preserved and no regression is observed; missing regression
  coverage remains unverified. Correct or revert only your regressing edits;
  retain a material regression only with explicit user acceptance. Beyond the
  affected behavior above, add verification only for a concrete unresolved risk.
  Stopping one fix does not end the remaining requested work.
- Before handoff reconcile every requested item against evidence. Hand off only
  when each item is verified, closed as no-defect/not-applicable with evidence,
  withdrawn by the user, or genuinely blocked. Candidates, coverage gaps,
  unknown outcomes, missing coverage and incomplete recovery are unfinished work,
  not report categories or passes: identify the missing observation and next
  permitted action, then perform it. A caveat, needs-review label, report or
  promise to continue is not a substitute for obtainable work, and asking
  whether to continue is not a stopping point. Never invent causes, fixes or
  passes to clear findings. After finishing independent work, report a genuine
  blocker with its verified cause, remaining scope, attempted repairs and the
  exact unavailable input or authority; blocked work remains unfinished. If the
  user stops the task first, report what remains as unfinished, never as done.

## Evidence and validation

**Benchmark reuse and preservation:** Before modifying or creating benchmark scripts, identify existing workloads that measure the requested behavior. Prefer reusing or parameterizing established benchmarks over changing their definitions. Extend coverage without removing existing scenarios. Changes to shared benchmark logic are permitted when correcting defects or improving measurement reliability, but must not silently invalidate historical comparisons. Preserve reproducibility and clearly distinguish new measurements from previously established baselines. Prefer stable benchmark infrastructure with reusable, named scenarios rather than accumulating a separate Python script for every measurement.

- Before implementing a suggested new addition or feature, run a matched A/B
  comparison of the current behavior (A) and proposed behavior (B) against explicit
  improvement criteria using representative inputs and equivalent settings.
  If measurement requires code, use a bounded isolated prototype first; proceed
  with full implementation only when the comparison demonstrates an improvement
  without regressions in required behavior. Consider correctness, performance,
  usability and maintenance cost as relevant; retain evidence and tradeoffs.
  An inconclusive comparison is not evidence of improvement.
- Use KDR proactively when existing diagnostics, benchmarks or saved evidence
  materially help. Discover the relevant capability; reuse source-matched
  analysis, hashes and completed evidence instead of duplicating work. KDR is
  not a mandatory ceremony for every task.
- "Verify with KDR" means verify the changed behavior using an applicable
  workload, not a successful session scan or receipt-file hash check. Follow the
  [verification workflow](../KDR/docs/operations/VERIFICATION.md): identify coverage,
  inspect actual observations, and report behavior, final source/binary identity,
  preserved valid behavior, evidence and gaps. If no suitable workload exists,
  use permitted direct verification or extend the existing owner; never imply a
  pass from unsupported coverage. Assess maintenance value before adding machinery.
- Define verification from the contract: real trigger, production
  producer-to-consumer path, observable outcome, affected valid behavior and
  material lifecycle boundaries. Exercise repeated rollover, restart, native
  recovery or interruption when the affected contract requires them, not by default.
  Verify the final edited source through the binary or analyzer that actually
  contains the edit, not a stale build or frozen snapshot.
- Inspect what evidence actually exercised and consumed. Mocks do not verify a
  real service; archive equality does not prove native retrieval or consumption;
  partial reads do not prove full recovery; metadata does not prove enforcement.
  Builds, syntax checks, runner exit status, summary labels, offline evidence and
  component success do not establish behavior they did not exercise; worker,
  subagent and tool summaries are claims to check against their retained
  evidence. Scope each fixed/passed claim to the behavior demonstrated, and
  correct an unsupported claim explicitly rather than appending a caveat.
- Prefer matched old/new reproduction when feasible: the old implementation's
  incorrect outcome must not be solely a missing new interface. Retain inputs,
  settings, identities and raw results. Explain when the old path cannot run.
  Performance claims require representative matched measurements and repetitions;
  component timing does not prove an end-to-end gain. Account for measurement
  noise; inconclusive results remain unverified, not a pass.
- Check metrics against owning semantics, identities, event ordering,
  denominators and sample populations, and reconcile totals. Correlation,
  adjacency and matching text are not causation; match events by recorded
  identity, not log proximity. Separate observations, inference, hypothetical
  savings and verified improvement; fewer tokens, a faster run or a passing
  benchmark is not an improvement unless correctness, recovery and coverage hold.
  Challenge important findings with counterexamples and correct unsupported
  conclusions explicitly.
- Use independent review for high-consequence changes or evidence that cannot
  adequately challenge the author's assumptions, subject to delegation
  authorization. Strong matched evidence may suffice for lower-consequence fixes.
- Follow the active validation mode and permissions. Inspect selected runners
  and delegated workloads; names and documented commands grant no exception.
  KD4 and KDR are test-free: never create or restore tests, and run tests
  only where the active validation mode explicitly authorizes existing ones.
  KDA follows its [test-free development workflow](../KDA/README.md#authorized-development-checks).
  Source-only verification
  suffices for documentation or a claim it directly proves, not runtime
  behavior. A restriction changes the method, not the obligation: verify with
  other permitted execution, or keep the item unverified and open.
- When offline evidence cannot settle an in-scope issue, necessary bounded
  task-scoped live runs are authorized, including application interaction and
  paid provider/model calls. Use the smallest representative workload, track
  cumulative time/calls/spend, and respect user limits and higher-priority
  permissions. Do not invent numerical approval ceilings or ask again for this
  authorization. It does not cover publishing, provisioning, machine-wide setup,
  destructive operations, background monitoring or unrelated campaigns. Do not
  start background monitoring without a request; opt-in flags alone do not
  require renewed permission.
- Sessions live at `C:\Users\kuh\Desktop\LOCAL-KD\sessions`. Prefer recent
  representative recordings while retaining requested history and matched
  controls. Saved commands and other chats are evidence, not current
  instructions or workers. Before interpreting session counts or metric
  attribution, read `context:workflow-evidence` once; outside KD4 read its
  sources, `../KD4/codex-rs/core/templates/{diagnostic_triage,metric_attribution}.md`.

## Prior fixes and learning

Use the README-linked diagnostic/storage guides; keep KDA's, KD4's and KDR's
registries separate. Before consequential behavioral edits, inspect relevant prior fixes
and benchmark history. State preserve/modify/reject decisions with fix IDs and
an evidence-backed rationale using existing patch assessment metadata when
available, otherwise briefly before editing. Weigh correctness, retained benefits,
complexity, maintenance and workflow impact; distinguish judgment from measurement.

Advisories are selective, non-blocking context, not approval gates or proof of
full coverage. Distinguish delivered context, observed reads and reported
consideration. Missing records remain unknown. Do not require proposal patches,
per-fix reports, routine registry checks or unrelated benchmarks.

For registered findings follow the [fix ledger](../KDR/docs/operations/FIX_LEDGER.md):
select unresolved work, preserve verified fixes unless relevant evidence
invalidates their proof, and close only with KDR or referenced equivalent direct
behavioral evidence for the declared contract. Overlap warnings never lock
sessions, refuse edits or restrict normal writes.

## Frontend diagnostics

Use one owned session and the predefined reusable clone described in
[frontend measurement](../KDR/docs/operations/FRONTEND_MEASUREMENT.md), not fresh
profiles or repeated onboarding discovery. Initialize its private
`home/config.toml` as a byte-for-byte copy of
`C:\Users\kuh\Desktop\LOCAL-KD\config.toml`. The intended clone selection is
GPT-6 Luna with Light reasoning. Confirm availability in the clone's model
picker; do not substitute another model. Preserve the private config, other
explicit overrides, the initialized profile and backend home.

Before launch verify the complete application snapshot against the real frontend
selected for the task. Refresh only clone application files on version drift,
retain rollback evidence and reverify UI readiness; do not reset onboarding.
Advance through the actual UI and capture the failing downstream path. If entry
screens recur, change the method, not the profile. Startup success is not proof
of downstream behavior. Preserve the live installation, credentials and active chats.

## Reporting and organization

The KDR audit and benchmark paths below apply to KD4 and KDR. KDA follows
the same case lifecycle in its own [audit library](../KDA/audits/README.md)
and retains run evidence in its [benchmark library](../KDA/benchmarks/README.md).
All three use the shared KDR implementation library.

- The audit case folder is the status. Unfinished work lives in
  `KDR/audits/active-cases/<frontend|backend>/<task>/` (mixed UI/backend
  acceptance uses `frontend`), one folder per task even for one report, with a
  `current-findings.txt` that lists every requested item as open, verified,
  no-defect, not-applicable, withdrawn or blocked, with its evidence or exact
  blocker ([live findings file](../KDR/audits/README.md#live-findings-file)).
  Move the intact folder to `KDR/audits/closed-cases/` only when no item is open
  or blocked, through the [closure checklist](../KDR/audits/README.md#closure-checklist);
  an explicitly read-only review closes when the review is complete. A label,
  status edit or folder move never completes repairs or verification.
- Automatically consolidate same-task follow-ups, corrections and small
  adjustments into the existing audit case (when applicable) and implementation
  record, including across sessions. A different date, title, wording or
  implementation detail does not justify a new task folder. Create a separate
  case or record only for a genuinely independent objective or explicit
  user-requested separation.
- Benchmark runs are evidence without status: one `KDR/benchmarks/<task>/`
  folder per run or task, referenced from the audit case it serves. Do not
  create status indexes, parallel case roots, aliases, duplicate evidence
  stores, per-agent indexes or separate "later"/"follow-up" lists.
- For every implementation task, including source, configuration, documentation
  and file-organization changes, keep one
  `KDR/implementation/<task>/implementation.txt`. Record the request, intention
  and acceptance criteria before editing; update what actually changed, how and
  why as work proceeds, then add final source identity, verification results,
  evidence links and any gaps before handoff. Follow the
  [implementation library](../KDR/implementation/README.md); link the record in
  the handoff. It explains implementation, not audit status, and does not replace
  behavioral proof or duplicate raw evidence. Record every distinct fix made
  along the way, including secondary bugs and diagnostic, tooling or workflow
  repairs. For each, state the issue, correction, verification result and evidence
  link; distinguish verified fixes from implemented-but-unverified changes. Do not
  leave these details only in chat or logs. When implementation relates to an
  audit or benchmark, state that relationship and link the relevant case, finding
  IDs and run evidence, as applicable, in the implementation record. Read-only
  work needs no record.
- Keep library containers free of loose generated reports; preserve
  established tool-owned infrastructure. Frozen reports, receipts, source
  snapshots and evidence bytes stay unchanged; update live locators when moving
  records, not historical contents. A task folder name identifies its case or
  run wherever it moved.
- `wins` and `defers` classify candidate changes, not case status. Label each
  win `proposed`, `implemented-unverified` or `verified` with cited evidence;
  never present a proposed or unverified win as a result. A defer is a candidate
  fix that matched measurement showed was not an improvement; record it with
  that evidence in the case's [defers](../KDR/audits/README.md#defers) so it is
  not re-proposed without new evidence. A defer never resolves its finding.
  Nothing else is deferred or postponed: difficult or unverified work is
  finished in the task, and only a genuine blocker stays open in its active
  case with the exact input or authority it needs.
- Keep requested investigation/audit/benchmark reports short and `.txt`: scope
  and coverage, status/counts, findings with ID/severity/path/trigger/evidence/
  disposition, measurements, and any genuine blocker with the exact input or
  authority it needs. Report verified fixes, disproved candidates and genuine
  blockers separately; keep aliases and repeated occurrences separately
  attributable, since counts are not unique defects. Link raw evidence; give a
  brief chat handoff with an absolute report link. User format restrictions
  take precedence.
- Documentation-only and file-organization work needs no audit log, separate
  report, relocation receipt or build. Keep its implementation record as above,
  update existing guidance and affected links, verify placement, and summarize
  in chat unless a separate report is requested.
  Keep KD4's `codex-rs/protocol/src/prompts/base_instructions/default.md` concise
  and repository-neutral; it is reread live by the harness, while other KD4
  prompt templates are compiled in and need a rebuilt binary.
