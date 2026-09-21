use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::NestedCancellation;
use codex_code_mode_protocol::StartedCell;
use codex_code_mode_protocol::host::DelegateRequest;
use codex_code_mode_protocol::host::DelegateRequestId;
use codex_code_mode_protocol::host::DelegateResponse;
use codex_code_mode_protocol::host::EncodedFrame;
use codex_code_mode_protocol::host::HostToClient;
use codex_code_mode_protocol::host::RequestId;
use codex_code_mode_protocol::host::SessionId;
use codex_code_mode_protocol::host::WireResult;
use tokio::sync::Notify;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

const CELL_MESSAGE_CAPACITY: usize = 128;
const MAX_PENDING_DELEGATE_CALLS: usize = 256;

pub(super) struct HostPeer {
    outgoing_tx: mpsc::Sender<EncodedFrame>,
    pending: StdMutex<HashMap<DelegateRequestId, PendingDelegate>>,
    delegate_permits: Arc<Semaphore>,
    cell_routes: StdMutex<HashMap<(SessionId, CellId), CellRoute>>,
    cell_routes_changed: Notify,
    next_request_id: AtomicI64,
    disconnected: CancellationToken,
    failure: StdMutex<Option<String>>,
}

struct PendingDelegate {
    response_tx: oneshot::Sender<Result<DelegateResponse, String>>,
    dispatched: bool,
    cell_key: (SessionId, CellId),
    _permit: OwnedSemaphorePermit,
}

enum CellRoute {
    Pending(VecDeque<CellMessage>),
    Active(Arc<CellQueue>),
}

#[derive(Default)]
struct CellQueue {
    messages: StdMutex<VecDeque<CellMessage>>,
    changed: Notify,
}

impl CellQueue {
    async fn recv(&self) -> Option<CellMessage> {
        loop {
            let changed = self.changed.notified();
            if let Some(message) = self
                .messages
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front()
            {
                return Some(message);
            }
            changed.await;
        }
    }
}

fn push_cell_message(
    messages: &mut VecDeque<CellMessage>,
    message: CellMessage,
) -> Result<(), String> {
    // Closure owns a reserved slot, and is idempotent. Callback admission can
    // fail locally without destroying other cells or the connection.
    if messages
        .iter()
        .any(|message| matches!(message, CellMessage::Closed))
    {
        return if matches!(message, CellMessage::Closed) {
            Ok(())
        } else {
            Err("code-mode cell is closed".to_string())
        };
    }
    if messages.len() >= CELL_MESSAGE_CAPACITY && !matches!(message, CellMessage::Closed) {
        return Err("code-mode cell message queue is full".to_string());
    }
    messages.push_back(message);
    Ok(())
}

enum CellMessage {
    Delegate {
        id: DelegateRequestId,
        request: Box<DelegateRequest>,
        dispatched_tx: oneshot::Sender<Result<(), String>>,
    },
    Closed,
}

impl HostPeer {
    pub(super) fn new(outgoing_tx: mpsc::Sender<EncodedFrame>) -> Self {
        Self {
            outgoing_tx,
            pending: StdMutex::new(HashMap::new()),
            delegate_permits: Arc::new(Semaphore::new(MAX_PENDING_DELEGATE_CALLS)),
            cell_routes: StdMutex::new(HashMap::new()),
            cell_routes_changed: Notify::new(),
            next_request_id: AtomicI64::new(1),
            disconnected: CancellationToken::new(),
            failure: StdMutex::new(None),
        }
    }

    pub(super) fn send(&self, message: HostToClient) -> Result<(), PeerSendError> {
        let frame = EncodedFrame::encode(&message)
            .map_err(|err| PeerSendError::Payload(err.to_string()))?;
        self.send_frame(frame)
    }

    pub(super) fn respond(
        &self,
        id: RequestId,
        result: Result<codex_code_mode_protocol::host::HostResponse, String>,
    ) {
        let message = HostToClient::Response {
            id,
            result: WireResult::from_result(result),
        };
        if let Err(PeerSendError::Payload(err)) = self.send(message) {
            let _ = self.send(HostToClient::Response {
                id,
                result: WireResult::Err {
                    message: format!("code-mode host response exceeds the IPC frame limit: {err}"),
                },
            });
        }
    }

