use codex_core::config::Constrained;
use codex_core::sandboxing::SandboxPermissions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::ReviewTarget;
use core_test_support::responses::ev_apply_patch_custom_tool_call;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_reasoning_item_added;
use core_test_support::responses::ev_reasoning_summary_text_delta;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event_with_timeout;
use pretty_assertions::assert_eq;
use std::time::Duration;

async fn assert_review_completes_without_approval(
    codex: &codex_core::CodexThread,
    explanation: &str,
) {
    let mut entered = false;
    let mut exited = false;
    loop {
        match wait_for_event_with_timeout(codex, |_| true, Duration::from_secs(30)).await {
            EventMsg::EnteredReviewMode(_) => entered = true,
            EventMsg::ExecApprovalRequest(_) | EventMsg::ApplyPatchApprovalRequest(_) => {
                panic!("the review child must honor its Never policy without asking the parent");
            }
            EventMsg::ExitedReviewMode(event) => {
                let output = event
                    .review_output
                    .expect("review returns its final result");
                assert_eq!(output.overall_explanation, explanation);
                exited = true;
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    assert!(entered && exited, "the real review lifecycle must complete");
}

/// Review children reject escalation even when the parent permits approval prompts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_delegate_review_rejects_exec_escalation_without_parent_approval() {
    let call_id = "call-exec-1";
    let args = serde_json::json!({
        "command": "echo forbidden > delegated.txt",
        "timeout_ms": 1000,
        "sandbox_permissions": SandboxPermissions::RequireEscalated,
    })
    .to_string();
    let review_json = serde_json::json!({
        "findings": [],
        "overall_correctness": "ok",
        "overall_explanation": "exec escalation rejected",
        "overall_confidence_score": 0.5
    })
    .to_string();
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(call_id, "shell_command", &args),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", &review_json),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex().with_model("gpt-5.4").with_config(|config| {
        config.permissions.approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
        config
            .permissions
            .set_permission_profile(PermissionProfile::read_only())
            .unwrap();
    });
    let test = builder.build(&server).await.expect("build test codex");
    test.codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "Please review".to_string(),
                },
                user_facing_hint: None,
            },
        })
        .await
        .expect("submit review");
    assert_review_completes_without_approval(&test.codex, "exec escalation rejected").await;
    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    let output = requests[1]
        .function_call_output_text(call_id)
        .expect("rejected exec reaches model");
    assert_eq!(
        output,
        "independent reviewers cannot request shell sandbox overrides or additional permissions"
    );
    assert!(!test.cwd.path().join("delegated.txt").exists());
}

/// Review children reject writes in a read-only sandbox without asking the parent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_delegate_review_rejects_patch_without_parent_approval() {
    let call_id = "call-patch-1";
    let patch = "*** Begin Patch\n*** Add File: delegated.txt\n+hello\n*** End Patch\n";
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![sse(vec![
            ev_response_created("resp-1"),
            ev_apply_patch_custom_tool_call(call_id, patch),
            ev_completed("resp-1"),
        ])],
    )
    .await;
    let mut builder = test_codex().with_model("gpt-5.4").with_config(|config| {
        config.permissions.approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
        config
            .permissions
            .set_permission_profile(PermissionProfile::read_only())
            .unwrap();
    });
    let test = builder.build(&server).await.expect("build test codex");
    test.codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "Please review".to_string(),
                },
                user_facing_hint: None,
            },
        })
        .await
        .expect("submit review");
    assert_review_completes_without_approval(&test.codex, "required tool `apply_patch` blocked")
        .await;
    let requests = responses.requests();
    assert_eq!(
        requests.len(),
        1,
        "required mutation denial ends the review without another model call"
    );
    assert!(!test.cwd.path().join("delegated.txt").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_delegate_ignores_legacy_deltas() {
    skip_if_no_network!();

    // Single response with reasoning summary deltas.
    let sse_stream = sse(vec![
        ev_response_created("resp-1"),
        ev_reasoning_item_added("reason-1", &["initial"]),
        ev_reasoning_summary_text_delta("think-1"),
        ev_completed("resp-1"),
    ]);

    let server = start_mock_server().await;
    mount_sse_sequence(&server, vec![sse_stream]).await;

    let mut builder = test_codex();
    let test = builder.build(&server).await.expect("build test codex");

    // Kick off review (delegated).
    test.codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "Please review".to_string(),
                },
                user_facing_hint: None,
            },
        })
        .await
        .expect("submit review");

    let mut reasoning_delta_count = 0;

    loop {
        let ev = wait_for_event_with_timeout(&test.codex, |_| true, Duration::from_secs(30)).await;
        match ev {
            EventMsg::ReasoningContentDelta(_) => reasoning_delta_count += 1,
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert_eq!(reasoning_delta_count, 1, "expected one new reasoning delta");
}
