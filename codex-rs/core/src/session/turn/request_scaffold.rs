//! Model-visible prompt construction and reusable request scaffolds.
//!
//! Owns scaffold cache identity, tool-surface invalidation, stable-context manifests,
//! and projection of prepared history into a prompt. The turn loop retains dispatch,
//! retries, terminal-request policy, and the session-owned cache lifetime.

use std::sync::Arc;
use std::sync::Weak;

use crate::agents_md::RepositoryStableContextBundle;
use crate::client_common::Prompt;
use crate::client_common::PromptDigests;
use crate::client_common::ToolSchemaArtifact;
use crate::context::ApprovalPromptContext;
use crate::context::ContextualUserFragment;
use crate::context::PermissionsInstructions;
use crate::context::PromptContextCategory;
use crate::context_manager::PreparedPromptInput;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::stable_context::StableContextKind;
use crate::tools::ToolRouter;
use codex_features::Feature;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::SessionSource;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RepositoryScaffoldIdentity {
    identity: [u8; 32],
    semantic_replacement: bool,
}

#[derive(Debug)]
struct RequestScaffoldOwner {
    config: Arc<crate::config::Config>,
    exec_policy: Option<Arc<codex_execpolicy::Policy>>,
    model_slug: String,
    stable_context_manifest: crate::stable_context::StableContextManifest,
    repository: Option<RepositoryScaffoldIdentity>,
}

impl RequestScaffoldOwner {
    fn matches(
        &self,
        prepared: &PreparedPromptInput,
        step_context: &StepContext,
        exec_policy: Option<&Arc<codex_execpolicy::Policy>>,
    ) -> bool {
        (Arc::ptr_eq(&self.config, &step_context.turn.config)
            || self.config.as_ref() == step_context.turn.config.as_ref())
            && match (&self.exec_policy, exec_policy) {
                (None, None) => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            }
            && self.model_slug == step_context.turn.model_info.slug
            && stable_context_owner_matches(
                &self.stable_context_manifest,
                prepared.stable_context_manifest(),
            )
            && self.repository == repository_scaffold_identity(step_context)
    }
}

#[derive(Debug)]
struct RequestScaffold {
    base_instructions: BaseInstructions,
    tools: Arc<ToolSchemaArtifact>,
    stable_context_manifest: crate::stable_context::StableContextManifest,
    reused_stable_context_manifest: crate::stable_context::StableContextManifest,
    stable_input_bytes: u64,
    stable_input_tokens: i64,
    digests: PromptDigests,
    exact_provenance_fragments: Vec<(String, PromptContextCategory)>,
}

impl RequestScaffold {
    fn build(
        prepared: &PreparedPromptInput,
        step_context: &StepContext,
        tools: Arc<ToolSchemaArtifact>,
        base_instructions: BaseInstructions,
        exec_policy: Option<&codex_execpolicy::Policy>,
    ) -> Self {
        let mut manifest = prepared.stable_context_manifest().with_repository_identity(
            step_context
                .agents_md_stable_context
                .as_ref()
                .map(RepositoryStableContextBundle::metadata),
        );
        let stable_input_bytes = manifest.projected_bytes();
        let stable_input_tokens = manifest.projected_tokens();
        manifest = manifest
            .with_base_model(&step_context.turn.model_info.slug, &base_instructions.text)
            .add_component_bytes(
                StableContextKind::ToolSchemas,
                "tool_schemas",
                tools.serialized(),
            );
        if tools.has_request_user_input() {
            manifest = manifest.add_measured_component(
                StableContextKind::RequestUserInput,
                "request_user_input_available",
                b"request_user_input",
                0,
                0,
            );
        }
        if tools.has_wait() {
            manifest = manifest.add_measured_component(
                StableContextKind::Wait,
                "wait_available",
                b"wait",
                0,
                0,
            );
        }
        let digests = PromptDigests {
            instructions: manifest.active_content_hash(StableContextKind::BaseModel),
            tools: Some(tools.digest()),
            history: None,
        };
        let reused_stable_context_manifest = manifest.with_local_reused(true);
        let mut exact_provenance_fragments = Vec::with_capacity(3);
        if step_context.turn.config.include_permissions_instructions {
            let Some(exec_policy) = exec_policy else {
                unreachable!("permissions scaffold requires the current exec policy");
            };
            let permissions = PermissionsInstructions::from_permission_profile(
                &step_context.turn.permission_profile,
                step_context.turn.approval_policy.value(),
                ApprovalPromptContext::new(
                    step_context
                        .turn
                        .model_info
                        .model_messages
                        .as_ref()
                        .and_then(|messages| messages.approvals.as_ref()),
                ),
                exec_policy,
                step_context.turn.cwd(),
                step_context
                    .turn
                    .config
                    .features
                    .enabled(Feature::ExecPermissionApprovals),
                step_context
                    .turn
                    .config
                    .features
                    .enabled(Feature::RequestPermissionsTool),
            )
            .render();
            exact_provenance_fragments
                .push((permissions, PromptContextCategory::EnvironmentPermissions));
        }
        if let Some(role_policy) = crate::session::multi_agents::usage_hint_text(
            &step_context.turn,
            &step_context.turn.session_source,
        ) {
            exact_provenance_fragments
                .push((role_policy.to_string(), PromptContextCategory::AgentRole));
        }
        if matches!(step_context.turn.session_source, SessionSource::VSCode)
            && let Some(desktop_context) = step_context.turn.developer_instructions.as_deref()
        {
            exact_provenance_fragments.push((
                crate::context::desktop_instructions::project(desktop_context).into_owned(),
                PromptContextCategory::AppDesktop,
            ));
        }
        Self {
            base_instructions,
            tools,
            stable_context_manifest: manifest,
            reused_stable_context_manifest,
            stable_input_bytes,
            stable_input_tokens,
            digests,
            exact_provenance_fragments,
        }
    }

