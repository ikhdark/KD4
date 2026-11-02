# Repository guidance

- Windows-only, local project. Do not publish or republish without permission.
- Upstream: https://github.com/openai/codex

## Harness prompts

Follow [instruction ownership and delivery](codex-rs/core/instruction-architecture.md). Diagnose prompt versus implementation defects; never mask implementation bugs with prompt changes. Review unintended behavior, replace stale guidance, avoid duplication/gratuitous edits, and prefer the best-supported version. Add instructions only when needed, in their authoritative owner. Keep `codex-rs/protocol/src/prompts/base_instructions/default.md` concise and repository-neutral—not KD4/KDR policy.

## Autonomy and evidence

Fix all verified in-scope issues, including audit/review findings; treat concrete harness failures as repair requests unless explicitly restricted to read-only/report-only work.

Missing evidence is unfinished work. For in-scope investigations/repairs, inspect capture, harness and analysis owners; implement and verify the smallest instrumentation/recovery fix, then resume. Preserve historical evidence; distinguish recoverable measurements from future capture improvements.

Only live model runs need extra permission for otherwise authorized work, including repairs, instrumentation, online lookups and non-model diagnostics. Honor read-only, privacy, safety, local-only/publishing, no-provisioning and higher-priority restrictions. Report genuine blockers as partial.

## Shared work and verification

KDR owns shared editing/session evidence, acceptance, benchmarks and reporting: [Work and evidence](../KDR/AGENTS.md#work-and-evidence), [Implementation and acceptance](../KDR/AGENTS.md#implementation-and-acceptance), [Benchmark workflow](../KDR/AGENTS.md#benchmark-workflow), and [Audits and reporting](../KDR/AGENTS.md#audits-and-reporting). Import only those sections; KD4 source/prompt and audit rules remain here.

## Audits and reviews

- Derive scope, exclusions and detection criteria from the request before investigation/delegation; include them in assignments. No invented exclusions, thresholds, sampling limits or finding caps. Adding categories needs user authorization.
- Cover all requested hunks/affected behavior, relevant subsystems, or full files/consumers as applicable. Only change-specific requests limit findings to new defects. Equivalence proves only the checked property.
- For broad audits/full reads, inventory current manifests/modules, including relevant hidden, ignored, generated and local-only paths. Check traversal limits/errors and cover every relevant producer, consumer and execution path against all criteria. Track unread ranges/mechanisms/follow-ons; finish in bounded batches. Prioritization never reduces coverage; partial coverage needs user-approved limits or a genuine access/permission blocker after independent work. Reads/hits/summaries/findings are not behavioral coverage.
- Historical-survival work accounts for every commit. Select production changes by requested behavior; triage artifacts by paths/manifests and inspect those affecting production/evidence. Check HEAD callers, startup config, defaults and gates first. Close clear survivors with source/equivalence evidence; trace uncertain survival/lost advantages further. Bulk object inventories use `git cat-file --batch-check`, preserving exact keys/missing results. Resolve evidence directories once to verified absolute paths. History triage does not replace other requested coverage.
- Account for every report entry/recommendation. Parse structured handoffs in-tool; fully read integrated entries and verify original evidence/artifacts. Dispositions: implemented/verified; verified/reported when read-only; rejected with evidence; out of scope; unresolved with next action/blocker. Track unread items separately; group only shared causes/dispositions, retaining counts/references. Recover omitted output with `read_tool_output`, not reruns.
- Findings require demonstrated behavior and production triggers; performance findings need measured costs. Never narrow coverage to ease verification. Send unverified candidates only to an authorized parent/verifier with evidence/reproduction steps, separately from findings; exclude them from final counts. Blocked verification is a coverage gap, not a finding or clean result.
- Keep reports decision-focused; summarize coverage by subsystem/mechanism and attach evidence to findings/candidates. Omit file inventories/hashes, no-finding narratives, dumps and extra artifacts/ledgers unless requested or required; retain diagnostics/internal tracking. Update the canonical report for follow-ups, preserving stable sections/deliverables without overlapping supplements or repeated qualifications.
- Write a short `.txt` report in a task-named folder under the repository-designated audit directory. Include status/counts, failures/gaps, coverage, unresolved items/blockers/next actions, and findings: ID, severity, `path:line`, defect/trigger, measured performance costs, evidence and disposition. Quote at most 3 lines; merge delegated results first. Chat gives brief status/counts and the absolute report link. Exact machine-readable schemas take precedence; the authorized caller handles export. Honor no-write/read-only, inline-only and permission restrictions; reports never expand scope.