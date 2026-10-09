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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AdditionalContextStore {
    values: IndexMap<String, AdditionalContextEntry>,
    delivered: IndexMap<String, AdditionalContextEntry>,
    delivery_invalidated: bool,
}

impl AdditionalContextStore {
    /// Account for the complete source before excerpt truncation. Otherwise a
    /// huge source appears to fit and its omitted middle becomes unrecoverable.
    /// The application envelope supplies a conservative, untruncated size only;
    /// it never changes the authority of the admitted or retained source.
    pub(crate) fn recovery_sources(values: &IndexMap<String, AdditionalContextEntry>) -> Vec<String> {
        let mut remaining = ADDITIONAL_CONTEXT_AGGREGATE_BYTE_BUDGET.saturating_sub(2050);
        let mut sources = Vec::new();
        for (key, entry) in policy_first(values) {
            let full_source = AdditionalContextDeveloperFragment::new(key.clone(), entry.value.clone())
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
    pub(crate) fn validate_application_context(values: &IndexMap<String, AdditionalContextEntry>) -> Result<(), String> {
        let mut bytes = 2usize;
        let mut count = 0usize;
        for (key, entry) in values.iter().filter(|(_, entry)| entry.kind == AdditionalContextKind::Application) {
            count += 1;
            bytes = bytes.saturating_add(serialized_item_bytes(&render_entry(key, entry)) + 1);
        }
        // Reserve one reset/rejection notice for replacing an older snapshot.
        if count >= ADDITIONAL_CONTEXT_MAX_ITEMS || bytes > ADDITIONAL_CONTEXT_AGGREGATE_BYTE_BUDGET - 2048 {
            return Err(format!("Application policy update rejected: {bytes} serialized bytes across {count} fragments exceed the whole-policy allowance. Previous application policy is unchanged."));
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
            .map(ResponseItem::from)
            .collect()
    }

    /// Reconcile acknowledgements, not the authoritative supplied snapshot. An
    /// identical map must be eligible for delivery again after context loss.
    pub(crate) fn reconcile_history(&mut self, items: &[ResponseItem]) {
        let before = self.delivered.len();
        self.delivered.retain(|key, entry| {
            let ResponseInputItem::Message { role, content, .. } = render_entry(key, entry) else {
                return false;
            };
            items.iter().any(|item| matches!(item,
                ResponseItem::Message { role: retained_role, content: retained_content, .. }
                    if retained_role == &role && retained_content == &content
            ))
        });
        self.delivery_invalidated |= self.delivered.len() != before;
    }

    pub(crate) fn merge(
        &mut self,
        values: IndexMap<String, AdditionalContextEntry>,
    ) -> Vec<ResponseInputItem> {
        if self.values == values && !self.delivery_invalidated {
            return Vec::new();
        }
        let mut fragments = Vec::new();
        // Include the JSON array brackets and each message's serialized envelope.
        let mut retained_bytes = 2;
        let mut overflowed = false;
        let mut delivered = self.delivered.clone();
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

fn policy_first(values: &IndexMap<String, AdditionalContextEntry>) -> impl Iterator<Item = (&String, &AdditionalContextEntry)> {
    values.iter().filter(|(_, entry)| entry.kind == AdditionalContextKind::Application)
        .chain(values.iter().filter(|(_, entry)| entry.kind != AdditionalContextKind::Application))
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

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::ContentItem;

    #[test]
    fn application_policy_is_whole_and_oversized_updates_are_rejected() {
        let mut values = IndexMap::from([("policy".to_string(), AdditionalContextEntry {
            value: format!("{}Never modify X.{}", "a".repeat(7000), "b".repeat(7000)),
            kind: AdditionalContextKind::Application,
        })]);
        assert!(AdditionalContextStore::validate_application_context(&values).is_ok());
        let rendered = AdditionalContextStore::default().merge(values.clone());
        assert_eq!(rendered.len(), 1);
        assert!(input_text(&rendered[0]).contains(&values["policy"].value));
        values["policy"].value = "x".repeat(ADDITIONAL_CONTEXT_AGGREGATE_BYTE_BUDGET);
        assert!(AdditionalContextStore::validate_application_context(&values).is_err());
    }

    #[test]
    fn application_context_is_reinstalled_once_without_a_resend() {
        let values = IndexMap::from([(
            "policy".to_string(),
            AdditionalContextEntry {
                value: "Never modify the protected file.".to_string(),
                kind: AdditionalContextKind::Application,
            },
        )]);
        let mut store = AdditionalContextStore::default();
        let original = store.merge(values.clone());
        let replacement = store.application_context_for_replacement(&[]);
        assert_eq!(
            replacement,
            original
                .into_iter()
                .map(ResponseItem::from)
                .collect::<Vec<_>>()
        );
        store.reconcile_history(&replacement);
        assert!(store.merge(values).is_empty());
        assert!(
            store
                .application_context_for_replacement(&replacement)
                .is_empty()
        );
    }

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

        assert_eq!(fragments.len(), 2);
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

    #[test]
    fn deleted_and_downgraded_context_invalidates_the_delivered_role() {
        let entry = AdditionalContextEntry {
            value: "old".to_string(),
            kind: AdditionalContextKind::Application,
        };
        let mut store = AdditionalContextStore::default();
        store.merge(IndexMap::from([("source".to_string(), entry.clone())]));
        let removed = store.merge(IndexMap::new());
        assert_eq!(removed.len(), 1);
        assert!(
            matches!(&removed[0], ResponseInputItem::Message { role, .. } if role == "developer")
        );
        assert!(input_text(&removed[0]).contains("previous_value_obsolete"));
        assert!(store.merge(IndexMap::new()).is_empty());
        store.merge(IndexMap::from([("source".to_string(), entry)]));
        let downgraded = store.merge(IndexMap::from([(
            "source".to_string(),
            AdditionalContextEntry {
                value: "new".to_string(),
                kind: AdditionalContextKind::Untrusted,
            },
        )]));
        assert_eq!(downgraded.len(), 2);
        assert!(
            matches!(&downgraded[0], ResponseInputItem::Message { role, .. } if role == "developer")
        );
        assert!(
            matches!(&downgraded[1], ResponseInputItem::Message { role, .. } if role == "user")
        );
    }

    #[test]
    fn previously_omitted_context_becomes_deliverable_after_shrink() {
        let values = (0..1024)
            .map(|index| {
                (
                    format!("source-{index:04}"),
                    AdditionalContextEntry {
                        value: "unchanged".to_string(),
                        kind: AdditionalContextKind::Untrusted,
                    },
                )
            })
            .collect::<IndexMap<_, _>>();
        let retained = IndexMap::from([("source-0500".to_string(), values["source-0500"].clone())]);
        let mut store = AdditionalContextStore::default();
        assert_eq!(store.merge(values.clone()).len(), 256);
        assert!(store.merge(values).is_empty());
        let fragments = store.merge(retained.clone());
        assert!(
            fragments
                .iter()
                .any(|item| input_text(item).contains("source-0500"))
        );
        assert_eq!(store.delivered, retained);
        assert!(store.merge(retained).is_empty());
    }

    #[test]
    fn deletion_and_downgrade_do_not_replay_unchanged_sources() {
        let mut store = AdditionalContextStore::default();
        let entry = AdditionalContextEntry {
            value: "still current".to_string(),
            kind: AdditionalContextKind::Application,
        };
        let mut values = IndexMap::from([
            ("unchanged".to_string(), entry.clone()),
            ("deleted".to_string(), entry.clone()),
            ("downgraded".to_string(), entry),
        ]);
        store.merge(values.clone());
        values.shift_remove("deleted");
        values.get_mut("downgraded").unwrap().kind = AdditionalContextKind::Untrusted;
        let fragments = store.merge(values.clone());
        assert_eq!(fragments.len(), 3);
        for (item, source) in fragments[..2].iter().zip(["deleted", "downgraded"]) {
            assert!(matches!(item, ResponseInputItem::Message { role, .. } if role == "developer"));
            assert!(input_text(item).contains(source));
            assert!(input_text(item).contains("previous_value_obsolete"));
        }
        assert!(fragments.iter().all(|item| !input_text(item).contains("unchanged")
            && !input_text(item).contains(ADDITIONAL_CONTEXT_RESET_SOURCE)));
        assert_eq!(store.delivered, values);
        assert!(store.merge(values).is_empty());
    }

    #[test]
    fn replacement_redelivers_only_missing_context() {
        let mut store = AdditionalContextStore::default();
        let values = IndexMap::from_iter(["retained", "missing"].map(|source| (
            source.to_string(),
            AdditionalContextEntry {
                value: source.to_string(),
                kind: AdditionalContextKind::Application,
            },
        )));
        let first = store.merge(values.clone());
        store.reconcile_history(&[ResponseItem::from(first[0].clone())]);
        assert_eq!(store.merge(values.clone()), vec![first[1].clone()]);
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
