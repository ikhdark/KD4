use super::PreviousSectionState;
use super::WorldStateSection;
use crate::agents_md::AgentsMdFreshness;
use crate::agents_md::LoadedAgentsMd;
use crate::agents_md::RepositoryStableContextBundle;
use crate::context::ContextualUserFragment;
use crate::context::InternalContextSource;
use crate::context::InternalModelContextFragment;
use crate::context::UserInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

const REPLACEMENT_NOTICE: &str =
    "These AGENTS.md instructions replace all previously provided AGENTS.md instructions.";
const GLOBAL_SUPERSESSION_NOTICE: &str = "The current host/global snapshot supersedes any global instructions in the retained aggregate, including when the current snapshot supplies no global instructions. Retained project restrictions keep their original scope.";
/// A line diff is hidden context, not the repository slot: retiring superseded
/// slots must keep the complete body it applies to.
const UPDATE_SOURCE: &str = "agents_md_update";
const UPDATE_OPEN: &str = "<codex_internal_context source=\"agents_md_update\">";
const UPDATE_CLOSE: &str = "</codex_internal_context>";
const UPDATE_IDENTITY: &str = "(diff identity: base ";
/// A changed body is sent as a line diff only when the diff is at most this
/// large and at most half the size of the body.
const MAX_UPDATE_BYTES: usize = 4 * 1024;

/// The AGENTS.md instructions currently visible to the model.
#[derive(Clone, Debug)]
pub(crate) struct AgentsMdState {
    instructions: Option<UserInstructions>,
    freshness: AgentsMdFreshness,
    source_snapshot: Option<serde_json::Value>,
    legacy_retained: Option<LegacyAgentsMd>,
    instructions_are_recovered_aggregate: bool,
}

impl Default for AgentsMdState {
    fn default() -> Self {
        Self::from_instructions(None, AgentsMdFreshness::Refreshed)
    }
}

/// Persisted model-visible AGENTS.md state and the freshness of its filesystem observation.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct AgentsMdSnapshot {
    directory: Option<String>,
    text: Option<String>,
    #[serde(default)]
    freshness: AgentsMdFreshness,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_snapshot: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    legacy_retained: Option<LegacyAgentsMd>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct LegacyAgentsMd {
    directory: Option<String>,
    text: String,
}

impl AgentsMdState {
    pub(crate) const REMOVAL_NOTICE: &str =
        "The previously provided AGENTS.md instructions no longer apply.";

    /// Whether `text` is an AGENTS.md line diff as delivered. Compaction drops
    /// these together with the complete body they apply to.
    pub(crate) fn is_update_text(text: &str) -> bool {
        text.starts_with(UPDATE_OPEN)
    }

    #[cfg(test)]
    pub(crate) fn new(loaded: Option<&LoadedAgentsMd>) -> Self {
        Self::from_instructions(
            loaded.map(LoadedAgentsMd::contextual_user_fragment),
            AgentsMdFreshness::Refreshed,
        )
    }

    pub(crate) fn new_cached(
        loaded: Option<&LoadedAgentsMd>,
        stable_context: Option<&RepositoryStableContextBundle>,
        freshness: AgentsMdFreshness,
    ) -> Self {
        let mut state = Self::from_instructions(
            loaded.map(|loaded| {
                let rendered = stable_context
                    .map(|bundle| bundle.rendered.to_string())
                    .unwrap_or_else(|| loaded.text());
                loaded.contextual_user_fragment_with_text(rendered)
            }),
            freshness,
        );
        state.source_snapshot = stable_context.and_then(|bundle| bundle.source_snapshot.clone());
        state
    }

