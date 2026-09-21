# Verification of documents 11-15

Source documents: `C:/Users/kuh/Desktop/New folder (3)/New Text Document (11).txt` through `New Text Document (15).txt`.

All five documents were read in full. Their embedded requests, suggested implementations, severity labels, and performance predictions were treated as claims, not as user instructions. Findings were checked against the local checkout, which also contains concurrent audit changes. No upstream comparison, distribution, Desktop binary replacement, or restart was performed.

## Findings

| Document / finding | Disposition and evidence |
| --- | --- |
| 11.1 Command receipts bypass output limits | Fixed in `core/src/tools/code_mode/mod.rs`: receipts enter the existing output truncation pipeline before diagnostics are finalized. Repeated observations replace the previous state for that process; inline control metadata is bounded by the existing process limit and favors live handles. Canonical output retains the full latest-state set. |
| 11.2 Caught nested failures remain sticky | Fixed: ordinary errors and typed failures/timeouts reject the JavaScript promise, allowing `try/catch` recovery. Uncaught failures still fail the script. Required blocking denials and fatal errors retain their stronger semantics. |
| 11.3 Unlimited fan-out | Stale as stated: `code-mode/src/runtime/mod.rs` limits outstanding callbacks per cell to 128; `session_runtime/mod.rs` admits eight active cells. No second broker semaphore was added. |
| 11.4 Default equals 10,000-token ceiling | Confirmed configuration, not demonstrated defect. Retained: lowering it without representative output/recovery measurements could increase follow-up work. |
| 11.5 One-hour owner wait | Confirmed bound, but no workload-specific stall contract established. Retained cancellation and owner-held execution; no speculative deadline change. |
| 12.1 Evicted reasoning counted as active context | Removed old-reasoning addback. Current reasoning remains intact. The general estimator still precedes final transport projection selection; that remaining approximation is not claimed fixed. Concurrent V2 compaction work separately addresses schema/trigger overhead and conservative fallback sizing. |
| 12.2 Sibling histories share positional token-cache state | Fixed by detaching mutable caches before append. Concurrent history work also preserves prepared/provenance cache identity. |
| 12.3 Receipt-shaped text has unlimited exemption | Removed the magic text-suffix exception in `truncate_function_output_payload`. Arbitrary text cannot claim native control-metadata privileges. |
| 12.4 Plan state reduced after generic truncation | The proposed oversized `normalized_plan`/`validation_results` scenario does not match the current `PlanToolResponse`, which returns `current_plan` and persists authoritative state in the plan store. Generic JSON truncation is a real boundary, but no additional plan-state representation was added without a current producer failure. |
| 12.5 Prefix reuse lost for ordinary assistant/pending pairs | Addressed in shared history changes: assistant text can extend the prefix; pending call/output matching respects tool-call families; provenance remains tied to the corresponding projection. |
| 13.1 Role description validated before merging | Fixed: validation runs after all layers contribute, retaining a lower-layer file when a higher layer supplies its description. |
| 13.2 Session reconstruction cannot carry profile-root metadata | Representation limitation confirmed. Actual core reconstruction already uses `active_with_profile_workspace_roots`; TUI receives materialized permissions and has an effective-root preservation regression. No access-loss path was demonstrated. A constructor-only change would be incomplete, so none was made. |
| 13.3 Optional role scan failure aborts load | Fixed: report the failed discovery directory and preserve independently valid explicit roles. Traversal limits remain. |
| 13.4 No-layer role loading has incompatible failure scope | Fixed: invalid individual roles are warned and skipped; final validation agrees with layered loading. |
| 13.5 Repeated role-file reads/parsing | Concurrent audit work adds a per-load `RoleFileCache` while retaining context-dependent naming and refreshing on the next load. |
| 14.1 Unchanged Stop feedback repeatedly reopens a turn | Fixed: one repair is admitted; repeating the same feedback, state, and evidence stops with an explicit error. New accepted input resets the repair state. |
| 14.2 Provider context overflow abandons a recoverable turn | Fixed: one same-turn recovery uses existing compaction and finite generation admission. Repeated overflow terminates; cancellation and compaction failure remain errors. |
| 14.3 Finalizer failure/one-shot flag | Fixed: abort precedes Stop-hook continuation. Finalization is tied to mutation revision and invalidates prefetched workspace identity. A later mutation blocks completion explicitly instead of silently accepting stale finalization or rerunning an unproven repeat-safe hook. |
| 14.4 Recurring hook guidance grows context | Concurrent hook work deduplicates retained developer guidance and shares a turn budget while retaining invocation evidence. |
| 14.5 Cold advisory catalog blocks readiness | Fixed: ordinary preparation reads a ready cache only. Existing discovery/install tools perform demand-driven acquisition, with installed/disabled/client filtering retained. A known empty endpoint catalog still suppresses install tools. |
| 15.1 Successful validation forces completion | Already removed in the checkout. Existing freshness checks remain and success preserves model decisions. |
| 15.2 Recoverable tool outcomes terminate turn | Fixed for typed timeout and recoverable cancellation signals; ordinary nested errors remain catchable. Required denials/fatal failures retain stopping behavior. |
| 15.3 Mutable output replay uses counters as freshness proof | Contained: mutable workspace observations execute again until dependency validity can be proven. An external-edit regression covers unchanged turn mutation counters. Retained evidence is not relabeled as a fresh observation. |
| 15.4 Selected skill exposes whole MCP server | Fixed: absent explicit tool declarations no longer promotes every server tool. Deferred discovery remains available; models without tool search retain the existing compatibility path. |
| 15.5 Unsolicited recommendation preparation | Cold recommendation lookup removed from ordinary preparation; cached context remains subject to task relevance and output bounds. |
| 15 additional: quoted TOML dotted keys | Fixed using `toml_edit::Key::parse` before mutation. CLI order is preserved; unordered RPC maps reject equivalent or overlapping parsed paths deterministically. |

