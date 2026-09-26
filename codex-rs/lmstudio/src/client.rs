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
    ) -> io::Result<(Self, io::Result<Vec<String>>)> {
        let client = Self::from_provider(config)?;
        let response = client.check_server().await?;
        let models = Self::models_from_response(response).await;
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
        let resp = self.client.get(&url).send().await.map_err(|err| {
            io::Error::other(format!(
                "{LMSTUDIO_CONNECTION_ERROR} Request to {url} failed: {}",
                error_with_sources(&err)
            ))
        })?;
        if resp.status().is_success() {
            Ok(resp)
        } else {
            Err(io::Error::other(format!(
                "Server returned error: {} {LMSTUDIO_CONNECTION_ERROR}",
                resp.status()
            )))
        }
    }

    // Load a model by sending an empty request with max_tokens 1
    pub async fn load_model(&self, model: &str) -> io::Result<()> {
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));

        let request_body = serde_json::json!({
            "model": model,
            "input": "",
            "max_output_tokens": 1
        });

        let response = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| io::Error::other(format!("Request failed: {e}")))?;

        if response.status().is_success() {
            tracing::info!("Successfully loaded model '{model}'");
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "Failed to load model: {}",
                response.status()
            )))
        }
    }

    // Return the list of models available on the LM Studio server.
    pub async fn fetch_models(&self) -> io::Result<Vec<String>> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let response = self
            .client
            .get(&url)
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
        let json: serde_json::Value = response.json().await.map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("JSON parse error: {e}"))
        })?;
        let models = json["data"]
            .as_array()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "No 'data' array in response")
            })?
            .iter()
            .filter_map(|model| model["id"].as_str())
            .map(std::string::ToString::to_string)
            .collect();
        Ok(models)
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

    /// Low-level constructor given a raw host root, e.g. "http://localhost:1234".
    #[cfg(test)]
    fn from_host_root(host_root: impl Into<String>) -> io::Result<Self> {
        let client = HttpClientBuilder::new()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build_direct()
            .map_err(io::Error::other)?;
        Ok(Self {
            client,
            base_url: host_root.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use std::path::Path;

    #[tokio::test]
    async fn test_fetch_models_happy_path() {
        if std::env::var(codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
            tracing::info!(
                "{} is set; skipping test_fetch_models_happy_path",
                codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR
            );
            return;
        }

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_raw(
                    serde_json::json!({
                        "data": [
                            {"id": "openai/gpt-oss-20b"},
                        ]
                    })
                    .to_string(),
                    "application/json",
                ),
            )
            .mount(&server)
            .await;

        let client = LMStudioClient::from_host_root(server.uri()).expect("shared HTTP client");
        let models = client.fetch_models().await.expect("fetch models");
        assert!(models.contains(&"openai/gpt-oss-20b".to_string()));
    }

    #[tokio::test]
    async fn test_fetch_models_no_data_array() {
        if std::env::var(codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
            tracing::info!(
                "{} is set; skipping test_fetch_models_no_data_array",
                codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR
            );
            return;
        }

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_raw(serde_json::json!({}).to_string(), "application/json"),
            )
            .mount(&server)
            .await;

        let client = LMStudioClient::from_host_root(server.uri()).expect("shared HTTP client");
        let result = client.fetch_models().await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No 'data' array in response")
        );
    }

    #[tokio::test]
    async fn test_fetch_models_server_error() {
        if std::env::var(codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
            tracing::info!(
                "{} is set; skipping test_fetch_models_server_error",
                codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR
            );
            return;
        }

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = LMStudioClient::from_host_root(server.uri()).expect("shared HTTP client");
        let result = client.fetch_models().await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to fetch models: 500")
        );
    }

    #[tokio::test]
    async fn test_check_server_happy_path() {
        if std::env::var(codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
            tracing::info!(
                "{} is set; skipping test_check_server_happy_path",
                codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR
            );
            return;
        }

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = LMStudioClient::from_host_root(server.uri()).expect("shared HTTP client");
        let response = client
            .check_server()
            .await
            .expect("server check should pass");
        assert_eq!(response.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn test_check_server_error() {
        if std::env::var(codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
            tracing::info!(
                "{} is set; skipping test_check_server_error",
                codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR
            );
            return;
        }

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = LMStudioClient::from_host_root(server.uri()).expect("shared HTTP client");
        let result = client.check_server().await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Server returned error: 404")
        );
    }

    #[tokio::test]
    async fn test_check_server_unreachable_reports_endpoint_and_cause() {
        if std::env::var(codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
            tracing::info!(
                "{} is set; skipping test_check_server_unreachable_reports_endpoint_and_cause",
                codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR
            );
            return;
        }

        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let client = LMStudioClient::from_host_root(format!("http://127.0.0.1:{port}"))
            .expect("shared HTTP client");
        let message = client.check_server().await.unwrap_err().to_string();
        assert!(message.starts_with(LMSTUDIO_CONNECTION_ERROR), "{message}");
        assert!(
            message.contains(&format!(
                "Request to http://127.0.0.1:{port}/models failed: "
            )),
            "{message}"
        );
        // The refused connection is only visible through the error's source chain.
        assert!(message.contains("(os error"), "{message}");
    }

    #[tokio::test]
    async fn test_load_model_posts_through_shared_http_client() {
        if std::env::var(codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
            tracing::info!(
                "{} is set; skipping test_load_model_posts_through_shared_http_client",
                codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR
            );
            return;
        }

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/responses"))
            .and(wiremock::matchers::header(
                "content-type",
                "application/json",
            ))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "model": "openai/gpt-oss-20b",
                "input": "",
                "max_output_tokens": 1
            })))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = LMStudioClient::from_host_root(server.uri()).expect("shared HTTP client");
        client
            .load_model("openai/gpt-oss-20b")
            .await
            .expect("load request should succeed");
    }

    #[tokio::test]
    async fn test_find_lms() {
        if std::env::var_os("CODEX_LMS_DISCOVERY_CHILD").is_some() {
            let error = missing_model_readiness_error().await;
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
            assert!(error.to_string().contains("LM Studio not found"));
        } else {
            run_discovery_child("client::tests::test_find_lms", false).await;
        }
    }

    #[tokio::test]
    async fn test_find_lms_with_mock_home() {
        if std::env::var_os("CODEX_LMS_DISCOVERY_CHILD").is_some() {
            let error = missing_model_readiness_error().await;
            assert!(
                error
                    .to_string()
                    .starts_with("Model download failed with exit code:"),
                "the discovered executable must run and its failure must propagate: {error}"
            );
        } else {
            run_discovery_child("client::tests::test_find_lms_with_mock_home", true).await;
        }
    }

    #[tokio::test]
    async fn test_find_lms_without_userprofile() {
        if std::env::var_os("CODEX_LMS_DISCOVERY_CHILD").is_some() {
            let error = missing_model_readiness_error().await;
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
            assert!(error.to_string().contains("USERPROFILE is unavailable"));
        } else {
            run_discovery_child_with_home(
                "client::tests::test_find_lms_without_userprofile",
                false,
                false,
            )
            .await;
        }
    }

    async fn missing_model_readiness_error() -> io::Error {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/models"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "data": [] })),
            )
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/responses"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let config_home = tempfile::tempdir().unwrap();
        let mut config = codex_core::config::ConfigBuilder::default()
            .codex_home(config_home.path().to_path_buf())
            .build()
            .await
            .unwrap();
        config.model = Some("test-model".to_string());
        config
            .model_providers
            .get_mut(LMSTUDIO_OSS_PROVIDER_ID)
            .unwrap()
            .base_url = Some(server.uri());
        let error = crate::ensure_oss_ready(&config).await.unwrap_err();
        server.verify().await;
        error
    }

    async fn run_discovery_child(test: &str, install_failing_executable: bool) {
        run_discovery_child_with_home(test, install_failing_executable, true).await;
    }

    async fn run_discovery_child_with_home(
        test: &str,
        install_failing_executable: bool,
        include_home: bool,
    ) {
        let home = tempfile::tempdir().unwrap();
        if install_failing_executable {
            let bin = home.path().join(".lmstudio/bin");
            std::fs::create_dir_all(&bin).unwrap();
            // WHERE is a real Windows executable. The unsupported --yes option
            // makes it fail without downloading a model or contacting a service.
            let windows = std::env::var_os("SystemRoot").expect("Windows system directory");
            std::fs::copy(
                Path::new(&windows).join("System32/where.exe"),
                bin.join("lms.exe"),
            )
            .unwrap();
        }
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test, "--nocapture"])
            .env("CODEX_LMS_DISCOVERY_CHILD", "1")
            .env("PATH", "");
        if include_home {
            command.env("USERPROFILE", home.path());
        } else {
            command.env_remove("USERPROFILE");
        }
        let output = command.output().await.unwrap();
        assert!(
            output.status.success(),
            "discovery subprocess failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }

    #[test]
    fn test_from_host_root() {
        let client =
            LMStudioClient::from_host_root("http://localhost:1234").expect("shared HTTP client");
        assert_eq!(client.base_url, "http://localhost:1234");

        let client = LMStudioClient::from_host_root("https://example.com:8080/api")
            .expect("shared HTTP client");
        assert_eq!(client.base_url, "https://example.com:8080/api");
    }
}
