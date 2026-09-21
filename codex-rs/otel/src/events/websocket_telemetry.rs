//! Validate the whole frame while retaining only telemetry's fixed field set.
use serde::de::DeserializeSeed;
use serde::de::MapAccess;
use serde::de::SeqAccess;
use serde::de::Visitor;
use serde_json::Value;

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
        while let Some(key) = map.next_key::<String>()? {
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
            let selection = match (self, key.as_str()) {
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
                retained.insert(key, value);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_observation_semantics_without_retaining_payloads() {
        for text in [
            r#"{"type":"response.failed","response":{"large":[1,2,3]}}"#,
            r#"{"type":"first","type":42}"#,
            r#"{"type":"response.\u0066ailed","timing_metrics":{"inference_time_ms":12}}"#,
            r#"{"timing_metrics":{"inference_time_ms":12},"timing_metrics":[]}"#,
            r#"{"type":null,"ignored":1e400}"#,
            r#"{"type":"response.failed"} trailing"#,
            "[]",
            "null",
            "42",
            "\"text\"",
            "{",
            "true",
        ] {
            let original = serde_json::from_str::<Value>(text);
            let projected = parse(text);
            assert_eq!(original.is_ok(), projected.is_ok(), "{text}");
            if let (Ok(original), Ok(projected)) = (original, projected) {
                assert_eq!(
                    original.get("type").and_then(Value::as_str),
                    projected.get("type").and_then(Value::as_str),
                    "{text}"
                );
                assert!(projected.get("response").is_none());
            }
        }
    }

    #[test]
    fn keeps_numeric_timing_fields_and_discards_wrong_types_and_unrelated_data() {
        let text = r#"{"type":"responsesapi.websocket_timing","timing_metrics":{
            "responses_duration_excl_engine_and_client_tool_time_ms":1,
            "engine_service_total_ms":2.5,"engine_iapi_ttft_total_ms":-3,
            "engine_service_ttft_total_ms":"unused text",
            "engine_iapi_tbt_across_engine_calls_ms":4,
            "engine_service_tbt_across_engine_calls_ms":5,
            "ignored":{"payload":[1,2,3]}}}"#;
        let value = parse(text).unwrap();
        assert_eq!(value["type"], "responsesapi.websocket_timing");
        let timing = &value["timing_metrics"];
        assert_eq!(
            timing[super::super::session_telemetry::RESPONSES_API_OVERHEAD_FIELD],
            1
        );
        assert_eq!(
            timing[super::super::session_telemetry::RESPONSES_API_INFERENCE_FIELD],
            2.5
        );
        assert_eq!(
            timing[super::super::session_telemetry::RESPONSES_API_ENGINE_IAPI_TTFT_FIELD],
            -3
        );
        assert!(
            timing[super::super::session_telemetry::RESPONSES_API_ENGINE_SERVICE_TTFT_FIELD]
                .is_null()
        );
        assert_eq!(
            timing[super::super::session_telemetry::RESPONSES_API_ENGINE_IAPI_TBT_FIELD],
            4
        );
        assert_eq!(
            timing[super::super::session_telemetry::RESPONSES_API_ENGINE_SERVICE_TBT_FIELD],
            5
        );
        assert!(timing.get("ignored").is_none());
    }
}
