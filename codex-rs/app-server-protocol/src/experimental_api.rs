use std::collections::BTreeMap;
use std::collections::HashMap;

/// Marker trait for protocol types that can signal experimental usage.
pub trait ExperimentalApi {
    /// Returns a short reason identifier when an experimental method or field is
    /// used, or `None` when the value is entirely stable.
    fn experimental_reason(&self) -> Option<&'static str>;
}

/// Describes an experimental field on a specific type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExperimentalField {
    pub type_name: &'static str,
    pub field_name: &'static str,
    /// Stable identifier returned when this field is used.
    /// Convention: `<method>` for method-level gates or `<method>.<field>` for
    /// field-level gates.
    pub reason: &'static str,
}

inventory::collect!(ExperimentalField);

/// Returns all experimental fields registered across the protocol types.
pub fn experimental_fields() -> Vec<&'static ExperimentalField> {
    inventory::iter::<ExperimentalField>.into_iter().collect()
}

/// Constructs a consistent error message for experimental gating.
pub fn experimental_required_message(reason: &str) -> String {
    format!("{reason} requires experimentalApi capability")
}

impl<T: ExperimentalApi> ExperimentalApi for Option<T> {
    fn experimental_reason(&self) -> Option<&'static str> {
        self.as_ref().and_then(ExperimentalApi::experimental_reason)
    }
}

impl<T: ExperimentalApi> ExperimentalApi for Vec<T> {
    fn experimental_reason(&self) -> Option<&'static str> {
        self.iter().find_map(ExperimentalApi::experimental_reason)
    }
}

impl<K, V: ExperimentalApi, S> ExperimentalApi for HashMap<K, V, S> {
    fn experimental_reason(&self) -> Option<&'static str> {
        self.values().find_map(ExperimentalApi::experimental_reason)
    }
}

impl<K: Ord, V: ExperimentalApi> ExperimentalApi for BTreeMap<K, V> {
    fn experimental_reason(&self) -> Option<&'static str> {
        self.values().find_map(ExperimentalApi::experimental_reason)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::ExperimentalApi as ExperimentalApiTrait;
    use codex_experimental_api_macros::ExperimentalApi;
    use pretty_assertions::assert_eq;

    #[allow(dead_code)]
    #[derive(ExperimentalApi)]
    enum EnumVariantShapes {
        #[experimental("enum/unit")]
        Unit,
        #[experimental("enum/tuple")]
        Tuple(u8),
        #[experimental("enum/named")]
        Named {
            value: u8,
        },
        StableTuple(u8),
    }

    #[allow(dead_code)]
    #[derive(ExperimentalApi)]
    struct NestedFieldShape {
        #[experimental(nested)]
        inner: Option<EnumVariantShapes>,
    }

    #[allow(dead_code)]
    #[derive(ExperimentalApi)]
    struct NestedCollectionShape {
        #[experimental(nested)]
        inners: Vec<EnumVariantShapes>,
    }

    #[allow(dead_code)]
    #[derive(ExperimentalApi)]
    struct NestedMapShape {
        #[experimental(nested)]
        inners: HashMap<String, EnumVariantShapes>,
    }

    #[allow(dead_code)]
    #[derive(ExperimentalApi)]
    struct ExperimentalFieldShape {
        #[experimental("field/optionalCollection")]
        optional_collection: Option<Vec<EnumVariantShapes>>,
    }

    #[test]
    fn derive_supports_all_enum_variant_shapes() {
        assert_eq!(
            ExperimentalApiTrait::experimental_reason(&EnumVariantShapes::Unit),
            Some("enum/unit")
        );
        assert_eq!(
            ExperimentalApiTrait::experimental_reason(&EnumVariantShapes::Tuple(1)),
            Some("enum/tuple")
        );
        assert_eq!(
            ExperimentalApiTrait::experimental_reason(&EnumVariantShapes::Named { value: 1 }),
            Some("enum/named")
        );
        assert_eq!(
            ExperimentalApiTrait::experimental_reason(&EnumVariantShapes::StableTuple(1)),
            None
        );
    }

    #[test]
    fn derive_supports_nested_experimental_fields() {
        assert_eq!(
            ExperimentalApiTrait::experimental_reason(&NestedFieldShape {
                inner: Some(EnumVariantShapes::Named { value: 1 }),
            }),
            Some("enum/named")
        );
        assert_eq!(
            ExperimentalApiTrait::experimental_reason(&NestedFieldShape { inner: None }),
            None
        );
    }

    #[test]
    fn derive_supports_nested_collections() {
        assert_eq!(
            ExperimentalApiTrait::experimental_reason(&NestedCollectionShape {
                inners: vec![
                    EnumVariantShapes::StableTuple(1),
                    EnumVariantShapes::Tuple(2)
                ],
            }),
            Some("enum/tuple")
        );
        assert_eq!(
            ExperimentalApiTrait::experimental_reason(&NestedCollectionShape {
                inners: Vec::new()
            }),
            None
        );
    }

