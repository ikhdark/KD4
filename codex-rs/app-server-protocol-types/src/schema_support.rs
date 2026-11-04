pub use crate::protocol::common::EXPERIMENTAL_CLIENT_METHOD_PARAM_TYPES;
pub use crate::protocol::common::EXPERIMENTAL_CLIENT_METHOD_RESPONSE_TYPES;
pub use crate::protocol::common::EXPERIMENTAL_CLIENT_METHODS;
pub use crate::protocol::common::EXPERIMENTAL_SERVER_METHOD_PARAM_TYPES;
pub use crate::protocol::common::EXPERIMENTAL_SERVER_METHOD_RESPONSE_TYPES;
pub use crate::protocol::common::EXPERIMENTAL_SERVER_METHODS;
pub use crate::protocol::common::SERVER_NOTIFICATION_METADATA;
pub use crate::protocol::common::ServerNotificationMetadata;
pub use crate::protocol::common::export_client_notification_schemas;
pub use crate::protocol::common::export_client_param_schemas;
pub use crate::protocol::common::export_client_response_schemas;
pub use crate::protocol::common::export_server_notification_schemas;
pub use crate::protocol::common::export_server_param_schemas;
pub use crate::protocol::common::export_server_response_schemas;

pub struct RawSchemaEntry {
    pub name: &'static str,
    pub schema: serde_json::Value,
}

/// Parameterless methods accept omission/null and the legacy empty-object form.
pub fn no_params_schema(_: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
    let mut object = schemars::schema::SchemaObject::default();
    object.instance_type = Some(schemars::schema::InstanceType::Object.into());
    object.object = Some(Box::new(schemars::schema::ObjectValidation {
        max_properties: Some(0), ..Default::default()
    }));
    schemars::schema::SchemaObject {
        subschemas: Some(Box::new(schemars::schema::SubschemaValidation {
            any_of: Some(vec![
                schemars::schema::SchemaObject {
                    instance_type: Some(schemars::schema::InstanceType::Null.into()), ..Default::default()
                }.into(),
                object.into(),
            ]), ..Default::default()
        })),
        ..Default::default()
    }.into()
}

pub fn generate_raw_json_schema<T: schemars::JsonSchema + 'static>(
    name: &'static str,
) -> anyhow::Result<RawSchemaEntry> {
    if std::any::TypeId::of::<T>() == std::any::TypeId::of::<Option<()>>() {
        return Ok(RawSchemaEntry {
            name,
            schema: serde_json::to_value(no_params_schema(&mut schemars::r#gen::SchemaGenerator::default()))?,
        });
    }
    let schema = schemars::schema_for!(T);
    Ok(RawSchemaEntry {
        name,
        schema: serde_json::to_value(schema)?,
    })
}
