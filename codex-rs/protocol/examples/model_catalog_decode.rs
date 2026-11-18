//! Decode a saved provider catalog without network access or model execution.
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelsResponse;
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("provide a saved catalog path")?;
    let bytes = std::fs::read(path)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let models = value
        .get("models")
        .and_then(|v| v.as_array())
        .ok_or("missing models")?;
    let observations = models
        .iter()
        .enumerate()
        .map(
            |(index, model)| match serde_json::from_value::<ModelInfo>(model.clone()) {
                Ok(decoded) => json!({"index": index, "slug": decoded.slug, "decoded": true,
                "input_modalities": decoded.input_modalities,
                "validation_error": decoded.validate_capabilities().err()}),
                Err(error) => json!({"index": index, "slug": model.get("slug"), "decoded": false,
                "error": error.to_string()}),
            },
        )
        .collect::<Vec<_>>();
    let decoded = serde_json::from_slice::<ModelsResponse>(&bytes);
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "bytes": bytes.len(), "catalog_decoded": decoded.is_ok(),
            "catalog_error": decoded.err().map(|error| error.to_string()),
            "models": observations,
        }))?
    );
    Ok(())
}
