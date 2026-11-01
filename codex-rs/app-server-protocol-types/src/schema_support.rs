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

pub fn generate_raw_json_schema<T: schemars::JsonSchema>(
    name: &'static str,
) -> anyhow::Result<RawSchemaEntry> {
    let schema = schemars::schema_for!(T);
    Ok(RawSchemaEntry {
        name,
        schema: serde_json::to_value(schema)?,
    })
}
