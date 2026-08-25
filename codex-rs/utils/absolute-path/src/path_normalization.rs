use std::path::Path;
use std::path::PathBuf;

pub fn normalize_for_path_comparison(path: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    path.as_ref().canonicalize()
}

/// Compare paths after applying Codex's filesystem normalization.
///
/// If either path cannot be normalized, this falls back to direct path equality.
pub fn paths_match_after_normalization(left: impl AsRef<Path>, right: impl AsRef<Path>) -> bool {
    if let (Ok(left), Ok(right)) = (
        normalize_for_path_comparison(left.as_ref()),
        normalize_for_path_comparison(right.as_ref()),
    ) {
        return path_values_equal(&left, &right, true);
    }
    path_values_equal(
        dunce::simplified(left.as_ref()),
        dunce::simplified(right.as_ref()),
        true,
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
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy()) => {}
                (None, None) => return true,
                _ => return false,
            }
        }
    } else {
        left == right
    }
}

pub fn normalize_for_native_workdir(path: impl AsRef<Path>) -> PathBuf {
    dunce::simplified(path.as_ref()).to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::normalize_for_native_workdir;
    use super::path_values_equal;
    use super::paths_match_after_normalization;
    use std::path::PathBuf;

    #[test]
    fn missing_paths_fall_back_to_direct_equality() {
        assert!(paths_match_after_normalization("missing", "missing"));
        assert!(!paths_match_after_normalization("missing-a", "missing-b"));
    }

    #[test]
    fn windows_native_workdir_strips_verbatim_prefix() {
        let path = PathBuf::from(r"\\?\D:\c\worktree");
        assert_eq!(
            normalize_for_native_workdir(path),
            PathBuf::from(r"D:\c\worktree")
        );
    }

    #[test]
    fn windows_path_comparison_is_ascii_case_insensitive() {
        assert!(path_values_equal(
            PathBuf::from(r"C:\Users\Codex").as_path(),
            PathBuf::from(r"c:\users\codex").as_path(),
            true,
        ));
        assert!(!path_values_equal(
            PathBuf::from("Alpha").as_path(),
            PathBuf::from("alpha").as_path(),
            false,
        ));
    }

    #[test]
    fn missing_windows_paths_ignore_verbatim_prefixes() {
        assert!(paths_match_after_normalization(
            PathBuf::from(r"\\?\C:\missing\marketplace"),
            PathBuf::from(r"C:\missing\marketplace"),
        ));
    }

    #[test]
    fn case_insensitive_comparison_ignores_trailing_separators() {
        assert!(path_values_equal(
            PathBuf::from(r"C:\missing\marketplace\").as_path(),
            PathBuf::from(r"c:\missing\marketplace").as_path(),
            true,
        ));
    }
}
