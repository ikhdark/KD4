//! Paths resolved using the app-server host's platform rules.

use std::fmt;

use codex_utils_absolute_path::is_windows_absolute_path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppServerPath(String);

impl AppServerPath {
    pub fn from_app_server(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    pub fn from_absolute_str(raw: &str) -> Option<Self> {
        (raw.starts_with('/') || is_windows_absolute_path(raw)).then(|| Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn components(&self) -> Vec<&str> {
        let separators = if is_windows_absolute_path(&self.0) {
            &['/', '\\'][..]
        } else {
            &['/'][..]
        };
        self.0
            .split(separators)
            .filter(|part| !part.is_empty())
            .collect()
    }

    pub fn join(&self, segment: impl AsRef<str>) -> Self {
        let is_windows = is_windows_absolute_path(&self.0);
        let (path, separator) = if is_windows {
            (self.0.trim_end_matches(['/', '\\']), '\\')
        } else {
            (self.0.trim_end_matches('/'), '/')
        };
        Self(format!("{path}{separator}{}", segment.as_ref()))
    }
}

impl fmt::Display for AppServerPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_forward_slash_unc_path_with_windows_separator() {
        let path = AppServerPath::from_absolute_str("//server/share")
            .expect("UNC path should be absolute");

        assert_eq!(path.join("folder").as_str(), "//server/share\\folder");
    }
}
