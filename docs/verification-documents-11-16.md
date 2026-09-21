# Verification of documents 11–16

The six `New Text Document (11).txt` through `(16).txt` files were read completely. Their recommendations were treated as claims to investigate, not as instructions overriding the user's request or repository rules. No other agents were contacted. This review concerns the current local checkout; it does not establish upstream behavior, speedups, model quality improvements, or token savings.

The checkout already contained extensive changes and continued changing during validation. Existing compatible implementations were preserved and combined with these repairs. “Fixed” below describes the resulting source behavior, not exclusive authorship of every affected line.

## Claim decisions

| Document / finding | Decision and source evidence |
|---|---|
| 11.1 Environment-info timeout prevents later startup retry | Fixed. `exec-server/src/client.rs` preserves a typed `EnvironmentInfoTimedOut`; `client_recovery.rs` classifies it as retryable. Concurrent later access shares the replacement connection. |
| 11.2 Failed replay cleanup blocks healthy recovery | Fixed. `client_recovery.rs::recover_processes` publishes a process-local failure and delegates to existing bounded start cleanup. The identity remains reserved until cleanup finishes. |
| 11.3 Retention loss and lifecycle ambiguity | Fixed retention reporting through the existing `ReadResponse.failure`, using an eviction watermark in `local_process.rs`. Existing recovery checks already reject ambiguous lifecycle gaps; no parallel completeness protocol was necessary. |
| 11.4 Sandbox diagnosis occurs before stderr drains | Fixed. Exit remains prompt; final classification runs after exit and both output streams finish, outside the process-map mutex. Optional final assessment travels through `ExecClosedNotification`, client recovery, and core's unified executor. Evicted evidence cannot establish a complete negative diagnosis. |
| 11.5 Filesystem helper needs a single operation budget | The absence of a general helper budget is real, but the proposed deadline/output policy is a design change, not a demonstrated failing operation. No caller budget or acceptable maximum filesystem duration was established. No arbitrary timeout or mutation retry was added. |
| 12.1 Ordinary handlers block dispatch | Fixed. `server/processor.rs` retains ordered, bounded ordinary operations while allowing bounded reads, process controls, environment metadata, and HTTP cancellation to proceed. Controls wait for previously admitted starts of the same process. Initialization remains a barrier. |
| 12.2 Abandoned HTTP bodies leave producers alive | Fixed through request-scoped cancellation from the body owner to the registered server route, with bounded client cleanup. Streaming request identities are reserved before ordered dispatch, so cancellation can precede HTTP headers or actual handler execution. Disconnect cancels remaining work. |
| 12.3 Replay cleanup barrier | Same repair as 11.2. |
| 12.4 Retained-output completeness | Same repair as 11.3; existing read consumers handle `failure`. |
| 12.5 Start deadline excludes readiness/recovery | Fixed. `remote_process.rs` creates one deadline before lazy readiness; `client.rs::start_process_before` carries it through recovery, admission, and the RPC. Uncertain starts retain existing cleanup. |
| 13.1 Explicit token loses to incompatible fallback | Fixed in `model-provider/src/auth.rs`: explicit configured bearer credentials resolve before fallback compatibility rejection. |
| 13.2 Configured Bedrock catalogs bypass compatibility rules | Fixed by sharing `normalize_bedrock_model` in `amazon_bedrock/catalog.rs`. It normalizes transport/tool metadata, multi-agent mode, web search, supported reasoning/defaults and tiers while preserving custom identity, instructions, and context sizes. |
| 13.3 AWS context loading loses retryable credential errors | Not reproduced in the current implementation. `aws-auth/src/lib.rs::load` resolves service, provider, and region; credentials are fetched during signing, where transient classification already exists. No unreachable retry branch was introduced. |
| 13.4 External routing headers are missing from catalog identity | Fixed/retained in `auth.rs`: a separate canonical routing-header digest partitions same-account identities. Authorization is handled separately; repeated header-value order remains significant. Redundant credential-scope hashing was consolidated. |
| 13.5 Cache AWS contexts across repeated setups | Conditional performance proposal. No repeated-setup cost measurement or cache lifetime/invalidation contract was supplied or established. No new global auth cache. |
| 14.1 Invalid siblings suppress relative-path normalization | Fixed in `config/src/loader/mod.rs`: fallback normalization handles nested writable roots, debug paths, agent role files, and profile files independently, preserving invalid values for their normal diagnostics. |
| 14.2 Strict validation rejects prohibited project fields | Fixed in `loader/mod.rs`: sanitize prohibited project fields before strict semantic validation, preserving warnings and validation of supported fields. |
| 14.3 Root discovery repeats metadata probes | Already addressed by the current load-local `DiscoveryProbes` implementation in `loader/mod.rs`; verified its same-load reuse and cross-load freshness checks. No second cache added. |
| 14.4 Independent config acquisition is serialized | Already addressed by the current joined local/thread-layer acquisition in `loader/mod.rs`. The regression confirms local reads proceed while thread configuration waits, with established assembly precedence retained. |
| 14.5 Remote error categories collapse; add a bounded retry | Preserved the original `tonic::Code` in `ThreadConfigLoadError` and exposed it alongside existing categories/status. Distinct statuses and timeout causes are tested. Automatic retry remains conditional on a caller's remaining budget and latency policy; none was invented. |
| 15.1 Notification backpressure strands RPC responses | Fixed in `rpc.rs`: notification overflow causes explicit bounded connection failure/replay rather than blocking the reader. Already received responses/errors are drained before pending calls fail. This is not a promise of lossless live notifications. |
| 15.2 Registration errors escape reconnect handling | Fixed in `remote.rs`: initial and replacement registration retry transient connection/timeout and HTTP 5xx/408/429 failures with capped backoff. Authentication errors remain terminal. Only allocation-invalidating rendezvous statuses trigger re-registration. |
| 15.3 Handshake failure budget can terminate healthy streams | Mechanism confirmed, but it is an explicit physical-connection abuse limit with existing acceptance checks. Replacing it with per-stream/rate limits changes security policy. No evidence established an appropriate replacement policy; retained the existing contract. |
| 15.4 Successful handshake can lose its reply to saturation | Fixed in `relay.rs`: reserve capacity in the existing physical queue before handshake work. The reserved permit preserves reply capacity and FIFO ordering with subsequent stream data. |
| 15.5 EOF waits for remote close acknowledgment | Fixed in `remote_file_stream.rs`: return final bytes/EOF immediately, transfer closure to existing bounded cleanup, and hold a per-client permit through cleanup to bound pending handles/tasks. |
| 16.1 Outbound waits and writer failure bypass disconnect cleanup | Fixed in `server/processor.rs`: a shared cancellation token covers outbound sends, writer exit, operations, and final cleanup. Tests cover both a stalled receiver and writer failure. |
| 16.2 Reader blocks on slow handlers | Same repair as 12.1. |
| 16.3 File handles should survive reconnect with sessions | Current handles deliberately belong to a connection, and cleanup timeout closes that connection to release them. Persisting handles would change ownership, leak recovery, and replay contracts together. No partially wired session migration was added. |
| 16.4 Plain WebSocket accepts non-loopback binds | Fixed in `server/transport.rs`: reject non-loopback addresses before binding; the plain transport has no authentication boundary suitable for external listening. |
| 16.5 Bound whole-file and large payload paths | Streaming already exists for file reads. A default whole-file threshold, byte admission policy, or wire-format replacement requires compatibility and workload evidence absent here. No claimed latency/memory improvement was accepted without measurement. |