    fn manifest(&self, locally_reused: bool) -> &crate::stable_context::StableContextManifest {
        if locally_reused {
            &self.reused_stable_context_manifest
        } else {
            &self.stable_context_manifest
        }
    }
}

#[derive(Debug)]
pub(super) struct ResolvedRequestScaffold {
    scaffold: Arc<RequestScaffold>,
    locally_reused: bool,
}

/// Cheap identity for the model-visible tool surface of one request.
///
/// Materializing the schema artifact walks the registry and takes the router schema lock, so
/// the scaffold cache compares this token first and only acquires the artifact on a miss.
/// `router` is the registry identity and `activation_revision` advances whenever deferred
/// tool activation changes the exposed surface, which together determine the artifact the
/// router would hand back. Keeping a weak reference reserves the allocation identity after
/// the router drops, so a later router cannot reuse its address and match a stale token.
#[derive(Clone, Debug)]
struct ToolSchemaSurfaceToken {
    router: Weak<ToolRouter>,
    activation_revision: u64,
    terminal_completion_only: bool,
}

impl ToolSchemaSurfaceToken {
    fn capture(
        router: &Arc<ToolRouter>,
        turn: &crate::session::turn_context::TurnContext,
        terminal_completion_only: bool,
    ) -> Self {
        Self {
            router: Arc::downgrade(router),
            activation_revision: turn.deferred_tool_activation_revision(),
            terminal_completion_only,
        }
    }

    fn matches(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.router, &other.router)
            && self.activation_revision == other.activation_revision
            && self.terminal_completion_only == other.terminal_completion_only
    }
}

#[derive(Debug)]
struct RequestScaffoldCacheEntry {
    owner: RequestScaffoldOwner,
    scaffold: Arc<RequestScaffold>,
    tool_schema_surface: ToolSchemaSurfaceToken,
}

#[derive(Debug, Default)]
pub(in crate::session) struct RequestScaffoldCache {
    entry: Option<RequestScaffoldCacheEntry>,
}

impl RequestScaffoldCache {
    pub(super) fn resolve(
        &mut self,
        prepared: &PreparedPromptInput,
        sess: &Session,
        router: &Arc<ToolRouter>,
        step_context: &StepContext,
        base_instructions: &BaseInstructions,
        terminal_completion_only: bool,
    ) -> ResolvedRequestScaffold {
        let exec_policy = step_context
            .turn
            .config
            .include_permissions_instructions
            .then(|| sess.services.exec_policy.current());
        let tool_schema_surface =
            ToolSchemaSurfaceToken::capture(router, &step_context.turn, terminal_completion_only);
        // Check the cheap surface token before materializing the schema artifact so a repeated
        // request with an unchanged tool surface never touches the router schema cache.
        if let Some(entry) = self.entry.as_ref()
            && entry.tool_schema_surface.matches(&tool_schema_surface)
            && entry
                .owner
                .matches(prepared, step_context, exec_policy.as_ref())
            && entry.scaffold.base_instructions == *base_instructions
        {
            return ResolvedRequestScaffold {
                scaffold: Arc::clone(&entry.scaffold),
                locally_reused: true,
            };
        }

        let tools = router.model_visible_schemas_for_turn(&step_context.turn);
        // A different router instance or activation revision can still project the same
        // surface. Fall back to the artifact comparison so those requests keep reusing the
        // scaffold instead of rebuilding it.
        if let Some(entry) = self.entry.as_mut()
            && entry
                .owner
                .matches(prepared, step_context, exec_policy.as_ref())
            && entry.scaffold.base_instructions == *base_instructions
            && tool_schema_surface_matches(&entry.scaffold.tools, &tools)
        {
            entry.tool_schema_surface = tool_schema_surface;
            return ResolvedRequestScaffold {
                scaffold: Arc::clone(&entry.scaffold),
                locally_reused: true,
            };
        }

        let owner = RequestScaffoldOwner {
            config: Arc::clone(&step_context.turn.config),
            exec_policy: exec_policy.as_ref().map(Arc::clone),
            model_slug: step_context.turn.model_info.slug.clone(),
            stable_context_manifest: prepared.stable_context_manifest().clone(),
            repository: repository_scaffold_identity(step_context),
        };
        let scaffold = Arc::new(RequestScaffold::build(
            prepared,
            step_context,
            tools,
            base_instructions.clone(),
            exec_policy.as_deref(),
        ));
        self.entry = Some(RequestScaffoldCacheEntry {
            owner,
            scaffold: Arc::clone(&scaffold),
            tool_schema_surface,
        });
        ResolvedRequestScaffold {
            scaffold,
            locally_reused: false,
        }
    }

}

