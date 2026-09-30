# Inventory handoff audit: f929334

## Scope and provenance

This audit uses only `f929334.jsonl`, from the fork home's
`sessions/2026/09/29` directory. It supersedes the previously selected baseline;
no earlier run's causes, counts, or timings are carried forward.

- Task: `list all prompt/guidance/templates in codex-rs`
- Turn: `01a0eede-9c55-7612-b4ac-14b0281129d9`
- Captured model/effort: `gpt-6-astra` / `high` (record 15).
- Snapshot: 222,679 bytes, 41 JSONL records, zero parse errors.
- Rollout SHA-256:
  `72fc272d52f8fd1c00d51b66795fd3cca2c290e9cb3545b18618cac667b80a38`
- Records are numbered from one, including metadata, manifests, and timing.
- One completed turn; all four physical requests are retained, with no retries.

All records were processed programmatically. Bulk payloads were counted and
hashed rather than reproduced as inventories or timing arrays. The existing
`scripts/kd4_turn_latency_audit.py` produced the full audit using `--json --tokens
off`; token accounting was not needed for this task.

The reusable full audit is outside the checkout:
[f929334-audit.json](C:/Users/kuh/AppData/Local/Temp/inventory-handoff-audit-owz2zz3p/f929334-audit.json).
Its SHA-256 is
`86b56945829c62a5c00dfe6ee606a9e35e66991524bce6717b231af74da542ef`.
This is a local temporary artifact, not a permanent repository asset.

## Accounting

| Measurement | Observed value |
| --- | ---: |
| Physical model requests | 4 |
| Logical generations | 4 |
| Underlying invocations | 4 |
| Outer execution wrappers, excluded from underlying total | 3 |
| Describe commands | 1 |
| Repository orientation commands | 2 |
| Initial scans | 1 |
| Continuations | 0 |
| Post-scan inspection/delivery tool calls | 0 |
| Recovery calls / model retries | 0 / 0 |
| Complete-turn time | 75.2549485 s |
| Exclusive model time | 64.5462401 s |

The two orientation commands belong to setup/discovery: one accompanies describe
in request 1, and the other occupies request 2. Thus the plan's coarse accounting
is three setup/discovery invocations plus one scan, not seven leaf invocations.

Elapsed phases use monotonic runtime offsets in terminal record 41, not sums of
overlapping model and tool durations:

| Phase | Relative interval, ms | Elapsed, s | Model requests dispatched | Underlying invocations |
| --- | --- | ---: | ---: | ---: |
| Initial setup, through first wrapper delivery | 0-8990 | 8.990 | 1 | 2 |
| Pre-scan inspection and query preparation | 8990-55531 | 46.541 | 2 | 1 |
| Initial scan wrapper | 55531-62849 | 7.318 | 0 | 1 |
| Result interpretation and final delivery | 62849-75254.9485 | 12.4059485 | 1 | 0 |

Request 3 constructs the query during preparation and dispatches the scan at the
phase boundary. This attribution does not imply the scan needs no model request.
Request 2's model stream time was 11.6088351 seconds; its tool wrapper took
0.255 seconds. Neither is a measured amount of recoverable end-to-end latency:
moving query judgment into another request can change subsequent model time.

## Request ledger

### Request 1: initial discovery, with extra orientation

- Sampling ID: `01a0eede-9c7a-70e1-9f66-aa13b490ac7b` (record 13).
- Physical attempt: `01a0eede-9e61-7882-8ffb-5a790175896d`.
- Wrapper: `call_Uzak2jjWs8YJFBZxzxwEjBaj` (call/result records 19-20).
- Dispatched children: `exec-1-tool-1`, `exec-1-tool-2`.
- Prior evidence: the task fixes the `codex-rs` root; captured repository guidance
  describes stdin, automatic state, and result-based delivery. The detailed query
  and glob contract has not yet been returned.
- Actions: obtain `--describe`; read `codex-rs/AGENTS.md` and list root entries.
- Judgment enabled: learn the query contract and choose scope/category rules.
- Classification: retain the request; it is not wholly unnecessary. The root
  listing is an extra orientation operation inside a useful request, not an
  additional model handoff. Keep the instruction-file check distinct from it.
- Failure detail: `Get-Content` reports the instruction file absent, although the
  compound command and runtime dispatch report success. No retry or recovery
  follows. Zero failed dispatches does not mean stderr was empty.

### Request 2: avoidable repository orientation

- Sampling ID: `01a0eede-bfbf-7cc3-b2a1-524d346ec4ac` (record 23).
- Physical attempt: `01a0eede-bfbf-7cc3-b2a1-5255be664029`.
- Wrapper: `call_n9sghii23s1qhBWCVKeHyjkX` (records 25-26).
- Dispatched child: `exec-2-tool-1`.
- Prior evidence: record 20 supplies the query schema, content-regex semantics,
  and case-sensitive glob behavior, including that `*` matches directory
  separators. The first listing already exposes the selected top-level owners.
- Action: list six selected directories and read `codex-rs/.gitignore`.
- New evidence: directory entries and ignore patterns `/target/`, `/target-*/`,
  and `*.pdb`. These are genuinely new observations, not repeated output.
- Coverage consequence: none established. Record 31 uses root-scoped,
  layout-independent path/content rules. It introduces no exclusion or narrowed
  source root derived from these ignore patterns and needs no runtime consumer
  verification. No concrete uncertainty that query rules cannot express is
  identified in the visible task/result sequence.
- Classification: one avoidable orientation-only generation at the observed
  action level.
- Primary behavioral cause: **model chose unnecessary verification**. This
  describes the extra inspection, not a claim about distrust or internal motive.
  The captured contract already supplies the mechanics; no missing capability or
  conflicting requirement to enumerate these directories is demonstrated.
