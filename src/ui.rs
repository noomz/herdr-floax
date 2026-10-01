//! Floating-box geometry and rendering: a dimmed backdrop, a centered rounded
//! box sized by the configured percentages, and the embedded vt100 screen
//! painted into the box interior.

use ratatui::layout::Position;
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Clear};

use crate::config::Config;

/// The floating box (border included), centered in `area` per the configured
/// percentages. Width/height are clamped so tiny panes still get a usable box.
pub fn box_rect(area: Rect, cfg: &Config) -> Rect {
    let w = ((u32::from(area.width) * u32::from(cfg.width_pct)) / 100) as u16;
    let h = ((u32::from(area.height) * u32::from(cfg.height_pct)) / 100) as u16;
    let w = w.clamp(20.min(area.width), area.width);
    let h = h.clamp(5.min(area.height), area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

/// The PTY-facing interior of the floating box (inside the 1-cell border).
/// This is what the embedded shell believes its terminal size is.
pub fn box_inner(area: Rect, cfg: &Config) -> Rect {
    let r = box_rect(area, cfg);
    Rect::new(
        r.x + 1,
        r.y + 1,
        r.width.saturating_sub(2).max(1),
        r.height.saturating_sub(2).max(1),
    )
}

/// Draw one frame: backdrop, box chrome, embedded screen, selection, cursor.
pub fn draw(f: &mut Frame, cfg: &Config, screen: &vt100::Screen, sel: &crate::mouse::Selection) {
    let area = f.area();

    // Backdrop. The app cannot composite live herdr panes behind the box
    // (herdr owns those PTYs), so it fills the area with the terminal background.
    // `Color::Reset` preserves the pane's default background, which herdr keeps
    // synchronized with the host terminal. Config may explicitly override it.
    let backdrop = cfg
        .backdrop
        .map_or(Color::Reset, |(r, g, b)| Color::Rgb(r, g, b));
    f.render_widget(
        Block::default().style(Style::default().bg(backdrop).fg(Color::DarkGray)),
        area,
    );

    let boxr = box_rect(area, cfg);
    f.render_widget(Clear, boxr);
    let chrome = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Magenta))
        .title(Line::from(" ⌂ floax ").centered())
        .title_bottom(
            Line::from(format!(" {} hides · shell persists ", cfg.key_hint))
                .centered()
                .style(Style::default().fg(Color::DarkGray)),
        );
    let inner = chrome.inner(boxr);
    f.render_widget(chrome, boxr);

    render_screen(f.buffer_mut(), inner, screen, sel);

    if !screen.hide_cursor() {
        let (r, c) = screen.cursor_position();
        if r < inner.height && c < inner.width {
            f.set_cursor_position(Position::new(inner.x + c, inner.y + r));
        }
    }
}

/// Paint the vt100 screen's cells into the buffer at `area`'s offset,
/// highlighting the active selection.
fn render_screen(buf: &mut Buffer, area: Rect, screen: &vt100::Screen, sel: &crate::mouse::Selection) {
    let (rows, cols) = screen.size();
    let span = sel.span(rows, cols);
    for r in 0..rows.min(area.height) {
        let mut skip_next = false; // second half of a wide (CJK/emoji) cell
        for c in 0..cols.min(area.width) {
            if skip_next {
                skip_next = false;
                continue;
            }
            let Some(cell) = screen.cell(r, c) else {
                continue;
            };
            let Some(target) = buf.cell_mut(Position::new(area.x + c, area.y + r)) else {
                continue;
            };
            let contents = cell.contents();
            target.set_symbol(if contents.is_empty() { " " } else { &contents });
            let mut style = Style::default()
                .fg(conv_color(cell.fgcolor()))
                .bg(conv_color(cell.bgcolor()));
            if cell.bold() {
                style = style.add_modifier(Modifier::BOLD);
            }
            if cell.italic() {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if cell.underline() {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            if cell.inverse() {
                style = style.add_modifier(Modifier::REVERSED);
            }
            if in_selection(span, r, c) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            target.set_style(style);
            skip_next = cell.is_wide();
        }
    }
}

/// Whether cell `(r, c)` falls in the selection span (middle rows of a
/// multi-line span are fully selected; only the first/last rows are
/// column-bounded).
fn in_selection(span: Option<(u16, u16, u16, u16)>, r: u16, c: u16) -> bool {
    match span {
        None => false,
        Some((r0, r1, c0, c1)) if r0 == r1 => r == r0 && c >= c0 && c <= c1,
        Some((r0, r1, c0, c1)) => {
            (r == r0 && c >= c0) || (r == r1 && c <= c1) || (r > r0 && r < r1)
        }
    }
}

fn conv_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(w: u16, h: u16) -> Config {
        Config {
            width_pct: w,
            height_pct: h,
            ..Config::default()
        }
    }

    #[test]
    fn box_is_centered_at_80pct() {
        let r = box_rect(Rect::new(0, 0, 100, 50), &cfg(80, 80));
        assert_eq!(r, Rect::new(10, 5, 80, 40));
    }

    #[test]
    fn box_respects_area_offset() {
        let r = box_rect(Rect::new(7, 3, 100, 50), &cfg(80, 80));
        assert_eq!(r, Rect::new(17, 8, 80, 40));
    }

    #[test]
    fn box_clamps_to_minimums_on_tiny_areas() {
        let r = box_rect(Rect::new(0, 0, 30, 6), &cfg(20, 20));
        assert!(r.width >= 20 && r.height >= 5);
        // ... and never exceeds the area even when the area is below minimums.
        let r = box_rect(Rect::new(0, 0, 12, 3), &cfg(80, 80));
        assert!(r.width <= 12 && r.height <= 3);
    }

    #[test]
    fn full_size_at_100pct() {
        let area = Rect::new(0, 0, 100, 50);
        assert_eq!(box_rect(area, &cfg(100, 100)), area);
    }

    #[test]
    fn inner_is_border_inset_and_never_zero() {
        let inner = box_inner(Rect::new(0, 0, 100, 50), &cfg(80, 80));
        assert_eq!(inner, Rect::new(11, 6, 78, 38));
        let tiny = box_inner(Rect::new(0, 0, 2, 2), &cfg(20, 20));
        assert!(tiny.width >= 1 && tiny.height >= 1);
    }
}
