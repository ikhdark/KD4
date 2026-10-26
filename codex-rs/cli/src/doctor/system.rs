use std::collections::BTreeMap;
use std::env;

use super::DoctorCheck;
use super::LOCALE_ENV_VARS;

const EDITOR_ENV_VARS: &[&str] = &["VISUAL", "EDITOR"];
const PAGER_ENV_VARS: &[&str] = &["PAGER", "GIT_PAGER", "GH_PAGER", "LESS"];

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SystemCheckInputs {
    os: String,
    os_type: String,
    os_version: String,
    os_language: Option<String>,
    locale_env: BTreeMap<String, String>,
    editor_env: BTreeMap<String, String>,
    pager_env: BTreeMap<String, String>,
}

impl SystemCheckInputs {
    fn detect() -> Self {
        let info = os_info::get();
        let locale_env = LOCALE_ENV_VARS
            .iter()
            .filter_map(|name| {
                env::var(name)
                    .ok()
                    .map(|value| ((*name).to_string(), value))
            })
            .collect();
        let editor_env = EDITOR_ENV_VARS
            .iter()
            .map(|name| {
                let value = env::var_os(name)
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "not set".to_string());
                ((*name).to_string(), value)
            })
            .collect();
        let pager_env = PAGER_ENV_VARS
            .iter()
            .filter_map(|name| {
                env::var_os(name)
                    .map(|value| ((*name).to_string(), value.to_string_lossy().into_owned()))
            })
            .collect();
        Self {
            os: info.to_string(),
            os_type: info.os_type().to_string(),
            os_version: info.version().to_string(),
            os_language: sys_locale::get_locale(),
            locale_env,
            editor_env,
            pager_env,
        }
    }
}

pub(super) fn system_check() -> DoctorCheck {
    system_check_from_inputs(SystemCheckInputs::detect())
}

fn system_check_from_inputs(inputs: SystemCheckInputs) -> DoctorCheck {
    let mut details = vec![
        format!("os: {}", inputs.os),
        format!("os type: {}", inputs.os_type),
        format!("os version: {}", inputs.os_version),
    ];
    if let Some(language) = inputs.os_language.as_deref() {
        details.push(format!("os language: {language}"));
    } else {
        details.push("os language: unavailable".to_string());
    }
    for name in LOCALE_ENV_VARS {
        if let Some(value) = inputs.locale_env.get(*name) {
            details.push(format!("{name}: {value}"));
        }
    }
    for name in EDITOR_ENV_VARS {
        if let Some(value) = inputs.editor_env.get(*name) {
            details.push(format!("{name}: {value}"));
        }
    }
    for name in PAGER_ENV_VARS {
        if let Some(value) = inputs.pager_env.get(*name) {
            details.push(format!("{name}: {value}"));
        }
    }

    let summary = inputs
        .os_language
        .as_deref()
        .map(|language| format!("OS language {language}"))
        .unwrap_or_else(|| "OS language unavailable".to_string());
    DoctorCheck::new(
        "system.environment",
        "system",
        super::CheckStatus::Ok,
        summary,
    )
    .details(details)
}

