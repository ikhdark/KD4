//! Validate discardable replay captures during append preflight without building a JSON tree.

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use std::fmt;

const CONTEXT_REPLAY: &str = "context_replay";

pub(super) fn try_project(line: &str, authenticated_body: &[u8]) -> Option<Value> {
    // Small-reference check: reject duplicate, unknown or escaped fields, plus
    // struct-as-sequence encodings. The ordinary reference has already been
    // parsed as Value and used to authenticate these exact body bytes.
    if line.as_bytes().contains(&b'\\') || !line.trim_start().starts_with('{') {
        return None;
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Reference<'a> {
        timestamp: &'a str,
        format_version: u32,
        #[serde(rename = "type")]
        kind: &'a str,
        #[serde(borrow)]
        payload: ReferenceMetadata<'a>,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ReferenceMetadata<'a> {
        sha256: &'a str,
        bytes: u64,
        item_type: &'a str,
    }
    let reference = serde_json::from_str::<Reference<'_>>(line).ok()?;
    if reference.kind != super::KIND
        || reference.format_version != codex_protocol::protocol::CURRENT_ROLLOUT_FORMAT_VERSION
        || reference.payload.item_type != CONTEXT_REPLAY
        || reference.payload.bytes != authenticated_body.len() as u64
        || reference.payload.sha256.len() != 64
        || time::OffsetDateTime::parse(
            reference.timestamp,
            &time::format_description::well_known::Rfc3339,
        )
        .is_err()
    {
        return None;
    }

    let mut parser = serde_json::Deserializer::from_slice(authenticated_body);
    let result = Envelope::Body.deserialize(&mut parser).ok()?;
    parser.end().ok()?;
    Some(result)
}

#[derive(Clone, Copy)]
enum Envelope {
    Body,
    Replay,
}

impl<'de> DeserializeSeed<'de> for Envelope {
    type Value = Value;

    fn deserialize<D: serde::Deserializer<'de>>(self, parser: D) -> Result<Value, D::Error> {
        parser.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Envelope {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a current replay object with unique canonical keys")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut fields = Map::new();
        while let Some(key) = map.next_key::<&str>()? {
            if fields.contains_key(key) {
                return Err(de::Error::custom("duplicate replay envelope key"));
            }
            let value = match (self, key) {
                (Self::Body, "type") => {
                    if map.next_value::<&str>()? != CONTEXT_REPLAY {
                        return Err(de::Error::custom("not a context replay body"));
                    }
                    Value::from(CONTEXT_REPLAY)
                }
                (Self::Body, "payload") => map.next_value_seed(Self::Replay)?,
                (Self::Replay, "version") => {
                    let version = map.next_value::<u8>()?;
                    if version != 1 {
                        return Err(de::Error::custom("unknown context replay version"));
                    }
                    Value::from(version)
                }
                (Self::Replay, "turn_id") => map
                    .next_value::<Option<&str>>()?
                    .map_or(Value::Null, Value::from),
                (Self::Replay, "sampling_request_id" | "physical_attempt_id") => {
                    Value::from(map.next_value::<&str>()?)
                }
                (Self::Replay, "capture") => {
                    map.next_value_seed(Plain)?;
                    Value::Null
                }
                _ => return Err(de::Error::custom("unexpected replay envelope key")),
            };
            fields.insert(key.to_owned(), value);
        }
        let expected = match self {
            Self::Body => 2,
            Self::Replay => 5,
        };
        if fields.len() != expected {
            return Err(de::Error::custom("incomplete replay envelope"));
        }
        Ok(Value::Object(fields))
    }
}

#[derive(Clone, Copy)]
struct Plain;

impl<'de> DeserializeSeed<'de> for Plain {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, parser: D) -> Result<(), D::Error> {
        parser.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Plain {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("plain replay JSON without reserved keys or ambiguous numbers")
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }

    fn visit_str<E: de::Error>(self, _: &str) -> Result<(), E> {
        Ok(())
    }

    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq.next_element_seed(self)?.is_some() {}
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        // The pinned arbitrary_precision parser also exposes non-integer
        // encodings through a synthetic map with a reserved first key.
        // Reject *all* occurrences. Borrowed keys reject JSON-escaped
        // spellings; ordinary duplicate capture keys have deterministic
        // Value last-wins semantics and cannot reach the typed envelope.
        while let Some(key) = map.next_key::<&str>()? {
            if key.starts_with("$serde_json::private::") {
                return Err(de::Error::custom("reserved replay JSON map key"));
            }
            map.next_value_seed(self)?;
        }
        Ok(())
    }
}
