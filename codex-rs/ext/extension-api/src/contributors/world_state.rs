use std::sync::Arc;

use codex_protocol::ThreadId;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::protocol::TurnEnvironmentSelection;
use serde_json::Value;

use crate::ExtensionData;

/// Host state available while an extension contributes one sampling step's World State.
pub struct WorldStateContributionInput<'a> {
    pub thread_id: ThreadId,
    pub turn_id: &'a str,
    pub environments: &'a [TurnEnvironmentSelection],
    /// Selected roots whose stable environments are ready in this sampling step.
    pub ready_selected_capability_roots: &'a [SelectedCapabilityRoot],
    pub bound_selected_capability_roots: &'a [codex_exec_server::ResolvedSelectedCapabilityRoot],
    pub session_store: &'a ExtensionData,
    pub thread_store: &'a ExtensionData,
    pub turn_store: &'a ExtensionData,
}

/// What the harness knows about the previous value of one extension-owned section.
pub enum PreviousWorldStateSection<'a> {
    /// No reusable section baseline is available, including when required retained context is missing.
    /// Render any current state the model needs without relying on an earlier value.
    Absent,
    /// Prior section context exists, but its exact comparison value is unavailable.
    /// Render an authoritative replacement or clearing update instead of a baseline-dependent delta.
    Unknown,
    /// The host has an accepted comparison snapshot and any required retained-context evidence.
    Known(&'a Value),
}

/// Plain model-visible data rendered by an extension-owned World State section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedWorldStateFragment {
    role: &'static str,
    markers: (&'static str, &'static str),
    body: String,
}

impl RenderedWorldStateFragment {
    /// Creates a fragment for the `developer` or `user` model-context role.
    ///
    /// The host omits fragments with other roles and retains the previously delivered section
    /// snapshot, or leaves it absent when the section has not been delivered yet.
    pub fn new(
        role: &'static str,
        markers: (&'static str, &'static str),
        body: impl Into<String>,
    ) -> Self {
        Self {
            role,
            markers,
            body: body.into(),
        }
    }

    pub fn role(&self) -> &'static str {
        self.role
    }

    pub fn markers(&self) -> (&'static str, &'static str) {
        self.markers
    }

    pub fn body(&self) -> &str {
        &self.body
    }
}

type RenderDiff = dyn for<'a> Fn(PreviousWorldStateSection<'a>) -> Option<RenderedWorldStateFragment>
    + Send
    + Sync;
type LegacyFragmentMatcher = dyn Fn(&str, &str) -> bool + Send + Sync;

/// One extension-owned World State section captured for a sampling step.
///
/// The extension owns the stable ID, comparison snapshot, and diff rendering. The harness owns
/// persistence and the concrete model-context fragment envelope.
#[derive(Clone)]
pub struct WorldStateSectionContribution {
    id: &'static str,
    unavailable: bool,
    snapshot: Value,
    render_diff: Arc<RenderDiff>,
    matches_legacy_fragment: Arc<LegacyFragmentMatcher>,
    matches_retained_fragment: Option<Arc<LegacyFragmentMatcher>>,
    budget_fallback: Option<(Value, RenderedWorldStateFragment)>,
}

impl WorldStateSectionContribution {
    /// Captures one current state and its renderer.
    ///
    /// `render_diff` must describe `snapshot`. Capture the resolved data needed for rendering;
    /// later mutations of extension state must not change which current state this contribution
    /// renders. The host may render a contribution more than once.
    pub fn new(
        id: &'static str,
        snapshot: Value,
        render_diff: impl for<'a> Fn(
            PreviousWorldStateSection<'a>,
        ) -> Option<RenderedWorldStateFragment>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            id,
            unavailable: false,
            snapshot,
            render_diff: Arc::new(render_diff),
            matches_legacy_fragment: Arc::new(|_, _| false),
            matches_retained_fragment: None,
            budget_fallback: None,
        }
    }

    /// Discovery did not establish a replacement for this section. The host preserves its
    /// last accepted snapshot and delivery, just as on contributor timeout. This is not
    /// successful omission or an empty catalog. Contribute a separate status section when
    /// the model needs an explanation of the unavailable observation.
    pub fn unavailable(id: &'static str) -> Self {
        let mut contribution = Self::new(id, Value::Null, |_| None);
        contribution.unavailable = true;
        contribution
    }

    pub fn is_unavailable(&self) -> bool {
        self.unavailable
    }

    pub fn with_legacy_matcher(
        mut self,
        matcher: impl Fn(&str, &str) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.matches_legacy_fragment = Arc::new(matcher);
        self
    }

    /// Requires a matching model-visible fragment whenever a persisted snapshot is reused.
    ///
    /// A match must establish that retained context supports reusing the comparison baseline.
    /// Recognizing an older fragment's envelope alone is insufficient when its contents no longer
    /// support that baseline. Use the legacy matcher for compatibility cleanup instead.
    pub fn with_retained_fragment_matcher(
        mut self,
        matcher: impl Fn(&str, &str) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.matches_retained_fragment = Some(Arc::new(matcher));
        self
    }

    /// A compact authoritative invalidation when an update cannot fit the optional budget.
    ///
    /// The host uses this only for an already observed section, with a separate 1 KiB control
    /// limit. `snapshot` must describe exactly the fallback, not an undelivered replacement.
    /// Keep a deferred replacement distinct so an unchanged next step can retry it.
    pub fn with_budget_fallback(
        mut self,
        snapshot: Value,
        fragment: RenderedWorldStateFragment,
    ) -> Self {
        self.budget_fallback = Some((snapshot, fragment));
        self
    }

    pub fn budget_fallback(&self) -> Option<&(Value, RenderedWorldStateFragment)> {
        self.budget_fallback.as_ref()
    }

    pub fn id(&self) -> &'static str {
        self.id
    }

    pub fn snapshot(&self) -> &Value {
        &self.snapshot
    }

    /// Renders the captured snapshot relative to the host's available baseline.
    ///
    /// `None` means no model-visible update is needed; the host accepts the current snapshot even
    /// though it emits no fragment. Use it only when that snapshot needs no new context, such as an
    /// unchanged known value or an initially empty section. It must not mean rendering failed or
    /// delivery should be deferred. An unavailable baseline cannot support an ordinary delta.
    pub fn render_diff(
        &self,
        previous: PreviousWorldStateSection<'_>,
    ) -> Option<RenderedWorldStateFragment> {
        (self.render_diff)(previous)
    }

    pub fn matches_legacy_fragment(&self, role: &str, text: &str) -> bool {
        (self.matches_legacy_fragment)(role, text)
    }

    pub fn has_retained_fragment_matcher(&self) -> bool {
        self.matches_retained_fragment.is_some()
    }

    pub fn matches_retained_fragment(&self, role: &str, text: &str) -> bool {
        self.matches_retained_fragment
            .as_ref()
            .is_some_and(|matcher| matcher(role, text))
    }
}
