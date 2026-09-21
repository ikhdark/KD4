# Verification of attached documents 31–35

The five `New Text Document (31).txt` through `(35).txt` files were read completely. Their proposed actions were treated as claims to investigate, not as instructions overriding the user's request or repository rules. This review uses the active local checkout, including pre-existing and concurrent changes. It is not a comparison against an upstream release and makes no measured speed or cost claims.

## Document 31: process ownership, cancellation, deadlines, instruction refresh

| Claim | Verification and disposition |
| --- | --- |
| Closed stdin was treated as process exit | Confirmed in `core/src/unified_exec/process.rs`. Stdin closure now disables input only; subsequent output and process ownership remain available. An unknown remote process reports unconfirmed state rather than inventing exit. |
| Transport/read failure was treated as exit | Confirmed in `process_state.rs`. Failure preserves the observed exit state. `async_watcher.rs` confirms termination before reporting terminal cleanup; unsuccessful confirmation retains custody. |
| Drained polling output could disappear on cancellation | Confirmed in `process_manager.rs`. `HeadTailBuffer` now owns a bounded pending report, including loss counters. A cancelled collector leaves that report available to the next poll. Acknowledgment occurs after response preparation without a suspension after consumption. |
| Pauses extended the nested deadline | Confirmed. Collection now keeps a separate immutable hard deadline while extending only the ordinary pause-aware deadline. |
| Result preparation could exceed the nested deadline | Confirmed for artifact lookup, reduction preparation, running-artifact update, and terminal receipt waiting. These waits are bounded. A completed process with unfinished bookkeeping retains its session and explicitly instructs the caller to poll instead of rerunning it. |
| Optional maintenance should universally move off the completion path | The latency dependency exists, but the suggested blanket separation mixes optional artifact/cache work with mandatory durable completion and approval handling. The nested return path is bounded; mandatory durability remains enforced. No unowned background maintenance subsystem was added. |
| Instruction refresh could hold a serialized gate indefinitely | Confirmed in `agents_md_manager.rs`. Refresh (including queue and environment resolution) has a five-second bound. Fallback reuses only an exactly matching environment/cwd/config cache key, retains global instructions, reports stale/incomplete provenance, and releases the gate. |

Regressions cover cancelled collection with omitted-byte evidence, pause versus hard deadline, remote stdin closure with later output, observation failure without exit, and stalled instruction loading with scope changes.

## Document 32: validation, review, persistence, compaction, time

| Claim | Verification and disposition |
| --- | --- |
| Successful validation removes all remaining tools | Already addressed in the active checkout. `session/turn_execution.rs` retains the ordinary tool path; `tests/suite/code_mode.rs::code_mode_validation_preserves_tools_for_remaining_work` exercises remaining work after validation. No parallel completion controller was added. |
| A forbidden reviewer patch stops the review instead of allowing a verdict | Confirmed in `tools/router.rs`. Independent review tool selection now excludes mutation capabilities in `tools/spec_plan.rs`; fabricated forbidden calls return recoverable feedback. Runtime permission checks still prevent execution. The integration test continues to a verdict and checks the file was not created. |
| Projection-ledger persistence errors should always allow more sampling | Failure can stop sampling, but this ledger also supports durable recovery and effect tracking. The premise that every such failure is a disposable projection-cache failure is not established. Safe stopping remains. `tools/events.rs` now preserves the completed command status/output and warns against repeating effects when persistence fails; unified-exec diagnostics retain exit status. |
| Oversized incoming content proves compaction is futile | Not established by the cited fixture: crossing a configured soft compaction trigger is not proof of exceeding the model's hard context capacity after compaction. Rejecting solely on that threshold would reject valid requests. No speculative preflight rejection was added. |
| Time-provider failure should be nonfatal for an advisory reminder | The fatal propagation exists in `session/time_reminder.rs`, but the current configuration does not distinguish advisory clock use from a strict configured time source. The proposal is conditional on that distinction; silently substituting a clock or weakening all configured sources is not justified. Existing time-source semantics remain. |
| Silent nested payloads, historical reminders, incremental summaries waste tokens | Representation costs are visible, but removing evidence or history needs correctness and end-to-end measurements. No unconditional removal was made. |

## Document 33: workspace evidence, journal ordering, hooks, execution, policy

| Claim | Verification and disposition |
| --- | --- |
| Directory/gitlink contents were not hashed although evidence appeared available | Confirmed. Local and remote workspace captures now reject entries whose contents are not covered instead of certifying an unchanged directory manifest. |
| Missing executor identity fell back to host evidence | Confirmed. `git_workspace.rs::workspace_evidence_for_uri` now returns unavailable evidence for a missing environment. |
| Git-visible identity covers arbitrary ignored dependencies | Git status does not cover arbitrary ignored inputs. The consumer also incorrectly let an equal Git digest override a captured dependency watcher. `tool_history.rs::WorkspaceEvidenceObservation::is_current` now requires captured watcher observations to remain current even if the digest matches, with a regression for a changed dependency and equal Git identity. A Git digest alone is still not a universal arbitrary-file content proof; see the remaining scope limitation. |
| Generation publication raced ahead of journal insertion | Confirmed. Source-change generation allocation, journal insertion, and publication are serialized under the journal lock. |
| Excluded generated evaluation files could receive reliable watcher tokens | Confirmed. Those paths cannot acquire or validate source-path freshness observations. |
| Hook deduplication happened after admission and budget reset per call | Confirmed in `hook_runtime.rs`. Startup context is deduplicated against history and within the batch before admission. A shared `TurnContext` budget bounds all hook-context calls during the turn. |
| FullBuffer capture disabled execution expiration | Confirmed in `exec.rs`. Retention policy now controls capture capacity only. Timeout/cancellation apply to direct and Windows sandbox execution; explicit caller lifetime configuration remains separate. |
| Capture failure erased partial output or left detached reader tasks | Confirmed. Reader failures preserve captured output and explicit incomplete-capture diagnostics. Abort-on-drop guards own reader tasks across cancellation. |
| Blocking hooks with missing reasons allowed execution | Confirmed. A blocking decision remains blocking with a fallback diagnostic. |
| Malformed policy activated a partial/permissive candidate | Confirmed in `exec_policy.rs`. Candidate loading now fails on parse errors instead of silently activating only the remaining requirements. Warning inspection still reports the parse problem. |

