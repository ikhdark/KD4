# Source inventory turn benchmark

This is an acceptance procedure for the existing rollout-analysis tooling, not
a new benchmark framework or a measured speedup. Run it after authorized Desktop
activation, against the same fully specified inventory request and comparable
source contents. The selected baseline is `f929334.jsonl` (2026-09-29), superseding
`f29235.jsonl`. Its audited turn took 75.25 seconds, with four physical model
requests and four underlying invocations. See the
[request-level audit](source-inventory-handoff-audit-2026-09-29.md) for evidence,
classification limits, and the distinction between avoidable orientation and a
proven mechanical-only handoff. This single run is not an improvement measurement.

## Expected flow

| Phase | Expected underlying invocations |
| --- | --- |
| Setup: `source_inventory.py --describe` | 1, or 0 if the current contract is already available |
| Pre-scan repository-structure inspection / query preparation | 0 unless a concrete scope uncertainty requires targeted inspection |
| Initial scan: stdin query, unique default state, report delivery | 1 |
| Continuation: same query and returned state | 0–N, only for pending work |
| Final answer from returned counts and links | 0 additional calls |

Require zero reads or content searches of `source_inventory.py` when the contract
supplies the needed facts. Also require zero standalone temporary-path,
query-file-writing, or existence-check calls, zero internal-state reads, and zero
post-scan inspection calls when the returned evidence is sufficient. Reading the
implementation while developing this change is not an inventory-task benchmark.

Build the query directly from requested categories and known scope after describe.
For repository-wide filename/rule requests, layout-independent globs let the scan
discover paths without first learning the directory layout. Preserve known roots
for restricted requests. A pre-scan inspection exception must name the uncertainty,
explain why query rules cannot express it, and show how the answer affects coverage
or verification. Do not suppress necessary semantic or runtime investigation just
to achieve three steps.

## Collection and accounting

Use `scripts/kd4_turn_latency_audit.py ROLLOUT --json` to retain the full audit
outside the source tree. Compare the new audit using `--baseline BASELINE_JSON`.
The script also accepts `--runner-evidence RUNNER_JSON` for versioned native
runner evidence; use it when available to establish physical model requests and
actual direct/nested executions. Do not infer successful execution by counting
tool names in generated JavaScript.

Analyze the full selected turn in a script, including setup, retries, failures,
cancellation, and final delivery. Count/hash bulk records rather than paging
them into model context. Record the source/rollout identities, task, tool version,
and whether the contract was already available.

Report these independently, both by phase and for the complete turn:

- **Model-request count:** actual physical model requests, including retries.
  Keep logical generations/model round trips visible separately when retries
  make them differ. Never substitute the number of tool calls for this count.
- **Underlying tool-invocation count:** direct non-orchestration tools plus
  actually dispatched nested tools, deduplicated by invocation identity.
  Count failed/cancelled dispatches too. Report outer exec/wait wrappers
  separately, not as substitutes for their nested operations or as extra leaf
  invocations. A wrapper spanning phases retains attribution for each child.
- **Setup calls**, **initial scans**, **continuations**, and **post-scan
  inspection/delivery calls**. Attribute recovery calls to their phase and also
  show a recovery subtotal; do not double-count it in the total.
- **Pre-scan inspection/query-preparation calls and generations**, separately from
  describe setup and the initial scan. Attribute model time between describe and
  scan, including any inspection round trip; zero inspection calls does not mean
  zero query-construction time. Report justified scope exceptions separately from
  avoidable repository orientation.
- Elapsed time by phase and for the complete turn. Identify overlapping intervals
  rather than summing concurrent child durations as elapsed time.

The audit already exposes runner physical requests and direct/nested activity,
tool orchestration, and timing coverage. Use retained rollout records to classify
inventory-specific phases. If nested dispatch records, request counts, or
timestamps are unavailable, report the affected measurement as unverified,
not zero. One exec must not hide several nested operations.

## Continuation evidence and acceptance

For each continuation, record its call identity, preceding `scan_pending`, newly
required evidence obtained, remaining pending work, and any completion
established. Use the already returned envelope/path pages/report records as
evidence; do not make extra inventory calls just to populate benchmark accounting.

A continuation returning no newly required inventory evidence is unnecessary
unless it is needed to establish completion. Technical permission to continue
does not establish progress. Newly captured nonmatches or resolved limitations
can advance coverage even when the included-path count does not increase.
If the available records cannot establish progress or completion, mark that
continuation unverified rather than passing it.

Require equivalent declared coverage, correct counts and limitations, and usable
report/canonical-path links. Every deviation from the expected flow needs a
concrete cause. Lower elapsed time alone is not a pass.

Prompt-contract tests establish guidance delivery, not compliance. Report
single-turn comparisons as observations, not statistically established speedups;
retain the audit's cohort/sample limitations. Do not claim a user-visible
improvement before a complete-turn comparison following authorized activation.
