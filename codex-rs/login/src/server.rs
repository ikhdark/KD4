//! Local OAuth callback server for CLI login.
//!
//! This module runs the short-lived localhost server used by interactive sign-in.
//!
//! The callback flow has two competing responsibilities:
//!
//! - preserve enough backend and transport detail for developers, sysadmins, and support
//!   engineers to diagnose failed sign-ins
//! - avoid persisting secrets or sensitive URL/query data into normal application logs
//!
//! This module therefore keeps the user-facing error path and the structured-log path separate.
//! Returned `io::Error` values still carry the detail needed by CLI/browser callers, while
//! structured logs only emit explicitly reviewed fields plus redacted URL/error values.
use std::io::Cursor;
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::net::SocketAddr;
use std::net::TcpStream;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use crate::auth::AuthDotJson;
use crate::auth::AuthKeyringBackendKind;
use crate::auth::save_auth;
use crate::callback_params::LoginCallbackResult;
use crate::callback_params::login_callback_result_from_state;
use crate::default_client::create_raw_auth_client_async;
use crate::default_client::originator;
use crate::outbound_proxy::AuthRouteConfig;
use crate::pkce::PkceCodes;
use crate::pkce::generate_pkce;
use crate::success_page::LoginSuccessPage;
use crate::success_page::LoginSuccessRedirect;
use crate::success_page::compose_success_url;
use crate::success_page::jwt_auth_claims;
use crate::token_data::TokenData;
use crate::token_data::parse_chatgpt_jwt_claims;
use base64::Engine;
use chrono::Utc;
use codex_config::types::AuthCredentialsStoreMode;
use codex_protocol::auth::AuthMode;
use codex_utils_template::Template;
use rand::RngCore;
use serde_json::Value as JsonValue;
use tiny_http::Header;
use tiny_http::Request;
use tiny_http::Response;
use tiny_http::Server;
use tiny_http::StatusCode;
use tracing::error;
use tracing::info;
use tracing::warn;

pub(super) const DEFAULT_ISSUER: &str = "https://auth.openai.com";
const DEFAULT_PORT: u16 = 1455;
// Keep in sync with the Codex CLI Hydra redirect URI allow-list.
const FALLBACK_PORT: u16 = 1457;
const SUCCESS_HANDOFF_TIMEOUT: Duration = Duration::from_secs(5);
const LOGIN_PENDING: u8 = 0;
const LOGIN_CANCELLED: u8 = 1;
const LOGIN_COMMITTING: u8 = 2;
static LOGIN_ERROR_PAGE_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    Template::parse(include_str!("assets/error.html"))
        .unwrap_or_else(|err| panic!("login error page template must parse: {err}"))
});

/// Options for launching the local login callback server.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub codex_home: PathBuf,
    pub client_id: String,
    pub issuer: String,
    pub port: u16,
    pub open_browser: bool,
    pub force_state: Option<String>,
    pub forced_chatgpt_workspace_id: Option<Vec<String>>,
    pub codex_streamlined_login: bool,
    pub login_success_page: LoginSuccessPage,
    pub cli_auth_credentials_store_mode: AuthCredentialsStoreMode,
    pub auth_keyring_backend_kind: AuthKeyringBackendKind,
    pub auth_route_config: AuthRouteConfig,
}

impl ServerOptions {
    /// Creates a server configuration with the default issuer and port.
    pub fn new(
        codex_home: PathBuf,
        client_id: String,
        forced_chatgpt_workspace_id: Option<Vec<String>>,
        cli_auth_credentials_store_mode: AuthCredentialsStoreMode,
        auth_keyring_backend_kind: AuthKeyringBackendKind,
        auth_route_config: AuthRouteConfig,
    ) -> Self {
        Self {
            codex_home,
            client_id,
            issuer: DEFAULT_ISSUER.to_string(),
            port: DEFAULT_PORT,
            open_browser: true,
            force_state: None,
            forced_chatgpt_workspace_id,
            codex_streamlined_login: false,
            login_success_page: LoginSuccessPage::default(),
            cli_auth_credentials_store_mode,
            auth_keyring_backend_kind,
            auth_route_config,
        }
    }
}

/// Handle for a running login callback server.
pub struct LoginServer {
    pub auth_url: String,
    pub actual_port: u16,
    server_handle: tokio::task::JoinHandle<io::Result<LoginCallbackResult>>,
    shutdown_handle: ShutdownHandle,
}

impl LoginServer {
    /// Waits for the login callback loop to finish.
    pub async fn block_until_done(self) -> io::Result<()> {
        self.block_until_done_with_callback_result()
            .await
            .map(|_| ())
    }

    /// Waits for login to finish and returns allowlisted callback metadata.
    pub async fn block_until_done_with_callback_result(
        mut self,
    ) -> io::Result<LoginCallbackResult> {
        (&mut self.server_handle)
            .await
            .map_err(|err| io::Error::other(format!("login server thread panicked: {err:?}")))?
    }

    /// Requests shutdown of the callback server.
    pub fn cancel(&self) {
        self.shutdown_handle.shutdown();
    }

    /// Returns a cloneable cancel handle for the running server.
    pub fn cancel_handle(&self) -> ShutdownHandle {
        self.shutdown_handle.clone()
    }
}

impl Drop for LoginServer {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Handle used to signal the login server loop to exit.
#[derive(Clone, Debug)]
pub struct ShutdownHandle {
    shutdown_notify: Arc<tokio::sync::Notify>,
    state: Arc<AtomicU8>,
}

impl ShutdownHandle {
    /// Signals the login loop to terminate.
    pub fn shutdown(&self) {
        if self
            .state
            .compare_exchange(
                LOGIN_PENDING,
                LOGIN_CANCELLED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            self.shutdown_notify.notify_one();
        }
    }

