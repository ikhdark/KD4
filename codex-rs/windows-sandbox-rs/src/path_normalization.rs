use std::path::Path;
use std::path::PathBuf;

pub fn canonicalize_path(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub fn canonical_path_key(path: &Path) -> String {
    canonicalize_path(path)
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase()
}

/// Lossless grant identity. Never reuse ambiguous legacy case-folded entries.
pub(crate) fn security_path_key(path: &Path) -> String {
    use std::os::windows::ffi::OsStrExt;
    let units = canonicalize_path(path).as_os_str().encode_wide()
        .map(|unit| format!("{unit:04x}")).collect::<String>();
    format!("v2:utf16:{units}")
}

