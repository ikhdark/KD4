use crate::harness::attributes_to_map;
use crate::harness::build_metrics_with_defaults;
use crate::harness::histogram_data;
use crate::harness::latest_metrics;
use codex_otel::Result;
use pretty_assertions::assert_eq;
use std::time::Duration;

// Ensures duration recording maps to histogram output.
#[test]
fn record_duration_records_histogram() -> Result<()> {
    let (metrics, exporter) = build_metrics_with_defaults(&[])?;

    metrics.record_duration(
        "codex.request_latency",
        Duration::from_millis(15),
        &[("route", "chat")],
    )?;
    metrics.shutdown()?;

    let resource_metrics = latest_metrics(&exporter);
    let (bounds, bucket_counts, sum, count) =
        histogram_data(&resource_metrics, "codex.request_latency");
    assert!(!bounds.is_empty());
    assert_eq!(bucket_counts.iter().sum::<u64>(), 1);
    assert_eq!(sum, 15.0);
    assert_eq!(count, 1);
    let metric = crate::harness::find_metric(&resource_metrics, "codex.request_latency")
        .expect("codex.request_latency metric should exist");
    assert_eq!(metric.unit(), "ms");
    assert_eq!(metric.description(), "Duration in milliseconds.");

    Ok(())
}

#[test]
fn record_duration_seconds_uses_fractional_seconds_and_scaled_buckets() -> Result<()> {
    let (metrics, exporter) = build_metrics_with_defaults(&[])?;

    for duration in [
        Duration::from_millis(200),
        Duration::from_secs(1),
        Duration::from_millis(4900),
    ] {
        metrics.record_duration_seconds_with_description(
            "codex.request_duration_seconds",
            "Duration of Codex requests in seconds.",
            duration,
            &[("method", "initialize")],
        )?;
    }
    metrics.shutdown()?;

    let resource_metrics = latest_metrics(&exporter);
    let (bounds, bucket_counts, sum, count) =
        histogram_data(&resource_metrics, "codex.request_duration_seconds");
    assert_eq!(
        bounds,
        vec![
            0.0, 0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
        ]
    );
    assert_eq!(
        bucket_counts,
        vec![0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 1, 0, 0, 0]
    );
    assert!((sum - 6.1).abs() < f64::EPSILON * 8.0);
    assert_eq!(count, 3);
    let metric = crate::harness::find_metric(&resource_metrics, "codex.request_duration_seconds")
        .expect("codex.request_duration_seconds metric should exist");
    assert_eq!(metric.unit(), "s");
    assert_eq!(
        metric.description(),
        "Duration of Codex requests in seconds."
    );

    Ok(())
}

// Dropping an unfinished timer records its duration and original tags.
#[test]
fn timer_result_records_success() -> Result<()> {
    let (metrics, exporter) = build_metrics_with_defaults(&[])?;

    {
        let timer = metrics.start_timer("codex.request_latency", &[("route", "chat")]);
        assert!(timer.is_ok());
    }

    metrics.shutdown()?;

    let resource_metrics = latest_metrics(&exporter);
    let (bounds, bucket_counts, _sum, count) =
        histogram_data(&resource_metrics, "codex.request_latency");
    assert!(!bounds.is_empty());
    assert_eq!(count, 1);
    assert_eq!(bucket_counts.iter().sum::<u64>(), 1);
    let metric = crate::harness::find_metric(&resource_metrics, "codex.request_latency")
        .expect("codex.request_latency metric should exist");
    assert_eq!(metric.unit(), "ms");
    assert_eq!(metric.description(), "Duration in milliseconds.");
    let attrs = attributes_to_map(
        crate::harness::find_metric(&resource_metrics, "codex.request_latency")
            .and_then(|metric| match metric.data() {
                opentelemetry_sdk::metrics::data::AggregatedMetrics::F64(
                    opentelemetry_sdk::metrics::data::MetricData::Histogram(histogram),
                ) => histogram
                    .data_points()
                    .next()
                    .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::attributes),
                _ => None,
            })
            .expect("codex.request_latency attributes should exist"),
    );
    assert_eq!(attrs.get("route").map(String::as_str), Some("chat"));

    Ok(())
}

#[test]
fn fractional_milliseconds_survive_both_recording_paths() -> Result<()> {
    let (metrics, exporter) = build_metrics_with_defaults(&[])?;
    metrics.record_duration("codex.fractional", Duration::from_micros(400), &[])?;
    metrics.record_count_and_duration(
        "codex.count",
        "codex.combined",
        1,
        Duration::from_micros(400),
        &[],
    )?;
    metrics.shutdown()?;
    let snapshot = latest_metrics(&exporter);
    for name in ["codex.fractional", "codex.combined"] {
        let (_, _, sum, count) = histogram_data(&snapshot, name);
        assert!((sum - 0.4).abs() < 1e-12);
        assert_eq!(count, 1);
    }
    Ok(())
}

#[test]
fn timer_finish_records_once_with_completion_tags() -> Result<()> {
    let (metrics, exporter) = build_metrics_with_defaults(&[])?;
    metrics
        .start_timer("codex.finished", &[("status", "running")])?
        .finish(&[("status", "success")])?;
    metrics.shutdown()?;
    let snapshot = latest_metrics(&exporter);
    let (_, _, _, count) = histogram_data(&snapshot, "codex.finished");
    assert_eq!(count, 1);
    let metric = crate::harness::find_metric(&snapshot, "codex.finished").unwrap();
    let opentelemetry_sdk::metrics::data::AggregatedMetrics::F64(
        opentelemetry_sdk::metrics::data::MetricData::Histogram(histogram),
    ) = metric.data()
    else {
        panic!("histogram")
    };
    let attrs = attributes_to_map(histogram.data_points().next().unwrap().attributes());
    assert_eq!(attrs.get("status").map(String::as_str), Some("success"));
    Ok(())
}

#[test]
fn failed_timer_finish_does_not_record_on_drop() -> Result<()> {
    let (metrics, exporter) = build_metrics_with_defaults(&[])?;
    assert!(matches!(
        metrics
            .start_timer("codex.failed", &[])?
            .finish(&[("bad key", "value")]),
        Err(codex_otel::MetricsError::InvalidTagComponent { .. })
    ));
    metrics.counter("codex.sentinel", 1, &[])?;
    metrics.shutdown()?;
    assert!(crate::harness::find_metric(&latest_metrics(&exporter), "codex.failed").is_none());
    Ok(())
}

#[test]
fn timer_checkpoint_preserves_drop_recording() -> Result<()> {
    let (metrics, exporter) = build_metrics_with_defaults(&[])?;
    {
        let timer = metrics.start_timer("codex.checkpoint", &[])?;
        timer.record(&[])?;
    }
    metrics.shutdown()?;
    assert_eq!(
        histogram_data(&latest_metrics(&exporter), "codex.checkpoint").3,
        2
    );
    Ok(())
}
