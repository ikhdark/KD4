use super::*;
use codex_app_server_protocol::AgentMessageDeltaNotification;
use codex_app_server_protocol::ConfigWarningNotification;
use codex_app_server_protocol::McpToolCallProgressNotification;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadRealtimeStartedNotification;
use codex_protocol::protocol::RealtimeConversationVersion;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::time::Duration;
use tokio::time::timeout;

fn absolute_path(path: &str) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(path).expect("absolute path")
}

fn thread_realtime_started_notification() -> ServerNotification {
    ServerNotification::ThreadRealtimeStarted(ThreadRealtimeStartedNotification {
        thread_id: "thread-1".to_string(),
        realtime_session_id: None,
        version: RealtimeConversationVersion::V1,
    })
}

#[tokio::test]
async fn to_connection_notification_respects_opt_out_filters() {
    let connection_id = ConnectionId(7);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);
    let initialized = Arc::new(AtomicBool::new(true));
    let opted_out_notification_methods =
        Arc::new(RwLock::new(HashSet::from(["configWarning".to_string()])));

    let mut connections = HashMap::new();
    connections.insert(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            initialized,
            Arc::new(AtomicBool::new(true)),
            opted_out_notification_methods,
            /*disconnect_sender*/ None,
        ),
    );

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
                ConfigWarningNotification {
                    summary: "task_started".to_string(),
                    details: None,
                    path: None,
                    range: None,
                },
            )),
            write_complete_tx: None,
        },
    )
    .await;

    assert!(
        writer_rx.try_recv().is_err(),
        "opted-out notification should be dropped"
    );
}

#[tokio::test]
async fn snapshot_accepted_notification_uses_the_frozen_filter_decision() {
    let connection_id = ConnectionId(8);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);
    let opted_out_notification_methods =
        Arc::new(RwLock::new(HashSet::from(["configWarning".to_string()])));
    let mut connections = HashMap::new();
    connections.insert(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            opted_out_notification_methods,
            /*disconnect_sender*/ None,
        ),
    );

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToSnapshotAcceptedConnection {
            connection_id,
            message: OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
                ConfigWarningNotification {
                    summary: "accepted-before-filter-change".to_string(),
                    details: None,
                    path: None,
                    range: None,
                },
            )),
            write_complete_tx: None,
        },
    )
    .await;

    let queued = writer_rx
        .recv()
        .await
        .expect("snapshot-accepted notification should bypass live re-filtering");
    assert!(matches!(
        queued.message,
        OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
            ConfigWarningNotification { summary, .. }
        )) if summary == "accepted-before-filter-change"
    ));
}

#[tokio::test]
async fn to_connection_notifications_are_dropped_for_opted_out_clients() {
    let connection_id = ConnectionId(10);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);

    let mut connections = HashMap::new();
    connections.insert(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::from(["configWarning".to_string()]))),
            /*disconnect_sender*/ None,
        ),
    );

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
                ConfigWarningNotification {
                    summary: "task_started".to_string(),
                    details: None,
                    path: None,
                    range: None,
                },
            )),
            write_complete_tx: None,
        },
    )
    .await;

    assert!(
        writer_rx.try_recv().is_err(),
        "opted-out notifications should not reach clients"
    );
}

#[tokio::test]
async fn to_connection_notifications_are_preserved_for_non_opted_out_clients() {
    let connection_id = ConnectionId(11);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);

    let mut connections = HashMap::new();
    connections.insert(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::new())),
            /*disconnect_sender*/ None,
        ),
    );

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
                ConfigWarningNotification {
                    summary: "task_started".to_string(),
                    details: None,
                    path: None,
                    range: None,
                },
            )),
            write_complete_tx: None,
        },
    )
    .await;

    let message = writer_rx
        .recv()
        .await
        .expect("notification should reach non-opted-out clients");
    assert!(matches!(
        message.message,
        OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
            ConfigWarningNotification { summary, .. }
        )) if summary == "task_started"
    ));
}

