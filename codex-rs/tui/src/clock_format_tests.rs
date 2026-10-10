//! Native pattern parsing without changing host settings.

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn native_patterns_ignore_literals_and_use_the_first_alternative() {
    for (pattern, symbols, expected) in [
        ("h:mm tt", "hH", Some(ClockFormat::TwelveHour)),
        ("HH:mm", "hH", Some(ClockFormat::TwentyFourHour)),
        ("h 'H; o''clock' a", "hHKk", Some(ClockFormat::TwelveHour)),
        ("'h' HH:mm;h:mm tt", "hH", Some(ClockFormat::TwentyFourHour)),
        ("h:mm tt;HH:mm", "hH", Some(ClockFormat::TwelveHour)),
        ("K a", "hHKk", Some(ClockFormat::TwelveHour)),
        ("kk", "hHKk", Some(ClockFormat::TwentyFourHour)),
        ("K HH:mm", "hH", Some(ClockFormat::TwentyFourHour)),
        ("h 'unterminated", "hH", None),
        ("h HH", "hH", None),
        ("'HH' mm", "hH", None),
        ("", "hH", None),
    ] {
        assert_eq!(
            parse_native_pattern(pattern, symbols),
            expected,
            "{pattern}"
        );
    }
}