fn arc_identity_or_equivalent<T: ?Sized>(
    left: &Arc<T>,
    right: &Arc<T>,
    equivalent: impl FnOnce(&T, &T) -> bool,
) -> bool {
    Arc::ptr_eq(left, right) || equivalent(left.as_ref(), right.as_ref())
}

fn tool_schema_surface_matches(
    left: &Arc<ToolSchemaArtifact>,
    right: &Arc<ToolSchemaArtifact>,
) -> bool {
    arc_identity_or_equivalent(left, right, |left, right| {
        left.digest() == right.digest() && left.serialized() == right.serialized()
    })
}

fn repository_scaffold_identity(step_context: &StepContext) -> Option<RepositoryScaffoldIdentity> {
    step_context
        .agents_md_stable_context
        .as_ref()
        .map(|bundle| {
            let (identity, _locally_reused, semantic_replacement) = bundle.metadata();
            RepositoryScaffoldIdentity {
                identity,
                semantic_replacement,
            }
        })
}

fn stable_context_owner_matches(
    left: &crate::stable_context::StableContextManifest,
    right: &crate::stable_context::StableContextManifest,
) -> bool {
    left.projection_enabled() == right.projection_enabled()
        && left.fail_open() == right.fail_open()
        && left.components().len() == right.components().len()
        && left
            .components()
            .iter()
            .zip(right.components())
            .all(|(left, right)| {
                left.kind == right.kind
                    && left.identity == right.identity
                    && left.active == right.active
                    && left.disposition == right.disposition
            })
}

pub(crate) fn build_projected_prompt(
    sess: &Session,
    prepared: &PreparedPromptInput,
    router: &ToolRouter,
    step_context: &StepContext,
    base_instructions: BaseInstructions,
) -> Prompt {
    let scaffold = ResolvedRequestScaffold {
        scaffold: Arc::new(RequestScaffold::build(
            prepared,
            step_context,
            router.model_visible_schemas_for_turn(&step_context.turn),
            base_instructions,
            step_context
                .turn
                .config
                .include_permissions_instructions
                .then(|| sess.services.exec_policy.current())
                .as_deref(),
        )),
        locally_reused: false,
    };
    build_projected_prompt_from_scaffold(prepared, step_context, &scaffold)
}

pub(super) fn build_projected_prompt_from_scaffold(
    prepared: &PreparedPromptInput,
    step_context: &StepContext,
    resolved_scaffold: &ResolvedRequestScaffold,
) -> Prompt {
    let scaffold = resolved_scaffold.scaffold.as_ref();
    let [
        input,
        fallback_input,
        tool_history_fallback_input,
        stable_context_tool_history_fallback_input,
    ] = prepared.shared_prompt_projections();
    let digests = PromptDigests {
        history: prepared.fingerprint(),
        ..scaffold.digests
    };
    let prompt_provenance = prepared.prompt_provenance().with_exact_fragments(
        &input,
        scaffold
            .exact_provenance_fragments
            .iter()
            .map(|(fragment, category)| (fragment.as_str(), *category)),
    );
    Prompt {
        input,
        replay_snapshot: Some(prepared.sampling_replay_snapshot()),
        formatted_inputs: Default::default(),
        stable_context_fallback_input: fallback_input,
        tool_history_fallback_input,
        stable_context_tool_history_fallback_input,
        tool_history_substitutions: prepared.tool_history_substitutions(),
        stable_context_fallback_tool_history_substitutions: prepared
            .fallback_tool_history_substitutions(),
        stable_context_manifest: scaffold.manifest(resolved_scaffold.locally_reused).clone(),
        deferred_dynamic_history: Some(
            crate::client_common::DeferredDynamicHistoryMeasurement::new(
                scaffold.stable_input_bytes,
                scaffold.stable_input_tokens,
            ),
        ),
        prompt_provenance,
        digests,
        tools: Arc::clone(&scaffold.tools),
        parallel_tool_calls: step_context.turn.model_info.supports_parallel_tool_calls,
        tool_calls_disabled: false,
        soft_limit_waiver: false,
        base_instructions: scaffold.base_instructions.clone(),
        output_schema: step_context
            .turn
            .final_output_json_schema
            .as_ref()
            .map(|schema| schema.original().clone()),
        output_schema_strict: true,
        stream_progress_timeout: None,
    }
}
