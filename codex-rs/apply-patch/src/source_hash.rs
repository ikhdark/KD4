//! The one owner of patch-supplied whole-file source hashes: `Guard File`
//! `Expect:`, `codex-range`, `codex-replace`, `codex-anchor` and `codex-json`.
use std::fmt;

/// Shortest accepted hexadecimal prefix.
const MIN_PREFIX_LEN: usize = 12;
const FULL_LEN: usize = 64;

/// A patch-supplied claim about a file revision: the whole-file raw SHA-256,
/// complete or as a hexadecimal prefix of at least 12 characters. It is not the
/// revision identity. Resolve it against the current full hash with
/// [`matches`](Self::matches); never key, store or compare it as the digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceHashRef(String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceHashRefError {
    TooShort(usize),
    TooLong(usize),
    NotHex,
}

impl SourceHashRef {
    /// Required spelling, shared by every diagnostic.
    pub const FORM: &'static str = "sha256:<hex prefix of the whole-file raw source_sha256: at least 12 characters, up to all 64>";

    /// Accepts upper- or lowercase hexadecimal of 12 to 64 characters.
    pub fn parse(text: &str) -> Result<Self, SourceHashRefError> {
        if !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(SourceHashRefError::NotHex);
        }
        match text.len() {
            length if length < MIN_PREFIX_LEN => Err(SourceHashRefError::TooShort(length)),
            length if length > FULL_LEN => Err(SourceHashRefError::TooLong(length)),
            _ => Ok(Self(text.to_ascii_lowercase())),
        }
    }

    /// The supplied hash, lowercased: a prefix or the full digest.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True when `actual_sha256_hex`, the current whole-file hash, starts with
    /// this reference (case-insensitive).
    pub fn matches(&self, actual_sha256_hex: &str) -> bool {
        actual_sha256_hex
            .as_bytes()
            .get(..self.0.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(self.0.as_bytes()))
    }
}

impl fmt::Display for SourceHashRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Display for SourceHashRefError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Self::TooShort(length) => format!("{length} hex characters is fewer than {MIN_PREFIX_LEN}"),
            Self::TooLong(length) => format!("{length} characters is more than the {FULL_LEN} of a SHA-256"),
            Self::NotHex => "it contains a non-hexadecimal character".to_string(),
        };
        write!(formatter, "invalid source hash ({reason}); expected {}", SourceHashRef::FORM)
    }
}
