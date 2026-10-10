use crate::events::CodexAcceptedLineFingerprintsEventParams;
use crate::events::CodexAcceptedLineFingerprintsEventRequest;
use crate::events::TrackEventRequest;
use codex_git_utils::canonicalize_git_remote_url;
use codex_git_utils::get_git_remote_urls_assume_git_repo;
use sha1::Digest;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AcceptedLineCounts {
    pub(crate) accepted_added_lines: u64,
    pub(crate) accepted_deleted_lines: u64,
}

pub(crate) struct AcceptedLineFingerprintEventInput {
    pub(crate) event_type: &'static str,
    pub(crate) turn_id: String,
    pub(crate) thread_id: String,
    pub(crate) product_surface: Option<String>,
    pub(crate) model_slug: Option<String>,
    pub(crate) completed_at: u64,
    pub(crate) repo_hash: Option<String>,
    pub(crate) accepted_added_lines: u64,
    pub(crate) accepted_deleted_lines: u64,
}

pub(crate) fn accepted_line_counts_from_unified_diff(unified_diff: &str) -> AcceptedLineCounts {
    let mut in_hunk = false;
    let mut accepted_added_lines = 0;
    let mut accepted_deleted_lines = 0;

    for line in unified_diff.lines() {
        if line.starts_with("diff --git ") {
            in_hunk = false;
        } else if line.starts_with("@@ ") {
            in_hunk = true;
        } else if in_hunk && line.starts_with('+') {
            accepted_added_lines += 1;
        } else if in_hunk && line.starts_with('-') {
            accepted_deleted_lines += 1;
        }
    }

    AcceptedLineCounts {
        accepted_added_lines,
        accepted_deleted_lines,
    }
}

pub fn fingerprint_hash(domain: &str, value: &str) -> String {
    let mut hasher = sha1::Sha1::new();
    hasher.update(b"file-line-v1\0");
    hasher.update(domain.as_bytes());
    hasher.update(b"\0");
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub(crate) fn accepted_line_fingerprint_event_requests(
    input: AcceptedLineFingerprintEventInput,
) -> Vec<TrackEventRequest> {
    let AcceptedLineFingerprintEventInput {
        event_type,
        turn_id,
        thread_id,
        product_surface,
        model_slug,
        completed_at,
        repo_hash,
        accepted_added_lines,
        accepted_deleted_lines,
    } = input;

    vec![TrackEventRequest::AcceptedLineFingerprints(Box::new(
        CodexAcceptedLineFingerprintsEventRequest {
            event_type: "codex_accepted_line_fingerprints",
            event_params: CodexAcceptedLineFingerprintsEventParams {
                event_type,
                turn_id,
                thread_id,
                product_surface,
                model_slug,
                completed_at,
                repo_hash,
                accepted_added_lines,
                accepted_deleted_lines,
                line_fingerprints: Vec::new(),
            },
        },
    ))]
}

pub async fn accepted_line_repo_hash_for_cwd(cwd: &Path) -> Option<String> {
    let remotes = get_git_remote_urls_assume_git_repo(cwd).await?;
    remotes
        .get("origin")
        .or_else(|| remotes.values().next())
        .map(|remote_url| {
            let canonical_remote_url =
                canonicalize_git_remote_url(remote_url).unwrap_or_else(|| remote_url.to_string());
            fingerprint_hash("repo", &canonical_remote_url)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_ignore_changes_outside_hunks() {
        let diff = "diff --git a/a b/a\n+++ b/a\n+ignore this\n-ignore this too\n@@ -1 +1 @@\n-old value\n+new value\ndiff --git a/b b/b\n+ignore again\n";
        assert_eq!(
            accepted_line_counts_from_unified_diff(diff),
            AcceptedLineCounts {
                accepted_added_lines: 1,
                accepted_deleted_lines: 1,
            }
        );
    }

    #[test]
    fn counts_added_and_deleted_hunk_lines() {
        let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,5 @@
-old line
+fn useful() {
+}
+    return user.id;
 context
";

        assert_eq!(
            accepted_line_counts_from_unified_diff(diff),
            AcceptedLineCounts {
                accepted_added_lines: 3,
                accepted_deleted_lines: 1,
            }
        );
    }

    #[test]
    fn fingerprint_hash_is_domain_separated_sha1() {
        // sha1("file-line-v1\0path\0src/lib.rs"), computed outside this crate.
        assert_eq!(
            fingerprint_hash("path", "src/lib.rs"),
            "d69749859af906cb3f8a6fb5d85a0871d2f3b3e9"
        );
        assert_ne!(
            fingerprint_hash("repo", "src/lib.rs"),
            fingerprint_hash("path", "src/lib.rs")
        );
    }

    #[test]
    fn skips_added_file_metadata_headers() {
        let diff = "\
diff --git a/new.py b/new.py
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/new.py
@@ -0,0 +1 @@
+print('hello')
";

        assert_eq!(
            accepted_line_counts_from_unified_diff(diff),
            AcceptedLineCounts {
                accepted_added_lines: 1,
                accepted_deleted_lines: 0,
            }
        );
    }

    #[test]
    fn parses_hunk_lines_that_look_like_file_headers() {
        let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,2 @@
--- old value
+++ new value
";

        assert_eq!(
            accepted_line_counts_from_unified_diff(diff),
            AcceptedLineCounts {
                accepted_added_lines: 1,
                accepted_deleted_lines: 1,
            }
        );
    }
}
