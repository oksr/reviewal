use crate::engine::store::{RunRecord, RunStatus};
use crate::engine::target::DetectedTarget;
use crate::ui::app::{ClaudeCheck, Transition};
use crate::ui::theme::Theme;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

pub(crate) enum HomeZone {
    Launcher,
    History,
}

pub(crate) struct HomeState {
    /// `launcher_idx` ranges over `0..=targets.len()`; `idx == targets.len()`
    /// is the trailing "spec files…" row.
    pub targets: Vec<DetectedTarget>,
    pub spec_count: usize,
    pub runs: Vec<RunRecord>,
    pub zone: HomeZone,
    pub launcher_idx: usize,
    pub history_idx: usize,
    pub warnings: Vec<String>,
    pub skill_installed: bool,
    pub defaults_code: Vec<String>,
    pub defaults_spec: Vec<String>,
    pub show_help: bool,
}

const HELP_ENTRIES: &[(&str, &str)] = &[
    ("enter", "start (launcher) / open (history)"),
    ("e", "edit setup before launch"),
    ("n", "new review"),
    ("tab", "switch zone"),
    ("j/k", "move"),
    ("?", "help"),
    ("q", "quit"),
];

impl HomeState {
    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Option<Transition> {
        // The overlay swallows every key that closes it — including one
        // that would otherwise start a run — so a mistimed keystroke never
        // fires an action the user couldn't see coming.
        if self.show_help {
            self.show_help = false;
            return None;
        }
        match self.zone {
            HomeZone::Launcher => self.handle_launcher_key(key),
            HomeZone::History => self.handle_history_key(key),
        }
    }

