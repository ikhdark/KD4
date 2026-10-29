# KD4 production-only Tungstenite repair

Source: https://github.com/openai-oss-forks/tungstenite-rs
Pinned upstream revision: 4fffad30fe373adbdcffab9545e9e9bf4f2fc19f
Package/version: tungstenite 0.27.0 (unchanged).
Upstream MIT and Apache-2.0 licenses are retained verbatim.

The only runtime semantic change is in src/handshake/client.rs::generate_request:
the raw serialized HTTP upgrade request is never logged. The TRACE event instead
reports the serialized byte count. Existing handshake started/completed and
transport/status diagnostics remain; no logging level or subscriber is changed.
Header sensitivity cannot sanitize the upstream raw-byte string diagnostic, and
upstream provides no request-log sanitizer hook. A new handshake implementation
or stream-rewriting layer would duplicate security-critical protocol machinery.

All production modules and runtime feature/dependency declarations come from
that exact fork revision. The original handshake/client.rs SHA256 before the
repair is 9b7b6180e5203e44b58efcb66c8540e2599b305eb9a924c21c240ab2e7a28dfd.
RFC response verification, extension negotiation, leftover-byte handoff, masking,
frame/status validation, proxy support and compression are unchanged.

This repository intentionally includes no upstream tests, doctests, benchmarks,
examples, fuzzers, dev dependencies or test helpers. Eighteen complete test-only
modules, three test-only derive attributes and four executable rustdoc blocks
were omitted BEFORE files were added. Test-bearing targets and auto-discovery are
disabled in the production-only manifest. No tests were copied or created.
The manifest also allows rustc's `deprecated` lint so upstream's unchanged
`std::usize` uses do not warn in local builds.

The workspace patches both crates.io and the pinned Git dependency sources to
this owned copy, so tokio-tungstenite and direct consumers share one implementation.
The original Cargo cache and upstream Git checkout are never edited.
