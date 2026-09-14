use codex_context_fragments::AdditionalContextDeveloperFragment;
use codex_context_fragments::AdditionalContextUserFragment;
use codex_context_fragments::ContextualUserFragment;
use codex_context_fragments::FragmentRegistration;
use codex_context_fragments::FragmentRegistrationProxy;
use codex_context_fragments::ModelContextBudget;
use codex_context_fragments::RenderedContextFragment;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;

#[test]
fn model_context_budget_enforces_aggregate_limit() {
    let mut budget = ModelContextBudget::new(4);
    assert_eq!(budget.take("12345678"), Some("12345678".to_string()));
    let truncated = budget.take("abcdefghijklmnop").expect("final item");
    assert_eq!(truncated, "abcdefgh");
    assert_eq!(budget.remaining_bytes(), 0);
    assert_eq!(budget.take("later"), None);
}

#[test]
fn model_context_budget_truncates_at_utf8_boundary() {
    let mut budget = ModelContextBudget::new(1);

    assert_eq!(budget.take("a😀"), Some("a".to_string()));
    assert_eq!(budget.remaining_bytes(), 3);
}

#[test]
fn item_cap_preserves_aggregate_budget_for_later_fragments() {
    let mut budget = ModelContextBudget::new(10);
    assert_eq!(
        budget.take_up_to(&"x".repeat(100), 12),
        Some("x".repeat(12))
    );
    assert_eq!(budget.remaining_bytes(), 28);
    assert_eq!(budget.take("later"), Some("later".to_string()));
    assert_eq!(budget.remaining_bytes(), 23);
}

#[test]
fn model_context_budget_rejects_empty_unicode_truncation_without_charging() {
    for cap in 1..4 {
        let mut budget = ModelContextBudget::new(1);
        assert_eq!(budget.take_up_to("😀", cap), None);
        assert_eq!(budget.remaining_bytes(), 4);
        assert_eq!(budget.take("😀"), Some("😀".to_string()));
        assert_eq!(budget.remaining_bytes(), 0);
    }

    let mut budget = ModelContextBudget::new(1);
    assert_eq!(budget.take("abc"), Some("abc".to_string()));
    assert_eq!(budget.take("😀"), None);
    assert_eq!(budget.remaining_bytes(), 1);
    assert_eq!(budget.take(""), Some(String::new()));
    assert_eq!(budget.remaining_bytes(), 1);
}

#[test]
fn model_context_budget_preserves_head_tail_and_marker_with_exact_charge() {
    let mut budget = ModelContextBudget::new(20);
    let text = "😀abcdefghijklmnopqrstuvwxyz😀";
    assert_eq!(
        budget.take_up_to(text, 33),
        Some("\n[... context truncated ...]\n".to_string())
    );
    assert_eq!(budget.remaining_bytes(), 51);
    assert_eq!(
        budget.take_up_to("abcdefghijklmnopqrstuvwxyz0123456789", 35),
        Some("abc\n[... context truncated ...]\n789".to_string())
    );
    assert_eq!(budget.remaining_bytes(), 16);
}

#[test]
fn rendered_fragment_response_conversions_move_text_and_preserve_message() {
    for boxed in [false, true] {
        let text = "already rendered 😀".to_string();
        let original_ptr = text.as_ptr();
        let fragment = RenderedContextFragment::new("developer", text);
        let item = if boxed {
            let fragment: Box<dyn ContextualUserFragment> = Box::new(fragment);
            fragment.into_boxed_response_item()
        } else {
            ContextualUserFragment::into(fragment)
        };
        assert_eq!(
            item,
            ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: "already rendered 😀".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }
        );
        let ResponseItem::Message { content, .. } = &item else {
            panic!("expected message");
        };
        let ContentItem::InputText { text } = &content[0] else {
            panic!("expected input text");
        };
        assert_eq!(text.as_ptr(), original_ptr);
    }
}

#[test]
fn rendered_fragment_input_conversion_moves_text_and_preserves_message() {
    let text = "already rendered 😀".to_string();
    let original_ptr = text.as_ptr();
    let item = RenderedContextFragment::new("user", text).into_response_input_item();
    assert_eq!(
        item,
        ResponseInputItem::Message {
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "already rendered 😀".to_string(),
            }],
            phase: None,
        }
    );
    let ResponseInputItem::Message { content, .. } = &item else {
        panic!("expected message");
    };
    let ContentItem::InputText { text } = &content[0] else {
        panic!("expected input text");
    };
    assert_eq!(text.as_ptr(), original_ptr);
}

struct TestFragment {
    body: String,
}

