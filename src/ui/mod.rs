pub mod app;
pub mod composer;
pub mod dashboard;
pub mod done;
pub(crate) mod editor;
pub mod format;
pub mod home;
pub mod overlay;
pub mod theme;
pub mod triage;

use app::{App, Bootstrap, Screen};
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::crossterm::{cursor, execute, terminal};
use std::time::Duration;

struct TermGuard;

impl TermGuard {
    fn new() -> anyhow::Result<TermGuard> {
        terminal::enable_raw_mode()?;
        execute!(std::io::stdout(), terminal::EnterAlternateScreen)?;
        Ok(TermGuard)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        // The TUI hides the cursor while it runs; re-show it AFTER leaving
        // the alternate screen so the Show lands on the primary screen the
        // shell prompt returns to, then drop raw mode.
        let _ = execute!(std::io::stdout(), terminal::LeaveAlternateScreen);
        let _ = execute!(std::io::stdout(), cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
}

fn draw_app(f: &mut ratatui::Frame, app: &App) {
    let area = f.area().inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    match &app.screen {
        Screen::Home(state) => home::draw(
            f,
            area,
            state,
            &app.claude_check,
            app.config.model.as_deref().unwrap_or("default"),
            &app.theme,
        ),
        Screen::Dashboard(state) => dashboard::draw(f, area, state, &app.theme),
        Screen::Triage(state) => triage::draw(f, area, state, &app.theme),
        Screen::Done(state) => done::draw(f, area, state, &app.theme),
        Screen::Composer(state) => composer::draw(f, area, state, &app.theme),
    }
    if let Some(line) = &app.status_line {
        let bottom = ratatui::layout::Rect {
            y: area.bottom().saturating_sub(2),
            height: 1,
            ..area
        };
        // One row above the screen's own hint line, not on top of it, so the
        // hints stay visible while a status notice shows. `Clear` guards
        // against a longer previous status line's tail bleeding through.
        f.render_widget(ratatui::widgets::Clear, bottom);
        f.render_widget(
            ratatui::widgets::Paragraph::new(line.as_str())
                .style(ratatui::style::Style::default().fg(app.theme.error)),
            bottom,
        );
    }
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), terminal::LeaveAlternateScreen);
        let _ = execute!(std::io::stdout(), cursor::Show);
        let _ = terminal::disable_raw_mode();
        original(info);
    }));
}

pub fn run_tui(
    root: std::path::PathBuf,
    config: crate::config::Config,
    bootstrap: Bootstrap,
) -> anyhow::Result<()> {
    install_panic_hook();
    let _guard = TermGuard::new()?;
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
    let mut app = App::try_new(root, config, bootstrap)?;

    while !app.should_quit {
        app.poll_preflight();
        terminal.draw(|f| draw_app(f, &app))?;
        while let Some(ev) = app.engine.try_recv() {
            app.handle_run_event(ev);
        }
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    app.handle_key(k);
                }
            }
        }
        // Staged editor requests must run here, between draws — never mid-draw.
        if let Screen::Composer(state) = &mut app.screen {
            if let Some(req) = state.pending_editor.take() {
                let cmd = editor::resolve_editor();
                let outcome = match editor::SuspendGuard::new() {
                    Ok(_suspended) => editor::run_editor(&cmd, &req.path)
                        .map_err(|e| format!("failed to run {cmd}: {e}")),
                    Err(e) => Err(format!("terminal suspend failed: {e}")),
                }; // _suspended drops here: terminal restored on every path
                terminal.clear()?; // force a full repaint after the editor
                match outcome {
                    Ok(editor::EditorExit::Clean) => state.on_editor_return(req, true),
                    Ok(editor::EditorExit::Failed) => state.on_editor_return(req, false),
                    Ok(editor::EditorExit::NotFound) => {
                        state.on_editor_return(req, false); // created files cleaned up
                        state.notice = Some("no editor found \u{2014} set $EDITOR".into());
                    }
                    Err(msg) => {
                        state.on_editor_return(req, false);
                        state.notice = Some(msg);
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_keys {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    pub(crate) fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    pub(crate) fn key_code(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use app::render_to_text;

    #[test]
    fn status_line_replaces_hints_row_completely() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");
        app.status_line = Some("run not resumable (stale/aborted)".into());
        let buffer = crate::ui::app::render_to_buffer(80, 20, |f| draw_app(f, &app));
        // row 17 (= height 20 − 1 margin − 2): status text, error-colored
        let mut status_row = String::new();
        for x in 0..buffer.area.width {
            status_row.push_str(buffer[(x, 20 - 3)].symbol());
        }
        assert!(
            status_row.contains("run not resumable"),
            "row: {status_row:?}"
        );
        let x = status_row.find("run").unwrap() as u16;
        assert_eq!(
            buffer[(x, 20 - 3)].style().fg,
            Some(ratatui::style::Color::Red),
            "status line carries the error color"
        );
        // row 18 (= height 20 − 1 margin − 1): the screen's own hint line, still visible
        let mut hint_row = String::new();
        for x in 0..buffer.area.width {
            hint_row.push_str(buffer[(x, 20 - 2)].symbol());
        }
        assert!(
            hint_row.contains("quit"),
            "hint row must remain visible under the status line: {hint_row:?}"
        );
    }

    #[test]
    fn draw_app_pads_screen_edges() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");
        app.status_line = Some("status".into());
        let text = render_to_text(80, 20, |f| draw_app(f, &app));
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].trim().is_empty(), "top row blank");
        assert!(lines[19].trim().is_empty(), "bottom row blank");
        for (i, line) in lines.iter().enumerate() {
            assert!(
                line.chars().take(2).all(|c| c == ' '),
                "left 2 cols blank on row {i}: {line:?}"
            );
            assert!(
                line.chars().rev().take(2).all(|c| c == ' '),
                "right 2 cols blank on row {i}: {line:?}"
            );
        }
        let status_row = lines
            .iter()
            .position(|l| l.contains("status"))
            .expect("status line rendered");
        assert_eq!(
            status_row,
            20 - 3,
            "status line sits one row above the padded bottom (hint) row"
        );
    }
}
