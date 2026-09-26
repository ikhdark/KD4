#![allow(clippy::expect_used)]

// Bounded codex-core integration shard.
// Model transport, request headers, websocket fallback, external auth, and
// telemetry integration tests.
//
// The `suite` module is rooted at `tests/suite/` so test IDs stay
// `suite::<module>::<test>`, and the shared bootstrap comes from the one
// included prelude. Each shard compiles only its assigned source modules.

#[path = "suite"]
mod suite {
    include!("suite/prelude.rs");

    #[path = "client.rs"]
    mod client;
    #[path = "client_websockets.rs"]
    mod client_websockets;
    #[path = "external_auth.rs"]
    mod external_auth;
    #[path = "otel.rs"]
    mod otel;
    #[path = "responses_api_proxy_headers.rs"]
    mod responses_api_proxy_headers;
    #[path = "responses_headers.rs"]
    mod responses_headers;
    #[path = "responses_lite.rs"]
    mod responses_lite;
    #[path = "websocket_fallback.rs"]
    mod websocket_fallback;
}
