use crate::context::AdditionalContextDeveloperFragment;
use crate::context::AdditionalContextUserFragment;
use crate::context::ContextualUserFragment;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::AdditionalContextKind;
use indexmap::IndexMap;

const ADDITIONAL_CONTEXT_AGGREGATE_BYTE_BUDGET: usize = 160_000;
const ADDITIONAL_CONTEXT_MAX_ITEMS: usize = 256;
const ADDITIONAL_CONTEXT_RESET_SOURCE: &str = "__codex_additional_context_reset__";
const ADDITIONAL_CONTEXT_RESET: &str = "Additional context snapshot replaced. All previously supplied additional context values are obsolete (previous_value_obsolete=\"true\"). Only additional-context entries following this reset in the current update remain available. Do not infer omitted values from earlier messages.";

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct AdditionalContextStore {
    values: IndexMap<String, AdditionalContextEntry>,
    delivered: IndexMap<String, AdditionalContextEntry>,
    delivery_invalidated: bool,
    #[serde(default)]
    legacy: Vec<ResponseItem>,
}

impl AdditionalContextStore {
    pub(crate) fn is_current(&self, values: &IndexMap<String, AdditionalContextEntry>) -> bool {
        self.values == *values && !self.delivery_invalidated && self.legacy.is_empty()
    }

    pub(crate) fn from_legacy_history(history: &[ResponseItem]) -> Self {
        Self {
            legacy: history
                .iter()
                .filter(|item| {
                    matches!(item,
                ResponseItem::Message { role, content, .. } if role == "developer"
                    && content.iter().any(|part| matches!(part,
                        codex_protocol::models::ContentItem::InputText { text }
                            if AdditionalContextDeveloperFragment::matches_text(text))))
                })
                .cloned()
                .collect(),
            ..Self::default()
        }
    }

    pub(crate) fn restore(value: &serde_json::Value, history: &[ResponseItem]) -> Option<Self> {
        let mut state: Self = serde_json::from_value(value.clone()).ok()?;
        Self::validate_application_context(&state.values).ok()?;
        if state
            .delivered
            .iter()
            .any(|(key, entry)| state.values.get(key) != Some(entry))
        {
            return None;
        }
        state.legacy.retain(|item| history.contains(item));
        state.reconcile_history(history);
        Some(state)
    }

    /// Account for the complete source before excerpt truncation. Otherwise a
    /// huge source appears to fit and its omitted middle becomes unrecoverable.
    /// The application envelope supplies a conservative, untruncated size only;
    /// it never changes the authority of the admitted or retained source.
    pub(crate) fn recovery_sources(
        values: &IndexMap<String, AdditionalContextEntry>,
    ) -> Vec<String> {
        let mut remaining = ADDITIONAL_CONTEXT_AGGREGATE_BYTE_BUDGET.saturating_sub(2050);
        let mut sources = Vec::new();
        for (key, entry) in policy_first(values) {
            let full_source =
                AdditionalContextDeveloperFragment::new(key.clone(), entry.value.clone())
                    .into_response_input_item();
            let bytes = serialized_item_bytes(&full_source).saturating_add(1);
            if bytes <= remaining || entry.kind == AdditionalContextKind::Application {
                remaining = remaining.saturating_sub(bytes);
            } else {
                sources.push(key.clone());
                // Reserve a bounded recovery notice so later small values do
                // not spend the room needed to keep this source recoverable.
                remaining = remaining.saturating_sub(2048);
            }
        }
        sources
    }

