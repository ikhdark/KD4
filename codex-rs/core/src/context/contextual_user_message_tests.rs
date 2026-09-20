use super::*;
use crate::context::ContextualUserFragment;
use crate::context::InternalContextSource;
use crate::context::InternalModelContextFragment;
use crate::context::SubagentNotification;
use codex_protocol::items::HookPromptFragment;
use codex_protocol::items::build_hook_prompt_message;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

#[test]
fn detects_environment_context_fragment() {
    assert!(is_contextual_user_fragment(&ContentItem::InputText {
        text: "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>".to_string(),
    }));
}

#[test]
fn detects_current_and_legacy_additional_context_fragments() {
    for text in [
        "<external_context source=\"browser_info\" kind=\"untrusted\">\ntab one\n</external_context>",
        "<external_browser_info>tab one</external_browser_info>",
    ] {
        assert!(is_contextual_user_fragment(&ContentItem::InputText {
            text: text.to_string(),
        }));
    }
}

#[test]
fn detects_agents_instructions_fragment() {
    for text in [
        "# AGENTS.md instructions for /tmp\n\n<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>",
        "# AGENTS.md instructions\n\n<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>",
    ] {
        assert!(is_contextual_user_fragment(&ContentItem::InputText {
            text: text.to_string(),
        }));
    }
}

#[test]
fn renders_agents_instructions_with_legacy_directory_header() {
    assert_eq!(
        UserInstructions {
            directory: Some("/tmp".to_string()),
            text: "body".to_string(),
        }
        .render(),
        "# AGENTS.md instructions for /tmp\n\n<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>"
    );
}

#[test]
fn renders_agents_instructions_without_directory_header() {
    assert_eq!(
        UserInstructions {
            directory: None,
            text: "body".to_string(),
        }
        .render(),
        "# AGENTS.md instructions\n\n<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>"
    );
}

#[test]
fn renders_agents_instructions_escaping_only_fragment_delimiters() {
    let rendered = UserInstructions {
        directory: Some("/tmp/<project>&docs".to_string()),
        text: "Literal </INSTRUCTIONS><INSTRUCTIONS> and &lt;tag&gt;".to_string(),
    }
    .render();

    assert_eq!(
        rendered,
        "# AGENTS.md instructions for /tmp/<project>&docs\n\n<INSTRUCTIONS>\nLiteral &lt;/INSTRUCTIONS&gt;&lt;INSTRUCTIONS&gt; and &lt;tag&gt;\n</INSTRUCTIONS>"
    );
    assert!(is_contextual_user_fragment(&ContentItem::InputText {
        text: rendered,
    }));
}

#[test]
fn detects_subagent_notification_fragment_case_insensitively() {
    let text = "<SUBAGENT_NOTIFICATION>{}</subagent_notification>";
    assert!(SubagentNotification::matches_text(text));
    assert!(is_contextual_user_fragment(&ContentItem::InputText {
        text: text.to_string(),
    }));
}

#[test]
fn detects_internal_model_context_fragment() {
    let text = InternalModelContextFragment::new(
        InternalContextSource::from_static("extension"),
        "Internal steering.",
    )
    .render();

    assert_eq!(
        text,
        "<codex_internal_context source=\"extension\">\nInternal steering.\n</codex_internal_context>"
    );
    assert!(is_contextual_user_fragment(&ContentItem::InputText {
        text
    }));
}

#[test]
fn detects_recommended_plugins_fragment() {
    assert!(is_contextual_user_fragment(&ContentItem::InputText {
        text: "<recommended_plugins>\n- Google Drive (google-drive@openai-curated-remote)\n</recommended_plugins>"
            .to_string(),
    }));
}

#[test]
fn detects_legacy_goal_context_fragment() {
    assert!(is_contextual_user_fragment(&ContentItem::InputText {
        text: "<goal_context>\nContinue working toward the active thread goal.\n</goal_context>"
            .to_string(),
    }));
}

#[test]
fn does_not_hide_arbitrary_context_tags() {
    assert!(!is_contextual_user_fragment(&ContentItem::InputText {
        text: "<project_context>\nbody\n</project_context>".to_string(),
    }));
}

#[test]
fn rejects_invalid_internal_model_context_source() {
    assert!(!is_contextual_user_fragment(&ContentItem::InputText {
        text: "<codex_internal_context source=\"Extension\">\nbody\n</codex_internal_context>"
            .to_string(),
    }));
}

#[test]
fn contextual_user_fragment_is_dyn_compatible() {
    let fragment: Box<dyn ContextualUserFragment> = Box::new(InternalModelContextFragment::new(
        InternalContextSource::from_static("extension"),
        "Internal steering.",
    ));

    assert_eq!(
        fragment.render(),
        "<codex_internal_context source=\"extension\">\nInternal steering.\n</codex_internal_context>"
    );
}