    async fn cancelled(&self) {
        if self.state.load(Ordering::SeqCst) != LOGIN_CANCELLED {
            self.shutdown_notify.notified().await;
        }
    }

    // Admission and shutdown have one ordering. An admitted blocking write must
    // finish; cancellation cannot undo it by dropping its async waiter.
    fn begin_commit(&self) -> bool {
        self.state
            .compare_exchange(
                LOGIN_PENDING,
                LOGIN_COMMITTING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }
}

/// Starts a local callback server and returns the browser auth URL.
pub fn run_login_server(opts: ServerOptions) -> io::Result<LoginServer> {
    let server = bind_server(opts.port)?;
    start_login_server(opts, server)
}

/// Starts a callback server without blocking the async caller during port retries.
///
/// Only binding runs in the blocking pool. If this future is cancelled, the
/// returned tiny_http server is dropped before callback workers or a browser
/// can be started.
pub async fn run_login_server_async(opts: ServerOptions) -> io::Result<LoginServer> {
    let port = opts.port;
    let server = tokio::task::spawn_blocking(move || bind_server(port))
        .await
        .map_err(|err| io::Error::other(format!("login server bind task failed: {err}")))??;
    start_login_server(opts, server)
}

fn start_login_server(opts: ServerOptions, server: Server) -> io::Result<LoginServer> {
    let pkce = generate_pkce();
    let state = opts.force_state.clone().unwrap_or_else(generate_state);

    let actual_port = match server.server_addr().to_ip() {
        Some(addr) => addr.port(),
        None => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "Unable to determine the server port",
            ));
        }
    };
    let server = Arc::new(server);

    let redirect_uri = format!("http://localhost:{actual_port}/auth/callback");
    let auth_url = build_authorize_url(
        &opts.issuer,
        &opts.client_id,
        &redirect_uri,
        &pkce,
        &state,
        opts.forced_chatgpt_workspace_id.as_deref(),
    );

    if opts.open_browser {
        let _ = webbrowser::open(&auth_url);
    }

    // Map blocking reads from server.recv() to an async channel.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Request>(16);
    let _server_handle = {
        let server = server.clone();
        thread::spawn(move || -> io::Result<()> {
            while let Ok(request) = server.recv() {
                match tx.blocking_send(request) {
                    Ok(()) => {}
                    Err(error) => {
                        eprintln!("Failed to send request to channel: {error}");
                        return Err(io::Error::other("Failed to send request to channel"));
                    }
                }
            }
            Ok(())
        })
    };

    let shutdown_handle = ShutdownHandle {
        shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        state: Arc::new(AtomicU8::new(LOGIN_PENDING)),
    };
    let server_handle = {
        let shutdown_handle = shutdown_handle.clone();
        let server = server;
        tokio::spawn(async move {
            let mut callback_result = None;
            let mut handoff_deadline = None;
            let result = loop {
                tokio::select! {
                    biased;
                    _ = shutdown_handle.cancelled() => {
                        break callback_result.ok_or_else(|| io::Error::other("Login was not completed"));
                    }
                    _ = async {
                        match handoff_deadline {
                            Some(deadline) => tokio::time::sleep_until(deadline).await,
                            None => std::future::pending().await,
                        }
                    } => {
                        break callback_result.ok_or_else(|| io::Error::other("Login was not completed"));
                    }
                    maybe_req = rx.recv() => {
                        let Some(req) = maybe_req else {
                            break Err(io::Error::other("Login was not completed"));
                        };

                        let url_raw = req.url().to_string();
                        let response =
                            process_request(
                                &url_raw,
                                &opts,
                                &redirect_uri,
                                &pkce,
                                actual_port,
                                &state,
                                callback_result.is_some(),
                                &shutdown_handle,
                            )
                            .await;

                        let exit_result = match response {
                            HandledRequest::Response(response) => {
                                let _ = send_login_response(move || req.respond(response)).await;
                                None
                            }
                            HandledRequest::RedirectWithHeader { header, result } => {
                                callback_result = Some(result);
                                handoff_deadline = Some(tokio::time::Instant::now() + SUCCESS_HANDOFF_TIMEOUT);
                                let redirect = Response::empty(302).with_header(header);
                                match send_login_response(move || req.respond(redirect)).await {
                                    Ok(Ok(())) => None,
                                    _ => {
                                        warn!("failed to send local login redirect after saving credentials");
                                        Some(Ok(result))
                                    }
                                }
                            }
                            HandledRequest::ResponseAndExit {
                                headers,
                                body,
                                result,
                            } => {
                                let _ = send_login_response(move || {
                                    send_response_with_disconnect(
                                        req,
                                        StatusCode(200),
                                        headers,
                                        body,
                                    )
                                })
                                .await;
                                Some(match callback_result {
                                    Some(result) => Ok(result),
                                    None => result.and_then(|()| Err(io::Error::other("Login was not completed"))),
                                })
                            }
                            HandledRequest::RedirectAndExit { header, result } => {
                                match send_login_response(move || {
                                    send_response_with_disconnect(
                                        req,
                                        StatusCode(302),
                                        vec![header],
                                        Vec::new(),
                                    )
                                })
                                .await
                                {
                                    Ok(Ok(())) => {}
                                    Ok(Err(err)) => {
                                        warn!("failed to send hosted login redirect: {err}");
                                    }
                                    Err(err) => {
                                        warn!("hosted login redirect task failed: {err}");
                                    }
                                }
                                Some(Ok(result))
                            }
                        };

                        if let Some(result) = exit_result {
                            break result;
                        }
                    }
                }
            };

            // Ensure that the server is unblocked so the thread dedicated to
            // running `server.recv()` in a loop exits cleanly.
            server.unblock();
            result
        })
    };

    Ok(LoginServer {
        auth_url,
        actual_port,
        server_handle,
        shutdown_handle,
    })
}

