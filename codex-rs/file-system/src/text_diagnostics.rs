//! Constant-space decoding and newline accounting over original file bytes.
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct TextDiagnostics {
    pub encoding: String,
    pub encoding_detection: EncodingDetection,
    pub bom: String,
    pub bom_bytes: u8,
    pub valid_encoding: bool,
    pub decoding_error: Option<DecodingError>,
    // Invalid/unknown text has no precise decoded newline claims.
    pub line_endings: Option<LineEndings>,
    pub mixed_endings: Option<bool>,
    pub final_newline: Option<FinalNewline>,
    pub line_count: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EncodingDetection {
    Bom,
    InferredUtf8,
    Unknown,
}

/// The first strict decoding failure; coordinates include the original BOM.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct DecodingError {
    pub encoding: String,
    pub byte_offset: u64,
    pub byte_length: u8,
    pub kind: DecodingErrorKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecodingErrorKind {
    InvalidUtf8Sequence,
    IncompleteUtf8Sequence,
    UnpairedUtf16Surrogate,
    IncompleteUtf16CodeUnit,
    InvalidUtf32Scalar,
    IncompleteUtf32CodeUnit,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct LineEndings {
    pub lf: u64,
    pub crlf: u64,
    pub cr: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FinalNewline {
    #[default]
    None,
    Lf,
    Crlf,
    Cr,
}

#[derive(Clone, Copy, Default)]
enum Encoding {
    #[default]
    Utf8,
    Utf16Le,
    Utf16Be,
    Utf32Le,
    Utf32Be,
}

impl Encoding {
    fn name(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16Le => "utf-16le",
            Self::Utf16Be => "utf-16be",
            Self::Utf32Le => "utf-32le",
            Self::Utf32Be => "utf-32be",
        }
    }
}

/// Fed only bytes accepted by the checksum owner's revision guard. Buffers at
/// most a BOM or one encoded scalar, including across arbitrary chunk splits.
#[derive(Default)]
pub struct TextDiagnosticsScanner {
    prefix: [u8; 4],
    prefix_len: usize,
    started: bool,
    encoding: Encoding,
    bom_bytes: u8,
    unit: [u8; 4],
    unit_len: usize,
    high_surrogate: Option<u16>,
    bytes_seen: u64,
    decoding_error: Option<DecodingError>,
    has_content: bool,
    endings: LineEndings,
    last: FinalNewline,
}

impl TextDiagnosticsScanner {
    pub fn push(&mut self, mut bytes: &[u8]) {
        if !self.started {
            let len = (4 - self.prefix_len).min(bytes.len());
            self.prefix[self.prefix_len..self.prefix_len + len].copy_from_slice(&bytes[..len]);
            self.prefix_len += len;
            bytes = &bytes[len..];
            if self.prefix_len < 4 {
                return;
            }
            self.start();
        }
        for &byte in bytes {
            self.byte(byte);
        }
    }

    fn start(&mut self) {
        let prefix = self.prefix;
        let bytes = &prefix[..self.prefix_len];
        (self.encoding, self.bom_bytes) = if bytes.starts_with(&[0xff, 0xfe, 0, 0]) {
            (Encoding::Utf32Le, 4)
        } else if bytes.starts_with(&[0, 0, 0xfe, 0xff]) {
            (Encoding::Utf32Be, 4)
        } else if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
            (Encoding::Utf8, 3)
        } else if bytes.starts_with(&[0xff, 0xfe]) {
            (Encoding::Utf16Le, 2)
        } else if bytes.starts_with(&[0xfe, 0xff]) {
            (Encoding::Utf16Be, 2)
        } else {
            (Encoding::Utf8, 0)
        };
        self.started = true;
        self.bytes_seen = u64::from(self.bom_bytes);
        for &byte in &bytes[usize::from(self.bom_bytes)..] {
            self.byte(byte);
        }
    }

    fn byte(&mut self, byte: u8) {
        self.bytes_seen += 1;
        if self.decoding_error.is_some() {
            return;
        }
        if matches!(self.encoding, Encoding::Utf8) && self.unit_len == 0 && byte.is_ascii() {
            self.character(char::from(byte));
            return;
        }
        self.unit[self.unit_len] = byte;
        self.unit_len += 1;
        if matches!(self.encoding, Encoding::Utf8) {
            match std::str::from_utf8(&self.unit[..self.unit_len]) {
                Ok(text) => {
                    let character = text.chars().next();
                    self.unit_len = 0;
                    if let Some(character) = character {
                        self.character(character);
                    }
                }
                Err(error) => {
                    if let Some(length) = error.error_len() {
                        self.fail(
                            self.bytes_seen - self.unit_len as u64,
                            length as u8,
                            DecodingErrorKind::InvalidUtf8Sequence,
                        );
                    }
                }
            }
            return;
        }
        let width = match self.encoding {
            Encoding::Utf8 => unreachable!("UTF-8 handled above"),
            Encoding::Utf16Le | Encoding::Utf16Be => 2,
            Encoding::Utf32Le | Encoding::Utf32Be => 4,
        };
        if self.unit_len != width {
            return;
        }
        self.unit_len = 0;
        let character = match self.encoding {
            Encoding::Utf8 => unreachable!("UTF-8 handled above"),
            Encoding::Utf16Le | Encoding::Utf16Be => {
                let pair = [self.unit[0], self.unit[1]];
                let unit = if matches!(self.encoding, Encoding::Utf16Le) {
                    u16::from_le_bytes(pair)
                } else {
                    u16::from_be_bytes(pair)
                };
                if let Some(high) = self.high_surrogate.take() {
                    if (0xdc00..=0xdfff).contains(&unit) {
                        char::from_u32(
                            0x10000 + (u32::from(high) - 0xd800) * 0x400 + u32::from(unit) - 0xdc00,
                        )
                    } else {
                        self.fail(
                            self.bytes_seen - 4,
                            2,
                            DecodingErrorKind::UnpairedUtf16Surrogate,
                        );
                        return;
                    }
                } else if (0xd800..=0xdbff).contains(&unit) {
                    self.high_surrogate = Some(unit);
                    return;
                } else {
                    char::from_u32(u32::from(unit))
                }
            }
            Encoding::Utf32Le => char::from_u32(u32::from_le_bytes(self.unit)),
            Encoding::Utf32Be => char::from_u32(u32::from_be_bytes(self.unit)),
        };
        match character {
            Some(character) => self.character(character),
            None => self.fail(
                self.bytes_seen - width as u64,
                width as u8,
                if width == 2 {
                    DecodingErrorKind::UnpairedUtf16Surrogate
                } else {
                    DecodingErrorKind::InvalidUtf32Scalar
                },
            ),
        }
    }

    fn fail(&mut self, byte_offset: u64, byte_length: u8, kind: DecodingErrorKind) {
        self.decoding_error = Some(DecodingError {
            encoding: self.encoding.name().into(),
            byte_offset,
            byte_length,
            kind,
        });
    }

    fn character(&mut self, character: char) {
        self.has_content = true;
        self.last = match character {
            '\r' => {
                self.endings.cr += 1;
                FinalNewline::Cr
            }
            '\n' if self.last == FinalNewline::Cr => {
                self.endings.cr -= 1;
                self.endings.crlf += 1;
                FinalNewline::Crlf
            }
            '\n' => {
                self.endings.lf += 1;
                FinalNewline::Lf
            }
            _ => FinalNewline::None,
        };
    }

    pub fn finish(mut self) -> TextDiagnostics {
        if !self.started {
            self.start();
        }
        if self.decoding_error.is_none() {
            if self.high_surrogate.is_some() {
                self.fail(
                    self.bytes_seen - self.unit_len as u64 - 2,
                    2,
                    DecodingErrorKind::UnpairedUtf16Surrogate,
                );
            } else if self.unit_len != 0 {
                self.fail(
                    self.bytes_seen - self.unit_len as u64,
                    self.unit_len as u8,
                    match self.encoding {
                        Encoding::Utf8 => DecodingErrorKind::IncompleteUtf8Sequence,
                        Encoding::Utf16Le | Encoding::Utf16Be => {
                            DecodingErrorKind::IncompleteUtf16CodeUnit
                        }
                        Encoding::Utf32Le | Encoding::Utf32Be => {
                            DecodingErrorKind::IncompleteUtf32CodeUnit
                        }
                    },
                );
            }
        }
        let valid = self.decoding_error.is_none();
        let mixed = [self.endings.lf, self.endings.crlf, self.endings.cr]
            .into_iter()
            .filter(|count| *count > 0)
            .count()
            > 1;
        let lines = self.endings.lf
            + self.endings.crlf
            + self.endings.cr
            + u64::from(self.has_content && self.last == FinalNewline::None);
        TextDiagnostics {
            encoding: if !valid && self.bom_bytes == 0 {
                "unknown"
            } else {
                self.encoding.name()
            }
            .into(),
            encoding_detection: if self.bom_bytes != 0 {
                EncodingDetection::Bom
            } else if valid {
                EncodingDetection::InferredUtf8
            } else {
                EncodingDetection::Unknown
            },
            bom: if self.bom_bytes == 0 {
                "none"
            } else {
                self.encoding.name()
            }
            .into(),
            bom_bytes: self.bom_bytes,
            valid_encoding: valid,
            decoding_error: self.decoding_error,
            line_endings: valid.then_some(self.endings),
            mixed_endings: valid.then_some(mixed),
            final_newline: valid.then_some(self.last),
            line_count: valid.then_some(lines),
        }
    }
}
