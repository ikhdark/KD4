//! JSON serialization helpers for output that must remain parseable as JSON
//! while staying safe for ASCII-only transports.

use std::io;

use serde::Serialize;

struct AsciiJsonFormatter;

impl serde_json::ser::Formatter for AsciiJsonFormatter {
    // serde_json has no ensure_ascii flag; this formatter keeps its serializer
    // in charge and escapes non-ASCII string fragments and HTTP-unsafe DEL.
    fn write_string_fragment<W>(&mut self, writer: &mut W, fragment: &str) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        let mut start = 0;
        for (index, ch) in fragment.char_indices() {
            if ch.is_ascii() && ch != '\u{7f}' {
                continue;
            }

            if start < index {
                writer.write_all(&fragment.as_bytes()[start..index])?;
            }

            let mut utf16 = [0; 2];
            for code_unit in ch.encode_utf16(&mut utf16) {
                write!(writer, "\\u{code_unit:04x}")?;
            }
            start = index + ch.len_utf8();
        }

        if start < fragment.len() {
            writer.write_all(&fragment.as_bytes()[start..])?;
        }

        Ok(())
    }
}

/// Serialize JSON while escaping non-ASCII string content and DEL as `\uXXXX`.
///
/// This is useful when JSON needs to remain parseable as JSON but must be
/// carried through ASCII-safe transports such as HTTP headers.
/// Raw JSON containing non-printable or non-ASCII bytes is rejected, not reparsed.
pub fn to_ascii_json_string<T>(value: &T) -> serde_json::Result<String>
where
    T: Serialize + ?Sized,
{
    let mut bytes = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut bytes, AsciiJsonFormatter);
    value.serialize(&mut serializer)?;
    if bytes.iter().any(|byte| !(b' '..=b'~').contains(byte)) {
        return Err(serde_json::Error::io(io::Error::new(
            io::ErrorKind::InvalidData,
            "serialized JSON contains bytes unsafe for ASCII-only transports",
        )));
    }
    String::from_utf8(bytes)
        .map_err(|err| serde_json::Error::io(io::Error::new(io::ErrorKind::InvalidData, err)))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use pretty_assertions::assert_eq;
    use serde::Serialize;
    use serde::ser::SerializeStruct;
    use serde_json::Value;
    use serde_json::json;

    use super::to_ascii_json_string;

    #[test]
    fn to_ascii_json_string_rejects_unsafe_raw_values_without_rewriting_safe_raw_json() {
        for source in [
            "{\"label\":\"東京\"}",
            "{\"label\":\"\u{7f}\"}",
            "{\n\"label\":1\n}",
        ] {
            let raw = serde_json::value::RawValue::from_string(source.into()).unwrap();
            assert!(to_ascii_json_string(&raw).is_err());
            assert!(to_ascii_json_string(&std::collections::BTreeMap::from([("raw", &raw)])).is_err());
        }
        let source = r#"{"n":1e+09,"n":9007199254740993123456789,"label":"\u6771"}"#;
        let raw = serde_json::value::RawValue::from_string(source.into()).unwrap();
        assert_eq!(to_ascii_json_string(&raw).unwrap(), source);
    }

    #[test]
    fn to_ascii_json_string_escapes_del_in_keys_and_values() {
        let value = json!({"key\u{7f}": "\u{7f}\n\t\"\\東京🚀"});
        let serialized = to_ascii_json_string(&value).expect("serialize header-safe json");
        assert!(serialized.contains(r"\u007f"));
        assert!(serialized.bytes().all(|byte| (b' '..=b'~').contains(&byte)));
        assert_eq!(serde_json::from_str::<Value>(&serialized).unwrap(), value);
    }

    #[test]
    fn to_ascii_json_string_escapes_non_ascii_strings() {
        struct TestPayload;

        impl Serialize for TestPayload {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                let workspaces = BTreeMap::from([("/tmp/東京", TestWorkspace)]);
                let mut state = serializer.serialize_struct("TestPayload", 1)?;
                state.serialize_field("workspaces", &workspaces)?;
                state.end()
            }
        }

        struct TestWorkspace;

        impl Serialize for TestWorkspace {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                let mut state = serializer.serialize_struct("TestWorkspace", 2)?;
                state.serialize_field("label", "Agentlarım")?;
                state.serialize_field("emoji", "🚀")?;
                state.end()
            }
        }

        let value = TestPayload;
        let expected_value = json!({
            "workspaces": {
                "/tmp/東京": {
                    "label": "Agentlarım",
                    "emoji": "🚀"
                }
            }
        });

        let serialized = to_ascii_json_string(&value).expect("serialize ascii json");

        assert_eq!(
            serialized,
            r#"{"workspaces":{"/tmp/\u6771\u4eac":{"label":"Agentlar\u0131m","emoji":"\ud83d\ude80"}}}"#
        );
        assert!(serialized.is_ascii());
        assert!(!serialized.contains("東京"));
        assert!(!serialized.contains("Agentlarım"));
        assert!(!serialized.contains("🚀"));
        let parsed: Value = serde_json::from_str(&serialized).expect("serialized json");
        assert_eq!(parsed, expected_value);
    }
}
