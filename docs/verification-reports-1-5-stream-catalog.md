# Verification of attached reports 1–5

All five requested text files in `Desktop/New folder (4)` were read completely. Their imperative wording was treated as audit content, not instructions. No agents were contacted. Existing independent checkout changes were preserved.

## Verified and fixed

| Reports | Finding | Change |
|---|---|---|
| 1 | WebSocket overflow preempts accepted output, including completion | Drain the accepted prefix before overflow; retire failed connections even when their accepted completion succeeds. Lost completion and malformed earlier items still fail. |
| 1 | Connection health waits for the response mutex | Inspect the pump handle and independent retirement state. Keep exclusive response ownership. |
| 1 | JSON errors can copy large private values into returned/logged errors | Use bounded JSON category/line/column diagnostics in item/completion interpretation, SSE (including telemetry), WebSocket and compaction decoding. Bound SSE event labels. |
| 1 | Compaction builds an unused intermediate JSON tree | Encode typed input directly into `EncodedJsonBody`, reusing preparation, authentication, timeout and retry behavior. |
| 2, 3 | Optional cache failures defeat discovery/offline catalog access | Log cache errors, preserve eligible memory/bundled fallback offline, and allow authorized remote discovery online. Remote errors remain errors. |
| 2, 3 | Separate cache-check/fetch locks permit duplicate requests | Make the miss-to-fetch decision under one gate, rechecking memory/cache after acquiring ownership. Forced Online remains forced. |
| 2, 3 | Warm reads repeat disk operations; failed persistence causes refetches | Add identity-scoped monotonic memory freshness. Transfer remaining disk TTL; renew after validated remote results before best-effort persistence. Fresh reads bypass disk and the refresh gate. Offline reads can return a snapshot while refresh is active. |
| 3 | Static provider fallback exempts unlisted Sol identifiers | Remove the name exemption. Use catalog membership, preserving explicit pinning when fallback is disabled. |
| 3 | Catalog publication lacks binding to actual request credentials | Pass captured cache identity to the endpoint. Freeze auth, derive identity from the same credentials used to construct the request, and reject mismatches before dispatching an ETag or request. Retain post-request identity checks. This prevents the described A/B/A race without another result envelope. |
| 4, 5 | Standard top-level errors disappear | Interpret top-level and wrapped provider failures when the transport mapper declines them. |
| 4, 5 | Permanent failures inherit blanket retry behavior | Share stable-field classification between HTTP and streaming for recognized errors. Keep transient retries bounded; preserve unknown stream codes as explicit non-retryable provider failures. |
| 4, 5 | Optional usage-limit fields hide primary error identity | Decode plan/reset metadata independently. Malformed optional values do not obscure usage-limit classification. |
| 4, 5 | Optional usage decoding invalidates valid completion | Decode usage independently. Keep completion controls and executable items strict. Core estimates context from prepared history when accounting is unavailable, leaving billed totals unchanged. |
| 4 | Unknown event fields collide with known typed fields | Use discriminator-only fallback after failed ordinary decoding to ignore unsupported event shapes. Known executable/control events remain strict. |
| 4 | Terminal snapshots allocate output that is discarded | Decode only response fields consumed by interpretation. Ordered output remains sourced from item events. |
| 4, 5 | Incompletion loses response identity, reason and usage | Preserve all three through dedicated API/protocol errors. No unchanged replay or transport fallback. Account for available usage and use the existing turn-tail cleanup. Preserve and extend the pre-existing content-filter-only fix. |
| 4, 5 | Header presence incorrectly means reasoning included | Require an affirmative header value. Current history accounting ignores this flag after removing resolved reasoning, so the reports' downstream compaction impact is not established here. |

## Claims that do not justify the proposed changes

| Reports | Finding | Disposition |
|---|---|---|
| 1 | Parsed queues are count-bounded, not byte-bounded; live consumers can apply backpressure | Mechanism confirmed. Core adds another queue and retains output for history, so API-only permits would not establish an end-to-end memory bound. Receiver-drop cancellation already exists. No supported delivery deadline or reproduced live-consumer leak was found. No arbitrary timeout or new queue framework added. Whole-path memory budgets remain a measured design question. |
| 1 | Expired connection and missing previous response require new recovery machinery | The current consumer already retires the socket, reconnects, invalidates incremental history and rebuilds full input. No missing repair established; retain the existing retry owner. |
| 2 | Shared cache filename causes harmful identity thrashing | Single-slot behavior confirmed, harmful workload unmeasured. Identity checks and stale-write protection remain. Partitioning is measurement-dependent. |
| 2, 3 | Initial-refresh owner cancellation can strand getters | Production worker captures an `InitialRefresh` drop guard before spawning. Cancellation, abortion, errors and unwinding release readiness; the endpoint bounds networking. The component-only example omits this owner. Existing freshness gate retained. |
| 4, 5 | Seven repeated unavailable-phase observations warrant telemetry redesign | Repetition confirmed, material production sink cost unmeasured. Consumers record these observations; the reports explicitly condition optimization on measurement. No exporter/schema redesign made. |
| All | Prompt bloat, redundant agent exploration/validation, superiority over upstream | Not established. No prompt rewrite, automatic continuation framework, upstream synchronization or comparative performance claim. |

