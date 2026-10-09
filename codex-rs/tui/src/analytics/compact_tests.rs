//! Large report totals and axes stay compact while selected-day details retain precision.
use super::*;

#[test]
fn report_headlines_and_axes_compact_large_values() {
    for (section, unit, total, headline, detail) in [
        (
            Section::Usage,
            models::AccountAnalyticsUnit::Tokens,
            12_280_365_226.0,
            "12.3B tokens",
            "12,280,365,226",
        ),
        (
            Section::Plugins,
            models::AccountAnalyticsUnit::Count,
            9_749.0,
            "9.7K calls",
            "9,749",
        ),
        (
            Section::Skills,
            models::AccountAnalyticsUnit::Count,
            2_901.0,
            "2.9K uses",
            "2,901",
        ),
    ] {
        let mut view = fixture::view(models::AccountKind::Enterprise);
        view.section = section;
        view.sections[section].history = Load::Ready(models::AccountAnalyticsHistory {
            unit,
            updated_at: None,
            data: vec![models::AccountAnalyticsDay {
                date: view.end_date,
                total,
                values: vec![models::AccountAnalyticsValue {
                    key: "test".into(),
                    label: "Example".into(),
                    value: total,
                }],
            }],
        });
        let rendered = screen(&mut view, /*width*/ 100, /*height*/ 30);
        assert!(rendered.contains(headline));
        // The selected-day total must retain every input digit; do not derive
        // this oracle with the same formatter used by the renderer.
        assert!(rendered.contains(detail));
    }
}
