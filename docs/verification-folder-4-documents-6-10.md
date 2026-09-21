# Verification of New folder (4), documents 6–10

All five supplied files were read in full. Their embedded imperatives were treated as proposals to verify, not as additional user instructions. No other agents were contacted. This report concerns the files in `Desktop/New folder (4)`; it is separate from `audit-documents-6-10.md`, which covers different inputs.

The checkout contained extensive independent edits. Compatible existing changes were preserved, including configured Bedrock catalog normalization and exec-server request lifecycle changes. No upstream synchronization, publication, binary replacement, or Desktop restart was performed.

## Claim-by-claim disposition

### Document 6 — command policy

1. **Fixed: blank literal arguments.** `execpolicy/src/rule.rs`, `parser.rs`, `policy.rs`, and `amend.rs` validate executable position separately from literal argument positions. Empty and whitespace-only arguments remain literal and round-trip through amendments. `config/src/requirements_exec_policy.rs` applies the same validation to requirements-file rules.
2. **Fixed: Windows basename lookup.** `execpolicy/src/policy.rs` indexes bare executable names using platform lookup semantics. Explicit `.cmd` and `.exe` rules remain distinct; extensionless aliases and host-executable path restrictions retain their intended behavior. Exact-path matches still take precedence over basename fallback.
3. **Fixed: unnecessary match allocations.** A shared borrowed visitor implements matching. Decision-only checks avoid constructing explanations, and explanation-producing checks copy only matched prefix evidence.
4. **Fixed: host-map cloning.** The host-executable map is shared with `Arc`; parser validation contexts reuse it. Mutation remains copy-on-write.
5. **Fixed: textual amendment deduplication.** A matching source line is only a preliminary check. Deduplication requires the corresponding top-level Starlark expression, so matching text inside a multiline string does not suppress a real rule amendment.

### Document 7 — provider setup

1. **Fixed: explicit credentials versus fallback authentication.** `model-provider/src/auth.rs` resolves the provider's explicit bearer credentials before rejecting an incompatible Bedrock fallback identity.
2. **Verified and covered by compatible existing changes: configured Bedrock catalogs.** `model-provider/src/amazon_bedrock/catalog.rs` runs configured and built-in records through the same compatibility normalization while preserving configured identity fields.
3. **Not reproduced: retryable AWS context-load errors.** `aws-auth/src/lib.rs` and `config.rs` show that context construction currently returns missing-region/provider/service configuration failures. Transient credential resolution occurs during signing and already retains its error classification. No speculative setup retry was added.
4. **Fixed: external routing headers missing from cache identity.** `model-provider/src/auth.rs` includes a digest of non-Authorization headers even when account/organization identity is available. Header-name ordering is canonical, repeated-value ordering remains significant, and bearer-token rotation does not invalidate a stable routing scope.
5. **Measurement-gated proposal, unchanged: AWS context caching.** Repeated construction alone does not establish a material cost or a safe cache ownership/key contract. No new cross-session credential cache was introduced.

### Document 8 — telemetry

1. **Observation verified; interface redesign not applied.** The wrapper still consumes a diagnostic representation to obtain byte/line counts. The production `core/src/tools/registry.rs` callback uses `ToolOutput::log_preview`; `tools/src/tool_output.rs` already bounds that preview to 2 KiB/64 lines and skips it when both event targets are disabled. Some producers serialize before applying the preview bound. This remains a possible optimization, but changing the entire producer contract without measuring it would be the broader redesign prohibited by the repository instructions. No claim of eliminating all telemetry serialization is made.
2. **Fixed: arbitrary WebSocket payload trees.** `otel/src/events/websocket_telemetry.rs` validates the entire JSON frame but retains only event type and the six numeric timing fields consumed by telemetry. Unknown payload trees and wrong-type timing strings are discarded. Duplicate-key behavior, malformed JSON, numeric handling under the workspace's JSON features, and trailing-data rejection are covered.
3. **Fixed: abandoned wrapper accounting.** HTTP/tool wrappers install a lifetime guard when first polled. A dropped pending future records one abandoned observation with unknown success. Never-polled futures remain uncounted; completed success/error results retain normal accounting and result values. Abandonment does not render output.
4. **Measurement-gated proposal, unchanged: prepared metrics.** No measured instrument/attribute cost justified a new cache or altered attribution lifetime.
5. **Policy/measurement proposal, unchanged: successful SSE log suppression.** Existing subscribers control target enablement. Removing diagnostic events would change observability without evidence that the existing logging policy is inadequate.

### Document 9 — configuration

