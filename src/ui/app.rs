use crate::engine::run::{RunEvent, RunSpec};
use crate::engine::store::{RunRecord, RunStatus, RunStore, Triage};
use crate::engine::synthesis::Synthesis;
use anyhow::Context;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) enum Transition {
    Quit,
    ToHome,
    Compose {
        target: Option<crate::engine::target::Target>,
        open_spec_picker: bool,
    },
    /// One-key launch: builds a default `RunSpec` for `target` and falls
    /// through to `StartRun`; blocked if the cached preflight already failed.
    QuickStart(crate::engine::target::Target),
    /// Reopens a `Finalized` run's triage — round files and triage.json stay
    /// on disk after finalize.
    ReopenTriage {
        run_id: String,
    },
    OpenRun(RunRecord),
    StartRun(RunSpec),
    CancelRun,
    ToTriage {
        run_id: String,
        target_desc: String,
        synthesis: Synthesis,
        triage: Triage,
    },
    Finalize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ClaudeCheck {
    Checking,
    Ok,
    Failed(String),
}

pub(crate) fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

pub enum Bootstrap {
    Home,
    Run(RunSpec),
}

pub(crate) enum Screen {
    Home(crate::ui::home::HomeState),
    Dashboard(crate::ui::dashboard::DashboardState),
    Triage(crate::ui::triage::TriageState),
    Done(crate::ui::done::DoneState),
    // Boxed: ComposerState is far larger than the other variants, so an
    // unboxed variant trips clippy::large_enum_variant (Screen would be
    // sized to it).
    Composer(Box<crate::ui::composer::ComposerState>),
}

pub(crate) struct EngineHandle {
    runtime: tokio::runtime::Runtime,
    tx: tokio::sync::mpsc::UnboundedSender<RunEvent>,
    rx: tokio::sync::mpsc::UnboundedReceiver<RunEvent>,
    cancel: Option<tokio::sync::watch::Sender<bool>>,
}

impl EngineHandle {
    pub(crate) fn try_new() -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("failed to start the async runtime")?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Ok(EngineHandle {
            runtime,
            tx,
            rx,
            cancel: None,
        })
    }

    pub(crate) fn start(&mut self, spec: RunSpec) {
        let (ctx, crx) = tokio::sync::watch::channel(false);
        self.cancel = Some(ctx);
        let tx = self.tx.clone();
        self.runtime
            .spawn(crate::engine::run::execute_run(spec, tx, crx));
    }

    pub(crate) fn cancel(&self) {
        if let Some(c) = &self.cancel {
            let _ = c.send(true);
        }
    }

    pub(crate) fn try_recv(&mut self) -> Option<RunEvent> {
        self.rx.try_recv().ok()
    }
}

/// `warnings` is a parameter (not read off `App`) because `try_new` calls
/// this before the `App` value exists.
fn home_screen(
    app_root: &Path,
    store: &RunStore,
    config: &crate::config::Config,
    mut warnings: Vec<String>,
) -> Screen {
    let (runs, mut store_warnings) = store.list_runs();
    warnings.append(&mut store_warnings);
    let dirs = config.persona_dirs(app_root);
    let (code_personas, _) =
        crate::engine::persona::available(crate::engine::model::TargetKind::Code, &dirs);
    let (spec_personas, _) =
        crate::engine::persona::available(crate::engine::model::TargetKind::Spec, &dirs);
    Screen::Home(crate::ui::home::HomeState {
        targets: crate::engine::target::detect_targets(app_root),
        spec_count: crate::ui::composer::collect_spec_files(app_root).len(),
        runs,
        zone: crate::ui::home::HomeZone::Launcher,
        launcher_idx: 0,
        history_idx: 0,
        warnings,
        skill_installed: crate::skill::ingest_skill_installed(app_root),
        defaults_code: code_personas.iter().map(|p| p.name.clone()).collect(),
        defaults_spec: spec_personas.iter().map(|p| p.name.clone()).collect(),
        show_help: false,
    })
}

pub(crate) struct App {
    pub root: PathBuf,
    pub config: crate::config::Config,
    pub store: RunStore,
    pub screen: Screen,
    pub status_line: Option<String>,
    pub should_quit: bool,
    pub engine: EngineHandle,
    pub theme: crate::ui::theme::Theme,
    theme_warnings: Vec<String>,
    pub claude_check: ClaudeCheck,
    claude_check_rx: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
}

impl App {
    pub(crate) fn try_new(
        root: PathBuf,
        config: crate::config::Config,
        bootstrap: Bootstrap,
    ) -> anyhow::Result<App> {
        let store = RunStore::new(&root);
        let (theme, theme_warnings) = crate::ui::theme::Theme::load(&config);
        // theme_warnings must stay theme-only: go_home reseeds from it on
        // every Home rebuild.
        let mut warnings = theme_warnings.clone();
        warnings.extend(store.mark_stale());
        let mut app = App {
            screen: home_screen(&root, &store, &config, warnings),
            root,
            config,
            store,
            status_line: None,
            should_quit: false,
            engine: EngineHandle::try_new()?,
            theme,
            theme_warnings,
            // Deliberately NOT spawned here: the check launches lazily on the
            // first poll_preflight, so building an App — which every UI test
            // does — never shells out to a claude binary.
            claude_check: ClaudeCheck::Checking,
            claude_check_rx: None,
        };
        if let Bootstrap::Run(spec) = bootstrap {
            app.apply(Transition::StartRun(spec));
        }
        Ok(app)
    }

