# Token/cache review benchmark — 2026-09-28

This report records the component/mock phases. The subsequent actual-provider
results are in [the live-model report](token-cache-live-2026-09-28.md): functional
checks passed, but the bundled candidates increased output tokens and latency.

## Scope and decision

Measured findings **15 → 17 → 5 → 2 → 10 → 18**, in that execution order.
All 12 component cases passed. All 11 selected baseline end-to-end checks have
passing evidence after repairing one test's receipt extraction.
The follow-up now also has a passing **online candidate-on/off full-turn test**
covering all six candidates together; see the candidate E2E section below.

These are **test-only candidate projections**, not production optimizations.
No Desktop binary was replaced or restarted. Provider uncached input, cached
input, model output tokens, total billed tokens, and live-model turn latency were
**not measured**. No live model requests were made. Token reductions below are
local `o200k_base` counts of the specified serialized payloads, not billing data.
Do not sum the rows: they represent different, sometimes overlapping surfaces.

## Results

| Finding | Fixture / measured surface | Baseline → candidate tokens | Reduction |
| --- | --- | ---: | ---: |
| 15: nested output budget | One running command, final outer text | 3,998 → 2,282 | 42.9% |
| 15 | Two running commands, final outer text | 3,998 → 2,352 | 41.2% |
| 17: read_file envelope | Whole UTF-8 file | 212 → 158 | 25.5% |
| 17 | Selected Unicode line | 229 → 180 | 21.4% |
| 5: repeated tool_search | Second identical search in one turn | 666 → 45 | 93.2% |
| 2: duplicate tool contracts | Mixed direct/code-mode router, 18 tools | 15,868 → 12,165 | 23.3% |
| 10: freshness notices | One invalidation | 248 → 252 | **1.6% increase** |
| 10 | Eight invalidations | 1,984 → 1,456 | 26.6% |
| 10 | 32 invalidations | 7,936 → 5,584 | 29.6% |
| 18: MCP JSON mirrors | Empty rows/error result | 45 → 30 | 33.3% |
| 18 | Ten rows | 348 → 172 | 50.6% |
| 18 | 100 rows | 3,048 → 1,432 | 53.0% |

For #15 the intermediate nested JSON is 7,084 → 2,282 tokens for one command,
and 14,168 → 2,352 for two. The outer budget already truncates the baseline;
those larger intermediate reductions are **not** model-input savings.

### What each candidate proves, and what remains

1. **15:** Explicit nested budgets totaling 2,500 tokens avoid much outer
   truncation. Actual persisted output recovers the omitted middle; running
   session ID and incomplete-output flags survive. Dynamic budget allocation,
   extra recovery turns, cancellation behavior under the candidate, and task
   success need production-path validation before adopting lower defaults.
2. **17:** Removing two duplicate fields and null `artifact_id` saves 49–54
   tokens. The original envelope reconstructs exactly for both fixtures,
   including completeness and Unicode. Removing fields from public/JS results
   would require compatibility handling; prefer a model-only projection.
3. **5:** Replacing an already-visible identical schema response with a digest
   receipt saves 621 tokens on the second response. Missing history or changed
   schemas fall back to full results in the prototype. A real visibility/revision
   ledger and resolution path are not implemented; compaction and resumed
   sessions must be covered before rollout. First search remains 666 tokens.
4. **2:** Removing eager nested declarations while keeping direct schemas
   intact saves 3,703 schema tokens. Real router flags and default registration
   are exercised. This does not prove equivalent model tool use: additional
   `resolve_tool` calls or errors could offset the saving. Stable smaller
   schemas can reduce input without necessarily increasing absolute cached input.
5. **10:** Common guidance plus a batch of exact freshness records reduces
   repeated notices. The historical prefix remains unchanged, and every
   original notice reconstructs exactly. **Retain the single-notice form**;
   batching one regresses. New batch grammar requires consumer/model testing.
   Preserving a prefix supports cacheability but does not demonstrate a cache hit.
