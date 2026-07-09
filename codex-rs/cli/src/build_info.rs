use std::sync::OnceLock;

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

    fn long_version(self) -> String {
        format!(
            "{}\ncommit: {}\ndirty: {}\nprofile: {}\nbuilt: {}",
            self.version, self.commit, self.dirty, self.profile, self.built
        )
    }
}

pub(crate) fn long_version() -> &'static str {
    static LONG_VERSION: OnceLock<String> = OnceLock::new();
    LONG_VERSION
        .get_or_init(|| BuildInfo::current().long_version())
        .as_str()
}

pub(crate) fn build_info() -> BuildInfo {
    BuildInfo::current()
}

fn default_build_profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_version_lists_build_stamp_fields() {
        let long = BuildInfo {
            version: "1.2.3",
            commit: "abcdef123456",
            dirty: "true",
            profile: "release",
            built: "123s since unix epoch",
        }
        .long_version();

        assert_eq!(
            long,
            "1.2.3\ncommit: abcdef123456\ndirty: true\nprofile: release\nbuilt: 123s since unix epoch"
        );
    }
}