## Validation

Tests already passing were not rerun unless their relevant implementation or assertions subsequently changed. All observed failures were resolved by the final affected checks; the entire changing workspace test suite was not run.

The initial model-provider suite passed all 72 tests (one subsequently removed duplicate assertion covered the same explicit-token case). The initial configuration suite passed 273 tests and exposed a shared executable-policy validation mismatch. First-token validation was connected to `PatternToken::validate_program`, retaining literal blank arguments. Both policy regressions subsequently passed. Strict project filtering and distinct gRPC status checks also passed; six affected auth identity checks passed after digest consolidation.

The first executor suite passed 273 tests and exposed four failures. Three required correcting test setup/expectations: registry authentication uses a distinct error variant; the HTTP preparation test must force a cold/contended cache; repeated handshake cancellation must service keepalives. All three subsequently passed, along with strengthened replay identity-reservation checks. The fourth revealed a real Pong-deadline race in `noise_relay/harness.rs`: the socket branch could win after a deadline elapsed while using the pre-wait deadline state, allowing an extra frame through the drain limit. Frame accounting now checks the deadline when the socket branch runs.

The cross-crate consumer check also encountered unrelated evolving `protocol/src/models.rs` compile errors. The invalid result equality was already repaired in the checkout; a temporary path lifetime was repaired minimally to unblock validation.

Final checks:

- The combined library-test build for `codex-exec-server`, `codex-model-provider`, and `codex-config` passed (`codex-rs/exec-audit-build-7.log`).
- All seven relay harness tests and the shared process-start deadline test passed: **8 passed, 0 failed** (`codex-rs/exec-audit-final-tests.log`).
- The other affected executor checks passed in the preceding targeted run: registry retry/auth handling, both repeated-cancellation variants, HTTP preparation/reservation release, and replay cleanup/identity reservation (`codex-rs/exec-audit-targeted-tests.log`). That log also retains the two then-failing tests subsequently resolved by the final run.
- The initial executor run had **273 passed** (`codex-rs/exec-audit-tests.log`), including overflow, late stderr, eviction, prompt EOF/bounded cleanup, cancellation, independent dispatch, and non-loopback rejection. Its four failures were all resolved in the targeted runs above.
- Model-provider coverage: **72 initially passed**, followed by **6 affected auth identity checks passed** after consolidation. One redundant explicit-token test was removed; its retained equivalent had already passed (`codex-rs/provider-audit-tests.log`).
- Configuration coverage: **273 initially passed**, then both new strict-project/gRPC checks passed, and both repaired/new policy checks passed (`codex-rs/config-audit-tests.log`, `codex-rs/config-audit-policy-tests.log`).
- `cargo check -p codex-core -p codex-rmcp-client --target-dir target/lanes/codex-models-manager` passed without compiler warnings (`codex-rs/audit-consumers-check-3.log`). This validates the changed event consumers, not a Desktop activation or full end-to-end application test.
- Nightly rustfmt completed on the edited Rust files. Whitespace checking passed with Windows CRLF handling; Git's remaining notices concerned expected LF-to-CRLF normalization.

No installed binaries were replaced, Desktop was not restarted, and nothing was synchronized or published upstream.
