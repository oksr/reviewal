//! The editor spawn is safe only because run_tui polls input on the same
//! main loop that drains the staged request — nothing else reads stdin while
//! the editor runs. If input polling ever moves to a background thread, park
//! it around the spawn or the TUI and the editor will fight over keystrokes.

use ratatui::crossterm::{cursor, execute, terminal};
use std::path::Path;

pub(crate) fn resolve_editor() -> String {
    let visual = std::env::var("VISUAL").ok();
    let editor = std::env::var("EDITOR").ok();
    resolve_editor_from(visual.as_deref(), editor.as_deref())
}

/// Pure resolution core, split from the env read so tests never mutate
/// process-global env (racy under the parallel test runner).
pub(crate) fn resolve_editor_from(visual: Option<&str>, editor: Option<&str>) -> String {
    [visual, editor]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("vi")
        .to_string()
}

pub(crate) enum EditorExit {
    Clean,
    Failed,
    /// `sh` exit 127: the editor command was not found on PATH.
    NotFound,
}

/// Runs `editor_cmd` on `path` with inherited stdio, via `sh -c` so
/// commands with arguments ("code --wait") work. Blocks until exit.
pub(crate) fn run_editor(editor_cmd: &str, path: &Path) -> std::io::Result<EditorExit> {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor_cmd} \"$0\""))
        .arg(path)
        .status()?;
    Ok(match status.code() {
        Some(0) => EditorExit::Clean,
        Some(127) => EditorExit::NotFound,
        _ => EditorExit::Failed,
    })
}

/// Inverse of run_tui's TermGuard for the editor's lifetime: cooked mode +
/// primary screen on construction, alternate screen + raw mode restored on
/// Drop — i.e. on EVERY exit path, spawn failures included.
pub(crate) struct SuspendGuard;

impl SuspendGuard {
    /// If raw mode disables cleanly but leaving the alternate screen fails,
    /// re-enables raw mode before returning the error — half-suspended
    /// (cooked mode, alternate screen still up) is worse than not suspended
    /// at all, since neither the caller nor the Drop impl runs to fix it.
    pub(crate) fn new() -> std::io::Result<SuspendGuard> {
        terminal::disable_raw_mode()?;
        if let Err(e) = execute!(
            std::io::stdout(),
            terminal::LeaveAlternateScreen,
            cursor::Show
        ) {
            let _ = terminal::enable_raw_mode();
            return Err(e);
        }
        Ok(SuspendGuard)
    }
}

impl Drop for SuspendGuard {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), terminal::EnterAlternateScreen);
        let _ = terminal::enable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_editor_precedence_visual_editor_vi() {
        assert_eq!(resolve_editor_from(Some("code -w"), Some("vim")), "code -w");
        assert_eq!(resolve_editor_from(None, Some("vim")), "vim");
        assert_eq!(
            resolve_editor_from(Some("  "), Some("vim")),
            "vim",
            "blank VISUAL skipped"
        );
        assert_eq!(resolve_editor_from(None, None), "vi");
    }

    #[test]
    fn run_editor_reports_clean_failed_and_notfound() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("file.md");
        std::fs::write(&target, "x").unwrap();

        // run_editor invokes `sh -c '<cmd> "$0"' <target>`, so the target
        // path arrives as the fake editor script's own $1.
        let ok = dir.path().join("ok.sh");
        std::fs::write(&ok, "#!/bin/sh\nprintf edited >> \"$1\"\nexit 0\n").unwrap();
        let bad = dir.path().join("bad.sh");
        std::fs::write(&bad, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for s in [&ok, &bad] {
                std::fs::set_permissions(s, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        assert!(matches!(
            run_editor(&ok.display().to_string(), &target).unwrap(),
            EditorExit::Clean
        ));
        assert!(std::fs::read_to_string(&target).unwrap().contains("edited"));
        assert!(matches!(
            run_editor(&bad.display().to_string(), &target).unwrap(),
            EditorExit::Failed
        ));
        assert!(matches!(
            run_editor("definitely-not-a-real-editor-xyz", &target).unwrap(),
            EditorExit::NotFound
        ));
    }
}
