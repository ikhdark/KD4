use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::fuzzy_file_search::FuzzyFileSearchSession;
use crate::fuzzy_file_search::run_fuzzy_file_search;
use crate::fuzzy_file_search::start_fuzzy_file_search_session;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingMessageSender;
use codex_app_server_protocol::FuzzyFileSearchParams;
use codex_app_server_protocol::FuzzyFileSearchResponse;
use codex_app_server_protocol::FuzzyFileSearchSessionStartParams;
use codex_app_server_protocol::FuzzyFileSearchSessionStartResponse;
use codex_app_server_protocol::FuzzyFileSearchSessionStopParams;
use codex_app_server_protocol::FuzzyFileSearchSessionStopResponse;
use codex_app_server_protocol::FuzzyFileSearchSessionUpdateParams;
use codex_app_server_protocol::FuzzyFileSearchSessionUpdateResponse;
use codex_app_server_protocol::JSONRPCErrorError;
use tokio::sync::Mutex;

/// Cancellation flags of in-flight fuzzy searches, keyed by connection and
/// cancellation token.
type PendingFuzzySearches = Arc<StdMutex<HashMap<(ConnectionId, String), Arc<AtomicBool>>>>;

#[derive(Clone)]
pub(crate) struct SearchRequestProcessor {
    outgoing: Arc<OutgoingMessageSender>,
    pending_fuzzy_searches: PendingFuzzySearches,
    // Sessions are owned by the connection that started them, like fs watches.
    fuzzy_search_sessions: Arc<Mutex<HashMap<(ConnectionId, String), FuzzyFileSearchSession>>>,
}

struct PendingFuzzySearch {
    searches: PendingFuzzySearches,
    token: Option<(ConnectionId, String)>,
    flag: Arc<AtomicBool>,
}

impl Drop for PendingFuzzySearch {
    fn drop(&mut self) {
        self.flag.store(true, Ordering::Relaxed);
        if let Some(token) = &self.token {
            let mut searches = self
                .searches
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if searches
                .get(token)
                .is_some_and(|flag| Arc::ptr_eq(flag, &self.flag))
            {
                searches.remove(token);
            }
        }
    }
}

impl SearchRequestProcessor {
    pub(crate) fn new(outgoing: Arc<OutgoingMessageSender>) -> Self {
        Self {
            outgoing,
            pending_fuzzy_searches: Arc::new(StdMutex::new(HashMap::new())),
            fuzzy_search_sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) async fn fuzzy_file_search(
        &self,
        connection_id: ConnectionId,
        params: FuzzyFileSearchParams,
    ) -> Result<FuzzyFileSearchResponse, JSONRPCErrorError> {
        let FuzzyFileSearchParams {
            query,
            roots,
            cancellation_token,
        } = params;

        let cancellation_token = cancellation_token.map(|token| (connection_id, token));
        let cancel_flag = match cancellation_token.clone() {
            Some(token) => {
                let mut pending_fuzzy_searches = self
                    .pending_fuzzy_searches
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // if a cancellation_token is provided and a pending_request exists for
                // that token, cancel it
                if let Some(existing) = pending_fuzzy_searches.get(&token) {
                    existing.store(true, Ordering::Relaxed);
                }
                let flag = Arc::new(AtomicBool::new(false));
                pending_fuzzy_searches.insert(token, flag.clone());
                flag
            }
            None => Arc::new(AtomicBool::new(false)),
        };
        // The registry only needs synchronous map access. Keeping its cleanup
        // synchronous also cancels requests dropped outside a running executor.
        let _pending_search = PendingFuzzySearch {
            searches: Arc::clone(&self.pending_fuzzy_searches),
            token: cancellation_token,
            flag: Arc::clone(&cancel_flag),
        };

        run_fuzzy_file_search(query, roots, cancel_flag.clone())
            .await
            .map_err(|err| internal_error(format!("fuzzy file search failed: {err}")))
    }

    pub(crate) async fn fuzzy_file_search_session_start_response(
        &self,
        connection_id: ConnectionId,
        params: FuzzyFileSearchSessionStartParams,
    ) -> Result<FuzzyFileSearchSessionStartResponse, JSONRPCErrorError> {
        let FuzzyFileSearchSessionStartParams { session_id, roots } = params;
        if session_id.is_empty() {
            return Err(invalid_request("sessionId must not be empty"));
        }

        let session = start_fuzzy_file_search_session(
            connection_id,
            session_id.clone(),
            roots,
            self.outgoing.clone(),
        )
        .map_err(|err| {
            internal_error(format!("failed to start fuzzy file search session: {err}"))
        })?;
        self.fuzzy_search_sessions
            .lock()
            .await
            .insert((connection_id, session_id), session);
        Ok(FuzzyFileSearchSessionStartResponse {})
    }

    pub(crate) async fn fuzzy_file_search_session_update_response(
        &self,
        connection_id: ConnectionId,
        params: FuzzyFileSearchSessionUpdateParams,
    ) -> Result<FuzzyFileSearchSessionUpdateResponse, JSONRPCErrorError> {
        let FuzzyFileSearchSessionUpdateParams { session_id, query } = params;
        let found = {
            let sessions = self.fuzzy_search_sessions.lock().await;
            if let Some(session) = sessions.get(&(connection_id, session_id.clone())) {
                session.update_query(query);
                true
            } else {
                false
            }
        };
        if !found {
            return Err(invalid_request(format!(
                "fuzzy file search session not found: {session_id}"
            )));
        }

        Ok(FuzzyFileSearchSessionUpdateResponse {})
    }

    pub(crate) async fn fuzzy_file_search_session_stop(
        &self,
        connection_id: ConnectionId,
        params: FuzzyFileSearchSessionStopParams,
    ) -> Result<FuzzyFileSearchSessionStopResponse, JSONRPCErrorError> {
        let FuzzyFileSearchSessionStopParams { session_id } = params;
        self.fuzzy_search_sessions
            .lock()
            .await
            .remove(&(connection_id, session_id));

        Ok(FuzzyFileSearchSessionStopResponse {})
    }

    /// Stops every session the closed connection left running; each session
    /// otherwise keeps its search workers and file index for the server lifetime.
    pub(crate) async fn connection_closed(&self, connection_id: ConnectionId) {
        self.fuzzy_search_sessions
            .lock()
            .await
            .retain(|(owner, _), _| *owner != connection_id);
    }
}