#[tokio::test]
async fn to_connection_receipt_sender_reaches_the_transport_writer() {
    let connection_id = ConnectionId(14);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);
    let (write_complete_tx, write_complete_rx) = tokio::sync::oneshot::channel();

    let mut connections = HashMap::new();
    connections.insert(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::new())),
            /*disconnect_sender*/ None,
        ),
    );

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
                ConfigWarningNotification {
                    summary: "receipt-probe".to_string(),
                    details: None,
                    path: None,
                    range: None,
                },
            )),
            write_complete_tx: Some(write_complete_tx),
        },
    )
    .await;

    let queued_message = writer_rx
        .recv()
        .await
        .expect("notification should reach the transport writer");
    queued_message
        .write_complete_tx
        .expect("routing must preserve the receipt sender")
        .send(())
        .expect("receipt receiver should remain live");
    write_complete_rx
        .await
        .expect("transport completion should reach the receipt collector");
}

#[tokio::test]
async fn experimental_notifications_are_dropped_without_capability() {
    let connection_id = ConnectionId(12);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);

    let mut connections = HashMap::new();
    connections.insert(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(RwLock::new(HashSet::new())),
            /*disconnect_sender*/ None,
        ),
    );

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: OutgoingMessage::AppServerNotification(thread_realtime_started_notification()),
            write_complete_tx: None,
        },
    )
    .await;

    assert!(
        writer_rx.try_recv().is_err(),
        "experimental notifications should not reach clients without capability"
    );
}

#[tokio::test]
async fn experimental_notifications_are_preserved_with_capability() {
    let connection_id = ConnectionId(13);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);

    let mut connections = HashMap::new();
    connections.insert(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::new())),
            /*disconnect_sender*/ None,
        ),
    );

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: OutgoingMessage::AppServerNotification(thread_realtime_started_notification()),
            write_complete_tx: None,
        },
    )
    .await;

    let message = writer_rx
        .recv()
        .await
        .expect("experimental notification should reach opted-in client");
    assert!(matches!(
        message.message,
        OutgoingMessage::AppServerNotification(ServerNotification::ThreadRealtimeStarted(_))
    ));
}

#[tokio::test]
async fn command_execution_request_approval_strips_additional_permissions_without_capability() {
    let connection_id = ConnectionId(8);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);

    let mut connections = HashMap::new();
    connections.insert(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(RwLock::new(HashSet::new())),
            /*disconnect_sender*/ None,
        ),
    );

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: OutgoingMessage::Request(ServerRequest::CommandExecutionRequestApproval {
                request_id: RequestId::Integer(1),
                params: codex_app_server_protocol::CommandExecutionRequestApprovalParams {
                    thread_id: "thr_123".to_string(),
                    turn_id: "turn_123".to_string(),
                    item_id: "call_123".to_string(),
                    started_at_ms: 0,
                    approval_id: None,
                    environment_id: None,
                    reason: Some("Need extra read access".to_string()),
                    network_approval_context: None,
                    command: Some("cat file".to_string()),
                    cwd: Some(absolute_path("/tmp").into()),
                    command_actions: None,
                    additional_permissions: Some(
                        codex_app_server_protocol::AdditionalPermissionProfile {
                            network: None,
                            file_system: Some(
                                codex_app_server_protocol::AdditionalFileSystemPermissions {
                                    read: Some(vec![absolute_path("/tmp/allowed").into()]),
                                    write: None,
                                    glob_scan_max_depth: None,
                                    entries: None,
                                },
                            ),
                        },
                    ),
                    proposed_execpolicy_amendment: None,
                    proposed_network_policy_amendments: None,
                    available_decisions: None,
                },
            }),
            write_complete_tx: None,
        },
    )
    .await;

    let message = writer_rx
        .recv()
        .await
        .expect("request should be delivered to the connection");
    let json = serde_json::to_value(message.message).expect("request should serialize");
    assert_eq!(json["params"].get("additionalPermissions"), None);
}

