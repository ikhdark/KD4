use dirs::home_dir;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::Error as SerdeError;
use std::borrow::Cow;
use std::cell::RefCell;
use std::marker::PhantomData;
use std::path::Display;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use ts_rs::TS;

mod absolutize;
mod path_normalization;

pub use path_normalization::normalize_for_native_workdir;
pub use path_normalization::normalize_for_path_comparison;
pub use path_normalization::paths_match_after_normalization;

/// A path that is guaranteed to be absolute and normalized (though it is not
/// guaranteed to be canonicalized or exist on the filesystem).
///
/// IMPORTANT: When deserializing an `AbsolutePathBuf`, a base path must be set
/// using [AbsolutePathBufGuard::new]. If no base path is set, the
/// deserialization will fail unless the path being deserialized is already
/// absolute.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema, TS)]
pub struct AbsolutePathBuf(PathBuf);

impl AbsolutePathBuf {
    fn maybe_expand_home_directory(path: &Path) -> PathBuf {
        if let Some(path_str) = path.to_str()
            && let Some(rest) = path_str.strip_prefix('~')
            && (rest.is_empty() || rest.starts_with(['/', '\\']))
            && let Some(home) = home_dir()
        {
            if rest.is_empty() {
                return home;
            } else if let Some(rest) = rest.strip_prefix('/') {
                return home.join(rest.trim_start_matches('/'));
            } else if let Some(rest) = rest.strip_prefix('\\') {
                return home.join(rest.trim_start_matches('\\'));
            }
        }
        path.to_path_buf()
    }

    /// Resolve a path without consulting the process working directory.
    ///
    /// # Panics
    /// Panics if the supplied path and base do not resolve to an absolute path.
    /// Validate raw bases with [`Self::from_absolute_path_checked`] before use.
    pub fn resolve_path_against_base<P: AsRef<Path>, B: AsRef<Path>>(
        path: P,
        base_path: B,
    ) -> Self {
        let expanded = Self::maybe_expand_home_directory(path.as_ref());
        let expanded = normalize_path_for_platform(&expanded);
        let base_path = normalize_path_for_platform(base_path.as_ref());
        let resolved = absolutize::absolutize_from(expanded.as_ref(), base_path.as_ref());
        assert!(resolved.is_absolute(), "path base must be absolute");
        Self(resolved)
    }

    pub fn from_absolute_path<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        let expanded = Self::maybe_expand_home_directory(path.as_ref());
        let expanded = normalize_path_for_platform(&expanded);
        Ok(Self(absolutize::absolutize(expanded.as_ref())?))
    }

    pub fn from_absolute_path_checked<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        let expanded = Self::maybe_expand_home_directory(path.as_ref());
        let expanded = normalize_path_for_platform(&expanded);
        if !expanded.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("path is not absolute: {}", path.as_ref().display()),
            ));
        }

        Ok(Self(absolutize::absolutize_from(
            expanded.as_ref(),
            Path::new("/"),
        )))
    }

    pub fn current_dir() -> std::io::Result<Self> {
        Self::from_absolute_path(std::env::current_dir()?)
    }

    /// Construct an absolute path from `path`, resolving relative paths against
    /// the process current working directory.
    pub fn relative_to_current_dir<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        Ok(Self::resolve_path_against_base(
            path,
            std::env::current_dir()?,
        ))
    }

    pub fn join<P: AsRef<Path>>(&self, path: P) -> Self {
        Self::resolve_path_against_base(path, &self.0)
    }

    pub fn canonicalize(&self) -> std::io::Result<Self> {
        dunce::canonicalize(&self.0).map(Self)
    }

    pub fn parent(&self) -> Option<Self> {
        self.0.parent().map(|p| {
            debug_assert!(
                p.is_absolute(),
                "parent of AbsolutePathBuf must be absolute"
            );
            Self(p.to_path_buf())
        })
    }

    pub fn ancestors(&self) -> impl Iterator<Item = Self> + '_ {
        self.0.ancestors().map(|p| {
            debug_assert!(
                p.is_absolute(),
                "ancestor of AbsolutePathBuf must be absolute"
            );
            Self(p.to_path_buf())
        })
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }

    pub fn to_path_buf(&self) -> PathBuf {
        self.0.clone()
    }

    pub fn to_string_lossy(&self) -> std::borrow::Cow<'_, str> {
        self.0.to_string_lossy()
    }

    pub fn display(&self) -> Display<'_> {
        self.0.display()
    }
}