    /// Reject a policy update atomically rather than silently dropping its middle
    /// or accepting only some of its application sources.
    pub(crate) fn validate_application_context(
        values: &IndexMap<String, AdditionalContextEntry>,
    ) -> Result<(), String> {
        let mut bytes = 2usize;
        let mut count = 0usize;
        for (key, entry) in values
            .iter()
            .filter(|(_, entry)| entry.kind == AdditionalContextKind::Application)
        {
            count += 1;
            bytes = bytes.saturating_add(serialized_item_bytes(&render_entry(key, entry)) + 1);
        }
        // Reserve one reset/rejection notice for replacing an older snapshot.
        if count >= ADDITIONAL_CONTEXT_MAX_ITEMS
            || bytes > ADDITIONAL_CONTEXT_AGGREGATE_BYTE_BUDGET - 2048
        {
            return Err(format!(
                "Application policy update rejected: {bytes} serialized bytes across {count} fragments exceed the whole-policy allowance. Previous application policy is unchanged."
            ));
        }
        Ok(())
    }
    /// Compaction must install application policy without waiting for a client
    /// resend. Reuse normal bounded admission for the authoritative snapshot.
    pub(crate) fn application_context_for_replacement(
        &self,
        history: &[ResponseItem],
    ) -> Vec<ResponseItem> {
        let values = self
            .values
            .iter()
            .filter(|(_, entry)| entry.kind == AdditionalContextKind::Application)
            .map(|(key, entry)| (key.clone(), entry.clone()))
            .collect();
        let mut retained = self
            .legacy
            .iter()
            .filter(|item| !history.contains(item))
            .cloned()
            .collect::<Vec<_>>();
        retained.extend(
            Self::default()
                .merge(values)
                .into_iter()
                .filter(|input| {
                    let ResponseInputItem::Message { role, content, .. } = input else {
                        return false;
                    };
                    !history.iter().any(|item| {
                        matches!(item,
                            ResponseItem::Message { role: retained_role, content: retained_content, .. }
                                if retained_role == role && retained_content == content
                        )
                    })
                })
                .map(ResponseItem::from),
        );
        retained
    }

    /// Reconcile acknowledgements, not the authoritative supplied snapshot. An
    /// identical map must be eligible for delivery again after context loss.
    pub(crate) fn reconcile_history(&mut self, items: &[ResponseItem]) {
        let before = self.delivered.len();
        self.delivered.retain(|key, entry| {
            let ResponseInputItem::Message { role, content, .. } = render_entry(key, entry) else {
                return false;
            };
            items.iter().any(|item| {
                matches!(item,
                    ResponseItem::Message { role: retained_role, content: retained_content, .. }
                        if retained_role == &role && retained_content == &content
                )
            })
        });
        self.delivery_invalidated |= self.delivered.len() != before;
    }