    /// The first call spawns the background check (keeping `try_new`, and
    /// every test that builds an App, hermetic); once a result lands the
    /// receiver is dropped, so the check can never re-spawn.
    pub(crate) fn poll_preflight(&mut self) {
        if self.claude_check == ClaudeCheck::Checking && self.claude_check_rx.is_none() {
            self.claude_check_rx = Some(crate::engine::preflight::spawn_check_claude(
                self.config.claude_bin.clone(),
            ));
        }
        if let Some(rx) = &self.claude_check_rx {
            if let Ok(result) = rx.try_recv() {
                self.claude_check = match result {
                    Ok(()) => ClaudeCheck::Ok,
                    Err(e) => ClaudeCheck::Failed(e),
                };
                self.claude_check_rx = None;
            }
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) {
        // The status line describes the outcome of the *last* action; every
        // keypress starts a clean slate. Without this, a "run not resumable"
        // notice would ride along into whatever screen the next action opens.
        self.status_line = None;
        // Raw mode swallows SIGINT, so ctrl+c arrives as a key event.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Screen::Dashboard(state) = &mut self.screen {
                if state.run_live() {
                    // Same two-step confirmation as plain `c` — a run is in flight.
                    if let Some(t) =
                        state.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
                    {
                        self.apply(t);
                    }
                    return;
                }
            }
            self.apply(Transition::Quit);
            return;
        }
        let transition = match &mut self.screen {
            Screen::Home(state) => state.handle_key(key),
            Screen::Triage(state) => {
                let transition = state.handle_key(key);
                if state.dirty {
                    match self
                        .store
                        .open_run(&state.run_id)
                        .and_then(|dir| dir.save_triage(&state.triage))
                    {
                        Ok(()) => state.dirty = false,
                        Err(e) => self.status_line = Some(format!("failed to save triage: {e}")),
                    }
                }
                transition
            }
            Screen::Done(state) => state.handle_key(key),
            Screen::Dashboard(state) => state.handle_key(key),
            Screen::Composer(state) => state.handle_key(key),
        };
        if let Some(t) = transition {
            self.apply(t);
        }
    }

    pub(crate) fn handle_run_event(&mut self, event: RunEvent) {
        // Soft warnings are run-scoped, not persona-panel-scoped, so they land
        // on the global status line instead of a dashboard panel.
        if let RunEvent::Warning { message } = event {
            self.status_line = Some(format!("warning: {message}"));
            return;
        }
        if let Screen::Dashboard(state) = &mut self.screen {
            if let Some(t) = state.handle_run_event(event) {
                self.apply(t);
            }
        }
    }

    pub(crate) fn apply(&mut self, transition: Transition) {
        // Status-line clearing lives in `handle_key`, not here: `apply` also
        // runs from `Bootstrap::Run` and from engine events, where there is
        // no "previous action" whose notice should be wiped.
        match transition {
            Transition::Quit => self.should_quit = true,
            Transition::ToHome => self.go_home(),
            // Deliberately exhaustive: a new RunStatus variant must decide its
            // OpenRun behavior here, not silently fall into "not resumable".
            Transition::OpenRun(record) => match record.status {
                RunStatus::ReviewsComplete => match self.resume_triage(&record) {
                    Ok(state) => self.screen = Screen::Triage(state),
                    Err(e) => self.status_line = Some(format!("failed to resume run: {e}")),
                },
                RunStatus::Finalized => match self.load_done_state(&record.id) {
                    Ok(state) => self.screen = Screen::Done(state),
                    Err(e) => self.status_line = Some(format!("failed to open report: {e}")),
                },
                RunStatus::Running => self.status_line = Some("run still in progress".into()),
                RunStatus::Aborted | RunStatus::Stale => {
                    self.status_line = Some("run not resumable (stale/aborted)".into())
                }
            },
            Transition::ToTriage {
                run_id,
                target_desc,
                synthesis,
                triage,
            } => {
                let mut state =
                    crate::ui::triage::TriageState::new(run_id, target_desc, synthesis, triage);
                state.persona_colors = self.persona_frontmatter_colors();
                self.screen = Screen::Triage(state);
            }
            Transition::Finalize => match self.finalize() {
                Ok((run_id, record, report)) => {
                    self.screen = Screen::Done(crate::ui::done::DoneState::from_report(
                        &run_id,
                        &record.target_desc,
                        &report,
                        crate::skill::ingest_skill_installed(&self.root),
                    ));
                }
                Err(e) => {
                    self.status_line = Some(format!("finalize failed: {e}"));
                    self.go_home();
                }
            },
            Transition::StartRun(spec) => {
                // Reuse the settled preflight result, re-shelling out
                // synchronously only if the background check hasn't resolved.
                // Target checks are cheap local git calls, so they always run
                // synchronously.
                let mut errors: Vec<String> = Vec::new();
                match &self.claude_check {
                    ClaudeCheck::Failed(e) => errors.push(e.clone()),
                    ClaudeCheck::Checking => {
                        if let Err(e) =
                            crate::engine::preflight::check_claude(&self.config.claude_bin)
                        {
                            errors.push(e.to_string());
                        }
                    }
                    ClaudeCheck::Ok => {}
                }
                if let Err(e) = crate::engine::preflight::check_target(&spec.target, &self.root) {
                    errors.push(e.to_string());
                }
                if !errors.is_empty() {
                    self.status_line = Some(errors.join("; "));
                } else {
                    let personas: Vec<String> =
                        spec.personas.iter().map(|p| p.name.clone()).collect();
                    let persona_colors = spec
                        .personas
                        .iter()
                        .map(|p| (p.name.clone(), p.color.clone()))
                        .collect();
                    let target_desc = spec.target.describe();
                    let model_label = spec.model.clone().unwrap_or_else(|| "default".into());
                    let cross = spec.cross_review;
                    self.engine.start(spec);
                    let mut dash = crate::ui::dashboard::DashboardState::new(
                        personas,
                        target_desc,
                        model_label,
                        cross,
                    );
                    dash.persona_colors = persona_colors;
                    self.screen = Screen::Dashboard(dash);
                }
            }
            Transition::CancelRun => self.engine.cancel(),
            Transition::Compose {
                target,
                open_spec_picker,
            } => {
                self.screen = Screen::Composer(Box::new(crate::ui::composer::ComposerState::new(
                    &self.root,
                    &self.config,
                    target,
                    open_spec_picker,
                )));
            }
            Transition::QuickStart(target) => {
                // A cached Failed check blocks the launch outright — no point
                // spinning up a dashboard just to preflight-fail it a moment
                // later.
                if let ClaudeCheck::Failed(e) = &self.claude_check {
                    self.status_line = Some(e.clone());
                    return;
                }
                let kind = target.kind();
                let (personas, warnings) =
                    crate::engine::persona::available(kind, &self.config.persona_dirs(&self.root));
                let joined = warnings
                    .iter()
                    .map(|w| w.to_string())
                    .collect::<Vec<_>>()
                    .join("; ");
                let spec = RunSpec {
                    root: self.root.clone(),
                    target,
                    personas,
                    model: self.config.model.clone(),
                    cross_review: false,
                    timeout_secs: self.config.timeout_secs,
                    claude_bin: self.config.claude_bin.clone(),
                    now_utc: now_rfc3339(),
                };
                self.apply(Transition::StartRun(spec));
                // Persona-load warnings land only when StartRun left the line
                // clean: a preflight error outranks them, and the "warning: "
                // prefix matches the engine-warning convention in
                // handle_run_event.
                if self.status_line.is_none() && !joined.is_empty() {
                    self.status_line = Some(format!("warning: {joined}"));
                }
            }
            Transition::ReopenTriage { run_id } => {
                let result = self
                    .store
                    .open_run(&run_id)
                    .and_then(|dir| dir.load_record())
                    .and_then(|record| self.resume_triage(&record));
                match result {
                    Ok(state) => self.screen = Screen::Triage(state),
                    Err(e) => self.status_line = Some(format!("failed to reopen triage: {e}")),
                }
            }
        }
    }

    fn go_home(&mut self) {
        // Built fresh each time so store warnings never accumulate across
        // Home builds.
        let mut warnings = self.theme_warnings.clone();
        warnings.extend(self.store.mark_stale());
        self.screen = home_screen(&self.root, &self.store, &self.config, warnings);
    }

    fn resume_triage(&self, record: &RunRecord) -> anyhow::Result<crate::ui::triage::TriageState> {
        let dir = self.store.open_run(&record.id)?;
        let round1 = dir.load_round1()?;
        let round2 = dir.load_round2()?;
        let synthesis = crate::engine::synthesis::synthesize(&round1, &round2, &record.degraded);
        let triage = dir.load_triage()?;
        let mut state = crate::ui::triage::TriageState::new(
            record.id.clone(),
            record.target_desc.clone(),
            synthesis,
            triage,
        );
        state.persona_colors = self.persona_frontmatter_colors();
        Ok(state)
    }

    /// name → frontmatter color for every persona currently on disk. Names
    /// from old runs that no longer exist miss the map and hash-fallback.
    fn persona_frontmatter_colors(&self) -> std::collections::BTreeMap<String, Option<String>> {
        let dirs = self.config.persona_dirs(&self.root);
        let (custom, _warnings) = crate::engine::persona::load_custom(&dirs);
        let mut map: std::collections::BTreeMap<String, Option<String>> =
            crate::engine::persona::builtins()
                .into_iter()
                .map(|p| (p.name, p.color))
                .collect();
        for p in custom {
            map.insert(p.name, p.color);
        }
        map
    }

    /// Returns the typed report alongside the saved record so the caller can
    /// build the Done screen without re-reading the file it just wrote.
    fn finalize(&self) -> anyhow::Result<(String, RunRecord, crate::engine::synthesis::Report)> {
        let Screen::Triage(state) = &self.screen else {
            anyhow::bail!("finalize called with no active triage screen");
        };
        let run_id = state.run_id.clone();
        let dir = self.store.open_run(&run_id)?;
        let mut record = dir.load_record()?;
        let title = format!("Adversarial Review — {}", record.target_desc);
        let md = crate::engine::synthesis::render_markdown(&state.synthesis, &state.triage, &title);
        let report = crate::engine::synthesis::build_report(&state.synthesis, &state.triage);
        dir.write_report(&md, &serde_json::to_value(&report)?)?;
        record.status = RunStatus::Finalized;
        let accepted = report
            .findings
            .iter()
            .filter(|f| f.triage.status == crate::engine::store::TriageStatus::Accepted)
            .count();
        record.verdict_label = Some(report.consensus_label.clone());
        record.accepted_count = Some(accepted);
        record.findings_total = Some(report.findings.len());
        dir.save_record(&record)?;
        self.store.set_latest(&run_id)?;
        Ok((run_id, record, report))
    }

    /// The external boundary where a drifted or hand-edited report.json must
    /// fail loudly: deserializes the strict typed `Report` rather than
    /// probing an untyped `Value`.
    fn load_done_state(&self, run_id: &str) -> anyhow::Result<crate::ui::done::DoneState> {
        let dir = self.store.open_run(run_id)?;
        let record = dir.load_record()?;
        let text = std::fs::read_to_string(dir.path.join("report.json"))?;
        let report: crate::engine::synthesis::Report = serde_json::from_str(&text)?;
        Ok(crate::ui::done::DoneState::from_report(
            run_id,
            &record.target_desc,
            &report,
            crate::skill::ingest_skill_installed(&self.root),
        ))
    }
}

