use std::time::Duration;

pub(crate) fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis() as i64;
    if millis < 1000 {
        format!("{millis}ms")
    } else if millis < 60_000 {
        format!("{:.2}s", millis as f64 / 1000.0)
    } else {
        let minutes = millis / 60_000;
        let seconds = (millis % 60_000) / 1000;
        format!("{minutes}m {seconds:02}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_each_duration_range() {
        assert_eq!(format_duration(Duration::from_millis(250)), "250ms");
        assert_eq!(format_duration(Duration::from_millis(1_500)), "1.50s");
        assert_eq!(format_duration(Duration::from_millis(75_000)), "1m 15s");
        assert_eq!(format_duration(Duration::from_millis(3_600_000)), "60m 00s");
    }
}