    fn initial_response(
        &self,
        id: RequestId,
        result: Result<codex_code_mode_protocol::host::WireRuntimeResponse, String>,
    ) {
        let message = HostToClient::InitialResponse {
            id,
            result: WireResult::from_result(result),
        };
        if let Err(PeerSendError::Payload(err)) = self.send(message) {
            let _ = self.send(HostToClient::InitialResponse {
                id,
                result: WireResult::Err {
                    message: format!(
                        "code-mode initial response exceeds the IPC frame limit: {err}"
                    ),
                },
            });
        }
    }

    pub(super) async fn call(
        self: &Arc<Self>,
        session_id: SessionId,
        request: DelegateRequest,
        cancellation: NestedCancellation,
    ) -> Result<DelegateResponse, String> {
        let cancellation_token = cancellation.token().clone();
        if self.disconnected.is_cancelled() {
            return Err("code-mode client connection closed".to_string());
        }
        if cancellation_token.is_cancelled() {
            return Err("code mode delegate request cancelled".to_string());
        }
        let Ok(permit) = Arc::clone(&self.delegate_permits).try_acquire_owned() else {
            return Err("code-mode host has too many pending delegate calls".to_string());
        };
        let id = DelegateRequestId::new(
            self.next_request_id
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .map_err(|_| "code-mode delegate request ID space exhausted".to_string())?,
        );
        let cell_id = match &request {
            DelegateRequest::InvokeTool { invocation } => invocation.cell_id.clone().into(),
            DelegateRequest::Notify { cell_id, .. } => cell_id.clone().into(),
        };
        let cell_key = (session_id, cell_id);
        let (response_tx, response_rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                id,
                PendingDelegate {
                    response_tx,
                    dispatched: false,
                    cell_key: cell_key.clone(),
                    _permit: permit,
                },
            );
        let mut pending = PendingDelegateRequest::new(Arc::clone(self), id);
        let (dispatched_tx, dispatched_rx) = oneshot::channel();
        if let Err(err) = self.route_cell_message(
            cell_key,
            CellMessage::Delegate {
                id,
                request: Box::new(request),
                dispatched_tx,
            },
            Some(&cancellation_token),
        ) {
            self.pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            pending.disarm();
            return Err(err);
        }

        let dispatched = tokio::select! {
            _ = cancellation_token.cancelled() => {
                self.cancel_pending(id, cancellation.cause());
                pending.disarm();
                return Err("code mode delegate request cancelled".to_string());
            }
            dispatched = dispatched_rx => dispatched.map_err(|_| {
                "code-mode cell route closed before dispatching delegate request".to_string()
            })?,
            _ = self.disconnected.cancelled() => {
                self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
                pending.disarm();
                return Err("code-mode client connection closed".to_string());
            }
        };
        if let Err(err) = dispatched {
            self.pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            pending.disarm();
            return Err(err);
        }

        tokio::select! {
            response = response_rx => {
                pending.disarm();
                response.map_err(|_| {
                    "code-mode client closed before returning delegate output".to_string()
                })?
            }
            _ = cancellation_token.cancelled() => {
                self.cancel_pending(id, cancellation.cause());
                pending.disarm();
                Err("code mode delegate request cancelled".to_string())
            }
            _ = self.disconnected.cancelled() => {
                self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
                pending.disarm();
                Err("code-mode client connection closed".to_string())
            }
        }
    }

    pub(super) async fn complete(
        &self,
        id: DelegateRequestId,
        response: Result<DelegateResponse, String>,
    ) {
        if let Some(pending) = self.remove_pending(id) {
            let _ = pending.response_tx.send(response);
        }
    }

    pub(super) fn start_cell(
        self: &Arc<Self>,
        session_id: SessionId,
        request_id: RequestId,
        started: StartedCell,
        active_cell_permit: OwnedSemaphorePermit,
    ) -> oneshot::Receiver<()> {
        let (initial_response_sent_tx, initial_response_sent_rx) = oneshot::channel();
        let key = (session_id, started.cell_id.clone());
        let messages_rx = Arc::new(CellQueue::default());
        let mut routes = self
            .cell_routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Keep activation and transfer atomic with respect to callbacks so a
        // newer message cannot overtake buffered delegates or closure.
        let previous = routes.insert(key.clone(), CellRoute::Active(Arc::clone(&messages_rx)));
        match previous {
            Some(CellRoute::Pending(messages)) => {
                *messages_rx
                    .messages
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = messages;
            }
            Some(CellRoute::Active(_)) => {
                self.fail("code-mode cell route is already active".to_string());
                return initial_response_sent_rx;
            }
            None => {}
        }
        drop(routes);
        let peer = Arc::clone(self);
        self.spawn_critical("cell forwarding", async move {
            drive_cell(
                peer,
                key,
                request_id,
                started,
                messages_rx,
                initial_response_sent_tx,
                active_cell_permit,
            )
            .await;
        });
        initial_response_sent_rx
    }

