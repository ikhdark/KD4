use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

use crate::models::ImageDetail;

/// Conservative cap so one user message cannot monopolize a large context window.
pub const MAX_USER_INPUT_TEXT_CHARS: usize = 1 << 20;

/// User input
#[non_exhaustive]
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, TS, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserInput {
    Text {
        text: String,
        /// UI-defined spans within `text` that should be treated as special elements.
        /// These are byte ranges into the UTF-8 `text` buffer and are used to render
        /// or persist rich input markers (e.g., image placeholders) across history
        /// and resume without mutating the literal text.
        #[serde(default)]
        text_elements: Vec<TextElement>,
    },
    /// Pre‑encoded data: URI image.
    Image {
        image_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        detail: Option<ImageDetail>,
    },

    /// Local image path provided by the user.  This will be converted to an
    /// `Image` variant (base64 data URL) during request serialization.
    LocalImage {
        path: std::path::PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        detail: Option<ImageDetail>,
    },

    /// Skill selected by the user (name + path to SKILL.md).
    Skill {
        name: String,
        path: std::path::PathBuf,
    },
    /// Explicit structured mention selected by the user.
    ///
    /// `path` identifies the exact mention target, for example
    /// `app://<connector-id>` or `plugin://<plugin-name>@<marketplace-name>`.
    Mention { name: String, path: String },
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, TS, JsonSchema)]
pub struct TextElement {
    /// Byte range in the parent `text` buffer that this element occupies.
    pub byte_range: ByteRange,
    /// Optional human-readable placeholder for the element, displayed in the UI.
    placeholder: Option<String>,
}

impl TextElement {
    pub fn new(byte_range: ByteRange, placeholder: Option<String>) -> Self {
        Self {
            byte_range,
            placeholder,
        }
    }

    /// Returns a copy of this element with a remapped byte range.
    ///
    /// The placeholder is preserved as-is; callers must ensure the new range
    /// still refers to the same logical element (and same placeholder)
    /// within the new text.
    pub fn map_range<F>(&self, map: F) -> Self
    where
        F: FnOnce(ByteRange) -> ByteRange,
    {
        Self {
            byte_range: map(self.byte_range),
            placeholder: self.placeholder.clone(),
        }
    }

    pub fn set_placeholder(&mut self, placeholder: Option<String>) {
        self.placeholder = placeholder;
    }

    /// Returns the stored placeholder without falling back to the text buffer.
    ///
    /// This must only be used inside `From<TextElement>` implementations on equivalent
    /// protocol types where the source text is unavailable. Prefer `placeholder(text)`
    /// everywhere else.
    #[doc(hidden)]
    pub fn _placeholder_for_conversion_only(&self) -> Option<&str> {
        self.placeholder.as_deref()
    }

    pub fn placeholder<'a>(&'a self, text: &'a str) -> Option<&'a str> {
        self.placeholder
            .as_deref()
            .or_else(|| text.get(self.byte_range.start..self.byte_range.end))
    }

    pub fn validate_for_text(&self, text: &str) -> Result<(), String> {
        self.byte_range
            .validate_for_text(text)
            .map_err(|err| format!("text element range is invalid: {err}"))
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, TS, JsonSchema)]
pub struct ByteRange {
    /// Start byte offset (inclusive) within the UTF-8 text buffer.
    pub start: usize,
    /// End byte offset (exclusive) within the UTF-8 text buffer.
    pub end: usize,
}

impl ByteRange {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn validate_for_text(&self, text: &str) -> Result<(), String> {
        if self.start > self.end {
            return Err("start must be less than or equal to end".to_string());
        }
        if self.end > text.len() {
            return Err("range end is outside the text".to_string());
        }
        if !text.is_char_boundary(self.start) || !text.is_char_boundary(self.end) {
            return Err("range must fall on a UTF-8 character boundary".to_string());
        }
        Ok(())
    }
}

impl From<std::ops::Range<usize>> for ByteRange {
    fn from(range: std::ops::Range<usize>) -> Self {
        Self {
            start: range.start,
            end: range.end,
        }
    }
}

impl UserInput {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            UserInput::Text {
                text,
                text_elements,
            } => {
                for (idx, element) in text_elements.iter().enumerate() {
                    element
                        .validate_for_text(text)
                        .map_err(|err| format!("text element {idx}: {err}"))?;
                }
                Ok(())
            }
            UserInput::Image { image_url, .. } => {
                if image_url.trim().is_empty() {
                    Err("image_url cannot be empty".to_string())
                } else {
                    Ok(())
                }
            }
            UserInput::LocalImage { path, .. } => {
                if path.as_os_str().is_empty() {
                    Err("path cannot be empty".to_string())
                } else {
                    Ok(())
                }
            }
            UserInput::Skill { name, path } => {
                if name.trim().is_empty() {
                    Err("skill name cannot be empty".to_string())
                } else if path.as_os_str().is_empty() {
                    Err("skill path cannot be empty".to_string())
                } else {
                    Ok(())
                }
            }
            UserInput::Mention { name, path } => {
                if name.trim().is_empty() {
                    Err("mention name cannot be empty".to_string())
                } else if path.trim().is_empty() {
                    Err("mention path cannot be empty".to_string())
                } else {
                    Ok(())
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "user_input_tests.rs"]
mod tests;
