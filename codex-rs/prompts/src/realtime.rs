pub const BACKEND_PROMPT: &str = include_str!("../templates/realtime/backend_prompt.compact.md");
pub const END_INSTRUCTIONS: &str = include_str!("../templates/realtime/realtime_end.md");
pub const START_INSTRUCTIONS: &str = include_str!("../templates/realtime/realtime_start.md");

#[cfg(test)]
#[path = "realtime_tests.rs"]
mod tests;