    /// Recover an accepted body after resume when the manager has no in-memory
    /// fallback. Keep its original scope and persist it through full reinjection.
    /// The closure is never evaluated on the normal successful-read path.
    pub(crate) fn with_unavailable_fallback(
        mut self,
        previous: impl FnOnce() -> Option<serde_json::Value>,
    ) -> Self {
        if self.freshness != AgentsMdFreshness::Refreshed
            && let Some(previous) =
                previous().and_then(|value| serde_json::from_value::<AgentsMdSnapshot>(value).ok())
            && let Some(body) = instruction_body(&previous)
        {
            if self.instructions.is_none() {
                self.instructions = Some(UserInstructions {
                    directory: previous.directory.clone(),
                    text: body.to_string(),
                });
                self.source_snapshot = previous.source_snapshot.clone();
                // The body is already rendered. Preserve its original opaque
                // fallback separately, without wrapping that body a second time.
                self.legacy_retained = previous.legacy_retained.clone().or_else(|| {
                    Some(LegacyAgentsMd {
                        directory: previous.directory.clone(),
                        text: body.to_string(),
                    })
                });
                self.instructions_are_recovered_aggregate = true;
            } else if previous.source_snapshot.is_none()
                || previous.legacy_retained.is_some()
                || !previous
                    .source_snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.get("observation_scope"))
                    .filter(|scope| !scope.is_null())
                    .is_some_and(|scope| {
                        self.source_snapshot
                            .as_ref()
                            .and_then(|snapshot| snapshot.get("observation_scope"))
                            == Some(scope)
                    })
            {
                // Legacy or mismatched scope cannot seed current source state.
                // Keep it opaque at its original scope, distinct from known
                // current contributions, even when the cwd string is unchanged.
                self.legacy_retained = previous.legacy_retained.clone().or_else(|| {
                    Some(LegacyAgentsMd {
                        directory: previous.directory.clone(),
                        text: body.to_string(),
                    })
                });
            }
            self.freshness = AgentsMdFreshness::CachedFallback;
        }
        self
    }

    fn from_instructions(
        instructions: Option<UserInstructions>,
        freshness: AgentsMdFreshness,
    ) -> Self {
        Self {
            instructions,
            freshness,
            source_snapshot: None,
            legacy_retained: None,
            instructions_are_recovered_aggregate: false,
        }
    }

    fn effective_instructions(&self) -> Option<UserInstructions> {
        let current = self.instructions.as_ref()?;
        if self.instructions_are_recovered_aggregate {
            return Some(current.clone());
        }
        let Some(legacy) = &self.legacy_retained else {
            return Some(current.clone());
        };
        Some(UserInstructions {
            directory: None,
            text: format!(
                "Incomplete repository observation: the retained legacy aggregate has unresolved source membership. Its instructions retain only their original scope ({}), not the current cwd. {GLOBAL_SUPERSESSION_NOTICE} Known current contributions below supersede matching earlier sources, not other retained constraints.\n\nRetained legacy instructions:\n{}\n\nKnown current contributions (scope: {}):\n{}",
                legacy
                    .directory
                    .as_deref()
                    .unwrap_or("source scopes stated in the body"),
                legacy.text,
                current
                    .directory
                    .as_deref()
                    .unwrap_or("source scopes stated in the body"),
                current.text
            ),
        })
    }

    /// A body change within one verified scope as a bounded line diff from the
    /// accepted body, so a mid-turn edit does not append another complete copy.
    /// `None` selects the complete replacement: unverified, recovered or legacy
    /// state, a changed scope, or a diff that is large or not much smaller than
    /// the body.
    fn render_update(
        &self,
        previous: &AgentsMdSnapshot,
        current: &AgentsMdSnapshot,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        if previous.freshness != AgentsMdFreshness::Refreshed
            || current.freshness != AgentsMdFreshness::Refreshed
            || previous.legacy_retained.is_some()
            || current.legacy_retained.is_some()
            || self.instructions_are_recovered_aggregate
            || previous.directory != current.directory
        {
            return None;
        }
        let (old, new) = (instruction_body(previous)?, instruction_body(current)?);
        let limit = MAX_UPDATE_BYTES.min(UserInstructions::escaped_text_len(new) / 2);
        if old.len().abs_diff(new.len()) > limit {
            return None;
        }
        let scope = current
            .directory
            .as_deref()
            .map(|directory| format!(" for {directory}"))
            .unwrap_or_default();
        let update = InternalModelContextFragment::new(
            InternalContextSource::from_static(UPDATE_SOURCE),
            format!(
                "AGENTS.md changed. Apply this line diff to the most recent complete AGENTS.md instructions{scope} above, after any earlier diffs to them: \"-\" lines no longer apply, \"+\" lines now apply, and all other lines are unchanged.\n{}\n{UPDATE_IDENTITY}{}, result {})",
                line_diff(old, new)?,
                state_digest(previous.directory.as_deref(), old),
                state_digest(current.directory.as_deref(), new),
            ),
        );
        (update.render().len() <= limit)
            .then(|| Box::new(update) as Box<dyn ContextualUserFragment>)
    }
}

