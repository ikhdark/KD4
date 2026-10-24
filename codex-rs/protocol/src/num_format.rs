use std::sync::OnceLock;

use icu_decimal::DecimalFormatter;
use icu_decimal::input::Decimal;
use icu_decimal::options::DecimalFormatterOptions;
use icu_locale_core::Locale;

fn make_local_formatter() -> Option<DecimalFormatter> {
    let loc: Locale = sys_locale::get_locale()?.parse().ok()?;
    DecimalFormatter::try_new(loc.into(), DecimalFormatterOptions::default()).ok()
}

fn make_en_us_formatter() -> DecimalFormatter {
    #![allow(clippy::expect_used)]
    let loc: Locale = "en-US".parse().expect("en-US wasn't a valid locale");
    DecimalFormatter::try_new(loc.into(), DecimalFormatterOptions::default())
        .expect("en-US wasn't a valid locale")
}

fn formatter() -> &'static DecimalFormatter {
    static FORMATTER: OnceLock<DecimalFormatter> = OnceLock::new();
    FORMATTER.get_or_init(|| make_local_formatter().unwrap_or_else(make_en_us_formatter))
}

/// Format an i64 with locale-aware digit separators (e.g. "12345" -> "12,345"
/// for en-US).
pub fn format_with_separators(n: i64) -> String {
    formatter().format(&Decimal::from(n)).to_string()
}

fn format_with_separators_with_formatter(n: i64, formatter: &DecimalFormatter) -> String {
    formatter.format(&Decimal::from(n)).to_string()
}

fn format_si_suffix_with_formatter(n: i64, formatter: &DecimalFormatter) -> String {
    let n = n.max(0);
    if n < 1000 {
        return formatter.format(&Decimal::from(n)).to_string();
    }

    // Format `n / scale` with the requested number of fractional digits.
    // Keep decimal half boundaries exact, including at i64::MAX.
    let rounded_scaled = |n: i64, scale: i64, frac_digits: u32| -> i64 {
        let numerator = i128::from(n) * 10_i128.pow(frac_digits);
        ((numerator + i128::from(scale) / 2) / i128::from(scale)) as i64
    };
    let format_scaled = |n: i64, scale: i64, frac_digits: u32| -> String {
        let scaled = rounded_scaled(n, scale, frac_digits);
        let mut dec = Decimal::from(scaled);
        dec.multiply_pow10(-(frac_digits as i16));
        formatter.format(&dec).to_string()
    };

    const UNITS: [(i64, &str); 3] = [(1_000, "K"), (1_000_000, "M"), (1_000_000_000, "G")];
    for &(scale, suffix) in &UNITS {
        if rounded_scaled(n, scale, 2) < 1000 {
            return format!("{}{}", format_scaled(n, scale, 2), suffix);
        } else if rounded_scaled(n, scale, 1) < 1000 {
            return format!("{}{}", format_scaled(n, scale, 1), suffix);
        } else if rounded_scaled(n, scale, 0) < 1000 {
            return format!("{}{}", format_scaled(n, scale, 0), suffix);
        }
    }

    // Above 1000G, keep whole‑G precision.
    format!(
        "{}G",
        format_with_separators_with_formatter(rounded_scaled(n, 1_000_000_000, 0), formatter)
    )
}

/// Format token counts to 3 significant figures, using base-10 SI suffixes.
///
/// Examples (en-US):
///   - 999 -> "999"
///   - 1200 -> "1.20K"
///   - 123456789 -> "123M"
pub fn format_si_suffix(n: i64) -> String {
    format_si_suffix_with_formatter(n, formatter())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kmg() {
        let formatter = make_en_us_formatter();
        let fmt = |n: i64| format_si_suffix_with_formatter(n, &formatter);
        assert_eq!(fmt(0), "0");
        assert_eq!(fmt(999), "999");
        assert_eq!(fmt(1_000), "1.00K");
        assert_eq!(fmt(1_200), "1.20K");
        assert_eq!(fmt(10_000), "10.0K");
        assert_eq!(fmt(100_000), "100K");
        assert_eq!(fmt(999_500), "1.00M");
        assert_eq!(fmt(1_000_000), "1.00M");
        assert_eq!(fmt(1_234_000), "1.23M");
        assert_eq!(fmt(12_345_678), "12.3M");
        assert_eq!(fmt(999_950_000), "1.00G");
        assert_eq!(fmt(1_000_000_000), "1.00G");
        assert_eq!(fmt(1_234_000_000), "1.23G");
        // Above 1000G we keep whole‑G precision (no higher unit supported here).
        assert_eq!(fmt(1_234_000_000_000), "1,234G");
    }

    #[test]
    fn decimal_halves_round_up_without_binary_float_error() {
        let formatter = make_en_us_formatter();
        for (n, expected) in [
            (1_004, "1.00K"),
            (1_005, "1.01K"),
            (1_005_000, "1.01M"),
            (1_005_000_000, "1.01G"),
            (10_050, "10.1K"),
            (10_050_000, "10.1M"),
            (10_050_000_000, "10.1G"),
            (999_499, "999K"),
            (999_500, "1.00M"),
            (999_500_000, "1.00G"),
            (i64::MIN, "0"),
            (i64::MAX, "9,223,372,037G"),
        ] {
            assert_eq!(format_si_suffix_with_formatter(n, &formatter), expected);
        }
    }
}
