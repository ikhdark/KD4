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