Incompletion is deliberately terminal: output-budget exhaustion, filtering and unknown reasons remain distinct structured values for diagnosis, rather than silently regenerating. The existing turn owner settles admitted work and retires unadmitted tool calls after a failed response.

## Source evidence

- `codex-rs/codex-api/src/responses_stream.rs`, `api_bridge.rs`, `error.rs`.
- `codex-rs/codex-api/src/endpoint/responses_websocket.rs`, `compact.rs`, `session.rs`; `sse/responses.rs`.
- `codex-rs/protocol/src/error.rs`; `codex-rs/core/src/client.rs`, `responses_retry.rs`, `session/turn.rs`, `session/mod.rs`, `context_manager/history.rs`.
- `codex-rs/models-manager/src/manager.rs`, `cache.rs`; `codex-rs/model-provider/src/models_endpoint.rs`, `provider.rs`, `auth.rs`.
- `codex-rs/app-server/src/models_refresh_worker.rs`; `codex-rs/codex-api/src/telemetry.rs`.

The [official streaming-event reference](https://developers.openai.com/api/reference/resources/responses/streaming-events) independently confirms ordinary error, completion and incompletion events. Local source determines the fork findings; no unpinned upstream comparison was used to claim regressions or superiority.

## Validation

The API/model-manager/provider compile check passed. The library regression run selected 300 tests: 294 passed initially, and all six failures passed in a focused repair run. Passing tests were not rerun. The repairs preserved bounded event/size diagnostics, aligned memory freshness with the cache's configured TTL, exercised expiry and restart semantics, and replaced an internal identity-call-count assertion with checks that an obsolete account's catalog cannot escape during identity changes.

Library commands (through the repository's `rust_build_status.py run-lane` wrapper):

```text
cargo check -p codex-api -p codex-models-manager -p codex-model-provider --tests
cargo nextest run -p codex-api -p codex-models-manager -p codex-model-provider --lib --no-fail-fast
cargo nextest run -p codex-api -p codex-models-manager --lib --no-fail-fast -E 'test(protocol_errors_stop_before_completed) | test(offline_refresh_revalidates_identity_at_cache_and_catalog_boundaries) | test(refresh_available_models_drops_removed_remote_models) | test(refresh_available_models_refetches_when_version_mismatch) | test(refresh_available_models_refetches_when_cache_stale)'
```

All four focused core checks passed:

- `session::tests::audit_missing_usage_estimates_context_without_fabricating_billed_tokens`.
- `responses_retry::tests::audit_declared_incompletion_and_unknown_failures_do_not_retry_or_switch_transport`.
- `client::tests::model_attempt_offsets_require_monotonic_elapsed_values_and_allow_nulls`.
- `core_transport_telemetry`: `suite::client::incomplete_response_emits_content_filter_error_message`. This exercises a full turn, asserts the supplied input/output/cumulative usage, checks the terminal error, and asserts exactly one request despite an available retry budget.

Core selection commands, through the named-target runner:

```text
python scripts/rust_test_runner.py run-target core_lib --no-fail-fast -E 'test(audit_declared_incompletion) | test(audit_missing_usage) | test(model_attempt_offsets_require_monotonic_elapsed_values_and_allow_nulls)'
python scripts/rust_test_runner.py run-target core_transport_telemetry --no-fail-fast -E 'test(incomplete_response_emits_content_filter_error_message)'
```

The core run IDs were `f31fe253-698c-4687-abcf-df8bfcad18a3` (three tests) and `417f02bd-a010-45df-857d-c12be32008a3` (one integration test). Total: **304 distinct passing tests**, excluding setup scripts. Passing tests were retained, not repeated.

Validation encountered concurrent-source build mismatches in unrelated cloud-config and goal-extension code, interruptions, and nondiagnostic process exits. The required helper build succeeded once; subsequent focused runs reused those recorded executable artifacts while compiling the actual core targets from current source. The selected tests do not exercise those changing cloud/goal helpers. A missing `completed_us` field in an existing client-test fixture blocked compilation; its ordered timestamp fixture was repaired and its existing assertions passed. No test-runner or process-wrapper source was changed for this audit.

An early helper build emitted unused-code warnings for `exec-server` helpers and the prior `SerializedByteCounter`/`serialized_json_len` core code. These were outside the audited changes; inspection found the unused import and core counters already removed by independent checkout changes. The final focused core runs emitted no warnings. This was targeted validation, not a full-workspace test run.

No installed binary was replaced and Desktop was not restarted.
