use chrono::DateTime;
use chrono::Local;
use chrono::Utc;

pub fn format_relative_time(reference: DateTime<Utc>, ts: DateTime<Utc>) -> String {
    let secs = (reference - ts).num_seconds().max(0);
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    ts.with_timezone(&Local).format("%b %e %H:%M").to_string()
}

pub fn format_relative_time_now(ts: DateTime<Utc>) -> String {
    format_relative_time(Utc::now(), ts)
}

#[cfg(test)]
mod tests {
    use super::format_relative_time;
    use chrono::TimeDelta;
    use chrono::Utc;

    #[test]
    fn formats_relative_time_boundaries_and_future_values() {
        let reference = Utc::now();
        assert_eq!(
            format_relative_time(reference, reference + TimeDelta::seconds(1)),
            "0s ago"
        );
        assert_eq!(
            format_relative_time(reference, reference - TimeDelta::seconds(59)),
            "59s ago"
        );
        assert_eq!(
            format_relative_time(reference, reference - TimeDelta::minutes(3)),
            "3m ago"
        );
        assert_eq!(
            format_relative_time(reference, reference - TimeDelta::hours(4)),
            "4h ago"
        );
    }
}
