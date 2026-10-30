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
) -> Result<(String, Option<DecodedSource>), FunctionCallError> {
    let error = |message: &str| FunctionCallError::RespondToModel(message.into());
    // FF FE 00 00 is the UTF-32LE BOM; reading it as UTF-16LE would invent NULs.
    let little_endian = if bytes.starts_with(&[0xff, 0xfe, 0x00, 0x00]) {
        return Err(error("read_file does not decode UTF-32; use encoding:hex or encoding:base64 for original bytes"));
    } else if bytes.starts_with(&[0xff, 0xfe]) {
        true
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        false
    } else {
        return String::from_utf8(bytes)
            .map(|text| (text, None))
            .map_err(|_| error("read_file requires UTF-8 or BOM-marked UTF-16LE/BE text; use encoding:hex or encoding:base64 for original bytes"));
    };
    // Decoding and hashing up to 8 MiB stays off async workers, like selection.
    tokio::task::spawn_blocking(move || decode_utf16(&bytes, little_endian, max_bytes))
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("UTF-16 decoding worker failed: {err}"))
        })?
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
        return Err(error("read_file found an incomplete UTF-16 code unit"));
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
        let character =
            character.map_err(|_| error("read_file found an unpaired UTF-16 surrogate"))?;
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