// A stalled browser must not hold the login task open after credentials commit.
async fn send_login_response<F>(send: F) -> Result<io::Result<()>, io::Error>
where
    F: FnOnce() -> io::Result<()> + Send + 'static,
{
    tokio::time::timeout(SUCCESS_HANDOFF_TIMEOUT, tokio::task::spawn_blocking(send))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "login response timed out"))?
        .map_err(io::Error::other)
}

/// Internal callback handling outcome.
enum HandledRequest {
    Response(Response<Cursor<Vec<u8>>>),
    RedirectWithHeader {
        header: Header,
        result: LoginCallbackResult,
    },
    RedirectAndExit {
        header: Header,
        result: LoginCallbackResult,
    },
    ResponseAndExit {
        headers: Vec<Header>,
        body: Vec<u8>,
        result: io::Result<()>,
    },
}

#[expect(
    clippy::too_many_arguments,
    reason = "Receives the active OAuth attempt state and shutdown context from the callback server"
)]
async fn process_request(
    url_raw: &str,
    opts: &ServerOptions,
    redirect_uri: &str,
    pkce: &PkceCodes,
    actual_port: u16,
    state: &str,
    credentials_committed: bool,
    shutdown_handle: &ShutdownHandle,
) -> HandledRequest {
    let parsed_url = match url::Url::parse(&format!("http://localhost{url_raw}")) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("URL parse error: {e}");
            return HandledRequest::Response(
                Response::from_string("Bad Request").with_status_code(400),
            );
        }
    };
    let path = parsed_url.path().to_string();

    match path.as_str() {
        "/auth/callback" => {
            if credentials_committed {
                return HandledRequest::Response(
                    Response::from_string("Login already completed").with_status_code(409),
                );
            }
            let params: std::collections::HashMap<String, String> =
                parsed_url.query_pairs().into_owned().collect();
            let has_code = params.get("code").is_some_and(|code| !code.is_empty());
            let has_state = params.get("state").is_some_and(|state| !state.is_empty());
            let has_error = params.get("error").is_some_and(|error| !error.is_empty());
            let callback_result = params
                .get("state")
                .and_then(|callback_state| login_callback_result_from_state(callback_state, state));
            let state_valid = callback_result.is_some();
            info!(
                path = %path,
                has_code,
                has_state,
                has_error,
                state_valid,
                "received login callback"
            );
            if !state_valid {
                warn!(
                    path = %path,
                    has_code,
                    has_state,
                    has_error,
                    "login callback state mismatch"
                );
                return HandledRequest::Response(
                    Response::from_string("State mismatch").with_status_code(400),
                );
            }
            if let Some(error_code) = params.get("error") {
                let error_description = params.get("error_description").map(String::as_str);
                let message = oauth_callback_error_message(error_code, error_description);
                eprintln!("OAuth callback error: {message}");
                warn!(
                    error_code = loggable_oauth_error_code(Some(error_code)),
                    has_error_description = error_description.is_some_and(|s| !s.trim().is_empty()),
                    "oauth callback returned error"
                );
                return login_error_response(
                    &message,
                    io::ErrorKind::PermissionDenied,
                    Some(error_code),
                    error_description,
                );
            }
            let code = match params.get("code") {
                Some(c) if !c.is_empty() => c.clone(),
                _ => {
                    return login_error_response(
                        "Missing authorization code. Sign-in could not be completed.",
                        io::ErrorKind::InvalidData,
                        Some("missing_authorization_code"),
                        /*error_description*/ None,
                    );
                }
            };
            let callback_result = callback_result.unwrap_or_default();

            let exchange = exchange_code_for_tokens(
                &opts.issuer,
                &opts.client_id,
                redirect_uri,
                pkce,
                &code,
                &opts.auth_route_config,
            );
            let exchanged = tokio::select! {
                biased;
                _ = shutdown_handle.cancelled() => return cancelled_login_response(),
                result = exchange => result,
            };
            match exchanged {
                Ok(tokens) => {
                    if let Err(message) = ensure_workspace_allowed(
                        opts.forced_chatgpt_workspace_id.as_deref(),
                        &tokens.id_token,
                    ) {
                        eprintln!("Workspace restriction error: {message}");
                        return login_error_response(
                            &message,
                            io::ErrorKind::PermissionDenied,
                            Some("workspace_restriction"),
                            /*error_description*/ None,
                        );
                    }
                    // Obtain API key via token-exchange and persist
                    let api_key_exchange = obtain_api_key(
                        &opts.issuer,
                        &opts.client_id,
                        &tokens.id_token,
                        &opts.auth_route_config,
                    );
                    let api_key = tokio::select! {
                        biased;
                        _ = shutdown_handle.cancelled() => return cancelled_login_response(),
                        result = api_key_exchange => result.ok(),
                    };
                    // Persistence is admitted here. Once the blocking write is
                    // started, await its real outcome even if shutdown arrives.
                    if !shutdown_handle.begin_commit() {
                        return cancelled_login_response();
                    }
                    if let Err(err) = persist_tokens_async(
                        &opts.codex_home,
                        api_key,
                        tokens.id_token.clone(),
                        tokens.access_token.clone(),
                        tokens.refresh_token.clone(),
                        opts.cli_auth_credentials_store_mode,
                        opts.auth_keyring_backend_kind,
                    )
                    .await
                    {
                        eprintln!("Persist error: {err}");
                        return login_error_response(
                            "Sign-in completed but credentials could not be saved locally.",
                            io::ErrorKind::Other,
                            Some("persist_failed"),
                            Some(&err.to_string()),
                        );
                    }

                    let redirect = compose_success_url(
                        actual_port,
                        &opts.issuer,
                        &tokens.id_token,
                        &tokens.access_token,
                        opts.codex_streamlined_login,
                        &opts.login_success_page,
                    );
                    let url = match &redirect {
                        LoginSuccessRedirect::Local(url) | LoginSuccessRedirect::Hosted(url) => url,
                    };
                    match tiny_http::Header::from_bytes(&b"Location"[..], url.as_bytes()) {
                        Ok(header) => match redirect {
                            LoginSuccessRedirect::Local(_) => HandledRequest::RedirectWithHeader {
                                header,
                                result: callback_result,
                            },
                            LoginSuccessRedirect::Hosted(_) => HandledRequest::RedirectAndExit {
                                header,
                                result: callback_result,
                            },
                        },
                        Err(_) => login_error_response(
                            "Sign-in completed but redirecting back to Codex failed.",
                            io::ErrorKind::Other,
                            Some("redirect_failed"),
                            /*error_description*/ None,
                        ),
                    }
                }
                Err(err) => {
                    eprintln!("Token exchange error: {err}");
                    error!("login callback token exchange failed");
                    login_error_response(
                        &format!("Token exchange failed: {err}"),
                        io::ErrorKind::Other,
                        Some("token_exchange_failed"),
                        /*error_description*/ None,
                    )
                }
            }
        }
        "/success" => {
            if !credentials_committed {
                return HandledRequest::Response(
                    Response::from_string("Login has not completed").with_status_code(409),
                );
            }
            let use_streamlined_success = parsed_url
                .query_pairs()
                .any(|(key, value)| key == "codex_streamlined_login" && value == "true");
            let body = if use_streamlined_success {
                include_str!("assets/success.html")
            } else {
                include_str!("assets/success_legacy.html")
            };
            HandledRequest::ResponseAndExit {
                headers: match Header::from_bytes(
                    &b"Content-Type"[..],
                    &b"text/html; charset=utf-8"[..],
                ) {
                    Ok(header) => vec![header],
                    Err(_) => Vec::new(),
                },
                body: body.as_bytes().to_vec(),
                result: Ok(()),
            }
        }
        "/cancel" if credentials_committed => HandledRequest::ResponseAndExit {
            headers: Vec::new(),
            body: b"Login already completed".to_vec(),
            result: Ok(()),
        },
        "/cancel" => cancelled_login_response(),
        _ => HandledRequest::Response(Response::from_string("Not Found").with_status_code(404)),
    }
}

