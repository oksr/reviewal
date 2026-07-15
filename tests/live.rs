//! Live smoke test: REVIEWAL_LIVE=1 cargo test --test live -- --nocapture
//! Guarded by the env var rather than `#[ignore]`, so plain `cargo test`
//! always skips it and CI never needs claude on PATH. Spends ~2 model calls.
use reviewal::engine::persona;
use reviewal::engine::run::{execute_run, RunEvent, RunSpec};
use reviewal::engine::target::Target;

#[tokio::test]
async fn live_spec_review_two_personas() {
    if std::env::var("REVIEWAL_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped (set REVIEWAL_LIVE=1 to run)");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("spec.md"),
        "# Feature: CSV export\n\nUsers can export their dashboard as CSV. The export runs \
         synchronously in the request handler and loads all rows into memory before \
         serializing. Success criterion: it works.\n",
    )
    .unwrap();
    let personas: Vec<_> = persona::builtins()
        .into_iter()
        .filter(|p| ["skeptic", "stickler"].contains(&p.name.as_str()))
        .collect();
    assert_eq!(personas.len(), 2);

    let spec = RunSpec {
        root: dir.path().to_path_buf(),
        target: Target::SpecFiles(vec!["spec.md".into()]),
        personas,
        model: None,
        cross_review: false,
        timeout_secs: 300,
        claude_bin: "claude".into(),
        now_utc: "2026-07-08T00:00:00Z".into(),
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_cancel_tx, cancel) = tokio::sync::watch::channel(false);
    execute_run(spec, tx, cancel).await;

    let mut completed = false;
    while let Some(event) = rx.recv().await {
        match event {
            RunEvent::RunCompleted { synthesis, .. } => {
                completed = true;
                eprintln!("verdict: {}", synthesis.consensus_label);
                assert_eq!(synthesis.verdicts.len(), 2);
            }
            RunEvent::RunFailed { message, .. } => panic!("run failed: {message}"),
            _ => {}
        }
    }
    assert!(completed, "expected RunCompleted");
}
