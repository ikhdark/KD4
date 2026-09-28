use super::*;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_model_provider::create_model_provider;
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::manager::RefreshStrategy;

#[tokio::test]
async fn bedrock_model_and_reasoning_pickers() {
    for (provider_info, default_model, astra_model, prefix) in [
        (
            ModelProviderInfo::create_amazon_bedrock_provider(None),
            "openai.gpt-6-sol",
            "openai.gpt-6-astra",
            "",
        ),
        (
            ModelProviderInfo::create_amazon_bedrock_runtime_provider(None),
            "global.openai.gpt-6-sol",
            "global.openai.gpt-6-astra",
            "global.",
        ),
    ] {
        let presets = create_model_provider(provider_info, None)
            .models_manager("bedrock-picker-test", std::env::temp_dir(), None)
            .list_models(
                RefreshStrategy::Offline,
                HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
            )
            .await
            .unwrap();
        let astra = presets
            .iter()
            .find(|model| model.model == astra_model)
            .unwrap()
            .clone();
        let (mut chat, _events, _ops) = make_chatwidget_manual(Some(default_model)).await;
        chat.thread_id = Some(ThreadId::new());
        chat.model_catalog = Arc::new(ModelCatalog::new(presets));
        chat.open_model_popup();
        let rendered = render_bottom_popup(&chat, 100);
        assert!(
            rendered.contains(&format!("{default_model} (current)")),
            "{rendered}"
        );
        let mut previous = 0;
        for name in [
            "gpt-6-sol",
            "gpt-6-astra",
            "gpt-6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
        ] {
            let position = rendered
                .find(&format!("{prefix}openai.{name}"))
                .expect("visible Bedrock model");
            assert!(position > previous, "{rendered}");
            previous = position;
        }
        chat.handle_key_event(KeyEvent::from(KeyCode::Esc));
        chat.open_reasoning_popup(astra);
        let rendered = render_bottom_popup(&chat, 100);
        assert!(rendered.contains("Maximum"), "{rendered}");
        assert!(!rendered.contains("Ultra"), "{rendered}");
    }
}
