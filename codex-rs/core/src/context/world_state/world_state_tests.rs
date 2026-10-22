use super::*;
use pretty_assertions::assert_eq;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

#[derive(Clone, Deserialize, Serialize)]
struct TestSection {
    value: String,
    optional: Option<String>,
    array: Vec<Value>,
}

impl WorldStateSection for TestSection {
    const ID: &'static str = "test";
    type Snapshot = Self;

    fn snapshot(&self) -> Self::Snapshot {
        self.clone()
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        match previous {
            PreviousSectionState::Known(previous) if self.value != previous.value => {
                Some(Box::new(TestFragment(self.value.clone())))
            }
            PreviousSectionState::Unknown => Some(Box::new(TestFragment("unknown".to_string()))),
            PreviousSectionState::Absent | PreviousSectionState::Known(_) => None,
        }
    }
}

struct TestFragment(String);

impl ContextualUserFragment for TestFragment {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.0)
    }
}

struct DuplicateTestSection;

impl WorldStateSection for DuplicateTestSection {
    const ID: &'static str = "test";
    type Snapshot = ();

    fn snapshot(&self) -> Self::Snapshot {}

    fn render_diff(
        &self,
        _previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        None
    }
}

#[test]
fn snapshot_merge_patch_preserves_replacements_and_rejects_invalid_roots() {
    let mut previous: WorldStateSnapshot = serde_json::from_value(json!({
        "keep": {"deep": {"value": "unchanged"}},
        "replace": [1, null],
        "remove": {"old": true}
    }))
    .unwrap();
    let current: WorldStateSnapshot = serde_json::from_value(json!({
        "keep": {"deep": {"value": "unchanged"}},
        "replace": {},
        "new": {"nested": [null, 3]}
    }))
    .unwrap();
    let patch = current.merge_patch_from(&previous).unwrap();
    assert_eq!(
        patch,
        json!({"replace": {}, "remove": null, "new": {"nested": [null, 3]}})
    );
    previous.apply_merge_patch(&patch).unwrap();
    assert_eq!(previous, current);
    assert_eq!(previous.merge_patch_from(&current), None);
    for invalid in [Value::Null, json!(3), json!([]), json!("invalid")] {
        assert!(previous.apply_merge_patch(&invalid).is_err());
        assert_eq!(previous, current);
    }
    previous
        .apply_merge_patch(&json!({"replace": {"nested": {"absent": null}}}))
        .unwrap();
    assert_eq!(previous.section("replace"), Some(&json!({"nested": {}})));
}

#[test]
fn snapshot_uses_stable_section_ids_and_omits_null_fields() {
    let mut world_state = WorldState::default();
    world_state.add_section(TestSection {
        value: "current".to_string(),
        optional: None,
        array: vec![json!({"value": null})],
    });

    assert_eq!(
        serde_json::to_value(world_state.snapshot()).expect("serialize world-state snapshot"),
        json!({"test": {"value": "current", "array": [{"value": null}]}})
    );
}

#[test]
fn render_diff_restores_the_typed_section_snapshot() {
    let mut previous = WorldState::default();
    previous.add_section(TestSection {
        value: "before".to_string(),
        optional: None,
        array: Vec::new(),
    });
    let mut current = WorldState::default();
    current.add_section(TestSection {
        value: "after".to_string(),
        optional: None,
        array: Vec::new(),
    });

    let rendered = current.render_diff(&previous.snapshot());

    assert_eq!(
        vec!["after"],
        rendered
            .into_iter()
            .map(|fragment| fragment.body().into_owned())
            .collect::<Vec<_>>()
    );
}

#[test]
fn extension_owned_section_uses_its_snapshot_and_renderer() {
    let mut world_state = WorldState::default();
    world_state.add_extension_section(WorldStateSectionContribution::new(
        "extension_test",
        json!({"value": "after", "optional": null}),
        |previous| match previous {
            PreviousWorldStateSection::Known(previous)
                if previous == &json!({"value": "before"}) =>
            {
                Some(RenderedWorldStateFragment::new(
                    "developer",
                    ("<extension_test>", "</extension_test>"),
                    "after",
                ))
            }
            PreviousWorldStateSection::Absent
            | PreviousWorldStateSection::Unknown
            | PreviousWorldStateSection::Known(_) => None,
        },
    ));
    let previous = WorldStateSnapshot {
        sections: BTreeMap::from([("extension_test".to_string(), json!({"value": "before"}))]),
    };

    let rendered = world_state.render_diff(&previous);

    assert_eq!(
        serde_json::to_value(world_state.snapshot()).expect("serialize world-state snapshot"),
        json!({"extension_test": {"value": "after"}})
    );
    assert_eq!(rendered.len(), 1);
    assert_eq!(rendered[0].role(), "developer");
    assert_eq!(
        rendered[0].render(),
        "<extension_test>after</extension_test>"
    );
}

