use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use codex_exec_server_protocol::JSONRPCErrorError;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::ExecServerRuntimePaths;
use crate::rpc::RpcNotificationSender;
use crate::rpc::invalid_request;
use crate::rpc::session_already_attached;
use crate::server::process_handler::ProcessHandler;
use crate::telemetry::ExecServerTelemetry;

const DETACHED_SESSION_TTL: Duration = Duration::from_secs(30);

pub(crate) struct SessionRegistry {
    sessions: Mutex<HashMap<String, Arc<SessionEntry>>>,
    telemetry: ExecServerTelemetry,
}

struct SessionEntry {
    session_id: String,
    process: ProcessHandler,
    attachment: StdMutex<AttachmentState>,
}

struct AttachmentState {
    current_connection_id: Option<ConnectionId>,
    detached_connection_id: Option<ConnectionId>,
    detached_expires_at: Option<tokio::time::Instant>,
    expiry_cancelled: Option<tokio_util::sync::CancellationToken>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ConnectionId(Uuid);

impl std::fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone)]
pub(crate) struct SessionHandle {
    registry: Arc<SessionRegistry>,
    entry: Arc<SessionEntry>,
    connection_id: ConnectionId,
}

impl SessionRegistry {
    pub(crate) fn new(telemetry: ExecServerTelemetry) -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            telemetry,
        })
    }

    pub(crate) async fn attach(
        self: &Arc<Self>,
        resume_session_id: Option<String>,
        notifications: RpcNotificationSender,
        runtime_paths: ExecServerRuntimePaths,
    ) -> Result<SessionHandle, JSONRPCErrorError> {
        enum AttachOutcome {
            Attached(Arc<SessionEntry>),
            Expired {
                session_id: String,
                entry: Arc<SessionEntry>,
            },
        }

        let connection_id = ConnectionId(Uuid::new_v4());
        let outcome = {
            let mut sessions = self.sessions.lock().await;
            if let Some(session_id) = resume_session_id {
                let entry = sessions
                    .get(&session_id)
                    .cloned()
                    .ok_or_else(|| invalid_request(format!("unknown session id {session_id}")))?;
                if entry.is_expired(tokio::time::Instant::now()) {
                    let entry = sessions.remove(&session_id).ok_or_else(|| {
                        invalid_request(format!("unknown session id {session_id}"))
                    })?;
                    Ok(AttachOutcome::Expired { session_id, entry })
                } else if entry.has_active_connection() {
                    Err(session_already_attached(format!(
                        "session {session_id} is already attached to another connection"
                    )))
                } else {
                    entry.process.set_notification_sender(Some(notifications));
                    entry.attach(connection_id);
                    Ok(AttachOutcome::Attached(entry))
                }
            } else {
                let session_id = Uuid::new_v4().to_string();
                let entry = Arc::new(SessionEntry::new(
                    session_id.clone(),
                    ProcessHandler::new(notifications, self.telemetry.clone(), runtime_paths),
                    connection_id,
                ));
                sessions.insert(session_id, Arc::clone(&entry));
                Ok(AttachOutcome::Attached(entry))
            }
        };
        let entry = match outcome? {
            AttachOutcome::Attached(entry) => entry,
            AttachOutcome::Expired { session_id, entry } => {
                entry.cancel_expiry();
                entry.process.shutdown().await;
                return Err(invalid_request(format!("unknown session id {session_id}")));
            }
        };

        Ok(SessionHandle {
            registry: Arc::clone(self),
            entry,
            connection_id,
        })
    }


    pub(crate) async fn shutdown(&self) {
        let sessions = std::mem::take(&mut *self.sessions.lock().await);
        for entry in sessions.into_values() {
            entry.cancel_expiry();
            entry.process.shutdown().await;
        }
    }

    async fn expire_if_detached(&self, session_id: String, connection_id: ConnectionId) {
        let removed = {
            let mut sessions = self.sessions.lock().await;
            let Some(entry) = sessions.get(&session_id) else {
                return;
            };
            if !entry.is_detached_connection_expired(connection_id, tokio::time::Instant::now()) {
                return;
            }
            sessions.remove(&session_id)
        };

        if let Some(entry) = removed {
            entry.process.shutdown().await;
        }
    }
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            telemetry: ExecServerTelemetry::default(),
        }
    }
}

impl SessionEntry {
    fn new(session_id: String, process: ProcessHandler, connection_id: ConnectionId) -> Self {
        Self {
            session_id,
            process,
            attachment: StdMutex::new(AttachmentState {
                current_connection_id: Some(connection_id),
                detached_connection_id: None,
                detached_expires_at: None,
                expiry_cancelled: None,
            }),
        }
    }

    fn attach(&self, connection_id: ConnectionId) {
        let mut attachment = self
            .attachment
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        attachment.current_connection_id = Some(connection_id);
        if let Some(cancelled) = attachment.expiry_cancelled.take() { cancelled.cancel(); }
        attachment.detached_connection_id = None;
        attachment.detached_expires_at = None;
    }

    fn detach(&self, connection_id: ConnectionId) -> Option<tokio_util::sync::CancellationToken> {
        let mut attachment = self
            .attachment
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if attachment.current_connection_id != Some(connection_id) {
            return None;
        }

        self.process.set_notification_sender(/*notifications*/ None);
        attachment.current_connection_id = None;
        attachment.detached_connection_id = Some(connection_id);
        attachment.detached_expires_at = Some(tokio::time::Instant::now() + DETACHED_SESSION_TTL);
        let cancelled = tokio_util::sync::CancellationToken::new();
        attachment.expiry_cancelled = Some(cancelled.clone());
        Some(cancelled)
    }

    fn cancel_expiry(&self) {
        if let Some(cancelled) = self.attachment.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner).expiry_cancelled.take()
        { cancelled.cancel(); }
    }

    fn has_active_connection(&self) -> bool {
        self.attachment
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current_connection_id
            .is_some()
    }

    fn is_attached_to(&self, connection_id: ConnectionId) -> bool {
        self.attachment
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current_connection_id
            == Some(connection_id)
    }

    fn is_expired(&self, now: tokio::time::Instant) -> bool {
        self.attachment
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .detached_expires_at
            .is_some_and(|deadline| now >= deadline)
    }

    fn is_detached_connection_expired(
        &self,
        connection_id: ConnectionId,
        now: tokio::time::Instant,
    ) -> bool {
        let attachment = self
            .attachment
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        attachment.current_connection_id.is_none()
            && attachment.detached_connection_id == Some(connection_id)
            && attachment
                .detached_expires_at
                .is_some_and(|deadline| now >= deadline)
    }
}

impl SessionHandle {
    pub(crate) fn session_id(&self) -> &str {
        &self.entry.session_id
    }

    pub(crate) fn connection_id(&self) -> String {
        self.connection_id.to_string()
    }

    pub(crate) fn is_session_attached(&self) -> bool {
        self.entry.is_attached_to(self.connection_id)
    }

    pub(crate) fn process(&self) -> &ProcessHandler {
        &self.entry.process
    }

    pub(crate) async fn detach(&self) {
        let Some(expiry_cancelled) = self.entry.detach(self.connection_id) else {
            return;
        };

        let registry = Arc::clone(&self.registry);
        let session_id = self.entry.session_id.clone();
        let connection_id = self.connection_id;
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = expiry_cancelled.cancelled() => return,
                _ = tokio::time::sleep(DETACHED_SESSION_TTL) => {},
            }
            // Once expiry claims the entry, native cleanup must finish even
            // if an observer disappears. Cancel only the obsolete timer wait.
            registry.expire_if_detached(session_id, connection_id).await;
        });
    }
}
