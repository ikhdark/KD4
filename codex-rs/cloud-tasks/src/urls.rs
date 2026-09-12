use std::fmt;

pub const DEFAULT_CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const TASK_ID_ENCODE_SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// A base URL normalized to the canonical backend-client form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudBaseUrl(String);

impl CloudBaseUrl {
    pub fn new(input: &str) -> Self {
        let mut base_url = input.to_string();
        while base_url.ends_with('/') {
            base_url.pop();
        }
        if (base_url.starts_with("https://chatgpt.com")
            || base_url.starts_with("https://chat.openai.com"))
            && !base_url.contains("/backend-api")
        {
            base_url = format!("{base_url}/backend-api");
        }
        Self(base_url)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CloudBaseUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Construct a browser-friendly task URL for the given backend base URL.
pub fn task_url(base_url: &CloudBaseUrl, task_id: &str) -> String {
    // Keep task IDs in one path segment, including IDs containing URL delimiters.
    let task_id = percent_encoding::utf8_percent_encode(task_id, TASK_ID_ENCODE_SET);
    if let Some(root) = base_url.as_str().strip_suffix("/backend-api") {
        return format!("{root}/codex/tasks/{task_id}");
    }
    if let Some(root) = base_url.as_str().strip_suffix("/api/codex") {
        return format!("{root}/codex/tasks/{task_id}");
    }
    if base_url.as_str().ends_with("/codex") {
        return format!("{base_url}/tasks/{task_id}");
    }
    format!("{base_url}/codex/tasks/{task_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_chatgpt_and_custom_base_urls() {
        assert_eq!(
            CloudBaseUrl::new("https://chatgpt.com///").as_str(),
            "https://chatgpt.com/backend-api",
        );
        assert_eq!(
            CloudBaseUrl::new("https://example.test/api/codex/").as_str(),
            "https://example.test/api/codex",
        );
    }

    #[test]
    fn builds_browser_task_urls_for_supported_backends() {
        assert_eq!(
            task_url(&CloudBaseUrl::new("https://chatgpt.com/"), "task-1"),
            "https://chatgpt.com/codex/tasks/task-1"
        );
        assert_eq!(
            task_url(
                &CloudBaseUrl::new("https://example.test/api/codex/"),
                "task-1"
            ),
            "https://example.test/codex/tasks/task-1"
        );
        assert_eq!(
            task_url(&CloudBaseUrl::new("https://example.test/custom"), "task-1"),
            "https://example.test/custom/codex/tasks/task-1"
        );
        for base in [
            "https://example.test/backend-api",
            "https://example.test/api/codex",
            "https://example.test/codex",
            "https://example.test",
        ] {
            assert_eq!(
                task_url(&CloudBaseUrl::new(base), "task_/a?b#c% d\u{00e9}"),
                "https://example.test/codex/tasks/task_%2Fa%3Fb%23c%25%20d%C3%A9"
            );
        }
    }
}
