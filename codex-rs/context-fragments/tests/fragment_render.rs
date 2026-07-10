use codex_context_fragments::AdditionalContextDeveloperFragment;
use codex_context_fragments::AdditionalContextUserFragment;
use codex_context_fragments::ContextualUserFragment;

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
fn render_compacts_repeated_fragment_lines() {
    let mut lines = vec!["important setup".to_string()];
    lines.extend((0..20).map(|_| "same generated warning".to_string()));
    lines.push("final rule".to_string());
    let fragment = TestFragment {
        body: lines.join("\n"),
    };

    let rendered = fragment.render();

    assert!(rendered.starts_with("<test_context>"));
    assert!(rendered.ends_with("</test_context>"));
    assert!(rendered.contains("important setup"));
    assert!(rendered.contains("final rule"));
    assert!(rendered.contains("[context compaction: omitted 19 repeated lines]"));
    assert_eq!(rendered.matches("same generated warning").count(), 1);
}

#[test]
fn render_compaction_preserves_trailing_body_newline() {
    let mut lines = vec!["important setup".to_string()];
    lines.extend((0..8).map(|_| "same generated warning".to_string()));
    let fragment = TestFragment {
        body: format!("{}\n", lines.join("\n")),
    };

    let rendered = fragment.render();

    assert!(rendered.contains(
        "same generated warning\n[context compaction: omitted 7 repeated lines]\n</test_context>"
    ));
}

#[test]
fn render_compaction_preserves_repeated_blank_lines() {
    let fragment = TestFragment {
        body: "first\n\n\nlast".to_string(),
    };

    assert_eq!(
        fragment.render(),
        "<test_context>first\n\n\nlast</test_context>"
    );
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
