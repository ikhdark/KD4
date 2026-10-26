//! Code-mode exec contract pipeline.
//!
//! ToolSpec registration builds the public exec prompt, [`parse_exec_source`]
//! parses the optional execution pragma, and runtime metadata exposes exact nested-tool
//! declarations. Schema rendering is isolated because it changes independently of both
//! the public prompt and the execution parser.

mod exec_prompt;
mod metadata;
mod pragma;
mod schema_ts;

pub use exec_prompt::build_exec_tool_description;
pub use exec_prompt::build_wait_tool_description;
pub use metadata::CodeModeToolKind;
pub use metadata::EnabledToolMetadata;
pub use metadata::ToolDefinition;
pub use metadata::augment_tool_definition;
pub use metadata::is_code_mode_nested_tool;
pub use metadata::legacy_code_mode_identifier;
pub use metadata::normalize_code_mode_identifier;
pub use metadata::render_code_mode_sample;
pub use metadata::render_code_mode_tool_bundle;
pub use pragma::CODE_MODE_PRAGMA_PREFIX;
pub use pragma::parse_exec_source;
pub use schema_ts::render_json_schema_to_typescript;
