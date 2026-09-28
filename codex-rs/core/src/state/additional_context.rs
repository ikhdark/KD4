use crate::context::AdditionalContextDeveloperFragment;
use crate::context::AdditionalContextUserFragment;
use crate::context::ContextualUserFragment;
use crate::tools::command_output_artifact::create_canonical_output_artifact;
use codex_context_fragments::additional_context_value_is_truncated;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::AdditionalContextKind;
use codex_tools::CanonicalToolResult;
use futures::StreamExt;
use indexmap::IndexMap;
use std::path::Path;

const ADDITIONAL_CONTEXT_AGGREGATE_BYTE_BUDGET: usize = 160_000;
const ADDITIONAL_CONTEXT_MAX_ITEMS: usize = 256;
const ADDITIONAL_CONTEXT_RESET_SOURCE: &str = "__codex_additional_context_reset__";
const ADDITIONAL_CONTEXT_RESET: &str = "Additional context snapshot replaced. All previously supplied additional context values are obsolete (previous_value_obsolete=\"true\"). Only additional-context entries following this reset in the current update remain available. Do not infer omitted values from earlier messages.";
const RECOVERY_UNAVAILABLE: &str = "Original context recovery is unavailable because its snapshot could not be retained. This excerpt is incomplete; request missing information from the source.";

fn recovery_notice(artifact_id: &str) -> String {
    format!(
        "Original context retained with source and trust kind. Recover missing content with read_tool_output: {{\"artifact_id\":\"{artifact_id}\",\"selectors\":[{{\"kind\":\"json_pointer\",\"pointer\":\"/value\"}}]}}. Use bounded search or ranges for large originals; do not reread unchanged excerpts."
    )
}

#[derive(Default)]
pub(crate) struct AdditionalContextUpdate {
    items: Vec<ResponseInputItem>,
    originals: Vec<OriginalContext>,
}

struct OriginalContext {
    index: usize,
    key: String,
    entry: AdditionalContextEntry,
    failure_item: ResponseInputItem,
}

