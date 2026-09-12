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

#[derive(Clone)]
pub(crate) struct SearchRequestProcessor {
    outgoing: Arc<OutgoingMessageSender>,
    pending_fuzzy_searches: Arc<StdMutex<HashMap<String, Arc<AtomicBool>>>>,
    fuzzy_search_sessions: Arc<Mutex<HashMap<String, FuzzyFileSearchSession>>>,
}

struct PendingFuzzySearch {
    searches: Arc<StdMutex<HashMap<String, Arc<AtomicBool>>>>,
    token: Option<String>,
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
        params: FuzzyFileSearchParams,
    ) -> Result<FuzzyFileSearchResponse, JSONRPCErrorError> {
        let FuzzyFileSearchParams {
            query,
            roots,
            cancellation_token,
        } = params;

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
                pending_fuzzy_searches.insert(token.clone(), flag.clone());
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

        let results = match query.as_str() {
            "" => vec![],
            _ => run_fuzzy_file_search(query, roots, cancel_flag.clone()).await,
        };

        Ok(FuzzyFileSearchResponse { files: results })
    }

    pub(crate) async fn fuzzy_file_search_session_start_response(
        &self,
        params: FuzzyFileSearchSessionStartParams,
    ) -> Result<FuzzyFileSearchSessionStartResponse, JSONRPCErrorError> {
        let FuzzyFileSearchSessionStartParams { session_id, roots } = params;
        if session_id.is_empty() {
            return Err(invalid_request("sessionId must not be empty"));
        }

        let session =
            start_fuzzy_file_search_session(session_id.clone(), roots, self.outgoing.clone())
                .map_err(|err| {
                    internal_error(format!("failed to start fuzzy file search session: {err}"))
                })?;
        self.fuzzy_search_sessions
            .lock()
            .await
            .insert(session_id, session);
        Ok(FuzzyFileSearchSessionStartResponse {})
    }

    pub(crate) async fn fuzzy_file_search_session_update_response(
        &self,
        params: FuzzyFileSearchSessionUpdateParams,
    ) -> Result<FuzzyFileSearchSessionUpdateResponse, JSONRPCErrorError> {
        let FuzzyFileSearchSessionUpdateParams { session_id, query } = params;
        let found = {
            let sessions = self.fuzzy_search_sessions.lock().await;
            if let Some(session) = sessions.get(&session_id) {
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
        params: FuzzyFileSearchSessionStopParams,
    ) -> Result<FuzzyFileSearchSessionStopResponse, JSONRPCErrorError> {
        let FuzzyFileSearchSessionStopParams { session_id } = params;
        self.fuzzy_search_sessions.lock().await.remove(&session_id);

        Ok(FuzzyFileSearchSessionStopResponse {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::task::Poll;

    #[test]
    fn canceled_fuzzy_search_removes_only_its_own_registration() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .expect("runtime");
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = runtime.spawn_blocking(move || {
            let _ = release_rx.recv();
        });
        let (outgoing_tx, _outgoing_rx) = tokio::sync::mpsc::channel(1);
        let processor = SearchRequestProcessor::new(Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        )));
        let directory = tempfile::TempDir::new().expect("search directory");
        let params = || FuzzyFileSearchParams {
            query: "needle".to_string(),
            roots: vec![directory.path().to_string_lossy().into_owned()],
            cancellation_token: Some("same-search".to_string()),
        };
        let mut first = Box::pin(processor.fuzzy_file_search(params()));
        runtime.block_on(std::future::poll_fn(|cx| {
            assert!(first.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        }));
        let first_flag =
            processor.pending_fuzzy_searches.lock().expect("registry")["same-search"].clone();
        assert!(!first_flag.load(Ordering::Relaxed));

        let mut replacement = Box::pin(processor.fuzzy_file_search(params()));
        runtime.block_on(std::future::poll_fn(|cx| {
            assert!(replacement.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        }));
        let replacement_flag =
            processor.pending_fuzzy_searches.lock().expect("registry")["same-search"].clone();
        assert!(first_flag.load(Ordering::Relaxed));
        assert!(!replacement_flag.load(Ordering::Relaxed));

        // Drop both normal request futures outside a runtime context. The old
        // request must not erase the replacement's cancellation registration.
        drop(first);
        assert!(Arc::ptr_eq(
            &processor.pending_fuzzy_searches.lock().expect("registry")["same-search"],
            &replacement_flag,
        ));
        drop(replacement);
        assert!(replacement_flag.load(Ordering::Relaxed));
        assert!(
            processor
                .pending_fuzzy_searches
                .lock()
                .expect("registry")
                .is_empty()
        );
        release_tx.send(()).expect("release blocking pool");
        runtime.block_on(blocker).expect("blocking pool released");
    }
}
