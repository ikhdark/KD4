use futures::FutureExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing::debug;
use tracing::warn;

use crate::ExecServerRuntimePaths;
use crate::connection::CHANNEL_CAPACITY;
use crate::connection::JsonRpcConnection;
use crate::connection::JsonRpcConnectionEvent;
use crate::rpc::RpcNotificationSender;
use crate::rpc::RpcServerOutboundMessage;
use crate::rpc::encode_server_message;
use crate::rpc::invalid_request;
use crate::rpc::method_not_found;
use crate::server::ExecServerHandler;
use crate::server::registry::build_router;
use crate::server::session_registry::SessionRegistry;
use crate::telemetry::ConnectionTransport;
use crate::telemetry::ExecServerTelemetry;

#[derive(Clone)]
pub(crate) struct ConnectionProcessor {
    session_registry: Arc<SessionRegistry>,
    runtime_paths: ExecServerRuntimePaths,
    telemetry: ExecServerTelemetry,
    stop: CancellationToken,
    connections: Arc<std::sync::Mutex<tokio_util::task::TaskTracker>>,
}

impl ConnectionProcessor {
    #[cfg(test)]
    pub(crate) fn new(runtime_paths: ExecServerRuntimePaths) -> Self {
        Self::new_with_telemetry(runtime_paths, ExecServerTelemetry::default())
    }

    pub(crate) fn new_with_telemetry(
        runtime_paths: ExecServerRuntimePaths,
        telemetry: ExecServerTelemetry,
    ) -> Self {
        Self {
            session_registry: SessionRegistry::new(telemetry.clone()),
            runtime_paths,
            telemetry,
            stop: CancellationToken::new(),
            connections: Arc::new(std::sync::Mutex::new(tokio_util::task::TaskTracker::new())),
        }
    }

    pub(crate) fn with_shutdown(mut self, stop: CancellationToken) -> Self {
        self.stop = stop;
        self
    }

    pub(crate) async fn wait_for_shutdown(&self) {
        self.stop.cancelled().await;
    }

    pub(crate) async fn run_connection(
        &self,
        connection: JsonRpcConnection,
        transport: ConnectionTransport,
    ) {
        let owner = {
            let connections = self
                .connections
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.stop.is_cancelled() {
                None
            } else {
                Some(connections.token())
            }
        };
        let Some(_owner) = owner else {
            for task in connection.task_handles {
                task.abort();
                let _ = task.await;
            }
            return;
        };
        run_connection_with_shutdown(
            connection,
            Arc::clone(&self.session_registry),
            self.runtime_paths.clone(),
            self.telemetry.clone(),
            transport,
            self.stop.child_token(),
        )
        .await;
    }

    pub(crate) async fn shutdown(&self) {
        self.stop.cancel();
        let connections = {
            let connections = self
                .connections
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            connections.close();
            connections.clone()
        };
        connections.wait().await;
        self.session_registry.shutdown().await;
    }
}

#[cfg(test)]
async fn run_connection(
    connection: JsonRpcConnection,
    session_registry: Arc<SessionRegistry>,
    runtime_paths: ExecServerRuntimePaths,
    telemetry: ExecServerTelemetry,
    transport: ConnectionTransport,
) {
    run_connection_with_shutdown(
        connection,
        session_registry,
        runtime_paths,
        telemetry,
        transport,
        CancellationToken::new(),
    )
    .await;
}

