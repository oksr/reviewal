use crate::engine::store::{RunRecord, RunStatus};
use crate::engine::target::{DetectedTarget, Target};
use crate::ui::app::{ClaudeCheck, Transition};
use crate::ui::personas::{armed_delete_label, invalid_tag, provenance_tag, PersonaManager};
use crate::ui::theme::Theme;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Padding, Paragraph};
use ratatui::Frame;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HomeTab {
    Start,
    Personas,
    History,
}

const TAB_ORDER: [HomeTab; 3] = [HomeTab::Start, HomeTab::Personas, HomeTab::History];

fn tab_label(tab: HomeTab) -> &'static str {
    match tab {
        HomeTab::Start => "start a review",
        HomeTab::Personas => "personas",
        HomeTab::History => "history",
    }
}

pub(crate) struct HomeState {
    /// `launcher_idx` ranges over `0..=targets.len()`; `idx == targets.len()`
    /// is the trailing "spec files…" row.
    pub targets: Vec<DetectedTarget>,
    pub spec_count: usize,
    pub runs: Vec<RunRecord>,
    pub tab: HomeTab,
    pub launcher_idx: usize,
    pub history_idx: usize,
    /// The persona library (no target filter) behind the personas tab;
    /// stages `$EDITOR` requests that `run_tui` drains through `App`.
    pub personas: PersonaManager,
    pub warnings: Vec<String>,
    pub skill_installed: bool,
    pub defaults_code: Vec<String>,
    pub defaults_spec: Vec<String>,
    pub show_help: bool,
}

fn help_entries(tab: HomeTab) -> &'static [(&'static str, &'static str)] {
    match tab {
        HomeTab::Start => &[
            ("enter", "start with defaults"),
            ("e", "edit setup before launch"),
            ("n", "new review"),
            ("tab/1·2·3", "switch tab"),
            ("j/k", "move"),
            ("?", "help"),
            ("q", "quit"),
        ],
        HomeTab::Personas => &[
            ("enter/v", "view persona"),
            ("e", "edit (built-ins copy first)"),
            ("n", "new from template"),
            ("d", "duplicate"),
            ("x", "delete (x again confirms)"),
            ("tab/1·2·3", "switch tab"),
            ("j/k", "move"),
            ("?", "help"),
            ("q", "quit"),
        ],
        HomeTab::History => &[
            ("enter", "open report / resume triage"),
            ("tab/1·2·3", "switch tab"),
            ("j/k", "move"),
            ("?", "help"),
            ("q", "quit"),
        ],
    }
}

impl HomeState {
    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Option<Transition> {
        // The overlay swallows every key that closes it — including one
        // that would otherwise start a run — so a mistimed keystroke never
        // fires an action the user couldn't see coming.
        if self.show_help {
            self.show_help = false;
            return None;
        }
        if self.tab == HomeTab::Personas {
            // Same one-shot notice and overlay routing as the composer's
            // checklist, so the two persona surfaces feel identical.
            self.personas.notice = None;
            if self.personas.pager.is_some() {
                self.personas.handle_pager_key(key);
                return None;
            }
            if self.personas.scope_prompt.is_some() {
                self.personas.handle_scope_key(key);
                return None;
            }
            if self.personas.handle_armed_key(key) {
                return None;
            }
        }
        match key.code {
            KeyCode::Char('q') => return Some(Transition::Quit),
            KeyCode::Char('?') => {
                self.show_help = true;
                return None;
            }
            KeyCode::Tab => {
                self.cycle_tab(1);
                return None;
            }
            KeyCode::BackTab => {
                self.cycle_tab(-1);
                return None;
            }
            KeyCode::Char('1') => {
                self.tab = HomeTab::Start;
                return None;
            }
            KeyCode::Char('2') => {
                self.tab = HomeTab::Personas;
                return None;
            }
            KeyCode::Char('3') => {
                self.tab = HomeTab::History;
                return None;
            }
            _ => {}
        }
        match self.tab {
            HomeTab::Start => self.handle_start_key(key),
            HomeTab::Personas => {
                self.handle_personas_key(key);
                None
            }
            HomeTab::History => self.handle_history_key(key),
        }
    }

    fn cycle_tab(&mut self, delta: i32) {
        let i = TAB_ORDER.iter().position(|t| *t == self.tab).unwrap_or(0) as i32;
        let n = TAB_ORDER.len() as i32;
        self.tab = TAB_ORDER[((i + delta).rem_euclid(n)) as usize];
    }

    fn handle_start_key(&mut self, key: KeyEvent) -> Option<Transition> {
        let spec_idx = self.targets.len();
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.launcher_idx = (self.launcher_idx + 1).min(spec_idx);
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.launcher_idx = self.launcher_idx.saturating_sub(1);
                None
            }
            KeyCode::Enter => {
                if self.launcher_idx == spec_idx {
                    Some(Transition::Compose {
                        target: None,
                        open_spec_picker: true,
                    })
                } else {
                    self.targets
                        .get(self.launcher_idx)
                        .map(|t| Transition::QuickStart(t.target.clone()))
                }
            }
            KeyCode::Char('e') => {
                let is_spec_row = self.launcher_idx == spec_idx;
                Some(Transition::Compose {
                    target: (!is_spec_row)
                        .then(|| self.targets.get(self.launcher_idx))
                        .flatten()
                        .map(|t| t.target.clone()),
                    open_spec_picker: is_spec_row,
                })
            }
            KeyCode::Char('n') => Some(Transition::Compose {
                target: None,
                open_spec_picker: false,
            }),
            _ => None,
        }
    }

    fn handle_personas_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.personas.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.personas.move_cursor(-1),
            KeyCode::Enter => self.personas.open_pager(),
            _ => {
                self.personas.handle_verb_key(key);
            }
        }
    }

    fn handle_history_key(&mut self, key: KeyEvent) -> Option<Transition> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.history_idx = (self.history_idx + 1).min(self.runs.len().saturating_sub(1));
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.history_idx = self.history_idx.saturating_sub(1);
                None
            }
            KeyCode::Enter => self
                .runs
                .get(self.history_idx)
                .cloned()
                .map(Transition::OpenRun),
            _ => None,
        }
    }

    /// Runs awaiting triage — the history tab's badge count.
    fn triage_waiting(&self) -> usize {
        self.runs
            .iter()
            .filter(|r| matches!(r.status, RunStatus::ReviewsComplete))
            .count()
    }
}