#[cfg(test)]
pub(crate) fn render_to_text<F: Fn(&mut ratatui::Frame)>(
    width: u16,
    height: u16,
    draw: F,
) -> String {
    buffer_text(&render_to_buffer(width, height, draw))
}

#[cfg(test)]
pub(crate) fn render_to_buffer<F: Fn(&mut ratatui::Frame)>(
    width: u16,
    height: u16,
    draw: F,
) -> ratatui::buffer::Buffer {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|f| draw(f)).unwrap();
    terminal.backend().buffer().clone()
}

#[cfg(test)]
pub(crate) fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    let mut out = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// Asserts no cell within any on-screen occurrence of `text` uses fg color
/// `forbidden`, without hard-coding which exact spans compose the text
/// (multiple styled spans can still concatenate into one contiguous match).
#[cfg(test)]
pub(crate) fn assert_no_cell_with_fg(
    buffer: &ratatui::buffer::Buffer,
    text: &str,
    forbidden: ratatui::style::Color,
) {
    let needle: Vec<char> = text.chars().collect();
    if needle.is_empty() {
        return;
    }
    for y in 0..buffer.area.height {
        let row: Vec<char> = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect();
        if needle.len() > row.len() {
            continue;
        }
        for start in 0..=row.len() - needle.len() {
            if row[start..start + needle.len()] != needle[..] {
                continue;
            }
            for (i, _) in needle.iter().enumerate() {
                let x = (start + i) as u16;
                let fg = buffer[(x, y)].style().fg;
                assert_ne!(
                    fg,
                    Some(forbidden),
                    "cell at ({x},{y}) within {text:?} must not use the forbidden color"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_keys::key;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn engine_warning_event_lands_on_status_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");
        app.handle_run_event(RunEvent::Warning {
            message: "could not save round1 output for prover".into(),
        });
        assert!(
            app.status_line
                .as_deref()
                .is_some_and(|s| s.contains("prover")),
            "warning must be visible on the status line: {:?}",
            app.status_line
        );
    }

    #[test]
    fn status_line_clears_on_next_action() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");
        let record = crate::engine::store::RunRecord {
            id: "2026-07-08T18-23-52Z-diff".into(),
            created_at: "2026-07-08T18:23:52Z".into(),
            target: crate::engine::target::Target::GitDiff { base: None },
            target_desc: "diff vs HEAD".into(),
            personas: vec!["prover".into()],
            model: None,
            cross_review: false,
            status: RunStatus::Aborted,
            degraded: vec![],
            findings_total: None,
            verdict_label: None,
            accepted_count: None,
        };
        app.apply(Transition::OpenRun(record));
        assert!(
            app.status_line.as_deref() == Some("run not resumable (stale/aborted)"),
            "notice set: {:?}",
            app.status_line
        );
        app.handle_key(key('n'));
        assert_eq!(
            app.status_line, None,
            "stale notice must not survive into the next screen"
        );
    }

    #[test]
    fn any_keypress_clears_stale_status_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");
        app.status_line = Some("old error".into());
        app.handle_key(key('j')); // j produces no Transition on Home
        assert_eq!(
            app.status_line, None,
            "navigation invalidates the previous action's status"
        );
    }

    #[test]
    fn quick_start_with_failed_claude_check_blocks_with_message() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");
        app.claude_check =
            ClaudeCheck::Failed("claude CLI not found on PATH — install Claude Code first".into());
        app.apply(Transition::QuickStart(
            crate::engine::target::Target::GitDiff { base: None },
        ));
        assert!(app
            .status_line
            .as_deref()
            .unwrap_or("")
            .contains("claude CLI not found"));
        assert!(matches!(app.screen, Screen::Home(_)), "stays on Home");
    }

    /// Executable stand-in for the claude CLI whose `--help` output
    /// advertises every flag `check_claude` requires.
    fn fake_claude_script(dir: &std::path::Path) -> String {
        let path = dir.join("claude");
        std::fs::write(
            &path,
            "#!/bin/bash\necho '--tools --safe-mode --json-schema all here'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path.display().to_string()
    }

    /// Polls until the background claude check settles (bounded — the check
    /// is a single local subprocess, comfortably under the 5s ceiling).
    fn poll_until_settled(app: &mut App) {
        for _ in 0..50 {
            app.poll_preflight();
            if app.claude_check != ClaudeCheck::Checking {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    #[test]
    fn poll_preflight_is_lazy_and_settles_ok_via_script() {
        let script_dir = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::Config {
            claude_bin: fake_claude_script(script_dir.path()),
            ..crate::config::Config::default()
        };
        let mut app =
            App::try_new(dir.path().to_path_buf(), config, Bootstrap::Home).expect("app builds");
        // Hermeticity invariant: constructing an App must never shell out.
        assert!(
            app.claude_check_rx.is_none(),
            "try_new must not spawn the claude check"
        );
        assert_eq!(app.claude_check, ClaudeCheck::Checking);
        poll_until_settled(&mut app);
        assert_eq!(app.claude_check, ClaudeCheck::Ok);
        assert!(
            app.claude_check_rx.is_none(),
            "receiver dropped once the result lands"
        );
    }

    #[test]
    fn poll_preflight_settles_failed_for_missing_binary() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::Config {
            claude_bin: "/definitely/not/a/claude/binary".into(),
            ..crate::config::Config::default()
        };
        let mut app =
            App::try_new(dir.path().to_path_buf(), config, Bootstrap::Home).expect("app builds");
        poll_until_settled(&mut app);
        match &app.claude_check {
            ClaudeCheck::Failed(e) => assert!(e.contains("not found"), "{e}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    fn write_bad_personas(root: &std::path::Path) {
        let dir = root.join(".reviewal/personas");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bad-one.md"), "no frontmatter here").unwrap();
        std::fs::write(dir.join("bad-two.md"), "also not a persona").unwrap();
    }

    #[test]
    fn quick_start_error_outranks_persona_warnings() {
        let dir = tempfile::tempdir().unwrap();
        write_bad_personas(dir.path());
        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");
        app.claude_check = ClaudeCheck::Ok;
        // The tempdir is not a git repo, so check_target fails in StartRun.
        app.apply(Transition::QuickStart(
            crate::engine::target::Target::GitDiff { base: None },
        ));
        let msg = app.status_line.clone().expect("status line set");
        assert!(msg.contains("requires a git repository"), "{msg}");
        assert!(
            !msg.contains("bad-one"),
            "persona warnings must not mask the StartRun error: {msg}"
        );
        assert!(matches!(app.screen, Screen::Home(_)), "stays on Home");
    }

    #[test]
    fn quick_start_success_carries_all_persona_warnings_prefixed() {
        let script_dir = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // A repo with a committed file carrying uncommitted changes, so
        // check_target passes and the run actually launches.
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.path().join("code.rs"), "fn main() {}\n").unwrap();
        git(&["add", "code.rs"]);
        git(&["commit", "-m", "x"]);
        std::fs::write(dir.path().join("code.rs"), "fn main() { changed(); }\n").unwrap();
        write_bad_personas(dir.path());
        let config = crate::config::Config {
            claude_bin: fake_claude_script(script_dir.path()),
            ..crate::config::Config::default()
        };
        let mut app =
            App::try_new(dir.path().to_path_buf(), config, Bootstrap::Home).expect("app builds");
        app.claude_check = ClaudeCheck::Ok;
        app.apply(Transition::QuickStart(
            crate::engine::target::Target::GitDiff { base: None },
        ));
        assert!(
            matches!(app.screen, Screen::Dashboard(_)),
            "warnings alone must not block the launch"
        );
        let msg = app.status_line.clone().expect("warnings surfaced");
        assert!(msg.starts_with("warning: "), "{msg}");
        assert!(
            msg.contains("bad-one.md") && msg.contains("bad-two.md"),
            "every skipped persona file is named: {msg}"
        );
    }

    /// A `Finalized` run with two valid round1 reviews on disk — enough for
    /// `resume_triage` to rebuild a synthesis from.
    fn finalized_record_with_round1(store: &RunStore) -> RunRecord {
        let mut record = RunRecord {
            id: "2026-07-09T12-00-00Z-reopen".into(),
            created_at: "2026-07-09T12:00:00Z".into(),
            target: crate::engine::target::Target::GitDiff { base: None },
            target_desc: "diff vs HEAD".into(),
            personas: vec!["prover".into(), "breaker".into()],
            model: None,
            cross_review: false,
            status: RunStatus::ReviewsComplete,
            degraded: vec![],
            findings_total: None,
            verdict_label: None,
            accepted_count: None,
        };
        let dir = store.create_run(&record).unwrap();
        for persona in ["prover", "breaker"] {
            dir.save_round(
                1,
                persona,
                &serde_json::json!({
                    "persona": persona,
                    "verdict": "approve",
                    "summary": "s",
                    "findings": []
                }),
                "raw",
            )
            .unwrap();
        }
        record.status = RunStatus::Finalized;
        dir.save_record(&record).unwrap();
        record
    }

    #[test]
    fn reopen_triage_enters_triage_for_finalized_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = RunStore::new(dir.path());
        let rec = finalized_record_with_round1(&store);
        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");
        app.apply(Transition::ReopenTriage {
            run_id: rec.id.clone(),
        });
        assert!(
            matches!(app.screen, Screen::Triage(_)),
            "finalized runs reopen into triage"
        );
    }

    #[test]
    fn triage_save_failure_keeps_dirty_and_surfaces_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");
        // A triage screen whose run does not exist on disk: open_run/save both fail.
        let mut state = crate::ui::triage::TriageState::new(
            "does-not-exist".to_string(),
            "diff vs HEAD".to_string(),
            crate::engine::synthesis::synthesize(&Default::default(), &Default::default(), &[]),
            crate::engine::store::Triage::new(),
        );
        state.dirty = true;
        app.screen = Screen::Triage(state);
        app.handle_key(key('z')); // unmapped key: no transition, dirty stays set into the persist block
        assert!(
            app.status_line
                .as_deref()
                .is_some_and(|s| s.contains("triage")),
            "a failed triage save must surface on the status line"
        );
        let Screen::Triage(state) = &app.screen else {
            panic!("still on triage screen");
        };
        assert!(state.dirty, "dirty must remain set when the save fails");
    }

    #[test]
    fn new_and_go_home_both_land_on_home() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");
        assert!(matches!(app.screen, Screen::Home(_)));
        app.apply(Transition::Compose {
            target: None,
            open_spec_picker: false,
        });
        app.apply(Transition::ToHome);
        assert!(matches!(app.screen, Screen::Home(_)));
    }

    #[test]
    fn store_warnings_appear_exactly_once_per_home_build() {
        let dir = tempfile::tempdir().unwrap();
        let store = RunStore::new(dir.path());
        let record = RunRecord {
            id: "2026-07-07T12-00-00Z-good".into(),
            created_at: "2026-07-07T12:00:00Z".into(),
            target: crate::engine::target::Target::GitDiff { base: None },
            target_desc: "diff vs HEAD (uncommitted)".into(),
            personas: vec!["prover".into()],
            model: None,
            cross_review: false,
            status: RunStatus::Finalized,
            degraded: vec![],
            findings_total: None,
            verdict_label: None,
            accepted_count: None,
        };
        store.create_run(&record).unwrap();
        let corrupt_id = "2026-07-08T12-00-00Z-corrupt";
        let corrupt = dir.path().join(".reviewal/runs").join(corrupt_id);
        std::fs::create_dir_all(&corrupt).unwrap();
        std::fs::write(corrupt.join("run.json"), "{not json").unwrap();

        let count_corrupt_warnings = |screen: &Screen| -> usize {
            let Screen::Home(state) = screen else {
                panic!("expected Home screen");
            };
            state
                .warnings
                .iter()
                .filter(|w| w.contains(corrupt_id))
                .count()
        };

        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");
        assert_eq!(
            count_corrupt_warnings(&app.screen),
            1,
            "initial Home build must surface the corruption warning exactly once"
        );

        app.apply(Transition::ToHome);
        assert_eq!(
            count_corrupt_warnings(&app.screen),
            1,
            "go_home must rebuild warnings fresh, not accumulate them"
        );
    }

    #[test]
    fn app_quits_on_quit_transition() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");
        app.handle_key(key('q'));
        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_c_quits_from_home() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_c_quits_from_composer() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");
        app.apply(Transition::Compose {
            target: None,
            open_spec_picker: false,
        });
        assert!(matches!(app.screen, Screen::Composer(_)));
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_c_on_live_dashboard_arms_instead_of_quitting() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");
        app.screen = Screen::Dashboard(crate::ui::dashboard::DashboardState::new(
            vec!["prover".into(), "breaker".into(), "steward".into()],
            "diff vs HEAD (uncommitted)".into(),
            "opus".into(),
            false,
        ));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        app.handle_key(ctrl_c);
        assert!(!app.should_quit, "ctrl+c must not kill a live run outright");
        let Screen::Dashboard(d) = &app.screen else {
            panic!("expected Dashboard screen");
        };
        assert!(d.cancel_armed);
    }

    #[test]
    fn open_run_resumes_reviews_complete() {
        let dir = tempfile::tempdir().unwrap();
        let store = RunStore::new(dir.path());
        let record = RunRecord {
            id: "2026-07-07T12-00-00Z-resume".into(),
            created_at: "2026-07-07T12:00:00Z".into(),
            target: crate::engine::target::Target::GitDiff { base: None },
            target_desc: "diff vs HEAD (uncommitted)".into(),
            personas: vec!["prover".into(), "breaker".into()],
            model: None,
            cross_review: false,
            status: RunStatus::ReviewsComplete,
            degraded: vec![],
            findings_total: None,
            verdict_label: None,
            accepted_count: None,
        };
        let run_dir = store.create_run(&record).unwrap();
        run_dir
            .save_round1(
                "prover",
                &serde_json::json!({
                    "persona": "prover",
                    "verdict": "approve",
                    "summary": "s",
                    "findings": [{
                        "severity": "warning",
                        "file": "a.rs",
                        "line": 1,
                        "title": "Finding one",
                        "detail": "detail",
                        "fix": null
                    }]
                }),
                "raw",
            )
            .unwrap();
        run_dir
            .save_round1(
                "breaker",
                &serde_json::json!({
                    "persona": "breaker",
                    "verdict": "approve",
                    "summary": "s",
                    "findings": []
                }),
                "raw",
            )
            .unwrap();

        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");
        let (runs, _) = app.store.list_runs();
        let rec = runs[0].clone();
        app.apply(Transition::OpenRun(rec));
        assert!(matches!(app.screen, Screen::Triage(_)));
    }

    #[test]
    fn open_run_finalized_opens_done_screen() {
        let dir = tempfile::tempdir().unwrap();
        let store = RunStore::new(dir.path());
        let record = RunRecord {
            id: "2026-07-07T12-00-00Z-final".into(),
            created_at: "2026-07-07T12:00:00Z".into(),
            target: crate::engine::target::Target::GitDiff { base: None },
            target_desc: "diff vs HEAD (uncommitted)".into(),
            personas: vec!["prover".into()],
            model: None,
            cross_review: false,
            status: RunStatus::Finalized,
            degraded: vec![],
            findings_total: None,
            verdict_label: None,
            accepted_count: None,
        };
        let run_dir = store.create_run(&record).unwrap();
        // load_done_state deserializes the strict typed Report, so the
        // fixture must be complete — partial reports don't parse.
        run_dir
            .write_report(
                "# report",
                &serde_json::json!({
                    "consensus_label": "SHIP (unanimous, 1/1)",
                    "consensus_score": 1.0,
                    "verdicts": {}, "summaries": {}, "degraded": [],
                    "findings": [
                        {"id":"a","severity":"info","title":"t","detail":"d","file":null,"line":null,"fix":null,
                         "reporters":[],"validators":[],"challengers":[],"confidence":"solo",
                         "triage":{"status":"accepted","note":null}},
                        {"id":"b","severity":"info","title":"t","detail":"d","file":null,"line":null,"fix":null,
                         "reporters":[],"validators":[],"challengers":[],"confidence":"solo",
                         "triage":{"status":"deferred","note":null}}
                    ]
                }),
            )
            .unwrap();

        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");
        let (runs, _) = app.store.list_runs();
        let rec = runs[0].clone();
        app.apply(Transition::OpenRun(rec));
        assert!(
            matches!(app.screen, Screen::Done(_)),
            "expected Done screen"
        );
        let Screen::Done(state) = &app.screen else {
            unreachable!("checked above");
        };
        assert_eq!(state.consensus_label, "SHIP (unanimous, 1/1)");
        assert_eq!((state.accepted, state.dismissed, state.deferred), (1, 0, 1));
        assert!(
            state.report_path.ends_with("report.md"),
            "{}",
            state.report_path
        );
    }

    #[test]
    fn open_run_finalized_missing_report_json_sets_status_line_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let store = RunStore::new(dir.path());
        let record = RunRecord {
            id: "2026-07-07T12-00-00Z-missing-report".into(),
            created_at: "2026-07-07T12:00:00Z".into(),
            target: crate::engine::target::Target::GitDiff { base: None },
            target_desc: "diff vs HEAD (uncommitted)".into(),
            personas: vec!["prover".into()],
            model: None,
            cross_review: false,
            status: RunStatus::Finalized,
            degraded: vec![],
            findings_total: None,
            verdict_label: None,
            accepted_count: None,
        };
        // No write_report call: report.json does not exist on disk.
        store.create_run(&record).unwrap();

        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");
        let (runs, _) = app.store.list_runs();
        let rec = runs[0].clone();
        app.apply(Transition::OpenRun(rec));
        assert!(matches!(app.screen, Screen::Home(_)), "screen unchanged");
        assert!(app.status_line.is_some());
    }

    #[test]
    fn open_run_stale_run_sets_not_resumable_status() {
        let dir = tempfile::tempdir().unwrap();
        let store = RunStore::new(dir.path());
        let record = RunRecord {
            id: "2026-07-07T12-00-00Z-stale".into(),
            created_at: "2026-07-07T12:00:00Z".into(),
            target: crate::engine::target::Target::GitDiff { base: None },
            target_desc: "diff vs HEAD (uncommitted)".into(),
            personas: vec!["prover".into()],
            model: None,
            cross_review: false,
            status: RunStatus::Running,
            degraded: vec![],
            findings_total: None,
            verdict_label: None,
            accepted_count: None,
        };
        store.create_run(&record).unwrap();

        // App::try_new runs mark_stale, flipping the leftover Running run to Stale.
        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");
        let (runs, _) = app.store.list_runs();
        let rec = runs[0].clone();
        assert_eq!(rec.status, RunStatus::Stale, "mark_stale flipped the run");
        app.apply(Transition::OpenRun(rec));
        assert!(matches!(app.screen, Screen::Home(_)), "screen unchanged");
        assert!(
            app.status_line
                .as_deref()
                .is_some_and(|s| s.contains("not resumable")),
            "expected a not-resumable notice, got: {:?}",
            app.status_line
        );
    }

    #[test]
    fn open_run_finalized_bad_triage_status_is_error_not_silent_zero() {
        let dir = tempfile::tempdir().unwrap();
        let store = RunStore::new(dir.path());
        let record = RunRecord {
            id: "2026-07-07T12-00-00Z-bogus-triage".into(),
            created_at: "2026-07-07T12:00:00Z".into(),
            target: crate::engine::target::Target::GitDiff { base: None },
            target_desc: "diff vs HEAD (uncommitted)".into(),
            personas: vec!["prover".into()],
            model: None,
            cross_review: false,
            status: RunStatus::Finalized,
            degraded: vec![],
            findings_total: None,
            verdict_label: None,
            accepted_count: None,
        };
        let run_dir = store.create_run(&record).unwrap();
        run_dir
            .write_report(
                "# report",
                &serde_json::json!({
                    "consensus_label": "SHIP (unanimous, 1/1)",
                    "consensus_score": 1.0,
                    "verdicts": {}, "summaries": {}, "degraded": [],
                    "findings": [
                        {"id":"a","severity":"info","title":"t","detail":"d","file":null,"line":null,"fix":null,
                         "reporters":[],"validators":[],"challengers":[],"confidence":"solo",
                         "triage":{"status":"bogus","note":null}}
                    ]
                }),
            )
            .unwrap();

        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");
        let (runs, _) = app.store.list_runs();
        let rec = runs[0].clone();
        app.apply(Transition::OpenRun(rec));
        assert!(
            matches!(app.screen, Screen::Home(_)),
            "a bogus triage status must not open a Done screen"
        );
        assert!(app.status_line.is_some());
    }

    #[test]
    fn finalize_writes_report_and_updates_run_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = RunStore::new(dir.path());
        let record = RunRecord {
            id: "2026-07-07T12-00-00Z-finalize".into(),
            created_at: "2026-07-07T12:00:00Z".into(),
            target: crate::engine::target::Target::GitDiff { base: None },
            target_desc: "diff vs HEAD (uncommitted)".into(),
            personas: vec!["prover".into()],
            model: None,
            cross_review: false,
            status: RunStatus::ReviewsComplete,
            degraded: vec![],
            findings_total: None,
            verdict_label: None,
            accepted_count: None,
        };
        let run_dir = store.create_run(&record).unwrap();
        run_dir
            .save_round1(
                "prover",
                &serde_json::json!({
                    "persona": "prover",
                    "verdict": "reject",
                    "summary": "s",
                    "findings": [{
                        "severity": "critical",
                        "file": "a.rs",
                        "line": 1,
                        "title": "Finding one",
                        "detail": "detail",
                        "fix": null
                    }]
                }),
                "raw",
            )
            .unwrap();

        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");

        let round1 = run_dir.load_round1().unwrap();
        let synthesis =
            crate::engine::synthesis::synthesize(&round1, &Default::default(), &record.degraded);
        let finding_id = synthesis.findings[0].id.clone();
        let mut triage = Triage::new();
        triage.insert(
            finding_id,
            crate::engine::store::TriageEntry {
                status: crate::engine::store::TriageStatus::Accepted,
                note: None,
                touched: true,
            },
        );
        app.apply(Transition::ToTriage {
            run_id: record.id.clone(),
            target_desc: record.target_desc.clone(),
            synthesis,
            triage,
        });
        assert!(matches!(app.screen, Screen::Triage(_)));

        app.apply(Transition::Finalize);

        assert!(
            matches!(app.screen, Screen::Done(_)),
            "expected Done screen after finalize"
        );
        assert!(run_dir.path.join("report.md").exists());
        assert!(run_dir.path.join("report.json").exists());

        let report_json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(run_dir.path.join("report.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            report_json["findings"][0]["triage"]["status"].as_str(),
            Some("accepted")
        );

        assert_eq!(store.latest().unwrap(), record.id);
        let rec = store.open_run(&record.id).unwrap().load_record().unwrap();
        assert_eq!(rec.status, RunStatus::Finalized);
    }

    #[test]
    fn finalize_stamps_history_metadata_on_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = RunStore::new(dir.path());
        let record = RunRecord {
            id: "2026-07-07T12-00-00Z-finalize".into(),
            created_at: "2026-07-07T12:00:00Z".into(),
            target: crate::engine::target::Target::GitDiff { base: None },
            target_desc: "diff vs HEAD (uncommitted)".into(),
            personas: vec!["prover".into()],
            model: None,
            cross_review: false,
            status: RunStatus::ReviewsComplete,
            degraded: vec![],
            findings_total: None,
            verdict_label: None,
            accepted_count: None,
        };
        let run_dir = store.create_run(&record).unwrap();
        run_dir
            .save_round1(
                "prover",
                &serde_json::json!({
                    "persona": "prover",
                    "verdict": "reject",
                    "summary": "s",
                    "findings": [{
                        "severity": "critical",
                        "file": "a.rs",
                        "line": 1,
                        "title": "Finding one",
                        "detail": "detail",
                        "fix": null
                    }]
                }),
                "raw",
            )
            .unwrap();

        let mut app = App::try_new(
            dir.path().to_path_buf(),
            crate::config::Config::default(),
            Bootstrap::Home,
        )
        .expect("app builds");

        let round1 = run_dir.load_round1().unwrap();
        let synthesis =
            crate::engine::synthesis::synthesize(&round1, &Default::default(), &record.degraded);
        let finding_id = synthesis.findings[0].id.clone();
        let mut triage = Triage::new();
        triage.insert(
            finding_id,
            crate::engine::store::TriageEntry {
                status: crate::engine::store::TriageStatus::Accepted,
                note: None,
                touched: true,
            },
        );
        app.apply(Transition::ToTriage {
            run_id: record.id.clone(),
            target_desc: record.target_desc.clone(),
            synthesis,
            triage,
        });

        app.apply(Transition::Finalize);

        let rec = store.open_run(&record.id).unwrap().load_record().unwrap();
        assert!(rec.verdict_label.is_some());
        assert_eq!(rec.accepted_count, Some(1));
        assert!(rec.findings_total.is_some());
    }

    #[test]
    fn start_run_preflight_failure_keeps_home_and_reports_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::Config {
            claude_bin: "/definitely/not/a/claude/binary".into(),
            ..crate::config::Config::default()
        };
        let mut app = App::try_new(dir.path().to_path_buf(), config.clone(), Bootstrap::Home)
            .expect("app builds");

        // Preflight checks self.config.claude_bin (not spec.claude_bin
        // directly), but every real caller sets spec.claude_bin from the
        // same config, so mirror that here.
        let spec = RunSpec {
            root: dir.path().to_path_buf(),
            target: crate::engine::target::Target::SpecFiles(vec!["missing.md".into()]),
            personas: vec![],
            model: None,
            cross_review: false,
            timeout_secs: 10,
            claude_bin: config.claude_bin.clone(),
            now_utc: "2026-07-07T12:00:00Z".into(),
        };
        app.apply(Transition::StartRun(spec));

        assert!(
            matches!(app.screen, Screen::Home(_)),
            "screen should remain Home on preflight failure"
        );
        let msg = app.status_line.clone().expect("status_line should be set");
        assert!(
            msg.contains("not found") && msg.contains("spec file not found"),
            "expected preflight error text, got: {msg}"
        );
    }

    // No CancelRun test: exercising it meaningfully requires a live engine
    // run in Dashboard state, which would need a real or long-running fake
    // `claude` process to observe an effect — skipped.
}