async fn run_connection_with_shutdown(
    connection: JsonRpcConnection,
    session_registry: Arc<SessionRegistry>,
    runtime_paths: ExecServerRuntimePaths,
    telemetry: ExecServerTelemetry,
    transport: ConnectionTransport,
    cancelled: CancellationToken,
) {
    let _connection_metrics = telemetry.connection_started(transport);
    let router = Arc::new(build_router());
    let JsonRpcConnection {
        outgoing_tx: json_outgoing_tx,
        mut incoming_rx,
        mut disconnected_rx,
        task_handles: connection_tasks,
        transport: _transport,
    } = connection;
    let (outgoing_tx, mut outgoing_rx) =
        mpsc::channel::<RpcServerOutboundMessage>(CHANNEL_CAPACITY);
    let notifications = RpcNotificationSender::new(outgoing_tx.clone());
    let handler = Arc::new(ExecServerHandler::new(
        session_registry,
        notifications,
        runtime_paths,
    ));

    if matches!(transport, ConnectionTransport::WebSocket) {
        let handler = Arc::downgrade(&handler);
        let cancelled = cancelled.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancelled.cancelled() => {},
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                    if handler.upgrade().is_some_and(|handler| !handler.is_initialized()) { cancelled.cancel(); }
                }
            }
        });
    }
    let disconnect_cancelled = cancelled.clone();
    let disconnect_task = tokio::spawn(async move {
        if !*disconnected_rx.borrow() {
            let _ = disconnected_rx.changed().await;
        }
        disconnect_cancelled.cancel();
    });
    let outbound_cancelled = cancelled.clone();
    let outbound_task = tokio::spawn(async move {
        let _cancel_on_exit = outbound_cancelled.clone().drop_guard();
        loop {
            let message = tokio::select! {
                biased;
                _ = outbound_cancelled.cancelled() => break,
                message = outgoing_rx.recv() => match message {
                    Some(message) => message,
                    None => break,
                },
            };
            let json_message = match encode_server_message(message) {
                Ok(json_message) => json_message,
                Err(err) => {
                    warn!("failed to serialize exec-server outbound message: {err}");
                    break;
                }
            };
            if tokio::select! {
                biased;
                _ = outbound_cancelled.cancelled() => true,
                result = json_outgoing_tx.send(json_message) => result.is_err(),
            } {
                break;
            }
        }
    });

    let mut reads = tokio::task::JoinSet::new();
    let mut controls = tokio::task::JoinSet::new();
    const CONTROL_CAPACITY: usize = 8;
    let mut ordered = tokio::task::JoinSet::new();
    type Completion = futures::future::Shared<futures::future::BoxFuture<'static, ()>>;
    let mut last_ordered: Option<Completion> = None;
    let mut process_writes: HashMap<String, Completion> = HashMap::new();
    let mut pending_starts: HashMap<
        String,
        (Completion, crate::local_process::ProcessStartIdentity),
    > = HashMap::new();
    // Mutations retain wire order. Reads, controls, and outbound HTTP bypass
    // unrelated waits: a remote server may withhold response headers for as long
    // as its work runs, and HTTP has no ordering relation to process or file state.
    // Process-scoped requests still depend on any start admitted for their identity.
    loop {
        let event = tokio::select! {
            _ = cancelled.cancelled() => break,
            result = controls.join_next(), if !controls.is_empty() => {
                if !matches!(result, Some(Ok(true))) { break; }
                continue;
            }
            result = reads.join_next(), if !reads.is_empty() => {
                if !matches!(result, Some(Ok(true))) { break; }
                continue;
            }
            result = ordered.join_next(), if !ordered.is_empty() => {
                if !matches!(result, Some(Ok(true))) { break; }
                continue;
            }
            event = incoming_rx.recv() => {
                let Some(event) = event else { break; };
                event
            }
        };
        if !handler.is_session_attached() {
            debug!("exec-server connection evicted after session resume");
            break;
        }
        match event {
            JsonRpcConnectionEvent::MalformedMessage { reason } => {
                warn!("ignoring malformed exec-server message: {reason}");
                if send_outbound(
                    &outgoing_tx,
                    RpcServerOutboundMessage::Error {
                        request_id: codex_exec_server_protocol::RequestId::Integer(-1),
                        error: invalid_request(reason),
                    },
                    &cancelled,
                )
                .await
                .is_err()
                {
                    break;
                }
            }
            JsonRpcConnectionEvent::Message(message) => match message {
                codex_exec_server_protocol::JSONRPCMessage::Request(request) => {
                    if matches!(&request.id, codex_exec_server_protocol::RequestId::String(id) if id.len() > 4096) {
                        if send_outbound(&outgoing_tx, RpcServerOutboundMessage::Error {
                            request_id: request.id,
                            error: invalid_request("JSON-RPC string request IDs must not exceed 4096 bytes; no operation started".into()),
                        }, &cancelled).await.is_err() { break; }
                        continue;
                    }
                    let request_started_at = Instant::now();
                    let request_route = router.request_route(request.method.as_str());
                    let mut request_metrics = telemetry.request_started_at(
                        request_route
                            .as_ref()
                            .map_or("unknown", |(method, _)| *method),
                        request_started_at,
                    );
                    if let Some((method, route)) = request_route {
                        let initialized = handler.is_initialized();
                        let control = initialized
                            && matches!(
                                method,
                                crate::protocol::EXEC_TERMINATE_METHOD
                                    | crate::protocol::HTTP_REQUEST_CANCEL_METHOD
                            );
                        let independent_read = initialized
                            && matches!(
                                method,
                                crate::protocol::EXEC_READ_METHOD
                                    | crate::protocol::EXEC_SIGNAL_METHOD
                                    | crate::protocol::EXEC_TERMINATE_METHOD
                                    | crate::protocol::ENVIRONMENT_INFO_METHOD
                                    | crate::protocol::HTTP_REQUEST_METHOD
                                    | crate::protocol::HTTP_REQUEST_CANCEL_METHOD
                            );
                        if initialized
                            && (if control {
                                controls.len()
                            } else if independent_read {
                                reads.len()
                            } else {
                                ordered.len()
                            }) >= if control {
                                CONTROL_CAPACITY
                            } else {
                                CHANNEL_CAPACITY
                            }
                        {
                            request_metrics.set_result("rejected_capacity");
                            if send_outbound(
                                &outgoing_tx,
                                RpcServerOutboundMessage::Error {
                                    request_id: request.id,
                                    error: if matches!(
                                        method,
                                        crate::protocol::EXEC_METHOD
                                            | crate::protocol::EXEC_OWNED_METHOD
                                    ) {
                                        crate::rpc::process_start_refused(
                                            "too many pending exec-server requests".into(),
                                        )
                                    } else {
                                        invalid_request(
                                            "too many pending exec-server requests".into(),
                                        )
                                    },
                                },
                                &cancelled,
                            )
                            .await
                            .is_err()
                            {
                                break;
                            }
                            continue;
                        }
                        // Reserve HTTP identity before spawning: cancellation may
                        // arrive before the spawned request starts running.
                        if initialized
                            && method == crate::protocol::HTTP_REQUEST_METHOD
                            && let Ok(params) =
                                serde_json::from_value::<crate::protocol::HttpRequestParams>(
                                    request.params.clone().unwrap_or_default(),
                                )
                            && params.stream_response
                            && let Err(error) = handler
                                .reserve_http_body_stream(&params.request_id, &request.id)
                                .await
                        {
                            request_metrics.set_result("rejected_body_reservation");
                            if send_outbound(
                                &outgoing_tx,
                                RpcServerOutboundMessage::Error {
                                    request_id: request.id,
                                    error,
                                },
                                &cancelled,
                            )
                            .await
                            .is_err()
                            {
                                break;
                            }
                            continue;
                        }
                        // Poll completed receipts even when no successor ever
                        // polled their Shared future; peek alone retains those
                        // unused identities forever.
                        pending_starts.retain(|_, (completion, _)| {
                            completion.clone().now_or_never().is_none()
                        });
                        process_writes
                            .retain(|_, completion| completion.clone().now_or_never().is_none());
                        // Reserve the exact generation before the start is queued
                        // behind unrelated ordered work. Its RAII guard travels
                        // with the request, including disconnect/cancellation.
                        let admitted_start = if initialized
                            && matches!(
                                method,
                                crate::protocol::EXEC_METHOD | crate::protocol::EXEC_OWNED_METHOD
                            ) {
                            Some(
                                match serde_json::from_value::<
                                    crate::protocol::OwnedProcessParams<
                                        crate::protocol::ExecParams,
                                    >,
                                >(
                                    request.params.clone().unwrap_or_default()
                                ) {
                                    Ok(params)
                                        if method == crate::protocol::EXEC_OWNED_METHOD
                                            && params.incarnation.is_none() =>
                                    {
                                        Err(crate::rpc::invalid_params(
                                            "owned start requires an incarnation".into(),
                                        ))
                                    }
                                    Ok(params) => handler
                                        .reserve_owned_start(&params)
                                        .await
                                        .map(|reservation| (params.params, reservation)),
                                    Err(error) => {
                                        Err(crate::rpc::invalid_params(error.to_string()))
                                    }
                                },
                            )
                        } else {
                            None
                        };
                        let start_identity = admitted_start
                            .as_ref()
                            .and_then(|result| result.as_ref().ok())
                            .map(|(_, reservation)| reservation.identity());
                        let process_id = request
                            .params
                            .as_ref()
                            .and_then(|params| params.get("processId"))
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string);
                        let admitted_terminate = if initialized
                            && method == crate::protocol::EXEC_TERMINATE_METHOD
                        {
                            Some(
                                match serde_json::from_value::<
                                    crate::protocol::OwnedProcessParams<
                                        crate::protocol::TerminateParams,
                                    >,
                                >(
                                    request.params.clone().unwrap_or_default()
                                ) {
                                    Ok(envelope) => {
                                        let params = envelope.params;
                                        let identity = if let Some(incarnation) =
                                            envelope.incarnation.as_deref()
                                        {
                                            // Terminate is idempotent: an expired or replaced handle is not
                                            // running under its incarnation. Settle it like an absent ID
                                            // below; an error here would keep cleanup owners retrying forever.
                                            handler
                                                .process_start_identity(&params.process_id)
                                                .await
                                                .map(|identity| {
                                                    identity.filter(|identity| {
                                                        identity.has_incarnation(incarnation)
                                                    })
                                                })
                                        } else {
                                            match pending_starts.get(params.process_id.as_str()) {
                                                Some((_, identity)) => Ok(Some(identity.clone())),
                                                None => {
                                                    handler
                                                        .process_start_identity(&params.process_id)
                                                        .await
                                                }
                                            }
                                        };
                                        identity.map(|identity| (params, identity))
                                    }
                                    Err(error) => {
                                        Err(crate::rpc::invalid_params(error.to_string()))
                                    }
                                },
                            )
                        } else {
                            None
                        };
                        // Act on the exact stream present at admission, never a
                        // same-named replacement reached by a deferred lookup.
                        let admitted_http_cancel = if initialized && method == crate::protocol::HTTP_REQUEST_CANCEL_METHOD {
                            Some(match serde_json::from_value::<crate::protocol::HttpRequestCancelParams>(request.params.clone().unwrap_or_default()) {
                                Ok(params) => handler.cancel_http_body_stream(&params.request_id).await,
                                Err(error) => Err(crate::rpc::invalid_params(error.to_string())),
                            })
                        } else { None };
                        let predecessor = if admitted_terminate.is_some() || admitted_http_cancel.is_some() {
                            None
                        } else if initialized && method == crate::protocol::EXEC_WRITE_METHOD {
                            // A blocked terminal owns only its own FIFO. Starts
                            // still depend on prior filesystem/environment work.
                            process_id
                                .as_ref()
                                .and_then(|id| process_writes.get(id))
                                .cloned()
                                .or_else(|| {
                                    process_id
                                        .as_ref()
                                        .and_then(|id| pending_starts.get(id))
                                        .map(|(completion, _)| completion.clone())
                                })
                        } else if independent_read {
                            process_id
                                .as_ref()
                                .and_then(|id| pending_starts.get(id))
                                .map(|(completion, _)| completion.clone())
                        } else {
                            last_ordered.take()
                        };
                        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel::<()>();
                        if initialized && !independent_read {
                            let completion = async move {
                                let _ = completion_rx.await;
                            }
                            .boxed()
                            .shared();
                            if method == crate::protocol::EXEC_WRITE_METHOD {
                                if let Some(id) = process_id.as_ref() {
                                    process_writes.insert(id.clone(), completion.clone());
                                }
                            } else {
                                last_ordered = Some(completion.clone());
                            }
                            if matches!(
                                method,
                                crate::protocol::EXEC_METHOD | crate::protocol::EXEC_OWNED_METHOD
                            ) && let Some(process_id) = process_id
                                && let Some(identity) = start_identity
                            {
                                pending_starts.insert(process_id, (completion, identity));
                            }
                        }
                        let request_span = request_span(method, &request);
                        let response: std::pin::Pin<
                            Box<
                                dyn std::future::Future<Output = Option<RpcServerOutboundMessage>>
                                    + Send,
                            >,
                        > = if let Some(admitted) = admitted_start {
                            let handler = Arc::clone(&handler);
                            Box::pin(async move {
                                let result = match admitted {
                                    Ok((params, reservation)) => {
                                        handler.exec_reserved(params, reservation).await
                                    }
                                    Err(error) => Err(error),
                                };
                                Some(process_response(request.id, result))
                            })
                        } else if let Some(admitted) = admitted_terminate {
                            let handler = Arc::clone(&handler);
                            Box::pin(async move {
                                let result = match admitted {
                                    Ok((params, Some(identity))) => {
                                        handler.terminate_admitted(params, identity).await
                                    }
                                    // Absence belongs to admission time too. A queued
                                    // terminate must never kill a subsequently admitted ID.
                                    Ok((_, None)) => {
                                        Ok(crate::protocol::TerminateResponse { running: false })
                                    }
                                    Err(error) => Err(error),
                                };
                                Some(process_response(request.id, result))
                            })
                        } else if let Some(result) = admitted_http_cancel {
                            Box::pin(async move { Some(process_response(request.id, result)) })
                        } else {
                            route(Arc::clone(&handler), request)
                        };
                        let response = response.instrument(request_span.clone());
                        let request_cancelled = cancelled.clone();
                        let outgoing_tx = outgoing_tx.clone();
                        let operation = async move {
                            // Dropping the sender also releases successors during cancellation.
                            let _completion = completion_tx;
                            if let Some(predecessor) = predecessor {
                                tokio::select! {
                                    _ = request_cancelled.cancelled() => return false,
                                    _ = predecessor => {}
                                }
                            }
                            let message = tokio::select! {
                                message = response => message,
                                _ = request_cancelled.cancelled() => {
                                    request_span.record("result", "disconnected");
                                    request_metrics.set_result("disconnected");
                                    return false;
                                }
                            };
                            let result = request_result(&message);
                            if let Some(message) = message
                                && send_outbound(&outgoing_tx, message, &request_cancelled)
                                    .await
                                    .is_err()
                            {
                                request_span.record("result", "disconnected");
                                request_metrics.set_result("disconnected");
                                return false;
                            }
                            request_span.record("result", result);
                            request_metrics.set_result(result);
                            true
                        };
                        if control {
                            controls.spawn(operation);
                        } else if independent_read {
                            reads.spawn(operation);
                        } else if initialized {
                            ordered.spawn(operation);
                        } else if !operation.await {
                            break;
                        }
                    } else {
                        let method = "unknown";
                        let request_span = request_span(method, &request);
                        if send_outbound(
                            &outgoing_tx,
                            RpcServerOutboundMessage::Error {
                                request_id: request.id,
                                error: method_not_found(format!(
                                    "exec-server stub does not implement `{}` yet",
                                    request.method
                                )),
                            },
                            &cancelled,
                        )
                        .await
                        .is_err()
                        {
                            request_span.record("result", "disconnected");
                            request_metrics.set_result("disconnected");
                            break;
                        }
                        request_span.record("result", "error");
                        request_metrics.set_result("error");
                    }
                }
                codex_exec_server_protocol::JSONRPCMessage::Notification(notification) => {
                    let Some(route) = router.notification_route(notification.method.as_str())
                    else {
                        warn!(
                            "closing exec-server connection after unexpected notification: {}",
                            notification.method
                        );
                        break;
                    };
                    let result = tokio::select! {
                        result = route(Arc::clone(&handler), notification) => result,
                        _ = cancelled.cancelled() => {
                            debug!(
                                "exec-server transport disconnected while handling notification"
                            );
                            break;
                        }
                    };
                    if let Err(err) = result {
                        warn!("closing exec-server connection after protocol error: {err}");
                        break;
                    }
                }
                codex_exec_server_protocol::JSONRPCMessage::Response(response) => {
                    warn!(
                        "closing exec-server connection after unexpected client response: {:?}",
                        response.id
                    );
                    break;
                }
                codex_exec_server_protocol::JSONRPCMessage::Error(error) => {
                    warn!(
                        "closing exec-server connection after unexpected client error: {:?}",
                        error.id
                    );
                    break;
                }
            },
            JsonRpcConnectionEvent::Disconnected { reason } => {
                if let Some(reason) = reason {
                    debug!("exec-server connection disconnected: {reason}");
                }
                break;
            }
        }
    }

    cancelled.cancel();
    disconnect_task.abort();
    let _ = disconnect_task.await;
    ordered.abort_all();
    while ordered.join_next().await.is_some() {}
    reads.abort_all();
    while reads.join_next().await.is_some() {}
    controls.abort_all();
    while controls.join_next().await.is_some() {}
    handler.shutdown().await;
    drop(handler);
    drop(outgoing_tx);
    for task in connection_tasks {
        task.abort();
        let _ = task.await;
    }
    let _ = outbound_task.await;
}