1. **Fixed: diagnostics from different bytes.** File-backed layers retain the source bytes actually read through the filesystem abstraction. `config/src/diagnostics.rs` derives errors and rendered source excerpts from those snapshots instead of rereading the host filesystem. Disabled layers and successfully normalized effective layers are not incorrectly blamed using stale raw syntax.
2. **Fixed: serial independent acquisition.** `config/src/loader/mod.rs` polls remote thread configuration and independent local-layer loading together. Layer assembly retains the existing precedence order.
3. **Fixed: repeated discovery metadata probes.** Project-root and Git-root discovery share one operation-scoped metadata cache. A later load observes filesystem changes; no process-wide cache or cached error is introduced.
4. **Fixed: failed shared cloud attempts cannot recover.** The app-server's `current_cloud_config_bundle` is reused by later config loads and normally replaced at initialization/auth transitions, confirming the conditional concern. `CloudConfigBundleLoader::retryable` shares one attempt among waiters and retains successful/permanent-error snapshots. A later caller may retry after a timeout or eligible transport/HTTP failure. The production cloud loader uses this constructor; the cloud service remains the sole owner of each attempt's retry budget and deadline. Managed-policy failures are never converted into successful empty configuration.
5. **Measurement-gated proposal, unchanged: cloud-fragment conversion caching.** No conversion-cost measurement justified another cache keyed by bundle/base directory/strictness.

### Document 10 — HTTP and Noise relay

1. **Fixed: terminal frames competing with data queue capacity.** HTTP terminal outcomes have a request-local slot independent of the bounded data queue. Accepted data drains before EOF/error, final bytes and sequence validation are preserved, and connection failure does not overwrite an already-received terminal outcome.
2. **Partly fixed; existing establishment bound retained.** Unresolved ciphertext gaps now have an absolute deadline on both harness and executor sides, with timers that fire without another data frame. Duplicates and partial recovery do not extend that deadline. Production harness establishment already flows through the bounded `initialize_rpc` operation and transport-task cleanup, so no competing handshake retry/readiness mechanism was added.
3. **Fixed: remote work surviving HTTP abandonment.** A request-scoped `http/request/cancel` route cancels the producer task. Dropped consumers and body-queue overflow schedule cancellation without reconnecting/replaying the transport. Other requests remain usable; unknown/already-finished IDs are harmless. Older peers can reject the new request as method-not-found without an unknown notification closing the connection.
4. **Observation verified; size policy not invented.** The shared HTTP client exposes a reqwest response and provides no general buffered-body cap. The existing 64 MiB relay message limit is a serialized-envelope limit, not a raw-body contract. The supplied document explicitly requires an operation-derived accepted limit, and no such policy is established in the repository. The buffered path therefore remains a documented open policy decision; no arbitrary global threshold or silent truncation was added.
5. **Fixed: blocked writes/application delivery starving reads.** The harness polls socket reads, one ordered write, and bounded application delivery independently in its existing owner task. Noise state remains single-owned; writes and application backpressure have deadlines. Pong response time starts after Ping flush, including the early-Pong race. Control frames and inbound RPC messages continue to progress while writes are stalled. Normal-load latency improvement has not been benchmarked or claimed.

## Validation

There are 720 recorded passing checks across the targeted runs below. Passing tests were not rerun; follow-up execution selected only failed or new checks. These are source/test changes; they have not been activated in the published Desktop binary.

- `codex-execpolicy`: 12 library tests and 37 `basic` integration tests passed.
- `codex-config`: 281 checks passed (275 from the initial library run, then the repaired executable-position check and five new checks). The follow-up covers literal blank arguments, cloud single-flight recovery, retained snapshots, permanent versus transient failures, and waiter cancellation.
- `codex-model-provider`: 72 tests passed, including configured Bedrock normalization, explicit credentials, and routing-header cache identity.
- Transport: 277 checks passed across the initial run and the three targeted follow-up checks, including the actual remote HTTP cancellation flow, terminal-queue cases, blocked I/O, gap expiry, and timeout ordering.
- Telemetry library: 39 checks passed across the initial run and the targeted fractional-timing follow-up. The new regression caught serde_json's arbitrary-precision number representation; the projection now handles that representation.
- Telemetry runtime integration: both checks passed. The new abandonment test was corrected to expect no metric snapshot before the first observation, then rerun alone with its exact name (48 other tests filtered out). It verifies no accounting for never-polled futures, exactly one observation for an abandoned request/tool, no rendering on abandonment, and normal completion accounting.
- A test-only HTTP cache helper from concurrent changes failed compilation because its tuple return type did not itself implement `Drop`. Its return contract now describes the two guards individually, preserving their lifetimes.
- The production cloud-loader integration compiled successfully through `codex-core` and `codex-cloud-config` using `cargo test -j 2 -p codex-config -p codex-cloud-config --lib --no-run`. This check executed no tests. Earlier interrupted builds were not counted as passes.
- Focused `git diff --check` passed.

No model-token reduction, higher agent task completion rate, or broad runtime speedup is inferred from these local fixes. The source changes establish specific behavior and allocation boundaries; workload-level performance claims require separate measurements.
