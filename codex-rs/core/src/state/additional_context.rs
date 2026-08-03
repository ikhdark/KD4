use std::collections::BTreeMap;

use crate::context::AdditionalContextDeveloperFragment;
use crate::context::AdditionalContextUserFragment;
use crate::context::ContextualUserFragment;
use codex_context_fragments::ModelContextBudget;
use codex_context_fragments::RenderedContextFragment;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::AdditionalContextKind;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AdditionalContextStore {
    values: BTreeMap<String, AdditionalContextEntry>,
}

impl AdditionalContextStore {
    pub(crate) fn merge(
        &mut self,
        values: BTreeMap<String, AdditionalContextEntry>,
    ) -> Vec<ResponseInputItem> {
        let mut budget = ModelContextBudget::default();
        let mut next_values = BTreeMap::new();
        let mut fragments = Vec::new();
        for (key, entry) in values {
            if self.values.get(&key) == Some(&entry) {
                next_values.insert(key, entry);
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
                next_values.insert(key, entry);
                fragments
                    .push(RenderedContextFragment::new(role, rendered).into_response_input_item());
            } else if let Some(previous) = self.values.get(&key) {
                next_values.insert(key, previous.clone());
            }
        }
        self.values = next_values;
        fragments
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::ContentItem;

    #[test]
    fn aggregate_budget_retries_an_omitted_entry_on_the_next_merge() {
        // Individual additional-context values are capped before the aggregate
        // budget is applied, so use enough maximum-sized entries to overflow the
        // shared budget without relying on pre-render input length.
        let values = (0..10)
            .map(|index| {
                (
                    format!("source-{index:02}"),
                    AdditionalContextEntry {
                        value: "a".repeat(4_000),
                        kind: AdditionalContextKind::Application,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let value_count = values.len();
        let mut store = AdditionalContextStore::default();

        let first = store.merge(values.clone());
        let second = store.merge(values.clone());
        let third = store.merge(values);

        assert!(!first.is_empty());
        assert!(first.len() < value_count);
        assert!(input_text(&first[0]).contains("source-00"));
        assert_eq!(first.len() + second.len(), value_count);
        assert!(
            second
                .iter()
                .any(|item| input_text(item).contains("source-09"))
        );
        assert!(third.is_empty());
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
