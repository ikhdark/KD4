use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TurnProfile {
    pub before_first_sampling_ms: u64,
    pub sampling_ms: u64,
    pub between_sampling_overhead_ms: u64,
    pub tool_blocking_ms: u64,
    pub after_last_sampling_ms: u64,
    pub sampling_request_count: u32,
    pub sampling_retry_count: u32,
    pub compaction: u32,
    pub tool_result: u32,
    pub server_end_turn_false: u32,
    pub pending_input: u32,
    pub stop_hook: u32,
    pub invalid_image_recovery: u32,
}
