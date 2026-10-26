use super::pairing_unavailable_error;
use super::protocol::RemoteControlPairingStatusRequest;
use super::protocol::RemoteControlTarget;
use super::protocol::StartRemoteControlPairingRequest;
use super::protocol::StartRemoteControlPairingResponse;
use axum::http::HeaderMap;
use codex_app_server_protocol::RemoteControlPairingStartResponse;
use codex_app_server_protocol::RemoteControlPairingStatusResponse;
use codex_http_client::HttpClient;
use codex_state::RemoteControlEnrollmentRecord;
use codex_state::StateRuntime;
use std::io;
use std::io::ErrorKind;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::info;
use tracing::warn;

const REMOTE_CONTROL_PAIRING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const REMOTE_CONTROL_RESPONSE_BODY_MAX_BYTES: usize = 4096;
const REMOTE_CONTROL_RESPONSE_READ_MAX_BYTES: usize = 1024 * 1024;
const REMOTE_CONTROL_FAILURE_BODY_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(100);
const REMOTE_CONTROL_SERVER_TOKEN_REFRESH_SKEW_SECS: i64 = 5 * 60;

const REQUEST_ID_HEADER: &str = "x-request-id";
const OAI_REQUEST_ID_HEADER: &str = "x-oai-request-id";
const CF_RAY_HEADER: &str = "cf-ray";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RemoteControlEnrollment {
    pub(super) remote_control_target: RemoteControlTarget,
    pub(super) account_id: String,
    pub(super) environment_id: String,
    pub(super) server_id: String,
    pub(super) server_name: String,
    pub(super) remote_control_token: Option<String>,
    pub(super) expires_at: Option<OffsetDateTime>,
    pub(super) next_refresh_at: Option<OffsetDateTime>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RemoteControlServerTokenRefreshRequirement {
    Required,
    Proactive,
    NotNeeded,
}

impl RemoteControlEnrollment {
    pub(super) fn from_persisted(
        target: &RemoteControlTarget,
        record: RemoteControlEnrollmentRecord,
    ) -> Self {
        Self {
            remote_control_target: target.clone(),
            account_id: record.account_id,
            environment_id: record.environment_id,
            server_id: record.server_id,
            server_name: record.server_name,
            remote_control_token: None,
            expires_at: None,
            next_refresh_at: None,
        }
    }


    pub(super) async fn start_pairing_with_client(
        &self,
        client: &HttpClient,
        request: StartRemoteControlPairingRequest,
    ) -> io::Result<RemoteControlPairingStartResponse> {
        if self.server_token_refresh_requirement()
            == RemoteControlServerTokenRefreshRequirement::Required
        {
            return Err(pairing_unavailable_error());
        }
        let remote_control_token = self
            .remote_control_token
            .as_deref()
            .ok_or_else(pairing_unavailable_error)?;

        let response = client
            .post(&self.remote_control_target.pair_url)
            .timeout(REMOTE_CONTROL_PAIRING_TIMEOUT)
            .bearer_auth(remote_control_token)
            .json(&request)
            .send()
            .await
            .map_err(|err| {
                io::Error::other(format!(
                    "failed to start remote control pairing at `{}`: {err}",
                    self.remote_control_target.pair_url
                ))
            })?;
        let headers = response.headers().clone();
        let status = response.status();
        let body = read_remote_control_response_body_for_status(response)
            .await
            .map_err(|err| {
                io::Error::other(format!(
                    "failed to read remote control pairing response from `{}`: {err}",
                    self.remote_control_target.pair_url
                ))
            })?;
        let body_preview = || preview_remote_control_response_body(&body);
        if !status.is_success() {
            let error_kind = match status.as_u16() {
                401 | 403 => ErrorKind::PermissionDenied,
                404 => ErrorKind::NotFound,
                _ => ErrorKind::Other,
            };
            return Err(io::Error::new(
                error_kind,
                format!(
                    "remote control pairing failed at `{}`: HTTP {status}, {}, body: {}",
                    self.remote_control_target.pair_url,
                    format_headers(&headers),
                    body_preview()
                ),
            ));
        }

        let pairing = serde_json::from_slice::<StartRemoteControlPairingResponse>(&body).map_err(
            |err| {
                let err = format_remote_control_decode_error(&err);
                io::Error::other(format!(
                    "failed to parse remote control pairing response from `{}`: HTTP {status}, {}, body: {}, decode error: {err}",
                    self.remote_control_target.pair_url,
                    format_headers(&headers),
                    body_preview()
                ))
            },
        )?;
        let StartRemoteControlPairingResponse {
            pairing_code,
            manual_pairing_code,
            server_id,
            environment_id,
            expires_at,
        } = pairing;
        if server_id != self.server_id || environment_id != self.environment_id {
            return Err(io::Error::other(format!(
                "remote control pairing returned mismatched enrollment: expected server_id={}, environment_id={}; got server_id={}, environment_id={}",
                self.server_id, self.environment_id, server_id, environment_id
            )));
        }
        let expires_at = OffsetDateTime::parse(&expires_at, &Rfc3339)
            .map_err(|err| {
                io::Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "failed to parse remote control pairing response from `{}`: HTTP {status}, {}, body: {}, expires_at parse error: {err}",
                        self.remote_control_target.pair_url,
                        format_headers(&headers),
                        body_preview()
                    ),
                )
            })?
            .unix_timestamp();

        Ok(RemoteControlPairingStartResponse {
            pairing_code,
            manual_pairing_code,
            environment_id,
            expires_at,
        })
    }


    pub(super) async fn pairing_status_with_client(
        &self,
        client: &HttpClient,
        request: RemoteControlPairingStatusRequest,
    ) -> io::Result<RemoteControlPairingStatusResponse> {
        if self.server_token_refresh_requirement()
            == RemoteControlServerTokenRefreshRequirement::Required
        {
            return Err(pairing_unavailable_error());
        }
        let remote_control_token = self
            .remote_control_token
            .as_deref()
            .ok_or_else(pairing_unavailable_error)?;

        let response = client
            .post(&self.remote_control_target.pair_status_url)
            .timeout(REMOTE_CONTROL_PAIRING_TIMEOUT)
            .bearer_auth(remote_control_token)
            .json(&request)
            .send()
            .await
            .map_err(|err| {
                io::Error::other(format!(
                    "failed to check remote control pairing status at `{}`: {err}",
                    self.remote_control_target.pair_status_url
                ))
            })?;
        let headers = response.headers().clone();
        let status = response.status();
        let body = read_remote_control_response_body_for_status(response)
            .await
            .map_err(|err| {
                io::Error::other(format!(
                    "failed to read remote control pairing status response from `{}`: {err}",
                    self.remote_control_target.pair_status_url
                ))
            })?;
        let body_preview = || preview_remote_control_response_body(&body);
        if !status.is_success() {
            let error_kind = match status.as_u16() {
                401 | 403 => ErrorKind::PermissionDenied,
                404 | 410 => ErrorKind::InvalidInput,
                _ => ErrorKind::Other,
            };
            return Err(io::Error::new(
                error_kind,
                format!(
                    "remote control pairing status failed at `{}`: HTTP {status}, {}, body: {}",
                    self.remote_control_target.pair_status_url,
                    format_headers(&headers),
                    body_preview()
                ),
            ));
        }

        let response = serde_json::from_slice::<RemoteControlPairingStatusResponse>(&body)
            .map_err(|err| {
                let err = format_remote_control_decode_error(&err);
                io::Error::other(format!(
                    "failed to parse remote control pairing status response from `{}`: HTTP {status}, {}, body: {}, decode error: {err}",
                    self.remote_control_target.pair_status_url,
                    format_headers(&headers),
                    body_preview()
                ))
            })?;
        Ok(response)
    }

    pub(super) fn server_token_refresh_requirement(
        &self,
    ) -> RemoteControlServerTokenRefreshRequirement {
        self.server_token_refresh_requirement_at(OffsetDateTime::now_utc())
    }

    pub(super) fn should_refresh_server_token(&self) -> bool {
        // Critical-path consumers can already use a proactive-window token.
        // Do not race a potentially rotating refresh against that bearer; defer
        // refresh until expiry/missing credentials or an actual auth rejection.
        self.server_token_refresh_requirement()
            == RemoteControlServerTokenRefreshRequirement::Required
    }

    pub(super) fn server_token_refresh_requirement_at(
        &self,
        now: OffsetDateTime,
    ) -> RemoteControlServerTokenRefreshRequirement {
        let Some(expires_at) = self.remote_control_token.as_ref().and(self.expires_at) else {
            return RemoteControlServerTokenRefreshRequirement::Required;
        };
        if expires_at <= now {
            return RemoteControlServerTokenRefreshRequirement::Required;
        }
        if expires_at > now + time::Duration::seconds(REMOTE_CONTROL_SERVER_TOKEN_REFRESH_SKEW_SECS)
            || self
                .next_refresh_at
                .is_some_and(|next_refresh_at| next_refresh_at > now)
        {
            return RemoteControlServerTokenRefreshRequirement::NotNeeded;
        }
        RemoteControlServerTokenRefreshRequirement::Proactive
    }

    pub(super) fn clear_server_token(&mut self) {
        self.remote_control_token = None;
        self.expires_at = None;
    }
}

