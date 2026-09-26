use futures::StreamExt;
use futures::stream::BoxStream;
use semver::Version;
use serde_json::Value as JsonValue;
use std::io;
use std::time::Duration;

use crate::line_buffer::LineBuffer;
use crate::parser::pull_events_from_value;
use crate::pull::CliProgressReporter;
use crate::pull::PullEvent;
use crate::url::base_url_to_host_root;
use crate::url::is_openai_compatible_base_url;
use codex_core::config::Config;
use codex_http_client::HttpClient;
use codex_http_client::HttpClientBuilder;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::OLLAMA_OSS_PROVIDER_ID;
#[cfg(test)]
use codex_model_provider_info::WireApi;
#[cfg(test)]
use codex_model_provider_info::create_oss_provider_with_base_url;

const OLLAMA_CONNECTION_ERROR: &str = "No running Ollama server detected. Start it with: `ollama serve` (after installing). Install instructions: https://github.com/ollama/ollama?tab=readme-ov-file#ollama";
/// Bounds each metadata request made during setup, so a server that accepts the connection but
/// never answers cannot hang startup. Model pulls stream for as long as the download takes.
const SETUP_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Pull progress lines are a few hundred bytes; a longer unterminated line is not Ollama output.
const MAX_PULL_LINE_BYTES: usize = 1024 * 1024;

/// Client for interacting with a local Ollama instance.
pub struct OllamaClient {
    client: HttpClient,
    host_root: String,
    uses_openai_compat: bool,
    setup_timeout: Duration,
}