/// Must mirror `App::apply`'s `OpenRun` match so the hint never promises
/// what the action won't deliver; deliberately exhaustive so a new
/// `RunStatus` variant has to decide its Enter behavior here too.
enum EnterAction {
    Open(&'static str),
    Blocked(&'static str),
}

fn enter_action(status: &RunStatus) -> EnterAction {
    match status {
        RunStatus::Finalized => EnterAction::Open("open report"),
        RunStatus::ReviewsComplete => EnterAction::Open("resume triage"),
        RunStatus::Running => EnterAction::Blocked("run in progress elsewhere"),
        RunStatus::Stale | RunStatus::Aborted => {
            EnterAction::Blocked("not resumable (stale/aborted)")
        }
    }
}

pub(crate) fn draw(
    f: &mut Frame,
    area: Rect,
    state: &HomeState,
    claude_check: &ClaudeCheck,
    model_label: &str,
    theme: &Theme,
) {
    let warn_height = state.warnings.len() as u16;
    let tip_height = u16::from(!state.skill_installed);

    // Every tab hugs its content, capped by the space above the footer —
    // a long history gets the full height (rows past it clip as before),
    // while a short one never drags an empty frame to the hints row. The
    // filler below the box keeps the hints bottom-anchored either way.
    let fixed = 1 + 1 + tip_height + warn_height + 1;
    let avail = area.height.saturating_sub(fixed).max(3);
    let content_height = match state.tab {
        HomeTab::Start => {
            let selected_is_diff = state.launcher_idx < state.targets.len();
            let detail_lines = 1 + u16::from(selected_is_diff); // personas line (+ files line)
            ((state.targets.len() + 1).max(1) as u16 + 2 + detail_lines).min(avail)
        }
        HomeTab::Personas => (state.personas.row_count().max(1) as u16 + 2).min(avail),
        HomeTab::History => (state.runs.len().max(1) as u16 + 2).min(avail),
    };

    let [header, _gap, content, _filler, tip, warn_area, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(content_height),
        Constraint::Min(0),
        Constraint::Length(tip_height),
        Constraint::Length(warn_height),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(f, header, claude_check, model_label, theme);

    let block = crate::ui::theme::bordered()
        .border_style(theme.accent_style())
        .title(crate::ui::theme::inset_title(
            tab_bar(state, theme),
            theme.accent_style(),
        ))
        .padding(Padding::horizontal(1));
    let inner = block.inner(content);
    f.render_widget(block, content);
    match state.tab {
        HomeTab::Start => draw_start(f, inner, state, theme),
        HomeTab::Personas => draw_personas(f, inner, state, theme),
        HomeTab::History => draw_history(f, inner, state, theme),
    }

    if !state.skill_installed {
        f.render_widget(
            Paragraph::new(Line::styled(
                "tip: run `reviewal init` once — installs the /reviewal-ingest skill and gitignores runs/",
                Style::default().fg(theme.severity_warning),
            )),
            tip,
        );
    }
    if !state.warnings.is_empty() {
        let lines: Vec<Line> = state
            .warnings
            .iter()
            .map(|w| Line::styled(w.clone(), Style::default().fg(theme.severity_warning)))
            .collect();
        f.render_widget(Paragraph::new(lines), warn_area);
    }
    draw_hints(f, hints, state, theme);

    if state.show_help {
        crate::ui::overlay::draw_help(f, area, help_entries(state.tab), theme);
    }
    if state.tab == HomeTab::Personas {
        crate::ui::personas::draw_pager(f, area, &state.personas, theme);
        crate::ui::personas::draw_scope(f, area, &state.personas, theme);
    }
}

/// The tab bar, set into the content box's border: active tab accent-bold,
/// inactive dim, and a triage-debt badge on history so it stays visible
/// from every tab.
fn tab_bar(state: &HomeState, theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, tab) in TAB_ORDER.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", theme.dim_style()));
        }
        let style = if *tab == state.tab {
            theme.title_style()
        } else {
            theme.dim_style()
        };
        spans.push(Span::styled(tab_label(*tab), style));
        if *tab == HomeTab::History {
            let waiting = state.triage_waiting();
            if waiting > 0 {
                spans.push(Span::styled(
                    format!(" \u{25d0} {waiting}"),
                    Style::default().fg(theme.run_status_reviews_complete),
                ));
            }
        }
    }
    Line::from(spans)
}

fn draw_header(
    f: &mut Frame,
    area: Rect,
    claude_check: &ClaudeCheck,
    model_label: &str,
    theme: &Theme,
) {
    f.render_widget(
        Paragraph::new(Line::styled("\u{2726} reviewal", theme.title_style())),
        area,
    );
    let (text, style) = match claude_check {
        ClaudeCheck::Ok => (
            format!("model {model_label} · claude cli \u{2713}"),
            theme.dim_style(),
        ),
        ClaudeCheck::Checking => (
            format!("model {model_label} · claude cli …"),
            theme.dim_style(),
        ),
        ClaudeCheck::Failed(e) => (
            format!("claude cli \u{2717} {e}"),
            Style::default().fg(theme.error),
        ),
    };
    f.render_widget(
        Paragraph::new(Line::styled(text, style)).alignment(Alignment::Right),
        area,
    );
}

/// Assembles a launcher row: a pointer gutter, the content spans, and — when
/// selected — the quick-start hint right-aligned, all under the selection
/// tint so per-span colors survive.
fn selectable_row(
    content: Vec<Span<'static>>,
    selected: bool,
    width: u16,
    theme: &Theme,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(content.len() + 3);
    spans.push(if selected {
        Span::styled(
            "\u{276f} ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("  ")
    });
    spans.extend(content);
    if !selected {
        return Line::from(spans);
    }
    const SUFFIX: &str = "enter starts with defaults";
    let width = width as usize;
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if used + 1 + SUFFIX.chars().count() <= width {
        spans.push(Span::raw(" ".repeat(width - used - SUFFIX.chars().count())));
        spans.push(Span::styled(SUFFIX, theme.dim_style()));
    } else if width > used {
        spans.push(Span::raw(" ".repeat(width - used)));
    }
    let sel = theme.selection_style();
    Line::from(
        spans
            .into_iter()
            .map(|s| Span::styled(s.content, s.style.patch(sel)))
            .collect::<Vec<_>>(),
    )
}

/// The launch consequences of the selected row: which personas run and the
/// cross-review default — a dim footnote inside the box, under the row it
/// describes.
fn launch_detail_line(state: &HomeState, is_spec_row: bool, theme: &Theme) -> Line<'static> {
    let names = if is_spec_row {
        &state.defaults_spec
    } else {
        &state.defaults_code
    };
    let mut spans = vec![Span::styled("    personas: ", theme.dim_style())];
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", theme.dim_style()));
        }
        spans.push(Span::styled(
            name.clone(),
            Style::default().fg(theme.persona_color(name, None)),
        ));
    }
    spans.push(Span::styled(" · cross-review off", theme.dim_style()));
    Line::from(spans)
}

