use super::*;
use codex_config::NetworkDomainPermissionToml;
use codex_config::NetworkDomainPermissionsToml;
use codex_network_proxy::NetworkDomainPermission;
use codex_protocol::models::ManagedFileSystemPermissions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::NetworkSandboxPolicy;
use pretty_assertions::assert_eq;

fn domain_permissions(
    entries: impl IntoIterator<Item = (&'static str, NetworkDomainPermissionToml)>,
) -> NetworkDomainPermissionsToml {
    NetworkDomainPermissionsToml {
        entries: entries
            .into_iter()
            .map(|(pattern, permission)| (pattern.to_string(), permission))
            .collect(),
    }
}

#[tokio::test]
async fn build_state_with_audit_metadata_threads_metadata_to_state() {
    let spec = NetworkProxySpec {
        base_config: NetworkProxyConfig::default(),
        requirements: None,
        config: NetworkProxyConfig::default(),
        constraints: NetworkProxyConstraints::default(),
        hard_deny_allowlist_misses: false,
    };
    let metadata = NetworkProxyAuditMetadata {
        conversation_id: Some("conversation-1".to_string()),
        app_version: Some("1.2.3".to_string()),
        user_account_id: Some("acct-1".to_string()),
        ..NetworkProxyAuditMetadata::default()
    };
    let codex_home = tempfile::tempdir().expect("temporary Codex home");

    let state = spec
        .build_state_with_audit_metadata(codex_home.path(), metadata.clone())
        .await
        .expect("state should build");
    assert_eq!(state.audit_metadata(), &metadata);
}

#[test]
fn requirements_allowed_domains_are_a_baseline_for_user_allowlist() {
    let mut config = NetworkProxyConfig::default();
    config.set_allowed_domains(vec!["api.example.com".to_string()]);
    let requirements = NetworkConstraints {
        domains: Some(domain_permissions([(
            "*.example.com",
            NetworkDomainPermissionToml::Allow,
        )])),
        ..Default::default()
    };

    let spec = NetworkProxySpec::from_config_and_constraints(
        config,
        Some(requirements),
        &PermissionProfile::read_only(),
    )
    .expect("config should stay within the managed allowlist");

    assert_eq!(
        spec.config.allowed_domains(),
        Some(vec![
            "*.example.com".to_string(),
            "api.example.com".to_string()
        ])
    );
    assert_eq!(
        spec.constraints.allowed_domains,
        Some(vec!["*.example.com".to_string()])
    );
    assert_eq!(spec.constraints.allowlist_expansion_enabled, Some(true));
}

#[test]
fn requirements_allowed_domains_do_not_override_user_denies_for_same_pattern() {
    let mut config = NetworkProxyConfig::default();
    config.set_denied_domains(vec!["api.example.com".to_string()]);
    let requirements = NetworkConstraints {
        domains: Some(domain_permissions([(
            "api.example.com",
            NetworkDomainPermissionToml::Allow,
        )])),
        ..Default::default()
    };

    let spec = NetworkProxySpec::from_config_and_constraints(
        config,
        Some(requirements),
        &PermissionProfile::workspace_write(),
    )
    .expect("managed allowlist should not erase a user deny");

    assert_eq!(spec.config.allowed_domains(), None);
    assert_eq!(
        spec.config.denied_domains(),
        Some(vec!["api.example.com".to_string()])
    );
    assert_eq!(
        spec.constraints.allowed_domains,
        Some(vec!["api.example.com".to_string()])
    );
}

#[test]
fn requirements_allowlist_expansion_keeps_user_entries_mutable() {
    let mut config = NetworkProxyConfig::default();
    config.set_allowed_domains(vec!["api.example.com".to_string()]);
    let requirements = NetworkConstraints {
        domains: Some(domain_permissions([(
            "*.example.com",
            NetworkDomainPermissionToml::Allow,
        )])),
        ..Default::default()
    };

    let spec = NetworkProxySpec::from_config_and_constraints(
        config,
        Some(requirements),
        &PermissionProfile::workspace_write(),
    )
    .expect("managed baseline should still allow user edits");

    let mut candidate = spec.config.clone();
    candidate.upsert_domain_permission(
        "api.example.com".to_string(),
        NetworkDomainPermission::Deny,
        normalize_host,
    );

    assert_eq!(
        candidate.allowed_domains(),
        Some(vec!["*.example.com".to_string()])
    );
    assert_eq!(
        candidate.denied_domains(),
        Some(vec!["api.example.com".to_string()])
    );
    validate_policy_against_constraints(&candidate, &spec.constraints)
        .expect("user allowlist entries should not become managed constraints");
}

#[test]
fn managed_unrestricted_profile_allows_domain_expansion() {
    let mut config = NetworkProxyConfig::default();
    config.set_allowed_domains(vec!["api.example.com".to_string()]);
    let requirements = NetworkConstraints {
        domains: Some(domain_permissions([(
            "*.example.com",
            NetworkDomainPermissionToml::Allow,
        )])),
        ..Default::default()
    };
    let permission_profile = PermissionProfile::Managed {
        file_system: ManagedFileSystemPermissions::Unrestricted,
        network: NetworkSandboxPolicy::Restricted,
    };

    let spec = NetworkProxySpec::from_config_and_constraints(
        config,
        Some(requirements),
        &permission_profile,
    )
    .expect("managed unrestricted filesystem should still use managed network constraints");

    assert_eq!(
        spec.config.allowed_domains(),
        Some(vec![
            "*.example.com".to_string(),
            "api.example.com".to_string()
        ])
    );
    assert_eq!(spec.constraints.allowlist_expansion_enabled, Some(true));
}

#[test]
fn danger_full_access_keeps_managed_allowlist_and_denylist_fixed() {
    let mut config = NetworkProxyConfig::default();
    config.set_allowed_domains(vec!["evil.com".to_string()]);
    config.set_denied_domains(vec!["more-blocked.example.com".to_string()]);
    let requirements = NetworkConstraints {
        domains: Some(domain_permissions([
            ("*.example.com", NetworkDomainPermissionToml::Allow),
            ("blocked.example.com", NetworkDomainPermissionToml::Deny),
        ])),
        ..Default::default()
    };

    let spec = NetworkProxySpec::from_config_and_constraints(
        config,
        Some(requirements),
        &PermissionProfile::Disabled,
    )
    .expect("yolo mode should pin the effective policy to the managed baseline");

    assert_eq!(
        spec.config.allowed_domains(),
        Some(vec!["*.example.com".to_string()])
    );
    assert_eq!(
        spec.config.denied_domains(),
        Some(vec!["blocked.example.com".to_string()])
    );
    assert_eq!(spec.constraints.allowlist_expansion_enabled, Some(false));
    assert_eq!(spec.constraints.denylist_expansion_enabled, Some(false));
}

#[test]
fn managed_allowed_domains_only_disables_default_mode_allowlist_expansion() {
    let mut config = NetworkProxyConfig::default();
    config.set_allowed_domains(vec!["api.example.com".to_string()]);
    let requirements = NetworkConstraints {
        domains: Some(domain_permissions([(
            "*.example.com",
            NetworkDomainPermissionToml::Allow,
        )])),
        managed_allowed_domains_only: Some(true),
        ..Default::default()
    };

    let spec = NetworkProxySpec::from_config_and_constraints(
        config,
        Some(requirements),
        &PermissionProfile::workspace_write(),
    )
    .expect("managed baseline should still load");

    assert_eq!(
        spec.config.allowed_domains(),
        Some(vec!["*.example.com".to_string()])
    );
    assert_eq!(spec.constraints.allowlist_expansion_enabled, Some(false));
}

#[test]
fn managed_allowed_domains_only_ignores_user_allowlist_and_hard_denies_misses() {
    let mut config = NetworkProxyConfig::default();
    config.set_allowed_domains(vec!["api.example.com".to_string()]);
    let requirements = NetworkConstraints {
        domains: Some(domain_permissions([(
            "managed.example.com",
            NetworkDomainPermissionToml::Allow,
        )])),
        managed_allowed_domains_only: Some(true),
        ..Default::default()
    };

    let spec = NetworkProxySpec::from_config_and_constraints(
        config,
        Some(requirements),
        &PermissionProfile::workspace_write(),
    )
    .expect("managed-only allowlist should still load");

    assert_eq!(
        spec.config.allowed_domains(),
        Some(vec!["managed.example.com".to_string()])
    );
    assert_eq!(
        spec.constraints.allowed_domains,
        Some(vec!["managed.example.com".to_string()])
    );
    assert_eq!(spec.constraints.allowlist_expansion_enabled, Some(false));
    assert!(spec.hard_deny_allowlist_misses);
}

#[test]
fn managed_allowed_domains_only_without_managed_allowlist_blocks_all_user_domains() {
    let mut config = NetworkProxyConfig::default();
    config.set_allowed_domains(vec!["api.example.com".to_string()]);
    let requirements = NetworkConstraints {
        managed_allowed_domains_only: Some(true),
        ..Default::default()
    };

    let spec = NetworkProxySpec::from_config_and_constraints(
        config,
        Some(requirements),
        &PermissionProfile::workspace_write(),
    )
    .expect("managed-only mode should treat missing managed allowlist as empty");

    assert_eq!(spec.config.allowed_domains(), None);
    assert_eq!(spec.constraints.allowed_domains, Some(Vec::new()));
    assert_eq!(spec.constraints.allowlist_expansion_enabled, Some(false));
    assert!(spec.hard_deny_allowlist_misses);
}

#[test]
fn managed_allowed_domains_only_blocks_all_user_domains_in_full_access_without_managed_list() {
    let mut config = NetworkProxyConfig::default();
    config.set_allowed_domains(vec!["api.example.com".to_string()]);
    let requirements = NetworkConstraints {
        managed_allowed_domains_only: Some(true),
        ..Default::default()
    };

    let spec = NetworkProxySpec::from_config_and_constraints(
        config,
        Some(requirements),
        &PermissionProfile::Disabled,
    )
    .expect("managed-only mode should treat missing managed allowlist as empty");

    assert_eq!(spec.config.allowed_domains(), None);
    assert_eq!(spec.constraints.allowed_domains, Some(Vec::new()));
    assert_eq!(spec.constraints.allowlist_expansion_enabled, Some(false));
    assert!(spec.hard_deny_allowlist_misses);
}

#[test]
fn deny_only_requirements_do_not_create_allow_constraints_in_full_access() {
    let mut config = NetworkProxyConfig::default();
    config.set_allowed_domains(vec!["api.example.com".to_string()]);
    let requirements = NetworkConstraints {
        domains: Some(domain_permissions([(
            "managed-blocked.example.com",
            NetworkDomainPermissionToml::Deny,
        )])),
        ..Default::default()
    };

    let spec = NetworkProxySpec::from_config_and_constraints(
        config,
        Some(requirements),
        &PermissionProfile::Disabled,
    )
    .expect("deny-only requirements should not constrain the allowlist");

    assert_eq!(
        spec.config.allowed_domains(),
        Some(vec!["api.example.com".to_string()])
    );
    assert_eq!(spec.constraints.allowed_domains, None);
    assert_eq!(spec.constraints.allowlist_expansion_enabled, None);
    assert_eq!(
        spec.config.denied_domains(),
        Some(vec!["managed-blocked.example.com".to_string()])
    );
}

#[test]
fn allow_only_requirements_do_not_create_deny_constraints_in_full_access() {
    let mut config = NetworkProxyConfig::default();
    config.set_denied_domains(vec!["blocked.example.com".to_string()]);
    let requirements = NetworkConstraints {
        domains: Some(domain_permissions([(
            "managed.example.com",
            NetworkDomainPermissionToml::Allow,
        )])),
        ..Default::default()
    };

    let spec = NetworkProxySpec::from_config_and_constraints(
        config,
        Some(requirements),
        &PermissionProfile::Disabled,
    )
    .expect("allow-only requirements should not constrain the denylist");

    assert_eq!(
        spec.config.allowed_domains(),
        Some(vec!["managed.example.com".to_string()])
    );
    assert_eq!(
        spec.config.denied_domains(),
        Some(vec!["blocked.example.com".to_string()])
    );
    assert_eq!(spec.constraints.denied_domains, None);
    assert_eq!(spec.constraints.denylist_expansion_enabled, None);
}

#[test]
fn requirements_denied_domains_are_a_baseline_for_default_mode() {
    let mut config = NetworkProxyConfig::default();
    config.set_denied_domains(vec!["blocked.example.com".to_string()]);
    let requirements = NetworkConstraints {
        domains: Some(domain_permissions([(
            "managed-blocked.example.com",
            NetworkDomainPermissionToml::Deny,
        )])),
        ..Default::default()
    };

    let spec = NetworkProxySpec::from_config_and_constraints(
        config,
        Some(requirements),
        &PermissionProfile::workspace_write(),
    )
    .expect("default mode should merge managed and user deny entries");

    assert_eq!(
        spec.config.denied_domains(),
        Some(vec![
            "managed-blocked.example.com".to_string(),
            "blocked.example.com".to_string()
        ])
    );
    assert_eq!(
        spec.constraints.denied_domains,
        Some(vec!["managed-blocked.example.com".to_string()])
    );
    assert_eq!(spec.constraints.denylist_expansion_enabled, Some(true));
}

#[test]
fn requirements_denylist_expansion_keeps_user_entries_mutable() {
    let mut config = NetworkProxyConfig::default();
    config.set_denied_domains(vec!["blocked.example.com".to_string()]);
    let requirements = NetworkConstraints {
        domains: Some(domain_permissions([(
            "managed-blocked.example.com",
            NetworkDomainPermissionToml::Deny,
        )])),
        ..Default::default()
    };

    let spec = NetworkProxySpec::from_config_and_constraints(
        config,
        Some(requirements),
        &PermissionProfile::workspace_write(),
    )
    .expect("managed baseline should still allow user edits");

    let mut candidate = spec.config.clone();
    candidate.upsert_domain_permission(
        "blocked.example.com".to_string(),
        NetworkDomainPermission::Allow,
        normalize_host,
    );

    assert_eq!(
        candidate.allowed_domains(),
        Some(vec!["blocked.example.com".to_string()])
    );
    assert_eq!(
        candidate.denied_domains(),
        Some(vec!["managed-blocked.example.com".to_string()])
    );
    validate_policy_against_constraints(&candidate, &spec.constraints)
        .expect("user denylist entries should not become managed constraints");
}

#[tokio::test(flavor = "current_thread")]
async fn start_proxy_initializes_managed_ca_without_blocking_runtime() {
    use std::collections::HashMap;
    use std::fs;
    use std::sync::mpsc;
    use std::time::Duration;

    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    let proxy_dir = codex_home.path().join("proxy");
    fs::create_dir(&proxy_dir).expect("create certificate directory");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(proxy_dir.join(".artifacts.lock"))
        .expect("open managed CA artifact lock");
    let (locked_tx, locked_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let lock_holder = std::thread::spawn(move || {
        lock.lock().expect("hold managed CA artifact lock");
        locked_tx.send(()).expect("announce held lock");
        // Release even on a regression, so synchronous CA construction fails instead of hanging.
        let runtime_progressed = release_rx.recv_timeout(Duration::from_secs(2)).is_ok();
        drop(lock);
        runtime_progressed
    });
    locked_rx.recv().expect("certificate lock should be held");

    let permission_profile = PermissionProfile::workspace_write();
    let mut config = NetworkProxyConfig {
        enabled: true,
        mitm: true,
        proxy_url: "http://127.0.0.1:0".to_string(),
        enable_socks5: false,
        ..NetworkProxyConfig::default()
    };
    config.set_allowed_domains(vec!["api.example.com".to_string()]);
    let spec = NetworkProxySpec::from_config_and_constraints(config, None, &permission_profile)
        .expect("valid proxy spec");
    let mut startup = Box::pin(spec.start_proxy(
        codex_home.path(),
        &permission_profile,
        None,
        None,
        false,
        NetworkProxyAuditMetadata::default(),
    ));

    assert!(futures::poll!(startup.as_mut()).is_pending());
    tokio::time::sleep(Duration::from_millis(10)).await;
    let _ = release_tx.send(());
    assert!(
        lock_holder.join().expect("lock holder should finish"),
        "managed CA initialization prevented the current-thread runtime from releasing the artifact lock"
    );
    let started = startup.await.expect("managed proxy should start");
    let proxy = started.proxy();
    let current = proxy.current_cfg().await.expect("published proxy config");
    assert!(current.mitm);
    assert_eq!(
        current.allowed_domains(),
        Some(vec!["api.example.com".to_string()])
    );

    let bundle_path = proxy
        .managed_mitm_ca_trust_bundle_path()
        .expect("child processes should receive the managed CA bundle");
    assert!(bundle_path.as_path().starts_with(&proxy_dir));
    let bundle = fs::read_to_string(bundle_path.as_path()).expect("read child CA bundle");
    let certificate_paths = fs::read_dir(&proxy_dir)
        .expect("certificate artifacts")
        .map(|entry| entry.expect("certificate entry").path())
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            name.starts_with("ca-") && !name.starts_with("ca-bundle-") && name.ends_with(".pem")
        })
        .collect::<Vec<_>>();
    assert_eq!(certificate_paths.len(), 1);
    let certificate = fs::read_to_string(&certificate_paths[0]).expect("read generated CA");
    assert!(certificate.starts_with("-----BEGIN CERTIFICATE-----"));
    assert!(bundle.contains(&certificate));
    assert!(!bundle.contains("PRIVATE KEY"));
    let mut child_env = HashMap::new();
    proxy.apply_to_env(&mut child_env);
    assert_eq!(
        child_env.get("SSL_CERT_FILE"),
        Some(&bundle_path.as_path().display().to_string())
    );
}
