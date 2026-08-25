use super::*;
use crate::outgoing_message::OutgoingResponse;
use codex_app_server_protocol::ConfigWarningNotification;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::TurnReasoningPolicySummaryNotification;
use codex_app_server_protocol::TurnReasoningPolicyUpdatedNotification;
use codex_protocol::protocol::ReasoningPolicyHistory;
use codex_protocol::protocol::ReasoningPolicyPhase;
use codex_protocol::protocol::ReasoningPolicySnapshot;
use codex_protocol::protocol::ReasoningPolicySource;
use codex_protocol::protocol::ReasoningPolicyTrigger;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::time::Duration;
use tokio::time::timeout;

fn absolute_path(path: &str) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(path).expect("absolute path")
}

fn reasoning_policy_updated_notification() -> ServerNotification {
    ServerNotification::TurnReasoningPolicyUpdated(TurnReasoningPolicyUpdatedNotification {
        thread_id: "thread-1".to_string(),
        turn_id: "turn-1".to_string(),
        snapshot: ReasoningPolicySnapshot {
            sequence: 1,
            timestamp: 1,
            phase: ReasoningPolicyPhase::Orient,
            configured_effort: None,
            effective_effort: None,
            request_effort: None,
            source: ReasoningPolicySource::TurnFallback,
            model: "gpt-5".to_string(),
            trigger: ReasoningPolicyTrigger::UserInput,
        },
    })
}

fn reasoning_policy_summary_notification() -> ServerNotification {
    ServerNotification::TurnReasoningPolicySummary(TurnReasoningPolicySummaryNotification {
        thread_id: "thread-1".to_string(),
        turn_id: "turn-1".to_string(),
        history: ReasoningPolicyHistory {
            turn_id: "turn-1".to_string(),
            entries: Vec::new(),
            total_entries: 2,
            truncated: false,
        },
    })
}

fn response_with_reasoning_policy_history() -> OutgoingMessage {
    OutgoingMessage::Response(OutgoingResponse {
        id: RequestId::Integer(99),
        result: json!({
            "thread": {
                "turns": [{
                    "id": "turn-1",
                    "reasoningPolicyHistory": {
                        "turnId": "turn-1",
                        "entries": [],
                        "totalEntries": 0,
                        "truncated": false
                    }
                }]
            }
        }),
    })
}

#[tokio::test]
async fn envelope_target_selection_preserves_the_only_delivery_path_difference() {
    let initialized_connection_id = ConnectionId(1);
    let uninitialized_connection_id = ConnectionId(2);
    let missing_connection_id = ConnectionId(3);
    let (initialized_writer_tx, mut initialized_writer_rx) = mpsc::channel(1);
    let (uninitialized_writer_tx, mut uninitialized_writer_rx) = mpsc::channel(1);
    let mut connections = HashMap::new();
    connections.insert(
        initialized_connection_id,
        OutboundConnectionState::new(
            initialized_writer_tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::new())),
            /*disconnect_sender*/ None,
        ),
    );
    connections.insert(
        uninitialized_connection_id,
        OutboundConnectionState::new(
            uninitialized_writer_tx,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(HashSet::new())),
            /*disconnect_sender*/ None,
        ),
    );

    let notification = OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
        ConfigWarningNotification {
            summary: "target-selection".to_string(),
            details: None,
            path: None,
            range: None,
        },
    ));
    let broadcast = OutgoingEnvelope::Broadcast {
        message: notification.clone(),
    };
    assert_eq!(
        target_connection_ids(&connections, &broadcast),
        vec![initialized_connection_id]
    );
    route_outgoing_envelope(&mut connections, broadcast).await;
    assert!(initialized_writer_rx.try_recv().is_ok());
    assert!(uninitialized_writer_rx.try_recv().is_err());

    for connection_id in [uninitialized_connection_id, missing_connection_id] {
        let direct = OutgoingEnvelope::ToConnection {
            connection_id,
            message: notification.clone(),
            write_complete_tx: None,
        };
        assert_eq!(
            target_connection_ids(&connections, &direct),
            vec![connection_id]
        );
    }

    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id: uninitialized_connection_id,
            message: notification,
            write_complete_tx: None,
        },
    )
    .await;
    assert!(uninitialized_writer_rx.try_recv().is_ok());
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

    let notification = reasoning_policy_updated_notification();
    assert_eq!(
        notification.experimental_reason(),
        Some("reasoningPolicyVisibility")
    );
    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: OutgoingMessage::AppServerNotification(notification),
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
            message: OutgoingMessage::AppServerNotification(reasoning_policy_updated_notification()),
            write_complete_tx: None,
        },
    )
    .await;

    let message = writer_rx
        .recv()
        .await
        .expect("experimental notification should reach opted-in client");
    let OutgoingMessage::AppServerNotification(ServerNotification::TurnReasoningPolicyUpdated(
        notification,
    )) = message.message
    else {
        panic!("expected reasoning-policy notification");
    };
    assert_eq!(notification.thread_id, "thread-1");
    assert_eq!(notification.turn_id, "turn-1");
    assert_eq!(notification.snapshot.sequence, 1);
    assert_eq!(notification.snapshot.phase, ReasoningPolicyPhase::Orient);
    assert_eq!(
        notification.snapshot.trigger,
        ReasoningPolicyTrigger::UserInput
    );
}

