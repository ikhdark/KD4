use crate::FunctionCallError;
use crate::ToolName;
use crate::ToolOutput;
use crate::ToolPayload;
use crate::ToolSearchInfo;
use crate::ToolSpec;
use std::future::Future;
use std::pin::Pin;

/// The boxed future returned by [`ToolExecutor::handle`].
pub type ToolExecutorFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Box<dyn ToolOutput>, FunctionCallError>> + Send + 'a>>;

pub trait TaskAcceptance: Send + Sync {
    /// Re-evaluate after sibling effects settle, before acquiring the extension
    /// state permit. No model-provided receipt can implement this check.
    fn check(&self) -> Pin<Box<dyn Future<Output = Result<(), FunctionCallError>> + Send + '_>>;
    /// Nonblocking fence recheck immediately before persistence, after the
    /// extension acquires its own permit. No cross-owner async call is allowed
    /// while holding that permit. Missing implementations fail closed.
    fn is_current(&self) -> bool {
        false
    }
    /// A blocked intent may retain explicitly blocked requirements, but must
    /// not hide independent obtainable work or a live required execution.
    fn check_blocked(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(), FunctionCallError>> + Send + '_>> {
        self.check()
    }
}

/// Conversation history an extension needs for one tool invocation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ConversationHistoryRequirement {
    /// Preserve compatibility by exposing the complete retained history.
    #[default]
    Full,
    /// Skip the history snapshot entirely for payloads that cannot read it.
    None,
}

/// Controls where a tool is exposed to the model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolExposure {
    /// Include this tool in the initial model-visible tool list.
    ///
    /// When code mode is enabled, this tool is also available as a nested
    /// code-mode tool.
    Direct,

    /// Register this tool for later discovery, but omit it from the initial
    /// model-visible tool list. Deferred tools must provide search metadata via
    /// [`ToolExecutor::search_info`]. The default implementation derives
    /// metadata from function and namespace specs.
    Deferred,

    /// Include this tool in the initial model-visible tool list only.
    ///
    /// In code-mode-only sessions, this keeps the tool callable as a normal
    /// model tool while excluding it from the nested code-mode tool surface.
    DirectModelOnly,

    /// Keep this tool registered for dispatch without exposing it to the model.
    Hidden,
}

impl ToolExposure {
    pub fn is_direct(self) -> bool {
        matches!(self, Self::Direct | Self::DirectModelOnly)
    }
}

/// Shared runtime contract for model-visible tools.
///
/// Implementations keep the model-visible spec tied to the executable runtime.
/// Host crates can layer routing, hooks, telemetry, or other orchestration on
/// top without reopening the spec/runtime split.
pub trait ToolExecutor<Invocation>: Send + Sync {
    /// The concrete tool name handled by this runtime instance.
    fn tool_name(&self) -> ToolName;

    fn spec(&self) -> ToolSpec;

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        let spec = self.spec();
        self.search_info_for_registered_spec(&spec)
    }

    /// Builds search metadata from the registry's authoritative spec snapshot.
    ///
    /// Hosts should use this after registration instead of asking the runtime
    /// to regenerate its spec. The separate method preserves the existing
    /// extension-facing `search_info` contract.
    fn search_info_for_registered_spec(
        &self,
        registered_spec: &ToolSpec,
    ) -> Option<ToolSearchInfo> {
        ToolSearchInfo::from_tool_spec(registered_spec, /*source_info*/ None)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        false
    }

    /// A terminal-success operation consumes the host's current scoped task
    /// decision before invoking the extension. Default preserves old runtimes.
    fn requires_task_acceptance(&self, _payload: &ToolPayload) -> bool {
        false
    }

    fn handle_with_task_acceptance(
        &self,
        invocation: Invocation,
        _acceptance: std::sync::Arc<dyn TaskAcceptance>,
    ) -> ToolExecutorFuture<'_> {
        self.handle(invocation)
    }

    /// Returns the history needed for this payload before the invocation is built.
    fn conversation_history_requirement(
        &self,
        _payload: &ToolPayload,
    ) -> ConversationHistoryRequirement {
        ConversationHistoryRequirement::Full
    }

    /// Whether this executor observes the invocation cancellation token and
    /// completes any runtime-owned lifecycle cleanup before returning.
    fn handles_runtime_cancellation(&self) -> bool {
        false
    }

    fn handle(&self, invocation: Invocation) -> ToolExecutorFuture<'_>;
}
