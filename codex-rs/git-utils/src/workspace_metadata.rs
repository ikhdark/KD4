#![deny(clippy::print_stdout, clippy::print_stderr)]
use serde::Serialize;
use std::collections::BTreeMap;



#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct GitWorkspaceMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub associated_remote_urls: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_git_commit_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_changes: Option<bool>,
}

impl GitWorkspaceMetadata {
    pub fn is_empty(&self) -> bool {
        self.associated_remote_urls.is_none()
            && self.latest_git_commit_hash.is_none()
            && self.has_changes.is_none()
    }
}
