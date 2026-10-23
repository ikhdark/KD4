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
    let syntax = codex_shell_command::PowershellAnalysisScope::current();
    tokio::task::spawn_blocking(move || {
        codex_utils_pty::process_observation::ProcessObservationContext::scope_sync(
            observation,
            || codex_shell_command::PowershellAnalysisScope::scope_sync(syntax, operation),
        )
    })
    .await
}

#[cfg(test)]
mod scope_tests {
    #[tokio::test]
    async fn command_workers_inherit_only_the_current_invocation_syntax_scope() {
        use codex_shell_command::PowershellAnalysisScope;
        assert!(PowershellAnalysisScope::current().is_none());
        PowershellAnalysisScope::default().scope(async {
            assert!(super::run_blocking_command_analysis(|| PowershellAnalysisScope::current().is_some()).await.unwrap());
        }).await;
        assert!(!super::run_blocking_command_analysis(|| PowershellAnalysisScope::current().is_some()).await.unwrap());
    }
}
