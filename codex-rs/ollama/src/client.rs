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
use codex_http_client::HttpResponse;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::OLLAMA_OSS_PROVIDER_ID;

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
        Self::try_from_provider(Self::oss_provider(config)?).await
    }

    /// Reuse the native health response as the initial listing, without caching later listings.
    pub(crate) async fn try_from_oss_provider_with_models(
        config: &Config,
    ) -> io::Result<(Self, io::Result<Vec<String>>)> {
        Self::try_from_provider_with_models(Self::oss_provider(config)?).await
    }

    fn oss_provider(config: &Config) -> io::Result<&ModelProviderInfo> {
        // Note that we must look up the provider from the Config to ensure that
        // any overrides the user has in their config.toml are taken into
        // account.
        config
            .model_providers
            .get(OLLAMA_OSS_PROVIDER_ID)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Built-in provider {OLLAMA_OSS_PROVIDER_ID} not found",),
                )
            })
    }


    /// Build a client from a provider definition and verify the server is reachable.
    pub(crate) async fn try_from_provider(provider: &ModelProviderInfo) -> io::Result<Self> {
        let client = Self::from_provider(provider)?;
        // Consume the health body so the connection can return to the pool.
        let _ = client.probe_server().await?.bytes().await;
        Ok(client)
    }

    async fn try_from_provider_with_models(
        provider: &ModelProviderInfo,
    ) -> io::Result<(Self, io::Result<Vec<String>>)> {
        let client = Self::from_provider(provider)?;
        let response = client.probe_server().await?;
        let models = if client.uses_openai_compat {
            let _ = response.bytes().await;
            client.fetch_models().await
        } else {
            Self::models_from_response(response).await
        };
        Ok((client, models))
    }

    fn from_provider(provider: &ModelProviderInfo) -> io::Result<Self> {
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
        Ok(client)
    }

    /// Probe whether the server is reachable by hitting the appropriate health endpoint.
    async fn probe_server(&self) -> io::Result<HttpResponse> {
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
            Ok(resp)
        } else {
            tracing::warn!(
                "Failed to probe server at {}: HTTP {}",
                self.host_root,
                resp.status()
            );
            Err(io::Error::other(format!(
                "Ollama answered with HTTP {}. Check the configured endpoint and server error; authentication or route configuration may need attention.",
                resp.status()
            )))
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
        Self::models_from_response(resp).await
    }

    async fn models_from_response(resp: HttpResponse) -> io::Result<Vec<String>> {
        if !resp.status().is_success() {
            return Err(io::Error::other(format!(
                "failed to list models: HTTP {}",
                resp.status()
            )));
        }
        let value = resp.json::<JsonValue>().await.map_err(io::Error::other)?;
        codex_model_provider_info::local::parse_model_ids(
            codex_model_provider_info::ProviderKind::Ollama,
            &value,
        )
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
                            if let Some(err_msg) = value.get("error").and_then(|e| e.as_str()) {
                                yield PullEvent::Error(err_msg.to_string());
                                return;
                            }
                            for ev in pull_events_from_value(&value) { yield ev; }
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

}