#[test]
fn rendered_sections_share_one_hard_budget() {
    let mut world_state = WorldState::default();
    for (index, (id, body)) in [("large_0", 'a'), ("large_1", 'b')].into_iter().enumerate() {
        world_state.add_extension_section(WorldStateSectionContribution::new(
            id,
            json!({"value": index}),
            move |previous| match previous {
                PreviousWorldStateSection::Absent => Some(RenderedWorldStateFragment::new(
                    "developer",
                    ("", ""),
                    body.to_string().repeat(30_000),
                )),
                _ => None,
            },
        ));
    }

    let (rendered, first_snapshot) = world_state.render_full_with_snapshot();
    let rendered = rendered
        .into_iter()
        .map(|fragment| fragment.render())
        .collect::<Vec<_>>();
    assert_eq!(rendered, vec!["a".repeat(30_000)]);
    assert_eq!(
        first_snapshot.clone().into_value(),
        json!({"large_0": {"value": 0}, "_codex_extension_delivery": {
            "large_0": {"role": "developer", "text": "a".repeat(30_000)}
        }})
    );

    let (rendered, second_snapshot) = world_state.render_diff_with_snapshot(&first_snapshot);
    let rendered = rendered
        .into_iter()
        .map(|fragment| fragment.render())
        .collect::<Vec<_>>();
    assert_eq!(rendered, vec!["b".repeat(30_000)]);
    assert_eq!(
        second_snapshot.into_value(),
        json!({
            "large_0": {"value": 0}, "large_1": {"value": 1},
            "_codex_extension_delivery": {
                "large_0": {"role": "developer", "text": "a".repeat(30_000)},
                "large_1": {"role": "developer", "text": "b".repeat(30_000)}
            }
        })
    );
}

#[test]
fn rejected_update_preserves_the_previously_delivered_value() {
    let state = |value: String| {
        let mut state = WorldState::default();
        state.add_extension_section(WorldStateSectionContribution::new(
            "updated",
            json!({"value": value}),
            move |_| {
                Some(RenderedWorldStateFragment::new(
                    "developer",
                    ("", ""),
                    value.clone(),
                ))
            },
        ));
        state
    };
    let (first, accepted) = state("A".to_string()).render_full_with_snapshot();
    assert_eq!(
        first
            .iter()
            .map(|fragment| fragment.render())
            .collect::<Vec<_>>(),
        vec!["A"]
    );
    let (rejected, next) = state("B".repeat(50_000)).render_diff_with_snapshot(&accepted);
    assert!(rejected.is_empty());
    assert_eq!(next, accepted);
    assert_eq!(
        next.into_value(),
        json!({"updated": {"value": "A"},
            "_codex_extension_delivery": {"updated": {"role": "developer", "text": "A"}}
        })
    );
}

#[test]
fn missing_retained_fragment_is_rendered_again() {
    let mut world_state = WorldState::default();
    world_state.add_extension_section(
        WorldStateSectionContribution::new(
            "extension_test",
            json!({"body": "current catalog"}),
            |previous| match previous {
                PreviousWorldStateSection::Absent => Some(RenderedWorldStateFragment::new(
                    "developer",
                    ("<extension_test>", "</extension_test>"),
                    "current catalog",
                )),
                PreviousWorldStateSection::Unknown | PreviousWorldStateSection::Known(_) => None,
            },
        )
        .with_retained_fragment_matcher(|role, text| {
            role == "developer" && text.contains("current catalog")
        }),
    );
    let previous = world_state.snapshot();
    let retained = ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: "<extension_test>current catalog</extension_test>".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };

    assert_eq!(
        world_state
            .render_history_diff(Some(&previous), &[])
            .into_iter()
            .map(|fragment| fragment.body().into_owned())
            .collect::<Vec<_>>(),
        vec!["<extension_test>current catalog</extension_test>"]
    );
    assert!(
        world_state
            .render_history_diff(Some(&previous), &[retained])
            .is_empty()
    );
}

#[test]
fn unreadable_section_snapshot_is_treated_as_unknown() {
    let mut current = WorldState::default();
    current.add_section(TestSection {
        value: "current".to_string(),
        optional: None,
        array: Vec::new(),
    });
    let previous = WorldStateSnapshot {
        sections: BTreeMap::from([("test".to_string(), json!({"invalid": true}))]),
    };

    let rendered = current.render_diff(&previous);

    assert_eq!(
        vec!["unknown"],
        rendered
            .into_iter()
            .map(|fragment| fragment.body().into_owned())
            .collect::<Vec<_>>()
    );
}

#[test]
#[should_panic(expected = "duplicate world-state section ID: test")]
fn duplicate_section_ids_are_rejected() {
    let mut world_state = WorldState::default();
    world_state.add_section(TestSection {
        value: "current".to_string(),
        optional: None,
        array: Vec::new(),
    });

    world_state.add_section(DuplicateTestSection);
}