    fn handle_launcher_key(&mut self, key: KeyEvent) -> Option<Transition> {
        let spec_idx = self.targets.len();
        match key.code {
            KeyCode::Char('q') => Some(Transition::Quit),
            KeyCode::Char('?') => {
                self.show_help = true;
                None
            }
            KeyCode::Tab => {
                if !self.runs.is_empty() {
                    self.zone = HomeZone::History;
                }
                None
            }
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

    fn handle_history_key(&mut self, key: KeyEvent) -> Option<Transition> {
        match key.code {
            KeyCode::Char('q') => Some(Transition::Quit),
            KeyCode::Char('?') => {
                self.show_help = true;
                None
            }
            KeyCode::Tab => {
                self.zone = HomeZone::Launcher;
                None
            }
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
    let launcher_focused = matches!(state.zone, HomeZone::Launcher);
    let selected_is_diff_row = launcher_focused && state.launcher_idx < state.targets.len();
    let extra_files_line: u16 = u16::from(selected_is_diff_row);
    let launcher_rows = (state.targets.len() + 1).max(1) as u16;
    let launcher_height = launcher_rows + 2 + extra_files_line;
    let warn_height = state.warnings.len() as u16;
    let tip_height = u16::from(!state.skill_installed);

    let [header, _gap, launcher, defaults, history, tip, warn_area, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(launcher_height),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(tip_height),
        Constraint::Length(warn_height),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(f, header, claude_check, model_label, theme);
    draw_launcher(f, launcher, state, theme);
    draw_defaults(f, defaults, state, theme);
    draw_history(f, history, state, theme);
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
        crate::ui::overlay::draw_help(f, area, HELP_ENTRIES, theme);
    }
}

fn draw_header(
    f: &mut Frame,
    area: Rect,
    claude_check: &ClaudeCheck,
    model_label: &str,
    theme: &Theme,
) {
    f.render_widget(
        Paragraph::new(Line::styled("reviewal", theme.title_style())),
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

fn selectable_row(content: String, selected: bool, width: u16, theme: &Theme) -> Line<'static> {
    if !selected {
        return Line::raw(content);
    }
    let base = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::REVERSED);
    const SUFFIX: &str = "enter starts with defaults";
    let width = width as usize;
    let content_len = content.chars().count();
    if content_len + 1 + SUFFIX.chars().count() <= width {
        let pad = width - SUFFIX.chars().count();
        Line::from(vec![
            Span::styled(format!("{content:<pad$}"), base),
            Span::styled(SUFFIX, theme.dim_style()),
        ])
    } else {
        Line::styled(content, base)
    }
}

fn draw_launcher(f: &mut Frame, area: Rect, state: &HomeState, theme: &Theme) {
    let focused = matches!(state.zone, HomeZone::Launcher);
    let border_style = if focused {
        Style::default().fg(theme.accent)
    } else {
        theme.dim_style()
    };
    let title_style = if focused {
        theme.title_style()
    } else {
        theme.dim_style()
    };
    let block = Block::bordered()
        .title(Span::styled("start a review", title_style))
        .border_style(border_style);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    if state.targets.is_empty() && state.spec_count == 0 {
        lines.push(Line::styled(
            "nothing to review here — not a git repo and no *.md files found",
            theme.dim_style(),
        ));
    } else {
        for (i, t) in state.targets.iter().enumerate() {
            let selected = focused && state.launcher_idx == i;
            let content = format!(
                "\u{25b8} {} — {} files · +{} \u{2212}{}",
                t.label,
                t.files.len(),
                t.additions,
                t.deletions
            );
            lines.push(selectable_row(content, selected, inner.width, theme));
            if selected {
                let names: Vec<&str> = t.files.iter().take(3).map(String::as_str).collect();
                lines.push(Line::styled(
                    format!("    {}", names.join(" · ")),
                    theme.dim_style(),
                ));
            }
        }
        let spec_idx = state.targets.len();
        let selected = focused && state.launcher_idx == spec_idx;
        let content = format!("\u{25b8} spec files… — {} *.md in repo", state.spec_count);
        lines.push(selectable_row(content, selected, inner.width, theme));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_defaults(f: &mut Frame, area: Rect, state: &HomeState, theme: &Theme) {
    let spec_idx = state.targets.len();
    let is_spec_row = state.launcher_idx == spec_idx;
    let names = if is_spec_row {
        &state.defaults_spec
    } else {
        &state.defaults_code
    };
    let mut spans = vec![Span::styled("  defaults: ", theme.dim_style())];
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", theme.dim_style()));
        }
        spans.push(Span::styled(
            name.clone(),
            Style::default().fg(theme.persona_color(name, None)),
        ));
    }
    spans.push(Span::styled(" · cross-review off — ", theme.dim_style()));
    spans.push(Span::styled("e", theme.accent_style()));
    spans.push(Span::styled(" to edit before launch", theme.dim_style()));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_history(f: &mut Frame, area: Rect, state: &HomeState, theme: &Theme) {
    let focused = matches!(state.zone, HomeZone::History);
    let border_style = if focused {
        Style::default().fg(theme.accent)
    } else {
        theme.dim_style()
    };
    let title_style = if focused {
        theme.title_style()
    } else {
        theme.dim_style()
    };
    let block = Block::bordered()
        .title(Span::styled("history", title_style))
        .border_style(border_style);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    if state.runs.is_empty() {
        lines.push(Line::styled(
            "no runs yet — your first review will appear here",
            theme.dim_style(),
        ));
    } else {
        let now = time::OffsetDateTime::now_utc();
        for (i, r) in state.runs.iter().enumerate() {
            let selected = focused && state.history_idx == i;
            let rel = crate::ui::format::relative_time(&r.created_at, now);
            let target_desc: String = r.target_desc.chars().take(28).collect();
            let prefix = format!("{rel:<11}{target_desc:<29}");
            let avail = (inner.width as usize).saturating_sub(prefix.chars().count());
            let mut spans = vec![Span::raw(prefix)];
            spans.extend(status_cell(r, theme, avail));
            if selected {
                let reversed = Style::default().add_modifier(Modifier::REVERSED);
                spans = spans
                    .into_iter()
                    .map(|s| Span::styled(s.content, s.style.patch(reversed)))
                    .collect();
            }
            lines.push(Line::from(spans));
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn status_cell(r: &RunRecord, theme: &Theme, avail: usize) -> Vec<Span<'static>> {
    let (label, style, suffix): (String, Style, Option<String>) = match r.status {
        RunStatus::Finalized => {
            let label = r
                .verdict_label
                .clone()
                .unwrap_or_else(|| "finalized".into());
            let style = theme.verdict(&label);
            let mut parts = Vec::new();
            if let Some(n) = r.findings_total {
                parts.push(format!("{n} findings"));
            }
            if let Some(n) = r.accepted_count {
                parts.push(format!("{n} accepted"));
            }
            let suffix = (!parts.is_empty()).then(|| format!("  {}", parts.join(" · ")));
            (label, style, suffix)
        }
        RunStatus::ReviewsComplete => (
            "needs triage \u{25b8}".into(),
            Style::default().fg(theme.run_status(&r.status)),
            r.findings_total.map(|n| format!("  {n} findings waiting")),
        ),
        RunStatus::Running => (
            r.status.label().into(),
            Style::default().fg(theme.run_status(&r.status)),
            None,
        ),
        // Aborted/Stale read as muted "not resumable" rather than an alert
        // color — dim, not their (Red/Gray) `run_status` role colors.
        RunStatus::Aborted | RunStatus::Stale => (r.status.label().into(), theme.dim_style(), None),
    };
    fit_status_cell(label, style, suffix, avail, theme)
}

/// Sacrifices the metadata suffix before ever cutting the label; an
/// over-wide label is ellipsized at a char boundary, never bare-clipped.
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
    if label_len <= avail {
        return vec![Span::styled(label, style)];
    }
    let cut: String = label.chars().take(avail - 1).collect();
    vec![Span::styled(format!("{cut}…"), style)]
}

fn draw_hints(f: &mut Frame, area: Rect, state: &HomeState, theme: &Theme) {
    let spans: Vec<Span> = match state.zone {
        HomeZone::Launcher => {
            let mut pairs: Vec<(&str, &str)> = vec![("enter", "start"), ("e", "edit setup")];
            if !state.runs.is_empty() {
                pairs.push(("tab", "history"));
            }
            pairs.push(("?", "help"));
            pairs.push(("q", "quit"));
            theme.hint_spans(&pairs)
        }
        HomeZone::History => {
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
            spans.extend(theme.hint_spans(&[("tab", "launcher"), ("q", "quit")]));
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
            zone: HomeZone::Launcher,
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
        waiting.findings_total = Some(9);
        state_with(vec![], vec![finalized, waiting])
    }

    #[test]
    fn history_rows_show_relative_time_verdict_and_triage_debt() {
        let s = history_fixture_state();
        // 94 cols models production: draw is called directly here, so we
        // simulate what a 100-col terminal leaves after draw_app's margins.
        let text = render_to_text(94, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        // At 94 cols the verdict label fits intact but its metadata suffix
        // does not.
        assert!(
            text.contains("SHIP-WITH-CAVEATS (2/3 ship, 1/3 block)"),
            "{text}"
        );
        assert!(
            !text.contains("12 findings"),
            "metadata that cannot fit whole is dropped, not clipped: {text}"
        );
        assert!(text.contains("needs triage \u{25b8}"), "{text}");
        assert!(text.contains("9 findings waiting"), "{text}");
        assert!(
            !text.contains("2026-07-10T08-00-00Z"),
            "raw ids replaced by relative times: {text}"
        );
    }

    #[test]
    fn history_status_cell_ellipsizes_verdict_at_narrow_width() {
        let s = history_fixture_state();
        // 76 cols ≈ an 80-col terminal after draw_app's margins; the
        // history box borders leave 74 inner columns, so after the fixed
        // 40-col time/target prefix only 34 remain for the status cell.
        let text = render_to_text(76, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        let row = text
            .lines()
            .find(|l| l.contains("SHIP-WITH-CAVEATS"))
            .expect("verdict row rendered")
            .to_string();
        let content = row.trim_end().trim_end_matches('│').trim_end();
        assert!(
            content.ends_with('…'),
            "an over-wide verdict ends with an ellipsis, not a bare cut: {row:?}"
        );
        assert!(
            !row.contains("finding"),
            "no clipped fragment of the findings metadata: {row:?}"
        );
    }

    #[test]
    fn history_status_cell_shows_full_metadata_at_wide_width() {
        let s = history_fixture_state();
        let text = render_to_text(120, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        assert!(
            text.contains("SHIP-WITH-CAVEATS (2/3 ship, 1/3 block)"),
            "{text}"
        );
        assert!(text.contains("12 findings · 8 accepted"), "{text}");
        assert!(text.contains("9 findings waiting"), "{text}");
    }

    #[test]
    fn tab_toggles_zone_and_enter_opens_run_in_history() {
        let mut s = state_with(vec![], vec![record("id-a", RunStatus::Finalized)]);
        assert!(s.handle_key(key_code(KeyCode::Tab)).is_none());
        assert!(matches!(s.zone, HomeZone::History));
        match s.handle_key(key_code(KeyCode::Enter)) {
            Some(Transition::OpenRun(r)) => assert_eq!(r.id, "id-a"),
            other => panic!("expected OpenRun, got {other:?}"),
        }
    }

    #[test]
    fn tab_is_noop_with_no_run_history() {
        let mut s = state_with(vec![detected_head_target()], vec![]);
        s.handle_key(key_code(KeyCode::Tab));
        assert!(
            matches!(s.zone, HomeZone::Launcher),
            "nothing to tab to with an empty history"
        );
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
        s.zone = HomeZone::History;
        let text = render_to_text(100, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        let hint_row = text
            .lines()
            .find(|l| l.contains("tab launcher"))
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
        s.zone = HomeZone::History;
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
        s.zone = HomeZone::History;
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
        s.zone = HomeZone::History;
        let text = render_to_text(100, 30, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        assert!(text.contains("enter resume triage"), "{text}");
    }

    #[test]
    fn launcher_hints_include_history_nav_only_when_runs_exist() {
        let with_runs = state_with(vec![], vec![record("a", RunStatus::Finalized)]);
        let text = render_to_text(100, 30, |f| {
            draw(
                f,
                f.area(),
                &with_runs,
                &ClaudeCheck::Ok,
                "opus",
                &Theme::default(),
            )
        });
        assert!(
            text.contains("enter start · e edit setup · tab history · ? help · q quit"),
            "{text}"
        );

        let without_runs = state_with(vec![], vec![]);
        let text = render_to_text(100, 30, |f| {
            draw(
                f,
                f.area(),
                &without_runs,
                &ClaudeCheck::Ok,
                "opus",
                &Theme::default(),
            )
        });
        assert!(
            text.contains("enter start · e edit setup · ? help · q quit"),
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