pub(super) async fn load_persisted_remote_control_enrollment(
    state_db: Option<&StateRuntime>,
    remote_control_target: &RemoteControlTarget,
    account_id: &str,
    app_server_client_name: Option<&str>,
) -> io::Result<Option<RemoteControlEnrollment>> {
    let Some(state_db) = state_db else {
        return Err(io::Error::new(
            ErrorKind::NotFound,
            format!(
                "remote control enrollment cache unavailable because sqlite state db is disabled: websocket_url={}, account_id={}, app_server_client_name={:?}",
                remote_control_target.websocket_url, account_id, app_server_client_name
            ),
        ));
    };
    let enrollment = match state_db
        .get_remote_control_enrollment(
            &remote_control_target.websocket_url,
            account_id,
            app_server_client_name,
        )
        .await
    {
        Ok(enrollment) => enrollment,
        Err(err) => {
            warn!(
                "failed to load persisted remote control enrollment: websocket_url={}, account_id={}, app_server_client_name={:?}, err={err}",
                remote_control_target.websocket_url, account_id, app_server_client_name
            );
            return Err(io::Error::other(err));
        }
    };

    match enrollment {
        Some(enrollment) => {
            info!(
                "reusing persisted remote control enrollment: websocket_url={}, account_id={}, app_server_client_name={:?}, server_id={}, environment_id={}",
                remote_control_target.websocket_url,
                account_id,
                app_server_client_name,
                enrollment.server_id,
                enrollment.environment_id
            );
            Ok(Some(RemoteControlEnrollment::from_persisted(
                remote_control_target,
                enrollment,
            )))
        }
        None => {
            info!(
                "no persisted remote control enrollment found: websocket_url={}, account_id={}, app_server_client_name={:?}",
                remote_control_target.websocket_url, account_id, app_server_client_name
            );
            Ok(None)
        }
    }
}

