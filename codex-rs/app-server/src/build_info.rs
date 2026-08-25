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

#[cfg(test)]
mod tests {
    use super::server_build_info;
    use codex_app_server_protocol::ServerBuildInfo;
    use codex_utils_build_info::BuildInfo;

    #[test]
    fn server_build_info_preserves_shared_runtime_metadata() {
        let build = BuildInfo::from_values(
            "1.2.3",
            Some("abc123"),
            None,
            Some("false"),
            Some("release"),
            Some("2026-08-24T00:00:00Z"),
            false,
        );

        assert_eq!(
            server_build_info(build),
            ServerBuildInfo {
                version: "1.2.3".to_string(),
                commit: "abc123".to_string(),
                dirty: "false".to_string(),
                profile: "release".to_string(),
                built: "2026-08-24T00:00:00Z".to_string(),
            }
        );
    }
}
