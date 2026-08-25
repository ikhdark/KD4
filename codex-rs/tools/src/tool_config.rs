use codex_features::Feature;
use codex_features::Features;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::TUI_VISIBLE_COLLABORATION_MODES;
use codex_protocol::openai_models::ConfigShellToolType;
use codex_protocol::openai_models::ModelInfo;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum UnifiedExecFeatureMode {
    /// Unified exec should not be selected by this feature set.
    Disabled,
    Direct,
}

pub fn request_user_input_available_modes(features: &Features) -> Vec<ModeKind> {
    TUI_VISIBLE_COLLABORATION_MODES
        .into_iter()
        .filter(|mode| {
            mode.allows_request_user_input()
                || (features.enabled(Feature::DefaultModeRequestUserInput)
                    && *mode == ModeKind::Default)
        })
        .collect()
}

/// Returns the unified-exec mode requested by feature policy, before runtime
/// platform support is resolved.
pub fn unified_exec_feature_mode_for_features(features: &Features) -> UnifiedExecFeatureMode {
    if !features.enabled(Feature::ShellTool) || !features.enabled(Feature::UnifiedExec) {
        UnifiedExecFeatureMode::Disabled
    } else {
        UnifiedExecFeatureMode::Direct
    }
}

pub fn shell_type_for_model_and_features(
    model_info: &ModelInfo,
    features: &Features,
) -> ConfigShellToolType {
    let unified_exec_feature_mode = unified_exec_feature_mode_for_features(features);
    let unified_exec_disabled =
        matches!(unified_exec_feature_mode, UnifiedExecFeatureMode::Disabled);
    let model_shell_type = match model_info.shell_type {
        ConfigShellToolType::UnifiedExec if unified_exec_disabled => {
            ConfigShellToolType::ShellCommand
        }
        ConfigShellToolType::Default | ConfigShellToolType::Local => {
            ConfigShellToolType::ShellCommand
        }
        other => other,
    };
    let shell_command_type = model_shell_type;

    if !features.enabled(Feature::ShellTool) {
        ConfigShellToolType::Disabled
    } else {
        match unified_exec_feature_mode {
            UnifiedExecFeatureMode::Disabled => shell_command_type,
            UnifiedExecFeatureMode::Direct => {
                if codex_utils_pty::conpty_supported() {
                    ConfigShellToolType::UnifiedExec
                } else {
                    ConfigShellToolType::ShellCommand
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolEnvironmentMode {
    None,
    Single,
    Multiple,
}

impl ToolEnvironmentMode {
    pub fn from_count(count: usize) -> Self {
        match count {
            0 => Self::None,
            1 => Self::Single,
            _ => Self::Multiple,
        }
    }

    pub fn has_environment(self) -> bool {
        !matches!(self, Self::None)
    }
}

#[cfg(test)]
#[path = "tool_config_tests.rs"]
mod tests;
