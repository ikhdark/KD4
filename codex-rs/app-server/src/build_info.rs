use codex_app_server_protocol::ServerBuildInfo;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BuildInfo {
    pub(crate) version: &'static str,
    pub(crate) commit: &'static str,
    pub(crate) dirty: &'static str,
    pub(crate) profile: &'static str,
    pub(crate) built: &'static str,
}

impl BuildInfo {
    pub(crate) fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            commit: option_env!("CODEX_BUILD_COMMIT")
                .or(option_env!("GIT_COMMIT"))
                .unwrap_or("unknown"),
            dirty: option_env!("CODEX_BUILD_DIRTY").unwrap_or("unknown"),
            profile: option_env!("CODEX_BUILD_PROFILE").unwrap_or_else(default_build_profile),
            built: option_env!("CODEX_BUILD_TIMESTAMP").unwrap_or("unknown"),
        }
    }
}

impl From<BuildInfo> for ServerBuildInfo {
    fn from(value: BuildInfo) -> Self {
        Self {
            version: value.version.to_string(),
            commit: value.commit.to_string(),
            dirty: value.dirty.to_string(),
            profile: value.profile.to_string(),
            built: value.built.to_string(),
        }
    }
}

fn default_build_profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}
