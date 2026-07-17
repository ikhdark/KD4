use crate::function_tool::FunctionCallError;
use crate::safety::SafetyCheck;
use crate::safety::assess_patch_safety;
use crate::session::turn_context::TurnContext;
use crate::tools::sandboxing::ExecApprovalRequirement;
use codex_apply_patch::AppliedPatchDelta;
use codex_apply_patch::AppliedPatchFileChange;
use codex_apply_patch::ApplyPatchAction;
use codex_apply_patch::ApplyPatchFileChange;
use codex_protocol::items::OrderedFileChange;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::FileSystemSandboxPolicy;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;
use std::path::PathBuf;

pub(crate) enum InternalApplyPatchInvocation {
    /// The `apply_patch` call was handled programmatically, without any sort
    /// of sandbox, because the user explicitly approved it. This is the
    /// result to use with the `shell` function call that contained `apply_patch`.
    Output {
        action: ApplyPatchAction,
        result: Result<String, FunctionCallError>,
    },

    /// The `apply_patch` call was approved, either automatically because it
    /// appears that it should be allowed based on the user's sandbox policy
    /// *or* because the user explicitly approved it. The runtime realizes the
    /// patch through the selected environment filesystem.
    DelegateToRuntime(ApplyPatchRuntimeInvocation),
}

#[derive(Debug)]
pub(crate) struct ApplyPatchRuntimeInvocation {
    pub(crate) action: ApplyPatchAction,
    pub(crate) auto_approved: bool,
    pub(crate) exec_approval_requirement: ExecApprovalRequirement,
}

pub(crate) async fn apply_patch(
    turn_context: &TurnContext,
    file_system_sandbox_policy: &FileSystemSandboxPolicy,
    action: ApplyPatchAction,
) -> InternalApplyPatchInvocation {
    match assess_patch_safety(
        &action,
        turn_context.approval_policy.value(),
        &turn_context.permission_profile(),
        file_system_sandbox_policy,
        &action.cwd,
        turn_context.windows_sandbox_level,
    ) {
        SafetyCheck::AutoApprove {
            user_explicitly_approved,
            ..
        } => InternalApplyPatchInvocation::DelegateToRuntime(ApplyPatchRuntimeInvocation {
            action,
            auto_approved: !user_explicitly_approved,
            exec_approval_requirement: ExecApprovalRequirement::Skip {
                bypass_sandbox: false,
                proposed_execpolicy_amendment: None,
            },
        }),
        SafetyCheck::AskUser => {
            // Delegate the approval prompt (including cached approvals) to the
            // tool runtime, consistent with how shell/unified_exec approvals
            // are orchestrator-driven.
            InternalApplyPatchInvocation::DelegateToRuntime(ApplyPatchRuntimeInvocation {
                action,
                auto_approved: false,
                exec_approval_requirement: ExecApprovalRequirement::NeedsApproval {
                    reason: None,
                    proposed_execpolicy_amendment: None,
                },
            })
        }
        SafetyCheck::Reject { reason } => InternalApplyPatchInvocation::Output {
            action,
            result: Err(FunctionCallError::RespondToModel(format!(
                "patch rejected: {reason}"
            ))),
        },
    }
}

/// Projects the ordered patch plan into the legacy protocol event map.
///
/// The protocol shape collapses repeated operations for the same path, so this
/// compatibility projection must not feed safety, approval, execution, or exact
/// result derivation.
pub(crate) fn convert_apply_patch_to_protocol_compatibility(
    action: &ApplyPatchAction,
) -> HashMap<PathBuf, FileChange> {
    convert_apply_patch_to_protocol_ordered(action)
        .into_iter()
        .map(|operation| (operation.path, operation.change))
        .collect()
}

pub(crate) fn convert_apply_patch_to_protocol_ordered(
    action: &ApplyPatchAction,
) -> Vec<OrderedFileChange> {
    action
        .operations()
        .iter()
        .map(|operation| {
            let protocol_change = match operation.change() {
                ApplyPatchFileChange::Add { content, .. } => FileChange::Add {
                    content: content.clone(),
                },
                ApplyPatchFileChange::Delete { content } => FileChange::Delete {
                    content: content.clone(),
                },
                ApplyPatchFileChange::Update {
                    unified_diff,
                    move_path,
                    new_content: _new_content,
                } => FileChange::Update {
                    unified_diff: unified_diff.clone(),
                    move_path: move_path.as_ref().map(PathUri::to_path_buf),
                },
            };
            // TODO(anp): Carry PathUri through patch protocol events once app-server and rollout
            // compatibility no longer require path-flavored strings.
            OrderedFileChange {
                path: operation.path().to_path_buf(),
                change: protocol_change,
            }
        })
        .collect()
}

/// Projects the exact committed delta into protocol order without consulting
/// the planned operation map. This also represents a destination-only partial
/// move as the committed add that actually occurred.
pub(crate) fn convert_applied_patch_delta_to_protocol_ordered(
    delta: &AppliedPatchDelta,
) -> Vec<OrderedFileChange> {
    delta
        .changes()
        .iter()
        .map(|applied| {
            let change = match &applied.change {
                AppliedPatchFileChange::Add { content, .. } => FileChange::Add {
                    content: content.clone(),
                },
                AppliedPatchFileChange::Delete { content } => FileChange::Delete {
                    content: content.clone(),
                },
                AppliedPatchFileChange::Update {
                    move_path,
                    old_content,
                    new_content,
                    ..
                } => FileChange::Update {
                    unified_diff: similar::TextDiff::from_lines(old_content, new_content)
                        .unified_diff()
                        .context_radius(3)
                        .to_string(),
                    move_path: move_path.clone(),
                },
            };
            OrderedFileChange {
                path: applied.path.clone(),
                change,
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "apply_patch_tests.rs"]
mod tests;
