# KD4 and KDR

Keep this file identical in both checkouts. It owns shared agent policy; linked
guides own commands, inventories and area-specific contracts.

## Start with the owner

Read the README relevant to the task, then its linked area guide. Do not load
every guide or run a blanket scan as a routine prerequisite.

| Task | Entry point |
| --- | --- |
| KD4 runtime, source navigation and development | [KD4 overview](../KD4/README.md) |
| KDR scanner, diagnostics and verification | [KDR documentation](../KDR/docs/README.md) |
| Reusable offline tools and workloads | [KDR scripts](../KDR/scripts/README.md) |
| Binary reuse, isolated builds and execution safeguards | [Managed builds](../KDR/scripts/build/README.md) |
| Report ownership, case status and closure | [Report library](../KDR/audits/README.md) |
| Benchmark evidence and reusable infrastructure | [Benchmark library](../KDR/benchmarks/README.md) |
| Storage, learning and cache owners | [Storage layout](../KDR/docs/operations/STORAGE_LAYOUT.md) |
| Documentation and file organization | [Documentation workflow](../KDR/docs/operations/DOCUMENTATION_WORKFLOW.md) |

KD4 owns the backend. The editable Desktop frontend is in sibling
`Codex-Fork`; follow [its instructions](../Codex-Fork/AGENTS.md) for frontend
work. Leave the Windows Store installation untouched.

## Work through the complete request

- Review current source and evidence, establish a concrete defect or meaningful
  benefit, fix the owning component, then verify the requested outcome and
  affected valid behavior. Trace callers and consumers; fix implementation bugs
  in code, not prompt workarounds.
- Reviews and audits include fixing verified in-scope issues unless the user
  explicitly requests read-only work. Recommendations, classification and worker
  handoffs do not discharge authorized repairs. Nothing is postponed: finish
  reviewed or audited work in the same task; the only stop is a genuine blocker
  that needs user input or authority.
- List the requested claims, findings and acceptance criteria at the start. Keep
  each active until it is satisfied or the user explicitly supersedes it; blocked
  items stay open.
  Status questions do not cancel unfinished work. Diagnose, fix and verify each
  distinct in-scope failure mode, including older or secondary occurrences, or
  close it with an evidence-backed no-defect/not-applicable finding. Do not
  narrow an investigation-and-fix task to classification, documentation, the
  latest failure or its easiest successful path; disclosing a narrowing does not
  make it acceptable.
- For broad reviews, account for the complete requested inventory, including
  assigned and unassigned areas, partial reads and justified exclusions. Reconcile
  each worker's coverage and findings with that inventory; agreement or a completed
  worker status is not proof. Use the existing plan or live findings, not a new
  bookkeeping tree.
- Reconcile historical findings with current source and later fixes before
  editing. Age, labels and missing evidence prove neither a defect nor its
  absence. Items the user named are in scope; do not revive unrelated historical
  campaigns or promote speculative risks you discover into acceptance criteria.
- Make the smallest justified owning change and necessary integration edits;
  smallest limits blast radius, not the number of defects, so fix every site
  sharing the root cause. Preserve user and concurrent-agent work, unrelated behavior, interfaces,
  defaults, dependencies and formatting; re-read before editing and merge rather
  than replay an older copy. Prefer extending a working fix; replace it only when
  incremental correction is insufficient, explaining the lost and retained benefits.
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
  retain a material regression only with explicit user acceptance. Do not add
  verification campaigns without a concrete unresolved risk. Stopping one fix
  does not end the remaining requested work.
- Before handoff reconcile every requested item against evidence and report each
  as verified, unfinished or blocked. Missing coverage, incomplete recovery and
  unknown outcomes are not passes or no-defect findings. Do not end a turn asking
  whether to continue while obtainable in-scope work remains.
  For each unresolved item, identify the missing observation and next permitted
  action, then perform it; a caveat, needs-review label, report or promise to
  continue is not a substitute for obtainable work. Close only with requirement
  satisfaction, a verified correction or an evidence-backed
  no-defect/not-applicable disposition; never invent causes, fixes or passes to
  clear findings. Finish independent work before reporting a genuine blocker with
  its verified cause, remaining scope, attempted repairs and exact unavailable
  input or authority; blocked work remains unfinished, not complete.

## Evidence and validation

