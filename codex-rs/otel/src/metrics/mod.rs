mod client;
mod config;
mod error;
pub(crate) mod names;
mod process;
pub(crate) mod runtime_metrics;
pub(crate) mod tags;
pub(crate) mod timer;
pub(crate) mod validation;

use crate::config::StatsigMetricsSettings;
pub use crate::metrics::client::MetricsClient;
pub use crate::metrics::config::MetricsConfig;
pub use crate::metrics::config::MetricsExporter;
pub use crate::metrics::error::MetricsError;
pub use crate::metrics::error::Result;
pub use crate::metrics::process::record_process_start_once;
pub use names::*;
use std::sync::RwLock;
pub use tags::ORIGINATOR_TAG;
pub use tags::SessionMetricTagValues;
pub use tags::bounded_originator_tag_value;

struct GlobalMetrics {
    metrics: MetricsClient,
    statsig_settings: Option<StatsigMetricsSettings>,
}
static GLOBAL_METRICS: RwLock<Option<GlobalMetrics>> = RwLock::new(None);

pub(crate) fn install_global(
    metrics: MetricsClient,
    statsig_settings: Option<StatsigMetricsSettings>,
) {
    let displaced = GLOBAL_METRICS
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .replace(GlobalMetrics {
            metrics,
            statsig_settings,
        });
    // Provider destruction must never run under the publication lock.
    drop(displaced);
}

pub fn global() -> Option<MetricsClient> {
    GLOBAL_METRICS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(|state| state.metrics.clone())
}

pub(crate) fn uninstall_global(metrics: &MetricsClient) {
    let removed = {
        let mut state = GLOBAL_METRICS
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .as_ref()
            .is_some_and(|state| state.metrics.same_client(metrics))
        {
            state.take()
        } else {
            None
        }
    };
    drop(removed);
}

pub(crate) fn global_statsig_settings() -> Option<StatsigMetricsSettings> {
    GLOBAL_METRICS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .and_then(|state| state.statsig_settings.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_replacement_tracks_metadata_and_shutdown_owner() {
        const CHILD: &str = "KD4_GLOBAL_METRICS_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            assert!(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "metrics::tests::global_replacement_tracks_metadata_and_shutdown_owner",
                        "--nocapture"
                    ])
                    .env(CHILD, "1")
                    .status()
                    .unwrap()
                    .success()
            );
            return;
        }
        let client = || {
            MetricsClient::new(
                MetricsConfig::in_memory(
                    "test",
                    "global-test",
                    "1",
                    opentelemetry_sdk::metrics::InMemoryMetricExporter::default(),
                )
                .with_runtime_reader(),
            )
            .unwrap()
        };
        let a = client();
        let b = client();
        let c = client();
        let owner = |metrics| crate::OtelProvider {
            logger: None,
            tracer_provider: None,
            tracer: None,
            metrics: Some(metrics),
        };
        let owner_a = owner(a.clone());
        let owner_b = owner(b.clone());
        let owner_c = owner(c.clone());
        install_global(a, None);
        assert_eq!(global_statsig_settings(), None);
        let settings = StatsigMetricsSettings {
            environment: "b".into(),
        };
        install_global(b.clone(), Some(settings.clone()));
        assert_eq!(global_statsig_settings(), Some(settings));
        drop(owner_a);
        assert!(global().unwrap().same_client(&b));
        global()
            .unwrap()
            .counter("codex.global_install_test", 1, &[])
            .unwrap();
        assert!(b.snapshot().is_ok());
        install_global(c.clone(), None);
        drop(owner_b);
        assert_eq!(global_statsig_settings(), None);
        assert!(global().unwrap().same_client(&c));
        global()
            .unwrap()
            .counter("codex.global_install_test", 1, &[])
            .unwrap();
        assert!(c.snapshot().is_ok());
        drop(owner_c);
        assert!(global().is_none());
        assert_eq!(global_statsig_settings(), None);
    }
}
