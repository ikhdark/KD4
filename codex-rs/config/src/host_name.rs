use std::sync::LazyLock;
use winapi_util::sysinfo::ComputerNameKind;
use winapi_util::sysinfo::get_computer_name;

static HOST_NAME: LazyLock<Option<String>> = LazyLock::new(compute_host_name);

/// Returns a process-cached canonical hostname, falling back to the normalized
/// Windows hostname.
pub fn host_name() -> Option<String> {
    HOST_NAME.clone()
}

fn compute_host_name() -> Option<String> {
    let kernel_hostname = gethostname::gethostname();
    let kernel_hostname = normalize_host_name(&kernel_hostname.to_string_lossy())?;

    // Remote sandbox requirements are meant to target remote hosts by DNS name,
    // so prefer the canonical FQDN when the local resolver can provide one.
    // This is best-effort host classification, not authenticated device proof.
    if let Some(fqdn) = local_fqdn_for_hostname(&kernel_hostname) {
        return Some(fqdn);
    }

    // Some machines have only a short local hostname or resolver setup that
    // does not return AI_CANONNAME. Keep matching behavior best-effort by
    // falling back to the cleaned kernel hostname instead of returning None.
    Some(kernel_hostname)
}

fn normalize_host_name(hostname: &str) -> Option<String> {
    let hostname = hostname.trim().trim_end_matches('.');
    (!hostname.is_empty()).then(|| hostname.to_ascii_lowercase())
}

fn local_fqdn_for_hostname(_hostname: &str) -> Option<String> {
    get_computer_name(ComputerNameKind::PhysicalDnsFullyQualified)
        .ok()
        .and_then(|hostname| hostname.into_string().ok())
        .and_then(|hostname| normalize_fqdn_candidate(&hostname))
}

fn normalize_fqdn_candidate(hostname: &str) -> Option<String> {
    normalize_host_name(hostname).filter(|hostname| hostname.contains('.'))
}

#[cfg(test)]
mod tests {
    use super::normalize_fqdn_candidate;
    use pretty_assertions::assert_eq;

    #[test]
    fn normalize_fqdn_candidate_accepts_dns_qualified_name() {
        assert_eq!(
            normalize_fqdn_candidate("runner-01.ci.example.com"),
            Some("runner-01.ci.example.com".to_string())
        );
    }

    #[test]
    fn normalize_fqdn_candidate_rejects_short_name() {
        assert_eq!(normalize_fqdn_candidate("runner-01"), None);
    }

    #[test]
    fn normalize_fqdn_candidate_trims_trailing_dot_and_normalizes_case() {
        assert_eq!(
            normalize_fqdn_candidate("RUNNER-01.CI.EXAMPLE.COM."),
            Some("runner-01.ci.example.com".to_string())
        );
    }
}