fn cancelled_login_response() -> HandledRequest {
    HandledRequest::ResponseAndExit {
        headers: Vec::new(),
        body: b"Login cancelled".to_vec(),
        result: Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Login cancelled",
        )),
    }
}

fn loggable_oauth_error_code(code: Option<&str>) -> &str {
    match code {
        Some(
            code @ ("invalid_request"
            | "invalid_client"
            | "invalid_grant"
            | "unauthorized_client"
            | "unsupported_grant_type"
            | "invalid_scope"
            | "access_denied"
            | "server_error"
            | "temporarily_unavailable"),
        ) => code,
        _ => "unknown",
    }
}

/// Write the terminal response with `Connection: close`, which tiny_http filters
/// from ordinary responses. This asks the client to stop reusing the connection;
/// it does not provide a hard shutdown of a stalled peer's socket.
fn send_response_with_disconnect(
    req: Request,
    status: StatusCode,
    mut headers: Vec<Header>,
    body: Vec<u8>,
) -> io::Result<()> {
    let mut writer = req.into_writer();
    let reason = status.default_reason_phrase();
    write!(writer, "HTTP/1.1 {} {}\r\n", status.0, reason)?;
    headers.retain(|h| !h.field.equiv("Connection"));
    if let Ok(close_header) = Header::from_bytes(&b"Connection"[..], &b"close"[..]) {
        headers.push(close_header);
    }

    let content_length_value = format!("{}", body.len());
    if let Ok(content_length_header) =
        Header::from_bytes(&b"Content-Length"[..], content_length_value.as_bytes())
    {
        headers.push(content_length_header);
    }

    for header in headers {
        write!(
            writer,
            "{}: {}\r\n",
            header.field.as_str(),
            header.value.as_str()
        )?;
    }

    writer.write_all(b"\r\n")?;
    writer.write_all(&body)?;
    writer.flush()
}

fn build_authorize_url(
    issuer: &str,
    client_id: &str,
    redirect_uri: &str,
    pkce: &PkceCodes,
    state: &str,
    forced_chatgpt_workspace_ids: Option<&[String]>,
) -> String {
    let mut query = vec![
        ("response_type".to_string(), "code".to_string()),
        ("client_id".to_string(), client_id.to_string()),
        ("redirect_uri".to_string(), redirect_uri.to_string()),
        (
            "scope".to_string(),
            "openid profile email offline_access api.connectors.read api.connectors.invoke"
                .to_string(),
        ),
        (
            "code_challenge".to_string(),
            pkce.code_challenge.to_string(),
        ),
        ("code_challenge_method".to_string(), "S256".to_string()),
        ("id_token_add_organizations".to_string(), "true".to_string()),
        ("codex_cli_simplified_flow".to_string(), "true".to_string()),
        ("state".to_string(), state.to_string()),
        ("originator".to_string(), originator().value),
    ];
    if let Some(workspace_ids) = forced_chatgpt_workspace_ids {
        query.push(("allowed_workspace_id".to_string(), workspace_ids.join(",")));
    }
    let qs = crate::form_urlencode(query);
    format!("{}/oauth/authorize?{qs}", issuer.trim_end_matches('/'))
}

