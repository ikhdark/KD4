use std::num::NonZero;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_app_server_protocol::FuzzyFileSearchMatchType;
use codex_app_server_protocol::FuzzyFileSearchResponse;
use codex_app_server_protocol::FuzzyFileSearchResult;
use codex_app_server_protocol::FuzzyFileSearchSessionCompletedNotification;
use codex_app_server_protocol::FuzzyFileSearchSessionUpdatedNotification;
use codex_app_server_protocol::ServerNotification;
use codex_file_search as file_search;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingMessageSender;

const MATCH_LIMIT: usize = 50;
const MAX_THREADS: usize = 12;

pub(crate) async fn run_fuzzy_file_search(
    query: String,
    roots: Vec<String>,
    cancellation_flag: Arc<AtomicBool>,
) -> anyhow::Result<FuzzyFileSearchResponse> {
    if roots.is_empty() || query.is_empty() {
        return Ok(FuzzyFileSearchResponse {
            walk_complete: roots.is_empty(),
            ..Default::default()
        });
    }

    #[expect(clippy::expect_used)]
    let limit = NonZero::new(MATCH_LIMIT).expect("MATCH_LIMIT should be a valid non-zero usize");

    let cores = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(1);
    let threads = cores.min(MAX_THREADS);
    #[expect(clippy::expect_used)]
    let threads = NonZero::new(threads.max(1)).expect("threads should be non-zero");
    let search_dirs: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();

    let result = tokio::task::spawn_blocking(move || {
        file_search::run(
            query.as_str(),
            search_dirs,
            file_search::FileSearchOptions {
                limit,
                threads,
                compute_indices: true,
                ..Default::default()
            },
            Some(cancellation_flag),
        )
    })
    .await??;
    Ok(FuzzyFileSearchResponse {
        files: collect_files(&result.matches),
        total_match_count: result.total_match_count,
        scanned_file_count: result.scanned_file_count,
        walk_complete: result.walk_complete,
    })
}

pub(crate) struct FuzzyFileSearchSession {
    session: file_search::FileSearchSession,
    shared: Arc<SessionShared>,
    delivery_relay: DeliveryRelay,
}

impl FuzzyFileSearchSession {
    pub(crate) fn update_query(&self, query: String) {
        if self.shared.canceled.load(Ordering::Relaxed) {
            return;
        }
        {
            #[expect(clippy::unwrap_used)]
            let mut latest_query = self.shared.latest_query.lock().unwrap();
            *latest_query = query.clone();
            self.shared.query_changed.notify_waiters();
        }
        self.session.update_query(&query);
    }
}

impl Drop for FuzzyFileSearchSession {
    fn drop(&mut self) {
        self.shared.canceled.store(true, Ordering::Relaxed);
        self.delivery_relay.cancel();
    }
}

pub(crate) fn start_fuzzy_file_search_session(
    connection_id: ConnectionId,
    session_id: String,
    roots: Vec<String>,
    outgoing: Arc<OutgoingMessageSender>,
) -> anyhow::Result<FuzzyFileSearchSession> {
    #[expect(clippy::expect_used)]
    let limit = NonZero::new(MATCH_LIMIT).expect("MATCH_LIMIT should be a valid non-zero usize");
    let cores = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(1);
    let threads = cores.min(MAX_THREADS);
    #[expect(clippy::expect_used)]
    let threads = NonZero::new(threads.max(1)).expect("threads should be non-zero");
    let search_dirs: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();
    let canceled = Arc::new(AtomicBool::new(false));

    let shared = Arc::new(SessionShared {
        connection_id,
        session_id,
        latest_query: Mutex::new(String::new()),
        outgoing,
        pending_deliveries: Mutex::new(PendingDeliveries::default()),
        delivery_ready: Notify::new(),
        query_changed: Notify::new(),
        completion_ready: Notify::new(),
        delivery_cancellation: CancellationToken::new(),
        canceled: canceled.clone(),
    });

    let reporter = Arc::new(SessionReporterImpl {
        shared: shared.clone(),
    });
    let session = file_search::create_session(
        search_dirs,
        file_search::FileSearchOptions {
            limit,
            threads,
            compute_indices: true,
            ..Default::default()
        },
        reporter,
        Some(canceled),
    )?;
    let delivery_relay = DeliveryRelay::start(shared.clone());

    Ok(FuzzyFileSearchSession {
        session,
        shared,
        delivery_relay,
    })
}

