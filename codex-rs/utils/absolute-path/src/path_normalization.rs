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
        dunce::simplified(left.as_ref()),
        dunce::simplified(right.as_ref()),
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
    dunce::simplified(path.as_ref()).to_path_buf()
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::normalize_for_native_workdir;
    #[cfg(windows)]
    use super::path_values_equal;
    use super::paths_match_after_normalization;
    #[cfg(windows)]
    use std::path::PathBuf;

    #[test]
    #[cfg(unix)]
    fn path_comparison_does_not_fold_case_or_lossy_native_names() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let dir = tempfile::tempdir().expect("directory");
        let upper = dir.path().join("A");
        let lower = dir.path().join("a");
        // The missing-path branch must also preserve case.
        assert!(!paths_match_after_normalization(&upper, &lower));
        for path in [&upper, &lower] {
            std::fs::write(path, path.as_os_str().as_encoded_bytes()).expect("write");
        }
        // Only assert distinct existing case spellings on case-sensitive volumes.
        if std::fs::read(&upper).expect("read") != std::fs::read(&lower).expect("read") {
            assert!(!paths_match_after_normalization(&upper, &lower));
        }
        let first = dir.path().join(OsString::from_vec(vec![0xff]));
        let second = dir.path().join(OsString::from_vec(vec![0xfe]));
        assert!(!paths_match_after_normalization(&first, &second));
        std::fs::write(&first, "first").expect("first");
        std::fs::write(&second, "second").expect("second");
        assert!(!paths_match_after_normalization(&first, &second));
    }

    #[test]
    fn missing_paths_fall_back_to_direct_equality() {
        assert!(paths_match_after_normalization("missing", "missing"));
        assert!(!paths_match_after_normalization("missing-a", "missing-b"));
    }

    #[test]
    #[cfg(windows)]
    fn windows_native_workdir_strips_verbatim_prefix() {
        let path = PathBuf::from(r"\\?\D:\c\worktree");
        assert_eq!(
            normalize_for_native_workdir(path),
            PathBuf::from(r"D:\c\worktree")
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_path_comparison_is_ascii_case_insensitive() {
        assert!(paths_match_after_normalization(
            r"C:\missing\Codex",
            r"c:\MISSING\codex",
        ));
        assert!(!paths_match_after_normalization(
            r"C:\missing\Codex",
            r"c:\missing\other",
        ));
        assert!(!paths_match_after_normalization(
            r"C:\missing\Ä",
            r"c:\missing\ä",
        ));
    }

    #[test]
    #[cfg(windows)]
    fn missing_windows_paths_ignore_verbatim_prefixes() {
        assert!(paths_match_after_normalization(
            PathBuf::from(r"\\?\C:\missing\marketplace"),
            PathBuf::from(r"C:\missing\marketplace"),
        ));
    }

    #[test]
    #[cfg(windows)]
    fn case_insensitive_comparison_ignores_trailing_separators() {
        assert!(path_values_equal(
            PathBuf::from(r"C:\missing\marketplace\").as_path(),
            PathBuf::from(r"c:\missing\marketplace").as_path(),
            true,
        ));
    }
}