#[tokio::test]
async fn reasoning_policy_summary_notification_respects_capability() {
    let connection_id = ConnectionId(14);
    let (writer_tx, mut writer_rx) = mpsc::channel(1);
    let experimental_api_enabled = Arc::new(AtomicBool::new(false));

    let mut connections = HashMap::new();
    connections.insert(
        connection_id,
        OutboundConnectionState::new(
            writer_tx,
            Arc::new(AtomicBool::new(true)),
            experimental_api_enabled.clone(),
            Arc::new(RwLock::new(HashSet::new())),
            /*disconnect_sender*/ None,
        ),
    );

    let notification = reasoning_policy_summary_notification();
    assert_eq!(
        notification.experimental_reason(),
        Some("reasoningPolicyVisibility")
    );
    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: OutgoingMessage::AppServerNotification(notification),
            write_complete_tx: None,
        },
    )
    .await;
    assert!(writer_rx.try_recv().is_err());

    experimental_api_enabled.store(true, std::sync::atomic::Ordering::Release);
    route_outgoing_envelope(
        &mut connections,
        OutgoingEnvelope::ToConnection {
            connection_id,
            message: OutgoingMessage::AppServerNotification(
                reasoning_policy_summary_notification(),
            ),
            write_complete_tx: None,
        },
    )
    .await;

    let message = writer_rx
        .recv()
        .await
        .expect("summary should reach opted-in client");
    assert!(matches!(
        message.message,
        OutgoingMessage::AppServerNotification(ServerNotification::TurnReasoningPolicySummary(_))
    ));
}

#[tokio::test]
async fn reasoning_policy_history_is_omitted_without_capability() {
    let connection_id = ConnectionId(15);
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
            message: response_with_reasoning_policy_history(),
            write_complete_tx: None,
        },
    )
    .await;

    let message = writer_rx
        .recv()
        .await
        .expect("response should be delivered");
    let OutgoingMessage::Response(response) = message.message else {
        panic!("expected response");
    };
    assert!(
        response
            .result
            .pointer("/thread/turns/0/reasoningPolicyHistory")
            .is_none(),
        "history must be omitted for clients without the capability"
    );
}

#[tokio::test]
async fn reasoning_policy_history_is_preserved_with_capability() {
    let connection_id = ConnectionId(16);
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
            message: response_with_reasoning_policy_history(),
            write_complete_tx: None,
        },
    )
    .await;

    let message = writer_rx
        .recv()
        .await
        .expect("response should be delivered");
    let OutgoingMessage::Response(response) = message.message else {
        panic!("expected response");
    };
    assert_eq!(
        response
            .result
            .pointer("/thread/turns/0/reasoningPolicyHistory/turnId"),
        Some(&json!("turn-1"))
    );
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

    let (fast_writer_tx, mut fast_writer_rx) = mpsc::channel(1);
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

    let broadcast_message = OutgoingMessage::AppServerNotification(
        ServerNotification::ConfigWarning(ConfigWarningNotification {
            summary: "test".to_string(),
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
    assert!(!connections.contains_key(&slow_connection_id));
    assert!(slow_disconnect_token.is_cancelled());
    assert!(!fast_disconnect_token.is_cancelled());
    let fast_message = fast_writer_rx
        .try_recv()
        .expect("fast connection should receive the broadcast notification");
    assert!(matches!(
        fast_message.message,
        OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
            ConfigWarningNotification { summary, .. }
        )) if summary == "test"
    ));

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
async fn to_connection_stdio_waits_instead_of_disconnecting_when_writer_queue_is_full() {
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
        .await
    });

    let first = timeout(Duration::from_millis(100), writer_rx.recv())
        .await
        .expect("first queued message should be readable")
        .expect("first queued message should exist");
    timeout(Duration::from_millis(100), route_task)
        .await
        .expect("routing should finish after the first queued message is drained")
        .expect("routing task should succeed");

    assert!(matches!(
        first.message,
        OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
            ConfigWarningNotification { summary, .. }
        )) if summary == "queued"
    ));
    let second = writer_rx
        .try_recv()
        .expect("second notification should be delivered once the queue has room");
    assert!(matches!(
        second.message,
        OutgoingMessage::AppServerNotification(ServerNotification::ConfigWarning(
            ConfigWarningNotification { summary, .. }
        )) if summary == "second"
    ));
}
