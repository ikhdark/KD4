# codex-client

Higher-level request policy layered on `codex-http-client` without any Codex/OpenAI API awareness.

- Provides retry utilities (`RetryPolicy`, `RetryOn`, `run_with_retry`, `backoff`) that callers plug into for unary and streaming calls. `RetryPolicy::max_retries` counts retries after the initial request, and `backoff` takes a one-based retry number.
- Supplies the `sse_stream` helper to turn byte streams into raw SSE `data:` frames with idle timeouts and surfaced stream errors. Clean source EOF closes the output channel; typed consumers enforce protocol-specific completion events.
- Defines the request telemetry callback used by higher-level clients.
- Re-exports the low-level HTTP types temporarily so consumers can migrate to `codex-http-client` incrementally.
