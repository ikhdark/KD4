use std::future::Future;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;

use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::CodeModeSessionDelegate;
use codex_code_mode_protocol::RuntimeResponse;
use codex_code_mode_protocol::StartedCell;
use codex_code_mode_protocol::host::DelegateRequest;
use codex_code_mode_protocol::host::RequestId;
use codex_code_mode_protocol::host::SessionId;
use pretty_assertions::assert_eq;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;
use tokio_util::sync::CancellationToken;

use super::HostPeer;
use super::MAX_PENDING_DELEGATE_CALLS;

fn session_id(value: &str) -> SessionId {
    SessionId::new(value).expect("session ID")
}

#[tokio::test]
async fn start_cell_reports_when_initial_response_is_enqueued() {
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(/*max_capacity*/ 4);
    let peer = Arc::new(HostPeer::new(outgoing_tx));
    let cell_id = CellId::new("cell-1".to_string());
    let (response_tx, response_rx) = oneshot::channel();
    let started = StartedCell::new(cell_id.clone(), response_rx);
    let active_cell_permits = Arc::new(Semaphore::new(/*permits*/ 1));
    let active_cell_permit = Arc::clone(&active_cell_permits)
        .try_acquire_owned()
        .expect("active cell permit");

    let mut initial_response_sent = peer.start_cell(
        session_id("session-1"),
        RequestId::new(/*value*/ 1),
        started,
        active_cell_permit,
    );
    assert_eq!(initial_response_sent.try_recv(), Err(TryRecvError::Empty));

    response_tx
        .send(RuntimeResponse::Result {
            cell_id: cell_id.clone(),
            content_items: Vec::new(),
            error_text: None,
        })
        .expect("initial response receiver");
    initial_response_sent
        .await
        .expect("initial response completion");
    outgoing_rx.recv().await.expect("initial response frame");
    assert_eq!(active_cell_permits.available_permits(), 0);

    peer.close_cell(session_id("session-1"), cell_id);
    let permit = tokio::time::timeout(
        Duration::from_secs(1),
        Arc::clone(&active_cell_permits).acquire_owned(),
    )
    .await
    .expect("cell permit should be released")
    .expect("cell permit semaphore should remain open");
    drop(permit);
}

#[tokio::test]
async fn pending_delegate_limit_rejects_call_without_disconnecting() {
    let (outgoing_tx, _outgoing_rx) = mpsc::channel(/*max_capacity*/ 1);
    let peer = Arc::new(HostPeer::new(outgoing_tx));
    let permits = Arc::clone(&peer.delegate_permits)
        .acquire_many_owned(MAX_PENDING_DELEGATE_CALLS as u32)
        .await
        .expect("delegate permits");

    let result = peer
        .call(
            session_id("session-1"),
            DelegateRequest::Notify {
                call_id: "call-1".to_string(),
                cell_id: CellId::new("cell-1".to_string()).into(),
                text: "hello".to_string(),
            },
            CancellationToken::new(),
        )
        .await;

    assert_eq!(
        result,
        Err("code-mode host has too many pending delegate calls".to_string())
    );
    assert!(!peer.is_disconnected());
    drop(permits);
}