6. **18:** Removing only exact JSON text mirrors retains structured data,
   nonmatching captions, error state, and equivalent direct-tool output. Raw JS
   results remain unchanged. A future implementation must operate only on the
   display projection and preserve image/content-only results, not mutate MCP data.

### Local timing, not user-visible speed

Debug build; three warmups, 21 samples of eight operations; median and p95 are
retained in JSONL. Baseline and candidate measurements are sequential, not a
randomized production benchmark. Serialization and transform work differ.

| Operation | Baseline → candidate median |
| --- | ---: |
| 15: outer formatter, one / two commands | 18.54 → 3.90 ms / 32.46 → 4.65 ms |
| 17: whole / selected envelope | 18.20 → 17.92 µs / 18.77 → 22.02 µs |
| 5: stringify / digest receipt transform+stringify | 91.64 → 160.12 µs |
| 2: schema serialization | 1.772 → 1.470 ms |
| 10: join prepared notices / serialize batch, 32 notices | 3.18 → 753.90 µs |
| 18: stringify / mirror transform+stringify, 100 rows | 371.08 → 611.86 µs |

Several candidates trade local CPU for smaller payloads. #10 especially compares
joining already-rendered baseline strings against serializing candidate records;
it does not measure the full notice-generation pipeline. No complete-turn speedup
is established. Future live A/B needs usage counters, task success, handoffs,
recovery cost, cancellation, and elapsed turn time—not replay after a model call.

## End-to-end non-regression evidence

The existing integration suite runs real code-mode JS and tool dispatch against
local mock Responses/MCP servers. Selected checks cover:

- Running command handles and artifact recovery at normal and zero output budgets.
- Truncated outer-cell artifact recovery, including the exact omitted middle line.
- History-prefix stability after unrelated edits and invalidation after source edits.
- Deferred discovery, schema exposure, and tool invocation.
- Command output, read_file execution-result shape, and MCP structured, content-only,
  error, and image results.

Initial run: **10 passed, 1 failed** (9.057 s test time). The failure parsed the
last text line as the recovery receipt. Replaced that assumption with the suite's
existing `output_recovery_receipt` helper, retaining all exact recovery assertions.
Only that affected test was rerun: **1 passed** (1.432 s). This is a repaired test,
not evidence of a flake. The other ten passing results are reused, not rerun.

These establish the scoped **baseline** behavior and test-harness non-regression.
The six test-only transformations have component assertions; they are not wired
into dispatch, so these E2E passes cannot establish candidate production or model
non-regression. No full suite was requested or run.

## Candidate end-to-end follow-up

Added `suite::code_mode::token_cache_e2e::candidates_complete_turn_non_regression`.
It executes two complete real Codex turns, with the candidates off and on, each
making **11 model requests**. The provider is deterministic and local. This is
**online, not replay**: it consumes the projected request before selecting the
next action and refuses to advance if required evidence is missing or wrong.

The missing production switch is not simulated as an installed optimization:
#15 uses the real command's explicit output budget; #17, #5, #2, #10, and #18
are applied in a test-only provider-boundary adapter. The adapter supports the
fixture's tagged file/search/MCP result shapes, not arbitrary tool output. The
search receipt references an earlier visible call ID, allowing the deterministic
consumer to resolve it. A lost-history check requires the full schema to return.
This validates this bounded experimental pipeline, **not** a shipping Codex
implementation or an LLM's ability to use shortened contracts/receipts.

### Observed complete-turn counts

Final run: `candidate-e2e-3`, **passed**, source manifest unchanged during the run.

| Metric | Baseline | Candidates enabled |
| --- | ---: | ---: |
| Sum of local input token estimates, all 11 requests | 196,303 | 145,855 (**−25.7%**) |
| Scripted output-event token estimates | 799 | 805 (**+6**) |
| Input + scripted output estimates | 197,102 | 146,660 (**−25.6%**) |
| Command-output item token estimate | 4,142 | 2,362 |
| Model requests / recovery calls / MCP calls | 11 / 1 / 1 | 11 / 1 / 1 |
| Visible invalidation messages at checked step | 6 | 3, retaining all 6 records |
| Observed turn wall time | 6,000 ms | 4,410 ms |

