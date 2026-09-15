use super::*;
use pretty_assertions::assert_eq;

#[test]
fn top_cli_parses_resume_prompt_after_config_flag() {
    const PROMPT: &str = "echo resume-with-global-flags-after-subcommand";
    let inner = TopCli::parse_exec_from(
        [
            "codex-exec",
            "-c",
            "model_provider=exec_fixture",
            "resume",
            "--strict-config",
            "--last",
            "--json",
            "--model",
            "gpt-5.2-codex",
            "--config",
            "reasoning_level=xhigh",
            "--dangerously-bypass-approvals-and-sandbox",
            "--skip-git-repo-check",
            PROMPT,
        ]
        .map(std::ffi::OsString::from),
    );

    let Some(codex_exec::Command::Resume(args)) = inner.command.as_ref() else {
        panic!("expected resume command");
    };
    assert_eq!(args.session_id, None);
    assert_eq!(args.prompt.as_deref(), Some(PROMPT));
    assert_eq!(inner.config_overrides.raw_overrides.len(), 2);
    assert_eq!(
        inner.config_overrides.raw_overrides,
        ["model_provider=exec_fixture", "reasoning_level=xhigh"]
    );
    assert!(inner.strict_config);
}