    pub(super) fn close_cell(&self, session_id: SessionId, cell_id: CellId) {
        let _ = self.route_cell_message((session_id, cell_id), CellMessage::Closed, None);
    }

    pub(super) fn disconnect(&self) {
        self.disconnected.cancel();
    }

    pub(super) fn fail(&self, reason: String) {
        let mut failure = self.failure.lock().unwrap_or_else(PoisonError::into_inner);
        if failure.is_none() {
            *failure = Some(reason);
        }
        drop(failure);
        self.disconnect();
    }

    pub(super) fn failure(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(super) fn is_disconnected(&self) -> bool {
        self.disconnected.is_cancelled()
    }

    pub(super) async fn disconnected(&self) {
        self.disconnected.cancelled().await;
    }

    pub(super) fn disconnection_token(&self) -> CancellationToken {
        self.disconnected.clone()
    }

    pub(super) async fn wait_for_session_cells(&self, session_id: &SessionId) {
        loop {
            let changed = self.cell_routes_changed.notified();
            if !self
                .cell_routes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .keys()
                .any(|(route_session_id, _)| route_session_id == session_id)
            {
                return;
            }
            tokio::select! {
                _ = changed => {}
                _ = self.disconnected.cancelled() => return,
            }
        }
    }

    async fn send_delegate_if_pending(
        &self,
        id: DelegateRequestId,
        session_id: SessionId,
        request: DelegateRequest,
        dispatched_tx: oneshot::Sender<Result<(), String>>,
    ) {
        // Prepare large frames without holding the shared pending-call lock.
        let frame = EncodedFrame::encode(&HostToClient::DelegateRequest {
            id,
            session_id,
            request,
        })
        .map_err(|error| PeerSendError::Payload(error.to_string()));
        let result = {
            let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(pending) = pending.get_mut(&id) else {
                let _ = dispatched_tx.send(Err(
                    "code-mode delegate request was cancelled before dispatch".to_string(),
                ));
                return;
            };
            match frame.and_then(|frame| self.send_frame(frame)) {
                Ok(()) => {
                    pending.dispatched = true;
                    Ok(())
                }
                Err(err) => Err(err.to_string()),
            }
        };
        let _ = dispatched_tx.send(result);
    }

    fn route_cell_message(
        &self,
        key: (SessionId, CellId),
        message: CellMessage,
        cancellation: Option<&CancellationToken>,
    ) -> Result<(), String> {
        use std::collections::hash_map::Entry;

        let mut routes = self
            .cell_routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Closure revokes callback tokens before removing the route. Recheck
        // under this lock to prevent late callbacks from recreating it.
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err("code mode delegate request cancelled".to_string());
        }
        if self.is_disconnected() {
            return Err("code-mode client connection closed".to_string());
        }
        match routes.entry(key) {
            Entry::Occupied(mut entry) => match entry.get_mut() {
                CellRoute::Pending(messages) => push_cell_message(messages, message),
                CellRoute::Active(queue) => {
                    push_cell_message(
                        &mut queue
                            .messages
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner),
                        message,
                    )?;
                    queue.changed.notify_one();
                    Ok(())
                }
            },
            Entry::Vacant(entry) => {
                entry.insert(CellRoute::Pending(VecDeque::from([message])));
                Ok(())
            }
        }
    }

    fn remove_pending(&self, id: DelegateRequestId) -> Option<PendingDelegate> {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id)?;
        let mut routes = self
            .cell_routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let retain = |message: &CellMessage| !matches!(message, CellMessage::Delegate { id: queued, .. } if *queued == id);
        match routes.get_mut(&pending.cell_key) {
            Some(CellRoute::Pending(messages)) => messages.retain(retain),
            Some(CellRoute::Active(queue)) => queue
                .messages
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(retain),
            None => {}
        }
        Some(pending)
    }

    fn cancel_pending(
        &self,
        id: DelegateRequestId,
        cause: Option<codex_code_mode_protocol::CancellationCause>,
    ) {
        if let Some(pending) = self.remove_pending(id)
            && pending.dispatched
        {
            let _ = self.send(HostToClient::CancelDelegateRequest { id, cause });
        }
    }

    pub(super) fn spawn_critical<F>(self: &Arc<Self>, task_name: &'static str, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let task = tokio::spawn(future);
        let peer = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = task.await {
                peer.fail(format!("code-mode {task_name} task failed: {err}"));
            }
        });
    }

    fn send_frame(&self, frame: EncodedFrame) -> Result<(), PeerSendError> {
        match self.outgoing_tx.try_send(frame) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.fail("code-mode host outgoing queue is full".to_string());
                Err(PeerSendError::Unavailable(
                    "code-mode host outgoing queue is full".to_string(),
                ))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.disconnect();
                Err(PeerSendError::Unavailable(
                    "code-mode client connection closed".to_string(),
                ))
            }
        }
    }
}