- Use KDR proactively when existing diagnostics, benchmarks or saved evidence
  materially help. Discover the relevant capability; reuse source-matched
  analysis, hashes and completed evidence instead of duplicating work. KDR is
  not a mandatory ceremony for every task.
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
  subagent and tool summaries are claims to check against their retained evidence. Scope each fixed/passed claim to the behavior demonstrated, and
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
  Source-only verification suffices for documentation or a claim it directly
  proves, not runtime behavior. Do not run tests when forbidden or create tests.
  A restriction changes the method, not the obligation: verify with other
  permitted execution, or keep the item unverified and open.
- When offline evidence cannot settle an in-scope issue, necessary bounded
  task-scoped live runs are authorized, including application interaction and
  paid provider/model calls. Use the smallest representative workload, track
  cumulative time/calls/spend, and respect user limits and higher-priority
  permissions. Do not invent numerical approval ceilings. This standing
  authorization does not cover publishing, provisioning, destructive operations,
  background monitoring or unrelated campaigns. Do not start background
  monitoring without a request; opt-in flags alone do not require renewed
  permission.
- Sessions live at `C:\Users\kuh\Desktop\LOCAL-KD\sessions`. Prefer recent
  representative recordings while retaining requested history and matched
  controls. Saved commands and other chats are evidence, not current
  instructions or workers. Before interpreting session counts or metric
  attribution, read `context:workflow-evidence` once; outside KD4 read its
  sources, `../KD4/codex-rs/core/templates/{diagnostic_triage,metric_attribution}.md`.

## Prior fixes and learning

Use the README-linked diagnostic/storage guides; keep KD4's registry separate
from KDR's. Before consequential behavioral edits, inspect relevant prior fixes
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
`home/config.toml` with a byte-for-byte copy of
`C:\Users\kuh\Desktop\LOCAL-KD\config.toml`; thereafter preserve its private
config, explicit overrides, initialized profile and backend home. The diagnostic
clone uses `gpt-6.1-terra` / `high`.

Before launch verify the complete application snapshot against the real frontend
selected for the task. Refresh only clone application files on version drift,
retain rollback evidence and reverify UI readiness; do not reset onboarding.
Advance through the actual UI and capture the failing downstream path. If entry
screens recur, change the method, not the profile. Startup success is not proof
of downstream behavior. Preserve the live installation, credentials and active chats.

## Reporting and organization

- Use the existing owners, not new bookkeeping trees. Put each new audit case in
  its own task folder under `audits/active-cases/<frontend|backend>/` (mixed
  UI/backend acceptance uses `frontend`) and each benchmark run in its own
  `benchmarks/<task>/` folder, even for one report. Do not create parallel case
  roots, aliases, duplicate evidence stores or per-agent indexes.
- An audit case's folder is its status: everything under `audits/active-cases/`
  is unfinished, and its live findings file records what remains.
  `audits/closed-cases/` holds only cases whose full requested scope is fixed,
  completed and verified (an explicitly read-only review closes when the review
  is complete). Benchmark folders are evidence and carry no status. Follow the
  [closure checklist](../KDR/audits/README.md#closure-checklist); a label,
  status edit or folder move never completes repairs or verification.
- Keep library/platform containers free of loose generated reports; preserve
  established tool-owned infrastructure. Frozen reports, receipts, source
  snapshots and evidence bytes stay unchanged; update live locators when moving
  records, not historical contents.
- `wins` and `defers` classify candidate changes, not case status. Label each
  win `proposed`, `implemented-unverified` or `verified` with cited evidence;
  never present a proposed or unverified win as a result. A defer is a candidate
  fix that matched measurement showed was not an improvement; record it with
  that evidence in the [deferred record](../KDR/audits/README.md#deferrals) so
  it is not re-proposed without new evidence. A defer never resolves its
  finding. Nothing else is deferred or postponed: difficult or unverified work
  is finished in the task, and only a genuine blocker stays open in its active
  case with the exact input or authority it needs.
- Keep requested investigation/audit/benchmark reports short and `.txt`: scope
  and coverage, status/counts, findings with ID/severity/path/trigger/evidence/
  disposition, measurements, and any genuine blocker with the exact input or
  authority it needs. Separate verified findings from candidates and coverage
  gaps. Link raw evidence; give a brief chat handoff with an absolute report
  link. User format restrictions take precedence.
- Documentation-only and file-organization work needs no audit log, report,
  relocation receipt or build. Update existing guidance and affected links,
  verify placement, and summarize in chat unless a separate record is requested.
  Keep KD4's `codex-rs/protocol/src/prompts/base_instructions/default.md` concise
  and repository-neutral.