fn generate_state() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn send_cancel_request(port: u16) -> io::Result<()> {
    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    stream.write_all(b"GET /cancel HTTP/1.1\r\n")?;
    stream.write_all(format!("Host: 127.0.0.1:{port}\r\n").as_bytes())?;
    stream.write_all(b"Connection: close\r\n\r\n")?;

    let mut buf = [0u8; 64];
    let _ = stream.read(&mut buf);
    Ok(())
}

fn bind_server(port: u16) -> io::Result<Server> {
    let preferred_bind_address = format!("127.0.0.1:{port}");
    let fallback_bind_address = format!("127.0.0.1:{FALLBACK_PORT}");
    let mut bind_address = preferred_bind_address.clone();
    let mut cancel_attempted = false;
    let mut attempts = 0;
    let mut using_fallback_port = false;
    const MAX_ATTEMPTS: u32 = 10;
    const RETRY_DELAY: Duration = Duration::from_millis(200);

    loop {
        match Server::http(&bind_address) {
            Ok(server) => return Ok(server),
            Err(err) => {
                attempts += 1;
                let is_addr_in_use = err
                    .downcast_ref::<io::Error>()
                    .map(|io_err| io_err.kind() == io::ErrorKind::AddrInUse)
                    .unwrap_or(false);

                // If the address is in use, there may be another instance of the login server
                // running. Attempt to cancel it and retry before falling back.
                if is_addr_in_use {
                    if !cancel_attempted && !using_fallback_port {
                        cancel_attempted = true;
                        if let Err(cancel_err) = send_cancel_request(port) {
                            eprintln!("Failed to cancel previous login server: {cancel_err}");
                        }
                    }

                    thread::sleep(RETRY_DELAY);

                    if attempts >= MAX_ATTEMPTS {
                        if port == DEFAULT_PORT && !using_fallback_port {
                            warn!(
                                %preferred_bind_address,
                                %fallback_bind_address,
                                "default login callback port is unavailable; falling back to the registered fallback port"
                            );
                            bind_address = fallback_bind_address.clone();
                            attempts = 0;
                            using_fallback_port = true;
                            continue;
                        }

                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            format!("Port {bind_address} is already in use"),
                        ));
                    }

                    continue;
                }

                return Err(io::Error::other(err));
            }
        }
    }
}

/// Tokens returned by the OAuth authorization-code exchange.
pub(crate) struct ExchangedTokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TokenEndpointErrorDetail {
    error_code: Option<String>,
    error_message: Option<String>,
    display_message: String,
}

impl std::fmt::Display for TokenEndpointErrorDetail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.display_message.fmt(f)
    }
}

const REDACTED_URL_VALUE: &str = "<redacted>";
const SENSITIVE_URL_QUERY_KEYS: &[&str] = &[
    "access_token",
    "api_key",
    "client_secret",
    "code",
    "code_verifier",
    "id_token",
    "key",
    "refresh_token",
    "requested_token",
    "state",
    "subject_token",
    "token",
];

fn redact_sensitive_query_value(key: &str, value: &str) -> String {
    if SENSITIVE_URL_QUERY_KEYS
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(key))
    {
        REDACTED_URL_VALUE.to_string()
    } else {
        value.to_string()
    }
}

/// Redacts URL components that commonly carry auth secrets while preserving the host/path shape.
///
/// This keeps developer-facing logs useful for debugging transport failures without persisting
/// tokens, callback codes, fragments, or embedded credentials.
fn redact_sensitive_url_parts(url: &mut url::Url) {
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_fragment(None);

    let query_pairs = url
        .query_pairs()
        .map(|(key, value)| {
            let key = key.into_owned();
            let value = value.into_owned();
            (key.clone(), redact_sensitive_query_value(&key, &value))
        })
        .collect::<Vec<_>>();

    if query_pairs.is_empty() {
        url.set_query(None);
        return;
    }

    let redacted_query = crate::form_urlencode(query_pairs);
    url.set_query(Some(&redacted_query));
}

/// Redacts any URL attached to an HTTP transport error before it is logged or returned.
fn redact_sensitive_error_url(
    mut err: codex_http_client::HttpError,
) -> codex_http_client::HttpError {
    if let Some(url) = err.url_mut() {
        redact_sensitive_url_parts(url);
    }
    err
}

