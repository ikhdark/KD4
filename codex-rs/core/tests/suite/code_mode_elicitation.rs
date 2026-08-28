#![allow(clippy::unwrap_used)]

use std::time::Duration;

use anyhow::Result;
use codex_core::config::Config;
use codex_features::Feature;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::TaskCompletionStatus;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnTimingToolCallSource;
use codex_protocol::request_permissions::PermissionGrantScope;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use core_test_support::wait_for_event_with_timeout;
use wiremock::MockServer;

const YIELD_TIME_MS: u64 = 1_000;
const TURN_COMPLETE_TIMEOUT: Duration = Duration::from_secs(30);

struct CodeModeElicitationHarness {
    _server: MockServer,
    test: TestCodex,
    follow_up: ResponseMock,
    turn_id: String,
}

impl CodeModeElicitationHarness {
    async fn start(
        code: &str,
        permission_profile: PermissionProfile,
        configure: impl FnOnce(&mut Config) + Send + 'static,
    ) -> Result<Self> {
        let server = responses::start_mock_server().await;
        let mut builder =
            test_codex()
                .with_model("test-gpt-5.1-codex")
                .with_config(move |config| {
                    let _ = config.features.enable(Feature::CodeMode);
                    configure(config);
                });
        let test = builder.build_with_auto_env(&server).await?;
        let follow_up = mount_code_mode_responses(&server, code).await;
        let turn_id = submit_turn(&test, permission_profile).await?;
        Ok(Self {
            _server: server,
            test,
            follow_up,
            turn_id,
        })
    }

    async fn assert_result_held(&self) {
        tokio::time::sleep(Duration::from_millis(YIELD_TIME_MS + 250)).await;
        assert!(
            self.follow_up.requests().is_empty(),
            "captured exec result should not return during a user elicitation"
        );
    }

    async fn finish(self) {
        wait_for_event_with_timeout(
            &self.test.codex,
            |event| match event {
                EventMsg::TurnComplete(event) => event.turn_id == self.turn_id,
                _ => false,
            },
            TURN_COMPLETE_TIMEOUT,
        )
        .await;
        self.follow_up.single_request();
    }
}

