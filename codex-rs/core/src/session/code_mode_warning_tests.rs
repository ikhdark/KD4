use super::unsupported_code_mode_warning;
use codex_features::Feature;
use codex_features::Features;
use codex_models_manager::model_info::model_info_from_slug;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ToolMode;
use pretty_assertions::assert_eq;

const MODEL_SLUG: &str = "test-model";

fn known_model_info() -> ModelInfo {
    ModelInfo {
        used_fallback_model_metadata: false,
        ..model_info_from_slug(MODEL_SLUG)
    }
}

#[test]
fn warning_policy_covers_features_metadata_and_model_selectors() {
    for code_mode in [false, true] {
        for code_mode_only in [false, true] {
            let mut features = Features::with_defaults();
            features.disable(Feature::CodeMode);
            features.disable(Feature::CodeModeOnly);
            if code_mode {
                features.enable(Feature::CodeMode);
            }
            if code_mode_only {
                features.enable(Feature::CodeModeOnly);
            }
            for fallback in [false, true] {
                for tool_mode in [
                    None,
                    Some(ToolMode::Direct),
                    Some(ToolMode::CodeMode),
                    Some(ToolMode::CodeModeOnly),
                ] {
                    let should_warn = (code_mode || code_mode_only)
                        && !fallback
                        && tool_mode.is_none();
                    let model_info = ModelInfo {
                        tool_mode,
                        used_fallback_model_metadata: fallback,
                        ..known_model_info()
                    };
                    let expected = should_warn.then(|| format!(
                        "Code Mode is enabled in configuration, but model `{MODEL_SLUG}` does not advertise Code Mode support. This may degrade model performance. Disable `features.code_mode` and `features.code_mode_only`, or select a model whose metadata enables Code Mode."
                    ));
                    assert_eq!(
                        unsupported_code_mode_warning(&model_info, &features),
                        expected,
                        "code_mode={code_mode}, code_mode_only={code_mode_only}, fallback={fallback}, tool_mode={:?}",
                        model_info.tool_mode
                    );
                }
            }
        }
    }
}
