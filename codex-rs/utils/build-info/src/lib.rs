//! Shared runtime build metadata for executable surfaces.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuildInfo {
    pub version: &'static str,
    pub commit: &'static str,
    pub dirty: &'static str,
    pub profile: &'static str,
    pub built: &'static str,
}

impl BuildInfo {
    pub fn current() -> Self {
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

    #[doc(hidden)]
    pub const fn from_values(
        version: &'static str,
        codex_commit: Option<&'static str>,
        legacy_commit: Option<&'static str>,
        dirty: Option<&'static str>,
        profile: Option<&'static str>,
        built: Option<&'static str>,
        debug_assertions: bool,
    ) -> Self {
        Self {
            version,
            commit: match codex_commit {
                Some(commit) => commit,
                None => match legacy_commit {
                    Some(commit) => commit,
                    None => "unknown",
                },
            },
            dirty: match dirty {
                Some(dirty) => dirty,
                None => "unknown",
            },
            profile: match profile {
                Some(profile) => profile,
                None if debug_assertions => "debug",
                None => "release",
            },
            built: match built {
                Some(built) => built,
                None => "unknown",
            },
        }
    }
}

const fn default_build_profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

#[cfg(test)]
mod tests {
    use super::BuildInfo;

    #[test]
    fn commit_precedence_and_fallbacks_are_shared() {
        assert_eq!(
            BuildInfo::from_values(
                "1.2.3",
                Some("codex"),
                Some("legacy"),
                Some("true"),
                Some("custom"),
                Some("now"),
                true,
            ),
            BuildInfo {
                version: "1.2.3",
                commit: "codex",
                dirty: "true",
                profile: "custom",
                built: "now",
            }
        );
        assert_eq!(
            BuildInfo::from_values("1.2.3", None, Some("legacy"), None, None, None, false),
            BuildInfo {
                version: "1.2.3",
                commit: "legacy",
                dirty: "unknown",
                profile: "release",
                built: "unknown",
            }
        );
        assert_eq!(
            BuildInfo::from_values("1.2.3", None, None, None, None, None, true).profile,
            "debug"
        );
    }
}