impl ContextualUserFragment for TestFragment {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<test_context>", "</test_context>")
    }

    fn body(&self) -> String {
        self.body.clone()
    }
}

#[test]
fn render_preserves_fragment_bodies_byte_for_byte() {
    let cases = [
        ("repeated braces", "}\n}"),
        ("repeated closing tags", "</nested>\n</nested>"),
        ("repeated short lines", "x\nx"),
        ("repeated blank lines", "first\n\n\nlast"),
        (
            "code block content",
            "```text\nsame source line\nsame source line\n```",
        ),
    ];

    for (case, body) in cases {
        let fragment = TestFragment {
            body: body.to_string(),
        };

        assert_eq!(
            fragment.render(),
            format!("<test_context>{body}</test_context>"),
            "render changed {case}"
        );
    }
}

#[test]
fn additional_context_user_render_escapes_source_and_value() {
    let fragment = AdditionalContextUserFragment::new(
        "browser<&\"'>".to_string(),
        "a < b & c > d".to_string(),
    );

    assert_eq!(
        fragment.render(),
        "<external_context source=\"browser&lt;&amp;&quot;&#39;&gt;\" kind=\"untrusted\">\n\
a &lt; b &amp; c &gt; d\n\
</external_context>"
    );
}

#[test]
fn additional_context_developer_render_uses_application_wrapper() {
    let fragment = AdditionalContextDeveloperFragment::new(
        "automation_info".to_string(),
        "run <trusted> & inspect".to_string(),
    );

    assert_eq!(
        fragment.render(),
        "<application_context source=\"automation_info\" kind=\"application\">\n\
run &lt;trusted&gt; &amp; inspect\n\
</application_context>"
    );
}

#[test]
fn additional_context_caps_entity_heavy_values_after_escaping() {
    let fragment = AdditionalContextUserFragment::new("browser".to_string(), "&".repeat(4_000));

    let rendered = fragment.render();
    assert!(AdditionalContextUserFragment::matches_text(&rendered));
    let body = rendered
        .strip_prefix("<external_context source=\"browser\" kind=\"untrusted\">\n")
        .and_then(|body| body.strip_suffix("\n</external_context>"))
        .expect("additional context wrapper should be intact");
    let (prefix, truncated) = body
        .split_once('…')
        .expect("oversized escaped context should include a truncation marker");
    let (marker, suffix) = truncated
        .split_once('…')
        .expect("truncation marker should have a closing delimiter");

    assert!(body.len() <= 4_000);
    assert_eq!(marker, "4008 tokens truncated");
    assert_eq!(prefix, "&amp;".repeat(397));
    assert_eq!(suffix, "&amp;".repeat(397));
}

#[test]
fn additional_context_caps_oversized_source_labels_after_escaping() {
    let fragment = AdditionalContextUserFragment::new("&".repeat(1_000), "value".to_string());

    let rendered = fragment.render();
    assert!(AdditionalContextUserFragment::matches_text(&rendered));
    let source = rendered
        .strip_prefix("<external_context source=\"")
        .and_then(|rest| rest.split_once("\" kind=\"untrusted\">\n"))
        .map(|(source, _)| source)
        .expect("additional context source attribute should be intact");

    assert!(source.len() <= 1_536);
    assert_eq!(
        source,
        format!(
            "{}…source truncated…{}",
            "&amp;".repeat(151),
            "&amp;".repeat(151)
        )
    );
}

#[test]
fn additional_context_source_limit_preserves_exact_boundary_and_rejects_oversize() {
    for source in [
        "s".repeat(1_536),
        "😀".repeat(384),
        format!("{}&", "s".repeat(1_531)),
    ] {
        let escaped_source = source.replace('&', "&amp;");
        let user = AdditionalContextUserFragment::new(source.clone(), "value".to_string());
        let developer = AdditionalContextDeveloperFragment::new(source, "value".to_string());
        let user_text = format!(
            "<external_context source=\"{escaped_source}\" kind=\"untrusted\">\nvalue\n</external_context>"
        );
        let developer_text = format!(
            "<application_context source=\"{escaped_source}\" kind=\"application\">\nvalue\n</application_context>"
        );
        assert_eq!(user.render(), user_text);
        assert_eq!(developer.render(), developer_text);
        assert!(AdditionalContextUserFragment::matches_text(&user_text));
        assert!(AdditionalContextDeveloperFragment::matches_text(
            &developer_text
        ));
    }

    for source in ["s".repeat(1_537), format!("{}😀", "s".repeat(1_535))] {
        assert!(!AdditionalContextUserFragment::matches_text(&format!(
            "<external_context source=\"{source}\" kind=\"untrusted\">\nvalue\n</external_context>"
        )));
        assert!(!AdditionalContextDeveloperFragment::matches_text(&format!(
            "<application_context source=\"{source}\" kind=\"application\">\nvalue\n</application_context>"
        )));
    }
}

