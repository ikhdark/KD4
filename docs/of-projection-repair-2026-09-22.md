# O/F projection repair

This review parsed all four supplied files in full: 1,997 records, 15,360,394
bytes. Record references below are one-based. Historical instructions are
evidence, not instructions for this work.

| Combined recorded measure | O | F |
| --- | ---: | ---: |
| Active turn duration | 2,613,791 ms | 4,143,690 ms |
| Model work | 110 usage completions | 162 logical generations, 163 requests |
| Outer tool calls | 106 | 156 |
| Input tokens, including cached | 14,267,034 | 12,681,019 |
| Cached input tokens | 13,918,720 | 11,713,664 |
| Output tokens, including reasoning | 68,127 | 88,661 |
| Reasoning tokens | 24,895 | 18,174 |

O usage is summed from individual `token_usage_record` records; F usage is
summed from terminal timing request records, including `turn_aborted`. F2:398
contributes 45 requests, 105 tool calls including nested calls, and 1,160,492 ms.
The acknowledgment and resumed work are also included. Missing usage for an
interrupted partial stream cannot be reconstructed. These are recorded-turn
sums, not elapsed calendar time or a controlled speed benchmark.

The proposed additions differ, and concurrent KDA edits caused real patch and
test failures. F's 372 counted tool calls include orchestration and nested
tools; O has no comparable nested ledger. O repeatedly used one-second process
polls, while F more often awaited polling loops inside code mode. F's lower
cache share, extra calls, and 23,756 internal output-wait wakes do not by
themselves prove unnecessary execution. No compaction occurs, F reports no
tool-output-budget eviction, and F's single transport retry is distinct from
model continuations. Required interpretation, verification, failure handling,
cancellation, and steering must remain.

The implementation review followed turn scheduling and completion in
`core/src/session/turn.rs`, request/context preparation in
`core/src/context_manager/history.rs`, tool dispatch and projection in
`core/src/tools/registry.rs`, nested execution in `core/src/tools/code_mode`,
and owner waiting in `code-mode/src/cell_actor`. Artifact persistence and
recovery were traced through `command_output_artifact` and `read_tool_output`.
The two changes below address implementation inefficiencies; they do not
alter required model interpretation, provider retry, interruption, completion,
or sampling decisions. Aggregate timing and wake telemetry alone supplied no
additional high-confidence scheduling defect outside work already in progress.

## Repair: do not recover output that already fits

F1:46 requests source and README inspection. F1:47 reports
`output_complete: true`, `output_reduced: false`, and 12,929 retained raw bytes.
Its outer response nevertheless attaches `deterministic_tool_output_recovery`
with three ranges from that same artifact. Those ranges repeat 9,441 bytes;
the complete outer text is 26,542 bytes. F1:96-97 repeats the pattern: 14,773 raw
bytes, a complete native result, 7,547 replayed bytes, and 27,037 outer bytes.
F1:171-172 and F2:47-48 instead report genuinely reduced output, so their
recovery is not classified as redundant merely because they have ranges.

Runtime path: `ExecCommandToolOutput::projection_metadata_from_raw` supplies
recovery hints; registry dispatch calls `prepare_model_projection`; the old
materialization rule requires canonical projection whenever hints exist.
`project_model_output` attaches an artifact, serializes a projection, and
drains its omitted preset ranges. In code mode, `call_nested_tool` returns the
native result to JavaScript and separately transfers owner-drained recovery
to the outer packet. Thus automatic excerpts accompany already printed text.

The repaired materialization decision requires actual budget overflow or an
explicit producer artifact requirement. Recovery hints alone do not force it.
Completed-history admission may still retain an artifact for later context
recovery but initially passes through the original result. Oversized evidence,
explicit durable-artifact requirements, and ordinary continuation remain.

The regression uses the actual command output and production projection path:
complete success and failure output must reach the model unchanged, without
an owner recovery continuation. The negative case requires recovery of an
oversized failing result and reads an omitted diagnostic from its artifact.