#[test]
fn snapshot_merge_patch_changes_and_removes_nested_values() {
    let mut previous = WorldStateSnapshot {
        sections: BTreeMap::from([
            (
                "kept".to_string(),
                json!({"same": true, "changed": "before", "removed": true}),
            ),
            ("removed_section".to_string(), json!({"value": true})),
        ]),
    };
    let current = WorldStateSnapshot {
        sections: BTreeMap::from([
            (
                "kept".to_string(),
                json!({"same": true, "changed": "after"}),
            ),
            ("added_section".to_string(), json!({"items": [1, 2]})),
        ]),
    };

    assert_eq!(
        current.merge_patch_from(&previous),
        Some(json!({
            "kept": {"changed": "after", "removed": null},
            "removed_section": null,
            "added_section": {"items": [1, 2]},
        }))
    );
    previous
        .apply_merge_patch(
            &current
                .merge_patch_from(&previous)
                .expect("changed snapshots should produce a patch"),
        )
        .expect("apply world-state merge patch");
    assert_eq!(previous, current);
    assert_eq!(current.merge_patch_from(&current), None);
}

struct PressureSection(String);

impl WorldStateSection for PressureSection {
    const ID: &'static str = "pressure";
    type Snapshot = String;

    fn snapshot(&self) -> String {
        self.0.clone()
    }
    fn required() -> bool {
        true
    }
    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, String>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        if matches!(previous, PreviousSectionState::Known(old) if old == &self.0) {
            None
        } else {
            Some(Box::new(TestFragment(self.0.clone())))
        }
    }
}

#[test]
fn optional_replacement_invalidates_under_pressure_without_forging_delivery() {
    let world = |body: String, pressure: &str| {
        let mut world = WorldState::default();
        world.add_section(PressureSection(pressure.into()));
        let current = body.clone();
        world.add_extension_section(
            WorldStateSectionContribution::new("catalog", json!({"body": body}), move |previous| {
                if matches!(previous, PreviousWorldStateSection::Known(old)
                    if old.get("body").and_then(Value::as_str) == Some(body.as_str()))
                {
                    return None;
                }
                Some(RenderedWorldStateFragment::new(
                    "developer",
                    ("<catalog>", "</catalog>"),
                    body.clone(),
                ))
            })
            .with_retained_fragment_matcher(move |role, text| {
                role == "developer"
                    && (text.contains(&current) || text.contains("catalog deferred"))
            })
            .with_budget_fallback(
                json!({"deferred": true}),
                RenderedWorldStateFragment::new(
                    "developer",
                    ("<catalog>", "</catalog>"),
                    "catalog deferred",
                ),
            ),
        );
        world
    };
    let (initial, accepted) = world("old".into(), "small").render_full_with_snapshot();
    let history = crate::context_manager::updates::merge_contextual_fragments(initial);
    let required = "R".repeat(41_000);
    let desired = world("new".into(), &required);
    let (changed, fallback) = desired.render_history_diff_with_snapshot(Some(&accepted), &history);
    let texts = changed
        .iter()
        .map(|fragment| fragment.render())
        .collect::<Vec<_>>();
    assert!(texts.contains(&required));
    assert!(texts.contains(&"<catalog>catalog deferred</catalog>".to_string()));
    assert!(
        !texts
            .iter()
            .any(|text| text.contains("<catalog>new</catalog>"))
    );
    assert_eq!(
        fallback.section("catalog"),
        Some(&json!({"deferred": true}))
    );
    let (retried, complete) = desired.render_diff_with_snapshot(&fallback);
    assert_eq!(
        retried
            .iter()
            .map(|fragment| fragment.render())
            .collect::<Vec<_>>(),
        vec!["<catalog>new</catalog>"]
    );
    assert_eq!(complete.section("catalog"), Some(&json!({"body": "new"})));
    assert!(desired.render_diff(&complete).is_empty());

    // First-time optional inventory is not promoted to required context.
    let (initial, _) = world("new".into(), &required).render_full_with_snapshot();
    assert_eq!(initial.len(), 1);
    // A replacement too large even without pressure must not repeat the invalidation.
    let oversized = world("N".repeat(50_000), &required);
    let (_, pending) = oversized.render_diff_with_snapshot(&accepted);
    assert!(oversized.render_diff(&pending).is_empty());
    assert_eq!(pending.section("catalog"), Some(&json!({"deferred": true})));
}

#[test]
fn subagent_removal_survives_required_instruction_pressure() {
    let mut initial = WorldState::default();
    initial.add_section(SubagentsState::new("worker running".into()));
    let (_, previous) = initial.render_full_with_snapshot();
    let mut current = WorldState::default();
    current.add_section(PressureSection("R".repeat(41_000)));
    current.add_section(SubagentsState::new(String::new()));
    let (fragments, accepted) = current.render_diff_with_snapshot(&previous);
    assert!(
        fragments
            .iter()
            .any(|fragment| fragment.render().contains("\nnone\n"))
    );
    assert_eq!(accepted.section("subagents").unwrap()["text"], "");
}