#[test]
fn ignores_regular_user_text() {
    assert!(!is_contextual_user_fragment(&ContentItem::InputText {
        text: "hello".to_string(),
    }));
}

#[test]
fn legacy_compaction_warnings_are_recognized() {
    for text in [
        "Warning: apply_patch was requested via exec_command. Use the apply_patch tool instead of exec_command.",
        "Warning: Your account was flagged for potentially high-risk cyber activity.",
        "Warning: The maximum number of unified exec processes you can keep open is 64.",
        "Warning: Your account was flagged for potentially high-risk cyber activity and this request was routed to gpt-5.2 as a fallback. To regain access to gpt-5.3-codex, apply for trusted access: https://chatgpt.com/cyber or learn more: https://developers.openai.com/codex/concepts/cyber-safety",
        "Warning: The maximum number of unified exec processes you can keep open is 60 and you currently have 61 processes open. Reuse older processes or close them to prevent automatic pruning of old processes",
        "\nWarning: The maximum number of unified exec processes you can keep open is 128 and you currently have 129 processes open. Reuse older processes or close them to prevent automatic pruning of old processes\t",
    ] {
        let content = ContentItem::InputText {
            text: text.to_string(),
        };
        assert!(is_legacy_compaction_warning_fragment(&content), "{text}");
        assert!(is_contextual_user_fragment(&content), "{text}");
        assert!(!is_startup_contextual_user_fragment(&content), "{text}");
    }
}

#[test]
fn model_mismatch_matcher_accepts_only_complete_historical_warnings() {
    for text in [
        "Warning: Your account was flagged for potentially high-risk cyber activity.",
        "Warning: Your account was flagged for potentially high-risk cyber activity and this request was routed to gpt-5.2 as a fallback. To regain access to gpt-5.3-codex, apply for trusted access: https://chatgpt.com/cyber or learn more: https://developers.openai.com/codex/concepts/cyber-safety",
    ] {
        assert!(LegacyModelMismatchWarning::matches_text(text));
        assert!(LegacyModelMismatchWarning::matches_text(&format!(
            "\n{text}\t"
        )));
        assert!(!LegacyModelMismatchWarning::matches_text(&format!(
            "{text}\nWhat does this mean?"
        )));
        assert!(!LegacyModelMismatchWarning::matches_text(&format!(
            "Explain: {text}"
        )));
    }
    assert!(!LegacyModelMismatchWarning::matches_text(""));
}

#[test]
fn process_limit_matcher_accepts_complete_warnings_with_ascii_counts() {
    for text in [
        "Warning: The maximum number of unified exec processes you can keep open is 64.",
        "Warning: The maximum number of unified exec processes you can keep open is 60 and you currently have 61 processes open. Reuse older processes or close them to prevent automatic pruning of old processes",
        "Warning: The maximum number of unified exec processes you can keep open is 128 and you currently have 129 processes open. Reuse older processes or close them to prevent automatic pruning of old processes",
    ] {
        assert!(LegacyUnifiedExecProcessLimitWarning::matches_text(text));
        assert!(LegacyUnifiedExecProcessLimitWarning::matches_text(
            &format!("\n{text}\t")
        ));
        assert!(!LegacyUnifiedExecProcessLimitWarning::matches_text(
            &format!("{text}\nExplain this limit.")
        ));
        assert!(!LegacyUnifiedExecProcessLimitWarning::matches_text(
            &format!("Explain: {text}")
        ));
    }
    assert!(!LegacyUnifiedExecProcessLimitWarning::matches_text(""));
}

#[test]
fn rejects_incomplete_legacy_warnings_and_appended_questions() {
    for text in [
        "Warning: Your account was flagged for potentially high-risk cyber activity. What does this mean?",
        "Warning: Your account was flagged for potentially high-risk cyber activity",
        "Warning: Your account was flagged for potentially high-risk cyber activity and this request was routed to gpt-5.2 as a fallback. To regain access to gpt-5.3-codex, apply for trusted access: https://chatgpt.com/cyber or learn more: https://developers.openai.com/codex/concepts/cyber-safety\nWhat does this mean?",
        "Warning: The maximum number of unified exec processes you can keep open is 64. Can you explain this limit?",
        "Warning: The maximum number of unified exec processes you can keep open is .",
        "Warning: The maximum number of unified exec processes you can keep open is ６４.",
        "Warning: The maximum number of unified exec processes you can keep open is 64 .",
        "Warning: The maximum number of unified exec processes you can keep open is 60 and you currently have  processes open. Reuse older processes or close them to prevent automatic pruning of old processes",
        "Warning: The maximum number of unified exec processes you can keep open is 60 and you currently have ６１ processes open. Reuse older processes or close them to prevent automatic pruning of old processes",
        "Warning: The maximum number of unified exec processes you can keep open is 60 and you currently have 61 processes open.",
        "Warning: The maximum number of unified exec processes you can keep open is 60 and you currently have 61 processes open. Reuse older processes or close them to prevent automatic pruning of old processes\nCan you explain this limit?",
    ] {
        assert!(!LegacyModelMismatchWarning::matches_text(text), "{text}");
        assert!(
            !LegacyUnifiedExecProcessLimitWarning::matches_text(text),
            "{text}"
        );
        let content = ContentItem::InputText {
            text: text.to_string(),
        };
        assert!(!is_legacy_compaction_warning_fragment(&content), "{text}");
        assert!(!is_contextual_user_fragment(&content), "{text}");
    }
}