    #[test]
    fn derive_supports_nested_maps() {
        assert_eq!(
            ExperimentalApiTrait::experimental_reason(&NestedMapShape {
                inners: HashMap::from([(
                    "default".to_string(),
                    EnumVariantShapes::Named { value: 1 },
                )]),
            }),
            Some("enum/named")
        );
        assert_eq!(
            ExperimentalApiTrait::experimental_reason(&NestedMapShape {
                inners: HashMap::new(),
            }),
            None
        );
    }

    #[test]
    fn derive_marks_optional_experimental_fields_when_some() {
        assert_eq!(
            ExperimentalApiTrait::experimental_reason(&ExperimentalFieldShape {
                optional_collection: Some(Vec::new()),
            }),
            Some("field/optionalCollection")
        );
        assert_eq!(
            ExperimentalApiTrait::experimental_reason(&ExperimentalFieldShape {
                optional_collection: None,
            }),
            None
        );
    }

    #[test]
    fn derive_registers_actual_serialized_field_names() {
        #[derive(serde::Serialize, ExperimentalApi)]
        #[serde(rename_all = "camelCase")]
        struct RenamedFields {
            #[serde(rename = "wire_flag")]
            #[experimental("renamed/flag")]
            preview_mode: bool,
            #[experimental("renamed/raw")]
            r#type: bool,
            #[experimental("renamed/camel")]
            another_flag: bool,
            #[experimental("renamed/leading")]
            _leading_flag: bool,
        }
        #[derive(serde::Serialize, ExperimentalApi)]
        struct DefaultFields {
            #[experimental("default/flag")]
            preview_mode: bool,
        }
        #[derive(serde::Serialize, ExperimentalApi)]
        #[serde(rename_all = "snake_case")]
        struct SnakeFields {
            #[experimental("snake/flag")]
            preview_mode: bool,
        }

        let renamed = RenamedFields {
            preview_mode: true,
            r#type: false,
            another_flag: false,
            _leading_flag: false,
        };
        assert_eq!(renamed.experimental_reason(), Some("renamed/flag"));
        assert_eq!(
            RenamedFields {
                preview_mode: false,
                r#type: false,
                another_flag: false,
                _leading_flag: false,
            }
            .experimental_reason(),
            None
        );
        for (value, fields, names) in [
            (
                serde_json::to_value(renamed).unwrap(),
                RenamedFields::EXPERIMENTAL_FIELDS,
                vec!["wire_flag", "type", "anotherFlag", "leadingFlag"],
            ),
            (
                serde_json::to_value(DefaultFields { preview_mode: true }).unwrap(),
                DefaultFields::EXPERIMENTAL_FIELDS,
                vec!["preview_mode"],
            ),
            (
                serde_json::to_value(SnakeFields { preview_mode: true }).unwrap(),
                SnakeFields::EXPERIMENTAL_FIELDS,
                vec!["preview_mode"],
            ),
        ] {
            assert_eq!(
                fields
                    .iter()
                    .map(|field| field.field_name)
                    .collect::<Vec<_>>(),
                names
            );
            for field in fields {
                assert!(
                    value.get(field.field_name).is_some(),
                    "registered field is absent from serialized value"
                );
                assert!(
                    super::experimental_fields().contains(&field),
                    "field must reach the schema registry"
                );
            }
        }
    }

    #[test]
    fn derive_preserves_generics_and_where_clauses() {
        #[derive(ExperimentalApi)]
        struct GenericFields<T>
        where
            T: ExperimentalApiTrait,
        {
            #[experimental(nested)]
            inner: T,
        }
        #[derive(ExperimentalApi)]
        struct GenericTuple<T>(#[experimental("generic/value")] Option<T>);
        #[derive(ExperimentalApi)]
        enum GenericEnum<T>
        where
            T: Copy,
        {
            #[experimental("generic/variant")]
            Preview(T),
            Stable,
        }
        assert_eq!(
            GenericFields {
                inner: Some(EnumVariantShapes::Unit)
            }
            .experimental_reason(),
            Some("enum/unit")
        );
        assert_eq!(
            GenericFields::<Option<EnumVariantShapes>> { inner: None }.experimental_reason(),
            None
        );
        assert_eq!(
            GenericFields::<Option<EnumVariantShapes>>::EXPERIMENTAL_FIELDS,
            &[]
        );
        assert_eq!(
            GenericTuple(Some(42)).experimental_reason(),
            Some("generic/value")
        );
        assert_eq!(GenericTuple::<u8>(None).experimental_reason(), None);
        assert_eq!(GenericTuple::<u8>::EXPERIMENTAL_FIELDS[0].field_name, "0");
        assert_eq!(
            GenericEnum::Preview(42).experimental_reason(),
            Some("generic/variant")
        );
        assert_eq!(GenericEnum::<u8>::Stable.experimental_reason(), None);
    }
}
