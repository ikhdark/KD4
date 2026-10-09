use codex_app_server_protocol::ThreadSourceKind;
use codex_core::INTERACTIVE_SESSION_SOURCES;
use codex_protocol::protocol::SessionSource as CoreSessionSource;
use codex_protocol::protocol::SubAgentSource as CoreSubAgentSource;

pub(crate) fn compute_source_filters(
    source_kinds: Option<Vec<ThreadSourceKind>>,
) -> (Vec<CoreSessionSource>, Option<Vec<ThreadSourceKind>>) {
    let Some(source_kinds) = source_kinds else {
        return (INTERACTIVE_SESSION_SOURCES.to_vec(), None);
    };

    if source_kinds.is_empty() {
        return (INTERACTIVE_SESSION_SOURCES.to_vec(), None);
    }

    let requires_post_filter = source_kinds.iter().any(|kind| {
        matches!(
            kind,
            ThreadSourceKind::SubAgent
                | ThreadSourceKind::SubAgentReview
                | ThreadSourceKind::SubAgentCompact
                | ThreadSourceKind::SubAgentThreadSpawn
                | ThreadSourceKind::SubAgentOther
                // Legacy rollouts can omit the source field. Storage filtering
                // would exclude them before they can be classified as Unknown.
                | ThreadSourceKind::Unknown
        )
    });

    if requires_post_filter {
        (Vec::new(), Some(source_kinds))
    } else {
        let allowed_sources = source_kinds
            .iter()
            .filter_map(|kind| match kind {
                ThreadSourceKind::Cli => Some(CoreSessionSource::Cli),
                ThreadSourceKind::VsCode => Some(CoreSessionSource::VSCode),
                ThreadSourceKind::Exec => Some(CoreSessionSource::Exec),
                ThreadSourceKind::AppServer => Some(CoreSessionSource::Mcp),
                ThreadSourceKind::SubAgent
                | ThreadSourceKind::SubAgentReview
                | ThreadSourceKind::SubAgentCompact
                | ThreadSourceKind::SubAgentThreadSpawn
                | ThreadSourceKind::SubAgentOther
                | ThreadSourceKind::Unknown => None,
            })
            .collect::<Vec<_>>();
        (allowed_sources, None)
    }
}

pub(crate) fn source_kind_matches(source: &CoreSessionSource, filter: &[ThreadSourceKind]) -> bool {
    filter.iter().any(|kind| match kind {
        ThreadSourceKind::Cli => matches!(source, CoreSessionSource::Cli),
        ThreadSourceKind::VsCode => matches!(source, CoreSessionSource::VSCode),
        ThreadSourceKind::Exec => matches!(source, CoreSessionSource::Exec),
        ThreadSourceKind::AppServer => matches!(source, CoreSessionSource::Mcp),
        ThreadSourceKind::SubAgent => matches!(source, CoreSessionSource::SubAgent(_)),
        ThreadSourceKind::SubAgentReview => {
            matches!(
                source,
                CoreSessionSource::SubAgent(CoreSubAgentSource::Review)
            )
        }
        ThreadSourceKind::SubAgentCompact => {
            matches!(
                source,
                CoreSessionSource::SubAgent(CoreSubAgentSource::Compact)
            )
        }
        ThreadSourceKind::SubAgentThreadSpawn => matches!(
            source,
            CoreSessionSource::SubAgent(CoreSubAgentSource::ThreadSpawn { .. })
        ),
        ThreadSourceKind::SubAgentOther => matches!(
            source,
            CoreSessionSource::SubAgent(CoreSubAgentSource::Other(_))
        ),
        ThreadSourceKind::Unknown => matches!(source, CoreSessionSource::Unknown),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::ThreadId;
    use pretty_assertions::assert_eq;
    use uuid::Uuid;

    #[test]
    fn compute_source_filters_defaults_to_interactive_sources() {
        for source_kinds in [None, Some(Vec::new())] {
            let (allowed_sources, filter) = compute_source_filters(source_kinds);
            assert_eq!(allowed_sources, INTERACTIVE_SESSION_SOURCES.to_vec());
            assert_eq!(filter, None);
        }
    }

    #[test]
    fn compute_source_filters_direct_sources_skip_post_filtering() {
        let source_kinds = vec![
            ThreadSourceKind::Cli,
            ThreadSourceKind::VsCode,
            ThreadSourceKind::Exec,
            ThreadSourceKind::AppServer,
        ];
        let (allowed_sources, filter) = compute_source_filters(Some(source_kinds));

        assert_eq!(
            allowed_sources,
            vec![
                CoreSessionSource::Cli,
                CoreSessionSource::VSCode,
                CoreSessionSource::Exec,
                CoreSessionSource::Mcp
            ]
        );
        assert_eq!(filter, None);
    }

    #[test]
    fn compute_source_filters_subagent_variant_requires_post_filtering() {
        for kind in [
            ThreadSourceKind::SubAgent,
            ThreadSourceKind::SubAgentReview,
            ThreadSourceKind::SubAgentCompact,
            ThreadSourceKind::SubAgentThreadSpawn,
            ThreadSourceKind::SubAgentOther,
            ThreadSourceKind::Unknown,
        ] {
            for source_kinds in [vec![kind], vec![ThreadSourceKind::Cli, kind]] {
                let (allowed_sources, filter) = compute_source_filters(Some(source_kinds.clone()));
                assert_eq!(allowed_sources, Vec::new(), "{source_kinds:?}");
                assert_eq!(filter, Some(source_kinds));
            }
        }
    }

    #[test]
    fn source_kind_matches_distinguishes_subagent_variants() {
        let parent_thread_id =
            ThreadId::from_string(&Uuid::new_v4().to_string()).expect("valid thread id");
        let spawn = CoreSessionSource::SubAgent(CoreSubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        });

        let cases = [
            (CoreSessionSource::Cli, ThreadSourceKind::Cli, false),
            (CoreSessionSource::VSCode, ThreadSourceKind::VsCode, false),
            (CoreSessionSource::Exec, ThreadSourceKind::Exec, false),
            (CoreSessionSource::Mcp, ThreadSourceKind::AppServer, false),
            (
                CoreSessionSource::SubAgent(CoreSubAgentSource::Review),
                ThreadSourceKind::SubAgentReview,
                true,
            ),
            (
                CoreSessionSource::SubAgent(CoreSubAgentSource::Compact),
                ThreadSourceKind::SubAgentCompact,
                true,
            ),
            (spawn, ThreadSourceKind::SubAgentThreadSpawn, true),
            (
                CoreSessionSource::SubAgent(CoreSubAgentSource::Other("custom".into())),
                ThreadSourceKind::SubAgentOther,
                true,
            ),
            (CoreSessionSource::Unknown, ThreadSourceKind::Unknown, false),
        ];
        for (source, expected_kind, is_subagent) in &cases {
            assert!(!source_kind_matches(source, &[]));
            assert_eq!(
                source_kind_matches(source, &[ThreadSourceKind::SubAgent]),
                *is_subagent
            );
            for (_, filter_kind, _) in &cases {
                assert_eq!(
                    source_kind_matches(source, &[*filter_kind]),
                    expected_kind == filter_kind,
                    "source {source:?}, filter {filter_kind:?}"
                );
                assert_eq!(
                    source_kind_matches(source, &[ThreadSourceKind::SubAgent, *filter_kind]),
                    *is_subagent || expected_kind == filter_kind,
                    "union filter for source {source:?}, filter {filter_kind:?}"
                );
            }
        }
    }
}
