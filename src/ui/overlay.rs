use crate::ui::theme::Theme;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

pub(crate) fn centered(area: Rect, width_pct: u16, height: u16) -> Rect {
    let width = (area.width.saturating_mul(width_pct.min(100)) / 100).min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - height) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// Every screen's `?` overlay renders through this so the chrome
/// (border, title, key/description coloring) stays identical everywhere.
pub(crate) fn draw_help(f: &mut Frame, area: Rect, entries: &[(&str, &str)], theme: &Theme) {
    let height = entries.len() as u16 + 2; // + top/bottom border
    let rect = centered(area, 60, height);
    f.render_widget(Clear, rect);
    let lines: Vec<Line> = entries
        .iter()
        .map(|(key, desc)| {
            Line::from(vec![
                Span::styled(format!("{key:<8}"), theme.accent_style()),
                Span::styled((*desc).to_string(), theme.dim_style()),
            ])
        })
        .collect();
    let block = Block::bordered()
        .title("help — any key closes")
        .border_style(theme.dim_style());
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centered_sizes_and_centers_within_area() {
        let area = Rect::new(0, 0, 100, 40);
        let r = centered(area, 60, 5);
        assert_eq!(r.width, 60);
        assert_eq!(r.height, 5);
        assert_eq!(r.x, 20);
        assert_eq!(r.y, 17);
    }

    #[test]
    fn centered_clamps_to_area_bounds() {
        let area = Rect::new(0, 0, 10, 3);
        let r = centered(area, 60, 20);
        assert!(r.width <= area.width);
        assert!(r.height <= area.height);
    }

    #[test]
    fn draw_help_renders_every_entry() {
        let theme = Theme::default();
        let entries: &[(&str, &str)] = &[("q", "quit"), ("enter", "open")];
        let text =
            crate::ui::app::render_to_text(80, 24, |f| draw_help(f, f.area(), entries, &theme));
        assert!(text.contains("help — any key closes"));
        assert!(text.contains("quit"));
        assert!(text.contains("open"));
    }
}
