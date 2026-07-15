use crate::engine::target::{precheck_diff, Target, TargetError};
use std::path::{Path, PathBuf};
use std::process::Command;

const REQUIRED_FLAGS: [&str; 3] = ["--json-schema", "--safe-mode", "--tools"];

#[derive(Debug, thiserror::Error)]
pub enum PreflightError {
    #[error("claude CLI not found on PATH — install Claude Code first")]
    ClaudeNotFound,
    #[error("installed claude CLI is too old (missing {flag}) — run: claude update")]
    ClaudeTooOld { flag: String },
    #[error("spec file not found: {0}")]
    SpecMissing(PathBuf),
    #[error(transparent)]
    Target(#[from] TargetError),
}

pub(crate) fn check_claude(claude_bin: &str) -> Result<(), PreflightError> {
    let out = Command::new(claude_bin)
        .arg("--help")
        .output()
        .map_err(|_| PreflightError::ClaudeNotFound)?;
    let help = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    for flag in REQUIRED_FLAGS {
        if !help.contains(flag) {
            return Err(PreflightError::ClaudeTooOld {
                flag: flag.to_string(),
            });
        }
    }
    Ok(())
}

pub(crate) fn check_target(target: &Target, root: &Path) -> Result<(), PreflightError> {
    match target {
        Target::SpecFiles(paths) => {
            for p in paths {
                if !root.join(p).is_file() {
                    return Err(PreflightError::SpecMissing(p.clone()));
                }
            }
            Ok(())
        }
        Target::GitDiff { base } => {
            precheck_diff(base.as_deref(), root)?;
            Ok(())
        }
    }
}

/// Combined checks for the `reviewal review` CLI path, run before it enters
/// the TUI; the TUI runs the two checks independently.
pub fn preflight(claude_bin: &str, target: &Target, root: &Path) -> Vec<PreflightError> {
    let mut errors = Vec::new();
    if let Err(e) = check_claude(claude_bin) {
        errors.push(e);
    }
    if let Err(e) = check_target(target, root) {
        errors.push(e);
    }
    errors
}

/// Runs `check_claude` off-thread so drawing never blocks on the subprocess;
/// the receiver yields exactly one result.
pub(crate) fn spawn_check_claude(
    claude_bin: String,
) -> std::sync::mpsc::Receiver<Result<(), String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(check_claude(&claude_bin).map_err(|e| e.to_string()));
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::target::Target;

    fn help_script(dir: &std::path::Path, help: &str) -> String {
        let path = dir.join("claude");
        std::fs::write(&path, format!("#!/bin/bash\necho '{help}'\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path.display().to_string()
    }

    #[test]
    fn check_claude_errs_when_binary_missing() {
        assert!(matches!(
            check_claude("/no/such/claude"),
            Err(PreflightError::ClaudeNotFound)
        ));
    }

    #[test]
    fn check_claude_errs_when_required_flag_missing() {
        let dir = tempfile::tempdir().unwrap();
        let old = help_script(dir.path(), "--tools --safe-mode only");
        match check_claude(&old) {
            Err(PreflightError::ClaudeTooOld { flag }) => assert_eq!(flag, "--json-schema"),
            other => panic!("expected ClaudeTooOld, got {other:?}"),
        }
    }

    #[test]
    fn check_claude_ok_when_all_flags_present() {
        let dir = tempfile::tempdir().unwrap();
        let good = help_script(dir.path(), "--tools --safe-mode --json-schema all here");
        assert!(check_claude(&good).is_ok());
    }

    #[test]
    fn check_target_errs_when_spec_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = Target::SpecFiles(vec!["ghost.md".into()]);
        assert!(check_target(&missing, dir.path()).is_err());
    }

    #[test]
    fn check_target_ok_when_spec_file_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s.md"), "x").unwrap();
        assert!(check_target(&Target::SpecFiles(vec!["s.md".into()]), dir.path()).is_ok());
    }

    #[test]
    fn check_target_errs_on_git_diff_in_non_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(check_target(&Target::GitDiff { base: None }, dir.path()).is_err());
    }

    #[test]
    fn spawn_check_claude_reports_on_channel() {
        let dir = tempfile::tempdir().unwrap();
        let good = help_script(dir.path(), "--tools --safe-mode --json-schema all here");
        let rx = spawn_check_claude(good);
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap(),
            Ok(())
        );
        let rx = spawn_check_claude("/no/such/claude".into());
        let err = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
            .unwrap_err();
        assert!(err.contains("claude CLI not found"), "{err}");
    }
}