impl WorldStateSection for AgentsMdState {
    const ID: &'static str = "agents_md";
    type Snapshot = AgentsMdSnapshot;

    fn snapshot(&self) -> Self::Snapshot {
        match self.effective_instructions() {
            Some(instructions) => AgentsMdSnapshot {
                directory: instructions.directory.clone(),
                // Preserve the persisted format used by existing rollout baselines.
                text: Some(format!(
                    "{}\n\n{}",
                    self.freshness.model_visible_description(),
                    instructions.text
                )),
                freshness: self.freshness,
                source_snapshot: self.source_snapshot.clone(),
                legacy_retained: self.legacy_retained.clone(),
            },
            None => AgentsMdSnapshot {
                freshness: self.freshness,
                ..AgentsMdSnapshot::default()
            },
        }
    }

    fn matches_legacy_fragment(role: &str, text: &str) -> bool {
        role == "user" && (UserInstructions::matches_text(text) || parse_update(text).is_some())
    }

    fn required() -> bool {
        true
    }

    fn retained_state_supported(
        previous: &Self::Snapshot,
        items: &[codex_protocol::models::ResponseItem],
    ) -> bool {
        // Freshness-only notices do not replace the last substantive instruction body.
        let deliveries = retained_deliveries(items);
        let Some(&(_, text)) = deliveries.last() else {
            if previous.text.is_some() {
                return false;
            }
            let observation = super::retained_texts(items, "developer")
                .rev()
                .find(|text| text.starts_with("<repository_observation>"));
            let expected = observation_text(previous);
            return if previous.freshness == AgentsMdFreshness::Refreshed {
                observation.is_none_or(|text| text == expected)
            } else {
                observation.is_some_and(|text| text == expected)
            };
        };
        if parse_update(text).is_some() {
            if !update_chain_supported(previous, items, &deliveries) {
                return false;
            }
        } else {
            let expected_body = instruction_body(previous).unwrap_or(Self::REMOVAL_NOTICE);
            let instructions = UserInstructions {
                directory: previous.directory.clone(),
                text: expected_body.to_string(),
            };
            let rendered = instructions.render();
            // Unframed renders cannot be matched against retained text; resend them.
            let Some((header, body)) = rendered.split_once("<INSTRUCTIONS>\n") else {
                return false;
            };
            let header = header.trim_end();
            let body_retained = text.starts_with(&format!("{header}\n\n"))
                && text
                    .split_once("<INSTRUCTIONS>\n")
                    .is_some_and(|(_, actual)| {
                        actual == body
                            || actual.strip_prefix(&format!("{REPLACEMENT_NOTICE}\n\n"))
                                == Some(body)
                    });
            if !body_retained {
                return false;
            }
        }
        // A warning from an older body must not certify the retained replacement.
        let expected = observation_text(previous);
        let mut seen_body = false;
        let mut observation = None;
        for item in items {
            let codex_protocol::models::ResponseItem::Message { role, content, .. } = item else {
                continue;
            };
            for part in content {
                let codex_protocol::models::ContentItem::InputText { text: retained } = part else {
                    continue;
                };
                if role == "user" && retained == text {
                    seen_body = true;
                    observation = None;
                } else if seen_body
                    && role == "developer"
                    && retained.starts_with("<repository_observation>")
                {
                    observation = Some(retained);
                }
            }
        }
        if previous.freshness == AgentsMdFreshness::Refreshed {
            observation.is_none_or(|text| text == &expected)
        } else {
            observation.is_some_and(|text| text == &expected)
        }
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        let current = self.snapshot();
        if matches!(previous, PreviousSectionState::Known(previous)
            if previous.directory == current.directory
                && previous.text.is_none() == current.text.is_none()
                && instruction_body(previous) == instruction_body(&current))
        {
            let PreviousSectionState::Known(previous) = previous else {
                unreachable!()
            };
            return (previous.freshness != current.freshness
                || previous.legacy_retained.is_some() != current.legacy_retained.is_some())
            .then(|| {
                Box::new(codex_context_fragments::RenderedContextFragment::new(
                    "developer",
                    observation_text(&current),
                )) as Box<dyn ContextualUserFragment>
            });
        }

        if self.instructions.is_none() && self.freshness != AgentsMdFreshness::Refreshed {
            return Some(Box::new(
                codex_context_fragments::RenderedContextFragment::new(
                    "developer",
                    observation_text(&current),
                ),
            ));
        }

        if let PreviousSectionState::Known(previous) = previous
            && let Some(update) = self.render_update(previous, &current)
        {
            return Some(update);
        }

        let previous_may_contain_instructions = match previous {
            PreviousSectionState::Known(previous) => previous.text.is_some(),
            PreviousSectionState::Unknown => true,
            PreviousSectionState::Absent => false,
        };
        let effective = self.effective_instructions();
        let instructions = match (&effective, previous_may_contain_instructions) {
            (Some(instructions), true) => UserInstructions {
                directory: instructions.directory.clone(),
                text: format!("{REPLACEMENT_NOTICE}\n\n{}", instructions.text),
            },
            (Some(instructions), false) => instructions.clone(),
            (None, true) => UserInstructions {
                directory: None,
                text: Self::REMOVAL_NOTICE.to_string(),
            },
            (None, false) => return None,
        };
        Some(Box::new(instructions))
    }