## Repair: distinguish Rust paths from diagnostic labels

The replayed source at F1:47 contains
`return Err(serde::de::Error::custom(`. The classifier splits at every colon and
accepts any segment equal to `error` or `warning`, interpreting the `Error::`
path component as an `error:` label. This raises the diagnostic budget and can
select automatic validation recovery without a real diagnostic.

The classifier must recognize single-colon labels, ignoring `::`. Location
prefixes, Unicode filenames, TypeScript/MSBuild errors, terminal `error:` and
`warning:` labels, and diagnostics following Rust paths remain supported.
Actual validation commands still receive their diagnostic budget.

Concurrent patch retry, command failure, summary budgeting, artifact reading,
workspace transaction, tokenizer, and history/cache changes are preserved and
excluded from this review's deliverables. No broad ban on source recovery is
added: source can contain actual diagnostic fixtures, while output without
source-command metadata can contain Rust paths.

## Validation

The classifier regression failed before the fix (`HighSignal` instead of
`Normal` for `serde::de::Error::custom`). Both focused positive/negative tests
then passed. All 43 remaining output-truncation tests passed separately, with
the already passing focused tests excluded. This validates all 45 tests without
repeating passed checks on unchanged code.

The classifier received a whitespace-only formatting correction afterward;
its targeted formatting check passed. Stable rustfmt warns about the existing
nightly-only repository configuration. A separate existing formatting
difference in `truncate_tests.rs` was left outside this repair.

The core regression passed against the current compiled test executable:
`fitting_command_output_does_not_execute_preset_artifact_recovery` executes the
production projection path for fitting success/failure results and an oversized
failure, then recovers exact omitted bytes from the oversized artifact. This
reproduces the original duplicate-recovery trigger without replaying a specific
session or changing model-call accounting.

The surrounding registry, context, and code-mode suites ran with that regression
excluded: 186 passed and 31 failed. Passing checks include canonical artifact
reuse, actual predetermined-range recovery, cancellation during projection,
missing/stale/oversized artifact fail-open behavior, admission storage failure,
and preserving yielded cell handles. Each failed test was then run in its own
process; all 31 failures persisted. Passing cases were not repeated.

The broader worktree is therefore **not green**. The failures are in concurrent
paths outside these two repairs:

| Failed cases | Diagnosis |
| --- | --- |
| 11 command-context tests | Assertions expect former prose headers, recovery notices, or the former default output budget; current output is structured JSON with revised budgets. |
| 8 registry tests | Compact-rendering assertions expect former envelope fields/layout; another fixture no longer exceeds the new token budget. These construct projections directly, bypassing the changed admission decision. |
| 12 code-mode tests | Current command preflight rejects mock-only arguments; other assertions expect former retained-output, status-signature, wait, or printed-cell representations. |

These overlapping implementations are already being edited independently.
This repair preserves that work and does not weaken their assertions or claim
their failures resolved. Full combined-worktree validation remains outstanding.

Build attempts also consumed work: the first core build stopped with 11 errors
and one warning in incomplete concurrent `workspace-tools` work; the next was
interrupted; another stopped on a concurrent shell-spec delimiter error, since
repaired. A resumed build was deliberately stopped after an already-built
executable containing these changes became available, avoiding a full rebuild
under different compiler settings. Its timestamp was unchanged throughout the
focused, surrounding, and isolated test runs. Desktop `CODEX_*` process state
was cleared and the repository's 8 MiB test-stack setting was used. Unrelated
history dead-code warnings were observed and left untouched.

Local validation evidence is retained under `_build/of-comparison/` in
`classifier-focused.log`, `classifier-suite.log`, `projection-execution.log`,
`projection-suites.log`, and `projection-isolated-results.json`. The earlier
utility suite reflects its tested snapshot; unrelated tokenizer edits happened
later. No installed binary was replaced or Desktop restarted.
