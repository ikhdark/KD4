#![allow(dead_code)]

// Keep the custom benchmark executable testable without moving benchmark-only
// types into the production crate API.
mod turn_latency {
    include!("../benches/turn_latency.rs");
}