struct OverlappingMarkerFragment;

impl ContextualUserFragment for OverlappingMarkerFragment {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("ab", "bc")
    }

    fn body(&self) -> String {
        String::new()
    }
}

#[test]
fn marker_registration_rejects_overlapping_markers() {
    let registration = FragmentRegistrationProxy::<OverlappingMarkerFragment>::new();
    assert!(!registration.matches_text("abc"));
    assert!(!registration.matches_text(" \nABC\t "));
    assert!(!registration.matches_text("wrong prefix bc"));
}

#[test]
fn marker_registration_accepts_empty_body_and_whitespace_and_case_variants() {
    let registration = FragmentRegistrationProxy::<OverlappingMarkerFragment>::new();
    let rendered = OverlappingMarkerFragment.render();
    assert_eq!(rendered, "abbc");
    assert!(registration.matches_text(&rendered));
    assert!(registration.matches_text(" \nABBC\t "));
    assert!(!RenderedContextFragment::matches_text("anything"));
}

#[test]
fn additional_context_match_accepts_fixed_and_legacy_user_wrappers() {
    assert!(AdditionalContextUserFragment::matches_text(
        "<external_context source=\"path\" kind=\"untrusted\">\nvalue\n</external_context>"
    ));
    assert!(AdditionalContextUserFragment::matches_text(
        "<external_browser_info>value</external_browser_info>"
    ));
    assert!(!AdditionalContextUserFragment::matches_text(
        "<external_contextual>\nvalue\n</external_context>"
    ));
    assert!(!AdditionalContextUserFragment::matches_text(
        "<external_context source=\"path\" kind=\"application\">\nvalue\n</external_context>"
    ));
}

#[test]
fn additional_context_match_rejects_malformed_explicit_wrappers() {
    let malformed = [
        (
            "kind embedded inside source",
            "<external_context source=\"x kind=\"untrusted\">\nvalue\n</external_context>",
        ),
        (
            "missing kind closing quote",
            "<external_context source=\"path\" kind=\"untrusted>\nvalue\n</external_context>",
        ),
        (
            "duplicate source attribute",
            "<external_context source=\"path\" source=\"other\" kind=\"untrusted\">\nvalue\n</external_context>",
        ),
        (
            "malformed source attribute",
            "<external_context source=path kind=\"untrusted\">\nvalue\n</external_context>",
        ),
        (
            "trailing opening-tag bytes",
            "<external_context source=\"path\" kind=\"untrusted\" unexpected>\nvalue\n</external_context>",
        ),
        (
            "unescaped body markup",
            "<external_context source=\"path\" kind=\"untrusted\">\n<raw> & value\n</external_context>",
        ),
    ];

    for (case, text) in malformed {
        assert!(
            !AdditionalContextUserFragment::matches_text(text),
            "matched malformed wrapper with {case}"
        );
    }

    let oversized_source = format!(
        "<external_context source=\"{}\" kind=\"untrusted\">\nvalue\n</external_context>",
        "s".repeat(1_537)
    );
    assert!(!AdditionalContextUserFragment::matches_text(
        &oversized_source
    ));

    let oversized_body = format!(
        "<external_context source=\"path\" kind=\"untrusted\">\n{}\n</external_context>",
        "v".repeat(4_001)
    );
    assert!(!AdditionalContextUserFragment::matches_text(
        &oversized_body
    ));
}

#[test]
fn additional_context_developer_match_requires_application_kind() {
    assert!(AdditionalContextDeveloperFragment::matches_text(
        "<application_context source=\"path\" kind=\"application\">\nvalue\n</application_context>"
    ));
    assert!(!AdditionalContextDeveloperFragment::matches_text(
        "<application_context source=\"path\" kind=\"untrusted\">\nvalue\n</application_context>"
    ));
}

#[test]
fn marker_match_allows_prefix_markers_ending_with_space() {
    struct PrefixFragment;

    impl ContextualUserFragment for PrefixFragment {
        fn role(&self) -> &'static str {
            "user"
        }

        fn markers(&self) -> (&'static str, &'static str) {
            Self::type_markers()
        }

        fn type_markers() -> (&'static str, &'static str) {
            ("# PREFIX ", "</PREFIX>")
        }

        fn body(&self) -> String {
            "value\n".to_string()
        }
    }

    assert!(PrefixFragment::matches_text("# PREFIX value\n</PREFIX>"));
}
