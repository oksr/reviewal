use crate::engine::agent::{self, AgentConfig};
use crate::engine::error::EngineError;
use crate::engine::model::{Round1Review, Round2Review};
use crate::engine::persona::Persona;
use crate::engine::prompt;
use crate::engine::store::{RunDir, RunRecord, RunStatus, RunStore};
use crate::engine::synthesis::{self, Synthesis};
use crate::engine::target::Target;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc::UnboundedSender, watch};
use tokio::task::JoinSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Collecting,
    Round1,
    Round2,
    Synthesizing,
}

#[derive(Debug, Clone)]
pub enum RunEvent {
    PhaseChanged(Phase),
    AgentStarted {
        persona: String,
    },
    AgentActivity {
        persona: String,
        activity: agent::AgentActivity,
    },
    AgentRetrying {
        persona: String,
        error: String,
    },
    AgentDone {
        persona: String,
        duration_secs: u64,
        findings: Vec<crate::engine::model::FindingBrief>,
        /// True when the round output was persisted to disk before this
        /// event fired.
        saved: bool,
    },
    AgentFailed {
        persona: String,
        error: String,
    },
    RunCompleted {
        run_id: String,
        synthesis: Synthesis,
    },
    RunFailed {
        run_id: Option<String>,
        message: String,
    },
    /// A user-requested cancel ended the run. Not a failure: `resumable`
    /// tells the UI whether enough reviews survived to resume into triage.
    RunCancelled {
        run_id: String,
        kept_reviews: usize,
        resumable: bool,
    },
    /// Non-fatal trouble worth telling the user about (e.g. a best-effort
    /// disk write failed). Never gates run completion.
    Warning {
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct RunSpec {
    pub root: PathBuf,
    pub target: Target,
    pub personas: Vec<Persona>,
    pub model: Option<String>,
    pub cross_review: bool,
    pub timeout_secs: u64,
    pub claude_bin: String,
    pub now_utc: String,
}

/// Lives in the library (not the binary) so the target/persona validation
/// branches are unit-testable.
#[allow(clippy::too_many_arguments)]
pub fn build_review_spec(
    root: &Path,
    config: &crate::config::Config,
    diff: Option<Option<String>>,
    spec: Vec<PathBuf>,
    personas_arg: Option<String>,
    cross_review: bool,
    model: Option<String>,
    now_utc: String,
) -> anyhow::Result<RunSpec> {
    // `diff` is the CLI tri-state verbatim: None = flag absent,
    // Some(None) = `--diff` (HEAD), Some(Some(b)) = `--diff b`. The inner
    // Option *is* the GitDiff base — no sentinel value.
    let target = match (diff, spec.is_empty()) {
        (Some(_), false) | (None, true) => {
            anyhow::bail!("exactly one review target is required: --diff [BASE] or --spec PATH...")
        }
        (Some(base), true) => Target::GitDiff { base },
        (None, false) => Target::SpecFiles(spec),
    };
    let kind = target.kind();
    let (available, failures) = crate::engine::persona::available(kind, &config.persona_dirs(root));
    for w in &failures {
        eprintln!("warning: {w}");
    }
    let personas = match personas_arg {
        None => available,
        Some(names) => {
            let wanted: Vec<&str> = names
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            let known: Vec<&str> = available.iter().map(|p| p.name.as_str()).collect();
            for name in &wanted {
                if !known.iter().any(|k| k == name) {
                    anyhow::bail!(
                        "unknown persona '{name}' — known personas for this target: {}",
                        known.join(", ")
                    );
                }
            }
            available
                .into_iter()
                .filter(|p| wanted.iter().any(|w| *w == p.name))
                .collect()
        }
    };
    if personas.len() < 2 {
        anyhow::bail!("need at least 2 reviewers — pass more names via --personas");
    }
    Ok(RunSpec {
        root: root.to_path_buf(),
        target,
        personas,
        model: model.or_else(|| config.model.clone()),
        cross_review,
        timeout_secs: config.timeout_secs,
        claude_bin: config.claude_bin.clone(),
        now_utc,
    })
}

fn slug(target: &Target) -> String {
    match target {
        Target::SpecFiles(_) => "spec".to_string(),
        Target::GitDiff { base: None } => "diff-head".to_string(),
        Target::GitDiff { base: Some(b) } => format!("diff-{}", b.to_lowercase().replace('/', "-")),
    }
}

/// Send failures are ignored: a dropped receiver just means the UI went away.
fn warn(tx: &UnboundedSender<RunEvent>, message: String) {
    let _ = tx.send(RunEvent::Warning { message });
}

const RETRY_TAIL: &str = "\n\nRe-emit your response as a single JSON object that satisfies the schema above. No markdown fences. No prose outside the JSON.";

fn retry_prompt(original: &str, error: &str) -> String {
    format!("{original}\n\n---\n\n# RETRY — your previous response was rejected\n\nReason: {error}{RETRY_TAIL}")
}

fn finding_briefs(v: &serde_json::Value) -> Vec<crate::engine::model::FindingBrief> {
    v.get("findings")
        .or_else(|| v.get("added"))
        .and_then(|f| f.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| serde_json::from_value(f.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

struct AgentResult<T> {
    persona: String,
    // `run_persona` already persisted the round output (success or failure)
    // to disk before returning; the caller only needs the typed value.
    parsed: Option<(serde_json::Value, T)>,
}

// One persona, one round: invoke → parse → validate, retrying once with
// feedback. Persists the round's output (parsed on success, raw on failure)
// to disk before emitting AgentDone/AgentFailed, so a cancel that lands right
// after this returns never discards a finished, paid-for review.
#[allow(clippy::too_many_arguments)]
async fn run_persona<T, V>(
    cfg: AgentConfig,
    persona: Persona,
    prompt: Arc<str>,
    schema: serde_json::Value,
    validate: V,
    tx: UnboundedSender<RunEvent>,
    dir: Arc<RunDir>,
    round: u8,
) -> AgentResult<T>
where
    T: Send + 'static,
    V: Fn(&serde_json::Value, &str) -> Result<T, prompt::ValidationError>,
{
    let name = persona.name.clone();
    let _ = tx.send(RunEvent::AgentStarted {
        persona: name.clone(),
    });
    // The retry override is built lazily: the (source-bundle-sized) prompt is
    // copied only when attempt 0 actually fails.
    let mut retry: Option<String> = None;
    let mut raw = String::new();
    for attempt in 0..2 {
        let this_prompt = retry.as_deref().unwrap_or(&prompt);
        let tx_act = tx.clone();
        let name_act = name.clone();
        let on_activity = move |a: agent::AgentActivity| {
            let _ = tx_act.send(RunEvent::AgentActivity {
                persona: name_act.clone(),
                activity: a,
            });
        };
        let outcome = agent::invoke(&cfg, &persona.system, &schema, this_prompt, on_activity).await;
        match outcome {
            Ok(out) => {
                raw = out.result_text.clone();
                let verdict: Result<(serde_json::Value, T), EngineError> =
                    crate::engine::parse::extract_json(&out.result_text)
                        .map_err(EngineError::from)
                        .and_then(|v| {
                            validate(&v, &name)
                                .map_err(EngineError::from)
                                .map(|review| (v, review))
                        });
                match verdict {
                    Ok((v, review)) => {
                        let findings = finding_briefs(&v);
                        // Persist before announcing done: a cancel racing this
                        // event must see the review already on disk.
                        let saved = match dir.save_round(round, &name, &v, &raw) {
                            Ok(()) => true,
                            Err(e) => {
                                warn(
                                    &tx,
                                    format!("failed to save round{round} output for {name}: {e:#}"),
                                );
                                false
                            }
                        };
                        let _ = tx.send(RunEvent::AgentDone {
                            persona: name.clone(),
                            duration_secs: out.duration.as_secs(),
                            findings,
                            saved,
                        });
                        return AgentResult {
                            persona: name,
                            parsed: Some((v, review)),
                        };
                    }
                    Err(e) if attempt == 0 => {
                        let reason = e.to_string();
                        let _ = tx.send(RunEvent::AgentRetrying {
                            persona: name.clone(),
                            error: reason.clone(),
                        });
                        retry = Some(retry_prompt(&prompt, &reason));
                    }
                    Err(e) => {
                        save_raw_best_effort(&dir, round, &name, &raw, &tx);
                        let _ = tx.send(RunEvent::AgentFailed {
                            persona: name.clone(),
                            error: e.to_string(),
                        });
                        return AgentResult {
                            persona: name,
                            parsed: None,
                        };
                    }
                }
            }
            Err(e) => {
                save_raw_best_effort(&dir, round, &name, &raw, &tx);
                let _ = tx.send(RunEvent::AgentFailed {
                    persona: name.clone(),
                    error: e.to_string(),
                });
                return AgentResult {
                    persona: name,
                    parsed: None,
                };
            }
        }
    }
    // Unreachable in practice — every branch above returns — but the loop's
    // final iteration always takes a `return`-bearing arm, so the compiler
    // still requires a trailing value of the right type.
    AgentResult {
        persona: name,
        parsed: None,
    }
}

/// A failed write is surfaced as a `Warning` and never gates the result.
fn save_raw_best_effort(
    dir: &RunDir,
    round: u8,
    name: &str,
    raw: &str,
    tx: &UnboundedSender<RunEvent>,
) {
    if raw.is_empty() {
        return;
    }
    if let Err(e) = dir.save_round_raw(round, name, raw) {
        warn(
            tx,
            format!("failed to save round{round} raw output for {name}: {e:#}"),
        );
    }
}

async fn join_or_cancel<T: 'static>(
    set: &mut JoinSet<AgentResult<T>>,
    cancel: &mut watch::Receiver<bool>,
) -> Option<Vec<AgentResult<T>>> {
    let mut results = Vec::new();
    let mut cancel_open = true;
    loop {
        if cancel_open {
            tokio::select! {
                joined = set.join_next() => match joined {
                    Some(Ok(r)) => results.push(r),
                    // A panicked/aborted task is dropped: it lands in neither the
                    // round map nor `failed`/`degraded` — absent, not counted.
                    Some(Err(_)) => {}
                    None => return Some(results),
                },
                changed = cancel.changed() => match changed {
                    Ok(()) if *cancel.borrow() => {
                        set.abort_all();
                        return None;
                    }
                    Ok(()) => {}
                    Err(_) => cancel_open = false, // sender gone; stop polling it
                }
            }
        } else {
            match set.join_next().await {
                Some(Ok(r)) => results.push(r),
                Some(Err(_)) => {}
                None => return Some(results),
            }
        }
    }
}

/// Only report success once the record is actually on disk: a failed final
/// write is surfaced as `RunFailed`, not a misleading clean completion.
fn persist_and_complete(
    dir: &RunDir,
    record: &RunRecord,
    run_id: String,
    synthesis: Synthesis,
    tx: &UnboundedSender<RunEvent>,
) {
    if let Err(e) = dir.save_record(record) {
        let _ = tx.send(RunEvent::RunFailed {
            run_id: Some(run_id),
            message: format!("failed to persist completed run: {e}"),
        });
        return;
    }
    let _ = tx.send(RunEvent::RunCompleted { run_id, synthesis });
}

pub async fn execute_run(
    spec: RunSpec,
    tx: UnboundedSender<RunEvent>,
    mut cancel: watch::Receiver<bool>,
) {
    let store = RunStore::new(&spec.root);

    let _ = tx.send(RunEvent::PhaseChanged(Phase::Collecting));
    let bundle = match crate::engine::target::collect(&spec.target, &spec.root) {
        Ok(b) => b,
        Err(e) => {
            let _ = tx.send(RunEvent::RunFailed {
                run_id: None,
                message: e.to_string(),
            });
            return;
        }
    };

    let mut record = RunRecord {
        id: RunStore::new_run_id(&slug(&spec.target), &spec.now_utc),
        created_at: spec.now_utc.clone(),
        target: spec.target.clone(),
        target_desc: spec.target.describe(),
        personas: spec.personas.iter().map(|p| p.name.clone()).collect(),
        model: spec.model.clone(),
        cross_review: spec.cross_review,
        status: RunStatus::Running,
        degraded: vec![],
        findings_total: None,
        verdict_label: None,
        accepted_count: None,
    };
    let dir: RunDir = match store.create_run(&record).and_then(|d| {
        d.save_source(&bundle.block)?;
        Ok(d)
    }) {
        Ok(d) => d,
        Err(e) => {
            let _ = tx.send(RunEvent::RunFailed {
                run_id: None,
                message: e.to_string(),
            });
            return;
        }
    };
    let dir = Arc::new(dir);
    let run_id = record.id.clone();

    let fail_run = |record: &mut RunRecord, message: String, tx: &UnboundedSender<RunEvent>| {
        record.status = RunStatus::Aborted;
        let message = match dir.save_record(record) {
            Ok(()) => message,
            Err(e) => format!("{message} (also failed to persist aborted status: {e})"),
        };
        let _ = tx.send(RunEvent::RunFailed {
            run_id: Some(record.id.clone()),
            message,
        });
    };

    let agent_cfg = |spec: &RunSpec| AgentConfig {
        claude_bin: spec.claude_bin.clone(),
        model: spec.model.clone(),
        timeout: Duration::from_secs(spec.timeout_secs),
    };

    let _ = tx.send(RunEvent::PhaseChanged(Phase::Round1));
    let kind = spec.target.kind();
    // The prompt embeds the whole source bundle, so build it once and share it
    // across the fan-out instead of re-allocating a copy per persona.
    let round1_prompt: Arc<str> =
        prompt::build_round1_prompt(prompt::ROUND1_INSTRUCTIONS, kind, &bundle.block).into();
    let mut set = JoinSet::new();
    for persona in spec.personas.iter().cloned() {
        set.spawn(run_persona(
            agent_cfg(&spec),
            persona,
            Arc::clone(&round1_prompt),
            prompt::round1_schema(),
            prompt::validate_round1,
            tx.clone(),
            Arc::clone(&dir),
            1,
        ));
    }
    let Some(results) = join_or_cancel(&mut set, &mut cancel).await else {
        finish_cancelled_round1(&store, &dir, &mut record, &spec, &tx);
        return;
    };

    // Each persona already persisted its own output (parsed or raw) before
    // its terminal event fired — this loop only rebuilds the in-memory maps
    // synthesis needs.
    let mut round1: BTreeMap<String, Round1Review> = BTreeMap::new();
    let mut failed: Vec<String> = Vec::new();
    for r in results {
        match r.parsed {
            Some((_value, review)) => {
                round1.insert(r.persona, review);
            }
            None => {
                failed.push(r.persona);
            }
        }
    }
    failed.sort();

    if round1.len() < 2 {
        fail_run(
            &mut record,
            "fewer than 2 reviewers produced valid output — synthesis aborted".into(),
            &tx,
        );
        return;
    }

    let mut round2: BTreeMap<String, Round2Review> = BTreeMap::new();
    if spec.cross_review {
        let _ = tx.send(RunEvent::PhaseChanged(Phase::Round2));
        let combined = serde_json::to_string_pretty(&round1).expect("round1 serializes");
        let round2_prompt: Arc<str> = prompt::build_round2_prompt(
            prompt::ROUND2_INSTRUCTIONS,
            kind,
            &bundle.block,
            &combined,
        )
        .into();
        let mut set = JoinSet::new();
        for persona in spec
            .personas
            .iter()
            .filter(|p| round1.contains_key(&p.name))
            .cloned()
        {
            set.spawn(run_persona(
                agent_cfg(&spec),
                persona,
                Arc::clone(&round2_prompt),
                prompt::round2_schema(),
                prompt::validate_round2,
                tx.clone(),
                Arc::clone(&dir),
                2,
            ));
        }
        let Some(results) = join_or_cancel(&mut set, &mut cancel).await else {
            // Round 1 is fully complete in memory; synthesize with whatever
            // round-2 output made it to disk before the cancel landed.
            let round2_disk = dir.load_round2().unwrap_or_default();
            let synthesis = synthesis::synthesize(&round1, &round2_disk, &failed);
            record.status = RunStatus::ReviewsComplete;
            record.degraded = failed;
            record.findings_total = Some(synthesis.findings.len());
            if let Err(e) = dir.save_record(&record) {
                warn(&tx, format!("failed to persist cancelled run: {e:#}"));
            }
            let _ = tx.send(RunEvent::RunCancelled {
                run_id: record.id.clone(),
                kept_reviews: round1.len(),
                resumable: true,
            });
            return;
        };
        for r in results {
            if let Some((_value, review)) = r.parsed {
                round2.insert(r.persona, review);
            }
        }
    }

    let _ = tx.send(RunEvent::PhaseChanged(Phase::Synthesizing));
    let synthesis = synthesis::synthesize(&round1, &round2, &failed);
    record.status = RunStatus::ReviewsComplete;
    record.degraded = failed;
    record.findings_total = Some(synthesis.findings.len());
    persist_and_complete(&dir, &record, run_id, synthesis, &tx);
}

/// Cancel landed during round 1: reload whatever made it to disk and decide
/// whether the run is resumable. `_store` is unused, kept for signature
/// symmetry with the other run-finishing helpers.
fn finish_cancelled_round1(
    _store: &RunStore,
    dir: &RunDir,
    record: &mut RunRecord,
    spec: &RunSpec,
    tx: &UnboundedSender<RunEvent>,
) {
    let round1 = dir.load_round1().unwrap_or_default();
    let kept = round1.len();
    if kept >= 2 {
        let missing: Vec<String> = spec
            .personas
            .iter()
            .map(|p| p.name.clone())
            .filter(|n| !round1.contains_key(n))
            .collect();
        let synthesis = synthesis::synthesize(&round1, &BTreeMap::new(), &missing);
        record.status = RunStatus::ReviewsComplete;
        record.degraded = missing;
        record.findings_total = Some(synthesis.findings.len());
    } else {
        record.status = RunStatus::Aborted;
    }
    let resumable = record.status == RunStatus::ReviewsComplete;
    if let Err(e) = dir.save_record(record) {
        warn(tx, format!("failed to persist cancelled run: {e:#}"));
    }
    let _ = tx.send(RunEvent::RunCancelled {
        run_id: record.id.clone(),
        kept_reviews: kept,
        resumable,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::persona::{Persona, PersonaTarget};
    use crate::engine::store::{RunStatus, RunStore};
    use crate::engine::target::Target;
    use tokio::sync::{mpsc, watch};

    #[test]
    fn finding_briefs_extracts_severity_and_title_from_round1_and_round2_shapes() {
        let r1 = serde_json::json!({"findings": [
            {"severity":"critical","title":"walk_dir follows symlink cycles","detail":"d","file":null,"line":null,"fix":null},
            {"severity":"info","title":"minor nit","detail":"d","file":null,"line":null,"fix":null}
        ]});
        let briefs = finding_briefs(&r1);
        assert_eq!(briefs.len(), 2);
        assert_eq!(briefs[0].severity, crate::engine::model::Severity::Critical);
        assert_eq!(briefs[0].title, "walk_dir follows symlink cycles");

        let r2 = serde_json::json!({"added": [
            {"severity":"warning","title":"missed case","detail":"d","file":null,"line":null,"fix":null}
        ], "validate": [], "challenge": []});
        assert_eq!(finding_briefs(&r2).len(), 1);

        assert!(finding_briefs(&serde_json::json!({"persona":"x"})).is_empty());
    }

    // fake claude: emits a valid round-1 review for the persona named by --system-prompt
    const HAPPY_SCRIPT: &str = r#"
persona=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "--system-prompt" ]; then persona="$arg"; fi
  prev="$arg"
done
cat > /dev/null
inner="{\"persona\":\"$persona\",\"verdict\":\"approve\",\"summary\":\"$persona\",\"findings\":[]}"
esc=$(printf '%s' "$inner" | sed 's/\\/\\\\/g; s/"/\\"/g')
echo "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"$esc\"}"
"#;

    fn script(dir: &std::path::Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/bash\n{body}")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path.display().to_string()
    }

    fn persona(name: &str) -> Persona {
        Persona {
            name: name.into(),
            title: name.into(),
            lens: "l".into(),
            target: PersonaTarget::Both,
            system: name.into(),
            builtin: false,
            color: None,
            source: None,
        }
    }

    fn spec_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s.md"), "spec body").unwrap();
        dir
    }

    fn run_spec(dir: &std::path::Path, bin: String, personas: Vec<Persona>) -> RunSpec {
        RunSpec {
            root: dir.to_path_buf(),
            target: Target::SpecFiles(vec!["s.md".into()]),
            personas,
            model: None,
            cross_review: false,
            // Generous: the fake-agent scripts finish in milliseconds, but
            // first-exec of freshly written scripts can stall for seconds
            // under a fully parallel `cargo test` (macOS assesses each new
            // executable), so a tight budget makes these tests flaky.
            timeout_secs: 60,
            claude_bin: bin,
            now_utc: "2026-07-07T12:00:00Z".into(),
        }
    }

    async fn drain(mut rx: mpsc::UnboundedReceiver<RunEvent>) -> Vec<RunEvent> {
        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        events
    }

    #[tokio::test]
    async fn failed_round1_save_warns_but_run_still_completes() {
        let dir = spec_dir();
        // Poison prover's round1 target: a non-empty directory defeats the
        // store's atomic temp+rename write (same trick as the persist test).
        let poisoned = dir
            .path()
            .join(".reviewal/runs/2026-07-07T12-00-00Z-spec/round1/prover.json");
        std::fs::create_dir_all(&poisoned).unwrap();
        std::fs::write(poisoned.join("occupant"), "x").unwrap();

        let bin = script(dir.path(), "claude", HAPPY_SCRIPT);
        let (tx, rx) = mpsc::unbounded_channel();
        let (_ctx, cancel) = watch::channel(false);
        execute_run(
            run_spec(dir.path(), bin, vec![persona("prover"), persona("skeptic")]),
            tx,
            cancel,
        )
        .await;
        let events = drain(rx).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, RunEvent::Warning { message } if message.contains("prover"))),
            "a swallowed round1 save failure must surface as a Warning naming the persona"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, RunEvent::RunCompleted { .. })),
            "best-effort saves must not gate run completion"
        );
    }

    #[tokio::test]
    async fn happy_path_completes_with_reviews_complete_status() {
        let dir = spec_dir();
        let bin = script(dir.path(), "claude", HAPPY_SCRIPT);
        let (tx, rx) = mpsc::unbounded_channel();
        let (_ctx, cancel) = watch::channel(false);
        execute_run(
            run_spec(dir.path(), bin, vec![persona("prover"), persona("skeptic")]),
            tx,
            cancel,
        )
        .await;
        let events = drain(rx).await;
        let completed = events
            .iter()
            .find_map(|e| match e {
                RunEvent::RunCompleted { run_id, synthesis } => {
                    Some((run_id.clone(), synthesis.clone()))
                }
                _ => None,
            })
            .expect("RunCompleted");
        assert_eq!(completed.1.verdicts.len(), 2);
        let store = RunStore::new(dir.path());
        let rec = store.open_run(&completed.0).unwrap().load_record().unwrap();
        assert_eq!(rec.status, RunStatus::ReviewsComplete);
        assert!(store
            .open_run(&completed.0)
            .unwrap()
            .load_round1()
            .unwrap()
            .contains_key("prover"));
    }

    #[tokio::test]
    async fn typed_reviews_survive_run_boundary() {
        let dir = spec_dir();
        let bin = script(dir.path(), "claude", HAPPY_SCRIPT);
        let (tx, rx) = mpsc::unbounded_channel();
        let (_ctx, cancel) = watch::channel(false);
        execute_run(
            run_spec(dir.path(), bin, vec![persona("prover"), persona("skeptic")]),
            tx,
            cancel,
        )
        .await;
        let events = drain(rx).await;
        let run_id = events
            .iter()
            .find_map(|e| match e {
                RunEvent::RunCompleted { run_id, .. } => Some(run_id.clone()),
                _ => None,
            })
            .expect("RunCompleted");
        let store = RunStore::new(dir.path());
        let round1 = store.open_run(&run_id).unwrap().load_round1().unwrap();
        let prover = round1.get("prover").expect("prover round1 persisted");
        assert_eq!(prover.persona, "prover");
        assert_eq!(prover.verdict, crate::engine::model::Verdict::Approve);
        assert_eq!(prover.summary, "prover");
        assert!(prover.findings.is_empty());
    }

    #[tokio::test]
    async fn run_completed_only_after_record_persists() {
        let dir = tempfile::tempdir().unwrap();
        // Make save_record fail: run.json is a non-empty DIRECTORY, so the
        // store's atomic write (temp file + rename over the target) cannot
        // replace it. A read-only file would not do — rename(2) happily
        // swaps a read-only target, and permission tricks are a no-op under
        // root anyway.
        let run_json = dir.path().join("run.json");
        std::fs::create_dir(&run_json).unwrap();
        std::fs::write(run_json.join("occupant"), "x").unwrap();

        let record = RunRecord {
            id: "test-run".into(),
            created_at: "2026-07-08T00:00:00Z".into(),
            target: Target::GitDiff { base: None },
            target_desc: "diff vs HEAD".into(),
            personas: vec!["prover".into(), "skeptic".into()],
            model: None,
            cross_review: false,
            status: RunStatus::ReviewsComplete,
            degraded: vec![],
            findings_total: None,
            verdict_label: None,
            accepted_count: None,
        };
        let run_dir = RunDir {
            path: dir.path().to_path_buf(),
        };
        let synthesis = synthesis::synthesize(&BTreeMap::new(), &BTreeMap::new(), &[]);
        let (tx, rx) = mpsc::unbounded_channel();
        persist_and_complete(&run_dir, &record, record.id.clone(), synthesis, &tx);
        drop(tx);
        let events = drain(rx).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, RunEvent::RunFailed { .. })),
            "a failed persist must be surfaced as RunFailed"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, RunEvent::RunCompleted { .. })),
            "must not announce completion when the record did not persist"
        );
    }

    #[tokio::test]
    async fn garbage_agent_degrades_run_after_retry() {
        let dir = spec_dir();
        let selective = HAPPY_SCRIPT.replace(
            "cat > /dev/null",
            "cat > /dev/null\nif [ \"$persona\" = \"breaker\" ]; then echo '{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"total garbage\"}'; exit 0; fi",
        );
        let bin = script(dir.path(), "claude", &selective);
        let (tx, rx) = mpsc::unbounded_channel();
        let (_ctx, cancel) = watch::channel(false);
        execute_run(
            run_spec(
                dir.path(),
                bin,
                vec![persona("prover"), persona("skeptic"), persona("breaker")],
            ),
            tx,
            cancel,
        )
        .await;
        let events = drain(rx).await;
        assert!(events
            .iter()
            .any(|e| matches!(e, RunEvent::AgentRetrying { persona, .. } if persona == "breaker")));
        assert!(events
            .iter()
            .any(|e| matches!(e, RunEvent::AgentFailed { persona, .. } if persona == "breaker")));
        let syn = events
            .iter()
            .find_map(|e| match e {
                RunEvent::RunCompleted { synthesis, .. } => Some(synthesis.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(syn.degraded, vec!["breaker"]);
    }

    #[tokio::test]
    async fn retry_prompt_is_built_only_when_first_attempt_fails() {
        // Each invocation appends its stdin (the full prompt) to cap/<persona>.txt.
        // "breaker" returns garbage on its first attempt (no RETRY marker in the
        // prompt) and valid output on the retry, so it recovers.
        let dir = spec_dir();
        let cap = dir.path().join("cap");
        std::fs::create_dir(&cap).unwrap();
        let body = format!(
            r#"
persona=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "--system-prompt" ]; then persona="$arg"; fi
  prev="$arg"
done
input=$(cat)
printf '%s\n===INVOCATION===\n' "$input" >> "{cap}/$persona.txt"
if [ "$persona" = "breaker" ] && ! printf '%s' "$input" | grep -q "RETRY"; then
  echo '{{"type":"result","subtype":"success","result":"garbage"}}'
  exit 0
fi
inner="{{\"persona\":\"$persona\",\"verdict\":\"approve\",\"summary\":\"$persona\",\"findings\":[]}}"
esc=$(printf '%s' "$inner" | sed 's/\\/\\\\/g; s/"/\\"/g')
echo "{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"$esc\"}}"
"#,
            cap = cap.display()
        );
        let bin = script(dir.path(), "claude", &body);
        let (tx, rx) = mpsc::unbounded_channel();
        let (_ctx, cancel) = watch::channel(false);
        execute_run(
            run_spec(dir.path(), bin, vec![persona("prover"), persona("breaker")]),
            tx,
            cancel,
        )
        .await;
        let events = drain(rx).await;
        let syn = events
            .iter()
            .find_map(|e| match e {
                RunEvent::RunCompleted { synthesis, .. } => Some(synthesis.clone()),
                _ => None,
            })
            .expect("RunCompleted");
        assert!(syn.degraded.is_empty());

        let read = |name: &str| std::fs::read_to_string(cap.join(name)).unwrap();
        let prover = read("prover.txt");
        let breaker = read("breaker.txt");
        // Happy persona: invoked exactly once, never sees a retry-augmented prompt.
        assert_eq!(prover.matches("===INVOCATION===").count(), 1);
        assert!(!prover.contains("RETRY"));
        // Failing persona: invoked twice; the retry prompt carries the RETRY tail,
        // i.e. the clone is built only because a retry actually happened.
        assert_eq!(breaker.matches("===INVOCATION===").count(), 2);
        assert!(breaker.contains("your previous response was rejected"));
    }

    #[tokio::test]
    async fn fewer_than_two_survivors_aborts() {
        let dir = spec_dir();
        let bin = script(
            dir.path(),
            "claude",
            "cat > /dev/null\necho '{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"garbage\"}'\n",
        );
        let (tx, rx) = mpsc::unbounded_channel();
        let (_ctx, cancel) = watch::channel(false);
        execute_run(
            run_spec(dir.path(), bin, vec![persona("prover"), persona("skeptic")]),
            tx,
            cancel,
        )
        .await;
        let events = drain(rx).await;
        let run_id = events
            .iter()
            .find_map(|e| match e {
                RunEvent::RunFailed {
                    run_id: Some(id), ..
                } => Some(id.clone()),
                _ => None,
            })
            .expect("RunFailed carries the run id");
        let store = RunStore::new(dir.path());
        let (runs, _) = store.list_runs();
        assert_eq!(runs[0].status, RunStatus::Aborted);
        // Every persona's raw (unparseable) output is still persisted for
        // post-mortem inspection, even though nothing validated.
        let run_dir = store.open_run(&run_id).unwrap();
        for persona in ["prover", "skeptic"] {
            let raw =
                std::fs::read_to_string(run_dir.path.join(format!("round1/{persona}.raw.txt")))
                    .unwrap_or_else(|e| panic!("{persona}.raw.txt should exist: {e}"));
            assert_eq!(raw, "garbage");
            assert!(!run_dir.path.join(format!("round1/{persona}.json")).exists());
        }
    }

    #[tokio::test]
    async fn cross_review_round_runs_and_saves() {
        let dir = spec_dir();
        let round_aware = r#"
persona=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "--system-prompt" ]; then persona="$arg"; fi
  prev="$arg"
done
input=$(cat)
if printf '%s' "$input" | grep -q "Round 2"; then
  inner="{\"persona\":\"$persona\",\"validate\":[],\"challenge\":[],\"added\":[]}"
else
  inner="{\"persona\":\"$persona\",\"verdict\":\"approve\",\"summary\":\"$persona\",\"findings\":[]}"
fi
esc=$(printf '%s' "$inner" | sed 's/\\/\\\\/g; s/"/\\"/g')
echo "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"$esc\"}"
"#;
        let bin = script(dir.path(), "claude", round_aware);
        let mut spec = run_spec(dir.path(), bin, vec![persona("prover"), persona("skeptic")]);
        spec.cross_review = true;
        let (tx, rx) = mpsc::unbounded_channel();
        let (_ctx, cancel) = watch::channel(false);
        execute_run(spec, tx, cancel).await;
        let events = drain(rx).await;
        let run_id = events
            .iter()
            .find_map(|e| match e {
                RunEvent::RunCompleted { run_id, .. } => Some(run_id.clone()),
                _ => None,
            })
            .unwrap();
        let store = RunStore::new(dir.path());
        let r2 = store.open_run(&run_id).unwrap().load_round2().unwrap();
        assert_eq!(r2.len(), 2);
    }

    #[tokio::test]
    async fn cancel_aborts_run() {
        let dir = spec_dir();
        let bin = script(dir.path(), "claude", "cat > /dev/null\nsleep 30\n");
        let (tx, rx) = mpsc::unbounded_channel();
        let (ctx, cancel) = watch::channel(false);
        let handle = tokio::spawn(execute_run(
            run_spec(dir.path(), bin, vec![persona("prover"), persona("skeptic")]),
            tx,
            cancel,
        ));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        ctx.send(true).unwrap();
        handle.await.unwrap();
        let events = drain(rx).await;
        assert!(events.iter().any(|e| matches!(
            e,
            RunEvent::RunCancelled {
                kept_reviews: 0,
                resumable: false,
                ..
            }
        )));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, RunEvent::RunFailed { .. })),
            "cancellation is no longer reported as RunFailed"
        );
        let store = RunStore::new(dir.path());
        let (runs, _) = store.list_runs();
        assert_eq!(runs[0].status, RunStatus::Aborted);
    }

    // fake claude: emits a valid round-1 review for prover/skeptic instantly,
    // but sleeps 30s for "slowpoke" before it would emit anything — used to
    // exercise cancel-after-some-reviews-saved.
    const HAPPY_HAPPY_SLOW_SCRIPT: &str = r#"
