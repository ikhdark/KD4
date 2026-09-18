#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::sync::Mutex;

use codex_extension_api::ConfigContributor;
use codex_extension_api::ContextContributor;
use codex_extension_api::ContextualUserFragment;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionEventSink;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::PromptFragment;
use codex_extension_api::PromptSlot;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::TokenUsageContributor;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::TurnContextContributionInput;
use codex_extension_api::TurnInputContext;
use codex_extension_api::TurnInputContributor;
use codex_extension_api::TurnItemContributor;
use codex_extension_api::TurnLifecycleContributor;
use codex_protocol::items::HookPromptItem;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use pretty_assertions::assert_eq;

#[path = "../examples/enabled_extensions/shared_state_extension.rs"]
mod shared_state_extension;

#[tokio::test]
async fn example_context_estimates_preserve_text_without_recording_contributions() {
    let mut builder = ExtensionRegistryBuilder::<()>::new();
    shared_state_extension::install(&mut builder);
    let registry = builder.build();
    let session_store = ExtensionData::new("session");
    let thread_store = ExtensionData::new("thread");
    let expected = vec![
        PromptFragment::developer_policy("Prefer short answers unless the user asks for detail."),
        PromptFragment::developer_capability(
            "This extension can contribute more than one prompt fragment.",
        ),
    ];

    // Preview both before the first contribution and after the stores contain counters.
    for expected_count in 0..2 {
        let mut estimated = Vec::new();
        for contributor in registry.context_contributors() {
            estimated.extend(
                contributor
                    .estimate_thread_context(&session_store, &thread_store)
                    .await,
            );
        }
        assert_eq!(estimated, expected);
        for store in [&session_store, &thread_store] {
            assert_eq!(
                shared_state_extension::recorded_style_contributions(store),
                expected_count
            );
            assert_eq!(
                shared_state_extension::recorded_usage_contributions(store),
                expected_count
            );
        }

        let mut contributed = Vec::new();
        for contributor in registry.context_contributors() {
            contributed.extend(
                contributor
                    .contribute_thread_context(&session_store, &thread_store)
                    .await,
            );
        }
        assert_eq!(contributed, expected);
        for store in [&session_store, &thread_store] {
            assert_eq!(
                shared_state_extension::recorded_style_contributions(store),
                expected_count + 1
            );
            assert_eq!(
                shared_state_extension::recorded_usage_contributions(store),
                expected_count + 1
            );
        }
    }
}

struct AllContributors;

impl ContextContributor for AllContributors {
    fn contribute_thread_context<'a>(
        &'a self,
        _session_store: &'a ExtensionData,
        _thread_store: &'a ExtensionData,
    ) -> ExtensionFuture<'a, Vec<PromptFragment>> {
        Box::pin(std::future::ready(Vec::new()))
    }
}

impl ThreadLifecycleContributor<()> for AllContributors {}

impl TurnLifecycleContributor for AllContributors {}

impl ConfigContributor<()> for AllContributors {}

impl TokenUsageContributor for AllContributors {}

impl TurnInputContributor for AllContributors {
    fn contribute<'a>(
        &'a self,
        input: &'a TurnInputContext,
        _session_store: &'a ExtensionData,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
    ) -> ExtensionFuture<'a, Vec<Box<dyn ContextualUserFragment + Send>>> {
        Box::pin(async move {
            let _self = self;
            let _input = input;
            Vec::new()
        })
    }
}

impl ToolContributor for AllContributors {
    fn surface_revision(
        &self,
        _session_store: &ExtensionData,
        _thread_store: &ExtensionData,
    ) -> u64 {
        41
    }

    fn tools(
        &self,
        _session_store: &ExtensionData,
        _thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn ToolExecutor<ToolCall>>> {
        Vec::new()
    }
}

impl ToolLifecycleContributor for AllContributors {}

impl TurnItemContributor for AllContributors {
    fn contribute<'a>(
        &'a self,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
        _item: &'a mut TurnItem,
    ) -> ExtensionFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let _self = self;
            Ok(())
        })
    }
}

#[tokio::test]
async fn build_round_trips_every_contributor_category() {
    let contributor = Arc::new(AllContributors);
    let mut builder = ExtensionRegistryBuilder::<()>::new();
    builder.thread_lifecycle_contributor(contributor.clone());
    builder.turn_lifecycle_contributor(contributor.clone());
    builder.config_contributor(contributor.clone());
    builder.token_usage_contributor(contributor.clone());
    builder.prompt_contributor(contributor.clone());
    builder.turn_input_contributor(contributor.clone());
    builder.tool_contributor(contributor.clone());
    builder.tool_lifecycle_contributor(contributor.clone());
    builder.turn_item_contributor(contributor);
    let registry = builder.build();

    assert_eq!(registry.thread_lifecycle_contributors().len(), 1);
    assert_eq!(registry.turn_lifecycle_contributors().len(), 1);
    assert_eq!(registry.config_contributors().len(), 1);
    assert_eq!(registry.token_usage_contributors().len(), 1);
    assert_eq!(registry.context_contributors().len(), 1);
    assert_eq!(registry.turn_input_contributors().len(), 1);
    assert_eq!(registry.tool_contributors().len(), 1);
    assert_eq!(
        registry.tool_contributors()[0].surface_revision(
            &ExtensionData::new("session"),
            &ExtensionData::new("thread")
        ),
        41
    );
    assert_eq!(registry.tool_lifecycle_contributors().len(), 1);
    assert_eq!(registry.turn_item_contributors().len(), 1);
}

