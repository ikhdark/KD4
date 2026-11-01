use codex_app_server_protocol_types::ConfigWarningNotification;
use codex_app_server_protocol_types::TextPosition as AppTextPosition;
use codex_app_server_protocol_types::TextRange as AppTextRange;
use codex_config::ConfigLoadError;
use codex_config::TextRange as CoreTextRange;
use codex_core::ExecPolicyError;

pub fn config_warning_from_error(
    summary: impl Into<String>,
    err: &std::io::Error,
) -> ConfigWarningNotification {
    let (path, range) = match config_error_location(err) {
        Some((path, range)) => (Some(path), Some(range)),
        None => (None, None),
    };
    ConfigWarningNotification {
        summary: summary.into(),
        details: Some(err.to_string()),
        path,
        range,
    }
}

fn config_error_location(err: &std::io::Error) -> Option<(String, AppTextRange)> {
    err.get_ref()
        .and_then(|err| err.downcast_ref::<ConfigLoadError>())
        .map(|err| {
            let config_error = err.config_error();
            (
                config_error.path.to_string_lossy().to_string(),
                app_text_range(&config_error.range),
            )
        })
}

pub fn exec_policy_warning_location(err: &ExecPolicyError) -> (Option<String>, Option<AppTextRange>) {
    match err {
        ExecPolicyError::ParsePolicy { path, source } => {
            if let Some(location) = source.location() {
                return (Some(location.path), Some(app_text_range(&location.range)));
            }
            (Some(path.clone()), None)
        }
        _ => (None, None),
    }
}

pub fn exec_policy_config_warning(err: &ExecPolicyError) -> ConfigWarningNotification {
    let (path, range) = exec_policy_warning_location(err);
    ConfigWarningNotification {
        summary: "Error parsing rules; custom rules not applied.".to_string(),
        details: Some(err.to_string()),
        path,
        range,
    }
}

fn app_text_range(range: &CoreTextRange) -> AppTextRange {
    AppTextRange {
        start: AppTextPosition {
            line: range.start.line,
            column: range.start.column,
        },
        end: AppTextPosition {
            line: range.end.line,
            column: range.end.column,
        },
    }
}
