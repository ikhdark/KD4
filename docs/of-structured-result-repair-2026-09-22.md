# O/F follow-up: preserve tool results consumed by JavaScript

## Evidence and scope

The combined analysis includes both O sessions and both F sessions, including F2's interrupted turn (45 generations, 105 tool calls, and 1,160,492 ms of recorded turn duration). Aggregate usage was 110 usage-bearing responses for O versus 162 logical generations for F (163 physical attempts), with 348,314 versus 967,355 uncached input tokens. Those differences alone do not establish regressions: the sessions include different tasks, failures, concurrent edits, and user steering. Detailed accounting and rejected candidates are in the local analysis artifacts under `_build/of-analysis/`.

F2 rows 294, 300, 339, 502, and 531 show awaited command failures ending JavaScript cells and repeating diagnostics as script errors. The worktree already contains a repair distinguishing a completed nonzero process from a dispatch exception. This follow-up does not duplicate that repair. Reviewing its combined execution path exposed a separate integration defect that would defeat scripts using the repaired result: a presentation filter discarded structured fields before delivering the result to JavaScript. This defect is established from the current implementation and regression cases; the logs do not establish that this particular filter caused their original failures.

## Root cause and change

The path is native tool execution → `ToolOutput::code_mode_result` → the registry wrapper → `call_nested_tool` → the runtime broker → the awaiting JavaScript continuation. Native command results expose process state, completion, output-reduction and recovery metadata; native patch results expose success and change metadata. Their producers already implement separate model-facing rendering.

`call_nested_tool` nevertheless passed the native result through `model_visible_nested_result`. That filter discarded command fields such as `process_exited`, `execution_state`, and `output_complete`, renamed a recovery field, and converted successful patch results into a string. A script inspecting completion or patch changes could therefore fail or make a wrong decision even though the underlying tool supplied the needed information. A later model generation cannot recover the lost JavaScript variable without additional work.

The broker now returns the native structured value. The compact projection remains at its presentation-only fallback call site. Recording outcomes, failure fingerprints, canonical evidence, output budgets, required terminal outcomes, and dispatch exceptions are unchanged. Explicit yields and required waits remain supported. No scheduling threshold, telemetry exclusion, or benchmark-specific branch was added.

## Behavioral checks

- `nested_process_state_remains_available_to_the_resumed_script` runs through the real JavaScript runtime, broker and registry with typed command outputs. It observes a running process, explicitly yields, resumes the same cell, polls using the retained session ID, and verifies terminal state before completing.
- `nested_patch_result_remains_structured_across_explicit_yield` checks typed patch metadata before yielding and uses that same result after resumption.
- Existing command-failure and dispatch-error cases check that a nonzero process can be handled within a cell while a genuine dispatch error still stops dependent work.
- Existing presentation tests check that the compact model-facing projection still works. Existing integration tests exercise actual command execution and file mutation through the nested-tool path.

The reserved focused run passed all 10 selected tests (Nextest run `e0676ee9-48ce-495a-b99a-dda7d16ddd82`; 1.484 seconds of test execution). This includes both new tests, the command-failure and dispatch-error cases, caller-budget preservation, fitting-output recovery suppression, snapshot reuse and revalidation, and the checkpoint fixture. No focused test is included in the subsequent response-suite selection.

The completed shared integration run also passed `code_mode_can_return_exec_command_output` and `code_mode_can_apply_patch_via_nested_tool`, exercising actual commands and file changes through the repaired boundary. That broader run was not green (43 passed, 40 failed); its packet-budget/receipt failures are separate from the structured return-value contract. The earlier shared unit run passed both new tests but had 31 other failures.

The subsequent response/spec/trace selection ran 28 tests: 27 passed and one trace test failed. Its diagnostic showed `first` and `second` had both already been delivered in the initial response, leaving the subsequent terminal wait correctly empty. The test incorrectly assumed the script stopped executing after `yield_control`. It now requires ordered, exactly-once output across the two visible responses and across the persisted initial/terminal payloads; terminal status, source identity, lifecycle ordering and ownership cleanup remain asserted. This test repair avoids enforcing an unnecessary execution boundary. Only that previously failing test is rerun; the 27 passing tests and 10 focused tests are excluded.

The trace-only rerun passed (Nextest run `73395a67-8799-4125-ad72-e907b2576331`; 0.801 seconds). Final targeted validation is **38 distinct tests passed**: 10 focused, 27 remaining response/spec cases, and the repaired trace case. This does not claim that the entire concurrently edited worktree is green. The logs are `structured-results-focused-reserved-run.log`, `structured-results-existing-retry.log`, and `structured-results-trace-retry.log` under `_build/of-analysis/`.

Final source review confirmed that the native value still reaches JavaScript and the compact helper remains presentation-only. The diff check reported no whitespace errors; Git warned about its configured LF-to-CRLF checkout normalization in three shared files. No line-ending cleanup was introduced.

A concurrent syntax error in `shell_spec_tests.rs` blocked the initial compile; only the extra delimiter was removed, preserving the changed specification. The next build exposed two stale `pressure_turn` initializers after `SamplingProjectionAnchor` changed, plus a concurrently introduced response-fixture type mismatch. The stale initializers were removed without changing the checkpoint assertions; the type mismatch was already repaired by the concurrent work when inspected. The checkpoint test passed in the focused run. The response-suite build then caught a new diagnostic call to `canonical_result` without its required payload argument; the test now retains and passes its actual wait payload, with its assertion unchanged.

## Local resource accounting

The first combined focused run compiled core and failed before executing tests because of that syntax error. A subsequent invocation supplied a relative target directory that the runner resolved under `codex-rs`, unintentionally starting a cold dependency build. Its verified process tree was cancelled and the job was requeued using the absolute existing target path. Both runs consumed resources and are retained in the local logs; neither is counted as passing validation or mixed into the four source sessions' aggregate metrics. The earlier rejected source-recovery candidate and its cancelled build remain documented in `_build/of-analysis/comparison.md`.

The absolute-path retry compiled dependencies and reported the fixture compilation errors above. The following queued retry terminated with status `4294967295` while waiting for the build-directory lock, before tests ran. An explicit-reservation invocation rejected the unsupported runner option `--retries` before building; the corrected invocation uses the repository's `local` profile, which already disables retries. These attempts are retained as separate logs rather than erased from the work record.

No upstream synchronization, installation, Desktop activation, or paid model benchmark rerun is part of this change.