impl OllamaClient {
    /// Construct a client for the built‑in open‑source ("oss") model provider
    /// and verify that a local Ollama server is reachable. If no server is
    /// detected, returns an error with helpful installation/run instructions.
    pub async fn try_from_oss_provider(config: &Config) -> io::Result<Self> {
        // Note that we must look up the provider from the Config to ensure that
        // any overrides the user has in their config.toml are taken into
        // account.
        let provider = config
            .model_providers
            .get(OLLAMA_OSS_PROVIDER_ID)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Built-in provider {OLLAMA_OSS_PROVIDER_ID} not found",),
                )
            })?;

        Self::try_from_provider(provider).await
    }

    #[cfg(test)]
    async fn try_from_provider_with_base_url(base_url: &str) -> io::Result<Self> {
        let provider = create_oss_provider_with_base_url(base_url, WireApi::Responses);
        Self::try_from_provider(&provider).await
    }

    /// Build a client from a provider definition and verify the server is reachable.
    pub(crate) async fn try_from_provider(provider: &ModelProviderInfo) -> io::Result<Self> {
        let base_url = provider.base_url.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Ollama provider must have a base_url",
            )
        })?;
        let uses_openai_compat = is_openai_compatible_base_url(base_url);
        let host_root = base_url_to_host_root(base_url);
        let client = HttpClientBuilder::new()
            .connect_timeout(Duration::from_secs(5))
            .build_with_transport_default_proxy()
            .map_err(io::Error::other)?;
        let client = Self {
            client,
            host_root,
            uses_openai_compat,
            setup_timeout: SETUP_REQUEST_TIMEOUT,
        };
        client.probe_server().await?;
        Ok(client)
    }

    /// Probe whether the server is reachable by hitting the appropriate health endpoint.
    async fn probe_server(&self) -> io::Result<()> {
        let url = if self.uses_openai_compat {
            format!("{}/v1/models", self.host_root.trim_end_matches('/'))
        } else {
            format!("{}/api/tags", self.host_root.trim_end_matches('/'))
        };
        let resp = self
            .client
            .get(url)
            .timeout(self.setup_timeout)
            .send()
            .await
            .map_err(|err| {
                tracing::warn!("Failed to connect to Ollama server: {err:?}");
                // A connected but silent server needs a different fix than a missing one.
                if err.is_timeout() && !err.is_connect() {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "Ollama server at {} accepted the connection but did not respond within {}s",
                            self.host_root,
                            self.setup_timeout.as_secs_f32()
                        ),
                    )
                } else {
                    io::Error::other(OLLAMA_CONNECTION_ERROR)
                }
            })?;
        if resp.status().is_success() {
            Ok(())
        } else {
            tracing::warn!(
                "Failed to probe server at {}: HTTP {}",
                self.host_root,
                resp.status()
            );
            Err(io::Error::other(OLLAMA_CONNECTION_ERROR))
        }
    }

    /// Return the list of model names known to the local Ollama instance.
    ///
    /// A rejected listing is an error rather than an empty list, so callers cannot mistake it
    /// for a server without the model.
    pub async fn fetch_models(&self) -> io::Result<Vec<String>> {
        let tags_url = format!("{}/api/tags", self.host_root.trim_end_matches('/'));
        let resp = self
            .client
            .get(tags_url)
            .timeout(self.setup_timeout)
            .send()
            .await
            .map_err(io::Error::other)?;
        if !resp.status().is_success() {
            return Err(io::Error::other(format!(
                "failed to list models: HTTP {}",
                resp.status()
            )));
        }
        let val = resp.json::<JsonValue>().await.map_err(io::Error::other)?;
        let names = val
            .get("models")
            .and_then(|m| m.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.get("name").and_then(|n| n.as_str()))
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(names)
    }

    /// Query the server for its version string, returning `None` when unavailable.
    pub async fn fetch_version(&self) -> io::Result<Option<Version>> {
        let version_url = format!("{}/api/version", self.host_root.trim_end_matches('/'));
        let resp = self
            .client
            .get(version_url)
            .timeout(self.setup_timeout)
            .send()
            .await
            .map_err(io::Error::other)?;
        if !resp.status().is_success() {
            return Ok(None);
        }
        // An unreadable body is as inconclusive as an unparsable version string.
        let val = match resp.json::<JsonValue>().await {
            Ok(val) => val,
            Err(err) => {
                tracing::warn!("Failed to read Ollama version response: {err}");
                return Ok(None);
            }
        };
        let Some(version_str) = val.get("version").and_then(|v| v.as_str()).map(str::trim) else {
            return Ok(None);
        };
        let normalized = version_str.trim_start_matches('v');
        match Version::parse(normalized) {
            Ok(version) => Ok(Some(version)),
            Err(err) => {
                tracing::warn!("Failed to parse Ollama version `{version_str}`: {err}");
                Ok(None)
            }
        }
    }

    /// Start a model pull and emit streaming events. The returned stream ends after
    /// a Success or Error event, or when the server closes the connection.
    pub async fn pull_model_stream(
        &self,
        model: &str,
    ) -> io::Result<BoxStream<'static, PullEvent>> {
        let url = format!("{}/api/pull", self.host_root.trim_end_matches('/'));
        let resp = self
            .client
            .post(url)
            .json(&serde_json::json!({"model": model, "stream": true}))
            .send()
            .await
            .map_err(io::Error::other)?;
        if !resp.status().is_success() {
            return Err(io::Error::other(format!(
                "failed to start pull: HTTP {}",
                resp.status()
            )));
        }

        let mut stream = resp.bytes_stream();
        let mut buf = LineBuffer::default();

        // Using an async stream adaptor backed by unfold-like manual loop.
        let s = async_stream::stream! {
            let mut finished = false;
            while !finished {
                match stream.next().await {
                    Some(Ok(bytes)) => buf.extend_from_slice(&bytes),
                    Some(Err(err)) => {
                        // Keep the transport cause; the caller would otherwise only see an
                        // unexplained early end.
                        yield PullEvent::Error(format!("pull response interrupted: {err}"));
                        return;
                    }
                    None => {
                        // Terminate a final update sent without a trailing newline.
                        buf.extend_from_slice(b"\n");
                        finished = true;
                    }
                }
                while let Some(line) = buf.take_line() {
                    if let Ok(text) = std::str::from_utf8(&line) {
                        let text = text.trim();
                        if text.is_empty() { continue; }
                        if let Ok(value) = serde_json::from_str::<JsonValue>(text) {
                            for ev in pull_events_from_value(&value) { yield ev; }
                            if let Some(err_msg) = value.get("error").and_then(|e| e.as_str()) {
                                yield PullEvent::Error(err_msg.to_string());
                                return;
                            }
                            // The parser already emitted `Success` for this line.
                            if let Some(status) = value.get("status").and_then(|s| s.as_str())
                                && status == "success" { return; }
                        }
                    }
                }
                if buf.pending_len() > MAX_PULL_LINE_BYTES {
                    yield PullEvent::Error(format!(
                        "pull response line exceeded {MAX_PULL_LINE_BYTES} bytes"
                    ));
                    return;
                }
            }
        };

        Ok(Box::pin(s))
    }

    /// Pull a model while reporting progress to the CLI.
    pub(crate) async fn pull_with_cli_progress(&self, model: &str) -> io::Result<()> {
        let mut reporter = CliProgressReporter::new();
        reporter.on_event(&PullEvent::Status(format!("Pulling model {model}...")))?;
        let mut stream = self.pull_model_stream(model).await?;
        while let Some(event) = stream.next().await {
            reporter.on_event(&event)?;
            match event {
                PullEvent::Success => {
                    return Ok(());
                }
                PullEvent::Error(err) => {
                    // Empirically, ollama returns a 200 OK response even when
                    // the output stream includes an error message. Verify with:
                    //
                    // `curl -i http://localhost:11434/api/pull -d '{ "model": "foobarbaz" }'`
                    //
                    // As such, we have to check the event stream, not the
                    // HTTP response status, to determine whether to return Err.
                    return Err(io::Error::other(format!("Pull failed: {err}")));
                }
                PullEvent::ChunkProgress { .. } | PullEvent::Status(_) => {
                    continue;
                }
            }
        }
        Err(io::Error::other(
            "Pull stream ended unexpectedly without success.",
        ))
    }

    /// Low-level constructor given a raw host root, e.g. "http://localhost:11434".
    #[cfg(test)]
    fn from_host_root(host_root: impl Into<String>) -> io::Result<Self> {
        let client = HttpClientBuilder::new()
            .connect_timeout(Duration::from_secs(5))
            .build_direct()
            .map_err(io::Error::other)?;
        Ok(Self {
            client,
            host_root: host_root.into(),
            uses_openai_compat: false,
            setup_timeout: SETUP_REQUEST_TIMEOUT,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn provider_without_base_url_is_a_configuration_error() {
        let mut provider =
            create_oss_provider_with_base_url("http://localhost:11434/v1", WireApi::Responses);
        provider.base_url = None;

        let Err(error) = OllamaClient::try_from_provider(&provider).await else {
            panic!("missing base URL must be a configuration error");
        };

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "Ollama provider must have a base_url");
    }

    // Happy-path tests using a mock HTTP server; skip if sandbox network is disabled.
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
            .and(wiremock::matchers::path("/api/tags"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_raw(
                    serde_json::json!({
                        "models": [ {"name": "llama3.2:3b"}, {"name":"mistral"} ]
                    })
                    .to_string(),
                    "application/json",
                ),
            )
            .mount(&server)
            .await;

        let client = OllamaClient::from_host_root(server.uri()).expect("shared HTTP client");
        let models = client.fetch_models().await.expect("fetch models");
        assert!(models.contains(&"llama3.2:3b".to_string()));
        assert!(models.contains(&"mistral".to_string()));
    }

    #[tokio::test]
    async fn fetch_models_reports_a_rejected_listing_instead_of_no_models() {
        if std::env::var(codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
            tracing::info!(
                "{} is set; skipping fetch_models_reports_a_rejected_listing_instead_of_no_models",
                codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR
            );
            return;
        }

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/tags"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let client = OllamaClient::from_host_root(server.uri()).expect("shared HTTP client");
        let error = client
            .fetch_models()
            .await
            .expect_err("an unavailable listing must not look like an empty model list");
        assert_eq!(
            error.to_string(),
            "failed to list models: HTTP 503 Service Unavailable"
        );
    }

    #[tokio::test]
    async fn test_fetch_version() {
        if std::env::var(codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
            tracing::info!(
                "{} is set; skipping test_fetch_version",
                codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR
            );
            return;
        }

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/tags"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw(
                serde_json::json!({ "models": [] }).to_string(),
                "application/json",
            ))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/version"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw(
                serde_json::json!({ "version": "0.14.1" }).to_string(),
                "application/json",
            ))
            .mount(&server)
            .await;

        let client = OllamaClient::try_from_provider_with_base_url(server.uri().as_str())
            .await
            .expect("client");

        let version = client.fetch_version().await.expect("version fetch");
        assert_eq!(version, Some(Version::new(0, 14, 1)));
    }

    #[tokio::test]
    async fn test_pull_model_stream_uses_shared_http_client() {
        if std::env::var(codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
            tracing::info!(
                "{} set; skipping test_pull_model_stream_uses_shared_http_client",
                codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR
            );
            return;
        }

        let server = wiremock::MockServer::start().await;
        // The final update has no trailing newline and must still be delivered.
        let body = format!(
            "{}\n{}\n{}",
            serde_json::json!({
                "status": "pulling layers",
                "padding": "x".repeat(128 * 1024),
            }),
            serde_json::json!({"status": "complete"}),
            serde_json::json!({"status": "success"}),
        );
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/pull"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_raw(body, "application/x-ndjson"),
            )
            .mount(&server)
            .await;

        let client = OllamaClient::from_host_root(server.uri()).expect("shared HTTP client");
        let events = client
            .pull_model_stream("test-model")
            .await
            .expect("start pull stream")
            .collect::<Vec<_>>()
            .await;

        assert_matches!(
            events.as_slice(),
            [
                PullEvent::Status(pulling),
                PullEvent::Status(complete),
                PullEvent::Status(success),
                PullEvent::Success,
            ] if pulling == "pulling layers" && complete == "complete" && success == "success"
        );
    }

    #[tokio::test]
    async fn pull_stream_rejects_an_unbounded_line() {
        if std::env::var(codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
            tracing::info!(
                "{} set; skipping pull_stream_rejects_an_unbounded_line",
                codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR
            );
            return;
        }

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/pull"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw(
                "x".repeat(MAX_PULL_LINE_BYTES + 1),
                "application/x-ndjson",
            ))
            .mount(&server)
            .await;

        let client = OllamaClient::from_host_root(server.uri()).expect("shared HTTP client");
        let events = client
            .pull_model_stream("test-model")
            .await
            .expect("start pull stream")
            .collect::<Vec<_>>()
            .await;

        assert_matches!(
            events.as_slice(),
            [PullEvent::Error(message)] if message.contains("exceeded")
        );
    }

    #[tokio::test]
    async fn setup_requests_time_out_when_the_server_never_answers() {
        if std::env::var(codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
            tracing::info!(
                "{} set; skipping setup_requests_time_out_when_the_server_never_answers",
                codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR
            );
            return;
        }

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
            .mount(&server)
            .await;
        let mut client = OllamaClient::from_host_root(server.uri()).expect("shared HTTP client");
        client.setup_timeout = Duration::from_millis(200);

        tokio::time::timeout(Duration::from_secs(5), async {
            let probe = client
                .probe_server()
                .await
                .expect_err("a silent server must not look reachable");
            assert_eq!(probe.kind(), io::ErrorKind::TimedOut);
            assert!(probe.to_string().contains("did not respond"), "{probe}");
            client
                .fetch_models()
                .await
                .expect_err("a silent listing must fail instead of hanging");
            client
                .fetch_version()
                .await
                .expect_err("a silent version endpoint must fail instead of hanging");
        })
        .await
        .expect("setup requests must be bounded");
    }

    #[tokio::test]
    async fn fetch_version_treats_unreadable_body_as_unknown() {
        if std::env::var(codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
            tracing::info!(
                "{} set; skipping fetch_version_treats_unreadable_body_as_unknown",
                codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR
            );
            return;
        }

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/version"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_raw("<html></html>", "text/html"),
            )
            .mount(&server)
            .await;

        let client = OllamaClient::from_host_root(server.uri()).expect("shared HTTP client");
        assert_eq!(client.fetch_version().await.expect("version fetch"), None);
    }

    #[tokio::test]
    async fn test_probe_server_happy_path_openai_compat_and_native() {
        if std::env::var(codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
            tracing::info!(
                "{} set; skipping test_probe_server_happy_path_openai_compat_and_native",
                codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR
            );
            return;
        }

        let server = wiremock::MockServer::start().await;

        // Native endpoint
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/tags"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let native = OllamaClient::from_host_root(server.uri()).expect("shared HTTP client");
        native.probe_server().await.expect("probe native");

        // OpenAI compatibility endpoint
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v1/models"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let ollama_client =
            OllamaClient::try_from_provider_with_base_url(&format!("{}/v1", server.uri()))
                .await
                .expect("probe OpenAI compat");
        ollama_client
            .probe_server()
            .await
            .expect("probe OpenAI compat");
    }

    #[tokio::test]
    async fn test_try_from_oss_provider_ok_when_server_running() {
        if std::env::var(codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
            tracing::info!(
                "{} set; skipping test_try_from_oss_provider_ok_when_server_running",
                codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR
            );
            return;
        }

        let server = wiremock::MockServer::start().await;

        // OpenAI‑compat models endpoint responds OK.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v1/models"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;

        OllamaClient::try_from_provider_with_base_url(&format!("{}/v1", server.uri()))
            .await
            .expect("client should be created when probe succeeds");
    }

    #[tokio::test]
    async fn test_try_from_oss_provider_err_when_server_missing() {
        if std::env::var(codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
            tracing::info!(
                "{} set; skipping test_try_from_oss_provider_err_when_server_missing",
                codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR
            );
            return;
        }

        let server = wiremock::MockServer::start().await;
        let err = OllamaClient::try_from_provider_with_base_url(&format!("{}/v1", server.uri()))
            .await
            .err()
            .expect("expected error");
        assert_eq!(OLLAMA_CONNECTION_ERROR, err.to_string());
    }
}