#[tokio::test]
async fn command_execution_request_approval_keeps_additional_permissions_with_capability() {
    let connection_id = ConnectionId(9);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);

    let mut connections = HashMap::new();
    connections.insert(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::new())),
            /*disconnect_sender*/ None,
        ),
    );

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: OutgoingMessage::Request(ServerRequest::CommandExecutionRequestApproval {
                request_id: RequestId::Integer(1),
                params: codex_app_server_protocol::CommandExecutionRequestApprovalParams {
                    thread_id: "thr_123".to_string(),
                    turn_id: "turn_123".to_string(),
                    item_id: "call_123".to_string(),
                    started_at_ms: 0,
                    approval_id: None,
                    environment_id: None,
                    reason: Some("Need extra read access".to_string()),
                    network_approval_context: None,
                    command: Some("cat file".to_string()),
                    cwd: Some(absolute_path("/tmp").into()),
                    command_actions: None,
                    additional_permissions: Some(
                        codex_app_server_protocol::AdditionalPermissionProfile {
                            network: None,
                            file_system: Some(
                                codex_app_server_protocol::AdditionalFileSystemPermissions {
                                    read: Some(vec![absolute_path("/tmp/allowed").into()]),
                                    write: None,
                                    glob_scan_max_depth: None,
                                    entries: None,
                                },
                            ),
                        },
                    ),
                    proposed_execpolicy_amendment: None,
                    proposed_network_policy_amendments: None,
                    available_decisions: None,
                },
            }),
            write_complete_tx: None,
        },
    )
    .await;

    let message = writer_rx
        .recv()
        .await
        .expect("request should be delivered to the connection");
    let json = serde_json::to_value(message.message).expect("request should serialize");
    let allowed_path = absolute_path("/tmp/allowed").to_string_lossy().into_owned();
    assert_eq!(
        json["params"]["additionalPermissions"],
        json!({
            "network": null,
            "fileSystem": {
                "read": [allowed_path],
            "write": null,
            },
        })
    );
}

#[tokio::test]
async fn broadcast_does_not_block_on_slow_connection() {
    let fast_connection_id = ConnectionId(1);
    let slow_connection_id = ConnectionId(2);

    let (fast_writer_tx, mut fast_writer_rx) = mpsc::channel(8);
    let (slow_writer_tx, mut slow_writer_rx) = mpsc::channel(1);
    let fast_disconnect_token = CancellationToken::new();
    let slow_disconnect_token = CancellationToken::new();

    let mut connections = HashMap::new();
    connections.insert(
        fast_connection_id,
        OutboundConnectionState::new(
            fast_writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::new())),
            Some(fast_disconnect_token.clone()),
        ),
    );
    connections.insert(
        slow_connection_id,
        OutboundConnectionState::new(
            slow_writer_tx.clone(),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::new())),
            Some(slow_disconnect_token.clone()),
        ),
    );

    let queued_message = OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
        ConfigWarningNotification {
            summary: "already-buffered".to_string(),
            details: None,
            path: None,
            range: None,
        },
    ));
    slow_writer_tx
        .try_send(QueuedOutgoingMessage::new(queued_message))
        .expect("channel should have room");

    for summary in ["test-1", "test-2", "test-3"] {
        let broadcast_message = OutgoingMessage::AppServerNotification(
            ServerNotification::ConfigWarning(ConfigWarningNotification {
                summary: summary.to_string(),
                details: None,
                path: None,
                range: None,
            }),
        );
        timeout(
            Duration::from_millis(100),
            route_outgoing_envelope(
                &mut connections,
                OutgoingEnvelope::Broadcast {
                    message: broadcast_message,
                },
            ),
        )
        .await
        .expect("broadcast should return even when one connection is slow");
    }
    assert!(!connections.contains_key(&slow_connection_id));
    assert!(slow_disconnect_token.is_cancelled());
    assert!(!fast_disconnect_token.is_cancelled());
    let mut fast_summaries = Vec::new();
    for _ in 0..3 {
        let fast_message = timeout(Duration::from_millis(100), fast_writer_rx.recv())
            .await
            .expect("fast connection should not be stalled by the slow connection")
            .expect("fast connection should remain open");
        let OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
            ConfigWarningNotification { summary, .. },
        )) = fast_message.message
        else {
            panic!("expected config warning broadcast");
        };
        fast_summaries.push(summary);
    }
    assert_eq!(
        fast_summaries,
        vec![
            "test-1".to_string(),
            "test-2".to_string(),
            "test-3".to_string(),
        ]
    );

    let slow_message = slow_writer_rx
        .try_recv()
        .expect("slow connection should retain its original buffered message");
    assert!(matches!(
        slow_message.message,
        OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
            ConfigWarningNotification { summary, .. }
        )) if summary == "already-buffered"
    ));
}