persona=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "--system-prompt" ]; then persona="$arg"; fi
  prev="$arg"
done
cat > /dev/null
if [ "$persona" = "slowpoke" ]; then sleep 30; fi
inner="{\"persona\":\"$persona\",\"verdict\":\"approve\",\"summary\":\"$persona\",\"findings\":[]}"
esc=$(printf '%s' "$inner" | sed 's/\\/\\\\/g; s/"/\\"/g')
echo "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"$esc\"}"
"#;

    #[tokio::test]
    async fn round1_output_is_on_disk_before_agent_done_event() {
        let dir = spec_dir();
        let bin = script(dir.path(), "claude", HAPPY_SCRIPT);
        let spec = run_spec(dir.path(), bin, vec![persona("prover"), persona("skeptic")]);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_ctx, crx) = watch::channel(false);
        execute_run(spec, tx, crx).await;
        let mut seen_done = 0;
        while let Ok(ev) = rx.try_recv() {
            if let RunEvent::AgentDone { persona, saved, .. } = ev {
                assert!(saved, "{persona} output must be persisted before AgentDone");
                seen_done += 1;
            }
        }
        assert_eq!(seen_done, 2);
        let store = RunStore::new(dir.path());
        let (runs, _) = store.list_runs();
        let run_dir = store.open_run(&runs[0].id).unwrap();
        assert_eq!(
            run_dir.load_round1().unwrap().len(),
            2,
            "both round1 jsons written"
        );
        assert_eq!(
            runs[0].findings_total,
            Some(0),
            "completion records findings_total"
        );
    }

    #[tokio::test]
    async fn cancel_after_reviews_saved_yields_resumable_run() {
        let dir = spec_dir();
        let bin = script(dir.path(), "claude", HAPPY_HAPPY_SLOW_SCRIPT);
        let spec = run_spec(
            dir.path(),
            bin,
            vec![persona("prover"), persona("skeptic"), persona("slowpoke")],
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (ctx, crx) = watch::channel(false);
        let run = tokio::spawn(execute_run(spec, tx, crx));
        let mut done = 0;
        let cancelled_event = loop {
            match tokio::time::timeout(std::time::Duration::from_secs(20), rx.recv()).await {
                Ok(Some(RunEvent::AgentDone { .. })) => {
                    done += 1;
                    if done == 2 {
                        ctx.send(true).unwrap();
                    }
                }
                Ok(Some(RunEvent::RunCancelled {
                    kept_reviews,
                    resumable,
                    ..
                })) => {
                    break (kept_reviews, resumable);
                }
                Ok(Some(_)) => {}
                _ => panic!("run hung"),
            }
        };
        run.await.unwrap();
        assert_eq!(cancelled_event, (2, true));
        let store = RunStore::new(dir.path());
        let (runs, _) = store.list_runs();
        assert_eq!(
            runs[0].status,
            RunStatus::ReviewsComplete,
            "cancelled run is resumable"
        );
        assert_eq!(
            runs[0].degraded.len(),
            1,
            "the unfinished persona is degraded"
        );
    }

    #[tokio::test]
    async fn cancel_before_two_reviews_aborts() {
        let dir = spec_dir();
        let slow = script(dir.path(), "claude", "cat > /dev/null\nsleep 30\n");
        let spec = run_spec(
            dir.path(),
            slow,
            vec![persona("prover"), persona("skeptic")],
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (ctx, crx) = watch::channel(false);
        let run = tokio::spawn(execute_run(spec, tx, crx));
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(20), rx.recv()).await {
                Ok(Some(RunEvent::PhaseChanged(Phase::Round1))) => {
                    ctx.send(true).unwrap();
                }
                Ok(Some(RunEvent::RunCancelled {
                    kept_reviews,
                    resumable,
                    ..
                })) => {
                    assert_eq!((kept_reviews, resumable), (0, false));
                    break;
                }
                Ok(Some(_)) => {}
                _ => panic!("run hung"),
            }
        }
        run.await.unwrap();
        let (runs, _) = RunStore::new(dir.path()).list_runs();
        assert_eq!(runs[0].status, RunStatus::Aborted);
    }

    #[tokio::test]
    async fn cancel_during_round2_yields_resumable_run_with_round1_kept() {
        // Round 1 is fully in memory when a round-2 cancel lands, so both
        // reviews must be kept and the run must come back
        // resumable/`ReviewsComplete`, not `Aborted`.
        let dir = spec_dir();
        let slow_round2 = r#"
persona=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "--system-prompt" ]; then persona="$arg"; fi
  prev="$arg"
