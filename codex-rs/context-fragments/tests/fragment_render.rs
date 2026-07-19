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
    assert!(marker.ends_with(" tokens truncated"));
    assert_eq!(prefix.replace("&amp;", ""), "");
    assert_eq!(suffix.replace("&amp;", ""), "");
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
    assert!(source.contains("…source truncated…"));
    assert_eq!(
        source
            .replace("&amp;", "")
            .replace("…source truncated…", ""),
        ""
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
