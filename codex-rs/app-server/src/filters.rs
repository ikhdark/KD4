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

