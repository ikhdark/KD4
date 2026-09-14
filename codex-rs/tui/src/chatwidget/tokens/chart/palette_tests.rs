use super::*;
use crate::terminal_palette::rgb_color;
use pretty_assertions::assert_eq;
use ratatui::style::Modifier;

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
fn ansi16_palette_uses_theme_accent_without_green_fallback() {
    let default_fg = Some((240, 240, 240));
    let default_bg = Some((0, 0, 0));
    let active_style = Style::default().fg(Color::Magenta).bold();
    let palette = TokenActivityPalette::from_parts(
        default_fg,
        default_bg,
        StdoutColorLevel::Ansi16,
        active_style,
    );

    assert_eq!(palette.for_level(/*level*/ 0), Style::default().dim());
    assert_eq!(palette.for_level(/*level*/ 1), active_style);
    assert_eq!(palette.for_bar_level(/*level*/ 4), active_style);
    assert!(!palette.uses_color);
}

#[test]
fn non_rgb_theme_accent_remains_active_fallback() {
    let default_fg = Some((240, 240, 240));
    let default_bg = Some((0, 0, 0));
    let active_style = Style::default().fg(Color::Cyan).bold();
    let palette = TokenActivityPalette::from_parts(
        default_fg,
        default_bg,
        StdoutColorLevel::TrueColor,
        active_style,
    );

    assert_eq!(palette.for_level(/*level*/ 1), active_style);
    assert!(
        palette
            .for_level(/*level*/ 1)
            .add_modifier
            .contains(Modifier::BOLD)
    );
    assert!(!palette.uses_color);
}

#[test]
fn missing_terminal_colors_use_theme_accent_fallback() {
    let default_fg = None;
    let default_bg = Some((0, 0, 0));
    let active_style = Style::default().fg(Color::Blue).bold();
    let palette = TokenActivityPalette::from_parts(
        default_fg,
        default_bg,
        StdoutColorLevel::TrueColor,
        active_style,
    );

    assert_eq!(palette.for_level(/*level*/ 0), Style::default().dim());
    assert_eq!(palette.for_level(/*level*/ 4), active_style);
    assert!(!palette.uses_color);
}

#[test]
fn fallback_quantizes_rgb_and_preserves_glyphs_and_modifiers() {
    for color_level in [
        StdoutColorLevel::Ansi16,
        StdoutColorLevel::Unknown,
        StdoutColorLevel::Ansi256,
    ] {
        let palette = TokenActivityPalette::from_parts(
            None,
            None,
            color_level,
            Style::default().fg(rgb_color((100, 200, 50))).bold(),
        );
        assert!(!matches!(palette.for_level(1).fg, Some(Color::Rgb(..))));
        assert!(palette.for_level(1).add_modifier.contains(Modifier::BOLD));
        assert_eq!(palette.glyph(TokenActivityView::Daily, 0), "□");
        assert_eq!(palette.glyph(TokenActivityView::Daily, 1), "■");
        for view in [TokenActivityView::Weekly, TokenActivityView::Cumulative] {
            assert_eq!(palette.glyph(view, 0), " ");
            assert_eq!(palette.glyph(view, 4), "█");
        }
    }
    let palette = TokenActivityPalette::from_parts(
        Some((240, 240, 240)),
        Some((0, 0, 0)),
        StdoutColorLevel::TrueColor,
        Style::default().fg(rgb_color((100, 200, 50))),
    );
    for level in 0..=4 {
        assert_eq!(palette.glyph(TokenActivityView::Daily, level), "■");
    }
}
