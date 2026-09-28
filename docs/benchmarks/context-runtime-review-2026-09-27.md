# Context/runtime regression measurements (2026-09-27)

Local Windows checkout; concurrent unrelated edits/builds were present. No
installed application was rebuilt, activated, or restarted. These measurements
are bounded regressions, not live-model productivity or latency claims.

## ToolHistory pressure

`tool_history::tests::pressure::long_session_pressure_records_prompt_receipts_recovery_and_elapsed`
replays three growing requests containing 24 completed artifact-backed results,
one checkpoint, and unresolved evidence. Each request needs old, middle, and
recent bytes. Recovery reads stored artifacts and asserts exact bytes; it never
invokes the producer. The recovery count represents artifact-owner reads, not
model-selected `read_tool_output` calls. Unresolved evidence must remain inline.

| Per-request tool budget | Aggregate model-input tool tokens | Compact receipt tokens | Recovery reads | Replay ms |
| --- | --- | --- | --- | --- |
| 129,000 (258k half-window) | 326,200 | 5,141 | 4 | 581.8 |
| 100,000 | 275,880 | 6,021 | 5 | 199.7 |
| 75,000 | 200,400 | 7,341 | 5 | 197.8 |
| 8,000 (stress only) | 11,685 | 10,626 | 9 | 176.1 |

Receipt tokens are a subset of input tokens. Times include projection and local
artifact recovery, exclude fixture creation/model/network time, and run in the
listed order without cold-cache normalization. Do not infer a live-turn speedup
from these timings. Smaller budgets caused extra recovery even in this fixed
workload. Production remains `context_window / 2`.

Set `KD4_TOOL_HISTORY_BENCH_OUTPUT_DIR` to an absolute output directory to record
JSON metrics (the older `CODEX_*` spelling is cleared by Windows test isolation).

## Local Nextest concurrency

The same 77 tests (tool planner, checkpoint handler, and real-turn lifecycle)
ran in order 2, 4, 4, 2 workers through the reserved `core-tests` lane and `fast`
profile. Every execution passed with retries disabled. Dependencies were warm.

| Workers | Nextest seconds | Whole command seconds |
| --- | --- | --- |
| 2 | 24.898, 24.877 | 28.611, 28.519 |
| 4 | 13.820, 14.205 | 17.447, 17.848 |

This supports removing the former global two-worker bottleneck. Preserve the
concurrently measured six-worker defaults documented in
`validation-parallelism-2026-09-27.md`: Windows core remains capped at four,
fixed-port/legacy tests at one, and process-heavy tests at two. This sample does
not establish six-worker performance outside those caps.

## Generation and schema bounds

Both 16 and 24 no-evidence generation bounds passed the existing continuation,
required-work, productive-renewal, owned-polling, and failure-at-limit cases.
Keep 16; productive evidence still resets the counter.

Serialized model-visible tool definitions measured 54,378 bytes (direct),
51,781 (plan), 86,485 (mixed Code Mode), and 66,603 (CodeModeOnly). Tests allow
25% mode-specific growth, without changing the existing 128 KiB runtime warning.
Small subsequent description edits may move these baselines slightly.

Raw measurements and command logs are in the ignored
`codex-rs/target/context-review-metrics` directory. The final-prompt guidance test,
token-budget lifecycle gate, real validation fail/repair/pass and source-mutation
tests, persisted checkpoint lifecycle, native shell recovery, schema discovery,
incremental recovery, and tiny Known Delta hit checks passed. The Windows paired
npm smoke also passed using the existing debug executable: locally generated
main/native tarballs launched `codex --version` and `codex --help`, and invalid
packages were rejected. This is not release-binary or publication validation.

The validation smoke keeps compiler products/control markers outside the watched
source tree and models a 250 ms response interval between repair and revalidation.
Without that interval, delayed OS notifications for the repair can conservatively
invalidate the subsequent pass despite matching Git identities. This existing
watcher-delivery limitation remains; freshness rejection was not weakened. The
separate in-flight source-mutation test has no settling interval and rejects the
completed pass as current proof for the changed checkout.