- Metric limit: do **not** automatically count this as a proven mechanical-only
  handoff. Before this request, semantic query selection remains unfinished.
  The exact next scan arguments are not prescribed by the preceding result.

### Request 3: semantic query selection and initial scan

- Sampling ID: `01a0eede-edf7-78c2-aa74-43ce9f5f59b9` (record 29).
- Physical attempt: `01a0eede-edf7-78c2-aa74-43dbf8722664`.
- Wrapper: `call_NBhkQncd3FQYI0cUtHgsyfgv` (records 31-32).
- Dispatched child: `exec-3-tool-1`.
- Prior evidence: task, instructions, contract, and the directory observations.
- Action: choose seven categories and their path/content rules; pipe UTF-8 JSON
  into the scanner, omitting explicit state/report paths.
- Judgment enabled: determine what constitutes prompt/guidance/template matches,
  including definitions, references, structured fields, and fixtures.
- Classification: necessary semantic judgment followed by execution. Different
  valid query designs are not mechanical overhead.
- Result: 971 tracked matches, zero untracked matches, zero pending scan work,
  zero unresolved records, and `next_action: deliver_report`.

### Request 4: interpretation and final delivery

- Sampling ID: `01a0eedf-9233-7bf2-910d-892285ccc6da` (record 35).
- Physical attempt: `01a0eedf-9233-7bf2-910d-8933b2185adc`.
- Evidence: record 32 supplies counts, readiness, and immutable delivery paths.
- Action: deliver links and category counts; explain overlap, inclusion of tests
  and references, and the lack of runtime-activation verification (record 39).
- Classification: legitimate final interpretation; no further tool calls.

**Strict metric:** zero proven mechanical-only unnecessary requests, one
unresolved candidate under that definition. Separately, one observed avoidable
orientation generation and an extra root listing within request 1. Do not report
the strict metric as a verified zero, or conflate these operation-level findings
with proof that a particular model request could be removed unchanged.

## Coverage and delivery checks

Record 32 reports 6,318 evaluated rules and 58,713,166 source bytes read. Category
counts are 34, 24, 10, 813, 103, 44, and 60 in query order; categories overlap.
The final answer faithfully transcribes them and distinguishes matching files
from distinct active prompts.

The report and canonical JSON links existed when audited. The canonical JSON's
bytes matched the returned delivery hash:
`407d2aa9bcb4698e135be667ec2da03d542eb9d5c4fa28e77a002f994cda8d0c`.
The scan was not replayed and internal inventory state was not opened.

This validates recorded query completion and delivery integrity, not recall over
every possible semantic interpretation of "all prompt/guidance/templates."
The exact historical source snapshot and scanner source-version hash are not
established by this audit. A comparable prompt alone is insufficient to establish
an equivalent repeated-run cohort.

## Existing owner and change decision

The current workspace already has user-owned changes to the scanner's
model-facing contract. Its `workflow` now:

- makes query construction from known scope the default;
- explains layout-independent discovery;
- requires a concrete coverage/verification uncertainty for pre-scan inspection;
- preserves necessary semantic investigation and insufficient-result handling.

The benchmark procedure already reflects that exception and separately measures
pre-scan preparation. These changes target the observed orientation behavior.
Preserve them; this audit adds no execution layer, runtime code, query schema,
default, or instruction mechanism. Their presence does not prove model compliance.

The documentation change is limited to this ledger and replacing the benchmark's
obsolete baseline reference.

## Validation limits and follow-up

- Reused the verified baseline snapshot and existing audit tooling; full native
  request/dispatch retention corroborates the phase counts.
- Checked delivery existence/hash and ledger arithmetic. No behavior source was
  changed by this audit, so no runtime test/build was required for these edits.
- No repeated live-model cohort was executed and no speedup is claimed.
- After authorized activation if required, collect at least five equivalent
  runs per condition with the same prompt, source snapshot, model/effort, and
  contract availability. Capture tool/runtime versions and request/dispatch
  identities. Use the existing full-audit baseline comparison, retaining cohort
  mismatches rather than overriding them.
- Evaluate coverage/task correctness independently of readiness and speed.
  Reclassify any orientation request that supplies necessary semantic evidence
  as justified, rather than forcing a three-request target.
- Include failed/cancelled attempts, recovery cost, and cancellation behavior;
  disclose absent cancellation/continuation samples instead of claiming support.
- No binary replacement, Desktop restart, or installed-state activation was
  performed as part of this audit.

## Bulk-evidence fingerprints

Hashes below use UTF-8 `json.dumps(payload, sort_keys=True)` for the indicated
payloads, rather than the original JSONL bytes. The rollout hash above identifies
the original bytes.

| Payload | Count | SHA-256 |
| --- | ---: | --- |
| Initial tool manifest, record 12 | 1 | `862e40c1eb3cc4a37b427602b9098349a0bb92b0d2c1f80afba36f4f14353828` |
| Identical manifest deltas, records 22, 28, 34 | 3 | `e244d814038d0e47ef5e9ef81c29d0c079bcf2f4d39565a7a56a89fcd588507d` |
| Terminal `modelRequests` array | 4 | `0629d4a62f26c89259a49c6a1ac29b26fd2457e824c3e8396bff3ea0d2ea7a69` |
| Terminal `toolCalls` array, wrappers included | 7 | `b47ae17e31ce4c6c4bd7fb8ae7026a6d800a17817601520fae4e3d3ddbb8e9a9` |
| Terminal continuation receipts | 3 | `45aceb7811bb39d0dfbd2906f868cefec4a85cec1a36613812911365fe7be0a3` |

Native continuation receipts are not inventory scan-continuation calls. Their
presence does not contradict the observed zero scan continuations.