struct NamedContextContributor(&'static str);

impl ContextContributor for NamedContextContributor {
    fn contribute_thread_context<'a>(
        &'a self,
        _session_store: &'a ExtensionData,
        _thread_store: &'a ExtensionData,
    ) -> ExtensionFuture<'a, Vec<PromptFragment>> {
        Box::pin(std::future::ready(vec![PromptFragment::developer_policy(
            self.0,
        )]))
    }
}

struct NamedTurnContextContributor(&'static str);

impl ContextContributor for NamedTurnContextContributor {
    fn contribute_turn_context<'a>(
        &'a self,
        _input: TurnContextContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<PromptFragment>> {
        Box::pin(std::future::ready(vec![PromptFragment::new(
            PromptSlot::ContextualUser,
            self.0,
        )]))
    }
}

struct RecordingTurnItemContributor {
    name: &'static str,
    calls: Arc<Mutex<Vec<&'static str>>>,
}

impl TurnItemContributor for RecordingTurnItemContributor {
    fn contribute<'a>(
        &'a self,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
        _item: &'a mut TurnItem,
    ) -> ExtensionFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.calls
                .lock()
                .expect("turn item calls lock should not be poisoned")
                .push(self.name);
            Ok(())
        })
    }
}

#[tokio::test]
async fn contributors_preserve_registration_order() {
    let turn_item_calls = Arc::new(Mutex::new(Vec::new()));
    let mut builder = ExtensionRegistryBuilder::<()>::new();
    builder.prompt_contributor(Arc::new(NamedContextContributor("first")));
    builder.prompt_contributor(Arc::new(NamedContextContributor("second")));
    builder.prompt_contributor(Arc::new(NamedTurnContextContributor("turn-first")));
    builder.prompt_contributor(Arc::new(NamedTurnContextContributor("turn-second")));
    for name in ["first", "second"] {
        builder.turn_item_contributor(Arc::new(RecordingTurnItemContributor {
            name,
            calls: Arc::clone(&turn_item_calls),
        }));
    }
    let registry = builder.build();
    let session_store = ExtensionData::new("session");
    let thread_store = ExtensionData::new("thread");
    let turn_store = ExtensionData::new("turn");

    let mut fragments = Vec::new();
    for contributor in registry.context_contributors() {
        fragments.extend(
            contributor
                .contribute_thread_context(&session_store, &thread_store)
                .await,
        );
    }
    for contributor in registry.context_contributors() {
        fragments.extend(
            contributor
                .contribute_turn_context(TurnContextContributionInput {
                    thread_id: codex_protocol::ThreadId::default(),
                    turn_id: turn_store.level_id(),
                    session_store: &session_store,
                    thread_store: &thread_store,
                    turn_store: &turn_store,
                    model_context_window: Some(123),
                })
                .await,
        );
    }
    let mut item = TurnItem::HookPrompt(HookPromptItem {
        id: "item".to_string(),
        fragments: Vec::new(),
    });
    for contributor in registry.turn_item_contributors() {
        contributor
            .contribute(&thread_store, &turn_store, &mut item)
            .await
            .expect("turn item contribution should succeed");
    }

    assert_eq!(
        fragments,
        vec![
            PromptFragment::developer_policy("first"),
            PromptFragment::developer_policy("second"),
            PromptFragment::new(PromptSlot::ContextualUser, "turn-first"),
            PromptFragment::new(PromptSlot::ContextualUser, "turn-second"),
        ]
    );
    assert_eq!(
        turn_item_calls
            .lock()
            .expect("turn item calls lock")
            .as_slice(),
        ["first", "second"]
    );
}

#[derive(Default)]
struct RecordingEventSink {
    events: Mutex<Vec<(String, String)>>,
}

impl ExtensionEventSink for RecordingEventSink {
    fn emit(&self, event: Event) {
        let EventMsg::Warning(warning) = event.msg else {
            panic!("test sink only accepts warning events");
        };
        self.events
            .lock()
            .expect("recording event sink lock should not be poisoned")
            .push((event.id, warning.message));
    }
}

#[test]
fn custom_event_sink_survives_registry_build() {
    let sink = Arc::new(RecordingEventSink::default());
    let builder = ExtensionRegistryBuilder::<()>::with_event_sink(sink.clone());

    builder
        .event_sink()
        .emit(warning_event("builder", "before"));
    let registry = builder.build();
    registry
        .event_sink()
        .emit(warning_event("registry", "after"));
    registry.event_sink().emit_for_thread(
        codex_protocol::ThreadId::new(),
        warning_event("turn-correlation", "scoped"),
    );

    assert_eq!(
        sink.events
            .lock()
            .expect("recording event sink lock")
            .as_slice(),
        [
            ("builder".to_string(), "before".to_string()),
            ("registry".to_string(), "after".to_string()),
            ("turn-correlation".to_string(), "scoped".to_string()),
        ]
    );
}

fn warning_event(id: &str, message: &str) -> Event {
    Event {
        id: id.to_string(),
        msg: EventMsg::Warning(WarningEvent {
            message: message.to_string(),
        }),
    }
}