pub(super) async fn update_persisted_remote_control_enrollment(
    state_db: Option<&StateRuntime>,
    remote_control_target: &RemoteControlTarget,
    account_id: &str,
    app_server_client_name: Option<&str>,
    enrollment: Option<&RemoteControlEnrollment>,
    remote_control_enabled: Option<bool>,
) -> io::Result<()> {
    let Some(state_db) = state_db else {
        return Err(io::Error::new(
            ErrorKind::NotFound,
            format!(
                "remote control enrollment persistence unavailable because sqlite state db is disabled: websocket_url={}, account_id={}, app_server_client_name={:?}, has_enrollment={}",
                remote_control_target.websocket_url,
                account_id,
                app_server_client_name,
                enrollment.is_some()
            ),
        ));
    };
    if let &Some(enrollment) = &enrollment
        && enrollment.account_id != account_id
    {
        return Err(io::Error::other(format!(
            "enrollment account_id does not match expected account_id `{account_id}`"
        )));
    }

    if let Some(enrollment) = enrollment {
        state_db
            .upsert_remote_control_enrollment(&RemoteControlEnrollmentRecord {
                websocket_url: remote_control_target.websocket_url.clone(),
                account_id: account_id.to_string(),
                app_server_client_name: app_server_client_name.map(str::to_string),
                server_id: enrollment.server_id.clone(),
                environment_id: enrollment.environment_id.clone(),
                server_name: enrollment.server_name.clone(),
                remote_control_enabled,
            })
            .await
            .map_err(io::Error::other)?;
        info!(
            "persisted remote control enrollment: websocket_url={}, account_id={}, app_server_client_name={:?}, server_id={}, environment_id={}",
            remote_control_target.websocket_url,
            account_id,
            app_server_client_name,
            enrollment.server_id,
            enrollment.environment_id
        );
        Ok(())
    } else {
        let rows_affected = state_db
            .delete_remote_control_enrollment(
                &remote_control_target.websocket_url,
                account_id,
                app_server_client_name,
            )
            .await
            .map_err(io::Error::other)?;
        info!(
            "cleared persisted remote control enrollment: websocket_url={}, account_id={}, app_server_client_name={:?}, rows_affected={rows_affected}",
            remote_control_target.websocket_url, account_id, app_server_client_name
        );
        Ok(())
    }
}

