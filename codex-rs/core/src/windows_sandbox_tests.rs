use super::*;
use codex_config::types::WindowsToml;
use codex_features::Features;
use codex_features::FeaturesToml;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

#[test]
fn setup_failure_metrics_collapse_dynamic_paths_and_keep_diagnostics() {
    use codex_otel::MetricsClient;
    use codex_otel::MetricsConfig;
    use codex_windows_sandbox::SetupErrorCode;
    use codex_windows_sandbox::SetupFailure;
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::metrics::data::AggregatedMetrics;
    use opentelemetry_sdk::metrics::data::MetricData;

    let metrics = MetricsClient::new(
        MetricsConfig::in_memory(
            "test",
            "codex-core",
            env!("CARGO_PKG_VERSION"),
            InMemoryMetricExporter::default(),
        )
        .with_runtime_reader(),
    )
    .expect("actual metric collector");
    let buffer: &'static std::sync::Mutex<Vec<u8>> =
        Box::leak(Box::new(std::sync::Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(tracing_test::internal::MockWriter::new(buffer))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let messages = [
        "failed to create C:/workspace-alpha/.sandbox",
        "failed to create D:/workspace-beta/.sandbox",
    ];
    for (message, originator) in messages.into_iter().zip(["custom-alpha", "custom-beta"]) {
        let error = anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::OrchestratorSandboxDirCreateFailed,
            message,
        ));
        emit_windows_sandbox_setup_failure_metrics(
            WindowsSandboxSetupMode::Elevated,
            originator,
            std::time::Duration::from_millis(1),
            &error,
            Some(&metrics),
        );
        assert!(error.to_string().contains(message));
    }
    let canceled = anyhow::Error::new(SetupFailure::new(
        SetupErrorCode::OrchestratorHelperLaunchCanceled,
        "user declined elevation for C:/workspace-gamma",
    ));
    emit_windows_sandbox_setup_failure_metrics(
        WindowsSandboxSetupMode::Elevated,
        "codex_cli_rs",
        std::time::Duration::from_millis(2),
        &canceled,
        Some(&metrics),
    );
    let unknown = anyhow::anyhow!("unexpected error at E:/workspace-delta");
    emit_windows_sandbox_setup_failure_metrics(
        WindowsSandboxSetupMode::Elevated,
        "custom-delta",
        std::time::Duration::from_millis(3),
        &unknown,
        Some(&metrics),
    );

    let snapshot = metrics.snapshot().expect("collect emitted counter labels");
    let counter_points = |name: &str| {
        let metric = snapshot
            .scope_metrics()
            .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
            .find(|metric| metric.name() == name)
            .expect("actual emitted counter");
        let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
            panic!("expected unsigned counter");
        };
        sum.data_points()
            .map(|point| {
                (
                    point
                        .attributes()
                        .map(|attribute| {
                            (
                                attribute.key.as_str().to_string(),
                                attribute.value.as_str().to_string(),
                            )
                        })
                        .collect::<BTreeMap<_, _>>(),
                    point.value(),
                )
            })
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(
        counter_points("codex.windows_sandbox.elevated_setup_failure"),
        std::collections::BTreeSet::from([
            (
                BTreeMap::from([
                    ("originator".to_string(), "other".to_string()),
                    (
                        "code".to_string(),
                        "orchestrator_sandbox_dir_create_failed".to_string(),
                    ),
                ]),
                2,
            ),
            (
                BTreeMap::from([("originator".to_string(), "other".to_string())]),
                1,
            ),
        ])
    );
    assert_eq!(
        counter_points("codex.windows_sandbox.elevated_setup_canceled"),
        std::collections::BTreeSet::from([(
            BTreeMap::from([
                ("originator".to_string(), "codex_cli_rs".to_string()),
                (
                    "code".to_string(),
                    "orchestrator_helper_launch_canceled".to_string(),
                ),
            ]),
            1,
        )])
    );
    let logs = String::from_utf8(buffer.lock().expect("diagnostic log lock").clone())
        .expect("UTF-8 diagnostics");
    for message in messages {
        assert!(
            logs.contains(message),
            "diagnostic detail must remain in logs: {message}"
        );
    }
}

#[test]
fn elevated_flag_works_by_itself() {
    let mut features = Features::with_defaults();
    features.enable(Feature::WindowsSandboxElevated);

    assert_eq!(
        WindowsSandboxLevel::from_features(&features),
        WindowsSandboxLevel::Elevated
    );
}

#[test]
fn restricted_token_flag_works_by_itself() {
    let mut features = Features::with_defaults();
    features.enable(Feature::WindowsSandbox);

    assert_eq!(
        WindowsSandboxLevel::from_features(&features),
        WindowsSandboxLevel::RestrictedToken
    );
}

#[test]
fn no_flags_means_no_sandbox() {
    let features = Features::with_defaults();

    assert_eq!(
        WindowsSandboxLevel::from_features(&features),
        WindowsSandboxLevel::Disabled
    );
}

#[test]
fn elevated_wins_when_both_flags_are_enabled() {
    let mut features = Features::with_defaults();
    features.enable(Feature::WindowsSandbox);
    features.enable(Feature::WindowsSandboxElevated);

    assert_eq!(
        WindowsSandboxLevel::from_features(&features),
        WindowsSandboxLevel::Elevated
    );
}

#[test]
fn legacy_mode_prefers_elevated() {
    let mut entries = BTreeMap::new();
    entries.insert(
        "experimental_windows_sandbox".to_string(),
        /*value*/ true,
    );
    entries.insert("elevated_windows_sandbox".to_string(), /*value*/ true);

    assert_eq!(
        legacy_windows_sandbox_mode_from_entries(&entries),
        Some(WindowsSandboxModeToml::Elevated)
    );
}

#[test]
fn legacy_mode_supports_alias_key() {
    let mut entries = BTreeMap::new();
    entries.insert(
        "enable_experimental_windows_sandbox".to_string(),
        /*value*/ true,
    );

    assert_eq!(
        legacy_windows_sandbox_mode_from_entries(&entries),
        Some(WindowsSandboxModeToml::Unelevated)
    );
}

#[test]
fn resolve_windows_sandbox_mode_falls_back_to_legacy_keys() {
    let mut entries = BTreeMap::new();
    entries.insert(
        "experimental_windows_sandbox".to_string(),
        /*value*/ true,
    );
    let cfg = ConfigToml {
        features: Some(FeaturesToml::from(entries)),
        ..Default::default()
    };

    assert_eq!(
        resolve_windows_sandbox_mode(&cfg),
        Some(WindowsSandboxModeToml::Unelevated)
    );
}

#[test]
fn resolve_windows_sandbox_private_desktop_defaults_to_true() {
    assert!(resolve_windows_sandbox_private_desktop(
        &ConfigToml::default()
    ));
}

#[test]
fn resolve_windows_sandbox_private_desktop_respects_explicit_cfg_value() {
    let cfg = ConfigToml {
        windows: Some(WindowsToml {
            sandbox_private_desktop: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };

    assert!(!resolve_windows_sandbox_private_desktop(&cfg));
}
