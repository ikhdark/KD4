//! Shell command analysis for codex-core's command tools: invocation shapes,
//! rg search narrowing, preflight checks, validation and mutation
//! classification, and model-facing output summaries.

pub mod command_mutation;
pub mod command_preflight;
pub mod command_search;
pub mod command_shape;
pub mod git_dirs;
pub mod shell_output_summary;
pub mod validation;

pub async fn run_blocking_command_analysis<T, F>(operation: F) -> Result<T, tokio::task::JoinError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let observation = codex_utils_pty::process_observation::ProcessObservationContext::current();
    tokio::task::spawn_blocking(move || {
        codex_utils_pty::process_observation::ProcessObservationContext::scope_sync(
            observation,
            operation,
        )
    })
    .await
}