/// Sanitizes a free-form URL string for structured logging.
///
/// This is used for caller-supplied issuer values, which may contain credentials or query
/// parameters on non-default deployments.
fn sanitize_url_for_logging(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut url) => {
            redact_sensitive_url_parts(&mut url);
            url.to_string()
        }
        Err(_) => "<invalid-url>".to_string(),
    }
}
/// Exchanges an authorization code for tokens.
///
/// The returned error remains suitable for user-facing CLI/browser surfaces, so backend-provided
/// non-JSON error text is preserved there. Structured logging stays narrower: it logs reviewed
/// fields from parsed token responses and redacted transport errors, but does not log the final
/// callback-layer `%err` string.
pub(crate) async fn exchange_code_for_tokens(
    issuer: &str,
    client_id: &str,
    redirect_uri: &str,
    pkce: &PkceCodes,
    code: &str,
    auth_route_config: &AuthRouteConfig,
) -> io::Result<ExchangedTokens> {
    #[derive(serde::Deserialize)]
    struct TokenResponse {
        id_token: String,
        access_token: String,
        refresh_token: String,
    }

    // The route selected for the issuer is reused for token exchange; the token endpoint path is
    // not resolved separately.
    let client =
        create_raw_auth_client_async(issuer.trim_end_matches('/'), auth_route_config).await?;
    let token_endpoint = format!("{}/oauth/token", issuer.trim_end_matches('/'));
    info!(
        issuer = %sanitize_url_for_logging(issuer),
        token_endpoint = %sanitize_url_for_logging(&token_endpoint),
        redirect_uri = %redirect_uri,
        "starting oauth token exchange"
    );
    let resp = client
        .post(token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(crate::form_urlencode([
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", pkce.code_verifier.as_str()),
        ]))
        .send()
        .await;
    let resp = match resp {
        Ok(resp) => resp,
        Err(error) => {
            let error = redact_sensitive_error_url(error);
            error!(
                is_timeout = error.is_timeout(),
                is_connect = error.is_connect(),
                is_request = error.is_request(),
                error = %error,
                "oauth token exchange transport failure"
            );
            return Err(io::Error::other(error));
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.map_err(io::Error::other)?;
        let detail = parse_token_endpoint_error(&body);
        warn!(
            %status,
            error_code = loggable_oauth_error_code(detail.error_code.as_deref()),
            "oauth token exchange returned non-success status"
        );
        return Err(io::Error::other(format!(
            "token endpoint returned status {status}: {detail}"
        )));
    }

    let tokens: TokenResponse = resp.json().await.map_err(io::Error::other)?;
    info!(%status, "oauth token exchange succeeded");
    Ok(ExchangedTokens {
        id_token: tokens.id_token,
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
    })
}

/// Persists exchanged credentials using the configured local auth store.
pub(crate) async fn persist_tokens_async(
    codex_home: &Path,
    api_key: Option<String>,
    id_token: String,
    access_token: String,
    refresh_token: String,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> io::Result<()> {
    // Reuse existing synchronous logic but run it off the async runtime.
    let codex_home = codex_home.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut tokens = TokenData {
            id_token: parse_chatgpt_jwt_claims(&id_token).map_err(io::Error::other)?,
            access_token,
            refresh_token,
            account_id: None,
        };
        tokens.account_id = tokens.id_token.chatgpt_account_id.clone();
        let auth = AuthDotJson {
            auth_mode: Some(AuthMode::Chatgpt),
            openai_api_key: api_key,
            tokens: Some(tokens),
            last_refresh: Some(Utc::now()),
            agent_identity: None,
            personal_access_token: None,
            bedrock_api_key: None,
        };
        save_auth(
            &codex_home,
            &auth,
            auth_credentials_store_mode,
            keyring_backend_kind,
        )
    })
    .await
    .map_err(|e| io::Error::other(format!("persist task failed: {e}")))?
}

/// Validates the ID token against an optional workspace restriction.
pub(crate) fn ensure_workspace_allowed(
    expected: Option<&[String]>,
    id_token: &str,
) -> Result<(), String> {
    let Some(expected) = expected else {
        return Ok(());
    };

    let claims = jwt_auth_claims(id_token);
    let Some(actual) = claims.get("chatgpt_account_id").and_then(JsonValue::as_str) else {
        return Err("Login is restricted to a specific workspace, but the token did not include an chatgpt_account_id claim.".to_string());
    };

    ensure_workspace_account_allowed(Some(expected), actual)
}

/// Validates an already known ChatGPT account ID against an optional workspace restriction.
///
/// PAT login calls this directly because `/whoami` supplies the account ID without an ID token.
pub(crate) fn ensure_workspace_account_allowed(
    expected: Option<&[String]>,
    actual: &str,
) -> Result<(), String> {
    let Some(expected) = expected else {
        return Ok(());
    };

    if expected.iter().any(|workspace_id| workspace_id == actual) {
        Ok(())
    } else {
        Err(format!(
            "Login is restricted to workspace id(s) {}.",
            expected.join(", ")
        ))
    }
}

/// Builds a terminal callback response for login failures.
fn login_error_response(
    message: &str,
    kind: io::ErrorKind,
    error_code: Option<&str>,
    error_description: Option<&str>,
) -> HandledRequest {
    let mut headers = Vec::new();
    if let Ok(header) = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]) {
        headers.push(header);
    }
    let body = render_login_error_page(message, error_code, error_description);
    HandledRequest::ResponseAndExit {
        headers,
        body,
        result: Err(io::Error::new(kind, message.to_string())),
    }
}

/// Returns true when the OAuth callback represents a missing Codex entitlement.
fn is_missing_codex_entitlement_error(error_code: &str, error_description: Option<&str>) -> bool {
    error_code == "access_denied"
        && error_description.is_some_and(|description| {
            description
                .to_ascii_lowercase()
                .contains("missing_codex_entitlement")
        })
}

/// Converts OAuth callback errors into a user-facing message.
fn oauth_callback_error_message(error_code: &str, error_description: Option<&str>) -> String {
    if is_missing_codex_entitlement_error(error_code, error_description) {
        return "Codex is not enabled for your workspace. Contact your workspace administrator to request access to Codex.".to_string();
    }

    if let Some(description) = error_description
        && !description.trim().is_empty()
    {
        return format!("Sign-in failed: {description}");
    }

    format!("Sign-in failed: {error_code}")
}

