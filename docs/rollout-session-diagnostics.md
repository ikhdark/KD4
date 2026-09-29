# Rollout session diagnostics

The existing `scripts/kd4_turn_latency_audit.py` audit now includes
`sessionDiagnostics` (independently versioned schema 1) in text and full JSON,
plus `perTurn[].diagnostics` for terminal tasks/turns in full JSON.
It reads existing rollout snapshots; no binary rebuild, runtime
activation, new telemetry collection, or network access is required.

## Capture and compare

Run from the checkout root in PowerShell. Use separate directories containing
matched before/after workloads, or substitute individual rollout paths or session
UUIDs. UUID lookup requires `--sessions-root` or `CODEX_HOME`.

```powershell
node scripts/run-python.js scripts/kd4_turn_latency_audit.py BEFORE --json |
  Set-Content -Encoding utf8 baseline.json
node scripts/run-python.js scripts/kd4_turn_latency_audit.py AFTER --baseline baseline.json
```

Add `--json` for all details or `--summary-json` for bounded output. Save the
baseline outside the rollout input directories. Summary JSON and older reports without
`sessionDiagnostics` must be regenerated; missing fields are not treated as zero.

## What to inspect

- Per-turn p50, p95, mean, maximum, total, measured count, and missing count for
  elapsed and agent-active time; model-only, tool-only, overlapping model/tool,
  orchestration, retry-only, and human-only wait time; generation, tool-call,
  same-purpose continuation, and wait-only generation counts.
- Ranked measured time costs within each cohort, to prioritize investigation.
  Human wait is separate. These are costs, not proven waste or potential savings.
- Separate cohorts by workload population, terminal status/lifecycle, and timing
  schema. Canceled turns do not make completed-turn latency appear faster.
- Comparison rows for p50 and p95: `increased`, `decreased`, `within_threshold`,
  or `unavailable` with a reason. Counts and time use their own units.

### Core diagnostics

| Diagnostic | Report metric / interpretation |
| --- | --- |
| Model generations | `generations`: logical generations, not physical retries |
| Model-active wall time | `modelActiveMs`: recorded activity union |
| Tool-active wall time | `toolActiveMs`: recorded activity union |
| Harness/orchestration overhead | `orchestrationMs`: exclusive harness phase |
| Total / cached / uncached input tokens | `inputTokens`, `cachedInputTokens`, `uncachedInputTokens`: provider usage; input includes cached |
| Tool-output tokens projected to model | `toolOutputTokensProjected`: runtime projection estimate, not billed tokens |
| Artifact recovery calls / rereads | `artifactRecoveryCalls`, `artifactRereads`: runtime counters, not artifact creation/reuse |
| Recovery retruncations | `recoveryRetruncations`: retruncated recovery sections |
| Failed commands | `failedCommands`: paired top-level command responses with nonzero exit codes |
| Duplicate / redundant requests | `duplicateToolRequests`: repeated name + canonical JSON arguments (exact text for code); `redundantToolRequests`: explicitly annotated request IDs |
| Nonprogress generations | `nonprogressGenerations`: recorded aggregate, or fully retained requests with explicit unchanged-state / unchanged-action flags |
| Checkpoint attempts / useful checkpoints | `checkpointAttempts`, `usefulCheckpoints`: top-level calls; useful requires `changed=true` and `checkpoint_item_persisted=true` |
| Truncation count by source | `truncationCountBySource`: runtime projection operations and recovery sections; also `projectionTruncations` / `recoveryRetruncations` in distributions |
| Discovered-but-not-finalized evidence | `discoveredButNotFinalizedEvidence`: annotated discovered IDs absent from annotated final synthesis |
| Evidence survival into final synthesis | `evidenceSurvival`: discovered IDs retained in final / discovered IDs |
| Final answer recall / precision | `finalAnswerRecall`, `finalAnswerPrecision`: matched truth IDs / truth IDs, and matched truth IDs / answer-claim IDs |

