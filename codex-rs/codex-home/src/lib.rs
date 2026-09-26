//! User-instruction loading rooted in the configured Codex home.

use std::borrow::Cow;
use std::io;

use codex_extension_api::LoadUserInstructionsFuture;
use codex_extension_api::LoadedUserInstructions;
use codex_extension_api::UserInstructions;
use codex_extension_api::UserInstructionsProvider;
use codex_utils_absolute_path::AbsolutePathBuf;

const DEFAULT_AGENTS_MD_FILENAME: &str = "AGENTS.md";
const LOCAL_AGENTS_MD_FILENAME: &str = "AGENTS.override.md";

/// Loads user instructions from a Codex home directory.
#[derive(Clone, Debug)]
pub struct CodexHomeUserInstructionsProvider {
    codex_home: AbsolutePathBuf,
}

impl CodexHomeUserInstructionsProvider {
    /// Creates a provider rooted at the supplied absolute Codex home directory.
    pub fn new(codex_home: AbsolutePathBuf) -> Self {
        Self { codex_home }
    }

    async fn load_from_codex_home(&self) -> LoadedUserInstructions {
        let mut warnings = Vec::new();
        for candidate in [LOCAL_AGENTS_MD_FILENAME, DEFAULT_AGENTS_MD_FILENAME] {
            let path = self.codex_home.join(candidate);
            match tokio::fs::metadata(path.as_path()).await {
                Ok(metadata) if !metadata.is_file() => continue,
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    warnings.push(format!(
                        "Failed to read global AGENTS.md instructions from `{}`: {err}",
                        path.display()
                    ));
                    continue;
                }
            }
            let data = match tokio::fs::read(path.as_path()).await {
                Ok(data) => data,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    warnings.push(format!(
                        "Failed to read global AGENTS.md instructions from `{}`: {err}",
                        path.display()
                    ));
                    continue;
                }
            };
            let contents = decode_instructions(&data);
            let trimmed = contents.trim();
            if !trimmed.is_empty() {
                return LoadedUserInstructions {
                    instructions: Some(UserInstructions {
                        text: trimmed.to_string(),
                        source: path,
                    }),
                    warnings,
                };
            }
        }
        LoadedUserInstructions {
            instructions: None,
            warnings,
        }
    }
}

/// Decodes instruction bytes, honoring a UTF-8 or UTF-16 byte-order mark as
/// written by common Windows editors and shells. `str::trim` keeps U+FEFF, so an
/// undecoded mark would let an otherwise empty override shadow `AGENTS.md`.
fn decode_instructions(data: &[u8]) -> Cow<'_, str> {
    if let Some(rest) = data.strip_prefix(b"\xEF\xBB\xBF") {
        String::from_utf8_lossy(rest)
    } else if let Some(rest) = data.strip_prefix(b"\xFF\xFE") {
        Cow::Owned(decode_utf16_lossy(rest, u16::from_le_bytes))
    } else if let Some(rest) = data.strip_prefix(b"\xFE\xFF") {
        Cow::Owned(decode_utf16_lossy(rest, u16::from_be_bytes))
    } else {
        String::from_utf8_lossy(data)
    }
}

fn decode_utf16_lossy(data: &[u8], unit_from_bytes: fn([u8; 2]) -> u16) -> String {
    let (units, truncated_unit) = data.as_chunks::<2>();
    let mut text: String = char::decode_utf16(units.iter().copied().map(unit_from_bytes))
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();
    if !truncated_unit.is_empty() {
        text.push(char::REPLACEMENT_CHARACTER);
    }
    text
}

impl UserInstructionsProvider for CodexHomeUserInstructionsProvider {
    fn load_user_instructions(&self) -> LoadUserInstructionsFuture<'_> {
        Box::pin(self.load_from_codex_home())
    }
}

#[cfg(test)]
mod tests;