struct SessionShared {
    connection_id: ConnectionId,
    session_id: String,
    latest_query: Mutex<String>,
    outgoing: Arc<OutgoingMessageSender>,
    pending_deliveries: Mutex<PendingDeliveries>,
    delivery_ready: Notify,
    query_changed: Notify,
    completion_ready: Notify,
    delivery_cancellation: CancellationToken,
    canceled: Arc<AtomicBool>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DeliveryKind {
    Update,
    Completion,
}

struct QueuedDelivery {
    kind: DeliveryKind,
    query: String,
    notification: ServerNotification,
}

#[derive(Default)]
struct PendingDeliveries {
    update: Option<QueuedDelivery>,
    completion: Option<QueuedDelivery>,
}

impl PendingDeliveries {
    fn enqueue(&mut self, kind: DeliveryKind, delivery: QueuedDelivery) {
        match kind {
            DeliveryKind::Update => {
                if !self.completion.as_ref().is_some_and(|completion| completion.query == delivery.query) {
                    self.update = Some(delivery);
                }
            }
            DeliveryKind::Completion => {
                if self.update.as_ref().is_some_and(|update| update.query == delivery.query) {
                    self.update = None;
                }
                self.completion = Some(delivery);
            }
        }
    }

    fn take_next(&mut self) -> Option<QueuedDelivery> {
        self.completion.take().or_else(|| self.update.take())
    }
}

impl SessionShared {
    fn enqueue_delivery(
        &self,
        kind: DeliveryKind,
        query: String,
        notification: ServerNotification,
    ) {
        if self.canceled.load(Ordering::Relaxed) {
            return;
        }
        #[expect(clippy::unwrap_used)]
        self.pending_deliveries.lock().unwrap().enqueue(
            kind,
            QueuedDelivery {
                kind,
                query,
                notification,
            },
        );
        if kind == DeliveryKind::Completion {
            self.completion_ready.notify_waiters();
        }
        self.delivery_ready.notify_one();
    }

    async fn progress_superseded(&self, query: &str) {
        loop {
            let completed = self.completion_ready.notified();
            tokio::pin!(completed);
            completed.as_mut().enable();
            #[expect(clippy::unwrap_used)]
            let superseded = self.pending_deliveries.lock().unwrap().completion.as_ref()
                .is_some_and(|completion| completion.query == query);
            if superseded {
                return;
            }
            completed.await;
        }
    }

    fn query_is_current(&self, query: &str) -> bool {
        #[expect(clippy::unwrap_used)]
        let latest_query = self.latest_query.lock().unwrap();
        query == latest_query.as_str()
    }
}

struct DeliveryRelay {
    cancellation: CancellationToken,
    task: JoinHandle<()>,
}

impl DeliveryRelay {
    fn start(shared: Arc<SessionShared>) -> Self {
        let cancellation = shared.delivery_cancellation.clone();
        let task = tokio::spawn(run_delivery_relay(shared));
        Self { cancellation, task }
    }

    fn cancel(&self) {
        self.cancellation.cancel();
    }
}

impl Drop for DeliveryRelay {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.task.abort();
    }
}

async fn run_delivery_relay(shared: Arc<SessionShared>) {
    loop {
        let notified = shared.delivery_ready.notified();
        let delivery = {
            #[expect(clippy::unwrap_used)]
            shared.pending_deliveries.lock().unwrap().take_next()
        };
        let Some(delivery) = delivery else {
            tokio::select! {
                biased;
                _ = shared.delivery_cancellation.cancelled() => return,
                _ = notified => continue,
            }
        };
        let query_changed = shared.query_changed.notified();
        tokio::pin!(query_changed);
        query_changed.as_mut().enable();
        if shared.canceled.load(Ordering::Relaxed) || !shared.query_is_current(&delivery.query) {
            continue;
        }
        let recipients = [shared.connection_id];
        tokio::select! {
            biased;
            _ = shared.delivery_cancellation.cancelled() => return,
            _ = query_changed => continue,
            _ = shared.progress_superseded(&delivery.query), if delivery.kind == DeliveryKind::Update => continue,
            _ = shared.outgoing.send_server_notification_to_connections(
                &recipients,
                delivery.notification,
            ) => {}
        }
    }
}