/// Extracts token endpoint error detail for both structured logging and caller-visible errors.
///
/// JSON fields and raw bodies may contain credentials. Preserve them for the caller-visible
/// error only; structured logs use allowlisted error classifications and HTTP status.
fn parse_token_endpoint_error(body: &str) -> TokenEndpointErrorDetail {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return TokenEndpointErrorDetail {
            error_code: None,
            error_message: None,
            display_message: "unknown error".to_string(),
        };
    }

    let parsed = serde_json::from_str::<JsonValue>(trimmed).ok();
    if let Some(json) = parsed {
        let error_code = json
            .get("error")
            .and_then(JsonValue::as_str)
            .filter(|error_code| !error_code.trim().is_empty())
            .map(ToString::to_string)
            .or_else(|| {
                json.get("error")
                    .and_then(JsonValue::as_object)
                    .and_then(|error_obj| error_obj.get("code"))
                    .and_then(JsonValue::as_str)
                    .filter(|code| !code.trim().is_empty())
                    .map(ToString::to_string)
            });
        if let Some(description) = json.get("error_description").and_then(JsonValue::as_str)
            && !description.trim().is_empty()
        {
            return TokenEndpointErrorDetail {
                error_code,
                error_message: Some(description.to_string()),
                display_message: description.to_string(),
            };
        }
        if let Some(error_obj) = json.get("error")
            && let Some(message) = error_obj.get("message").and_then(JsonValue::as_str)
            && !message.trim().is_empty()
        {
            return TokenEndpointErrorDetail {
                error_code,
                error_message: Some(message.to_string()),
                display_message: message.to_string(),
            };
        }
        if let Some(error_code) = error_code {
            return TokenEndpointErrorDetail {
                display_message: error_code.clone(),
                error_code: Some(error_code),
                error_message: None,
            };
        }
    }

    // Preserve non-JSON token-endpoint bodies for the returned error so CLI/browser flows still
    // surface the backend detail users and admins need, but keep that text out of structured logs
    // by only logging allowlisted error classifications and avoiding `%err` logging at the callback
    // layer.
    TokenEndpointErrorDetail {
        error_code: None,
        error_message: None,
        display_message: trimmed.to_string(),
    }
}

/// Renders the branded error page used by callback failures.
fn render_login_error_page(
    message: &str,
    error_code: Option<&str>,
    error_description: Option<&str>,
) -> Vec<u8> {
    let code = error_code.unwrap_or("unknown_error");
    let (title, display_message, display_description, help_text) =
        if is_missing_codex_entitlement_error(code, error_description) {
            (
                "You do not have access to Codex".to_string(),
                "This account is not currently authorized to use Codex in this workspace."
                    .to_string(),
                "Contact your workspace administrator to request access to Codex.".to_string(),
                "Contact your workspace administrator to get access to Codex, then return to Codex and try again."
                    .to_string(),
            )
        } else {
            (
                "Sign-in could not be completed".to_string(),
                message.to_string(),
                error_description.unwrap_or(message).to_string(),
                "Return to Codex to retry, switch accounts, or contact your workspace admin if access is restricted."
                    .to_string(),
            )
        };
    LOGIN_ERROR_PAGE_TEMPLATE
        .render([
            ("error_title", html_escape(&title)),
            ("error_message", html_escape(&display_message)),
            ("error_code", html_escape(code)),
            ("error_description", html_escape(&display_description)),
            ("error_help", html_escape(&help_text)),
        ])
        .unwrap_or_else(|err| panic!("login error page template must render: {err}"))
        .into_bytes()
}