async fn drive_cell(
    peer: Arc<HostPeer>,
    key: (SessionId, CellId),
    request_id: RequestId,
    started: StartedCell,
    messages_rx: Arc<CellQueue>,
    initial_response_sent_tx: oneshot::Sender<()>,
    _active_cell_permit: OwnedSemaphorePermit,
) {
    let mut initial_response_sent_tx = Some(initial_response_sent_tx);
    let initial_response = started.initial_response();
    tokio::pin!(initial_response);
    let closed = loop {
        tokio::select! {
            biased;
            result = &mut initial_response => {
                peer.initial_response(request_id, result.map(Into::into));
                if let Some(initial_response_sent_tx) = initial_response_sent_tx.take() {
                    let _ = initial_response_sent_tx.send(());
                }
                break false;
            }
            message = messages_rx.recv() => match message {
                Some(CellMessage::Delegate {
                    id,
                    request,
                    dispatched_tx,
                }) => {
                    peer.send_delegate_if_pending(id, key.0.clone(), *request, dispatched_tx).await;
                }
                Some(CellMessage::Closed) | None => break true,
            },
            _ = peer.disconnected.cancelled() => {
                peer.remove_cell_route(&key);
                return;
            }
        }
    };

    if closed {
        let result = tokio::select! {
            result = &mut initial_response => result,
            _ = peer.disconnected.cancelled() => {
                peer.remove_cell_route(&key);
                return;
            }
        };
        peer.initial_response(request_id, result.map(Into::into));
        if let Some(initial_response_sent_tx) = initial_response_sent_tx.take() {
            let _ = initial_response_sent_tx.send(());
        }
    } else {
        loop {
            tokio::select! {
                message = messages_rx.recv() => match message {
                    Some(CellMessage::Delegate {
                        id,
                        request,
                        dispatched_tx,
                    }) => {
                        peer.send_delegate_if_pending(id, key.0.clone(), *request, dispatched_tx).await;
                    }
                    Some(CellMessage::Closed) | None => break,
                },
                _ = peer.disconnected.cancelled() => {
                    peer.remove_cell_route(&key);
                    return;
                }
            }
        }
    }
    let _ = peer.send(HostToClient::CellClosed {
        session_id: key.0.clone(),
        cell_id: (&key.1).into(),
    });
    peer.remove_cell_route(&key);
}

impl HostPeer {
    fn remove_cell_route(&self, key: &(SessionId, CellId)) {
        let removed = self
            .cell_routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
        if removed.is_some() {
            self.cell_routes_changed.notify_waiters();
        }
    }
}

pub(super) enum PeerSendError {
    Payload(String),
    Unavailable(String),
}

impl std::fmt::Display for PeerSendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Payload(message) | Self::Unavailable(message) => formatter.write_str(message),
        }
    }
}

struct PendingDelegateRequest {
    peer: Arc<HostPeer>,
    id: Option<DelegateRequestId>,
}

impl PendingDelegateRequest {
    fn new(peer: Arc<HostPeer>, id: DelegateRequestId) -> Self {
        Self { peer, id: Some(id) }
    }

    fn disarm(&mut self) {
        self.id = None;
    }
}

impl Drop for PendingDelegateRequest {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else {
            return;
        };
        if let Some(pending) = self.peer.remove_pending(id)
            && pending.dispatched
        {
            // Dropped without reaching a cancellation origin, so there is no
            // cause to report.
            let _ = self
                .peer
                .send(HostToClient::CancelDelegateRequest { id, cause: None });
        }
    }
}

#[cfg(test)]
#[path = "peer_tests.rs"]
mod tests;
