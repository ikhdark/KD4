use std::path::Path;
use std::path::PathBuf;

pub fn normalize_for_path_comparison(path: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    path.as_ref().canonicalize()
}

/// Compare paths after applying Codex's filesystem normalization.
///
/// If either path cannot be normalized, compare native path components. Only
/// the Windows fallback folds ASCII case and simplifies safe verbatim prefixes.
pub fn paths_match_after_normalization(left: impl AsRef<Path>, right: impl AsRef<Path>) -> bool {
    if left.as_ref() == right.as_ref() {
        return true;
    }
    if let (Ok(left), Ok(right)) = (
        normalize_for_path_comparison(left.as_ref()),
        normalize_for_path_comparison(right.as_ref()),
    ) {
        return left == right;
    }
    path_values_equal(
        crate::normalize_path_for_platform(left.as_ref()).as_ref(),
        crate::normalize_path_for_platform(right.as_ref()).as_ref(),
        cfg!(windows),
    )
}

fn path_values_equal(left: &Path, right: &Path, case_insensitive: bool) -> bool {
    if case_insensitive {
        let mut left = left.components();
        let mut right = right.components();
        loop {
            match (left.next(), right.next()) {
                (Some(left), Some(right))
                    if left
                        .as_os_str()
                        .as_encoded_bytes()
                        .eq_ignore_ascii_case(right.as_os_str().as_encoded_bytes()) => {}
                (None, None) => return true,
                _ => return false,
            }
        }
    } else {
        left == right
    }
}

pub fn normalize_for_native_workdir(path: impl AsRef<Path>) -> PathBuf {
    crate::normalize_path_for_platform(path.as_ref()).into_owned()
}

