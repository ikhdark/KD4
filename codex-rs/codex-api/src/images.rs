use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ImageGenerationRequest {
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<ImageBackground>,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<ImageQuality>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ImageEditRequest {
    pub images: Vec<ImageUrl>,
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<ImageBackground>,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<ImageQuality>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageUrl {
    pub image_url: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImageBackground {
    Transparent,
    Opaque,
    Auto,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImageQuality {
    Low,
    Medium,
    High,
    Auto,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ImageResponse {
    pub created: u64,
    pub data: Vec<ImageData>,
    #[serde(default)]
    pub background: Option<ImageBackground>,
    #[serde(default)]
    pub quality: Option<ImageQuality>,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default, deserialize_with = "deserialize_image_usage")]
    pub usage: Option<codex_protocol::items::ImageGenerationUsage>,
}

fn deserialize_image_usage<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<codex_protocol::items::ImageGenerationUsage>, D::Error> {
    use codex_protocol::items::ImageGenerationUsage;
    use codex_protocol::items::ImageTokenDetails;
    let value = serde_json::Value::deserialize(deserializer)?;
    if !value.is_object() {
        return Ok(None);
    }
    let count = |key: &str| value.get(key).and_then(serde_json::Value::as_u64);
    let details = |key: &str| {
        value
            .get(key)
            .filter(|v| v.is_object())
            .map(|v| ImageTokenDetails {
                image_tokens: v.get("image_tokens").and_then(serde_json::Value::as_u64),
                text_tokens: v.get("text_tokens").and_then(serde_json::Value::as_u64),
            })
    };
    Ok(Some(ImageGenerationUsage {
        model: None,
        reported: true,
        input_tokens: count("input_tokens"),
        output_tokens: count("output_tokens"),
        total_tokens: count("total_tokens"),
        input_tokens_details: details("input_tokens_details"),
        output_tokens_details: details("output_tokens_details"),
    }))
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ImageData {
    pub b64_json: String,
}