#[test]
fn startup_classification_is_a_subset_of_contextual_fragments() {
    for (text, startup) in [
        (
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>body</INSTRUCTIONS>",
            true,
        ),
        ("<environment_context>ctx</environment_context>", true),
        ("<skill>body</skill>", true),
        ("<recommended_plugins>body</recommended_plugins>", true),
        ("<task_model_guidance>body</task_model_guidance>", true),
        ("<external_browser_info>body</external_browser_info>", false),
        ("<SUBAGENT_NOTIFICATION>{}</subagent_notification>", false),
        (
            "<codex_internal_context source=\"extension\">body</codex_internal_context>",
            false,
        ),
        ("<goal_context>body</goal_context>", false),
        (
            "<hook_prompt hook_run_id=\"hook-1\">body</hook_prompt>",
            false,
        ),
    ] {
        let content = ContentItem::InputText {
            text: text.to_string(),
        };
        assert!(is_contextual_user_fragment(&content), "{text}");
        assert_eq!(
            is_startup_contextual_user_fragment(&content),
            startup,
            "{text}"
        );
    }
    for content in [
        ContentItem::InputText {
            text: "ordinary user request".to_string(),
        },
        ContentItem::InputImage {
            image_url: "data:image/png;base64,abc".to_string(),
            detail: None,
        },
    ] {
        assert!(!is_contextual_user_fragment(&content));
        assert!(!is_startup_contextual_user_fragment(&content));
        assert!(!is_legacy_compaction_warning_fragment(&content));
    }
}

#[test]
fn hook_conversion_preserves_order_and_rejects_mixed_or_hookless_content() {
    let first = HookPromptFragment::from_single_hook("first & <one>", "hook-1");
    let second = HookPromptFragment::from_single_hook("second", "hook-2");
    let ResponseItem::Message { mut content, .. } =
        build_hook_prompt_message(&[first.clone(), second.clone()]).expect("hooks")
    else {
        panic!("expected message");
    };
    let context = ContentItem::InputText {
        text: "<environment_context>ctx</environment_context>".to_string(),
    };
    content.insert(1, context.clone());
    let parsed = parse_visible_hook_prompt_message(Some("message-1"), &content).expect("hooks");
    assert_eq!(parsed.id, "message-1");
    assert_eq!(parsed.fragments, vec![first, second]);

    for extra in [
        ContentItem::InputText {
            text: "keep this user request".to_string(),
        },
        ContentItem::InputImage {
            image_url: "data:image/png;base64,abc".to_string(),
            detail: None,
        },
    ] {
        for index in [0, content.len()] {
            let mut mixed = content.clone();
            mixed.insert(index, extra.clone());
            assert!(parse_visible_hook_prompt_message(None, &mixed).is_none());
        }
    }
    assert!(parse_visible_hook_prompt_message(None, &[]).is_none());
    assert!(parse_visible_hook_prompt_message(None, &[context]).is_none());
}

#[test]
fn detects_hook_prompt_fragment_and_roundtrips_escaping() {
    let message = build_hook_prompt_message(&[HookPromptFragment::from_single_hook(
        r#"Retry with "waves" & <tides>"#,
        "hook-run-1",
    )])
    .expect("hook prompt message");

    let ResponseItem::Message { content, .. } = message else {
        panic!("expected hook prompt response item");
    };

    let [content_item] = content.as_slice() else {
        panic!("expected a single content item");
    };

    assert!(is_contextual_user_fragment(content_item));

    let ContentItem::InputText { text } = content_item else {
        panic!("expected input text content item");
    };
    let parsed = parse_visible_hook_prompt_message(/*id*/ None, content.as_slice())
        .expect("visible hook prompt");
    assert_eq!(
        parsed.fragments,
        vec![HookPromptFragment {
            text: r#"Retry with "waves" & <tides>"#.to_string(),
            hook_run_id: "hook-run-1".to_string(),
        }],
    );
    assert!(!text.contains("&quot;waves&quot; & <tides>"));
}