#[test]
fn dropped_delegate_call_releases_capacity_without_a_runtime() {
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(4);
    let peer = Arc::new(HostPeer::new(outgoing_tx));
    let mut call = Box::pin(peer.call(
        session_id("session-1"),
        DelegateRequest::Notify {
            call_id: "call-1".to_string(),
            cell_id: CellId::new("cell-1".to_string()).into(),
            text: "hello".to_string(),
        },
        CancellationToken::new(),
    ));
    assert!(matches!(
        call.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    assert_eq!(
        peer.delegate_permits.available_permits(),
        MAX_PENDING_DELEGATE_CALLS - 1
    );

    drop(call);

    assert_eq!(
        peer.delegate_permits.available_permits(),
        MAX_PENDING_DELEGATE_CALLS
    );
    assert!(matches!(
        outgoing_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(!peer.is_disconnected());
}

#[tokio::test]
async fn exhausted_delegate_ids_reject_calls_without_side_effects() {
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(1);
    let peer = Arc::new(HostPeer::new(outgoing_tx));
    peer.next_request_id
        .store(i64::MAX, std::sync::atomic::Ordering::Relaxed);

    for _ in 0..2 {
        let result = peer
            .call(
                session_id("session-1"),
                DelegateRequest::Notify {
                    call_id: "exhausted-call".to_string(),
                    cell_id: CellId::new("cell-1".to_string()).into(),
                    text: "must not dispatch".to_string(),
                },
                CancellationToken::new(),
            )
            .await;

        assert_eq!(
            result,
            Err("code-mode delegate request ID space exhausted".to_string())
        );
        assert_eq!(
            peer.delegate_permits.available_permits(),
            MAX_PENDING_DELEGATE_CALLS
        );
        assert!(peer.pending.lock().expect("pending delegates").is_empty());
        assert!(peer.cell_routes.lock().expect("cell routes").is_empty());
        assert!(matches!(
            outgoing_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(!peer.is_disconnected());
    }
}

#[tokio::test]
async fn cancellation_before_cell_dispatch_releases_delegate_call() {
    use codex_code_mode_protocol::host::DelegateResponse;
    use codex_code_mode_protocol::host::FramedReader;
    use codex_code_mode_protocol::host::FramedWriter;
    use codex_code_mode_protocol::host::HostToClient;

    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(4);
    let peer = Arc::new(HostPeer::new(outgoing_tx));
    let delegate = crate::delegate::RemoteDelegate::new(session_id("session-1"), Arc::clone(&peer));
    let cell_id = CellId::new("cell-1".to_string());
    let cancellation = CancellationToken::new();
    let mut call = delegate.notify(
        "cancelled-call".to_string(),
        cell_id.clone(),
        "must not dispatch".to_string(),
        cancellation.clone(),
    );
    assert!(matches!(
        call.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    cancellation.cancel();

    let result = tokio::time::timeout(Duration::from_secs(1), call)
        .await
        .expect("cancellation must finish before a cell route starts");

    assert_eq!(
        result,
        Err("code mode delegate request cancelled".to_string())
    );
    assert_eq!(
        peer.delegate_permits.available_permits(),
        MAX_PENDING_DELEGATE_CALLS
    );
    assert!(matches!(
        outgoing_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    // Starting the route must skip the cancelled request and preserve a later
    // notification on the same cell. Reading the next frame proves both.
    let (_response_tx, response_rx) = oneshot::channel();
    let active_cell_permit = Arc::new(Semaphore::new(1))
        .try_acquire_owned()
        .expect("cell permit");
    let _initial_response = peer.start_cell(
        session_id("session-1"),
        RequestId::new(1),
        StartedCell::new(cell_id.clone(), response_rx),
        active_cell_permit,
    );
    let mut live_call = delegate.notify(
        "live-call".to_string(),
        cell_id.clone(),
        "delivered".to_string(),
        CancellationToken::new(),
    );
    assert!(matches!(
        live_call
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    let frame = tokio::time::timeout(Duration::from_secs(1), outgoing_rx.recv())
        .await
        .expect("live delegate dispatch")
        .expect("live request frame");
    let mut bytes = Vec::new();
    FramedWriter::new(&mut bytes)
        .write_frame(&frame)
        .await
        .expect("frame encoding");
    let Some(HostToClient::DelegateRequest {
        id,
        session_id: actual_session,
        request,
    }) = FramedReader::new(bytes.as_slice())
        .read()
        .await
        .expect("frame decoding")
    else {
        panic!("expected live delegate request");
    };
    assert_eq!(actual_session, session_id("session-1"));
    assert_eq!(
        request,
        DelegateRequest::Notify {
            call_id: "live-call".to_string(),
            cell_id: cell_id.into(),
            text: "delivered".to_string(),
        }
    );
    peer.complete(id, Ok(DelegateResponse::NotificationDelivered))
        .await;
    assert_eq!(live_call.await, Ok(()));
    assert_eq!(
        peer.delegate_permits.available_permits(),
        MAX_PENDING_DELEGATE_CALLS
    );
    assert!(matches!(
        outgoing_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(!peer.is_disconnected());
    peer.disconnect();
}

#[tokio::test]
async fn dropped_dispatched_delegate_call_sends_cancellation_immediately() {
    use codex_code_mode_protocol::host::FramedReader;
    use codex_code_mode_protocol::host::FramedWriter;
    use codex_code_mode_protocol::host::HostToClient;

    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(4);
    let peer = Arc::new(HostPeer::new(outgoing_tx));
    let cell_id = CellId::new("cell-1".to_string());
    let (_response_tx, response_rx) = oneshot::channel();
    let active_cell_permit = Arc::new(Semaphore::new(1))
        .try_acquire_owned()
        .expect("cell permit");
    let _initial_response = peer.start_cell(
        session_id("session-1"),
        RequestId::new(1),
        StartedCell::new(cell_id.clone(), response_rx),
        active_cell_permit,
    );
    let mut call = Box::pin(peer.call(
        session_id("session-1"),
        DelegateRequest::Notify {
            call_id: "call-1".to_string(),
            cell_id: cell_id.into(),
            text: "hello".to_string(),
        },
        CancellationToken::new(),
    ));
    assert!(matches!(
        call.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    let request_frame = tokio::time::timeout(Duration::from_secs(1), outgoing_rx.recv())
        .await
        .expect("delegate dispatch")
        .expect("request frame");

    drop(call);

    let cancellation_frame = outgoing_rx
        .try_recv()
        .expect("immediate cancellation frame");
    assert_eq!(
        peer.delegate_permits.available_permits(),
        MAX_PENDING_DELEGATE_CALLS
    );
    let mut bytes = Vec::new();
    let mut writer = FramedWriter::new(&mut bytes);
    writer
        .write_frame(&request_frame)
        .await
        .expect("request encoding");
    writer
        .write_frame(&cancellation_frame)
        .await
        .expect("cancellation encoding");
    let mut reader = FramedReader::new(bytes.as_slice());
    let Some(HostToClient::DelegateRequest { id, .. }) =
        reader.read().await.expect("request decoding")
    else {
        panic!("expected delegate request");
    };
    assert_eq!(
        reader
            .read::<HostToClient>()
            .await
            .expect("cancellation decoding"),
        Some(HostToClient::CancelDelegateRequest { id })
    );
    assert!(matches!(
        outgoing_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    peer.disconnect();
}