fn normalize_path_for_platform(path: &Path) -> Cow<'_, Path> {
    // dunce only simplifies native Windows paths when ordinary-path semantics
    // are equivalent; foreign backslashes and unsafe verbatim paths stay intact.
    Cow::Borrowed(dunce::simplified(path))
}

/// Normalize supported Windows device-path prefixes into ordinary absolute paths.
pub fn normalize_windows_device_path(path: &str) -> Option<String> {
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        return Some(format!(r"\\{unc}"));
    }
    if let Some(unc) = path.strip_prefix(r"\\.\UNC\") {
        return Some(format!(r"\\{unc}"));
    }
    if let Some(path) = path.strip_prefix(r"\\?\")
        && is_windows_drive_absolute_path(path)
    {
        return Some(path.to_string());
    }
    if let Some(path) = path.strip_prefix(r"\\.\")
        && is_windows_drive_absolute_path(path)
    {
        return Some(path.to_string());
    }
    None
}

fn is_windows_drive_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}

/// Return whether `path` is absolute according to Windows path syntax.
pub fn is_windows_absolute_path(path: &str) -> bool {
    is_windows_drive_absolute_path(path) || path.starts_with(r"\\") || path.starts_with("//")
}

/// Canonicalize a path when possible, but preserve the logical absolute path
/// whenever canonicalization would rewrite it through a nested symlink.
///
/// Top-level system aliases such as macOS `/var -> /private/var` still remain
/// canonicalized so existing runtime expectations around those paths stay
/// stable. If the full path cannot be canonicalized, this returns the logical
/// absolute path; use [`canonicalize_existing_preserving_symlinks`] for paths
/// that must exist.
pub fn canonicalize_preserving_symlinks(path: &Path) -> std::io::Result<PathBuf> {
    canonicalize_with_symlink_policy(path, false)
}

/// Canonicalize an existing path while preserving the logical absolute path
/// whenever canonicalization would rewrite it through a nested symlink.
///
/// Unlike [`canonicalize_preserving_symlinks`], canonicalization failures are
/// propagated so callers can reject invalid working directories early.
pub fn canonicalize_existing_preserving_symlinks(path: &Path) -> std::io::Result<PathBuf> {
    canonicalize_with_symlink_policy(path, true)
}

fn canonicalize_with_symlink_policy(
    path: &Path,
    require_existing: bool,
) -> std::io::Result<PathBuf> {
    let expanded = AbsolutePathBuf::maybe_expand_home_directory(path);
    let expanded = normalize_path_for_platform(&expanded);
    let input = if expanded.is_absolute() {
        expanded.into_owned()
    } else {
        absolutize::path_with_base(expanded.as_ref(), &std::env::current_dir()?)
    };
    let logical = absolutize::absolutize_from(&input, Path::new("/"));
    let canonical = match dunce::canonicalize(&input) {
        Ok(canonical) => canonical,
        Err(error) if require_existing => return Err(error),
        Err(_) => return Ok(logical),
    };
    // Removing `symlink/..` lexically can name a different (or missing) file.
    // In that case only the canonical result establishes filesystem identity.
    if canonical != logical
        && !input
            .components()
            .any(|part| part == std::path::Component::ParentDir)
        && should_preserve_logical_path(&logical)
    {
        Ok(logical)
    } else {
        Ok(canonical)
    }
}

fn should_preserve_logical_path(logical: &Path) -> bool {
    logical.ancestors().any(|ancestor| {
        let Ok(metadata) = std::fs::symlink_metadata(ancestor) else {
            return false;
        };
        metadata.file_type().is_symlink() && ancestor.parent().and_then(Path::parent).is_some()
    })
}

impl AsRef<Path> for AbsolutePathBuf {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl std::ops::Deref for AbsolutePathBuf {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<AbsolutePathBuf> for PathBuf {
    fn from(path: AbsolutePathBuf) -> Self {
        path.into_path_buf()
    }
}

/// Helpers for constructing absolute paths in tests.
pub mod test_support {
    use super::AbsolutePathBuf;
    use std::path::Path;
    use std::path::PathBuf;