Input counts tokenize complete JSON request bodies with `o200k_base`; scripted
output counts tokenize the generated SSE action objects, including their event
wrappers. They are consistent local comparison proxies, **not provider usage**.
Output did not decrease. Cached/uncached counts remain unmeasured. This is one
sequential pair, not a latency study; temporary paths, IDs, watcher timing, and
process scheduling can vary. No statistically established speedup is claimed.

### Non-regression assertions exercised

- Real UTF-8 file reads, deferred search twice, schema receipt resolution, and
  actual HTTP MCP dispatch; MCP executes exactly once and retains its raw JS
  shape, caption, structured data, and success flag.
- A real subprocess produces truncated output, its session is drained to exit 0,
  and the omitted `ROW_0500` is recovered from the persisted artifact. The
  producer runs exactly once: recovery cannot silently rerun it.
- Real `apply_patch` changes the source; both prior reads become stale while
  their old contents remain intact. Batching must actually reduce notice count
  and notice-only token cost, and preserve every freshness record exactly.
- The initial request input prefix remains unchanged in every subsequent request.
  A singleton notice is unchanged; missing search history cannot leave a receipt
  referencing an invisible schema.
- A fresh read after mutation and the MCP data determine the final write.
  Exact final file contents and final assistant completion are asserted; the
  evidence source and an unrelated sentinel file remain unchanged.
- Each candidate must reduce its own measured request surface, and the combined
  run must reduce total input/total token estimates without extra recovery or
  model requests. These are not merely checks of a scripted "success" label.

The earlier 11 baseline checks are retained historical coverage, not rerun as
part of this follow-up. The new test does not cover live inference, cancellation,
all output modalities, or production deployment. Broader production integration
and live-model A/B remain separate requirements.

## Historical evidence

The Rust benchmark system and its Python runners have been removed. This report
preserves historical measurements, not instructions for a supported benchmark.

The retired runner reserved the repository's core-tests Cargo lane, validated execution
counts/order, and recorded commands, timestamps, exit status, and before/after
SHA-256 hashes for relevant source files (12 originally, 13 with the candidate
E2E source). This is a scoped source manifest,
not a hermetic dependency/build fingerprint. Successful component and focused
E2E manifests had unchanged sources and matched the inspected files afterward.
Final inspection subsequently found a concurrent, unrelated E2E benchmark module
registration in `core/tests/suite/code_mode.rs`; it was preserved and is not
validated by these runs. The benchmark source and Python runner also received
formatting-only line wraps afterward. The ten reused E2E passes precede the
receipt-parser repair, which changes only the separately rerun test.

Local raw evidence (ignored build artifacts, not committed):

- `codex-rs/target/token-cache-review/attempt-6/`: complete 12 records, manifest,
  runner log; 1 ignored benchmark test passed in 12.303 s, wrapper wall 44.041 s.
- `codex-rs/target/token-cache-review/e2e-1/`: initial 11-check run and failure.
- `codex-rs/target/token-cache-review/e2e-2/`: focused successful recovery rerun.
- `codex-rs/target/token-cache-review/candidate-e2e-3/`: final candidate-on/off
  pair, per-request counts, source manifest, and passing runner log.

The first candidate pair passed; a second added direct batching/prefix assertions
and also passed, but its wrapper rejected the measurement because concurrent
edits changed read_file, tool_history, and command_output_artifact during the run.
The third pair reran that exact focused test against the updated sources and
passed with an unchanged manifest. No concurrent changes were overwritten.

Earlier attempts are retained. They exposed fixture construction/env/payload
issues that were repaired, Cargo lane contention, and an unrelated concurrently
edited benchmark compile failure. The latter no longer blocked the successful
run; its source was not modified by this work. Partial attempts are not used
as final measurement evidence.

The production full-turn regression remains in
`codex-rs/core/tests/suite/code_mode_token_cache_e2e.rs`, without the retired
component, candidate-on/off, or live-model benchmark machinery.
No provider cache improvement or reduced model output is claimed. Production
implementation and live-model A/B remain separate follow-up work.
