use super::*;
use crate::terminal_palette::rgb_color;
use pretty_assertions::assert_eq;
use ratatui::style::Modifier;

#[test]
fn current_palette_uses_effective_terminal_color_level() {
    let theme =
        crate::render::highlight::resolve_theme_by_name("dracula", /*codex_home*/ None).unwrap();
    crate::render::highlight::set_syntax_theme(theme);
    crate::terminal_palette::with_test_default_colors(
        crate::terminal_probe::DefaultColors {
            fg: (240, 240, 240),
            bg: (0, 0, 0),
        },
        || {
            let palette = TokenActivityPalette::current();
            assert!(palette.uses_color);
            let expected = TokenActivityPalette::from_parts(
                Some((240, 240, 240)),
                Some((0, 0, 0)),
                StdoutColorLevel::TrueColor,
                theme_activity_style(),
            );
            for level in 0..5 {
                assert_eq!(palette.for_level(level), expected.for_level(level));
                assert_eq!(palette.glyph(TokenActivityView::Daily, level), "■");
            }
        },
    );
}

#[test]
fn truecolor_palette_blends_theme_accent_against_dark_background() {
    let default_fg = Some((240, 240, 240));
    let default_bg = Some((0, 0, 0));
    let active_style = Style::default().fg(rgb_color((100, 200, 50))).bold();
    let palette = TokenActivityPalette::from_parts(
        default_fg,
        default_bg,
        StdoutColorLevel::TrueColor,
        active_style,
    );

    assert_eq!(
        palette.for_level(/*level*/ 0).fg,
        Some(rgb_color((33, 33, 33)))
    );
    assert_eq!(
        palette.for_level(/*level*/ 1).fg,
        Some(rgb_color((22, 44, 11)))
    );
    assert_eq!(
        palette.for_level(/*level*/ 4).fg,
        Some(rgb_color((100, 200, 50)))
    );
    assert_eq!(
        palette.for_bar_level(/*level*/ 4).fg,
        Some(rgb_color((78, 156, 39)))
    );
    assert!(palette.uses_color);
}

#[test]
fn truecolor_palette_blends_empty_cell_for_light_background() {
    let default_fg = Some((0, 0, 0));
    let default_bg = Some((255, 255, 255));
    let active_style = Style::default().fg(rgb_color((0, 95, 135))).bold();
    let palette = TokenActivityPalette::from_parts(
        default_fg,
        default_bg,
        StdoutColorLevel::TrueColor,
        active_style,
    );

    assert_eq!(
        palette.for_level(/*level*/ 0).fg,
        Some(rgb_color((209, 209, 209)))
    );
    assert_eq!(
        palette.for_level(/*level*/ 4).fg,
        Some(rgb_color((0, 95, 135)))
    );
    assert!(palette.uses_color);
}

#[test]
fn unsupported_colors_preserve_theme_accent_and_distinct_empty_cells() {
    let fg = Some((240, 240, 240));
    let bg = Some((0, 0, 0));
    let rgb = rgb_color((100, 200, 50));
    for (fg, bg, color_level, accent) in [
        (fg, bg, StdoutColorLevel::Ansi16, Color::Magenta),
        (fg, bg, StdoutColorLevel::TrueColor, Color::Cyan),
        (None, bg, StdoutColorLevel::TrueColor, Color::Blue),
        // RGB accents isolate the color-depth and missing-default branches:
        // non-RGB accents would fall back before either condition is tested.
        (fg, bg, StdoutColorLevel::Ansi16, rgb),
        (fg, bg, StdoutColorLevel::Unknown, rgb),
        (None, bg, StdoutColorLevel::TrueColor, rgb),
        (fg, None, StdoutColorLevel::TrueColor, rgb),
    ] {
        let active_style = Style::default().fg(accent).bold();
        let palette = TokenActivityPalette::from_parts(fg, bg, color_level, active_style);
        assert!(!palette.uses_color);
        assert_eq!(palette.for_level(0), Style::default().dim());
        assert_eq!(palette.glyph(TokenActivityView::Daily, 0), "□");
        for level in 1..=4 {
            assert_eq!(palette.for_level(level), active_style);
            assert_eq!(palette.for_bar_level(level), active_style);
            assert!(palette.for_level(level).add_modifier.contains(Modifier::BOLD));
            assert_eq!(palette.glyph(TokenActivityView::Daily, level), "■");
        }
    }
}