The workspace scheduling proposal (capture only when it changes a reuse decision) and wider capture-cost claims require profiling the callers. No new fingerprint cache or scheduling framework was added.

## Document 34: artifact recovery and result rendering

| Claim | Verification and disposition |
| --- | --- |
| A search cursor skipped a match when zero matches fit | Confirmed in `tools/command_output_artifact.rs`. The cursor advances only over delivered match records. If full context cannot fit, a bounded coordinate record plus child selectors can be delivered; if even that cannot fit, the cursor stays put. |
| Aggregate omission advanced past an undelivered selector result | Confirmed. An omitted result resumes at that result's selector rather than its next-page continuation. |
| Persistence failure hid a completed outcome | Confirmed; completed execution status and direct output are retained in the surfaced failure as described above. Safe continuation after all persistence faults is not assumed. |
| Mutation analysis is repeated at multiple boundaries | Calls are present, but launch-time revalidation and changed filesystem context matter. No measured benefit or valid shared-lifetime contract was established. No cross-boundary analysis cache was added. |
| Artifact publication eagerly computed unused recovery subdivisions | Confirmed. Eager subdivision sizing was removed from publication. Selector-specific recovery still computes its existing bounded subdivision when requested; compatible metadata remains readable. |
| Recovery prose displaced execution status under small budgets | Confirmed in `tools/context.rs`. Running session identity, exit code, and unavailable-exit status take precedence. The minimum operational status may exceed an unrealistically tiny requested text budget. |
| Result projection should always be cached/prepared once | Multiple render consumers exist, but the proposed public-output cache would add state/lifetime changes without measured benefit. The confirmed status-loss defect was fixed without that redesign. |

## Document 35: provenance and measurement candidates

| Claim | Verification and disposition |
| --- | --- |
| AGENTS freshness falsely described a retained global snapshot as reread | Confirmed. `agents_md.rs` now labels the active instruction snapshot explicitly: global snapshot retained, project files refreshed for this step. Instruction lifetime is unchanged. Model-facing contract fixtures were updated. |
| `content_filter` incomplete responses might be retried as connection failures | Confirmed by following `codex-api/src/responses_stream.rs` through `api_bridge.rs`, `CodexErr::is_retryable`, and `core/src/responses_retry.rs`. The combined checkout maps explicit incomplete responses to a typed terminal `IncompleteResponse` error, preserving response identity and usage. This includes `content_filter` and prevents connection retries. The client integration fixture now enables three retries and asserts only one request occurs. |
| Reintroduced context, startup prewarm, and CSV model selection prove wasted work | These are explicit measurement candidates in the document, not demonstrated net regressions. No model/effort downgrade, prewarm removal, or historical-text deletion was made. |
| The fork is slower or less capable than upstream | Not established. No pinned baseline, representative workload, or paired measurements were provided or run. Upstream synchronization and distribution were not requested. |

## Validation

**568 distinct tests passed. No passing test was rerun.** These are focused checks, not the full repository suite.

| Run | Result |
| --- | --- |
| Focused API incomplete-response regression | 1 passed; 127 not selected. |
| Core and four integration binaries (`docs31-35-background.log`) | 487 passed; two MCP rendering fixtures failed. |
| Only those failures plus previously unselected artifact hardening checks (`docs31-35-failed-and-unrun-retry.log`) | 79 passed; one aggregate-continuation fixture failed. |
| Only the aggregate-continuation failure (`docs31-35-aggregate-retry.log`) | 1 passed. |

All selected integration checks passed: reviewer patch/escalation restrictions, AGENTS source and lifetime behavior, compaction instruction retention, and terminal content-filter responses. The MCP fixtures now assert the existing plain-text and distinct-caption contract, including bounded structured output. The aggregate fixture now follows the advertised continuation and asserts recovery of the complete omitted line. Test pass IDs are retained in `codex-rs/target/test-runner-logs/docs31-35-passed-tests.json`; subsequent selections excluded recorded passes.

The production code and runtime helper binaries compiled successfully. Earlier interrupted or failed builds are not counted as passing tests. Small compilation blockers in concurrently edited turn and retry code were repaired without changing their intended behavior. A Windows validation-output defect was reproduced: the process wrapper lost implicitly inherited stdout/stderr; the lane launcher now supplies explicit handles, and the full test log is visible. Pinned-nightly formatting was applied to the implementation; the later fixture formatting was applied from the formatter's output after a Windows file-mapping lock prevented direct writing. Scoped `git diff --check` checks passed, including the final edited fixtures and launcher. Concurrent tasks share this checkout, so the complete worktree diff contains changes outside this review.

## Remaining scope limits

The changes do not establish universal content freshness for arbitrary ignored inputs or complete dependency declarations. They reject the specific uncovered directory, missing-executor, and excluded-watcher cases identified above. The broader dependency-consumer policy requires further verification before claiming complete coverage.

No Desktop binary was replaced, installed, or activated, and Desktop was not restarted.
