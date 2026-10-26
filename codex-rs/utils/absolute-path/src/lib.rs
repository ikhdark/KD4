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
            let Some(separator) = rest.chars().next() else {
                return home;
            };
            // Consume the whole separator run: a remaining root, such as the `\`
            // in `~/\code` on Windows, would otherwise replace `home` in `join`.
            let rest =
                rest.trim_start_matches(|c: char| c == separator || std::path::is_separator(c));
            return home.join(rest);
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

    /// Resolve a native filesystem payload literally, without expanding `~`.
    /// The base must be absolute, as for [`Self::resolve_path_against_base`].
    pub fn resolve_literal_path_against_base<P: AsRef<Path>, B: AsRef<Path>>(
        path: P,
        base_path: B,
    ) -> Self {
        let path = normalize_path_for_platform(path.as_ref());
        let base_path = normalize_path_for_platform(base_path.as_ref());
        let resolved = absolutize::absolutize_from(path.as_ref(), base_path.as_ref());
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
        if path.as_ref().is_absolute() {
            return Self::from_absolute_path(path);
        }

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
    // dunce handles drive aliases, but deliberately leaves VerbatimUNC alone.
    // Reuse its filename/device/length checks for the UNC components before
    // removing that namespace; unsafe or non-Unicode paths stay verbatim.
    #[cfg(windows)]
    if matches!(path.components().next(),
        Some(std::path::Component::Prefix(prefix))
            if matches!(prefix.kind(), std::path::Prefix::VerbatimUNC(_, _)))
        && let Some(raw) = path.to_str()
        && let Some(tail) = raw.strip_prefix(r"\\?\UNC\")
        && raw.encode_utf16().count() <= 260
    {
        let probe = PathBuf::from(format!(r"\\?\C:\{tail}"));
        if dunce::simplified(&probe) != probe {
            return Cow::Owned(PathBuf::from(format!(r"\\{tail}")));
        }
    }
    Cow::Borrowed(dunce::simplified(path))
}

/// Simplify device syntax only when ordinary Windows parsing preserves identity.
/// None means the caller must retain the original spelling.
pub fn normalize_windows_device_path(path: &str) -> Option<String> {
    let tail = path
        .strip_prefix(r"\\?\")
        .or_else(|| path.strip_prefix(r"\\.\"))?;
    let (normalized, components, unc) = if let Some(unc) = tail.strip_prefix(r"UNC\") {
        (format!(r"\\{unc}"), unc, true)
    } else if is_windows_drive_absolute_path(tail) && tail.as_bytes()[2] == b'\\' {
        (tail.to_owned(), &tail[3..], false)
    } else {
        return None;
    };
    if path.encode_utf16().count() > 260 {
        return None;
    }
    let components = components.strip_suffix('\\').unwrap_or(components);
    if unc && components.split('\\').count() < 2 {
        return None;
    }
    if (!components.is_empty() || unc)
        && !components
            .split('\\')
            .all(ordinary_windows_component_is_safe)
    {
        return None;
    }
    Some(normalized)
}

fn ordinary_windows_component_is_safe(component: &str) -> bool {
    if component.is_empty()
        || component.encode_utf16().count() > 255
        || component.ends_with(['.', ' '])
        || component.bytes().any(|ch| {
            ch < 32 || matches!(ch, b'<' | b'>' | b':' | b'"' | b'/' | b'|' | b'?' | b'*')
        })
    {
        return false;
    }
    let stem = component
        .split('.')
        .next()
        .unwrap_or(component)
        .trim_end_matches(' ');
    if ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"]
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
    {
        return false;
    }
    !["COM", "LPT"].iter().any(|prefix| {
        stem.get(..3)
            .is_some_and(|value| value.eq_ignore_ascii_case(prefix))
            && stem.get(3..).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
    })
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
/// stable. If the full path cannot be canonicalized, unresolved parent traversal
/// is retained verbatim rather than changing its filesystem destination. Other
/// missing paths use the logical absolute path. Use
/// [`canonicalize_existing_preserving_symlinks`] for paths that must exist.
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
        Err(_) => {
            return Ok(
                if input
                    .components()
                    .any(|part| part == std::path::Component::ParentDir)
                {
                    input
                } else {
                    logical
                },
            );
        }
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