fn process_response<T: serde::Serialize>(
    request_id: codex_exec_server_protocol::RequestId,
    result: Result<T, codex_exec_server_protocol::JSONRPCErrorError>,
) -> RpcServerOutboundMessage {
    match result.and_then(|value| {
        serde_json::to_value(value).map_err(|error| crate::rpc::internal_error(error.to_string()))
    }) {
        Ok(result) => RpcServerOutboundMessage::Response { request_id, result },
        Err(error) => RpcServerOutboundMessage::Error { request_id, error },
    }
}

async fn send_outbound(
    sender: &mpsc::Sender<RpcServerOutboundMessage>,
    message: RpcServerOutboundMessage,
    cancelled: &CancellationToken,
) -> Result<(), ()> {
    tokio::select! {
        biased;
        _ = cancelled.cancelled() => Err(()),
        result = sender.send(message) => result.map_err(|_| ()),
    }
}

fn request_span(
    span_name: &str,
    request: &codex_exec_server_protocol::JSONRPCRequest,
) -> tracing::Span {
    let method = request.method.as_str();
    let span = tracing::info_span!(
        "codex.exec_server.request",
        otel.kind = "server",
        otel.name = span_name,
        method,
        result = tracing::field::Empty,
    );
    if let Some(trace) = &request.trace
        && !codex_otel::set_parent_from_w3c_trace_context(&span, trace)
    {
        warn!(method, "ignoring invalid inbound exec-server trace carrier");
    }
    span
}