    /// Creates a native absolute [`PathBuf`] from a slash-separated test path.
    ///
    /// On Windows, `/tmp/example` maps to `C:\tmp\example`.
    pub fn test_path_buf(test_path: &str) -> PathBuf {
        let mut path = PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" });
        path.extend(
            test_path
                .trim_start_matches('/')
                .split('/')
                .filter(|segment| !segment.is_empty()),
        );
        path
    }

    /// Extension methods for converting paths into [`AbsolutePathBuf`] values in tests.
    pub trait PathExt {
        /// Converts an already absolute path into an [`AbsolutePathBuf`].
        fn abs(&self) -> AbsolutePathBuf;
    }

    impl PathExt for Path {
        #[expect(clippy::expect_used)]
        fn abs(&self) -> AbsolutePathBuf {
            AbsolutePathBuf::from_absolute_path_checked(self)
                .expect("path should already be absolute")
        }
    }

    /// Extension methods for converting path buffers into [`AbsolutePathBuf`] values in tests.
    pub trait PathBufExt {
        /// Converts an already absolute path buffer into an [`AbsolutePathBuf`].
        fn abs(&self) -> AbsolutePathBuf;
    }

    impl PathBufExt for PathBuf {
        fn abs(&self) -> AbsolutePathBuf {
            self.as_path().abs()
        }
    }
}

impl TryFrom<&Path> for AbsolutePathBuf {
    type Error = std::io::Error;

    fn try_from(value: &Path) -> Result<Self, Self::Error> {
        Self::from_absolute_path(value)
    }
}

impl TryFrom<PathBuf> for AbsolutePathBuf {
    type Error = std::io::Error;

    fn try_from(value: PathBuf) -> Result<Self, Self::Error> {
        Self::from_absolute_path(value)
    }
}

impl TryFrom<&str> for AbsolutePathBuf {
    type Error = std::io::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::from_absolute_path(value)
    }
}

impl TryFrom<String> for AbsolutePathBuf {
    type Error = std::io::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::from_absolute_path(value)
    }
}

