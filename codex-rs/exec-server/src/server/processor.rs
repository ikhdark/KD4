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
use crate::server::registry::shared_router;
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


async fn run_connection_with_shutdown(
    connection: JsonRpcConnection,
    session_registry: Arc<SessionRegistry>,
    runtime_paths: ExecServerRuntimePaths,
    telemetry: ExecServerTelemetry,
    transport: ConnectionTransport,
    cancelled: CancellationToken,
) {
    let _connection_metrics = telemetry.connection_started(transport);
    let router = shared_router();
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
    let mut filesystem_cohort: Vec<Completion> = Vec::new();
    let mut filesystem_handle_reads: HashMap<String, Completion> = HashMap::new();
    let filesystem_capacity = Arc::new(tokio::sync::Semaphore::new(4));
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
                codex_exec_server_protocol::JSONRPCMessage::Request(mut request) => {
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
                                    | crate::protocol::FS_CHECKSUM_FILE_CANCEL_METHOD
                            );
                        let filesystem_read = initialized && matches!(method,
                            crate::protocol::FS_READ_FILE_METHOD
                                | crate::protocol::FS_READ_FILE_BOUNDED_METHOD
                                | crate::protocol::FS_READ_BLOCK_METHOD
                                | crate::protocol::FS_GET_METADATA_METHOD
                                | crate::protocol::FS_CANONICALIZE_METHOD
                                | crate::protocol::FS_READ_DIRECTORY_METHOD
                                | crate::protocol::FS_WALK_METHOD
                                | crate::protocol::FS_READ_FILE_RANGE_METHOD
                                | crate::protocol::FS_CHECKSUM_FILE_METHOD
                                | crate::protocol::LOOKUP_EXECUTABLE_METHOD);
                        let independent_read = initialized
                            && (filesystem_read || matches!(
                                method,
                                crate::protocol::EXEC_READ_METHOD
                                    | crate::protocol::EXEC_SIGNAL_METHOD
                                    | crate::protocol::EXEC_TERMINATE_METHOD
                                    | crate::protocol::ENVIRONMENT_INFO_METHOD
                                    | crate::protocol::HTTP_REQUEST_METHOD
                                    | crate::protocol::HTTP_REQUEST_CANCEL_METHOD
                                    | crate::protocol::FS_CHECKSUM_FILE_CANCEL_METHOD
                            ));
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
                        let process_id = request.params.as_ref()
                            .and_then(|params| params.get("processId"))
                            .and_then(serde_json::Value::as_str).map(str::to_string);
                        let admitted_http = (initialized && method == crate::protocol::HTTP_REQUEST_METHOD)
                            .then(|| crate::rpc::decode_request_params::<crate::protocol::HttpRequestParams>(request.params.take()));
                        if let Some(Ok(params)) = &admitted_http
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
                        // Act on the exact stream or checksum present at admission,
                        // never a same-named replacement reached by a deferred lookup.
                        let admitted_cancel = if !initialized {
                            None
                        } else if method == crate::protocol::HTTP_REQUEST_CANCEL_METHOD {
                            Some(match serde_json::from_value::<crate::protocol::HttpRequestCancelParams>(request.params.clone().unwrap_or_default()) {
                                Ok(params) => handler.cancel_http_body_stream(&params.request_id).await,
                                Err(error) => Err(crate::rpc::invalid_params(error.to_string())),
                            })
                        } else if method == crate::protocol::FS_CHECKSUM_FILE_CANCEL_METHOD {
                            Some(match serde_json::from_value::<crate::protocol::FsChecksumFileCancelParams>(request.params.clone().unwrap_or_default()) {
                                Ok(params) => handler.cancel_checksum_file(&params.cancel_id),
                                Err(error) => Err(crate::rpc::invalid_params(error.to_string())),
                            })
                        } else { None };
                        // A checksum keeps its place in the read cohort that later
                        // writes and starts await. Register its cancel id before it
                        // queues: the cancel bypasses ordering and may arrive first.
                        let admitted_checksum = (initialized && method == crate::protocol::FS_CHECKSUM_FILE_METHOD)
                            .then(|| request.params.as_ref()?.get("cancelId")?.as_str().map(str::to_string))
                            .flatten()
                            .map(|cancel_id| handler.reserve_checksum_cancel(cancel_id));
                        filesystem_handle_reads.retain(|_, completion| completion.clone().now_or_never().is_none());
                        let read_handle = (method == crate::protocol::FS_READ_BLOCK_METHOD)
                            .then(|| request.params.as_ref()?.get("handleId")?.as_str().map(str::to_string))
                            .flatten();
                        let mut predecessor = if admitted_terminate.is_some() || admitted_cancel.is_some() {
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
                        } else if filesystem_read {
                            let ordered_barrier = last_ordered.clone();
                            let previous_read = read_handle.as_ref()
                                .and_then(|handle| filesystem_handle_reads.get(handle)).cloned();
                            Some(async move {
                                if let Some(barrier) = ordered_barrier { barrier.await; }
                                // A read failure retires this handle. Preserve
                                // same-handle error/close ordering, not a global
                                // chain between independent file handles.
                                if let Some(previous) = previous_read { previous.await; }
                            }.boxed().shared())
                        } else if independent_read {
                            process_id
                                .as_ref()
                                .and_then(|id| pending_starts.get(id))
                                .map(|(completion, _)| completion.clone())
                        } else {
                            last_ordered.take()
                        };
                        filesystem_cohort.retain(|completion| completion.clone().now_or_never().is_none());
                        if initialized && !independent_read && method != crate::protocol::EXEC_WRITE_METHOD
                            && !filesystem_cohort.is_empty()
                        {
                            // Reads form a finite cohort between mutation/start
                            // barriers. Never run a write/open/close ahead of any
                            // earlier read, including its response admission.
                            let cohort = std::mem::take(&mut filesystem_cohort);
                            predecessor = Some(async move {
                                if let Some(previous) = predecessor { previous.await; }
                                futures::future::join_all(cohort).await;
                            }.boxed().shared());
                        }
                        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel::<()>();
                        if initialized && (!independent_read || filesystem_read) {
                            let completion = async move {
                                let _ = completion_rx.await;
                            }
                            .boxed()
                            .shared();
                            if filesystem_read {
                                filesystem_cohort.push(completion.clone());
                                if let Some(handle) = read_handle {
                                    filesystem_handle_reads.insert(handle, completion.clone());
                                }
                            } else if method == crate::protocol::EXEC_WRITE_METHOD {
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
                        } else if let Some(result) = admitted_cancel {
                            Box::pin(async move { Some(process_response(request.id, result)) })
                        } else if let Some(admitted) = admitted_checksum {
                            match admitted {
                                Ok(cancellation) => {
                                    let request_id = request.id.clone();
                                    let response = route(Arc::clone(&handler), request);
                                    // Dropping the route stops local hashing at its next
                                    // chunk or terminates the sandbox helper; dropping
                                    // `cancellation` afterwards releases the id.
                                    Box::pin(async move {
                                        tokio::select! {
                                            biased;
                                            _ = cancellation.cancelled() => Some(RpcServerOutboundMessage::Error {
                                                request_id,
                                                error: invalid_request("fs/checksumFile cancelled".into()),
                                            }),
                                            message = response => message,
                                        }
                                    })
                                }
                                Err(error) => Box::pin(async move {
                                    Some(RpcServerOutboundMessage::Error { request_id: request.id, error })
                                }),
                            }
                        } else if let Some(admitted) = admitted_http {
                            let handler = Arc::clone(&handler);
                            Box::pin(async move {
                                let result = match admitted {
                                    Ok(params) => handler.http_request(request.id.clone(), params).await,
                                    Err(error) => Err(error),
                                };
                                result.err().map(|error| RpcServerOutboundMessage::Error {
                                    request_id: request.id, error,
                                })
                            })
                        } else {
                            route(Arc::clone(&handler), request)
                        };
                        let response = response.instrument(request_span.clone());
                        let request_cancelled = cancelled.clone();
                        let outgoing_tx = outgoing_tx.clone();
                        let read_capacity = filesystem_read.then(|| Arc::clone(&filesystem_capacity));
                        let operation = async move {
                            // Dropping the sender also releases successors during cancellation.
                            let _completion = completion_tx;
                            if let Some(predecessor) = predecessor {
                                tokio::select! {
                                    _ = request_cancelled.cancelled() => return false,
                                    _ = predecessor => {}
                                }
                            }
                            let _read_permit = if let Some(capacity) = read_capacity {
                                tokio::select! {
                                    _ = request_cancelled.cancelled() => return false,
                                    permit = capacity.acquire_owned() => match permit {
                                        Ok(permit) => Some(permit),
                                        Err(_) => return false,
                                    },
                                }
                            } else { None };
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

