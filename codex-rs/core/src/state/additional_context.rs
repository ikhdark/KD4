use crate::context::AdditionalContextDeveloperFragment;
use crate::context::AdditionalContextUserFragment;
use crate::context::ContextualUserFragment;
use codex_context_fragments::ModelContextBudget;
use codex_context_fragments::RenderedContextFragment;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::AdditionalContextKind;
use indexmap::IndexMap;

const ADDITIONAL_CONTEXT_AGGREGATE_TOKEN_BUDGET: usize = 40_000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AdditionalContextStore {
    values: IndexMap<String, AdditionalContextEntry>,
}

impl AdditionalContextStore {
    pub(crate) fn merge(
        &mut self,
        values: IndexMap<String, AdditionalContextEntry>,
    ) -> Vec<ResponseInputItem> {
        let mut budget = ModelContextBudget::new(ADDITIONAL_CONTEXT_AGGREGATE_TOKEN_BUDGET);
        let mut fragments = Vec::new();
        for (key, entry) in &values {
            if self.values.get(key) == Some(entry) {
                continue;
            }

            let (role, rendered) = match entry.kind {
                AdditionalContextKind::Untrusted => {
                    let fragment =
                        AdditionalContextUserFragment::new(key.clone(), entry.value.clone());
                    (fragment.role(), fragment.render())
                }
                AdditionalContextKind::Application => {
                    let fragment =
                        AdditionalContextDeveloperFragment::new(key.clone(), entry.value.clone());
                    (fragment.role(), fragment.render())
                }
            };
            if budget.try_take(&rendered) {
                fragments
                    .push(RenderedContextFragment::new(role, rendered).into_response_input_item());
            } else {
                let omission_role = if matches!(
                    self.values.get(key),
                    Some(previous) if previous.kind == AdditionalContextKind::Application
                ) {
                    "developer"
                } else {
                    role
                };
                let omission = format!(
                    r#"<additional_context_omitted source={key:?} reason="aggregate budget exceeded" previous_value_obsolete="true" />"#
                );
                fragments.push(
                    RenderedContextFragment::new(omission_role, omission)
                        .into_response_input_item(),
                );
            }
        }
        self.values = values;
        fragments
    }
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
        store.merge(IndexMap::from([(
            "target".to_string(),
            AdditionalContextEntry {
                value: "old".to_string(),
                kind: AdditionalContextKind::Application,
            },
        )]));

        let mut values = (0..12)
            .map(|index| {
                (
                    format!("source-{index:02}"),
                    AdditionalContextEntry {
                        value: "a".repeat(20_000),
                        kind: AdditionalContextKind::Application,
                    },
                )
            })
            .collect::<IndexMap<_, _>>();
        values.insert(
            "target".to_string(),
            AdditionalContextEntry {
                value: "new".to_string(),
                kind: AdditionalContextKind::Untrusted,
            },
        );

        let fragments = store.merge(values.clone());

        assert!(
            fragments
                .iter()
                .any(|item| input_text(item).contains("previous_value_obsolete=\"true\""))
        );
        assert!(fragments.iter().any(|item| matches!(
            item,
            ResponseInputItem::Message { role, content, .. }
                if role == "developer"
                    && content.iter().any(|content| matches!(
                        content,
                        ContentItem::InputText { text }
                            if text.contains("previous_value_obsolete=\"true\"")
                    ))
        )));
        assert_eq!(store.values.get("target"), values.get("target"));
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
