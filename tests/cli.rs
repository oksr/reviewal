use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_reviewal"))
}

#[test]
fn personas_lists_builtins() {
    let out = bin().arg("personas").output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    for name in [
        "prover", "breaker", "steward", "skeptic", "stickler", "advocate",
    ] {
        assert!(text.contains(name), "missing {name}");
    }
}

#[test]
fn init_scaffolds_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let out = bin().arg("init").current_dir(dir.path()).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir
        .path()
        .join(".claude/skills/reviewal-ingest/SKILL.md")
        .exists());
    assert!(dir.path().join(".reviewal/config.toml").exists());
    let out2 = bin().arg("init").current_dir(dir.path()).output().unwrap();
    assert!(String::from_utf8_lossy(&out2.stdout).contains("up-to-date"));
}

#[test]
fn review_requires_a_target() {
    let out = bin().arg("review").output().unwrap();
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(
        text.contains("--diff") && text.contains("--spec"),
        "stderr: {text}"
    );
}

#[test]
fn review_rejects_unknown_persona() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.md"), "x").unwrap();
    let out = bin()
        .args(["review", "--spec", "s.md", "--personas", "ghost"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("ghost"));
}

#[test]
fn non_tty_invocation_fails_with_guidance() {
    // Stdout is a pipe when run under the test harness via Command — exactly the non-TTY case.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_reviewal"))
        .current_dir(tempfile::tempdir().unwrap().path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("interactive TUI"), "{err}");
}

#[test]
fn review_with_missing_spec_fails_before_the_tui_with_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_reviewal"))
        .current_dir(dir.path())
        .args(["review", "--spec", "ghost.md"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "preflight failures exit 2, not 0"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("spec file not found"), "{err}");
}