#[tokio::test]
async fn to_connection_stdio_accepts_into_bounded_mailbox_when_writer_queue_is_full() {
    let connection_id = ConnectionId(3);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);
    writer_tx
        .send(QueuedOutgoingMessage::new(
            OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
                ConfigWarningNotification {
                    summary: "queued".to_string(),
                    details: None,
                    path: None,
                    range: None,
                },
            )),
        ))
        .await
        .expect("channel should accept the first queued message");

    let mut connections = HashMap::new();
    connections.insert(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::new())),
            /*disconnect_sender*/ None,
        ),
    );

    let route_task = tokio::spawn(async move {
        route_outgoing_envelope(
            &mut connections,
            OutgoingEnvelope::ToConnection {
                connection_id,
                message: OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
                    ConfigWarningNotification {
                        summary: "second".to_string(),
                        details: None,
                        path: None,
                        range: None,
                    },
                )),
                write_complete_tx: None,
            },
        )
        .await;
        connections
    });

    let _connections = timeout(Duration::from_millis(100), route_task)
        .await
        .expect("routing should finish after bounded mailbox acceptance")
        .expect("routing task should succeed");
    let first = timeout(Duration::from_millis(100), writer_rx.recv())
        .await
        .expect("first queued message should be readable")
        .expect("first queued message should exist");

    assert!(matches!(
        first.message,
        OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
            ConfigWarningNotification { summary, .. }
        )) if summary == "queued"
    ));
    let second = timeout(Duration::from_millis(100), writer_rx.recv())
        .await
        .expect("mailbox should drain once the writer has room")
        .expect("second notification should be delivered once the queue has room");
    assert!(matches!(
        second.message,
        OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
            ConfigWarningNotification { summary, .. }
        )) if summary == "second"
    ));
}

#[tokio::test]
async fn saturated_writer_coalesces_same_stream_deltas_and_preserves_terminal_order() {
    let connection_id = ConnectionId(4);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);
    writer_tx
        .send(QueuedOutgoingMessage::new(
            OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
                ConfigWarningNotification {
                    summary: "already-buffered".to_string(),
                    details: None,
                    path: None,
                    range: None,
                },
            )),
        ))
        .await
        .expect("writer should accept the initial message");

    let mut connections = HashMap::new();
    connections.insert(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::new())),
            /*disconnect_sender*/ None,
        ),
    );

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: OutgoingMessage::AppServerNotification(
                ServerNotification::McpToolCallProgress(McpToolCallProgressNotification {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "item-1".to_string(),
                    message: "working".to_string(),
                }),
            ),
            write_complete_tx: None,
        },
    )
    .await;
    for delta in ["hello ", "world"] {
        route_outgoing_envelope(
            &mut connections,
            OutgoingEnvelope::ToConnection {
                connection_id,
                message: OutgoingMessage::AppServerNotification(
                    ServerNotification::AgentMessageDelta(AgentMessageDeltaNotification {
                        thread_id: "thread-1".to_string(),
                        turn_id: "turn-1".to_string(),
                        item_id: "item-1".to_string(),
                        delta: delta.to_string(),
                    }),
                ),
                write_complete_tx: None,
            },
        )
        .await;
    }
    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
                ConfigWarningNotification {
                    summary: "terminal".to_string(),
                    details: None,
                    path: None,
                    range: None,
                },
            )),
            write_complete_tx: None,
        },
    )
    .await;

    let initial = timeout(Duration::from_millis(100), writer_rx.recv())
        .await
        .expect("initial writer message should arrive")
        .expect("writer should remain open");
    assert!(matches!(
        initial.message,
        OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
            ConfigWarningNotification { summary, .. }
        )) if summary == "already-buffered"
    ));

    let mut reconstructed = String::new();
    loop {
        let message = timeout(Duration::from_millis(100), writer_rx.recv())
            .await
            .expect("mailbox should drain without losing transcript or terminal events")
            .expect("writer should remain open");
        match message.message {
            OutgoingMessage::AppServerNotification(ServerNotification::AgentMessageDelta(
                notification,
            )) => reconstructed.push_str(&notification.delta),
            OutgoingMessage::AppServerNotification(ServerNotification::McpToolCallProgress(_)) => {}
            OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
                ConfigWarningNotification { summary, .. },
            )) if summary == "terminal" => break,
            other => panic!("unexpected mailbox message: {other:?}"),
        }
    }
    assert_eq!(reconstructed, "hello world");
}