async fn mount_code_mode_responses(server: &MockServer, code: &str) -> ResponseMock {
    responses::mount_sse_once(
        server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_custom_tool_call("call-1", "exec", code),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    responses::mount_sse_once(
        server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await
}

async fn submit_turn(test: &TestCodex, permission_profile: PermissionProfile) -> Result<String> {
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(permission_profile, test.config.cwd.as_path());
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "run a code-mode tool that needs user input".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                approval_policy: Some(AskForApproval::OnRequest),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;

    Ok(wait_for_event_match(&test.codex, |event| match event {
        EventMsg::TurnStarted(event) => Some(event.turn_id.clone()),
        _ => None,
    })
    .await)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_holds_yielded_result_during_command_approval() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = CodeModeElicitationHarness::start(
        r#"// @exec: {"yield_time_ms": 1000}
await tools.exec_command({
  cmd: "[Console]::Out.Write('code_mode_approval_marker')",
  sandbox_permissions: "require_escalated",
  justification: "test command approval",
});"#,
        PermissionProfile::read_only(),
        |_| {},
    )
    .await?;
    let approval = wait_for_event_match(&harness.test.codex, |event| match event {
        EventMsg::ExecApprovalRequest(approval) => Some(approval.clone()),
        _ => None,
    })
    .await;

    harness.assert_result_held().await;
    harness
        .test
        .codex
        .submit(Op::ExecApproval {
            id: approval.effective_approval_id(),
            turn_id: Some(harness.turn_id.clone()),
            decision: ReviewDecision::Approved,
        })
        .await?;
    harness.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_holds_yielded_result_during_patch_approval() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = CodeModeElicitationHarness::start(
        r#"// @exec: {"yield_time_ms": 1000}
await tools.apply_patch("*** Begin Patch\n*** Add File: code_mode_patch_approval.txt\n+held\n*** End Patch\n");"#,
        PermissionProfile::read_only(),
        |_| {},
    )
    .await?;
    let approval = wait_for_event_match(&harness.test.codex, |event| match event {
        EventMsg::ApplyPatchApprovalRequest(approval) => Some(approval.clone()),
        _ => None,
    })
    .await;

    harness.assert_result_held().await;
    harness
        .test
        .codex
        .submit(Op::PatchApproval {
            id: approval.call_id,
            decision: ReviewDecision::Approved,
        })
        .await?;
    harness.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_holds_yielded_result_during_permission_request() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = CodeModeElicitationHarness::start(
        r#"// @exec: {"yield_time_ms": 1000}
await tools.request_permissions({
  reason: "test permission request",
  permissions: { network: { enabled: true } },
});"#,
        PermissionProfile::read_only(),
        |config| {
            let _ = config.features.enable(Feature::RequestPermissionsTool);
        },
    )
    .await?;
    let request = wait_for_event(&harness.test.codex, |event| {
        matches!(
            event,
            EventMsg::RequestPermissions(_) | EventMsg::TurnComplete(_) | EventMsg::Error(_)
        )
    })
    .await;
    let EventMsg::RequestPermissions(request) = request else {
        panic!("expected request_permissions before turn completion, got {request:?}");
    };

    harness.assert_result_held().await;
    harness
        .test
        .codex
        .submit(Op::RequestPermissionsResponse {
            id: request.call_id,
            response: RequestPermissionsResponse {
                permissions: Default::default(),
                scope: PermissionGrantScope::Turn,
                strict_auto_review: false,
            },
        })
        .await?;
    harness.finish().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_nested_denial_completes_blocked_without_follow_up_or_error() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = CodeModeElicitationHarness::start(
        r#"// @exec: {"yield_time_ms": 1000}
await tools.apply_patch("*** Begin Patch\n*** Add File: code_mode_denied_patch.txt\n+denied\n*** End Patch\n");"#,
        PermissionProfile::read_only(),
        |config| {
            config
                .features
                .enable(Feature::TaskCompletionReviewer)
                .expect("enable completion reviewer to prove blocked outcome bypasses it");
        },
    )
    .await?;
    let approval = wait_for_event_match(&harness.test.codex, |event| match event {
        EventMsg::ApplyPatchApprovalRequest(approval) => Some(approval.clone()),
        _ => None,
    })
    .await;

    harness
        .test
        .codex
        .submit(Op::PatchApproval {
            id: approval.call_id.clone(),
            decision: ReviewDecision::Denied,
        })
        .await?;

    let (completed, errors) = tokio::time::timeout(TURN_COMPLETE_TIMEOUT, async {
        let mut errors = Vec::new();
        loop {
            let event = harness
                .test
                .codex
                .next_event()
                .await
                .expect("event stream should remain open");
            match event.msg {
                EventMsg::Error(error) => errors.push(error),
                EventMsg::TurnComplete(event) if event.turn_id == harness.turn_id => {
                    break (event, errors);
                }
                EventMsg::TurnAborted(event)
                    if event.turn_id.as_deref() == Some(harness.turn_id.as_str()) =>
                {
                    panic!("blocked tool outcome emitted TurnAborted")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for blocked completion");

    assert!(
        errors.is_empty(),
        "blocked completion must not emit a typed error: {errors:?}"
    );
    assert!(completed.error.is_none());
    let completion = completed
        .completion
        .expect("blocked tool outcome must carry semantic completion status");
    assert_eq!(completion.status, TaskCompletionStatus::Blocked);
    assert!(!completion.reasons.is_empty());

    let timing = completed
        .timing
        .expect("blocked completion must carry the closed timing profile");
    assert!(timing.profile_valid);
    assert!(timing.classification_complete);
    assert_eq!(timing.exclusive.unclassified_ns, 0);
    assert_eq!(timing.terminalization.unclassified_ns, 0);
    assert_eq!(timing.counters.model_request_count, 1);
    assert_eq!(timing.counters.logical_generation_count, 1);
    assert_eq!(timing.counters.attempts_by_kind.primary, 1);
    assert_eq!(timing.counters.attempts_by_kind.retry, 0);
    assert_eq!(timing.counters.attempts_by_kind.fallback, 0);
    assert_eq!(timing.counters.model_retry_count, 0);
    assert_eq!(timing.counters.model_fallback_count, 0);
    assert_eq!(timing.counters.tool_call_count, 2);
    assert_eq!(timing.tool_calls.len(), 2);

    let direct = timing
        .tool_calls
        .iter()
        .find(|call| call.source == TurnTimingToolCallSource::Direct)
        .expect("missing owning direct CodeMode call");
    let nested = timing
        .tool_calls
        .iter()
        .find(|call| call.source == TurnTimingToolCallSource::CodeMode)
        .expect("missing nested blocked apply_patch call");
    assert_eq!(direct.call_id, "call-1");
    assert_eq!(direct.tool_name, "exec");
    assert_eq!(nested.call_id, approval.call_id);
    assert_eq!(nested.tool_name, "apply_patch");
    assert_eq!(
        nested.parent_call_id.as_deref(),
        Some(direct.call_id.as_str())
    );
    assert_eq!(direct.sampling_generation_id, nested.sampling_generation_id);
    assert!(direct.outcome.is_some());
    assert!(nested.outcome.is_some());
    assert!(
        direct.output_projection_ms.is_some(),
        "the synthesized direct abort result must record its response projection"
    );
    assert!(
        nested.output_projection_ms.is_some(),
        "the synthesized nested abort result must record its CodeMode projection"
    );
    assert!(direct.output_model_visible_at_ms.is_some());
    assert!(direct.model_resumed_at_ms.is_none());
    assert!(nested.model_resumed_at_ms.is_none());

    let closure = &timing.tool_closure;
    assert_eq!(closure.accepted_count, 2);
    assert_eq!(closure.timing_paired_count, 2);
    assert_eq!(closure.terminal_count, 2);
    assert_eq!(closure.persisted_count, 2);
    assert_eq!(closure.duplicate_call_id_count, 0);
    assert_eq!(closure.duplicate_acceptance_count, 0);
    assert_eq!(closure.duplicate_timing_count, 0);
    assert_eq!(closure.duplicate_persistence_count, 0);
    assert_eq!(closure.orphan_timing_count, 0);
    assert_eq!(closure.orphan_persistence_count, 0);
    assert_eq!(closure.overflow_count, 0);
    assert!(closure.unresolved_calls.is_empty());
    assert!(closure.orphan_calls.is_empty());
    assert!(closure.complete);
    assert!(harness.follow_up.requests().is_empty());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_interrupt_closes_direct_and_nested_calls_before_turn_aborted() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = CodeModeElicitationHarness::start(
        r#"// @exec: {"yield_time_ms": 1000}
await tools.request_permissions({
  reason: "test permission request",
  permissions: { network: { enabled: true } },
});"#,
        PermissionProfile::read_only(),
        |config| {
            let _ = config.features.enable(Feature::RequestPermissionsTool);
        },
    )
    .await?;

    let request = tokio::time::timeout(TURN_COMPLETE_TIMEOUT, async {
        loop {
            let event = harness
                .test
                .codex
                .next_event()
                .await
                .expect("event stream should remain open");
            match event.msg {
                EventMsg::RequestPermissions(request) if request.turn_id == harness.turn_id => {
                    break request;
                }
                EventMsg::Error(error) => {
                    panic!("turn failed before interrupt: {}", error.message)
                }
                EventMsg::TurnComplete(event) if event.turn_id == harness.turn_id => {
                    panic!("turn completed before interrupt")
                }
                EventMsg::TurnAborted(event)
                    if event.turn_id.as_deref() == Some(harness.turn_id.as_str()) =>
                {
                    panic!("turn aborted before explicit interrupt: {:?}", event.reason)
                }
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for request_permissions barrier");
    assert_eq!(request.turn_id, harness.turn_id);

    harness.test.codex.submit(Op::Interrupt).await?;

    let aborted = tokio::time::timeout(TURN_COMPLETE_TIMEOUT, async {
        loop {
            let event = harness
                .test
                .codex
                .next_event()
                .await
                .expect("event stream should remain open");
            match event.msg {
                EventMsg::TurnAborted(event)
                    if event.turn_id.as_deref() == Some(harness.turn_id.as_str()) =>
                {
                    break event;
                }
                EventMsg::Error(error) => {
                    panic!(
                        "turn emitted an error while interrupting: {}",
                        error.message
                    )
                }
                EventMsg::TurnComplete(event) if event.turn_id == harness.turn_id => {
                    panic!("interrupted turn emitted TurnComplete")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for TurnAborted after tool closure");

    assert_eq!(aborted.reason, TurnAbortReason::Interrupted);
    let timing = aborted
        .timing
        .expect("TurnAborted must carry the closed timing profile");
    assert!(timing.profile_valid);
    assert!(timing.classification_complete);
    assert_eq!(timing.exclusive.unclassified_ns, 0);
    assert_eq!(timing.terminalization.unclassified_ns, 0);
    assert_eq!(timing.counters.invalid_transition_count, 0);
    assert_eq!(timing.counters.clock_regression_count, 0);
    assert_eq!(timing.counters.saturation_count, 0);
    assert_eq!(timing.counters.model_request_count, 1);
    assert_eq!(timing.counters.logical_generation_count, 1);
    assert_eq!(timing.counters.attempts_by_kind.primary, 1);
    assert_eq!(timing.counters.attempts_by_kind.retry, 0);
    assert_eq!(timing.counters.attempts_by_kind.fallback, 0);
    assert_eq!(timing.counters.model_retry_count, 0);
    assert_eq!(timing.counters.model_fallback_count, 0);
    assert_eq!(timing.model_requests.len(), 1);
    assert_eq!(timing.counters.tool_call_count, 2);
    assert_eq!(timing.counters.permission_wait_count, 1);
    assert_eq!(timing.tool_call_timing_overflow, 0);
    assert_eq!(timing.tool_calls.len(), 2);

    let direct = timing
        .tool_calls
        .iter()
        .find(|call| call.source == TurnTimingToolCallSource::Direct)
        .expect("missing direct CodeMode call");
    let nested = timing
        .tool_calls
        .iter()
        .find(|call| call.source == TurnTimingToolCallSource::CodeMode)
        .expect("missing nested request_permissions call");
    assert_eq!(direct.call_id, "call-1");
    assert_eq!(direct.tool_name, "exec");
    assert!(direct.parent_call_id.is_none());
    assert_eq!(nested.call_id, request.call_id);
    assert_eq!(nested.tool_name, "request_permissions");
    assert_eq!(
        nested.parent_call_id.as_deref(),
        Some(direct.call_id.as_str())
    );
    assert!(
        nested
            .parent_cell_id
            .as_ref()
            .is_some_and(|id| !id.is_empty())
    );
    assert!(
        nested
            .runtime_tool_call_id
            .as_ref()
            .is_some_and(|id| !id.is_empty())
    );
    assert!(!direct.execution_id.0.is_empty());
    assert!(!nested.execution_id.0.is_empty());
    assert_ne!(direct.execution_id, nested.execution_id);
    assert!(!direct.sampling_generation_id.0.is_empty());
    assert_eq!(direct.sampling_generation_id, nested.sampling_generation_id);
    assert!(direct.outcome.is_some());
    assert!(nested.outcome.is_some());
    assert!(direct.output_model_visible_at_ms.is_some());
    assert!(nested.output_model_visible_at_ms.is_none());
    assert!(direct.model_resumed_at_ms.is_none());
    assert!(nested.model_resumed_at_ms.is_none());

    let closure = &timing.tool_closure;
    assert_eq!(closure.accepted_count, 2);
    assert_eq!(closure.timing_paired_count, 2);
    assert_eq!(closure.terminal_count, 2);
    assert_eq!(closure.persisted_count, 2);
    assert_eq!(closure.duplicate_call_id_count, 0);
    assert_eq!(closure.duplicate_acceptance_count, 0);
    assert_eq!(closure.duplicate_timing_count, 0);
    assert_eq!(closure.duplicate_persistence_count, 0);
    assert_eq!(closure.orphan_timing_count, 0);
    assert_eq!(closure.orphan_persistence_count, 0);
    assert_eq!(closure.overflow_count, 0);
    assert!(closure.unresolved_calls.is_empty());
    assert!(closure.orphan_calls.is_empty());
    assert!(closure.complete);
    assert!(harness.follow_up.requests().is_empty());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_terminal_failure_stops_before_another_model_request() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = CodeModeElicitationHarness::start(
        r#"throw new Error("terminal failure marker");"#,
        PermissionProfile::Disabled,
        |config| {
            config
                .features
                .enable(Feature::TaskCompletionReviewer)
                .expect("enable completion reviewer to prove terminal failure bypasses it");
        },
    )
    .await?;

    let (completed, errors) = tokio::time::timeout(TURN_COMPLETE_TIMEOUT, async {
        let mut errors = Vec::new();
        loop {
            let event = harness
                .test
                .codex
                .next_event()
                .await
                .expect("event stream should remain open");
            match event.msg {
                EventMsg::Error(error) => errors.push(error),
                EventMsg::TurnComplete(event) if event.turn_id == harness.turn_id => {
                    break (event, errors);
                }
                EventMsg::TurnAborted(event)
                    if event.turn_id.as_deref() == Some(harness.turn_id.as_str()) =>
                {
                    panic!("terminal tool failure emitted TurnAborted")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for terminal failure completion");

    assert_eq!(
        errors.len(),
        1,
        "terminal failure must emit one typed error"
    );
    let error_message = errors[0].message.as_str();
    assert!(
        !error_message.is_empty(),
        "terminal failure must surface a nonempty error"
    );
    assert_eq!(
        completed.error.as_ref().map(|error| error.message.as_str()),
        Some(error_message),
        "the terminal receipt must preserve the emitted typed error"
    );
    let completion = completed
        .completion
        .expect("terminal failure must carry semantic completion status");
    assert_eq!(completion.status, TaskCompletionStatus::Partial);
    assert!(
        completion
            .reasons
            .iter()
            .any(|reason| reason == error_message)
    );

    let timing = completed
        .timing
        .expect("terminal failure must carry the closed timing profile");
    assert!(timing.profile_valid);
    assert!(timing.classification_complete);
    assert_eq!(timing.exclusive.unclassified_ns, 0);
    assert_eq!(timing.terminalization.unclassified_ns, 0);
    assert_eq!(timing.counters.model_request_count, 1);
    assert_eq!(timing.counters.logical_generation_count, 1);
    assert_eq!(timing.counters.attempts_by_kind.primary, 1);
    assert_eq!(timing.counters.attempts_by_kind.retry, 0);
    assert_eq!(timing.counters.attempts_by_kind.fallback, 0);
    assert_eq!(timing.counters.model_retry_count, 0);
    assert_eq!(timing.counters.model_fallback_count, 0);
    assert_eq!(timing.counters.tool_call_count, 1);
    assert_eq!(timing.tool_calls.len(), 1);
    let call = &timing.tool_calls[0];
    assert_eq!(call.call_id, "call-1");
    assert_eq!(call.tool_name, "exec");
    assert_eq!(call.source, TurnTimingToolCallSource::Direct);
    assert_eq!(call.outcome.as_deref(), Some("failure"));
    assert!(call.output_model_visible_at_ms.is_some());
    assert!(call.model_resumed_at_ms.is_none());

    let closure = &timing.tool_closure;
    assert_eq!(closure.accepted_count, 1);
    assert_eq!(closure.timing_paired_count, 1);
    assert_eq!(closure.terminal_count, 1);
    assert_eq!(closure.persisted_count, 1);
    assert_eq!(closure.duplicate_call_id_count, 0);
    assert_eq!(closure.duplicate_acceptance_count, 0);
    assert_eq!(closure.duplicate_timing_count, 0);
    assert_eq!(closure.duplicate_persistence_count, 0);
    assert_eq!(closure.orphan_timing_count, 0);
    assert_eq!(closure.orphan_persistence_count, 0);
    assert_eq!(closure.overflow_count, 0);
    assert!(closure.unresolved_calls.is_empty());
    assert!(closure.orphan_calls.is_empty());
    assert!(closure.complete);
    assert!(harness.follow_up.requests().is_empty());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_nested_required_nonzero_stops_before_another_model_request() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = CodeModeElicitationHarness::start(
        r#"await tools.exec_command({ cmd: "exit 7" });"#,
        PermissionProfile::Disabled,
        |config| {
            config
                .features
                .enable(Feature::TaskCompletionReviewer)
                .expect("enable completion reviewer to prove nested failure bypasses it");
        },
    )
    .await?;

    let (completed, errors) = tokio::time::timeout(TURN_COMPLETE_TIMEOUT, async {
        let mut errors = Vec::new();
        loop {
            let event = harness
                .test
                .codex
                .next_event()
                .await
                .expect("event stream should remain open");
            match event.msg {
                EventMsg::Error(error) => errors.push(error),
                EventMsg::TurnComplete(event) if event.turn_id == harness.turn_id => {
                    break (event, errors);
                }
                EventMsg::TurnAborted(event)
                    if event.turn_id.as_deref() == Some(harness.turn_id.as_str()) =>
                {
                    panic!("nested required failure emitted TurnAborted")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for nested required failure completion");

    assert_eq!(
        errors.len(),
        1,
        "nested required failure must emit one typed error"
    );
    let error_message = errors[0].message.as_str();
    assert!(!error_message.is_empty());
    assert_eq!(
        completed.error.as_ref().map(|error| error.message.as_str()),
        Some(error_message)
    );
    let completion = completed
        .completion
        .expect("nested required failure must carry semantic completion status");
    assert_eq!(completion.status, TaskCompletionStatus::Partial);
    assert!(
        completion
            .reasons
            .iter()
            .any(|reason| reason == error_message)
    );

    let timing = completed
        .timing
        .expect("nested required failure must carry the closed timing profile");
    assert!(timing.profile_valid);
    assert!(timing.classification_complete);
    assert_eq!(timing.exclusive.unclassified_ns, 0);
    assert_eq!(timing.terminalization.unclassified_ns, 0);
    assert_eq!(timing.counters.model_request_count, 1);
    assert_eq!(timing.counters.logical_generation_count, 1);
    assert_eq!(timing.counters.attempts_by_kind.primary, 1);
    assert_eq!(timing.counters.attempts_by_kind.retry, 0);
    assert_eq!(timing.counters.attempts_by_kind.fallback, 0);
    assert_eq!(timing.counters.model_retry_count, 0);
    assert_eq!(timing.counters.model_fallback_count, 0);
    assert_eq!(timing.counters.tool_call_count, 2);
    assert_eq!(timing.tool_calls.len(), 2);

    let direct = timing
        .tool_calls
        .iter()
        .find(|call| call.source == TurnTimingToolCallSource::Direct)
        .expect("missing owning direct CodeMode call");
    let nested = timing
        .tool_calls
        .iter()
        .find(|call| call.source == TurnTimingToolCallSource::CodeMode)
        .expect("missing nested required exec_command call");
    assert_eq!(direct.call_id, "call-1");
    assert_eq!(direct.tool_name, "exec");
    assert_eq!(direct.outcome.as_deref(), Some("failure"));
    assert_eq!(nested.tool_name, "exec_command");
    assert_eq!(nested.outcome.as_deref(), Some("failure"));
    assert_eq!(
        nested.parent_call_id.as_deref(),
        Some(direct.call_id.as_str())
    );
    assert_eq!(direct.sampling_generation_id, nested.sampling_generation_id);
    assert!(direct.output_model_visible_at_ms.is_some());
    assert!(direct.model_resumed_at_ms.is_none());
    assert!(nested.model_resumed_at_ms.is_none());

    let closure = &timing.tool_closure;
    assert_eq!(closure.accepted_count, 2);
    assert_eq!(closure.timing_paired_count, 2);
    assert_eq!(closure.terminal_count, 2);
    assert_eq!(closure.persisted_count, 2);
    assert_eq!(closure.duplicate_call_id_count, 0);
    assert_eq!(closure.duplicate_acceptance_count, 0);
    assert_eq!(closure.duplicate_timing_count, 0);
    assert_eq!(closure.duplicate_persistence_count, 0);
    assert_eq!(closure.orphan_timing_count, 0);
    assert_eq!(closure.orphan_persistence_count, 0);
    assert_eq!(closure.overflow_count, 0);
    assert!(closure.unresolved_calls.is_empty());
    assert!(closure.orphan_calls.is_empty());
    assert!(closure.complete);
    assert!(harness.follow_up.requests().is_empty());

    Ok(())
}