impl AdditionalContextUpdate {
    /// Persist only the originals selected by the pure admission pass. Recovery
    /// failures keep the bounded excerpt, but never advertise an unusable handle.
    pub(crate) async fn retain_originals(
        mut self,
        codex_home: &Path,
        thread_id: &str,
    ) -> (Vec<ResponseInputItem>, Vec<(String, u64, String)>) {
        let mut artifacts = Vec::new();
        let mut pending = futures::stream::iter(self.originals)
            .map(|original| async move {
                let kind = match original.entry.kind {
                    AdditionalContextKind::Untrusted => "untrusted",
                    AdditionalContextKind::Application => "application",
                };
                let canonical = CanonicalToolResult::json(serde_json::json!({
                    "source": original.key,
                    "kind": kind,
                    "value": original.entry.value,
                }));
                let artifact = create_canonical_output_artifact(codex_home, thread_id, &canonical).await;
                let id = artifact.complete.then(|| artifact.artifact_id()).flatten();
                if let Some(id) = id {
                    let item = render_entry(&original.key, &original.entry, Some(&recovery_notice(&id)));
                    (original.index, item, Some((id, canonical.exact_bytes, canonical.sha256)))
                } else {
                    tracing::warn!(error = ?artifact.error, "failed to retain original additional context");
                    (original.index, original.failure_item, None)
                }
            })
            .buffered(4);
        while let Some((index, item, artifact)) = pending.next().await {
            self.items[index] = item;
            artifacts.extend(artifact);
        }
        (self.items, artifacts)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AdditionalContextStore {
    values: IndexMap<String, AdditionalContextEntry>,
    delivered: IndexMap<String, AdditionalContextEntry>,
}

impl AdditionalContextStore {
    pub(crate) fn prepare_merge(
        &mut self,
        values: IndexMap<String, AdditionalContextEntry>,
    ) -> AdditionalContextUpdate {
        if self.values == values {
            return AdditionalContextUpdate::default();
        }
        let mut update = AdditionalContextUpdate::default();
        // Include the JSON array brackets and each message's serialized envelope.
        let mut retained_bytes = 2;
        let mut overflowed = self
            .delivered
            .iter()
            .any(|(key, old)| values.get(key).is_none_or(|new| new.kind != old.kind));
        let mut delivered = self.delivered.clone();
        for (key, entry) in &values {
            if overflowed {
                break;
            }
            if self.delivered.get(key) == Some(entry) {
                continue;
            }
            if !push_bounded_item(&mut update, &mut retained_bytes, key, entry) {
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
                None,
            );
            update.items.clear();
            update.originals.clear();
            delivered.clear();
            retained_bytes = 2 + serialized_item_bytes(&reset) + 1;
            update.items.push(reset);
            for (key, entry) in &values {
                if update.items.len() == ADDITIONAL_CONTEXT_MAX_ITEMS {
                    break;
                }
                // An oversized entry does not consume the remaining budget: a
                // later, smaller entry may still fit the replacement snapshot.
                if push_bounded_item(&mut update, &mut retained_bytes, key, entry) {
                    delivered.insert(key.clone(), entry.clone());
                }
            }
        }

        // The supplied map remains authoritative even for omitted entries.
        // An unchanged remerge must not restore or repeatedly emit older values.
        self.values = values;
        self.delivered = delivered;
        update
    }

    #[cfg(test)]
    fn merge(
        &mut self,
        values: IndexMap<String, AdditionalContextEntry>,
    ) -> Vec<ResponseInputItem> {
        self.prepare_merge(values).items
    }
}

fn render_entry(
    key: &str,
    entry: &AdditionalContextEntry,
    notice: Option<&str>,
) -> ResponseInputItem {
    match entry.kind {
        AdditionalContextKind::Untrusted => {
            let mut fragment =
                AdditionalContextUserFragment::new(key.to_string(), entry.value.clone());
            if let Some(notice) = notice {
                fragment = fragment.with_recovery_notice(notice.to_string());
            }
            fragment.into_response_input_item()
        }
        AdditionalContextKind::Application => {
            let mut fragment =
                AdditionalContextDeveloperFragment::new(key.to_string(), entry.value.clone());
            if let Some(notice) = notice {
                fragment = fragment.with_recovery_notice(notice.to_string());
            }
            fragment.into_response_input_item()
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
    update: &mut AdditionalContextUpdate,
    retained_bytes: &mut usize,
    key: &str,
    entry: &AdditionalContextEntry,
) -> bool {
    let needs_recovery = additional_context_value_is_truncated(&entry.value);
    // UUIDs have a fixed serialized size. Admit the larger success/failure
    // envelope before any I/O, including replay after an aggregate reset.
    let placeholder =
        needs_recovery.then(|| recovery_notice("00000000-0000-0000-0000-000000000000"));
    let item = render_entry(key, entry, placeholder.as_deref());
    let failure_item = needs_recovery.then(|| render_entry(key, entry, Some(RECOVERY_UNAVAILABLE)));
    let item_bytes = serialized_item_bytes(&item).max(
        failure_item
            .as_ref()
            .map(serialized_item_bytes)
            .unwrap_or_default(),
    ) + 1;
    if update.items.len() == ADDITIONAL_CONTEXT_MAX_ITEMS
        || item_bytes > ADDITIONAL_CONTEXT_AGGREGATE_BYTE_BUDGET.saturating_sub(*retained_bytes)
    {
        return false;
    }
    *retained_bytes += item_bytes;
    if let Some(failure_item) = failure_item {
        update.originals.push(OriginalContext {
            index: update.items.len(),
            key: key.to_string(),
            entry: entry.clone(),
            failure_item,
        });
    }
    update.items.push(item);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::ContentItem;

    #[tokio::test]
    async fn originals_are_recoverable_immutable_and_thread_scoped() {
        use crate::tools::command_output_artifact::ToolOutputSelector;
        use crate::tools::command_output_artifact::read_tool_output_selectors;
        use crate::tools::command_output_artifact::remint_tool_history_artifact_for_thread;

        let home = tempfile::tempdir().unwrap();
        for (index, kind) in [
            AdditionalContextKind::Untrusted,
            AdditionalContextKind::Application,
        ]
        .into_iter()
        .enumerate()
        {
            let source = format!("source-{index}<&\"");
            let old = format!(
                "{}MIDDLE_REQUIREMENT=old{}",
                "é<&>".repeat(500),
                "😀tail".repeat(500)
            );
            let new = old.replace("=old", "=new");
            let values = |value: String| {
                IndexMap::from([(source.clone(), AdditionalContextEntry { value, kind })])
            };
            let mut store = AdditionalContextStore::default();
            let (initial, artifacts) = store
                .prepare_merge(values(old.clone()))
                .retain_originals(home.path(), "parent")
                .await;
            let [(id, bytes, sha)] = artifacts.as_slice() else {
                panic!("one retained original")
            };
            assert_eq!(initial.len(), 1);
            assert!(input_text(&initial[0]).contains(id));
            assert!(!input_text(&initial[0]).contains("MIDDLE_REQUIREMENT"));
            match kind {
                AdditionalContextKind::Untrusted => assert!(
                    AdditionalContextUserFragment::matches_text(input_text(&initial[0]))
                ),
                AdditionalContextKind::Application => assert!(
                    AdditionalContextDeveloperFragment::matches_text(input_text(&initial[0]))
                ),
            }
            let (unchanged, no_artifacts) = store
                .prepare_merge(values(old.clone()))
                .retain_originals(home.path(), "parent")
                .await;
            assert!(unchanged.is_empty());
            assert!(no_artifacts.is_empty());
            let (changed, next_artifacts) = store
                .prepare_merge(values(new.clone()))
                .retain_originals(home.path(), "parent")
                .await;
            assert_eq!(next_artifacts.len(), 1);
            assert_ne!(id, &next_artifacts[0].0);
            assert!(input_text(&changed[0]).contains(&next_artifacts[0].0));
            let selectors = || {
                vec![ToolOutputSelector::JsonPointer {
                    pointer: String::new(),
                }]
            };
            for (artifact_id, value) in [(id, &old), (&next_artifacts[0].0, &new)] {
                let recovered =
                    read_tool_output_selectors(home.path(), "parent", artifact_id, selectors())
                        .await
                        .unwrap();
                assert!(recovered.complete);
                assert_eq!(
                    recovered.results[0].value,
                    Some(serde_json::json!({
                        "source": source, "kind": if kind == AdditionalContextKind::Untrusted { "untrusted" } else { "application" }, "value": value,
                    }))
                );
            }
            assert!(
                read_tool_output_selectors(home.path(), "unrelated", id, selectors())
                    .await
                    .is_err()
            );
            let copied = remint_tool_history_artifact_for_thread(
                home.path(),
                "parent",
                "fork",
                id,
                *bytes,
                sha,
            )
            .await
            .unwrap();
            assert_eq!(&copied, id);
            let recovered = read_tool_output_selectors(home.path(), "fork", id, selectors())
                .await
                .unwrap();
            assert_eq!(recovered.results[0].value.as_ref().unwrap()["value"], old);
        }
    }

    #[tokio::test]
    async fn retention_failure_is_explicit_and_small_values_do_not_touch_storage() {
        let home = tempfile::tempdir().unwrap();
        let blocked = home.path().join("not-a-directory");
        std::fs::write(&blocked, "unchanged file").unwrap();
        let mut store = AdditionalContextStore::default();
        let (small, artifacts) = store
            .prepare_merge(IndexMap::from([(
                "source".to_string(),
                AdditionalContextEntry {
                    value: "small".to_string(),
                    kind: AdditionalContextKind::Untrusted,
                },
            )]))
            .retain_originals(&blocked, "thread")
            .await;
        assert_eq!(
            input_text(&small[0]),
            "<external_context source=\"source\" kind=\"untrusted\">\nsmall\n</external_context>"
        );
        assert!(artifacts.is_empty());
        let (large, artifacts) = store
            .prepare_merge(IndexMap::from([(
                "source".to_string(),
                AdditionalContextEntry {
                    value: "&".repeat(5_000),
                    kind: AdditionalContextKind::Untrusted,
                },
            )]))
            .retain_originals(&blocked, "thread")
            .await;
        assert!(artifacts.is_empty());
        assert!(input_text(&large[0]).contains(RECOVERY_UNAVAILABLE));
        assert!(!input_text(&large[0]).contains("artifact_id"));
        assert!(AdditionalContextUserFragment::matches_text(input_text(
            &large[0]
        )));
        assert_eq!(std::fs::read_to_string(&blocked).unwrap(), "unchanged file");
    }

    #[tokio::test]
    async fn aggregate_admission_retains_only_the_final_selected_originals() {
        let home = tempfile::tempdir().unwrap();
        let values = (0..64)
            .map(|index| {
                (
                    format!("source-{index}"),
                    AdditionalContextEntry {
                        value: "\"\\&".repeat(2_000),
                        kind: AdditionalContextKind::Application,
                    },
                )
            })
            .collect();
        let update = AdditionalContextStore::default().prepare_merge(values);
        let originals = update.originals.len();
        assert!(originals > 0 && originals < 64);
        assert_eq!(
            originals,
            update.items.len() - 1,
            "the reset does not need an artifact"
        );
        let (items, artifacts) = update.retain_originals(home.path(), "thread").await;
        assert_eq!(artifacts.len(), originals);
        assert!(
            serde_json::to_vec(&items).unwrap().len() <= ADDITIONAL_CONTEXT_AGGREGATE_BYTE_BUDGET
        );
        let metadata_count = std::fs::read_dir(home.path().join("tool-output/thread"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".meta.json"))
            .count();
        assert_eq!(metadata_count, originals);
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