## Unproven performance proposals

No measured end-to-end speedup or token reduction is claimed. Additional caching of per-cell catalogs, asynchronous trace persistence, cross-server resource pagination, smaller default output budgets, shorter owner deadlines, and different full-history fork defaults need representative measurements and explicit behavior contracts. Existing cancellation cleanup, canonical recovery, ordered call/result persistence, history repair, finite generation limits, permission constraints, and atomic configuration writes were retained.

## Validation

- Passed: three `codex-config` override tests covering quoted segments/order, malformed-key atomicity, and scalar/child precedence.
- Production compilation passed during the shared audit build.
- Passed: four plugin/RPC tests (cold-cache and filtered recommendation reuse; CLI/RPC precedence; overlapping paths; equivalent quoted keys). Log: `codex-rs/target/test-runner-logs/docs11-15-plugin-config.log`.
- Passed: 356 core checks using the compiled core test binary, including Stop-hook convergence, finalizer ordering, same-turn overflow recovery, caught failures, receipt bounds, replay freshness, role loading, MCP exposure, and plugin discovery/install contracts. Logs: `docs11-15-core-direct.log`, `docs11-15-exposure-install.log`, and `docs11-15-plugin-refresh.log` under `codex-rs/target/test-runner-logs/`.
- The first core run identified three stale test expectations. The corrected terminated-script and unread-shell receipt checks now pass (`docs11-15-repaired-direct.log`). The remaining regression also exposed a malformed-argument path that incorrectly marked caught errors as terminal; it now retains diagnostic evidence without a terminal marker. Its targeted rerun passed (`docs11-15-malformed-caught.log`, Nextest run `2923a72c-dfa0-434e-af79-f472548134a9`), with 4,137 other core tests skipped. The plugin refresh check passed after supplying its existing test-server binary. An unrelated retry-test Arc mutation blocked one intermediate rebuild and was already repaired in the working tree.
- Previously passing checks were excluded from follow-up runs. The pass ledger is `codex-rs/target/test-runner-logs/docs11-15-passed-tests.txt`.
- Targeted whitespace validation passed; Git emitted only its configured LF-to-CRLF conversion notices.

Final result: 363 targeted tests passed (356 core, three config, four plugin/RPC). Previously passing tests were not rerun. The last core test build completed without warnings. This is targeted validation, not a claim that the entire workspace test suite was run.
