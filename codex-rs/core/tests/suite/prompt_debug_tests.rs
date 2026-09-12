use std::sync::Arc;

use anyhow::Result;
use codex_core::build_prompt_input;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_home::CodexHomeUserInstructionsProvider;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::strip_metadata;
use core_test_support::responses::strip_response_item_ids;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

const TEST_INSTRUCTIONS: &str = "Global test instructions";

#[tokio::test]
async fn build_prompt_input_includes_context_and_user_message() -> Result<()> {
    assert_prompt_input_and_bundled_skill_cache(true).await
}

#[tokio::test]
async fn build_prompt_input_removes_disabled_bundled_skills_and_preserves_user_message()
-> Result<()> {
    assert_prompt_input_and_bundled_skill_cache(false).await
}

async fn assert_prompt_input_and_bundled_skill_cache(bundled_enabled: bool) -> Result<()> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    std::fs::write(codex_home.path().join("AGENTS.md"), TEST_INSTRUCTIONS)?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        format!("[skills.bundled]\nenabled = {bundled_enabled}\n"),
    )?;
    let cached_system_skills = codex_home.path().join("skills/.system");
    if !bundled_enabled {
        let stale_skill = cached_system_skills.join("stale-skill");
        std::fs::create_dir_all(&stale_skill)?;
        std::fs::write(stale_skill.join("SKILL.md"), "# stale bundled skill\n")?;
    }
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            codex_self_exe: Some(std::env::current_exe()?),
            ..ConfigOverrides::default()
        })
        .build()
        .await?;
    let user_instructions_provider = Arc::new(CodexHomeUserInstructionsProvider::new(
        config.codex_home.clone(),
    ));

    let input = build_prompt_input(
        config,
        vec![UserInput::Text {
            text: "hello from debug prompt".to_string(),
            text_elements: Vec::new(),
        }],
        /*state_db*/ None,
        user_instructions_provider,
    )
    .await?;

    assert_eq!(
        cached_system_skills.exists(),
        bundled_enabled,
        "normal prompt construction must honor the configured bundled skill cache policy"
    );

    let expected_user_message = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "hello from debug prompt".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    assert_eq!(
        strip_response_item_ids(
            &input
                .last()
                .cloned()
                .map(strip_metadata)
                .into_iter()
                .collect::<Vec<_>>()
        ),
        vec![expected_user_message]
    );
    assert!(input.iter().any(|item| {
        let ResponseItem::Message { content, .. } = item else {
            return false;
        };

        content.iter().any(|content_item| {
            let (ContentItem::InputText { text } | ContentItem::OutputText { text }) = content_item
            else {
                return false;
            };
            text.contains(TEST_INSTRUCTIONS)
        })
    }));

    Ok(())
}