fn draw_start(f: &mut Frame, inner: Rect, state: &HomeState, theme: &Theme) {
    let mut lines: Vec<Line> = Vec::new();
    if state.targets.is_empty() && state.spec_count == 0 {
        lines.push(Line::styled(
            "nothing to review here — not a git repo and no *.md files found",
            theme.dim_style(),
        ));
    } else {
        for (i, t) in state.targets.iter().enumerate() {
            let selected = state.launcher_idx == i;
            let content = vec![
                Span::raw(t.label.clone()),
                Span::styled(format!(" — {} files · ", t.files.len()), theme.dim_style()),
                Span::styled(
                    format!("+{}", t.additions),
                    Style::default().fg(theme.status_done),
                ),
                Span::raw(" "),
                Span::styled(
                    format!("\u{2212}{}", t.deletions),
                    Style::default().fg(theme.error),
                ),
            ];
            lines.push(selectable_row(content, selected, inner.width, theme));
            if selected {
                let names: Vec<&str> = t.files.iter().take(3).map(String::as_str).collect();
                lines.push(Line::styled(
                    format!("    {}", names.join(" · ")),
                    theme.dim_style(),
                ));
                lines.push(launch_detail_line(state, false, theme));
            }
        }
        let spec_idx = state.targets.len();
        let selected = state.launcher_idx == spec_idx;
        let content = vec![
            Span::raw("spec files\u{2026}"),
            Span::styled(
                format!(" — {} *.md in repo", state.spec_count),
                theme.dim_style(),
            ),
        ];
        lines.push(selectable_row(content, selected, inner.width, theme));
        if selected {
            lines.push(launch_detail_line(state, true, theme));
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// Display columns for the persona-name cell in a roster row.
const NAME_COLS: usize = 14;

fn draw_personas(f: &mut Frame, inner: Rect, state: &HomeState, theme: &Theme) {
    let mgr = &state.personas;
    let mut lines: Vec<Line> = Vec::new();
    for (i, c) in mgr.personas.iter().enumerate() {
        let selected = mgr.cursor == i;
        let name = crate::ui::format::truncate_end(&c.persona.name, NAME_COLS - 2);
        let target = match c.persona.target {
            crate::engine::persona::PersonaTarget::Code => "code",
            crate::engine::persona::PersonaTarget::Spec => "spec",
            crate::engine::persona::PersonaTarget::Both => "both",
        };
        let tag = provenance_tag(mgr, &c.persona);
        let color = theme.persona_color(&c.persona.name, c.persona.color.as_deref());
        let mut spans = vec![
            if selected {
                Span::styled(
                    "\u{276f} ",
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("  ")
            },
            Span::styled(format!("{name:<NAME_COLS$}"), Style::default().fg(color)),
            Span::styled(format!("{target:<6}"), theme.dim_style()),
        ];
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let avail = (inner.width as usize)
            .saturating_sub(used)
            .saturating_sub(tag.chars().count() + 2);
        let lens = crate::ui::format::truncate_end(&c.persona.lens, avail);
        let pad = avail.saturating_sub(lens.chars().count()) + 2;
        spans.push(Span::styled(lens, theme.dim_style()));
        spans.push(Span::styled(
            format!("{}{tag}", " ".repeat(pad)),
            theme.dim_style(),
        ));
        lines.push(finish_roster_row(spans, selected, inner.width, theme));
    }
    for (j, row) in mgr.invalid.iter().enumerate() {
        let i = mgr.personas.len() + j;
        let selected = mgr.cursor == i;
        let stem = row
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let stem = crate::ui::format::truncate_end(&stem, NAME_COLS - 2);
        let tag = invalid_tag(mgr, &row.path);
        let err_avail = (inner.width as usize)
            .saturating_sub(2 + NAME_COLS + 6)
            .saturating_sub(tag.chars().count() + 2);
        let err = crate::ui::format::truncate_end(&row.error, err_avail);
        let pad = err_avail.saturating_sub(err.chars().count()) + 2;
        let spans = vec![
            Span::styled(
                if selected { "\u{276f} " } else { "! " }.to_string(),
                Style::default().fg(theme.error),
            ),
            Span::styled(
                format!("{stem:<NAME_COLS$}"),
                Style::default().fg(theme.error),
            ),
            Span::styled(format!("{:<6}", "—"), Style::default().fg(theme.error)),
            Span::styled(err, Style::default().fg(theme.error)),
            Span::styled(format!("{}{tag}", " ".repeat(pad)), theme.dim_style()),
        ];
        lines.push(finish_roster_row(spans, selected, inner.width, theme));
    }
    if lines.is_empty() {
        lines.push(Line::styled("no personas found", theme.dim_style()));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// Pads a roster row to the box width and applies the selection tint.
fn finish_roster_row(
    mut spans: Vec<Span<'static>>,
    selected: bool,
    width: u16,
    theme: &Theme,
) -> Line<'static> {
    if !selected {
        return Line::from(spans);
    }
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if (width as usize) > used {
        spans.push(Span::raw(" ".repeat(width as usize - used)));
    }
    let sel = theme.selection_style();
    Line::from(
        spans
            .into_iter()
            .map(|s| Span::styled(s.content, s.style.patch(sel)))
            .collect::<Vec<_>>(),
    )
}

/// Display columns for the target text in a history row.
const TARGET_COLS: usize = 28;

/// The run's target, stripped of the `spec:`/`diff` word that now lives in
/// the kind column; spec paths keep their tail, diff descriptions their head.
fn target_text(r: &RunRecord) -> String {
    match &r.target {
        Target::SpecFiles(_) => {
            let rest = r
                .target_desc
                .strip_prefix("spec: ")
                .unwrap_or(&r.target_desc);
            crate::ui::format::truncate_path_start(rest, TARGET_COLS)
        }
        Target::GitDiff { .. } => {
            let rest = r
                .target_desc
                .strip_prefix("diff ")
                .unwrap_or(&r.target_desc);
            crate::ui::format::truncate_end(rest, TARGET_COLS)
        }
    }
}

fn draw_history(f: &mut Frame, inner: Rect, state: &HomeState, theme: &Theme) {
    let mut lines: Vec<Line> = Vec::new();
    if state.runs.is_empty() {
        lines.push(Line::styled(
            "no runs yet — your first review will appear here",
            theme.dim_style(),
        ));
    } else {
        let now = time::OffsetDateTime::now_utc();
        for (i, r) in state.runs.iter().enumerate() {
            let selected = state.history_idx == i;
            let rel = crate::ui::format::relative_time(&r.created_at, now);
            let kind = match &r.target {
                Target::SpecFiles(_) => "spec",
                Target::GitDiff { .. } => "diff",
            };
            // Dead runs recede entirely; the eye should land on finished work.
            let muted = matches!(r.status, RunStatus::Aborted | RunStatus::Stale);
            let target_style = if muted {
                theme.dim_style()
            } else {
                Style::default()
            };
            let mut spans = vec![
                if selected {
                    Span::styled("\u{258c} ", theme.accent_style())
                } else {
                    Span::raw("  ")
                },
                Span::styled(format!("{rel:<11}"), theme.dim_style()),
                Span::styled(format!("{kind:<6}"), theme.dim_style()),
                Span::styled(
                    format!("{:<width$}", target_text(r), width = TARGET_COLS + 2),
                    target_style,
                ),
            ];
            let prefix_cols: usize = spans.iter().map(|s| s.content.chars().count()).sum();
            let avail = (inner.width as usize).saturating_sub(prefix_cols);
            spans.extend(status_cell(r, theme, avail));
            if selected {
                let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
                if (inner.width as usize) > used {
                    spans.push(Span::raw(" ".repeat(inner.width as usize - used)));
                }
                let sel = theme.selection_style();
                spans = spans
                    .into_iter()
                    .map(|s| Span::styled(s.content, s.style.patch(sel)))
                    .collect();
            }
            lines.push(Line::from(spans));
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// Badge column width: the widest badge (`● finalized`, 11 cols) plus one,
/// so the dim metadata suffixes align across rows.
const BADGE_COLS: usize = 12;

/// Short badge word for a consensus verdict label — the vote-split detail
/// stays in the report view. Prefix order matters: SHIP-WITH-CAVEATS starts
/// with "SHIP". Unknown labels fall back to their first word.
fn verdict_badge(label: &str) -> &str {
    if label.starts_with("SHIP-WITH-CAVEATS") {
        "CAVEATS"
    } else if label.starts_with("SHIP") {
        "SHIP"
    } else if label.starts_with("HOLD") {
        "HOLD"
    } else if label.starts_with("BLOCK") {
        "BLOCK"
    } else {
        label.split_whitespace().next().unwrap_or("finalized")
    }
}

fn status_cell(r: &RunRecord, theme: &Theme, avail: usize) -> Vec<Span<'static>> {
    let (label, style, suffix): (String, Style, Option<String>) = match r.status {
        RunStatus::Finalized => {
            let (word, style) = match &r.verdict_label {
                Some(v) => (verdict_badge(v).to_string(), theme.verdict(v)),
                None => (
                    "finalized".into(),
                    Style::default()
                        .fg(theme.run_status(&r.status))
                        .add_modifier(Modifier::BOLD),
                ),
            };
            let mut parts = Vec::new();
            if let Some(n) = r.findings_total {
                parts.push(format!("{n} findings"));
            }
            if let Some(n) = r.accepted_count {
                parts.push(format!("{n} accepted"));
            }
            let suffix = (!parts.is_empty()).then(|| parts.join(" · "));
            (format!("\u{25cf} {word}"), style, suffix)
        }
        RunStatus::ReviewsComplete => (
            "\u{25d0} triage \u{25b8}".into(),
            Style::default()
                .fg(theme.run_status(&r.status))
                .add_modifier(Modifier::BOLD),
            r.findings_total.map(|n| format!("{n} waiting")),
        ),
        RunStatus::Running => (
            "\u{25cc} running".into(),
            Style::default().fg(theme.run_status(&r.status)),
            None,
        ),
        // Aborted/Stale read as muted "not resumable" rather than an alert
        // color — dim, not their (Red/Gray) `run_status` role colors.
        RunStatus::Aborted | RunStatus::Stale => (
            format!("\u{25cb} {}", r.status.label()),
            theme.dim_style(),
            None,
        ),
    };
    let label = format!("{label:<BADGE_COLS$}");
    fit_status_cell(label, style, suffix, avail, theme)
}

/// Sacrifices the metadata suffix first, then the label's alignment padding;
/// an over-wide label is ellipsized at a char boundary, never bare-clipped.
fn fit_status_cell(
    label: String,
    style: Style,
    suffix: Option<String>,
    avail: usize,
    theme: &Theme,
) -> Vec<Span<'static>> {
    if avail == 0 {
        return vec![];
    }
    let label_len = label.chars().count();
    if let Some(sfx) = suffix {
        if label_len + sfx.chars().count() <= avail {
            return vec![
                Span::styled(label, style),
                Span::styled(sfx, theme.dim_style()),
            ];
        }
    }
    let trimmed = label.trim_end().to_string();
    let trimmed_len = trimmed.chars().count();
    if trimmed_len <= avail {
        return vec![Span::styled(trimmed, style)];
    }
    let cut: String = trimmed.chars().take(avail - 1).collect();
    vec![Span::styled(format!("{cut}…"), style)]
}

fn draw_hints(f: &mut Frame, area: Rect, state: &HomeState, theme: &Theme) {
    let spans: Vec<Span> = match state.tab {
        HomeTab::Start => {
            let pairs: Vec<(&str, &str)> = vec![
                ("enter", "start"),
                ("e", "edit setup"),
                ("tab", "personas"),
                ("?", "help"),
                ("q", "quit"),
            ];
            theme.hint_spans(&pairs)
        }
        HomeTab::Personas => {
            // Armed-delete confirmation and one-shot notices replace the
            // verb hints — same footer contract as the composer checklist.
            if let Some(armed) = state.personas.armed_delete {
                let name = state
                    .personas
                    .personas
                    .get(armed)
                    .map(|c| c.persona.name.clone())
                    .or_else(|| {
                        state
                            .personas
                            .invalid
                            .get(armed.saturating_sub(state.personas.personas.len()))
                            .and_then(|r| {
                                r.path.file_stem().map(|s| s.to_string_lossy().into_owned())
                            })
                    })
                    .unwrap_or_default();
                vec![Span::styled(
                    format!(
                        "x {}",
                        armed_delete_label(&name, state.personas.armed_delete_shadows_global)
                    ),
                    Style::default().fg(theme.severity_warning),
                )]
            } else if let Some(notice) = &state.personas.notice {
                vec![Span::styled(
                    notice.clone(),
                    Style::default().fg(theme.severity_warning),
                )]
            } else {
                theme.hint_spans(&[
                    ("v", "view"),
                    ("e", "edit"),
                    ("n", "new"),
                    ("d", "duplicate"),
                    ("x", "delete"),
                    ("tab", "history"),
                    ("q", "quit"),
                ])
            }
        }
        HomeTab::History => {
            let mut spans = Vec::new();
            match state
                .runs
                .get(state.history_idx)
                .map(|r| enter_action(&r.status))
            {
                Some(EnterAction::Open(desc)) => {
                    spans.extend(theme.hint_spans(&[("enter", desc)]));
                }
                Some(EnterAction::Blocked(reason)) => {
                    spans.push(Span::styled(
                        reason,
                        Style::default().fg(theme.severity_warning),
                    ));
                }
                None => {}
            }
            if !spans.is_empty() {
                spans.push(Span::styled(" · ", theme.dim_style()));
            }
            spans.extend(theme.hint_spans(&[("tab", "start a review"), ("q", "quit")]));
            spans
        }
    };
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::target::Target;
    use crate::ui::app::render_to_text;
    use crate::ui::test_keys::{key, key_code};

    fn detected_head_target_with(files: usize, additions: u64, deletions: u64) -> DetectedTarget {
        DetectedTarget {
            target: Target::GitDiff { base: None },
            label: "diff vs HEAD (uncommitted)".into(),
            files: (0..files).map(|i| format!("f{i}.rs")).collect(),
            additions,
            deletions,
        }
    }

    fn detected_head_target() -> DetectedTarget {
        detected_head_target_with(1, 3, 1)
    }

    fn record(id: &str, status: RunStatus) -> RunRecord {
        RunRecord {
            id: id.into(),
            created_at: "2026-07-10T08:00:00Z".into(),
            target: Target::GitDiff { base: None },
            target_desc: "diff vs HEAD (uncommitted)".into(),
            personas: vec!["prover".into()],
            model: None,
            cross_review: false,
            status,
            degraded: vec![],
            findings_total: None,
            verdict_label: None,
            accepted_count: None,
        }
    }

    fn state_with(targets: Vec<DetectedTarget>, runs: Vec<RunRecord>) -> HomeState {
        HomeState {
            targets,
            spec_count: 0,
            runs,
            tab: HomeTab::Start,
            personas: PersonaManager::new(
                std::path::Path::new("/nonexistent-home-fixture"),
                &crate::config::Config::default(),
                None,
            ),
            launcher_idx: 0,
            history_idx: 0,
            warnings: vec![],
            skill_installed: true,
            defaults_code: vec!["prover".into(), "breaker".into(), "steward".into()],
            defaults_spec: vec!["prover".into(), "skeptic".into(), "advocate".into()],
            show_help: false,
        }
    }

    #[test]
    fn enter_on_diff_row_quick_starts_with_that_target() {
        let mut s = state_with(vec![detected_head_target()], vec![]);
        match s.handle_key(key_code(KeyCode::Enter)) {
            Some(Transition::QuickStart(Target::GitDiff { base: None })) => {}
            other => panic!("expected QuickStart, got {other:?}"),
        }
    }

    #[test]
    fn enter_on_spec_row_opens_composer_with_picker() {
        let mut s = state_with(vec![], vec![]); // no diffs → row 0 is the spec row
        assert!(matches!(
            s.handle_key(key_code(KeyCode::Enter)),
            Some(Transition::Compose {
                target: None,
                open_spec_picker: true,
            })
        ));
    }

    #[test]
    fn e_opens_composer_seeded_with_the_hovered_diff_target() {
        let mut s = state_with(vec![detected_head_target()], vec![]);
        match s.handle_key(key('e')) {
            Some(Transition::Compose {
                target: Some(Target::GitDiff { base: None }),
                open_spec_picker: false,
            }) => {}
            other => panic!("expected Compose seeded with the hovered diff, got {other:?}"),
        }
    }

    #[test]
    fn e_on_spec_row_opens_composer_with_picker() {
        let mut s = state_with(vec![], vec![]); // no diffs → row 0 is the spec row
        match s.handle_key(key('e')) {
            Some(Transition::Compose {
                target: None,
                open_spec_picker: true,
            }) => {}
            other => panic!("expected Compose with the picker open, got {other:?}"),
        }
    }

    #[test]
    fn n_opens_a_blank_composer() {
        let mut s = state_with(vec![detected_head_target()], vec![]);
        assert!(matches!(
            s.handle_key(key('n')),
            Some(Transition::Compose {
                target: None,
                open_spec_picker: false,
            })
        ));
    }

    #[test]
    fn launcher_row_shows_stats_and_selected_defaults_hint() {
        let s = state_with(vec![detected_head_target_with(3, 214, 38)], vec![]);
        let text = render_to_text(100, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        assert!(
            text.contains("diff vs HEAD (uncommitted) — 3 files · +214 −38"),
            "{text}"
        );
        assert!(text.contains("enter starts with defaults"), "{text}");
        assert!(text.contains("spec files…"), "{text}");
    }

    #[test]
    fn empty_history_and_missing_skill_show_placeholder_and_tip() {
        let mut s = state_with(vec![], vec![]);
        s.skill_installed = false;
        s.tab = HomeTab::History;
        let text = render_to_text(100, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        assert!(
            text.contains("no runs yet — your first review will appear here"),
            "{text}"
        );
        assert!(text.contains("tip: run `reviewal init` once"), "{text}");
        assert!(
            !text.contains("j/k"),
            "footer hints never mention j/k — arrows are self-evident: {text}"
        );
    }

    #[test]
    fn preflight_failure_lands_in_header() {
        let s = state_with(vec![], vec![]);
        // A realistic bare PreflightError message — draw_header prepends
        // the "claude cli ✗ " marker itself, so the fixture must not bake
        // it in (a prefixed fixture would mask a double-prefix bug).
        let check = ClaudeCheck::Failed(
            "installed claude CLI is too old (missing --json-schema) — run: claude update".into(),
        );
        let text = render_to_text(100, 30, |f| {
            draw(f, f.area(), &s, &check, "opus", &Theme::default())
        });
        assert!(
            text.contains("claude cli \u{2717} installed claude CLI is too old"),
            "{text}"
        );
        assert_eq!(
            text.matches('\u{2717}').count(),
            1,
            "the failure marker renders exactly once: {text}"
        );
    }

    fn history_fixture_state() -> HomeState {
        let mut finalized = record("2026-07-10T08-00-00Z-diff-main", RunStatus::Finalized);
        finalized.verdict_label = Some("SHIP-WITH-CAVEATS (2/3 ship, 1/3 block)".into());
        finalized.findings_total = Some(12);
        finalized.accepted_count = Some(8);
        let mut waiting = record("2026-07-10T05-00-00Z-spec", RunStatus::ReviewsComplete);
        waiting.target = Target::SpecFiles(vec!["docs/superpowers/specs".into()]);
        waiting.target_desc = "spec: docs/superpowers/specs".into();
        waiting.findings_total = Some(9);
        state_with(vec![], vec![finalized, waiting])
    }

    #[test]
    fn history_rows_show_badges_kind_tags_and_metadata() {
        let mut s = history_fixture_state();
        s.tab = HomeTab::History;
        // 94 cols models production: draw is called directly here, so we
        // simulate what a 100-col terminal leaves after draw_app's margins.
        let text = render_to_text(94, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        assert!(text.contains("\u{25cf} CAVEATS"), "{text}");
        assert!(
            !text.contains("SHIP-WITH-CAVEATS"),
            "vote-split detail stays in the report view: {text}"
        );
        assert!(text.contains("12 findings · 8 accepted"), "{text}");
        assert!(text.contains("\u{25d0} triage \u{25b8}"), "{text}");
        assert!(text.contains("9 waiting"), "{text}");
        let diff_row = text
            .lines()
            .find(|l| l.contains("CAVEATS"))
            .expect("diff row rendered");
        assert!(
            diff_row.contains("diff  vs HEAD (uncommitted)"),
            "kind column + stripped target: {diff_row:?}"
        );
        let spec_row = text
            .lines()
            .find(|l| l.contains("triage"))
            .expect("spec row rendered");
        assert!(
            spec_row.contains("spec  docs/superpowers/specs"),
            "spec kind column without the `spec:` prefix: {spec_row:?}"
        );
        assert!(
            !text.contains("2026-07-10T08-00-00Z"),
            "raw ids replaced by relative times: {text}"
        );
    }

    #[test]
    fn history_status_cell_drops_metadata_at_narrow_width() {
        let mut s = history_fixture_state();
        s.tab = HomeTab::History;
        // 76 cols ≈ an 80-col terminal after draw_app's margins; borders,
        // padding, and the 47-col gutter/time/kind/target prefix leave too
        // little room for the metadata suffix — the badge survives whole.
        let text = render_to_text(76, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        assert!(text.contains("\u{25cf} CAVEATS"), "{text}");
        assert!(
            !text.contains("finding"),
            "metadata that cannot fit whole is dropped, not clipped: {text}"
        );
    }

    #[test]
    fn fit_status_cell_sheds_suffix_then_padding_then_ellipsizes() {
        let theme = Theme::default();
        let cell = |avail| {
            fit_status_cell(
                format!("{:<BADGE_COLS$}", "\u{25cf} CAVEATS"),
                Style::default(),
                Some("12 findings".into()),
                avail,
                &theme,
            )
        };
        let text =
            |spans: Vec<Span>| -> String { spans.iter().map(|s| s.content.as_ref()).collect() };
        assert_eq!(text(cell(23)), "\u{25cf} CAVEATS   12 findings");
        assert_eq!(
            text(cell(22)),
            "\u{25cf} CAVEATS",
            "suffix dropped first, padding shed with it"
        );
        assert_eq!(
            text(cell(5)),
            "\u{25cf} CA\u{2026}",
            "over-wide badge ellipsizes, never bare-clips"
        );
        assert!(text(cell(0)).is_empty());
    }

    #[test]
    fn history_status_cell_shows_full_metadata_at_wide_width() {
        let mut s = history_fixture_state();
        s.tab = HomeTab::History;
        let text = render_to_text(120, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        assert!(text.contains("\u{25cf} CAVEATS"), "{text}");
        assert!(text.contains("12 findings · 8 accepted"), "{text}");
        assert!(text.contains("9 waiting"), "{text}");
    }

    #[test]
    fn verdict_badge_maps_labels_and_falls_back_to_first_word() {
        assert_eq!(verdict_badge("SHIP-WITH-CAVEATS (2/3 ship)"), "CAVEATS");
        assert_eq!(verdict_badge("SHIP (unanimous, 2/2)"), "SHIP");
        assert_eq!(verdict_badge("HOLD — split decision"), "HOLD");
        assert_eq!(verdict_badge("BLOCK (2/2 block)"), "BLOCK");
        assert_eq!(verdict_badge("SOMETHING else"), "SOMETHING");
        assert_eq!(verdict_badge(""), "finalized");
    }

    #[test]
    fn start_tab_box_hugs_content_and_history_tab_fills() {
        // Start tab: spec row + its personas detail line + borders = 4 rows.
        let s = history_fixture_state();
        let text = render_to_text(94, 40, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        let lines: Vec<&str> = text.lines().collect();
        let top = lines
            .iter()
            .position(|l| l.contains("start a review"))
            .expect("tab bar rendered");
        let bottom = lines
            .iter()
            .rposition(|l| l.contains('\u{2570}'))
            .expect("bottom border rendered");
        assert_eq!(
            bottom - top,
            3,
            "content-fit, not a frame to the footer: {text}"
        );

        // History tab hugs too: two runs → a 4-row box; a history taller
        // than the screen takes the full height instead (clipping beyond).
        let mut s = history_fixture_state();
        s.tab = HomeTab::History;
        let text = render_to_text(94, 40, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        let lines: Vec<&str> = text.lines().collect();
        let top = lines
            .iter()
            .position(|l| l.contains("start a review"))
            .expect("tab bar rendered");
        let bottom = lines
            .iter()
            .rposition(|l| l.contains('\u{2570}'))
            .expect("bottom border rendered");
        assert_eq!(bottom - top, s.runs.len() + 1, "{text}");

        let mut many = state_with(
            vec![],
            (0..50)
                .map(|i| record(&format!("r{i}"), RunStatus::Finalized))
                .collect(),
        );
        many.tab = HomeTab::History;
        let text = render_to_text(94, 24, |f| {
            draw(
                f,
                f.area(),
                &many,
                &ClaudeCheck::Ok,
                "opus",
                &Theme::default(),
            )
        });
        let lines: Vec<&str> = text.lines().collect();
        let bottom = lines
            .iter()
            .rposition(|l| l.contains('\u{2570}'))
            .expect("bottom border rendered");
        assert!(
            bottom >= 20,
            "an overflowing history takes the full height: bottom={bottom}\n{text}"
        );
    }

    #[test]
    fn start_tab_shows_personas_detail_line_under_selected_row_only() {
        let s = state_with(vec![detected_head_target()], vec![]);
        let text = render_to_text(100, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        assert!(
            text.contains("personas: prover · breaker · steward · cross-review off"),
            "{text}"
        );
        let spec_line = text
            .lines()
            .find(|l| l.contains("spec files…"))
            .expect("spec row rendered");
        assert!(
            !spec_line.contains("personas:"),
            "unselected rows carry no detail line: {spec_line:?}"
        );
    }

    #[test]
    fn tab_bar_shows_triage_badge_and_active_tab() {
        let mut waiting = record("w", RunStatus::ReviewsComplete);
        waiting.findings_total = Some(3);
        let s = state_with(vec![], vec![waiting, record("f", RunStatus::Finalized)]);
        let text = render_to_text(100, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        assert!(
            text.contains("start a review · personas · history \u{25d0} 1"),
            "{text}"
        );
    }

    #[test]
    fn personas_tab_lists_builtin_roster_with_badges() {
        let mut s = state_with(vec![], vec![]);
        s.tab = HomeTab::Personas;
        let text = render_to_text(100, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        for name in crate::ui::theme::BUILTIN_SLOTS {
            assert!(text.contains(name), "{name} missing from roster: {text}");
        }
        assert!(text.contains("built-in"), "provenance tag rendered: {text}");
        assert!(
            text.contains("v view · e edit · n new · d duplicate · x delete"),
            "{text}"
        );
    }

    #[test]
    fn personas_tab_n_prompts_scope_instead_of_new_review() {
        let mut s = state_with(vec![], vec![]);
        s.tab = HomeTab::Personas;
        assert!(s.handle_key(key('n')).is_none(), "no Compose transition");
        assert!(matches!(
            s.personas.scope_prompt,
            Some(crate::ui::personas::ScopeOp::New)
        ));
        let text = render_to_text(100, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        assert!(text.contains("new persona in:"), "{text}");
    }

    #[test]
    fn personas_tab_enter_opens_pager_with_builtin_source() {
        let mut s = state_with(vec![], vec![]);
        s.tab = HomeTab::Personas;
        assert!(s.handle_key(key_code(KeyCode::Enter)).is_none());
        assert!(s.personas.pager.is_some(), "enter views the persona");
        let text = render_to_text(100, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        assert!(
            text.contains("built-in"),
            "pager title carries provenance: {text}"
        );
    }

    #[test]
    fn personas_tab_x_arms_and_footer_explains_two_step_delete() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".reviewal/personas");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("redteam.md"),
            "+++\nname = \"redteam\"\ntitle = \"Red Team\"\nlens = \"attack\"\ntarget = \"both\"\n+++\nbody",
        )
        .unwrap();
        let mut s = state_with(vec![], vec![]);
        s.personas = PersonaManager::new(root.path(), &crate::config::Config::default(), None);
        s.tab = HomeTab::Personas;
        let row = s
            .personas
            .personas
            .iter()
            .position(|c| c.persona.name == "redteam")
            .expect("custom persona loaded");
        s.personas.cursor = row;
        s.handle_key(key('x'));
        assert_eq!(s.personas.armed_delete, Some(row));
        let text = render_to_text(100, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        assert!(
            text.contains("again deletes redteam"),
            "armed footer shown: {text}"
        );
        s.handle_key(key('x'));
        assert!(
            !dir.join("redteam.md").exists(),
            "second x deletes the file"
        );
    }

    #[test]
    fn personas_tab_e_on_custom_persona_stages_editor_request() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(".reviewal/personas");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("redteam.md");
        std::fs::write(
            &path,
            "+++\nname = \"redteam\"\ntitle = \"Red Team\"\nlens = \"attack\"\ntarget = \"both\"\n+++\nbody",
        )
        .unwrap();
        let mut s = state_with(vec![], vec![]);
        s.personas = PersonaManager::new(root.path(), &crate::config::Config::default(), None);
        s.tab = HomeTab::Personas;
        let row = s
            .personas
            .personas
            .iter()
            .position(|c| c.persona.name == "redteam")
            .unwrap();
        s.personas.cursor = row;
        s.handle_key(key('e'));
        let req = s.personas.pending_editor.take().expect("editor staged");
        assert_eq!(req.path, path);
        assert!(!req.created, "existing file must never be cleaned up");
        s.personas.on_editor_return(req, true);
        assert!(
            s.personas
                .personas
                .iter()
                .any(|c| c.persona.name == "redteam"),
            "roster reloaded after the round-trip"
        );
    }

    #[test]
    fn selected_history_row_wears_a_bg_tint_not_reverse_video() {
        let mut s = history_fixture_state();
        s.tab = HomeTab::History;
        let buffer = crate::ui::app::render_to_buffer(94, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        let mut bar = None;
        'outer: for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                if buffer[(x, y)].symbol() == "\u{258c}" {
                    bar = Some((x, y));
                    break 'outer;
                }
            }
        }
        let (x, y) = bar.expect("selection bar rendered");
        let style = buffer[(x, y)].style();
        assert_eq!(style.bg, Some(ratatui::style::Color::DarkGray));
        assert!(
            !style
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED),
            "selection is a tint, not reverse video"
        );
    }

    #[test]
    fn tab_cycles_through_all_three_tabs_and_enter_opens_run_in_history() {
        let mut s = state_with(vec![], vec![record("id-a", RunStatus::Finalized)]);
        assert!(s.handle_key(key_code(KeyCode::Tab)).is_none());
        assert!(matches!(s.tab, HomeTab::Personas));
        assert!(s.handle_key(key_code(KeyCode::Tab)).is_none());
        assert!(matches!(s.tab, HomeTab::History));
        match s.handle_key(key_code(KeyCode::Enter)) {
            Some(Transition::OpenRun(r)) => assert_eq!(r.id, "id-a"),
            other => panic!("expected OpenRun, got {other:?}"),
        }
        s.handle_key(key_code(KeyCode::Tab));
        assert!(matches!(s.tab, HomeTab::Start), "cycle wraps");
    }

    #[test]
    fn backtab_cycles_backwards_and_digits_jump() {
        let mut s = state_with(vec![], vec![]);
        s.handle_key(key_code(KeyCode::BackTab));
        assert!(
            matches!(s.tab, HomeTab::History),
            "backtab wraps to the end"
        );
        s.handle_key(key('2'));
        assert!(matches!(s.tab, HomeTab::Personas));
        s.handle_key(key('1'));
        assert!(matches!(s.tab, HomeTab::Start));
        s.handle_key(key('3'));
        assert!(matches!(s.tab, HomeTab::History));
    }

    #[test]
    fn launcher_navigation_clamps_at_spec_row_and_zero() {
        let mut s = state_with(vec![detected_head_target()], vec![]);
        s.handle_key(key('j'));
        assert_eq!(s.launcher_idx, 1, "moved onto the spec row");
        s.handle_key(key('j'));
        assert_eq!(s.launcher_idx, 1, "clamped at the spec row");
        s.handle_key(key('k'));
        s.handle_key(key('k'));
        assert_eq!(s.launcher_idx, 0, "clamped at zero");
    }

    #[test]
    fn help_overlay_toggles_and_swallows_keys() {
        let mut s = state_with(vec![], vec![]);
        assert!(s.handle_key(key('?')).is_none());
        assert!(s.show_help);
        assert!(
            s.handle_key(key('q')).is_none(),
            "the closing key must not also fire its normal action"
        );
        assert!(!s.show_help);
    }

    #[test]
    fn help_overlay_renders_over_home() {
        let mut s = state_with(vec![], vec![]);
        s.show_help = true;
        let text = render_to_text(100, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        assert!(text.contains("help — any key closes"), "{text}");
        assert!(text.contains("new review"), "{text}");
    }

    #[test]
    fn enter_action_mirrors_apply_open_run_semantics() {
        assert!(matches!(
            enter_action(&RunStatus::Finalized),
            EnterAction::Open("open report")
        ));
        assert!(matches!(
            enter_action(&RunStatus::ReviewsComplete),
            EnterAction::Open("resume triage")
        ));
        assert!(matches!(
            enter_action(&RunStatus::Running),
            EnterAction::Blocked("run in progress elsewhere")
        ));
        for status in [RunStatus::Stale, RunStatus::Aborted] {
            assert!(
                matches!(
                    enter_action(&status),
                    EnterAction::Blocked("not resumable (stale/aborted)")
                ),
                "{status:?} must be blocked as not-resumable"
            );
        }
    }

    #[test]
    fn history_hovering_blocked_run_shows_reason_not_enter_hint() {
        let mut s = state_with(
            vec![],
            vec![
                record("2026-07-08T18-23-52Z-a", RunStatus::Aborted),
                record("2026-07-08T16-05-04Z-b", RunStatus::Finalized),
            ],
        );
        s.tab = HomeTab::History;
        let text = render_to_text(100, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        let hint_row = text
            .lines()
            .find(|l| l.contains("tab start a review"))
            .expect("hint row rendered");
        assert!(
            hint_row.contains("not resumable (stale/aborted)"),
            "hover reason shown: {hint_row:?}"
        );
        assert!(
            !hint_row.contains("enter"),
            "enter hint hidden for a blocked run: {hint_row:?}"
        );
    }

    #[test]
    fn history_hovering_running_run_shows_in_progress_reason() {
        let mut s = state_with(
            vec![],
            vec![record("2026-07-08T18-23-52Z-a", RunStatus::Running)],
        );
        s.tab = HomeTab::History;
        let text = render_to_text(100, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        assert!(text.contains("run in progress elsewhere"), "{text}");
    }

    #[test]
    fn history_hovering_finalized_run_offers_open_report() {
        let mut s = state_with(
            vec![],
            vec![record("2026-07-08T16-05-04Z-b", RunStatus::Finalized)],
        );
        s.tab = HomeTab::History;
        let text = render_to_text(100, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        assert!(text.contains("enter open report"), "{text}");
    }

    #[test]
    fn history_hovering_reviews_complete_run_offers_resume_triage() {
        let mut s = state_with(
            vec![],
            vec![record("2026-07-08T16-05-04Z-b", RunStatus::ReviewsComplete)],
        );
        s.tab = HomeTab::History;
        let text = render_to_text(100, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        assert!(text.contains("enter resume triage"), "{text}");
    }

    #[test]
    fn start_tab_hints_offer_personas_tab() {
        let s = state_with(vec![], vec![]);
        let text = render_to_text(100, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        assert!(
            text.contains("enter start · e edit setup · tab personas · ? help · q quit"),
            "{text}"
        );
    }

    #[test]
    fn draw_shows_warnings_above_hints() {
        let mut s = state_with(vec![], vec![]);
        s.warnings = vec!["theme: invalid color \"blurple\" for accent".into()];
        let text = render_to_text(100, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        assert!(text.contains("invalid color"), "{text}");
        let lines: Vec<&str> = text.lines().collect();
        let warn = lines
            .iter()
            .position(|l| l.contains("invalid color"))
            .unwrap();
        let hint = lines.iter().position(|l| l.contains("edit setup")).unwrap();
        assert_eq!(hint, warn + 1, "warnings sit directly above hints");
    }

    #[test]
    fn title_carries_accent() {
        let s = state_with(vec![], vec![]);
        let buffer = crate::ui::app::render_to_buffer(100, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        let mut title_pos = None;
        'outer: for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                if buffer[(x, y)].symbol() == "r" {
                    title_pos = Some((x, y));
                    break 'outer;
                }
            }
        }
        let (x, y) = title_pos.expect("title rendered");
        assert_eq!(buffer[(x, y)].style().fg, Some(ratatui::style::Color::Blue));
    }
}