/// Escapes error strings before inserting them into HTML.
fn html_escape(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// Exchanges an authenticated ID token for an API-key style access token.
pub(crate) async fn obtain_api_key(
    issuer: &str,
    client_id: &str,
    id_token: &str,
    auth_route_config: &AuthRouteConfig,
) -> io::Result<String> {
    // Token exchange for an API key access token
    #[derive(serde::Deserialize)]
    struct ExchangeResp {
        access_token: String,
    }
    let token_endpoint = format!("{}/oauth/token", issuer.trim_end_matches('/'));
    let client =
        create_raw_auth_client_async(issuer.trim_end_matches('/'), auth_route_config).await?;
    let resp = client
        .post(token_endpoint)
        .timeout(Duration::from_secs(10))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(crate::form_urlencode([
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:token-exchange",
            ),
            ("client_id", client_id),
            ("requested_token", "openai-api-key"),
            ("subject_token", id_token),
            (
                "subject_token_type",
                "urn:ietf:params:oauth:token-type:id_token",
            ),
        ]))
        .send()
        .await
        .map_err(io::Error::other)?;
    if !resp.status().is_success() {
        return Err(io::Error::other(format!(
            "api key exchange failed with status {}",
            resp.status()
        )));
    }
    let body: ExchangeResp = resp.json().await.map_err(io::Error::other)?;
    Ok(body.access_token)
}
#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    #[test]
    fn logged_oauth_codes_exclude_unreviewed_values() {
        assert_eq!(
            super::loggable_oauth_error_code(Some("invalid_grant")),
            "invalid_grant"
        );
        assert_eq!(
            super::loggable_oauth_error_code(Some("token=secret")),
            "unknown"
        );
        assert_eq!(super::loggable_oauth_error_code(None), "unknown");
    }

    #[tokio::test]
    async fn structured_token_errors_do_not_log_backend_secrets() -> anyhow::Result<()> {
        use tracing::instrument::WithSubscriber;
        let issuer = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::path("/oauth/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": "secret-code", "error_description": "secret-description"
                })),
            )
            .expect(1)
            .mount(&issuer)
            .await;
        let logs = tempfile::NamedTempFile::new()?;
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(logs.reopen()?)
            .finish();
        let pkce = crate::pkce::generate_pkce();
        let result = super::exchange_code_for_tokens(
            &issuer.uri(),
            "client",
            "http://localhost/auth/callback",
            &pkce,
            "secret-auth-code",
            &crate::test_support::transport_default_auth_route_config(),
        )
        .with_subscriber(subscriber)
        .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("non-success status must fail"),
        };
        assert!(error.to_string().contains("secret-description"));
        let output = std::fs::read_to_string(logs.path())?;
        assert!(output.contains("oauth token exchange returned non-success status"));
        assert!(output.contains("unknown"));
        for secret in [
            "secret-code",
            "secret-description",
            "secret-auth-code",
            pkce.code_verifier.as_str(),
        ] {
            assert!(
                !output.contains(secret),
                "structured logs exposed a credential"
            );
        }
        issuer.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_and_commit_admission_have_one_ordering() {
        for cancel_first in [false, true] {
            let handle = super::ShutdownHandle {
                shutdown_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
                state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(super::LOGIN_PENDING)),
            };
            if cancel_first {
                handle.shutdown();
                assert!(!handle.begin_commit());
                tokio::time::timeout(std::time::Duration::from_secs(1), handle.cancelled())
                    .await
                    .unwrap();
                assert_eq!(
                    handle.state.load(std::sync::atomic::Ordering::SeqCst),
                    super::LOGIN_CANCELLED
                );
            } else {
                assert!(handle.begin_commit());
                handle.shutdown();
                assert_eq!(
                    handle.state.load(std::sync::atomic::Ordering::SeqCst),
                    super::LOGIN_COMMITTING
                );
                assert!(!handle.begin_commit());
            }
        }
    }

    use super::TokenEndpointErrorDetail;
    use super::html_escape;
    use super::is_missing_codex_entitlement_error;
    use super::parse_token_endpoint_error;
    use super::redact_sensitive_query_value;
    use super::redact_sensitive_url_parts;
    use super::render_login_error_page;
    use super::sanitize_url_for_logging;

    #[test]
    fn parse_token_endpoint_error_prefers_error_description() {
        let detail = parse_token_endpoint_error(
            r#"{"error":"invalid_grant","error_description":"refresh token expired"}"#,
        );

        assert_eq!(
            detail,
            TokenEndpointErrorDetail {
                error_code: Some("invalid_grant".to_string()),
                error_message: Some("refresh token expired".to_string()),
                display_message: "refresh token expired".to_string(),
            }
        );
    }

    #[test]
    fn parse_token_endpoint_error_reads_nested_error_message_and_code() {
        let detail = parse_token_endpoint_error(
            r#"{"error":{"code":"proxy_auth_required","message":"proxy authentication required"}}"#,
        );

        assert_eq!(
            detail,
            TokenEndpointErrorDetail {
                error_code: Some("proxy_auth_required".to_string()),
                error_message: Some("proxy authentication required".to_string()),
                display_message: "proxy authentication required".to_string(),
            }
        );
    }

    #[test]
    fn parse_token_endpoint_error_falls_back_to_error_code() {
        let detail = parse_token_endpoint_error(r#"{"error":"temporarily_unavailable"}"#);

        assert_eq!(
            detail,
            TokenEndpointErrorDetail {
                error_code: Some("temporarily_unavailable".to_string()),
                error_message: None,
                display_message: "temporarily_unavailable".to_string(),
            }
        );
    }

    #[test]
    fn parse_token_endpoint_error_preserves_plain_text_for_display() {
        let detail = parse_token_endpoint_error("service unavailable");

        assert_eq!(
            detail,
            TokenEndpointErrorDetail {
                error_code: None,
                error_message: None,
                display_message: "service unavailable".to_string(),
            }
        );
    }

    #[test]
    fn redact_sensitive_query_value_only_scrubs_known_keys() {
        assert_eq!(
            redact_sensitive_query_value("code", "abc123"),
            "<redacted>".to_string()
        );
        assert_eq!(
            redact_sensitive_query_value("redirect_uri", "http://localhost:1455/auth/callback"),
            "http://localhost:1455/auth/callback".to_string()
        );
    }

    #[test]
    fn redact_sensitive_url_parts_preserves_safe_url_shape() {
        let mut url = url::Url::parse(
            "https://user:pass@auth.openai.com/oauth/token?code=abc123&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback#frag",
        )
        .expect("valid url");

        redact_sensitive_url_parts(&mut url);

        assert_eq!(
            url.as_str(),
            "https://auth.openai.com/oauth/token?code=%3Credacted%3E&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"
        );
    }

    #[test]
    fn sanitize_url_for_logging_redacts_sensitive_issuer_parts() {
        let redacted =
            sanitize_url_for_logging("https://user:pass@example.com/base?token=abc123&env=prod");

        assert_eq!(
            redacted,
            "https://example.com/base?token=%3Credacted%3E&env=prod".to_string()
        );
    }

    #[test]
    fn render_login_error_page_escapes_dynamic_fields() {
        let body = String::from_utf8(render_login_error_page(
            "<bad>",
            Some("code&value"),
            Some("\"quoted\""),
        ))
        .expect("login error page should be utf-8");

        assert!(body.contains(&html_escape("Sign-in could not be completed")));
        assert!(body.contains("&lt;bad&gt;"));
        assert!(body.contains("code&amp;value"));
        assert!(body.contains("&quot;quoted&quot;"));
    }

    #[test]
    fn render_login_error_page_uses_entitlement_copy() {
        let error_description = Some("missing_codex_entitlement");
        assert!(is_missing_codex_entitlement_error(
            "access_denied",
            error_description
        ));

        let body = String::from_utf8(render_login_error_page(
            "access denied",
            Some("access_denied"),
            error_description,
        ))
        .expect("login error page should be utf-8");

        assert!(body.contains("You do not have access to Codex"));
        assert!(body.contains("Contact your workspace administrator"));
        assert!(!body.contains("missing_codex_entitlement"));
    }
}
