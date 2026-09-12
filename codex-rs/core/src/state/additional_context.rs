use crate::context::AdditionalContextDeveloperFragment;
use crate::context::AdditionalContextUserFragment;
use crate::context::ContextualUserFragment;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::AdditionalContextKind;
use indexmap::IndexMap;

const ADDITIONAL_CONTEXT_AGGREGATE_BYTE_BUDGET: usize = 160_000;
const ADDITIONAL_CONTEXT_MAX_ITEMS: usize = 256;
const ADDITIONAL_CONTEXT_RESET_SOURCE: &str = "__codex_additional_context_reset__";
const ADDITIONAL_CONTEXT_RESET: &str = "Additional context budget exceeded. All previously supplied additional context values are obsolete (previous_value_obsolete=\"true\"). Only additional-context entries following this reset in the current update remain available. Do not infer omitted values from earlier messages.";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AdditionalContextStore {
    values: IndexMap<String, AdditionalContextEntry>,
}

impl AdditionalContextStore {
    pub(crate) fn merge(
        &mut self,
        values: IndexMap<String, AdditionalContextEntry>,
    ) -> Vec<ResponseInputItem> {
        let mut fragments = Vec::new();
        // Include the JSON array brackets and each message's serialized envelope.
        let mut retained_bytes = 2;
        let mut overflowed = false;
        for (key, entry) in &values {
            if self.values.get(key) == Some(entry) {
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
        }

        if overflowed {
            // An arbitrary set of source names cannot be identified in a bounded
            // omission notice. Reset earlier context explicitly, then replay the
            // admitted portion of the current snapshot, including unchanged values.
            // A developer-level prior value must also be invalidated at that role
            // when its replacement has been downgraded to untrusted context.
            let kind = if self
                .values
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
            retained_bytes = 2 + serialized_item_bytes(&reset) + 1;
            fragments.push(reset);
            for (key, entry) in &values {
                if fragments.len() == ADDITIONAL_CONTEXT_MAX_ITEMS {
                    break;
                }
                // An oversized entry does not consume the remaining budget: a
                // later, smaller entry may still fit the replacement snapshot.
                push_bounded_item(
                    &mut fragments,
                    &mut retained_bytes,
                    render_entry(key, entry),
                );
            }
        }

        // The supplied map remains authoritative even for omitted entries.
        // An unchanged remerge must not restore or repeatedly emit older values.
        self.values = values;
        fragments
    }
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

fn serialized_item_bytes(item: &ResponseInputItem) -> usize {
    // These messages contain only strings and InputText, whose JSON serialization
    // is infallible. Count actual escaping instead of assuming a fixed envelope.
    serde_json::to_vec(item)
        .expect("additional-context text messages are serializable")
        .len()
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

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::ContentItem;

    #[test]
    fn preserves_source_insertion_order() {
        let values = IndexMap::from([
            (
                "z-first".to_string(),
                AdditionalContextEntry {
                    value: "first".to_string(),
                    kind: AdditionalContextKind::Application,
                },
            ),
            (
                "a-second".to_string(),
                AdditionalContextEntry {
                    value: "second".to_string(),
                    kind: AdditionalContextKind::Application,
                },
            ),
        ]);
        let mut store = AdditionalContextStore::default();

        let fragments = store.merge(values);

        assert!(input_text(&fragments[0]).contains("z-first"));
        assert!(input_text(&fragments[1]).contains("a-second"));
    }

    #[test]
    fn over_budget_updates_state_instead_of_restoring_stale_values() {
        let mut store = AdditionalContextStore::default();
        let unchanged = AdditionalContextEntry {
            value: "unchanged value remains available".to_string(),
            kind: AdditionalContextKind::Application,
        };
        store.merge(IndexMap::from([
            ("unchanged".to_string(), unchanged.clone()),
            (
                "target".to_string(),
                AdditionalContextEntry {
                    value: "old developer value".to_string(),
                    kind: AdditionalContextKind::Application,
                },
            ),
        ]));

        let mut values = IndexMap::from([("unchanged".to_string(), unchanged)]);
        for index in 0..80 {
            values.insert(
                format!("source-{index:02}-{}", "\"\n\\".repeat(8_000)),
                AdditionalContextEntry {
                    value: "a".repeat(20_000),
                    kind: AdditionalContextKind::Application,
                },
            );
        }
        values.insert(
            "target".to_string(),
            AdditionalContextEntry {
                value: "new untrusted value".to_string(),
                kind: AdditionalContextKind::Untrusted,
            },
        );

        let fragments = store.merge(values.clone());

        assert!(serde_json::to_vec(&fragments).unwrap().len() <= 160_000);
        assert!(fragments.len() <= 256);
        assert!(matches!(
            &fragments[0],
            ResponseInputItem::Message { role, .. } if role == "developer"
        ));
        let reset = input_text(&fragments[0]);
        assert!(reset.contains("All previously supplied additional context values are obsolete"));
        assert!(reset.contains("previous_value_obsolete=\"true\""));
        assert!(reset.contains("Only additional-context entries following this reset"));
        assert!(reset.len() < 1_024);
        assert_eq!(
            fragments
                .iter()
                .filter(|item| input_text(item).contains("__codex_additional_context_reset__"))
                .count(),
            1
        );
        assert!(
            fragments
                .iter()
                .skip(1)
                .any(|item| { input_text(item).contains("unchanged value remains available") })
        );
        assert!(
            fragments
                .iter()
                .all(|item| !input_text(item).contains("old developer value"))
        );
        assert_eq!(store.values, values);
        assert!(store.merge(values).is_empty());
    }

    #[test]
    fn many_small_entries_bound_message_count_and_preserve_authoritative_state() {
        let values = (0..1_024)
            .map(|index| {
                (
                    format!("source-{index:04}"),
                    AdditionalContextEntry {
                        value: "small".to_string(),
                        kind: AdditionalContextKind::Untrusted,
                    },
                )
            })
            .collect::<IndexMap<_, _>>();
        let mut store = AdditionalContextStore::default();

        let fragments = store.merge(values.clone());

        assert_eq!(fragments.len(), 256);
        assert!(serde_json::to_vec(&fragments).unwrap().len() <= 160_000);
        assert!(matches!(
            &fragments[0],
            ResponseInputItem::Message { role, .. } if role == "user"
        ));
        assert!(
            input_text(&fragments[0])
                .contains("All previously supplied additional context values are obsolete")
        );
        assert!(input_text(&fragments[1]).contains("source-0000"));
        assert!(input_text(fragments.last().unwrap()).contains("source-0254"));
        assert_eq!(store.values, values);
        assert!(store.merge(values).is_empty());
    }

    fn input_text(item: &ResponseInputItem) -> &str {
        let ResponseInputItem::Message { content, .. } = item else {
            panic!("expected additional context message");
        };
        let Some(ContentItem::InputText { text }) = content.first() else {
            panic!("expected additional context input text");
        };
        text
    }
}