    pub(crate) fn merge(
        &mut self,
        values: IndexMap<String, AdditionalContextEntry>,
    ) -> Vec<ResponseInputItem> {
        if self.is_current(&values) {
            return Vec::new();
        }
        let mut fragments = Vec::new();
        // Include the JSON array brackets and each message's serialized envelope.
        let mut retained_bytes = 2;
        let mut overflowed = false;
        let mut delivered = self.delivered.clone();
        if !self.legacy.is_empty() {
            let reset = render_entry(
                ADDITIONAL_CONTEXT_RESET_SOURCE,
                &AdditionalContextEntry {
                    value: ADDITIONAL_CONTEXT_RESET.to_string(),
                    kind: AdditionalContextKind::Application,
                },
            );
            push_bounded_item(&mut fragments, &mut retained_bytes, reset);
            self.legacy.clear();
        }
        for (key, old) in &self.delivered {
            if values.get(key).is_some_and(|new| new.kind == old.kind) {
                continue;
            }
            // Revoke at the former authority, including application -> untrusted
            // transitions. Unchanged sources need neither a reset nor a replay.
            let tombstone = render_entry(key, &AdditionalContextEntry {
                value: "This source's previous additional-context value is obsolete (previous_value_obsolete=\"true\"). It is no longer available; do not treat earlier values from this source as current. Any replacement follows separately.".to_string(),
                kind: old.kind,
            });
            if !push_bounded_item(&mut fragments, &mut retained_bytes, tombstone) {
                overflowed = true;
                break;
            }
            delivered.shift_remove(key);
        }
        for (key, entry) in policy_first(&values) {
            if overflowed {
                break;
            }
            if self.delivered.get(key) == Some(entry) {
                continue;
            }
            if !push_bounded_item(
                &mut fragments,
                &mut retained_bytes,
                render_entry(key, entry),
            ) {
                overflowed = true;
                break;
            }
            delivered.insert(key.clone(), entry.clone());
        }

        if overflowed {
            // An arbitrary set of source names cannot be identified in a bounded
            // omission notice. Reset earlier context explicitly, then replay the
            // admitted portion of the current snapshot, including unchanged values.
            // A developer-level prior value must also be invalidated at that role
            // when its replacement has been downgraded to untrusted context.
            let kind = if self
                .delivered
                .values()
                .any(|entry| entry.kind == AdditionalContextKind::Application)
            {
                AdditionalContextKind::Application
            } else {
                AdditionalContextKind::Untrusted
            };
            let reset = render_entry(
                ADDITIONAL_CONTEXT_RESET_SOURCE,
                &AdditionalContextEntry {
                    value: ADDITIONAL_CONTEXT_RESET.to_string(),
                    kind,
                },
            );
            fragments.clear();
            delivered.clear();
            retained_bytes = 2 + serialized_item_bytes(&reset) + 1;
            fragments.push(reset);
            for (key, entry) in policy_first(&values) {
                if fragments.len() == ADDITIONAL_CONTEXT_MAX_ITEMS {
                    break;
                }
                // An oversized entry does not consume the remaining budget: a
                // later, smaller entry may still fit the replacement snapshot.
                if push_bounded_item(
                    &mut fragments,
                    &mut retained_bytes,
                    render_entry(key, entry),
                ) {
                    delivered.insert(key.clone(), entry.clone());
                }
            }
        }

        // The supplied map remains authoritative even for omitted entries.
        // An unchanged remerge must not restore or repeatedly emit older values.
        self.values = values;
        self.delivered = delivered;
        self.delivery_invalidated = false;
        fragments
    }
}

fn policy_first(
    values: &IndexMap<String, AdditionalContextEntry>,
) -> impl Iterator<Item = (&String, &AdditionalContextEntry)> {
    values
        .iter()
        .filter(|(_, entry)| entry.kind == AdditionalContextKind::Application)
        .chain(
            values
                .iter()
                .filter(|(_, entry)| entry.kind != AdditionalContextKind::Application),
        )
}

fn render_entry(key: &str, entry: &AdditionalContextEntry) -> ResponseInputItem {
    match entry.kind {
        AdditionalContextKind::Untrusted => {
            AdditionalContextUserFragment::new(key.to_string(), entry.value.clone())
                .into_response_input_item()
        }
        AdditionalContextKind::Application => {
            AdditionalContextDeveloperFragment::new(key.to_string(), entry.value.clone())
                .into_response_input_item()
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "Additional context contains only text messages with infallible JSON serialization"
)]
fn serialized_item_bytes(item: &ResponseInputItem) -> usize {
    struct ByteCounter(usize);

    impl std::io::Write for ByteCounter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // These messages contain only strings and InputText, whose JSON serialization
    // is infallible. Count actual escaping instead of assuming a fixed envelope.
    let mut counter = ByteCounter(0);
    serde_json::to_writer(&mut counter, item)
        .expect("additional-context text messages are serializable");
    counter.0
}

fn push_bounded_item(
    fragments: &mut Vec<ResponseInputItem>,
    retained_bytes: &mut usize,
    item: ResponseInputItem,
) -> bool {
    let item_bytes = serialized_item_bytes(&item) + 1;
    if fragments.len() == ADDITIONAL_CONTEXT_MAX_ITEMS
        || item_bytes > ADDITIONAL_CONTEXT_AGGREGATE_BYTE_BUDGET.saturating_sub(*retained_bytes)
    {
        return false;
    }
    *retained_bytes += item_bytes;
    fragments.push(item);
    true
}