    fn render_fragments(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> Vec<Box<dyn ContextualUserFragment>> {
        let mut fragments = self.render_diff(previous).into_iter().collect::<Vec<_>>();
        if self.freshness != AgentsMdFreshness::Refreshed
            && fragments
                .first()
                .is_some_and(|fragment| fragment.role() == "user")
        {
            fragments.push(Box::new(
                codex_context_fragments::RenderedContextFragment::new(
                    "developer",
                    observation_text(&self.snapshot()),
                ),
            ));
        }
        fragments
    }
}

fn observation_text(snapshot: &AgentsMdSnapshot) -> String {
    if snapshot.text.is_none() && snapshot.freshness != AgentsMdFreshness::Refreshed {
        return format!(
            "<repository_observation>\nAGENTS.md observation unavailable: {} This is not an instruction removal. Previously accepted instructions retain their original scope; applicability to a new scope is unverified. No automatic retry is requested.\n</repository_observation>",
            snapshot.freshness.model_visible_description(),
        );
    }
    // A tuple of optional strings always serializes.
    let identity =
        serde_json::to_vec(&(&snapshot.directory, instruction_body(snapshot))).unwrap_or_default();
    let global_supersession = if snapshot.legacy_retained.is_some() {
        format!(" {GLOBAL_SUPERSESSION_NOTICE}")
    } else {
        String::new()
    };
    format!(
        "<repository_observation>\nAGENTS.md observation: {} Body/scope sha256: {}. This is not an instruction removal. Previously accepted instructions retain their stated scope.{global_supersession} No automatic retry is requested.\n</repository_observation>",
        snapshot.freshness.model_visible_description(),
        hex::encode(Sha256::digest(identity)),
    )
}

fn instruction_body(snapshot: &AgentsMdSnapshot) -> Option<&str> {
    let text = snapshot.text.as_deref()?;
    Some(
        text.strip_prefix(snapshot.freshness.model_visible_description())
            .and_then(|text| text.strip_prefix("\n\n"))
            .unwrap_or(text),
    )
}

/// Unified-diff hunks with one line of context and no file headers.
fn line_diff(old: &str, new: &str) -> Option<String> {
    let mut config = similar::TextDiff::configure();
    // Past the deadline the diff is coarser but still exact; the size limit
    // then selects the complete body.
    config.timeout(std::time::Duration::from_millis(100));
    let diff = config.diff_lines(old, new);
    let hunks = diff
        .grouped_ops(1)
        .iter()
        .filter_map(|group| {
            let (first, last) = (group.first()?, group.last()?);
            let mut hunk = format!(
                "@@ -{} +{} @@",
                hunk_range(first.old_range().start, last.old_range().end),
                hunk_range(first.new_range().start, last.new_range().end),
            );
            for change in group.iter().flat_map(|op| diff.iter_changes(op)) {
                hunk.push('\n');
                hunk.push(match change.tag() {
                    similar::ChangeTag::Equal => ' ',
                    similar::ChangeTag::Delete => '-',
                    similar::ChangeTag::Insert => '+',
                });
                hunk.push_str(change.value().trim_end_matches(['\r', '\n']));
            }
            Some(hunk)
        })
        .collect::<Vec<_>>();
    (!hunks.is_empty()).then(|| hunks.join("\n"))
}

/// A unified-diff line range: the 1-based first line and the line count, or
/// the preceding line for an empty range.
fn hunk_range(start: usize, end: usize) -> String {
    match end - start {
        0 => format!("{start},0"),
        1 => (start + 1).to_string(),
        len => format!("{},{len}", start + 1),
    }
}

/// Short identity of a complete rendered body; line diffs chain through it.
fn rendered_digest(rendered: &str) -> String {
    hex::encode(&Sha256::digest(rendered.as_bytes())[..8])
}

fn state_digest(directory: Option<&str>, body: &str) -> String {
    rendered_digest(
        &UserInstructions {
            directory: directory.map(str::to_string),
            text: body.to_string(),
        }
        .render(),
    )
}

/// The base and result identities of a retained line diff.
fn parse_update(text: &str) -> Option<(&str, &str)> {
    let body = text
        .trim()
        .strip_prefix(UPDATE_OPEN)?
        .strip_suffix(UPDATE_CLOSE)?
        .trim_end();
    let identity = body.rsplit_once('\n').map_or(body, |(_, line)| line);
    identity
        .strip_prefix(UPDATE_IDENTITY)?
        .strip_suffix(')')?
        .split_once(", result ")
}

/// Substantive deliveries in history order, with their item index: complete
/// bodies (not freshness-only notices) and line diffs.
fn retained_deliveries(items: &[ResponseItem]) -> Vec<(usize, &str)> {
    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| match item {
            ResponseItem::Message { role, content, .. } if role == "user" => Some((index, content)),
            _ => None,
        })
        .flat_map(|(index, content)| {
            content.iter().filter_map(move |part| match part {
                ContentItem::InputText { text }
                    if (UserInstructions::matches_text(text)
                        && !text.contains("<INSTRUCTIONS>\nThe previously provided instruction body is unchanged.\n</INSTRUCTIONS>"))
                        || parse_update(text).is_some() =>
                {
                    Some((index, text.as_str()))
                }
                _ => None,
            })
        })
        .collect()
}