fn request_result(message: &Option<RpcServerOutboundMessage>) -> &'static str {
    match message {
        Some(RpcServerOutboundMessage::Error { .. }) => "error",
        Some(
            RpcServerOutboundMessage::Response { .. } | RpcServerOutboundMessage::Notification(_),
        )
        | None => "success",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn rejected_shutdown_connection_finishes_transport_tasks() {
        let processor = super::ConnectionProcessor::new(test_runtime_paths());
        processor.shutdown().await;
        let (_silent_peer, reader) = tokio::io::duplex(64);
        let connection = JsonRpcConnection::from_stdio(reader, tokio::io::sink(), "rejected-after-shutdown".into());
        let tasks = connection.task_handles.iter().map(tokio::task::JoinHandle::abort_handle).collect::<Vec<_>>();
        timeout(Duration::from_secs(1), processor.run_connection(connection, crate::telemetry::ConnectionTransport::Stdio)).await.unwrap();
        assert!(tasks.iter().all(tokio::task::AbortHandle::is_finished));
    }

    use codex_exec_server_protocol::JSONRPCMessage;
    use codex_exec_server_protocol::JSONRPCNotification;
    use codex_exec_server_protocol::JSONRPCRequest;
    use codex_exec_server_protocol::JSONRPCResponse;
    use codex_exec_server_protocol::RequestId;
    use codex_utils_path_uri::PathUri;
    use opentelemetry::trace::SpanId;
    use opentelemetry::trace::TraceId;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::trace::InMemorySpanExporter;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use pretty_assertions::assert_eq;
    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use tokio::io::AsyncBufReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::io::BufReader;
    use tokio::io::DuplexStream;
    use tokio::io::Lines;
    use tokio::io::duplex;
    use tokio::task::JoinHandle;
    use tokio::time::timeout;
    use tracing_subscriber::filter::filter_fn;
    use tracing_subscriber::prelude::*;

    use super::request_span;
    use super::run_connection;
    use crate::ExecServerRuntimePaths;
    use crate::ProcessId;
    use crate::connection::JsonRpcConnection;
    use crate::protocol::ENVIRONMENT_INFO_METHOD;
    use crate::protocol::EXEC_METHOD;
    use crate::protocol::EXEC_READ_METHOD;
    use crate::protocol::EXEC_TERMINATE_METHOD;
    use crate::protocol::EnvironmentInfo;
    use crate::protocol::ExecParams;
    use crate::protocol::ExecResponse;
    use crate::protocol::INITIALIZE_METHOD;
    use crate::protocol::INITIALIZED_METHOD;
    use crate::protocol::InitializeParams;
    use crate::protocol::InitializeResponse;
    use crate::protocol::ReadParams;
    use crate::protocol::TerminateParams;
    use crate::protocol::TerminateResponse;
    use crate::server::session_registry::SessionRegistry;

    #[test]
    fn request_span_uses_bounded_name_wire_method_and_inbound_trace_parent() {
        let span_exporter = InMemorySpanExporter::default();
        let tracer_provider = SdkTracerProvider::builder()
            .with_simple_exporter(span_exporter.clone())
            .build();
        let tracer = tracer_provider.tracer("exec-server-test");
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(filter_fn(codex_otel::OtelProvider::trace_export_filter)),
        );
        let trace_id = TraceId::from_hex("00000000000000000000000000000001").expect("trace id");
        let parent_span_id = SpanId::from_hex("0000000000000002").expect("span id");
        let trace = codex_protocol::protocol::W3cTraceContext {
            traceparent: Some(format!("00-{trace_id}-{parent_span_id}-01")),
            tracestate: None,
        };

        let method = "custom/method";
        tracing::subscriber::with_default(subscriber, || {
            tracing::callsite::rebuild_interest_cache();
            let request = JSONRPCRequest {
                id: RequestId::Integer(1),
                method: method.to_string(),
                params: None,
                trace: Some(trace),
            };
            let request_span = request_span("unknown", &request);
            request_span.in_scope(|| {});
            drop(request_span);
        });

        tracer_provider.force_flush().expect("flush traces");
        let spans = span_exporter.get_finished_spans().expect("span export");
        let request_span = spans
            .iter()
            .find(|span| span.name.as_ref() == "unknown")
            .expect("unknown method span");
        assert_eq!(
            request_span
                .attributes
                .iter()
                .find(|attribute| attribute.key.as_str() == "method")
                .map(|attribute| attribute.value.clone()),
            Some(opentelemetry::Value::String(method.into()))
        );
        assert_eq!(request_span.span_context.trace_id(), trace_id);
        assert_eq!(request_span.parent_span_id, parent_span_id);
    }

    #[tokio::test]
    async fn connection_accepts_pipelined_scalar_requests() {
        let registry = SessionRegistry::new(crate::ExecServerTelemetry::default());
        let (mut writer, mut lines, task) = spawn_test_connection(registry, "pipelined-scalar");

        send_request(
            &mut writer,
            /*id*/ 1,
            INITIALIZE_METHOD,
            &InitializeParams {
                client_name: "exec-server-test".to_string(),
                resume_session_id: None,
            },
        )
        .await;
        let _: InitializeResponse = read_response(&mut lines, /*expected_id*/ 1).await;
        send_notification(&mut writer, INITIALIZED_METHOD, &()).await;

        send_request(&mut writer, /*id*/ 2, ENVIRONMENT_INFO_METHOD, &()).await;
        send_request(&mut writer, /*id*/ 3, ENVIRONMENT_INFO_METHOD, &()).await;

        timeout(Duration::from_secs(1), async {
            let mut ids = Vec::new();
            for _ in 0..2 {
                let line = lines
                    .next_line()
                    .await
                    .expect("read response")
                    .expect("response line");
                let JSONRPCMessage::Response(JSONRPCResponse { id, result }) =
                    serde_json::from_str(&line).expect("decode JSON-RPC response")
                else {
                    panic!("expected environment response: {line}");
                };
                let _: EnvironmentInfo = serde_json::from_value(result).expect("environment info");
                ids.push(id);
            }
            // Independent scalar requests correlate by id, not completion order.
            assert_eq!(ids.len(), 2);
            assert!(ids.contains(&RequestId::Integer(2)));
            assert!(ids.contains(&RequestId::Integer(3)));
        })
        .await
        .expect("both pipelined requests must complete");

        drop(writer);
        drop(lines);
        timeout(Duration::from_secs(1), task)
            .await
            .expect("processor should exit")
            .expect("processor should join");
    }

    #[tokio::test]
    async fn connection_serves_metadata_while_process_read_is_pending() {
        let registry = SessionRegistry::new(crate::ExecServerTelemetry::default());
        let (mut writer, mut lines, task) =
            spawn_test_connection(Arc::clone(&registry), "independent-read");
        send_request(
            &mut writer,
            1,
            INITIALIZE_METHOD,
            &InitializeParams {
                client_name: "test".into(),
                resume_session_id: None,
            },
        )
        .await;
        let _: InitializeResponse = read_response(&mut lines, 1).await;
        send_notification(&mut writer, INITIALIZED_METHOD, &()).await;
        let process_id = ProcessId::from("pending-read");
        send_request(
            &mut writer,
            2,
            EXEC_METHOD,
            &exec_params(process_id.clone()),
        )
        .await;
        let response: ExecResponse = read_response(&mut lines, 2).await;
        assert_eq!(response.process_id, process_id);
        send_request(
            &mut writer,
            3,
            EXEC_READ_METHOD,
            &ReadParams {
                process_id,
                after_seq: None,
                max_bytes: None,
                wait_ms: Some(5_000),
            },
        )
        .await;
        send_request(&mut writer, 4, ENVIRONMENT_INFO_METHOD, &()).await;
        let info: EnvironmentInfo = timeout(Duration::from_secs(1), read_response(&mut lines, 4))
            .await
            .expect("metadata must complete before the pending read");
        assert_eq!(info, EnvironmentInfo::local());
        drop(writer);
        drop(lines);
        timeout(Duration::from_secs(1), task)
            .await
            .expect("read must cancel on disconnect")
            .unwrap();
        registry.shutdown().await;
    }

    #[tokio::test]
    async fn full_http_pool_keeps_cleanup_controls_admissible() {
        let registry = SessionRegistry::new(crate::ExecServerTelemetry::default());
        let (mut writer, mut lines, task) =
            spawn_test_connection(Arc::clone(&registry), "full-http");
        send_request(
            &mut writer,
            1,
            INITIALIZE_METHOD,
            &InitializeParams {
                client_name: "test".into(),
                resume_session_id: None,
            },
        )
        .await;
        let _: InitializeResponse = read_response(&mut lines, 1).await;
        send_notification(&mut writer, INITIALIZED_METHOD, &()).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut sockets = Vec::new();
        for id in 0..crate::connection::CHANNEL_CAPACITY {
            send_request(
                &mut writer,
                10 + id as i64,
                crate::protocol::HTTP_REQUEST_METHOD,
                &crate::protocol::HttpRequestParams {
                    method: "GET".into(),
                    url: format!("http://{}/wait", listener.local_addr().unwrap()),
                    headers: Vec::new(),
                    body: None,
                    timeout_ms: Some(60_000),
                    redirect_policy: crate::protocol::HttpRedirectPolicy::Follow,
                    request_id: format!("held-{id}"),
                    stream_response: true,
                },
            )
            .await;
            sockets.push(
                timeout(Duration::from_secs(2), listener.accept())
                    .await
                    .unwrap()
                    .unwrap()
                    .0,
            );
        }
        send_request(&mut writer, 500, ENVIRONMENT_INFO_METHOD, &()).await;
        let overflow: JSONRPCMessage = serde_json::from_str(
            &timeout(Duration::from_secs(2), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(
            matches!(overflow, JSONRPCMessage::Error(error) if error.id == RequestId::Integer(500)
            && error.error.message.contains("too many pending"))
        );
        send_request(
            &mut writer,
            501,
            EXEC_TERMINATE_METHOD,
            &TerminateParams {
                process_id: "missing".into(),
            },
        )
        .await;
        let response: TerminateResponse =
            timeout(Duration::from_secs(2), read_response(&mut lines, 501))
                .await
                .unwrap();
        assert!(
            !response.running,
            "unknown process semantics still run while ordinary requests are full"
        );
        send_request(
            &mut writer,
            502,
            crate::protocol::HTTP_REQUEST_CANCEL_METHOD,
            &crate::protocol::HttpRequestCancelParams {
                request_id: "held-0".into(),
            },
        )
        .await;
        timeout(Duration::from_secs(2), async {
            let mut cancelled = false;
            let mut acknowledged = false;
            while !cancelled || !acknowledged {
                let message: JSONRPCMessage =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                match message {
                    JSONRPCMessage::Error(error) if error.id == RequestId::Integer(10) => {
                        assert!(error.error.message.contains("cancelled"));
                        cancelled = true;
                    }
                    JSONRPCMessage::Response(response)
                        if response.id == RequestId::Integer(502) =>
                    {
                        acknowledged = true
                    }
                    other => panic!("unexpected control outcome: {other:?}"),
                }
            }
        })
        .await
        .unwrap();
        drop(writer);
        drop(lines);
        timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        drop(sockets);
        registry.shutdown().await;
    }

    #[tokio::test]
    async fn owned_terminate_of_an_expired_handle_settles_as_not_running() {
        let registry = SessionRegistry::new(crate::ExecServerTelemetry::default());
        let (mut writer, mut lines, task) =
            spawn_test_connection(Arc::clone(&registry), "owned-terminate");
        send_request(
            &mut writer,
            1,
            INITIALIZE_METHOD,
            &InitializeParams {
                client_name: "test".into(),
                resume_session_id: None,
            },
        )
        .await;
        let _: InitializeResponse = read_response(&mut lines, 1).await;
        send_notification(&mut writer, INITIALIZED_METHOD, &()).await;
        // Client cleanup owners retry termination errors. An incarnation that is
        // no longer in custody (failed launch, eviction, replacement) must settle.
        send_request(
            &mut writer,
            2,
            EXEC_TERMINATE_METHOD,
            &crate::protocol::OwnedProcessParams {
                incarnation: Some("expired-incarnation".to_string()),
                params: TerminateParams {
                    process_id: ProcessId::from("expired-owned-process"),
                },
            },
        )
        .await;
        let response: TerminateResponse =
            timeout(Duration::from_secs(2), read_response(&mut lines, 2))
                .await
                .expect("owned terminate should answer");
        assert!(!response.running);
        drop(writer);
        drop(lines);
        timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        registry.shutdown().await;
    }

    #[tokio::test]
    async fn slow_http_headers_do_not_block_other_requests_or_cancellation() {
        let registry = SessionRegistry::new(crate::ExecServerTelemetry::default());
        let (mut writer, mut lines, task) =
            spawn_test_connection(Arc::clone(&registry), "slow-http");
        send_request(
            &mut writer,
            1,
            INITIALIZE_METHOD,
            &InitializeParams {
                client_name: "test".into(),
                resume_session_id: None,
            },
        )
        .await;
        let _: InitializeResponse = read_response(&mut lines, 1).await;
        send_notification(&mut writer, INITIALIZED_METHOD, &()).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        send_request(
            &mut writer,
            2,
            crate::protocol::HTTP_REQUEST_METHOD,
            &crate::protocol::HttpRequestParams {
                method: "GET".into(),
                url: format!("http://{}/wait", listener.local_addr().unwrap()),
                headers: Vec::new(),
                body: None,
                timeout_ms: Some(10_000),
                redirect_policy: crate::protocol::HttpRedirectPolicy::Follow,
                request_id: "pre-header".into(),
                stream_response: true,
            },
        )
        .await;
        let (socket, _) = timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        send_request(&mut writer, 3, ENVIRONMENT_INFO_METHOD, &()).await;
        let info: EnvironmentInfo = timeout(Duration::from_secs(1), read_response(&mut lines, 3))
            .await
            .unwrap();
        assert_eq!(info, EnvironmentInfo::local());
        // Filesystem requests keep wire order among themselves, but must not wait
        // for a remote server that is still withholding HTTP response headers.
        let temp = tempfile::TempDir::new().expect("temp directory");
        send_request(
            &mut writer,
            5,
            crate::protocol::FS_GET_METADATA_METHOD,
            &crate::protocol::FsGetMetadataParams {
                path: PathUri::from_host_native_path(temp.path()).expect("temp directory URI"),
                sandbox: None,
                include_directory_case_sensitivity: None,
            },
        )
        .await;
        let metadata: crate::protocol::FsGetMetadataResponse =
            timeout(Duration::from_secs(1), read_response(&mut lines, 5))
                .await
                .expect("filesystem request must not wait for pending HTTP headers");
        assert!(metadata.is_directory);
        send_request(
            &mut writer,
            4,
            crate::protocol::HTTP_REQUEST_CANCEL_METHOD,
            &crate::protocol::HttpRequestCancelParams {
                request_id: "pre-header".into(),
            },
        )
        .await;
        let mut cancelled = false;
        let mut acknowledged = false;
        timeout(Duration::from_secs(2), async {
            while !cancelled || !acknowledged {
                let message: JSONRPCMessage =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                match message {
                    JSONRPCMessage::Error(error) if error.id == RequestId::Integer(2) => {
                        assert!(error.error.message.contains("cancelled"));
                        cancelled = true;
                    }
                    JSONRPCMessage::Response(response) if response.id == RequestId::Integer(4) => {
                        acknowledged = true
                    }
                    other => panic!("unexpected response: {other:?}"),
                }
            }
        })
        .await
        .unwrap();
        drop(socket);
        drop(writer);
        drop(lines);
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        registry.shutdown().await;
    }

    #[tokio::test]
    async fn outbound_failure_or_backpressure_cannot_prevent_disconnect_cleanup() {
        for writer_failure in [false, true] {
            let registry = SessionRegistry::new(crate::ExecServerTelemetry::default());
            let (outgoing_tx, outgoing_rx) = tokio::sync::mpsc::channel(1);
            let (incoming_tx, incoming_rx) =
                tokio::sync::mpsc::channel(crate::connection::CHANNEL_CAPACITY * 2);
            let (disconnected_tx, disconnected_rx) = tokio::sync::watch::channel(false);
            let task = tokio::spawn(run_connection(
                crate::connection::JsonRpcConnection {
                    outgoing_tx,
                    incoming_rx,
                    disconnected_rx,
                    task_handles: Vec::new(),
                    transport: crate::connection::JsonRpcTransport::Plain,
                },
                Arc::clone(&registry),
                test_runtime_paths(),
                crate::ExecServerTelemetry::default(),
                crate::telemetry::ConnectionTransport::Stdio,
            ));
            for _ in 0..crate::connection::CHANNEL_CAPACITY + 4 {
                incoming_tx
                    .try_send(
                        crate::connection::JsonRpcConnectionEvent::MalformedMessage {
                            reason: "test".into(),
                        },
                    )
                    .unwrap();
            }
            tokio::task::yield_now().await;
            if writer_failure {
                drop(outgoing_rx);
            } else {
                disconnected_tx.send(true).unwrap();
            }
            timeout(Duration::from_secs(1), task)
                .await
                .expect("cleanup must not depend on the outbound receiver draining")
                .unwrap();
            registry.shutdown().await;
        }
    }

    #[test]
    fn registered_http_route_bounds_preparation_and_releases_stream_reservation() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let registry = SessionRegistry::new(crate::ExecServerTelemetry::default());
            let (mut writer, mut lines, task) = spawn_test_connection(registry, "http-preparation");
            send_request(
                &mut writer,
                1,
                INITIALIZE_METHOD,
                &InitializeParams {
                    client_name: "http-preparation-test".into(),
                    resume_session_id: None,
                },
            )
            .await;
            let _: InitializeResponse = read_response(&mut lines, 1).await;
            send_notification(&mut writer, INITIALIZED_METHOD, &()).await;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("listener");
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
            });
            started_rx.await.expect("worker occupied");
            let cache_guard = crate::ReqwestHttpClient::block_cache_for_test();
            let mut results = Vec::new();
            // Reusing the streaming request ID must reach preparation again, proving failed
            // preparation released the handler's stream reservation through normal routing.
            for id in [2, 3] {
                send_request(
                    &mut writer,
                    id,
                    crate::protocol::HTTP_REQUEST_METHOD,
                    &crate::protocol::HttpRequestParams {
                        method: "GET".into(),
                        url: format!(
                            "http://{}/must-not-send",
                            listener.local_addr().expect("address")
                        ),
                        headers: Vec::new(),
                        body: None,
                        timeout_ms: Some(50),
                        redirect_policy: crate::protocol::HttpRedirectPolicy::Follow,
                        request_id: "reusable-stream".into(),
                        stream_response: true,
                    },
                )
                .await;
                results.push(timeout(Duration::from_millis(500), lines.next_line()).await);
            }
            drop(cache_guard);
            release_tx.send(()).expect("release worker");
            blocker.await.expect("worker joined");
            tokio::task::spawn_blocking(|| ())
                .await
                .expect("queued preparation drained");
            for (id, result) in [2, 3].into_iter().zip(results) {
                let line = result
                    .expect("request deadline must return through router")
                    .expect("read response")
                    .expect("response line");
                let message: JSONRPCMessage = serde_json::from_str(&line).expect("decode response");
                let JSONRPCMessage::Error(error) = message else {
                    panic!("expected timeout error: {message:?}");
                };
                assert_eq!(error.id, RequestId::Integer(id));
                assert_eq!(
                    error.error.message,
                    "http/request timed out preparing HTTP client"
                );
            }
            assert!(
                timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err(),
                "expired routed requests must not connect after preparation resumes"
            );
            drop(writer);
            drop(lines);
            timeout(Duration::from_secs(1), task)
                .await
                .expect("processor exits")
                .expect("processor joins");
        });
    }

    #[tokio::test]
    async fn transport_disconnect_detaches_session_during_in_flight_read() {
        let registry = SessionRegistry::new(crate::ExecServerTelemetry::default());
        let (mut first_writer, mut first_lines, first_task) =
            spawn_test_connection(Arc::clone(&registry), "first");

        send_request(
            &mut first_writer,
            /*id*/ 1,
            INITIALIZE_METHOD,
            &InitializeParams {
                client_name: "exec-server-test".to_string(),
                resume_session_id: None,
            },
        )
        .await;
        let initialize_response: InitializeResponse =
            read_response(&mut first_lines, /*expected_id*/ 1).await;
        send_notification(&mut first_writer, INITIALIZED_METHOD, &()).await;

        let process_id = ProcessId::from("proc-long-poll");
        send_request(
            &mut first_writer,
            /*id*/ 2,
            EXEC_METHOD,
            &exec_params(process_id.clone()),
        )
        .await;
        let _: ExecResponse = read_response(&mut first_lines, /*expected_id*/ 2).await;

        send_request(
            &mut first_writer,
            /*id*/ 3,
            EXEC_READ_METHOD,
            &ReadParams {
                process_id: process_id.clone(),
                after_seq: None,
                max_bytes: None,
                wait_ms: Some(5_000),
            },
        )
        .await;
        timeout(
            Duration::from_secs(1),
            registry
                .process_for_test(&initialize_response.session_id)
                .await
                .wait_for_read_wait(),
        )
        .await
        .expect("read must be pending before disconnect");
        drop(first_writer);
        timeout(Duration::from_secs(1), first_task)
            .await
            .expect("disconnect must cancel the pending read")
            .expect("first processor should join");

        let (mut second_writer, mut second_lines, second_task) =
            spawn_test_connection(Arc::clone(&registry), "second");
        send_request(
            &mut second_writer,
            /*id*/ 1,
            INITIALIZE_METHOD,
            &InitializeParams {
                client_name: "exec-server-test".to_string(),
                resume_session_id: Some(initialize_response.session_id.clone()),
            },
        )
        .await;
        let second_initialize_response = timeout(
            Duration::from_secs(1),
            read_response::<InitializeResponse>(&mut second_lines, /*expected_id*/ 1),
        )
        .await
        .expect("resume initialize should not wait for the old read to finish");
        assert_eq!(
            second_initialize_response.session_id,
            initialize_response.session_id
        );
        send_notification(&mut second_writer, INITIALIZED_METHOD, &()).await;

        send_request(
            &mut second_writer,
            /*id*/ 2,
            EXEC_TERMINATE_METHOD,
            &TerminateParams { process_id },
        )
        .await;
        let _: TerminateResponse = read_response(&mut second_lines, /*expected_id*/ 2).await;

        drop(second_writer);
        drop(second_lines);
        timeout(Duration::from_secs(1), second_task)
            .await
            .expect("second processor should exit")
            .expect("second processor should join");
    }

    #[test]
    fn same_connection_terminate_cancels_preparation_before_native_launch() {
        struct Release(Option<std::sync::mpsc::Sender<()>>);
        impl Drop for Release {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let registry = SessionRegistry::new(crate::ExecServerTelemetry::default());
            let (mut writer, mut lines, task) =
                spawn_test_connection(registry.clone(), "same-connection-cancel");
            send_request(
                &mut writer,
                1,
                INITIALIZE_METHOD,
                &InitializeParams {
                    client_name: "cancel".into(),
                    resume_session_id: None,
                },
            )
            .await;
            let initialized: InitializeResponse = read_response(&mut lines, 1).await;
            send_notification(&mut writer, INITIALIZED_METHOD, &()).await;
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let release = Release(Some(release_tx));
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = entered_tx.send(());
                let _ = release_rx.recv_timeout(Duration::from_secs(10));
            });
            entered_rx.await.unwrap();
            let temp = tempfile::tempdir().unwrap();
            let forbidden = temp.path().join("must-not-run");
            let id = ProcessId::from("cancel-before-launch");
            let mut params = exec_params(id.clone());
            params.argv = vec![
                std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()),
                "/C".into(),
                format!("echo forbidden>\"{}\"", forbidden.display()),
            ];
            send_request(&mut writer, 2, EXEC_METHOD, &params).await;
            let process = registry.process_for_test(&initialized.session_id).await;
            timeout(Duration::from_secs(1), async {
                loop {
                    let result = process
                        .exec_read(ReadParams {
                            process_id: id.clone(),
                            after_seq: None,
                            max_bytes: None,
                            wait_ms: Some(0),
                        })
                        .await;
                    if result
                        .is_err_and(|error| error.message == format!("process id {id} is starting"))
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            // Signals may wait on preparation but must not exhaust cleanup.
            for request_id in 10..18 {
                send_request(&mut writer, request_id, crate::protocol::EXEC_SIGNAL_METHOD,
                    &crate::protocol::SignalParams { process_id: id.clone(), signal: crate::protocol::ProcessSignal::Interrupt }).await;
            }
            send_request(&mut writer, 19, crate::protocol::HTTP_REQUEST_CANCEL_METHOD,
                &crate::protocol::HttpRequestCancelParams { request_id: "absent".into() }).await;
            let mut pending = std::collections::VecDeque::new();
            let _: () = timeout(
                Duration::from_secs(1),
                read_response_preserving_other_messages(&mut lines, 19, &mut pending),
            )
            .await
            .expect("signals must not exhaust HTTP cleanup");
            send_request(
                &mut writer,
                3,
                EXEC_TERMINATE_METHOD,
                &TerminateParams {
                    process_id: id.clone(),
                },
            )
            .await;
            let first: TerminateResponse =
                timeout(Duration::from_secs(1), read_response_preserving_other_messages(&mut lines, 3, &mut pending))
                    .await
                    .expect("control must not wait for preparation");
            assert!(first.running);
            send_request(
                &mut writer,
                4,
                EXEC_TERMINATE_METHOD,
                &TerminateParams { process_id: id },
            )
            .await;
            let settled: TerminateResponse =
                timeout(Duration::from_secs(1), read_response_preserving_other_messages(&mut lines, 4, &mut pending))
                    .await
                    .unwrap();
            assert!(!settled.running);
            assert!(!forbidden.exists());
            drop(release);
            blocker.await.unwrap();
            let mut receipts = std::collections::HashSet::new();
            for _ in 0..9 {
                let message = if let Some(message) = pending.pop_front() {
                    message
                } else {
                    let line = timeout(Duration::from_secs(2), lines.next_line()).await.unwrap().unwrap().unwrap();
                    serde_json::from_str::<JSONRPCMessage>(&line).unwrap()
                };
                let request_id = match message {
                    JSONRPCMessage::Error(error) => error.id,
                    JSONRPCMessage::Response(response) => {
                        assert_ne!(response.id, RequestId::Integer(2), "cancelled start must fail");
                        response.id
                    }
                    other => panic!("unexpected message: {other:?}"),
                };
                assert!(receipts.insert(request_id));
            }
            assert!(receipts.contains(&RequestId::Integer(2)));
            for id in 10..18 { assert!(receipts.contains(&RequestId::Integer(id))); }
            tokio::task::spawn_blocking(|| ()).await.unwrap();
            assert!(!forbidden.exists());
            drop(writer);
            drop(lines);
            timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap();
            registry.shutdown().await;
        });
    }

    #[test]
    fn transport_disconnect_cleans_pending_start_before_process_id_reuse() {
        struct ReleaseWorker(Option<std::sync::mpsc::Sender<()>>);
        impl Drop for ReleaseWorker {
            fn drop(&mut self) {
                if let Some(release) = self.0.take() {
                    let _ = release.send(());
                }
            }
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let registry = SessionRegistry::new(crate::ExecServerTelemetry::default());
            let (mut first_writer, mut first_lines, first_task) =
                spawn_test_connection(Arc::clone(&registry), "cancelled-start");
            send_request(
                &mut first_writer,
                1,
                INITIALIZE_METHOD,
                &InitializeParams {
                    client_name: "exec-server-test".to_string(),
                    resume_session_id: None,
                },
            )
            .await;
            let initialized: InitializeResponse = read_response(&mut first_lines, 1).await;
            send_notification(&mut first_writer, INITIALIZED_METHOD, &()).await;
            let process = registry.process_for_test(&initialized.session_id).await;

            // Occupy the actual Windows process-spawn worker, not a fake spawn.
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let release_worker = ReleaseWorker(Some(release_tx));
            let worker = tokio::task::spawn_blocking(move || {
                let _ = entered_tx.send(());
                release_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("worker released");
            });
            entered_rx.await.expect("blocking worker entered");
            let process_id = ProcessId::from("cancelled-process-start");
            let temp = tempfile::TempDir::new().expect("temp directory");
            let forbidden = temp.path().join("cancelled-child-ran.txt");
            let mut first_params = exec_params(process_id.clone());
            first_params.argv = vec![
                std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()),
                "/C".to_string(),
                format!("echo forbidden>\"{}\"", forbidden.display()),
            ];
            send_request(&mut first_writer, 2, EXEC_METHOD, &first_params).await;
            timeout(Duration::from_secs(2), async {
                loop {
                    let result = process
                        .exec_read(ReadParams {
                            process_id: process_id.clone(),
                            after_seq: None,
                            max_bytes: None,
                            wait_ms: Some(0),
                        })
                        .await;
                    if result.is_err_and(|error| {
                        error.message == format!("process id {process_id} is starting")
                    }) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("registered exec inserted its provisional entry");
            assert!(!forbidden.exists());

            drop(first_writer);
            drop(first_lines);
            timeout(Duration::from_secs(2), first_task)
                .await
                .expect("disconnect must cancel in-flight exec without the worker")
                .expect("first connection joined");
            let (mut writer, mut lines, task) =
                spawn_test_connection(Arc::clone(&registry), "resumed-after-cancelled-start");
            send_request(
                &mut writer,
                1,
                INITIALIZE_METHOD,
                &InitializeParams {
                    client_name: "exec-server-test".to_string(),
                    resume_session_id: Some(initialized.session_id.clone()),
                },
            )
            .await;
            let resumed: InitializeResponse = read_response(&mut lines, 1).await;
            assert_eq!(resumed.session_id, initialized.session_id);
            send_notification(&mut writer, INITIALIZED_METHOD, &()).await;

            // Observe cleanup through the registered RPC, before permitting spawn.
            timeout(Duration::from_secs(2), async {
                for request_id in 2.. {
                    send_request(
                        &mut writer,
                        request_id,
                        EXEC_READ_METHOD,
                        &ReadParams {
                            process_id: process_id.clone(),
                            after_seq: None,
                            max_bytes: None,
                            wait_ms: Some(0),
                        },
                    )
                    .await;
                    let line = lines
                        .next_line()
                        .await
                        .expect("read error")
                        .expect("error line");
                    let JSONRPCMessage::Error(error) =
                        serde_json::from_str(&line).expect("JSON-RPC error")
                    else {
                        panic!("cancelled start must not publish a process");
                    };
                    assert_eq!(error.id, RequestId::Integer(request_id));
                    if error.error.message == format!("unknown process id {process_id}") {
                        break;
                    }
                    assert_eq!(
                        error.error.message,
                        format!("process id {process_id} is starting")
                    );
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("cancelled reservation removed without TTL expiry");
            drop(release_worker);
            worker.await.expect("blocking gate joined");
            // FIFO on the sole blocking worker also waits for any queued spawn
            // result to be discarded; it must never resume the cancelled child.
            tokio::task::spawn_blocking(|| ())
                .await
                .expect("spawn work drained");
            assert!(!forbidden.exists());

            send_request(
                &mut writer,
                10_000,
                EXEC_METHOD,
                &exec_params(process_id.clone()),
            )
            .await;
            let accepted: ExecResponse =
                timeout(Duration::from_secs(3), read_response(&mut lines, 10_000))
                    .await
                    .expect("same process ID is reusable");
            assert_eq!(accepted.process_id, process_id);
            send_request(
                &mut writer,
                10_001,
                EXEC_TERMINATE_METHOD,
                &TerminateParams { process_id },
            )
            .await;
            let terminated: TerminateResponse = read_response(&mut lines, 10_001).await;
            assert!(terminated.running);
            drop(writer);
            drop(lines);
            timeout(Duration::from_secs(2), task)
                .await
                .expect("second connection ended")
                .expect("second joined");
            registry.shutdown().await;
        });
    }

    #[tokio::test]
    async fn dropping_remote_http_body_stops_producer_and_keeps_connection_usable() {
        use tokio::io::AsyncReadExt;
        let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/stream", http.local_addr().unwrap());
        let producer = tokio::spawn(async move {
            let (mut socket, _) = http.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\ntest\r\n").await.unwrap();
            let mut byte = [0];
            // The response never completes normally: consumer abandonment must close it.
            assert_eq!(socket.read(&mut byte).await.unwrap(), 0);
        });
        let registry = SessionRegistry::new(crate::ExecServerTelemetry::default());
        let (client_writer, server_reader) = duplex(1 << 20);
        let (server_writer, client_reader) = duplex(1 << 20);
        let server = tokio::spawn(run_connection(
            JsonRpcConnection::from_stdio(server_reader, server_writer, "cancel-server".into()),
            registry.clone(),
            test_runtime_paths(),
            crate::ExecServerTelemetry::default(),
            crate::telemetry::ConnectionTransport::Stdio,
        ));
        let client = crate::ExecServerClient::connect(
            JsonRpcConnection::from_stdio(client_reader, client_writer, "cancel-client".into()),
            crate::ExecServerClientConnectOptions {
                client_name: "cancel-test".into(),
                initialize_timeout: Duration::from_secs(5),
                resume_session_id: None,
            },
        )
        .await
        .unwrap();
        let (_, mut body) = client
            .http_request_stream(crate::protocol::HttpRequestParams {
                method: "GET".into(),
                url,
                headers: vec![],
                body: None,
                timeout_ms: None,
                redirect_policy: Default::default(),
                request_id: "ignored".into(),
                stream_response: true,
            })
            .await
            .unwrap();
        assert_eq!(
            timeout(Duration::from_secs(5), body.recv())
                .await
                .unwrap()
                .unwrap(),
            Some(b"test".to_vec())
        );
        drop(body);
        timeout(Duration::from_secs(5), producer)
            .await
            .expect("cancellation must reach the HTTP producer")
            .unwrap();
        timeout(Duration::from_secs(5), client.environment_info())
            .await
            .unwrap()
            .expect("unrelated RPC remains usable");
        drop(client);
        server.abort();
        registry.shutdown().await;
    }

    fn spawn_test_connection(
        registry: Arc<SessionRegistry>,
        label: &str,
    ) -> (DuplexStream, Lines<BufReader<DuplexStream>>, JoinHandle<()>) {
        let (client_writer, server_reader) = duplex(1 << 20);
        let (server_writer, client_reader) = duplex(1 << 20);
        let connection =
            JsonRpcConnection::from_stdio(server_reader, server_writer, label.to_string());
        let task = tokio::spawn(run_connection(
            connection,
            registry,
            test_runtime_paths(),
            crate::ExecServerTelemetry::default(),
            crate::telemetry::ConnectionTransport::Stdio,
        ));
        (client_writer, BufReader::new(client_reader).lines(), task)
    }

    fn test_runtime_paths() -> ExecServerRuntimePaths {
        ExecServerRuntimePaths::new(std::env::current_exe().expect("current exe"))
            .expect("runtime paths")
    }

    async fn send_request<P: Serialize>(
        writer: &mut DuplexStream,
        id: i64,
        method: &str,
        params: &P,
    ) {
        write_message(
            writer,
            &JSONRPCMessage::Request(JSONRPCRequest {
                id: RequestId::Integer(id),
                method: method.to_string(),
                params: Some(serde_json::to_value(params).expect("serialize params")),
                trace: None,
            }),
        )
        .await;
    }

    async fn send_notification<P: Serialize>(writer: &mut DuplexStream, method: &str, params: &P) {
        write_message(
            writer,
            &JSONRPCMessage::Notification(JSONRPCNotification {
                method: method.to_string(),
                params: Some(serde_json::to_value(params).expect("serialize params")),
            }),
        )
        .await;
    }

    async fn write_message(writer: &mut DuplexStream, message: &JSONRPCMessage) {
        let encoded = serde_json::to_vec(message).expect("serialize JSON-RPC message");
        writer.write_all(&encoded).await.expect("write request");
        writer.write_all(b"\n").await.expect("write newline");
    }

    async fn read_response_preserving_other_messages<T: DeserializeOwned>(
        lines: &mut Lines<BufReader<DuplexStream>>,
        expected_id: i64,
        pending: &mut std::collections::VecDeque<JSONRPCMessage>,
    ) -> T {
        loop {
            let line = lines
                .next_line()
                .await
                .expect("read response")
                .expect("response line");
            match serde_json::from_str::<JSONRPCMessage>(&line).expect("decode JSON-RPC response") {
                JSONRPCMessage::Response(JSONRPCResponse { id, result })
                    if id == RequestId::Integer(expected_id) =>
                {
                    return serde_json::from_value(result).expect("decode response result");
                }
                JSONRPCMessage::Error(error) if error.id == RequestId::Integer(expected_id) => {
                    panic!("unexpected JSON-RPC error: {error:?}");
                }
                other => pending.push_back(other),
            }
        }
    }

    async fn read_response<T: DeserializeOwned>(
        lines: &mut Lines<BufReader<DuplexStream>>,
        expected_id: i64,
    ) -> T {
        let line = lines
            .next_line()
            .await
            .expect("read response")
            .expect("response line");
        match serde_json::from_str::<JSONRPCMessage>(&line).expect("decode JSON-RPC response") {
            JSONRPCMessage::Response(JSONRPCResponse { id, result }) => {
                assert_eq!(id, RequestId::Integer(expected_id));
                serde_json::from_value(result).expect("decode response result")
            }
            JSONRPCMessage::Error(error) => panic!("unexpected JSON-RPC error: {error:?}"),
            other => panic!("expected JSON-RPC response, got {other:?}"),
        }
    }

    fn exec_params(process_id: ProcessId) -> ExecParams {
        let mut env = HashMap::new();
        if let Some(path) = std::env::var_os("PATH") {
            env.insert("PATH".to_string(), path.to_string_lossy().into_owned());
        }
        ExecParams {
            process_id,
            argv: sleep_then_print_argv(),
            cwd: PathUri::from_host_native_path(std::env::current_dir().expect("cwd"))
                .expect("cwd URI"),
            env_policy: None,
            env,
            tty: false,
            pipe_stdin: false,
            arg0: None,
            sandbox: None,
            enforce_managed_network: false,
            managed_network: None,
        }
    }

    fn sleep_then_print_argv() -> Vec<String> {
        vec![
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()),
            "/C".to_string(),
            "ping -n 3 127.0.0.1 >NUL && echo late".to_string(),
        ]
    }
}
