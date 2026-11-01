use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS, Default)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AttestationGenerateParams {}

#[derive(Serialize, Deserialize, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AttestationGenerateResponse {
    /// Opaque client attestation token.
    pub token: String,
}

impl std::fmt::Debug for AttestationGenerateResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttestationGenerateResponse")
            .field("token", &"[REDACTED]")
            .finish()
    }
}