/// Whether retained line diffs lead from a retained complete body to
/// `previous`. They stand only within the turn that sent them: after a later
/// real user message (a new turn, or steering) the complete body is sent
/// again, as it is once the diffs outgrow half of that body.
fn update_chain_supported(
    previous: &AgentsMdSnapshot,
    items: &[ResponseItem],
    deliveries: &[(usize, &str)],
) -> bool {
    let Some(body) = instruction_body(previous) else {
        return false;
    };
    let mut expected = state_digest(previous.directory.as_deref(), body);
    let mut first_update: Option<usize> = None;
    let mut update_bytes = 0_usize;
    for &(index, text) in deliveries.iter().rev() {
        let Some((base, result)) = parse_update(text) else {
            return first_update.is_some_and(|first| {
                rendered_digest(&without_replacement_notice(text)) == expected
                    && update_bytes.saturating_mul(2) <= text.len()
                    && !items[first + 1..].iter().any(is_real_user_message)
            });
        };
        if result != expected {
            return false;
        }
        expected = base.to_string();
        update_bytes = update_bytes.saturating_add(text.len());
        first_update = Some(index);
    }
    false
}

/// A retained complete body as rendered without its replacement notice.
fn without_replacement_notice(text: &str) -> std::borrow::Cow<'_, str> {
    text.split_once("<INSTRUCTIONS>\n")
        .and_then(|(header, body)| {
            let body = body.strip_prefix(REPLACEMENT_NOTICE)?.strip_prefix("\n\n")?;
            Some(std::borrow::Cow::Owned(format!("{header}<INSTRUCTIONS>\n{body}")))
        })
        .unwrap_or(std::borrow::Cow::Borrowed(text))
}

/// A user turn boundary as sampling projection counts one.
fn is_real_user_message(item: &ResponseItem) -> bool {
    matches!(item, ResponseItem::Message { role, .. } if role == "user")
        && codex_context_fragments::is_user_turn_boundary(item)
        && !crate::compact::is_compaction_summary_item(item)
        && !crate::stable_context::is_trusted_stable_context_item(item)
}

#[cfg(test)]
#[path = "agents_md_tests.rs"]
mod tests;
