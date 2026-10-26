use codex_app_server_protocol::ServerBuildInfo;
use codex_utils_build_info::BuildInfo;

pub(crate) fn server_build_info(value: BuildInfo) -> ServerBuildInfo {
    ServerBuildInfo {
        version: value.version.to_string(),
        commit: value.commit.to_string(),
        dirty: value.dirty.to_string(),
        profile: value.profile.to_string(),
        built: value.built.to_string(),
    }
}

