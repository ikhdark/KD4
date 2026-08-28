use std::num::NonZero;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_app_server_protocol::FuzzyFileSearchMatchType;
use codex_app_server_protocol::FuzzyFileSearchResult;
use codex_app_server_protocol::FuzzyFileSearchSessionCompletedNotification;
use codex_app_server_protocol::FuzzyFileSearchSessionUpdatedNotification;
use codex_app_server_protocol::ServerNotification;
use codex_file_search as file_search;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::outgoing_message::OutgoingMessageSender;

const MATCH_LIMIT: usize = 50;
const MAX_THREADS: usize = 12;

pub(crate) async fn run_fuzzy_file_search(
    query: String,
    roots: Vec<String>,
    cancellation_flag: Arc<AtomicBool>,
) -> Vec<FuzzyFileSearchResult> {
    if roots.is_empty() {
        return Vec::new();
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

    let mut files = match tokio::task::spawn_blocking(move || {
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
    .await
    {
        Ok(Ok(res)) => res
            .matches
            .into_iter()
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
                    indices: m.indices,
                }
            })
            .collect::<Vec<_>>(),
        Ok(Err(err)) => {
            warn!("fuzzy-file-search failed: {err}");
            Vec::new()
        }
        Err(err) => {
            warn!("fuzzy-file-search join failed: {err}");
            Vec::new()
        }
    };

    files.sort_by(file_search::cmp_by_score_desc_then_path_asc::<
        FuzzyFileSearchResult,
        _,
        _,
    >(|f| f.score, |f| f.path.as_str()));

    files
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
        session_id,
        latest_query: Mutex::new(String::new()),
        outgoing,
        pending_deliveries: Mutex::new(PendingDeliveries::default()),
        delivery_ready: Notify::new(),
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
    session_id: String,
    latest_query: Mutex<String>,
    outgoing: Arc<OutgoingMessageSender>,
    pending_deliveries: Mutex<PendingDeliveries>,
    delivery_ready: Notify,
    delivery_cancellation: CancellationToken,
    canceled: Arc<AtomicBool>,
}

#[derive(Clone, Copy)]
enum DeliveryKind {
    Update,
    Completion,
}

struct QueuedDelivery {
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
            DeliveryKind::Update => self.update = Some(delivery),
            DeliveryKind::Completion => self.completion = Some(delivery),
        }
    }

    fn take_next(&mut self) -> Option<QueuedDelivery> {
        self.update.take().or_else(|| self.completion.take())
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
                query,
                notification,
            },
        );
        self.delivery_ready.notify_one();
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
        if shared.canceled.load(Ordering::Relaxed) || !shared.query_is_current(&delivery.query) {
            continue;
        }
        tokio::select! {
            biased;
            _ = shared.delivery_cancellation.cancelled() => return,
            _ = shared.outgoing.send_server_notification(delivery.notification) => {}
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
            collect_files(snapshot)
        };

        let notification = ServerNotification::FuzzyFileSearchSessionUpdated(
            FuzzyFileSearchSessionUpdatedNotification {
                session_id: self.shared.session_id.clone(),
                query: query.clone(),
                files,
            },
        );
        self.shared
            .enqueue_delivery(DeliveryKind::Update, query, notification);
    }

    fn send_complete(&self, query: &str) {
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

    fn on_complete(&self, query: &str) {
        self.send_complete(query);
    }
}

fn collect_files(snapshot: &file_search::FileSearchSnapshot) -> Vec<FuzzyFileSearchResult> {
    let mut files = snapshot
        .matches
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
        .collect::<Vec<_>>();

    files.sort_by(file_search::cmp_by_score_desc_then_path_asc::<
        FuzzyFileSearchResult,
        _,
        _,
    >(|f| f.score, |f| f.path.as_str()));
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    use crate::outgoing_message::OutgoingMessageSender;

    fn completion_notification(query: &str) -> ServerNotification {
        ServerNotification::FuzzyFileSearchSessionCompleted(
            FuzzyFileSearchSessionCompletedNotification {
                session_id: "session".to_string(),
                query: query.to_string(),
            },
        )
    }

    #[tokio::test]
    async fn delivery_relay_bounds_pending_work_and_cancels_a_saturated_send() {
        let (tx, mut rx) = mpsc::channel(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        assert!(outgoing.try_send_server_notification(completion_notification("blocker")));

        let shared = Arc::new(SessionShared {
            session_id: "session".to_string(),
            latest_query: Mutex::new("query".to_string()),
            outgoing,
            pending_deliveries: Mutex::new(PendingDeliveries::default()),
            delivery_ready: Notify::new(),
            delivery_cancellation: CancellationToken::new(),
            canceled: Arc::new(AtomicBool::new(false)),
        });
        for _ in 0..32 {
            shared.enqueue_delivery(
                DeliveryKind::Update,
                "query".to_string(),
                completion_notification("query"),
            );
        }
        shared.enqueue_delivery(
            DeliveryKind::Completion,
            "query".to_string(),
            completion_notification("query"),
        );
        {
            let pending = shared.pending_deliveries.lock().unwrap();
            assert!(pending.update.is_some());
            assert!(pending.completion.is_some());
        }

        let relay = DeliveryRelay::start(shared.clone());
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let update_pending = shared.pending_deliveries.lock().unwrap().update.is_some();
                if !update_pending {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("relay should begin the saturated send");

        drop(relay);
        rx.recv().await.expect("blocking envelope");
        tokio::task::yield_now().await;
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }
}
