# Passive HTTP response-start timing

This instrumentation extends existing telemetry records. It does not add model
requests, tasks, queues, body buffering, awaits, locks, atomics, or log emissions.
It captures `std::time::Instant` values in the existing request-local timing
slot, and enriches the telemetry callbacks that already run. SSE processing uses
that same task-local mechanism in its existing stream task; no state is shared
between independent requests.

## Boundaries

| Boundary | Existing record and added field | Meaning |
| --- | --- | --- |
| Request dispatch | `codex.transport_phase`, `phase=response_headers`, `request_send_started_ns` | Immediately before handing the prepared request to reqwest. Includes subsequent connection setup/upload; not proof bytes reached the network or provider. |
| Request send completion | Same record, `request_send_completed_ns` | Unavailable: reqwest does not expose upload completion. `send_completion_unavailable_reason` says so; absent/null is unknown, never zero. |
| Response headers | Same record, `response_headers_received_ns` | First successful reqwest response-header return, before request diagnostic logging. Redirects can make this the first response in a chain rather than its final response. |
| First raw response-body byte | `codex.sse_phase`, `ordinal=0`, `first_response_body_byte_ns` | First nonempty raw body chunk returned by the HTTP body stream, before SSE framing, UTF-8 or JSON decoding. Not a NIC/kernel timestamp. Empty chunks and header-derived metadata do not count. |
| First decoded model output | Existing model-attempt/turn timing | Remains distinct from raw bytes and decoded metadata-only events. No new event is added. |
| Server publication | Existing `codex.model_output_published`, `server_publication_ns` | After accepted, filtered content enters the session event channel. This is not UI rendering. The existing Unix timestamp is retained separately. |

`clock_id` identifies the existing protocol process-monotonic clock. Added
timestamps ending in `_ns` are absolute offsets in that domain. Compare them
only when clock IDs match. Never subtract a wall-clock value or a turn-relative
millisecond offset directly from these values. Captured instants predating the
clock domain remain unavailable.

## Correlation and interpretation

Join HTTP header and first-body observations using `(logical_request_id,
attempt)`. `attempt` is the lower HTTP retry ordinal, not the outer sampling
retry. `sampling_request_id` and `physical_attempt_id` identify the enclosing
core API attempt. For lower retries, existing turn timing names their physical
entries `<physical_attempt_id>:http-<attempt>`. This preserves failed attempts
without attaching a later response to an earlier retry.

The header phase includes a bounded `provider_request_id` from `x-request-id`
when supplied, and `requested_service_tier` from the actual inference-request
telemetry context. The existing `codex.provider_reported_model` receipt also
retains a bounded `reported_service_tier` from `response.service_tier` when a
response reports its model. Join that receipt to the completed model attempt by
its provider response ID. Missing provider metadata is unknown, not the requested
tier. These observations do not independently prove provider queueing,
throttling, or provider computation time.

The header and first-body timestamps describe occurrences, not when their
existing telemetry callbacks finish. The first-body receipt is emitted with
the existing first SSE poll result, including its timeout/error paths. If the
stream is cancelled before that callback, or the HTTP attempt is cancelled
before its existing phase callbacks, those observations can be absent. Do not
fill missing timestamps from adjacent attempts.

This addition targets HTTP/SSE. WebSocket transport keeps its existing timing
semantics; its frame-write and ingress boundaries are not mislabeled as HTTP
header/body timings. The native UI is outside this checkout: server publication
does not establish when content became visible in the interface.

Use existing local/OTEL telemetry collection and filters to retain the phase
records; no new logger or background collector is installed. Saved rollout
terminal timing alone does not contain these new transport-phase fields.
