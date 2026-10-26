use codex_core::config::Config;
use codex_http_client::HttpClient;
use codex_http_client::HttpClientBuilder;
use codex_http_client::HttpResponse;
use codex_model_provider_info::LMSTUDIO_OSS_PROVIDER_ID;
use std::io;
use std::path::PathBuf;

#[derive(Clone)]
pub struct LMStudioClient {
    client: HttpClient,
    base_url: String,
}

const LMSTUDIO_CONNECTION_ERROR: &str = "LM Studio is not responding. Install from https://lmstudio.ai/download and run 'lms server start'.";

// A server may accept a connection without ever answering a metadata request.
// Downloads and model warm-up can legitimately take longer and use separate paths.
const SETUP_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
// Keep best-effort background warm-up bounded, including response-body completion.
const MODEL_WARMUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

fn error_with_sources(err: &dyn std::error::Error) -> String {
    let mut message = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

impl LMStudioClient {
    pub async fn try_from_provider(config: &Config) -> std::io::Result<Self> {
        let client = Self::from_provider(config)?;
        client.check_server().await?;
        Ok(client)
    }

    pub(crate) async fn try_from_provider_with_models(
        config: &Config,
    ) -> io::Result<(Self, Vec<String>)> {
        let client = Self::from_provider(config)?;
        let response = client.check_server().await?;
        let models = Self::models_from_response(response).await?;
        Ok((client, models))
    }

    fn from_provider(config: &Config) -> io::Result<Self> {
        let provider = config
            .model_providers
            .get(LMSTUDIO_OSS_PROVIDER_ID)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Built-in provider {LMSTUDIO_OSS_PROVIDER_ID} not found",),
                )
            })?;
        let base_url = provider.base_url.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "oss provider must have a base_url",
            )
        })?;

        let client = HttpClientBuilder::new()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build_with_transport_default_proxy()
            .map_err(io::Error::other)?;

        let client = LMStudioClient {
            client,
            base_url: base_url.to_string(),
        };
        Ok(client)
    }

    async fn check_server(&self) -> io::Result<HttpResponse> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        // Setup can run before logging is initialized, so the returned error is
        // the only place the transport cause (refused, timeout, proxy) surfaces.
        let resp = self
            .client
            .get(&url)
            .timeout(SETUP_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|err| {
                io::Error::other(format!(
                    "{LMSTUDIO_CONNECTION_ERROR} Request to {url} failed: {}",
                    error_with_sources(&err)
                ))
            })?;
        if resp.status().is_success() {
            Ok(resp)
        } else {
            Err(io::Error::other(format!(
                "LM Studio answered /models with HTTP {}. Check the configured endpoint and server error; authentication or route configuration may need attention.",
                resp.status()
            )))
        }
    }

    /// Complete a minimal warm-up response, not just its HTTP status headers.
    /// This does not guarantee that LM Studio keeps the model resident afterward.
    pub async fn load_model(&self, model: &str) -> io::Result<()> {
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));

        let request_body = serde_json::json!({
            "model": model,
            "input": "",
            "max_output_tokens": 1,
            "stream": false
        });

        let response = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&request_body)
            .timeout(MODEL_WARMUP_TIMEOUT)
            .send()
            .await
            .map_err(|e| io::Error::other(format!("Request failed: {e}")))?;

        if !response.status().is_success() {
            return Err(io::Error::other(format!(
                "Failed to load model: {}",
                response.status()
            )));
        }
        let response: serde_json::Value = response.json().await.map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Failed to read LM Studio warm-up completion: {error}"),
            )
        })?;
        let status = response.get("status").and_then(serde_json::Value::as_str);
        let completed = status == Some("completed")
            || (status == Some("incomplete")
                && response
                    .pointer("/incomplete_details/reason")
                    .and_then(serde_json::Value::as_str)
                    == Some("max_output_tokens"));
        if !completed || response.get("error").is_some_and(|error| !error.is_null()) {
            return Err(io::Error::other(format!(
                "LM Studio warm-up did not complete successfully (status: {})",
                status.unwrap_or("missing")
            )));
        }
        tracing::info!("LM Studio warm-up response completed for model '{model}'");
        Ok(())
    }

    // Return the list of models available on the LM Studio server.
    pub async fn fetch_models(&self) -> io::Result<Vec<String>> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let response = self
            .client
            .get(&url)
            .timeout(SETUP_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| io::Error::other(format!("Request failed: {e}")))?;

        if response.status().is_success() {
            Self::models_from_response(response).await
        } else {
            Err(io::Error::other(format!(
                "Failed to fetch models: {}",
                response.status()
            )))
        }
    }

    async fn models_from_response(response: HttpResponse) -> io::Result<Vec<String>> {
        let value = response
            .json()
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        codex_model_provider_info::local::parse_model_ids(
            codex_model_provider_info::ProviderKind::LmStudio,
            &value,
        )
    }

    // Find lms on PATH, falling back to LM Studio's per-user install location.
    fn find_lms() -> std::io::Result<PathBuf> {
        if let Ok(path) = which::which("lms") {
            return Ok(path);
        }

        let Some(home) = std::env::var_os("USERPROFILE").filter(|home| !home.is_empty()) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "LM Studio not found: USERPROFILE is unavailable for the home-directory fallback.",
            ));
        };

        let fallback_path = PathBuf::from(home).join(".lmstudio/bin/lms.exe");
        if fallback_path.is_file() {
            Ok(fallback_path)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "LM Studio not found. Please install LM Studio from https://lmstudio.ai/",
            ))
        }
    }

    pub async fn download_model(&self, model: &str) -> std::io::Result<()> {
        let lms = tokio::task::spawn_blocking(Self::find_lms)
            .await
            .map_err(io::Error::other)??;
        eprintln!("Downloading model: {model}");

        let status = tokio::process::Command::new(&lms)
            .args(["get", "--yes", model])
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .status()
            .await
            .map_err(|e| {
                std::io::Error::other(format!(
                    "Failed to execute '{} get --yes {model}': {e}",
                    lms.display()
                ))
            })?;

        if !status.success() {
            return Err(std::io::Error::other(format!(
                "Model download failed with exit code: {}",
                status.code().unwrap_or(-1)
            )));
        }

        tracing::info!("Successfully downloaded model '{model}'");
        Ok(())
    }

}