done
input=$(cat)
if printf '%s' "$input" | grep -q "Round 2"; then
  sleep 30
fi
inner="{\"persona\":\"$persona\",\"verdict\":\"approve\",\"summary\":\"$persona\",\"findings\":[]}"
esc=$(printf '%s' "$inner" | sed 's/\\/\\\\/g; s/"/\\"/g')
echo "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"$esc\"}"
"#;
        let bin = script(dir.path(), "claude", slow_round2);
        let mut spec = run_spec(dir.path(), bin, vec![persona("prover"), persona("skeptic")]);
        spec.cross_review = true;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (ctx, crx) = watch::channel(false);
        let run = tokio::spawn(execute_run(spec, tx, crx));
        let mut saw_round2 = false;
        let cancelled = loop {
            match tokio::time::timeout(std::time::Duration::from_secs(20), rx.recv()).await {
                Ok(Some(RunEvent::PhaseChanged(Phase::Round2))) => {
                    saw_round2 = true;
                    ctx.send(true).unwrap();
                }
                Ok(Some(RunEvent::RunCancelled {
                    kept_reviews,
                    resumable,
                    ..
                })) => {
                    break (kept_reviews, resumable);
                }
                Ok(Some(_)) => {}
                _ => panic!("run hung"),
            }
        };
        run.await.unwrap();
        assert!(saw_round2, "must have entered round 2 before cancelling");
        assert_eq!(cancelled, (2, true));
        let (runs, _) = RunStore::new(dir.path()).list_runs();
        assert_eq!(
            runs[0].status,
            RunStatus::ReviewsComplete,
            "round-2 cancel keeps the run resumable, not aborted"
        );
        assert!(
            runs[0].degraded.is_empty(),
            "both personas fully completed round 1"
        );
    }

    // These tests pass Config::default(), whose persona_dirs contain only
    // the tempdir project dir (no ambient global dir — that's resolved
    // solely in config::load), so persona counts are fully deterministic.

    #[test]
    fn build_review_spec_requires_exactly_one_target() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::Config::default();
        for (diff, spec) in [(None, vec![]), (Some(None), vec![PathBuf::from("x")])] {
            let err = build_review_spec(
                dir.path(),
                &config,
                diff,
                spec,
                None,
                false,
                None,
                "2026-01-01T00:00:00Z".into(),
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("exactly one review target"),
                "got: {err}"
            );
        }
    }

    #[test]
    fn build_review_spec_rejects_unknown_persona() {
        let dir = tempfile::tempdir().unwrap();
        let err = build_review_spec(
            dir.path(),
            &crate::config::Config::default(),
            Some(None),
            vec![],
            Some("nope".into()),
            false,
            None,
            "2026-01-01T00:00:00Z".into(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("unknown persona 'nope'"),
            "got: {err}"
        );
    }

    #[test]
    fn build_review_spec_needs_two_reviewers() {
        let dir = tempfile::tempdir().unwrap();
        let err = build_review_spec(
            dir.path(),
            &crate::config::Config::default(),
            Some(None),
            vec![],
            Some("prover".into()),
            false,
            None,
            "2026-01-01T00:00:00Z".into(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("need at least 2 reviewers"),
            "got: {err}"
        );
    }

    #[test]
    fn build_review_spec_builds_a_git_diff_spec() {
        let dir = tempfile::tempdir().unwrap();
        let spec = build_review_spec(
            dir.path(),
            &crate::config::Config::default(),
            Some(None),
            vec![],
            None,
            false,
            None,
            "2026-01-01T00:00:00Z".into(),
        )
        .unwrap();
        assert!(matches!(spec.target, Target::GitDiff { base: None }));
        assert!(spec.personas.len() >= 2);
        assert_eq!(spec.now_utc, "2026-01-01T00:00:00Z");
    }
}
