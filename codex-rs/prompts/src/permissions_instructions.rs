use codex_context_fragments::ContextualUserFragment;
use codex_execpolicy::Policy;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::format_allow_prefixes;
use codex_protocol::openai_models::ApprovalMessages;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::GranularApprovalConfig;
use std::path::Path;

const APPROVAL_POLICY_NEVER: &str =
    include_str!("../templates/permissions/approval_policy/never.md");
const APPROVAL_POLICY_UNLESS_TRUSTED: &str =
    include_str!("../templates/permissions/approval_policy/unless_trusted.md");
const APPROVAL_POLICY_ON_REQUEST: &str =
    include_str!("../templates/permissions/approval_policy/on_request.md");
const COMMAND_APPROVAL_REQUESTS: &str =
    include_str!("../templates/permissions/approval_policy/command_approval_requests.md");
/// KD4 executes commands and file tools without a sandbox; legacy permission
/// profiles never restrict access, so the access statement is fixed.
const FULL_ACCESS: &str = include_str!("../templates/permissions/full_access.md");
const MAX_APPROVAL_MESSAGE_BYTES: usize = 12_000;
const OVERSIZED_APPROVAL_MESSAGE: &str = "The mandatory custom approval policy exceeds the supported size and could not be loaded in full. No tool execution, permission request, or other action is authorized. Stop and report this configuration error; do not infer permission from a partial policy.";

#[derive(Debug, Clone, PartialEq)]
/// Developer instructions that describe execution access and the approval policy.
pub struct PermissionsInstructions {
    text: String,
}

#[derive(Debug, Clone, Copy)]
pub struct ApprovalPromptContext<'a> {
    messages: Option<&'a ApprovalMessages>,
}

impl<'a> ApprovalPromptContext<'a> {
    pub fn new(messages: Option<&'a ApprovalMessages>) -> Self {
        Self { messages }
    }
}

impl PermissionsInstructions {
    /// Builds permissions instructions from the approval policy.
    ///
    /// The permission profile, cwd and the legacy permission-request feature
    /// flags are accepted for caller compatibility only: KD4 has no sandbox, so
    /// they never change the full-access statement or add permission-request
    /// guidance.
    pub fn from_permission_profile(
        _permission_profile: &PermissionProfile,
        approval_policy: AskForApproval,
        approval_context: ApprovalPromptContext<'_>,
        exec_policy: &Policy,
        _cwd: &Path,
        _exec_permission_approvals_enabled: bool,
        _request_permissions_tool_enabled: bool,
    ) -> Self {
        let mut text = String::new();
        append_section(&mut text, FULL_ACCESS.trim_end());
        append_section(
            &mut text,
            &approval_text(approval_policy, approval_context.messages, exec_policy),
        );
        if !text.ends_with('\n') {
            text.push('\n');
        }
        Self { text }
    }

    pub fn body(&self) -> String {
        self.text.clone()
    }
}

impl ContextualUserFragment for PermissionsInstructions {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<permissions instructions>", "</permissions instructions>")
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.text)
    }
}

fn append_section(text: &mut String, section: &str) {
    // The rendered fragment has no separator after its opening marker, so the
    // body intentionally starts with a newline.
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(section);
}

fn approval_text(
    approval_policy: AskForApproval,
    approval_messages: Option<&ApprovalMessages>,
    exec_policy: &Policy,
) -> String {
    if approval_policy == AskForApproval::OnRequest
        && let Some(approval_messages) = approval_messages
        && let Some(selected) = approval_messages.on_request.as_ref()
    {
        return if selected.len() > MAX_APPROVAL_MESSAGE_BYTES {
            OVERSIZED_APPROVAL_MESSAGE.to_string()
        } else {
            selected.clone()
        };
    }

    match approval_policy {
        AskForApproval::Never => APPROVAL_POLICY_NEVER.to_string(),
        AskForApproval::UnlessTrusted => APPROVAL_POLICY_UNLESS_TRUSTED.to_string(),
        AskForApproval::OnRequest => {
            let mut sections = vec![
                APPROVAL_POLICY_ON_REQUEST.trim_end().to_string(),
                COMMAND_APPROVAL_REQUESTS.trim_end().to_string(),
            ];
            if let Some(prefixes) = approved_command_prefixes_text(exec_policy) {
                sections.push(approved_command_prefixes_section(&prefixes));
            }
            sections.join("\n\n")
        }
        AskForApproval::Granular(granular_config) => {
            granular_instructions(granular_config, exec_policy)
        }
    }
}

fn approved_command_prefixes_text(exec_policy: &Policy) -> Option<String> {
    format_allow_prefixes(exec_policy.get_allowed_prefixes())
        .filter(|prefixes| !prefixes.is_empty())
}

fn approved_command_prefixes_section(prefixes: &str) -> String {
    format!(
        "## Approved command prefixes\nThe following prefix rules have already been approved: {prefixes}"
    )
}

fn granular_prompt_intro_text() -> &'static str {
    "# Approval Requests\n\nApproval policy is `granular`. Categories set to `false` are automatically rejected instead of prompting the user."
}

fn granular_instructions(granular_config: GranularApprovalConfig, exec_policy: &Policy) -> String {
    // `sandbox_approval` remains the compatibility config key for ordinary
    // command approval prompts; it no longer refers to a sandbox.
    let command_approvals_allowed = granular_config.allows_sandbox_approval();
    let categories = [
        (command_approvals_allowed, "command approvals"),
        (granular_config.allows_rules_approval(), "`rules`"),
        (granular_config.allows_skill_approval(), "`skill_approval`"),
        (
            granular_config.allows_mcp_elicitations(),
            "`mcp_elicitations`",
        ),
    ];
    let prompted_categories = categories
        .iter()
        .filter(|&&(is_allowed, _)| is_allowed)
        .map(|&(_, category)| format!("- {category}"))
        .collect::<Vec<_>>();
    let rejected_categories = categories
        .iter()
        .filter(|&&(is_allowed, _)| !is_allowed)
        .map(|&(_, category)| format!("- {category}"))
        .collect::<Vec<_>>();

    let mut sections = vec![granular_prompt_intro_text().to_string()];

    if !prompted_categories.is_empty() {
        sections.push(format!(
            "These approval categories may still prompt the user when needed:\n{}",
            prompted_categories.join("\n")
        ));
    }
    if !rejected_categories.is_empty() {
        sections.push(format!(
            "These approval categories are automatically rejected instead of prompting the user:\n{}",
            rejected_categories.join("\n")
        ));
    }

    if command_approvals_allowed {
        sections.push(COMMAND_APPROVAL_REQUESTS.trim_end().to_string());
    }

    if let Some(prefixes) = approved_command_prefixes_text(exec_policy) {
        sections.push(approved_command_prefixes_section(&prefixes));
    }

    sections.join("\n\n")
}
