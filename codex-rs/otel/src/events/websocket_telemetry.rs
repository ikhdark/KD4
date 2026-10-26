//! Validate the whole frame while retaining only telemetry's fixed field set.
use serde::de::DeserializeSeed;
use serde::de::MapAccess;
use serde::de::SeqAccess;
use serde::de::Visitor;
use serde_json::Value;

// Most keys are discarded. Borrow unescaped keys while still accepting escaped
// spellings and owning the small set retained in the projection.
#[derive(serde::Deserialize)]
#[serde(transparent)]
struct JsonKey<'a>(#[serde(borrow)] std::borrow::Cow<'a, str>);

#[derive(Clone, Copy)]
enum Projection {
    Root,
    Timing,
    EventType,
    Number,
    Ignore,
}

pub(super) fn parse(text: &str) -> serde_json::Result<Value> {
    let mut decoder = serde_json::Deserializer::from_str(text);
    let value = Projection::Root.deserialize(&mut decoder)?;
    decoder.end()?;
    Ok(value)
}

impl<'de> DeserializeSeed<'de> for Projection {
    type Value = Value;

    fn deserialize<D: serde::Deserializer<'de>>(self, decoder: D) -> Result<Value, D::Error> {
        decoder.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Projection {
    type Value = Value;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut retained = serde_json::Map::new();
        let mut first_key = true;
        while let Some(JsonKey(key)) = map.next_key::<JsonKey<'de>>()? {
            // With serde_json's workspace-enabled arbitrary_precision feature,
            // fractional/large numbers arrive as this synthetic one-entry map.
            // Match Value's first-key handling without retaining payload trees.
            if first_key && key == "$serde_json::private::Number" {
                let encoded = map.next_value::<String>()?;
                let number = encoded
                    .parse::<serde_json::Number>()
                    .map_err(serde::de::Error::custom)?;
                return Ok(if matches!(self, Self::Number) {
                    Value::Number(number)
                } else {
                    Value::Null
                });
            }
            first_key = false;
            let selection = match (self, key.as_ref()) {
                (Self::Root, "type") => Self::EventType,
                (Self::Root, "timing_metrics") => Self::Timing,
                (Self::Timing, key)
                    if [
                        super::session_telemetry::RESPONSES_API_OVERHEAD_FIELD,
                        super::session_telemetry::RESPONSES_API_INFERENCE_FIELD,
                        super::session_telemetry::RESPONSES_API_ENGINE_IAPI_TTFT_FIELD,
                        super::session_telemetry::RESPONSES_API_ENGINE_SERVICE_TTFT_FIELD,
                        super::session_telemetry::RESPONSES_API_ENGINE_IAPI_TBT_FIELD,
                        super::session_telemetry::RESPONSES_API_ENGINE_SERVICE_TBT_FIELD,
                    ]
                    .contains(&key) =>
                {
                    Self::Number
                }
                _ => Self::Ignore,
            };
            let value = map.next_value_seed(selection)?;
            if !matches!(selection, Self::Ignore) {
                // Preserve Value's last-duplicate-key semantics, including wrong types.
                retained.insert(key.into_owned(), value);
            }
        }
        Ok(Value::Object(retained))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Value, A::Error> {
        while sequence.next_element_seed(Self::Ignore)?.is_some() {}
        Ok(Value::Null)
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(if matches!(self, Self::EventType) {
            Value::String(value.into())
        } else {
            Value::Null
        })
    }

    fn visit_bool<E: serde::de::Error>(self, _value: bool) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(if matches!(self, Self::Number) {
            Value::from(value)
        } else {
            Value::Null
        })
    }

    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(if matches!(self, Self::Number) {
            Value::from(value)
        } else {
            Value::Null
        })
    }

    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Value, E> {
        Ok(if matches!(self, Self::Number) {
            Value::from(value)
        } else {
            Value::Null
        })
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
}

