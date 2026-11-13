use crate::outgoing_message::OutgoingMessage;
use codex_app_server_protocol_types::JSONRPCMessage;
use serde::Deserialize;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io;
use std::io::ErrorKind;
use url::Host;
use url::Url;

// Desktop rejects the workspace's 0.0.0 development marker for remote control.
// Remote clients also gate project discovery on 0.148.0-alpha.21. The project
// APIs are implemented here; advertising the older v3 floor hides them. Keep
// this below the paginated-history floor and do not relabel local/model traffic.
const DEVELOPMENT_COMPATIBILITY_VERSION: &str = "0.148.0-alpha.21+kd4.remote-control-v3";

pub(super) fn remote_control_app_server_version() -> &'static str {
    match codex_utils_build_info::CODEX_VERSION {
        "0.0.0" => DEVELOPMENT_COMPATIBILITY_VERSION,
        version => version,
    }
}

/// Keep enrollment, relay headers, and remote initialize replies on one version.
pub fn remote_control_user_agent() -> String {
    codex_login::default_client::get_codex_user_agent_with_version(
        remote_control_app_server_version(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RemoteControlTarget {
    pub(super) websocket_url: String,
    pub(super) enroll_url: String,
    pub(super) refresh_url: String,
    pub(super) pair_url: String,
    pub(super) pair_status_url: String,
}

#[derive(Debug, Serialize)]
pub(super) struct EnrollRemoteServerRequest {
    pub(super) name: String,
    pub(super) os: &'static str,
    pub(super) arch: &'static str,
    pub(super) app_server_version: &'static str,
    pub(super) installation_id: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct EnrollRemoteServerResponse {
    pub(super) server_id: String,
    pub(super) environment_id: String,
    pub(super) remote_control_token: String,
    pub(super) expires_at: String,
}

#[derive(Debug, Serialize)]
pub(super) struct RefreshRemoteServerRequest {
    pub(super) server_id: String,
    pub(super) installation_id: String,
}

#[derive(Debug, Serialize)]
pub(super) struct StartRemoteControlPairingRequest {
    pub(super) manual_code: bool,
}

#[derive(Debug, Deserialize)]
pub(super) struct StartRemoteControlPairingResponse {
    pub(super) pairing_code: String,
    pub(super) manual_pairing_code: Option<String>,
    pub(super) server_id: String,
    pub(super) environment_id: String,
    pub(super) expires_at: String,
}

#[derive(Debug, Serialize)]
pub(super) struct RemoteControlPairingStatusRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pairing_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) manual_pairing_code: Option<String>,
}

#[derive(Clone)]
pub(super) enum RemoteControlPairingStatusCode {
    PairingCode(String),
    ManualPairingCode(String),
}

impl From<RemoteControlPairingStatusCode> for RemoteControlPairingStatusRequest {
    fn from(code: RemoteControlPairingStatusCode) -> Self {
        match code {
            RemoteControlPairingStatusCode::PairingCode(pairing_code) => Self {
                pairing_code: Some(pairing_code),
                manual_pairing_code: None,
            },
            RemoteControlPairingStatusCode::ManualPairingCode(manual_pairing_code) => Self {
                pairing_code: None,
                manual_pairing_code: Some(manual_pairing_code),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StreamId(pub String);

/// Opt-in, content-free custody observations. A relay ACK is not phone UI proof.
#[derive(Clone)]
pub(super) struct DeliveryObservation {
    stream_key: String,
    seq_id: u64,
    segment_id: Option<usize>,
}

impl DeliveryObservation {
    pub(super) fn new(client: &ClientId, stream: &StreamId, seq_id: u64, segment_id: Option<usize>) -> Option<Self> {
        if !codex_protocol::timing::observations::enabled() { return None; }
        let bytes = serde_json::to_vec(&(client, stream)).ok()?;
        Some(Self { stream_key: Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect(), seq_id, segment_id })
    }

    pub(super) fn record(&self, kind: &str, mut facts: serde_json::Value) {
        let Some(data) = facts.as_object_mut() else { return; };
        data.insert("stream_key_sha256".into(), self.stream_key.clone().into());
        data.insert("seq_id".into(), self.seq_id.into());
        data.insert("segment_id".into(), self.segment_id.into());
        let _ = codex_protocol::timing::observations::record(kind, "", "", "", facts);
    }
}

pub(super) fn response_observation(message: &OutgoingMessage) -> serde_json::Value {
    let (id, error_code) = match message {
        OutgoingMessage::Response(response) => (&response.id, None),
        OutgoingMessage::Error(error) => (&error.id, Some(error.error.code)),
        _ => return serde_json::json!({"message_kind":"other"}),
    };
    let key = serde_json::to_vec(id).ok().map(|bytes| Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect::<String>());
    serde_json::json!({"message_kind":if error_code.is_some() { "error" } else { "response" },
        "request_key_sha256":key, "error_code":error_code})
}

impl StreamId {
    pub fn new_random() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientEvent {
    ClientMessage {
        message: JSONRPCMessage,
    },
    ClientMessageChunk {
        segment_id: usize,
        segment_count: usize,
        message_size_bytes: usize,
        message_chunk_base64: String,
    },
    /// Backend-generated acknowledgement for all server envelopes addressed to
    /// `client_id` and `stream_id` whose envelope `seq_id` is less than or equal
    /// to this ack's `seq_id`. Chunk acknowledgements carry `segment_id` so the
    /// sender can retain only the still-unacked wire chunks on reconnect.
    Ack {
        #[serde(skip_serializing_if = "Option::is_none")]
        segment_id: Option<usize>,
    },
    Ping,
    ClientClosed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct ClientEnvelope {
    #[serde(flatten)]
    pub(crate) event: ClientEvent,
    #[serde(rename = "client_id")]
    pub(crate) client_id: ClientId,
    #[serde(rename = "stream_id", skip_serializing_if = "Option::is_none")]
    pub(crate) stream_id: Option<StreamId>,
    /// For `Ack`, this is the backend-generated per-stream cursor over
    /// `ServerEnvelope.seq_id`.
    #[serde(rename = "seq_id", skip_serializing_if = "Option::is_none")]
    pub(crate) seq_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PongStatus {
    Active,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    ServerMessage {
        message: Box<OutgoingMessage>,
    },
    ServerMessageChunk {
        segment_id: usize,
        segment_count: usize,
        message_size_bytes: usize,
        message_chunk_base64: String,
    },
    Pong {
        status: PongStatus,
    },
}

impl ServerEvent {
    pub(crate) fn segment_id(&self) -> Option<usize> {
        match self {
            Self::ServerMessageChunk { segment_id, .. } => Some(*segment_id),
            Self::ServerMessage { .. } | Self::Pong { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct ServerEnvelope {
    #[serde(flatten)]
    pub(crate) event: ServerEvent,
    #[serde(rename = "client_id")]
    pub(crate) client_id: ClientId,
    #[serde(rename = "stream_id")]
    pub(crate) stream_id: StreamId,
    #[serde(rename = "seq_id")]
    pub(crate) seq_id: u64,
}

fn is_allowed_remote_control_chatgpt_host(host: &Option<Host<&str>>) -> bool {
    let Some(Host::Domain(host)) = *host else {
        return false;
    };
    host == "chatgpt.com"
        || host == "chatgpt-staging.com"
        || host.ends_with(".chatgpt.com")
        || host.ends_with(".chatgpt-staging.com")
}

fn is_localhost(host: &Option<Host<&str>>) -> bool {
    match host {
        Some(Host::Domain("localhost")) => true,
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    }
}

pub(super) fn normalize_remote_control_url(
    remote_control_url: &str,
) -> io::Result<RemoteControlTarget> {
    let remote_control_url = normalize_remote_control_base_url(remote_control_url)?;
    let map_url_parse_error = |err: url::ParseError| -> io::Error {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("invalid remote control URL (details withheld): {err}"),
        )
    };

    let enroll_url = remote_control_url
        .join("wham/remote/control/server/enroll")
        .map_err(map_url_parse_error)?;
    let refresh_url = remote_control_url
        .join("wham/remote/control/server/refresh")
        .map_err(map_url_parse_error)?;
    let pair_url = remote_control_url
        .join("wham/remote/control/server/pair")
        .map_err(map_url_parse_error)?;
    let pair_status_url = remote_control_url
        .join("wham/remote/control/server/pair/status")
        .map_err(map_url_parse_error)?;
    let mut websocket_url = remote_control_url
        .join("wham/remote/control/server")
        .map_err(map_url_parse_error)?;
    websocket_url
        .set_scheme(if enroll_url.scheme() == "https" {
            "wss"
        } else {
            "ws"
        })
        .map_err(|()| {
            io::Error::new(
                ErrorKind::InvalidInput,
                format!("invalid remote control URL `{remote_control_url}`"),
            )
        })?;

    Ok(RemoteControlTarget {
        websocket_url: websocket_url.to_string(),
        enroll_url: enroll_url.to_string(),
        refresh_url: refresh_url.to_string(),
        pair_url: pair_url.to_string(),
        pair_status_url: pair_status_url.to_string(),
    })
}

pub(super) fn normalize_remote_control_base_url(remote_control_url: &str) -> io::Result<Url> {
    let map_url_parse_error = |err: url::ParseError| -> io::Error {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("invalid remote control URL (details withheld): {err}"),
        )
    };
    let map_scheme_error = |_: ()| -> io::Error {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "invalid remote control URL; expected HTTPS URL for chatgpt.com or chatgpt-staging.com, or HTTP/HTTPS URL for localhost"
            ),
        )
    };

    let mut remote_control_url = Url::parse(remote_control_url).map_err(map_url_parse_error)?;
    if !remote_control_url.path().ends_with('/') {
        let normalized_path = format!("{}/", remote_control_url.path());
        remote_control_url.set_path(&normalized_path);
    }

    let host = remote_control_url.host();
    match remote_control_url.scheme() {
        "https" if is_localhost(&host) || is_allowed_remote_control_chatgpt_host(&host) => {}
        "http" if is_localhost(&host) => {}
        _ => return Err(map_scheme_error(())),
    }

    Ok(remote_control_url)
}

