//! Decode text without confusing the file identity with the selector snapshot.
use crate::FunctionCallError;

pub(super) struct DecodedSource {
    pub encoding: &'static str,
    pub sha256: String,
    pub bytes: usize,
}

pub(super) async fn decode(
    bytes: Vec<u8>,
    max_bytes: usize,
) -> Result<DecodedFile, FunctionCallError> {
    // Recognized encodings precede byte heuristics: UTF-16 zero bytes are
    // code units, not binary evidence.
    let source_encoding = bom_encoding(&bytes);
    if matches!(source_encoding, Some("utf-32le" | "utf-32be")) {
        return Ok(DecodedFile::Unsupported(Classification::unsupported(
            decoding_error("read_file does not decode UTF-32; use encoding:hex or encoding:base64 for original bytes", &bytes).to_string(),
            source_encoding,
        )));
    }
    let little_endian = match source_encoding {
        Some("utf-16le") => true,
        Some("utf-16be") => false,
        _ => {
            return Ok(match String::from_utf8(bytes) {
                Ok(text) => decoded_text(text, None),
                Err(error) => {
                    let mut classification =
                        classify_page(error.as_bytes(), false).unwrap_or_else(|| {
                            Classification::unsupported("unknown text encoding".into(), None)
                        });
                    classification.reason.push_str("; ");
                    classification.reason.push_str(
                        &decoding_error(
                            "invalid UTF-8 is not proof of binary content",
                            error.as_bytes(),
                        )
                        .to_string(),
                    );
                    DecodedFile::Unsupported(classification)
                }
            });
        }
    };
    // Decoding and hashing up to 8 MiB stays off async workers, like selection.
    let result =
        tokio::task::spawn_blocking(move || decode_utf16(&bytes, little_endian, max_bytes))
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!("UTF-16 decoding worker failed: {err}"))
            })?;
    Ok(match result {
        Ok((text, source)) => decoded_text(text, source),
        Err(error) => DecodedFile::Unsupported(Classification::unsupported(
            error.to_string(),
            source_encoding,
        )),
    })
}
fn decode_utf16(
    bytes: &[u8],
    little_endian: bool,
    max_bytes: usize,
) -> Result<(String, Option<DecodedSource>), FunctionCallError> {
    let error = |message: &str| FunctionCallError::RespondToModel(message.into());
    let payload = &bytes[2..];
    let (units, remainder) = payload.as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(decoding_error(
            "read_file found an incomplete UTF-16 code unit",
            bytes,
        ));
    }
    let units = units.iter().map(|&pair| {
        if little_endian {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    });
    let mut text = String::with_capacity(payload.len().min(max_bytes));
    for character in char::decode_utf16(units) {
        let character = character
            .map_err(|_| decoding_error("read_file found an unpaired UTF-16 surrogate", bytes))?;
        if text.len().saturating_add(character.len_utf8()) > max_bytes {
            return Err(error(
                "decoded UTF-16 text exceeds the 8 MiB snapshot limit; use encoding:hex or encoding:base64 for original byte pages",
            ));
        }
        text.push(character);
    }
    Ok((
        text,
        Some(DecodedSource {
            encoding: if little_endian {
                "utf-16le"
            } else {
                "utf-16be"
            },
            sha256: crate::tool_history::sha256(bytes),
            bytes: bytes.len(),
        }),
    ))
}

fn decoding_error(message: &str, bytes: &[u8]) -> FunctionCallError {
    let mut scanner = codex_file_system::TextDiagnosticsScanner::default();
    scanner.push(bytes);
    FunctionCallError::RespondToModel(format!(
        "{message}; text_diagnostics: {}",
        serde_json::json!(scanner.finish())
    ))
}

#[derive(serde::Serialize)]
pub(super) struct Classification {
    kind: &'static str,
    confidence: &'static str,
    reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    encoding: Option<&'static str>,
}

impl Classification {
    pub(super) fn unsupported(reason: String, encoding: Option<&'static str>) -> Self {
        Self {
            kind: "unsupported_text",
            confidence: if encoding.is_some() { "high" } else { "low" },
            reason,
            encoding,
        }
    }
}

pub(super) enum DecodedFile {
    Text(String, Option<DecodedSource>),
    Unsupported(Classification),
}

fn bom_encoding(bytes: &[u8]) -> Option<&'static str> {
    [
        (&[0xff, 0xfe, 0x00, 0x00][..], "utf-32le"),
        (&[0x00, 0x00, 0xfe, 0xff][..], "utf-32be"),
        (&[0xff, 0xfe][..], "utf-16le"),
        (&[0xfe, 0xff][..], "utf-16be"),
    ]
    .into_iter()
    .find_map(|(bom, encoding)| bytes.starts_with(bom).then_some(encoding))
}

fn decoded_text(text: String, source: Option<DecodedSource>) -> DecodedFile {
    if let Some(mut classification) = classify_page(text.as_bytes(), false) {
        classification.encoding = Some(source.as_ref().map_or("utf-8", |source| source.encoding));
        DecodedFile::Unsupported(classification)
    } else {
        DecodedFile::Text(text, source)
    }
}

/// A local byte-range observation, not a whole-file verdict. Original-byte
/// ranges never implicitly decode recognized encodings or split UTF-8.
pub(super) fn classify_page(bytes: &[u8], at_file_start: bool) -> Option<Classification> {
    if at_file_start && let Some(encoding) = bom_encoding(bytes) {
        return Some(Classification::unsupported(
            "recognized encoded text in an original-byte range; no implicit decoding".into(),
            Some(encoding),
        ));
    }
    if bytes
        .iter()
        .any(|byte| matches!(byte, 0..=8 | 11 | 14..=31 | 127))
    {
        return Some(Classification {
            kind: "likely_binary",
            confidence: "medium",
            reason: "non-display control bytes; binary content is likely, not proven".into(),
            encoding: None,
        });
    }
    std::str::from_utf8(bytes).err().map(|_| {
        Classification::unsupported(
            "invalid or split UTF-8; unknown text encoding or binary content, not proven binary"
                .into(),
            None,
        )
    })
}

/// UTF-8 is the default: its canonical bytes are the source bytes, so only a
/// decoded snapshot carries encoding and coordinate-space fields.
pub(super) fn annotate(output: &mut serde_json::Value, source: Option<&DecodedSource>) {
    let Some(source) = source else {
        return;
    };
    output["source_encoding"] = serde_json::json!(source.encoding);
    output["selector_encoding"] = serde_json::json!("utf-8");
    output["selector_coordinate_space"] = serde_json::json!("decoded_utf8");
    output["source_sha256"] = serde_json::json!(source.sha256);
    output["source_bytes"] = serde_json::json!(source.bytes);
}