thread_local! {
    static ABSOLUTE_PATH_BASE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Ensure this guard is held while deserializing `AbsolutePathBuf` values to
/// provide a base path for resolving relative paths. Because this relies on
/// thread-local storage, the deserialization must be single-threaded and
/// occur on the same thread that created the guard.
pub struct AbsolutePathBufGuard {
    previous_base: Option<PathBuf>,
    _same_thread: PhantomData<Rc<()>>,
}

impl AbsolutePathBufGuard {
    /// Set the base for synchronous deserialization on this thread.
    ///
    /// # Panics
    /// Panics if `base_path` is not absolute. Validate raw bases with
    /// [`AbsolutePathBuf::from_absolute_path_checked`] before use.
    pub fn new(base_path: &Path) -> Self {
        assert!(
            base_path.is_absolute(),
            "deserialization base must be absolute"
        );
        let previous_base =
            ABSOLUTE_PATH_BASE.with(|cell| cell.replace(Some(base_path.to_path_buf())));
        Self {
            previous_base,
            _same_thread: PhantomData,
        }
    }
}

impl Drop for AbsolutePathBufGuard {
    fn drop(&mut self) {
        ABSOLUTE_PATH_BASE.with(|cell| {
            *cell.borrow_mut() = self.previous_base.take();
        });
    }
}

impl<'de> Deserialize<'de> for AbsolutePathBuf {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let path = PathBuf::deserialize(deserializer)?;
        ABSOLUTE_PATH_BASE.with(|cell| match cell.borrow().as_deref() {
            Some(base) => Ok(Self::resolve_path_against_base(path, base)),
            None if path.is_absolute() => {
                Self::from_absolute_path(path).map_err(SerdeError::custom)
            }
            None => Err(SerdeError::custom(
                "AbsolutePathBuf deserialized without a base path",
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_path_buf;
    use pretty_assertions::assert_eq;
    use std::fs;

    use tempfile::tempdir;

    #[test]
    #[should_panic(expected = "path base must be absolute")]
    fn relative_base_cannot_construct_an_absolute_path() {
        let _ = AbsolutePathBuf::resolve_path_against_base("file.txt", "relative-base");
    }

    #[test]
    fn invalid_guard_base_does_not_replace_outer_base() {
        let dir = tempdir().expect("base directory");
        let _guard = AbsolutePathBufGuard::new(dir.path());
        assert!(
            std::panic::catch_unwind(|| AbsolutePathBufGuard::new(Path::new("relative"))).is_err()
        );
        let resolved: AbsolutePathBuf =
            serde_json::from_str(r#""file.txt""#).expect("outer base remains active");
        assert_eq!(resolved.as_path(), dir.path().join("file.txt"));
    }

    #[test]
    fn canonicalization_expands_home_before_checking_existence() {
        let home = home_dir().expect("home directory");
        let expected = canonicalize_existing_preserving_symlinks(&home).expect("existing home");
        assert_eq!(
            canonicalize_existing_preserving_symlinks(Path::new("~")).expect("expanded home"),
            expected
        );
        assert_eq!(
            canonicalize_preserving_symlinks(Path::new("~")).expect("expanded home"),
            expected
        );
    }

    #[test]
    #[cfg(unix)]
    fn symlink_parent_traversal_returns_the_path_that_was_checked() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().expect("directory");
        let root = dir.path();
        fs::create_dir(root.join("a")).expect("a");
        fs::create_dir_all(root.join("b/inside")).expect("inside");
        fs::write(root.join("b/marker"), "expected").expect("marker");
        symlink(root.join("a"), root.join("view")).expect("view symlink");
        symlink(root.join("b/inside"), root.join("a/inner")).expect("inner symlink");
        let input = root.join("view/inner/../marker");
        let expected = dunce::canonicalize(root.join("b/marker")).expect("marker path");
        for resolved in [
            canonicalize_preserving_symlinks(&input),
            canonicalize_existing_preserving_symlinks(&input),
        ] {
            let resolved = resolved.expect("existing path");
            assert_eq!(resolved, expected);
            assert_eq!(
                fs::read_to_string(resolved).expect("read returned path"),
                "expected"
            );
        }
        let logical = root.join("view/inner");
        assert_eq!(
            canonicalize_existing_preserving_symlinks(&logical).expect("logical path"),
            logical
        );
        let missing = root.join("view/missing");
        assert_eq!(
            canonicalize_preserving_symlinks(&missing).expect("missing logical path"),
            missing
        );
    }

    #[test]
    #[cfg(unix)]
    fn native_paths_do_not_rewrite_foreign_windows_prefixes() {
        let dir = tempdir().expect("directory");
        let filename = r"\\?\C:\file";
        let resolved = AbsolutePathBuf::resolve_path_against_base(filename, dir.path());
        assert_eq!(resolved.as_path(), dir.path().join(filename));
    }

    #[test]
    #[cfg(windows)]
    fn unsafe_verbatim_path_components_are_preserved() {
        for raw in [r"\\?\C:\name.", r"\\?\C:\name ", r"\\?\C:\NUL"] {
            let path =
                AbsolutePathBuf::from_absolute_path_checked(raw).expect("absolute verbatim path");
            assert_eq!(path.as_path(), Path::new(raw));
        }
    }

    #[test]
    fn create_with_absolute_path_ignores_base_path() {
        let base_dir = tempdir().expect("base dir");
        let absolute_dir = tempdir().expect("absolute dir");
        let base_path = base_dir.path();
        let absolute_path = absolute_dir.path().join("file.txt");
        let abs_path_buf =
            AbsolutePathBuf::resolve_path_against_base(absolute_path.clone(), base_path);
        assert_eq!(abs_path_buf.as_path(), absolute_path.as_path());
    }

    #[test]
    fn from_absolute_path_checked_rejects_relative_path() {
        let err = AbsolutePathBuf::from_absolute_path_checked("relative/path")
            .expect_err("relative path should fail");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn normalize_windows_device_path_strips_supported_verbatim_prefixes() {
        assert_eq!(
            normalize_windows_device_path(r"\\?\D:\c\x\worktrees\2508\swift-base"),
            Some(r"D:\c\x\worktrees\2508\swift-base".to_string())
        );
        assert_eq!(
            normalize_windows_device_path(r"\\.\D:\c\x\worktrees\2508\swift-base"),
            Some(r"D:\c\x\worktrees\2508\swift-base".to_string())
        );
        assert_eq!(
            normalize_windows_device_path(r"\\?\UNC\server\share\workspace"),
            Some(r"\\server\share\workspace".to_string())
        );
        assert_eq!(
            normalize_windows_device_path(r"\\.\UNC\server\share\workspace"),
            Some(r"\\server\share\workspace".to_string())
        );
        assert_eq!(
            normalize_windows_device_path(r"\\?\GLOBALROOT\Device"),
            None
        );
    }

    #[test]
    fn windows_absolute_path_recognizes_drive_and_unc_forms() {
        assert!(is_windows_absolute_path(r"C:\workspace"));
        assert!(is_windows_absolute_path("C:/workspace"));
        assert!(is_windows_absolute_path(r"\\server\share"));
        assert!(is_windows_absolute_path("//server/share"));
        assert!(!is_windows_absolute_path("workspace/file"));
    }

    #[test]
    #[cfg(windows)]
    fn from_absolute_path_strips_windows_verbatim_prefix() {
        let path =
            AbsolutePathBuf::from_absolute_path_checked(r"\\?\D:\c\x\worktrees\2508\swift-base")
                .expect("verbatim drive path should be absolute");

        assert_eq!(
            path.as_path(),
            Path::new(r"D:\c\x\worktrees\2508\swift-base")
        );
    }

    #[test]
    fn relative_path_is_resolved_against_base_path() {
        let temp_dir = tempdir().expect("base dir");
        let base_dir = temp_dir.path();
        let abs_path_buf = AbsolutePathBuf::resolve_path_against_base("file.txt", base_dir);
        assert_eq!(abs_path_buf.as_path(), base_dir.join("file.txt").as_path());
    }

    #[test]
    fn relative_path_dots_are_normalized_against_base_path() {
        let temp_dir = tempdir().expect("base dir");
        let base_dir = temp_dir.path();
        let abs_path_buf =
            AbsolutePathBuf::resolve_path_against_base("./nested/../file.txt", base_dir);
        assert_eq!(abs_path_buf.as_path(), base_dir.join("file.txt").as_path());
    }

    #[test]
    fn canonicalize_returns_absolute_path_buf() {
        let temp_dir = tempdir().expect("base dir");
        fs::create_dir(temp_dir.path().join("one")).expect("create one dir");
        fs::create_dir(temp_dir.path().join("two")).expect("create two dir");
        fs::write(temp_dir.path().join("two").join("file.txt"), "").expect("write file");
        let abs_path_buf =
            AbsolutePathBuf::from_absolute_path(temp_dir.path().join("one/../two/./file.txt"))
                .expect("absolute path");
        assert_eq!(
            abs_path_buf
                .canonicalize()
                .expect("path should canonicalize")
                .as_path(),
            dunce::canonicalize(temp_dir.path().join("two").join("file.txt"))
                .expect("expected path should canonicalize")
                .as_path()
        );
    }

    #[test]
    fn canonicalize_returns_error_for_missing_path() {
        let temp_dir = tempdir().expect("base dir");
        let abs_path_buf = AbsolutePathBuf::from_absolute_path(temp_dir.path().join("missing.txt"))
            .expect("absolute path");

        assert!(abs_path_buf.canonicalize().is_err());
    }

    #[test]
    fn ancestors_returns_absolute_path_bufs() {
        let abs_path_buf =
            AbsolutePathBuf::from_absolute_path_checked(test_path_buf("/tmp/one/two"))
                .expect("absolute path");

        let ancestors = abs_path_buf
            .ancestors()
            .map(|path| path.to_path_buf())
            .collect::<Vec<_>>();

        let expected = vec![
            test_path_buf("/tmp/one/two"),
            test_path_buf("/tmp/one"),
            test_path_buf("/tmp"),
            test_path_buf("/"),
        ];

        assert_eq!(ancestors, expected);
    }

    #[test]
    fn relative_to_current_dir_resolves_relative_path() -> std::io::Result<()> {
        let current_dir = std::env::current_dir()?;
        let abs_path_buf = AbsolutePathBuf::relative_to_current_dir("file.txt")?;
        assert_eq!(
            abs_path_buf.as_path(),
            current_dir.join("file.txt").as_path()
        );
        Ok(())
    }

    #[test]
    fn guard_used_in_deserialization() {
        let temp_dir = tempdir().expect("base dir");
        let base_dir = temp_dir.path();
        let relative_path = "subdir/file.txt";
        let abs_path_buf = {
            let _guard = AbsolutePathBufGuard::new(base_dir);
            serde_json::from_str::<AbsolutePathBuf>(&format!(r#""{relative_path}""#))
                .expect("failed to deserialize")
        };
        assert_eq!(
            abs_path_buf.as_path(),
            base_dir.join(relative_path).as_path()
        );
    }

    #[test]
    fn nested_guards_restore_deserialization_base() {
        let outer_dir = tempdir().expect("outer base");
        let inner_dir = tempdir().expect("inner base");
        let input = r#""relative.txt""#;
        let deserialize = || serde_json::from_str::<AbsolutePathBuf>(input);
        assert!(deserialize().is_err());
        {
            let _outer = AbsolutePathBufGuard::new(outer_dir.path());
            assert_eq!(
                deserialize().expect("outer path").as_path(),
                outer_dir.path().join("relative.txt")
            );
            {
                let _inner = AbsolutePathBufGuard::new(inner_dir.path());
                assert_eq!(
                    deserialize().expect("inner path").as_path(),
                    inner_dir.path().join("relative.txt")
                );
            }
            assert_eq!(
                deserialize().expect("restored outer path").as_path(),
                outer_dir.path().join("relative.txt")
            );
        }
        assert!(deserialize().is_err());
    }

    #[test]
    fn home_directory_root_is_expanded_in_deserialization() {
        let home = home_dir().expect("home directory");
        let temp_dir = tempdir().expect("base dir");
        let abs_path_buf = {
            let _guard = AbsolutePathBufGuard::new(temp_dir.path());
            serde_json::from_str::<AbsolutePathBuf>("\"~\"").expect("failed to deserialize")
        };
        assert_eq!(abs_path_buf.as_path(), home.as_path());
    }

    #[test]
    fn home_directory_subpath_is_expanded_in_deserialization() {
        let home = home_dir().expect("home directory");
        let temp_dir = tempdir().expect("base dir");
        let abs_path_buf = {
            let _guard = AbsolutePathBufGuard::new(temp_dir.path());
            serde_json::from_str::<AbsolutePathBuf>("\"~/code\"").expect("failed to deserialize")
        };
        assert_eq!(abs_path_buf.as_path(), home.join("code").as_path());
    }

    #[test]
    fn home_directory_double_slash_is_expanded_in_deserialization() {
        let home = home_dir().expect("home directory");
        let temp_dir = tempdir().expect("base dir");
        let abs_path_buf = {
            let _guard = AbsolutePathBufGuard::new(temp_dir.path());
            serde_json::from_str::<AbsolutePathBuf>("\"~//code\"").expect("failed to deserialize")
        };
        assert_eq!(abs_path_buf.as_path(), home.join("code").as_path());
    }

    #[test]
    fn canonicalize_existing_preserving_symlinks_errors_for_missing_path() {
        let temp_dir = tempdir().expect("temp dir");
        let missing = temp_dir.path().join("missing");

        let err = canonicalize_existing_preserving_symlinks(&missing)
            .expect_err("missing path should fail canonicalization");

        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn home_directory_backslash_subpath_is_expanded_in_deserialization() {
        let home = home_dir().expect("home directory");
        let temp_dir = tempdir().expect("base dir");
        let abs_path_buf = {
            let _guard = AbsolutePathBufGuard::new(temp_dir.path());
            let input =
                serde_json::to_string(r#"~\code"#).expect("string should serialize as JSON");
            serde_json::from_str::<AbsolutePathBuf>(&input).expect("is valid abs path")
        };
        assert_eq!(abs_path_buf.as_path(), home.join("code").as_path());
    }

    #[test]
    fn canonicalize_preserving_symlinks_avoids_verbatim_prefixes() {
        let temp_dir = tempdir().expect("temp dir");

        let canonicalized =
            canonicalize_preserving_symlinks(temp_dir.path()).expect("canonicalize");

        assert_eq!(
            canonicalized,
            dunce::canonicalize(temp_dir.path()).expect("canonicalize temp dir")
        );
        assert!(
            !canonicalized.to_string_lossy().starts_with(r"\\?\"),
            "expected a non-verbatim Windows path, got {canonicalized:?}"
        );
    }
}
