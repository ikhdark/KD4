use std::collections::HashSet;
use std::ffi::OsString;

pub const FEEDBACK_DIAGNOSTICS_ATTACHMENT_FILENAME: &str = "codex-connectivity-diagnostics.txt";
const PROXY_ENV_VARS: &[&str] = &[
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeedbackDiagnostics {
    diagnostics: Vec<FeedbackDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackDiagnostic {
    pub headline: String,
    pub details: Vec<String>,
}

impl FeedbackDiagnostics {
    pub fn new(diagnostics: Vec<FeedbackDiagnostic>) -> Self {
        Self { diagnostics }
    }

    pub fn collect_from_env() -> Self {
        Self::collect_from_pairs(std::env::vars_os())
    }

    fn collect_from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
    {
        let env = pairs
            .into_iter()
            .filter_map(|(key, _)| key.into().into_string().ok())
            .collect::<HashSet<_>>();
        let mut diagnostics = Vec::new();

        let proxy_details = PROXY_ENV_VARS
            .iter()
            .filter(|key| env.contains(**key))
            .map(|key| format!("{key} is set; value redacted"))
            .collect::<Vec<_>>();
        if !proxy_details.is_empty() {
            diagnostics.push(FeedbackDiagnostic {
                headline: "Proxy environment variables are set and may affect connectivity."
                    .to_string(),
                details: proxy_details,
            });
        }

        Self { diagnostics }
    }

    pub fn is_empty(&self) -> bool {
        self.diagnostics.is_empty()
    }

    pub fn diagnostics(&self) -> &[FeedbackDiagnostic] {
        &self.diagnostics
    }

    pub fn attachment_text(&self) -> Option<String> {
        if self.diagnostics.is_empty() {
            return None;
        }

        let mut lines = vec!["Connectivity diagnostics".to_string(), String::new()];
        for diagnostic in &self.diagnostics {
            lines.push(format!("- {}", diagnostic.headline));
            lines.extend(
                diagnostic
                    .details
                    .iter()
                    .map(|detail| format!("  - {detail}")),
            );
        }

        Some(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::FeedbackDiagnostic;
    use super::FeedbackDiagnostics;

    #[test]
    fn collect_from_pairs_redacts_values_and_attachment() {
        let diagnostics = FeedbackDiagnostics::collect_from_pairs([
            (
                "HTTPS_PROXY",
                "https://user:password@secure-proxy.example.com:443?secret=1",
            ),
            ("http_proxy", "proxy.example.com:8080"),
            ("all_proxy", "socks5h://all-proxy.example.com:1080"),
        ]);

        assert_eq!(
            diagnostics,
            FeedbackDiagnostics {
                diagnostics: vec![FeedbackDiagnostic {
                    headline: "Proxy environment variables are set and may affect connectivity."
                        .to_string(),
                    details: vec![
                        "http_proxy is set; value redacted".to_string(),
                        "HTTPS_PROXY is set; value redacted".to_string(),
                        "all_proxy is set; value redacted".to_string(),
                    ],
                },],
            }
        );

        assert_eq!(
            diagnostics.attachment_text(),
            Some(
                r#"Connectivity diagnostics

- Proxy environment variables are set and may affect connectivity.
  - http_proxy is set; value redacted
  - HTTPS_PROXY is set; value redacted
  - all_proxy is set; value redacted"#
                    .to_string()
            )
        );
    }

    #[test]
    fn collect_from_pairs_ignores_absent_values() {
        let diagnostics = FeedbackDiagnostics::collect_from_pairs(Vec::<(String, String)>::new());
        assert_eq!(diagnostics, FeedbackDiagnostics::default());
        assert_eq!(diagnostics.attachment_text(), None);
    }

    #[test]
    fn collect_from_pairs_redacts_whitespace_and_empty_values() {
        let diagnostics =
            FeedbackDiagnostics::collect_from_pairs([("HTTP_PROXY", "  proxy with spaces  ")]);

        assert_eq!(
            diagnostics,
            FeedbackDiagnostics {
                diagnostics: vec![FeedbackDiagnostic {
                    headline: "Proxy environment variables are set and may affect connectivity."
                        .to_string(),
                    details: vec!["HTTP_PROXY is set; value redacted".to_string()],
                },],
            }
        );
    }

    #[test]
    fn collect_from_pairs_never_reports_invalid_values() {
        let proxy_value = "not a valid proxy";
        let diagnostics = FeedbackDiagnostics::collect_from_pairs([("HTTP_PROXY", proxy_value)]);

        assert_eq!(
            diagnostics,
            FeedbackDiagnostics {
                diagnostics: vec![FeedbackDiagnostic {
                    headline: "Proxy environment variables are set and may affect connectivity."
                        .to_string(),
                    details: vec!["HTTP_PROXY is set; value redacted".to_string()],
                },],
            }
        );
    }

    #[test]
    fn collect_from_pairs_does_not_inspect_values() {
        struct PanicIfInspected;

        impl From<PanicIfInspected> for String {
            fn from(_: PanicIfInspected) -> Self {
                panic!("proxy value was inspected");
            }
        }

        let diagnostics =
            FeedbackDiagnostics::collect_from_pairs([("HTTPS_PROXY", PanicIfInspected)]);

        assert_eq!(
            diagnostics.diagnostics()[0].details,
            ["HTTPS_PROXY is set; value redacted"]
        );
    }

    #[test]
    fn collect_from_pairs_ignores_non_utf8_entries_without_inspecting_values() {
        use std::os::windows::ffi::OsStringExt;

        let diagnostics = FeedbackDiagnostics::collect_from_pairs([
            (
                std::ffi::OsString::from_wide(&[0xd800]),
                std::ffi::OsString::from("unrelated"),
            ),
            (
                std::ffi::OsString::from("HTTPS_PROXY"),
                std::ffi::OsString::from_wide(&[0xd801]),
            ),
        ]);

        assert_eq!(
            diagnostics.diagnostics()[0].details,
            ["HTTPS_PROXY is set; value redacted"]
        );
    }
}
