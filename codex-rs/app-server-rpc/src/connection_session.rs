use std::collections::HashSet;
use std::sync::Arc;
use std::sync::OnceLock;

use crate::connection_rpc_gate::ConnectionRpcGate;

#[derive(Debug)]
pub struct ConnectionSessionState {
    pub rpc_gate: Arc<ConnectionRpcGate>,
    initialized: OnceLock<InitializedConnectionSessionState>,
}

#[derive(Debug)]
pub struct InitializedConnectionSessionState {
    pub experimental_api_enabled: bool,
    pub opted_out_notification_methods: HashSet<String>,
    pub app_server_client_name: String,
    pub client_version: String,
    pub request_attestation: bool,
    pub supports_openai_form_elicitation: bool,
}

impl Default for ConnectionSessionState {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionSessionState {
    pub fn new() -> Self {
        Self {
            rpc_gate: Arc::new(ConnectionRpcGate::new()),
            initialized: OnceLock::new(),
        }
    }

    pub fn initialized(&self) -> bool {
        self.initialized.get().is_some()
    }

    pub fn experimental_api_enabled(&self) -> bool {
        self.initialized
            .get()
            .is_some_and(|session| session.experimental_api_enabled)
    }

    pub fn opted_out_notification_methods(&self) -> HashSet<String> {
        self.initialized
            .get()
            .map(|session| session.opted_out_notification_methods.clone())
            .unwrap_or_default()
    }

    pub fn app_server_client_name(&self) -> Option<&str> {
        self.initialized
            .get()
            .map(|session| session.app_server_client_name.as_str())
    }

    pub fn client_version(&self) -> Option<&str> {
        self.initialized
            .get()
            .map(|session| session.client_version.as_str())
    }

    pub fn request_attestation(&self) -> bool {
        self.initialized
            .get()
            .is_some_and(|session| session.request_attestation)
    }

    pub fn supports_openai_form_elicitation(&self) -> bool {
        self.initialized
            .get()
            .is_some_and(|session| session.supports_openai_form_elicitation)
    }

    pub fn initialize(&self, session: InitializedConnectionSessionState) -> Result<(), ()> {
        self.initialized.set(session).map_err(|_| ())
    }
}
