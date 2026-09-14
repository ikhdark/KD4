//! Session-based orchestration for `@` file searches.
//!
//! `ChatComposer` publishes every change of the `@token` as
//! `AppEvent::StartFileSearch(query)`. This manager owns a single
//! `codex-file-search` session for the current search root, updates the query
//! on every keystroke, and drops the session when the query becomes empty.

use codex_file_search as file_search;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;

pub(crate) struct FileSearchManager {
    state: Arc<Mutex<SearchState>>,
    search_dir: PathBuf,
    app_tx: AppEventSender,
}

struct SearchState {
    latest_query: String,
    session: Option<file_search::FileSearchSession>,
    session_token: usize,
}

impl FileSearchManager {
    pub fn new(search_dir: PathBuf, tx: AppEventSender) -> Self {
        Self {
            state: Arc::new(Mutex::new(SearchState {
                latest_query: String::new(),
                session: None,
                session_token: 0,
            })),
            search_dir,
            app_tx: tx,
        }
    }

    /// Updates the directory used for file searches.
    /// This should be called when the session's CWD changes on resume.
    /// Drops the current session so it will be recreated with the new directory on next query.
    pub fn update_search_dir(&mut self, new_dir: PathBuf) {
        if self.search_dir == new_dir {
            return;
        }
        self.search_dir = new_dir;
        #[expect(clippy::unwrap_used)]
        let mut st = self.state.lock().unwrap();
        st.session.take();
        st.latest_query.clear();
    }

    /// Call whenever the user edits the `@` token.
    pub fn on_user_query(&self, query: String) {
        #[expect(clippy::unwrap_used)]
        let mut st = self.state.lock().unwrap();
        if query == st.latest_query && (query.is_empty() || st.session.is_some()) {
            return;
        }
        st.latest_query.clear();
        st.latest_query.push_str(&query);

        if query.is_empty() {
            st.session.take();
            return;
        }

        if st.session.is_none() {
            self.start_session_locked(&mut st);
        }
        if let Some(session) = st.session.as_ref() {
            session.update_query(&query);
        }
    }

    fn start_session_locked(&self, st: &mut SearchState) {
        st.session_token = st.session_token.wrapping_add(1);
        let session_token = st.session_token;
        let reporter = Arc::new(TuiSessionReporter {
            state: Arc::downgrade(&self.state),
            app_tx: self.app_tx.clone(),
            session_token,
        });
        let session = file_search::create_session(
            vec![self.search_dir.clone()],
            file_search::FileSearchOptions {
                compute_indices: true,
                ..Default::default()
            },
            reporter,
            /*cancel_flag*/ None,
        );
        match session {
            Ok(session) => st.session = Some(session),
            Err(err) => {
                tracing::warn!("file search session failed to start: {err}");
                st.session = None;
            }
        }
    }
}

struct TuiSessionReporter {
    state: Weak<Mutex<SearchState>>,
    app_tx: AppEventSender,
    session_token: usize,
}

impl TuiSessionReporter {
    fn send_snapshot(&self, snapshot: &file_search::FileSearchSnapshot) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        #[expect(clippy::unwrap_used)]
        let st = state.lock().unwrap();
        if st.session_token != self.session_token
            || st.latest_query.is_empty()
            || snapshot.query != st.latest_query
        {
            return;
        }
        let query = snapshot.query.clone();
        drop(st);
        self.app_tx.send(AppEvent::FileSearchResult {
            query,
            matches: snapshot.matches.clone(),
        });
    }
}

impl file_search::SessionReporter for TuiSessionReporter {
    fn on_update(&self, snapshot: &file_search::FileSearchSnapshot) {
        self.send_snapshot(snapshot);
    }

    fn on_complete(&self, _query: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn reporter_drops_stale_query_results_and_delivers_current_query() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let manager = FileSearchManager::new(PathBuf::from("/repo"), AppEventSender::new(tx));
        manager.state.lock().unwrap().latest_query = "current".to_string();
        let reporter = TuiSessionReporter {
            state: Arc::downgrade(&manager.state),
            app_tx: manager.app_tx.clone(),
            session_token: 0,
        };
        reporter.send_snapshot(&file_search::FileSearchSnapshot {
            query: "old".to_string(),
            ..Default::default()
        });
        assert!(
            rx.try_recv().is_err(),
            "old-query results must not enter the event queue"
        );
        reporter.send_snapshot(&file_search::FileSearchSnapshot {
            query: "current".to_string(),
            ..Default::default()
        });
        assert!(
            matches!(rx.try_recv().expect("current results"), AppEvent::FileSearchResult { query, matches }
            if query == "current" && matches.is_empty())
        );
    }

    #[tokio::test]
    async fn file_search_manager_publishes_matches_and_releases_workers_on_drop() {
        let directory = tempfile::tempdir().expect("search directory");
        std::fs::write(directory.path().join("needle.rs"), "fn needle() {}")
            .expect("write matching file");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut manager =
            FileSearchManager::new(directory.path().to_path_buf(), AppEventSender::new(tx));
        // A failed session startup leaves the query recorded but no live session.
        manager.state.lock().unwrap().latest_query = "needle".to_string();
        manager.on_user_query("needle".to_string());
        let token = manager.state.lock().unwrap().session_token;
        manager.update_search_dir(directory.path().to_path_buf());
        {
            let state = manager.state.lock().unwrap();
            assert!(
                state.session.is_some(),
                "an unchanged directory must retain the live search"
            );
            assert_eq!(state.session_token, token);
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match rx.recv().await.expect("search must publish before closing") {
                    AppEvent::FileSearchResult { query, matches }
                        if query == "needle"
                            && matches
                                .iter()
                                .any(|entry| entry.path == std::path::Path::new("needle.rs")) =>
                    {
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("normal query must find the matching file");

        drop(manager);
        tokio::time::timeout(Duration::from_secs(5), async {
            while rx.recv().await.is_some() {}
        })
        .await
        .expect("dropping manager must release its session workers and event senders");
    }
}