// Error headers determine recovery. Diagnostics must neither delay it for the
// request's entire budget nor replace its status with an unrelated body error.
pub(super) async fn read_remote_control_response_body_for_status(
    mut response: codex_http_client::HttpResponse,
) -> io::Result<Vec<u8>> {
    if response.status().is_success() {
        return read_remote_control_response_body(response).await;
    }
    let diagnostics = async {
        let mut body = Vec::new();
        while let Ok(Some(chunk)) = response.chunk().await {
            if chunk.len() > REMOTE_CONTROL_RESPONSE_BODY_MAX_BYTES - body.len() {
                return Vec::new();
            }
            body.extend_from_slice(&chunk);
        }
        // Previewing incomplete JSON is deliberately omitted by the redactor.
        body
    };
    Ok(tokio::time::timeout(REMOTE_CONTROL_FAILURE_BODY_TIMEOUT, diagnostics)
        .await
        .unwrap_or_default())
}

pub(super) async fn read_remote_control_response_body(
    mut response: codex_http_client::HttpResponse,
) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|err| {
        io::Error::new(
            if err.is_timeout() {
                ErrorKind::TimedOut
            } else {
                ErrorKind::Other
            },
            err,
        )
    })? {
        if chunk.len() > REMOTE_CONTROL_RESPONSE_READ_MAX_BYTES - body.len() {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "remote control response exceeds 1 MiB limit",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub(super) fn format_remote_control_decode_error(err: &serde_json::Error) -> String {
    // Serde's display text can echo a credential supplied as an unexpected value.
    format!(
        "{:?} at line {} column {}",
        err.classify(),
        err.line(),
        err.column()
    )
}

pub(crate) fn preview_remote_control_response_body(body: &[u8]) -> String {
    let body = String::from_utf8_lossy(body);
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "<empty>".to_string();
    }
    let redacted = redact_remote_control_response_body(trimmed);
    if redacted.len() <= REMOTE_CONTROL_RESPONSE_BODY_MAX_BYTES {
        return redacted;
    }

    let mut cut = REMOTE_CONTROL_RESPONSE_BODY_MAX_BYTES;
    while !redacted.is_char_boundary(cut) {
        cut = cut.saturating_sub(1);
    }
    let mut truncated = redacted[..cut].to_string();
    truncated.push_str("...");
    truncated
}

fn redact_remote_control_response_body(body: &str) -> String {
    let Ok(mut body_json) = serde_json::from_str::<serde_json::Value>(body) else {
        return "<omitted non-JSON response body>".to_string();
    };
    fn redact(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(object) => {
                for (key, value) in object {
                    if matches!(
                        key.as_str(),
                        "remote_control_token" | "pairing_code" | "manual_pairing_code"
                    ) {
                        *value = serde_json::Value::String("<redacted>".to_string());
                    } else {
                        redact(value);
                    }
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    redact(value);
                }
            }
            _ => {}
        }
    }
    if !body_json.is_object() && !body_json.is_array() {
        return "<omitted scalar response body>".to_string();
    }
    redact(&mut body_json);
    body_json.to_string()
}

pub(crate) fn format_headers(headers: &HeaderMap) -> String {
    let request_id_str = headers
        .get(REQUEST_ID_HEADER)
        .or_else(|| headers.get(OAI_REQUEST_ID_HEADER))
        .map(|value| value.to_str().unwrap_or("<invalid utf-8>").to_owned())
        .unwrap_or_else(|| "<none>".to_owned());
    let cf_ray_str = headers
        .get(CF_RAY_HEADER)
        .map(|value| value.to_str().unwrap_or("<invalid utf-8>").to_owned())
        .unwrap_or_else(|| "<none>".to_owned());
    format!("request-id: {request_id_str}, cf-ray: {cf_ray_str}")
}