Model-active and tool-active unions overlap; do not add them or subtract request
durations from elapsed time to infer overhead. Session totals sum terminal-turn
measurements, not cross-turn wall-time unions. Tasks use the rollout's terminal
task/turn identity; arbitrary multi-turn task grouping is not inferred.

Tool-event observations cover paired **top-level** calls, not operations hidden in
code-mode wrappers. Missing command exit codes or checkpoint receipts remain
unavailable. Nonzero exits include expected domain-specific statuses (such as a
search with no matches), so they are not automatically defects. Identical requests
can be legitimate polls or rereads after changes: duplicates are not proven waste.
Checkpoint usefulness here means a persisted context reduction, not task success.
Projection and recovery truncation counters can overlap; other source layers are
unmeasured, not zero. `--tokens off` disables provider-token analysis but still
reports recorded runtime projection counters.

Missing/invalid/saturated counters are not synthesized as zero. Each metric has
measured/missing counts; a partial `total` covers only measured turns. Ratios have
per-turn distributions and means, not summed or micro-averaged ratios.

### Evidence annotations and truth sets

Quality cannot be inferred from token counts or string overlap. Supply reviewed
semantic IDs with `--diagnostic-evidence evidence.json`. Use the exact `turnId`
from `perTurn` (directory inputs namespace IDs by file) and the matching
`coverage.snapshots[].sha256` from the same captured rollout. Stale snapshots,
unknown/duplicate turn IDs, and malformed ID lists are rejected.

```json
{
  "schemaVersion": 1,
  "turns": [{
    "turnId": "TURN_ID_FROM_REPORT",
    "rolloutSha256": "SHA256_FROM_REPORT",
    "discoveredEvidenceIds": ["fact-a", "fact-b", "fact-c"],
    "finalEvidenceIds": ["fact-a", "fact-b"],
    "truthIds": ["fact-a", "fact-b", "fact-c", "fact-d"],
    "answerClaimIds": ["fact-a", "fact-b", "incorrect-claim"],
    "redundantToolRequestIds": ["reviewed-call-id"]
  }]
}
```

All ID-list fields are optional; omit unreviewed dimensions rather than asserting
empty sets. Lists must contain distinct nonempty strings. Annotate all substantive
answer claims (including incorrect ones), using matching IDs for semantically
equivalent truth facts. The example yields one lost evidence item, survival 2/3,
recall 1/2, and precision 2/3. Empty denominators are unavailable, not perfect scores.
Annotations are reviewer assertions, not automated verification of answer truth or
evidence relevance; a discovered fact absent from the final is not necessarily an
error. Without annotations these dimensions explicitly remain unavailable.

Comparisons require at least five measured turns **on each side**, complete metric
coverage, and no session parse/profile/terminal-coverage blockers. Active turns are
excluded, not treated as completed. Invalid or conflicting terminal profiles are
excluded by the existing audit. Missing or saturated measurements stay unavailable.

The default review threshold requires both a 20% relative change and an absolute
change of 50 ms, one count/token, or 0.05 for ratios. A zero baseline uses the
absolute threshold without an undefined percentage. Override the sample minimum
or relative threshold with
`--comparison-min-samples 10 --comparison-threshold 0.30`.

Percentiles use nearest rank; small-sample tails are noisy. An increase is an
observed review cue, **not proof of a code regression**. Match tasks, model,
permissions, configuration, and task-success criteria before attributing a change.
Reduced tool/generation counts alone do not establish better work. Phase totals
sum turn time, not session wall time, and the ranked phases are not exhaustive.

Save **full JSON** as a baseline: summary JSON retains the existing compact audit
but omits the expanded session/turn diagnostics and cohort distributions to keep
its output bounded. When `--baseline` is supplied,
summary JSON includes comparison results. Comparison detail can be trimmed with an exact
`omittedMetrics` count; use full JSON to see every comparison. Essential totals
can exceed the normal summary budget, explicitly marked by `limitExceeded`.
