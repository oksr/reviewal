use crate::engine::store::{RunRecord, RunStatus};
use crate::engine::target::{DetectedTarget, Target};
use crate::ui::app::{ClaudeCheck, Transition};
use crate::ui::theme::Theme;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Padding, Paragraph};
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

    // The history box hugs its rows; leftover space collects in the filler
    // below it, so the hints stay bottom-anchored without an empty frame
    // stretching down the screen.
    let fixed = 1 + 1 + launcher_height + 1 + tip_height + warn_height + 1;
    let history_avail = area.height.saturating_sub(fixed).max(3);
    let history_height = (state.runs.len().max(1) as u16 + 2).min(history_avail);

    let [header, _gap, launcher, defaults, history, _filler, tip, warn_area, hints] =
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(launcher_height),
            Constraint::Length(1),
            Constraint::Length(history_height),
            Constraint::Min(0),
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

fn draw_launcher(f: &mut Frame, area: Rect, state: &HomeState, theme: &Theme) {
    let focused = matches!(state.zone, HomeZone::Launcher);
    let block = theme
        .panel("start a review", focused)
        .padding(Padding::horizontal(1));
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
            }
        }
        let spec_idx = state.targets.len();
        let selected = focused && state.launcher_idx == spec_idx;
        let content = vec![
            Span::raw("spec files\u{2026}"),
            Span::styled(
                format!(" — {} *.md in repo", state.spec_count),
                theme.dim_style(),
            ),
        ];
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
    // Indent under the launcher's border + padding + pointer gutter so the
    // line reads as a footnote to the row above it.
    let mut spans = vec![Span::styled("    Personas: ", theme.dim_style())];
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

fn draw_history(f: &mut Frame, area: Rect, state: &HomeState, theme: &Theme) {
    let focused = matches!(state.zone, HomeZone::History);
    let block = theme
        .panel("history", focused)
        .padding(Padding::horizontal(1));
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
        waiting.target = Target::SpecFiles(vec!["docs/superpowers/specs".into()]);
        waiting.target_desc = "spec: docs/superpowers/specs".into();
        waiting.findings_total = Some(9);
        state_with(vec![], vec![finalized, waiting])
    }

    #[test]
    fn history_rows_show_badges_kind_tags_and_metadata() {
        let s = history_fixture_state();
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
        let s = history_fixture_state();
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
        let s = history_fixture_state();
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
    fn history_box_hugs_its_rows() {
        let s = history_fixture_state();
        let text = render_to_text(94, 40, |f| {
            draw(f, f.area(), &s, &ClaudeCheck::Ok, "opus", &Theme::default())
        });
        let lines: Vec<&str> = text.lines().collect();
        let top = lines
            .iter()
            .position(|l| l.contains(" history "))
            .expect("history title rendered");
        let bottom = lines
            .iter()
            .rposition(|l| l.contains('\u{2570}'))
            .expect("history bottom border rendered");
        assert_eq!(
            bottom - top,
            s.runs.len() + 1,
            "two runs → a 4-row box, not a frame to the footer: {text}"
        );
    }

    #[test]
    fn selected_history_row_wears_a_bg_tint_not_reverse_video() {
        let mut s = history_fixture_state();
        s.zone = HomeZone::History;
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