struct SessionReporterImpl {
    shared: Arc<SessionShared>,
}

impl SessionReporterImpl {
    fn send_snapshot(&self, snapshot: &file_search::FileSearchSnapshot) {
        if self.shared.canceled.load(Ordering::Relaxed) {
            return;
        }

        let query = {
            #[expect(clippy::unwrap_used)]
            self.shared.latest_query.lock().unwrap().clone()
        };
        if snapshot.query != query {
            return;
        }

        let files = if query.is_empty() {
            Vec::new()
        } else {
            collect_files(&snapshot.matches)
        };

        let notification = ServerNotification::FuzzyFileSearchSessionUpdated(
            FuzzyFileSearchSessionUpdatedNotification {
                session_id: self.shared.session_id.clone(),
                query: query.clone(),
                files,
                total_match_count: if query.is_empty() {
                    0
                } else {
                    snapshot.total_match_count
                },
                scanned_file_count: snapshot.scanned_file_count,
                walk_complete: snapshot.walk_complete
                    && snapshot.match_complete
                    && !snapshot.cancelled,
            },
        );
        self.shared
            .enqueue_delivery(DeliveryKind::Update, query, notification);
    }

    fn send_complete(&self, snapshot: &file_search::FileSearchSnapshot) {
        let query = snapshot.query.as_str();
        if self.shared.canceled.load(Ordering::Relaxed) {
            return;
        }
        {
            #[expect(clippy::unwrap_used)]
            let latest_query = self.shared.latest_query.lock().unwrap();
            if query != latest_query.as_str() {
                return;
            }
        }
        let query = query.to_string();
        let notification = ServerNotification::FuzzyFileSearchSessionCompleted(
            FuzzyFileSearchSessionCompletedNotification {
                session_id: self.shared.session_id.clone(),
                query: query.clone(),
                results: Some(FuzzyFileSearchResponse {
                    files: if query.is_empty() {
                        Vec::new()
                    } else {
                        collect_files(&snapshot.matches)
                    },
                    total_match_count: if query.is_empty() {
                        0
                    } else {
                        snapshot.total_match_count
                    },
                    scanned_file_count: snapshot.scanned_file_count,
                    walk_complete: snapshot.walk_complete
                        && snapshot.match_complete
                        && !snapshot.cancelled,
                }),
                cancelled: snapshot.cancelled,
            },
        );
        self.shared
            .enqueue_delivery(DeliveryKind::Completion, query, notification);
    }
}

impl file_search::SessionReporter for SessionReporterImpl {
    fn on_update(&self, snapshot: &file_search::FileSearchSnapshot) {
        self.send_snapshot(snapshot);
    }

    fn on_complete(&self, _query: &str) {}

    fn on_complete_snapshot(&self, snapshot: &file_search::FileSearchSnapshot) {
        self.send_complete(snapshot);
    }
}

fn collect_files(matches: &[file_search::FileMatch]) -> Vec<FuzzyFileSearchResult> {
    // The engine chooses top-k using the full relevance key. Keep its ordering.
    matches
        .iter()
        .map(|m| {
            let file_name = m.path.file_name().unwrap_or_default();
            FuzzyFileSearchResult {
                root: m.root.to_string_lossy().to_string(),
                path: m.path.to_string_lossy().to_string(),
                match_type: match m.match_type {
                    file_search::MatchType::File => FuzzyFileSearchMatchType::File,
                    file_search::MatchType::Directory => FuzzyFileSearchMatchType::Directory,
                },
                file_name: file_name.to_string_lossy().to_string(),
                score: m.score,
                indices: m.indices.clone(),
            }
        })
        .collect()
}

