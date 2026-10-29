//! Validate unregistered byte notifications without retaining decoded output.
//! The authoritative notification type still validates every field, in order.

use std::cell::Cell;
use std::io::Read;

use serde::Deserialize;
use serde::Deserializer;
use serde::de::DeserializeOwned;
use serde::de::Visitor;
use serde::de::value::MapDeserializer;
use serde_json::Value;

pub(super) fn parse_without_chunk<T: DeserializeOwned>(
    params: Value,
    chunk_field: &str,
) -> Result<(T, Option<String>), serde_json::Error> {
    let Value::Object(fields) = params else {
        return serde_json::from_value(params).map(|parsed| (parsed, None));
    };
    let deferred = Cell::new(None);
    let fields = fields.into_iter().map(|(key, value)| {
        let chunk = (key == chunk_field).then_some(&deferred);
        (key, NotificationField { value, chunk })
    });
    let parsed = <T as Deserialize>::deserialize(MapDeserializer::<_, serde_json::Error>::new(fields))?;
    Ok((parsed, deferred.into_inner()))
}

struct NotificationField<'a> {
    value: Value,
    chunk: Option<&'a Cell<Option<String>>>,
}

impl<'de> serde::de::IntoDeserializer<'de, serde_json::Error> for NotificationField<'_> {
    type Deserializer = Self;

    fn into_deserializer(self) -> Self { self }
}

// Forward non-string operations to Value unchanged: Option/default handling,
// numeric ranges, enums and malformed field errors stay with the real type.
macro_rules! forward_value {
    ($($method:ident($($argument:ident: $ty:ty),*)),* $(,)?) => {
        $(fn $method<V>(self, $($argument: $ty,)* visitor: V) -> Result<V::Value, Self::Error>
        where V: Visitor<'de> {
            self.value.$method($($argument,)* visitor)
        })*
    };
}

impl<'de> Deserializer<'de> for NotificationField<'_> {
    type Error = serde_json::Error;

    fn deserialize_string<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where V: Visitor<'de> {
        let Some(deferred) = self.chunk else {
            return self.value.deserialize_string(visitor);
        };
        let Value::String(encoded) = self.value else {
            return self.value.deserialize_string(visitor);
        };
        // Use the same STANDARD engine as ByteChunk, retaining its padding,
        // alphabet and trailing-bit checks across the complete input. The
        // decoder's fixed scratch buffers never retain a full decoded payload.
        let mut decoder = base64::read::DecoderReader::new(
            encoded.as_bytes(), &base64::engine::general_purpose::STANDARD,
        );
        let mut scratch = [0u8; 4096];
        loop {
            match decoder.read(&mut scratch) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) => return Err(serde::de::Error::custom(error)),
            }
        }
        deferred.set(Some(encoded));
        // ByteChunk's existing deserializer now observes valid empty base64;
        // rehydration, if a route appeared, uses that same deserializer again.
        visitor.visit_string(String::new())
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where V: Visitor<'de> {
        self.deserialize_string(visitor)
    }

    forward_value! {
        deserialize_any(), deserialize_bool(),
        deserialize_i8(), deserialize_i16(), deserialize_i32(), deserialize_i64(), deserialize_i128(),
        deserialize_u8(), deserialize_u16(), deserialize_u32(), deserialize_u64(), deserialize_u128(),
        deserialize_f32(), deserialize_f64(), deserialize_char(),
        deserialize_bytes(), deserialize_byte_buf(), deserialize_option(), deserialize_unit(),
        deserialize_unit_struct(name: &'static str),
        deserialize_newtype_struct(name: &'static str),
        deserialize_seq(), deserialize_tuple(len: usize),
        deserialize_tuple_struct(name: &'static str, len: usize), deserialize_map(),
        deserialize_struct(name: &'static str, fields: &'static [&'static str]),
        deserialize_enum(name: &'static str, variants: &'static [&'static str]),
        deserialize_identifier(), deserialize_ignored_any(),
    }
}
