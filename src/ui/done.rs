use crate::engine::model::Severity;
use crate::engine::store::TriageStatus;
use crate::engine::synthesis::Report;
use crate::ui::app::Transition;
use crate::ui::theme::Theme;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

pub(crate) struct DoneState {
    pub run_id: String,
    pub target_desc: String,
    pub consensus_label: String,
    pub degraded: Vec<String>,
    /// verdicts.len() + degraded.len() — the requested reviewer count.
    pub requested_reviewers: usize,
    pub accepted: usize,
    pub dismissed: usize,
    pub deferred: usize,
    /// (critical, warning, info) among ACCEPTED findings.
    pub accepted_severities: (usize, usize, usize),
    /// Project-relative: ".reviewal/runs/{id}/report.md".
    pub report_path: String,
    pub skill_installed: bool,
}

impl DoneState {
    /// `report_path` is computed here rather than passed in: it is always
    /// project-relative — the shell's cwd IS the project root — so there is
    /// nothing for a caller to decide.
    pub(crate) fn from_report(
        run_id: &str,
        target_desc: &str,
        report: &Report,
        skill_installed: bool,
    ) -> Self {
        let mut accepted = 0;
        let mut dismissed = 0;
        let mut deferred = 0;
        let (mut critical, mut warning, mut info) = (0, 0, 0);
        for f in &report.findings {
            // Exhaustive on purpose: a new TriageStatus variant must decide
            // its Done-screen bucket here at compile time.
            match f.triage.status {
                TriageStatus::Accepted => {
                    accepted += 1;
                    match f.finding.severity {
                        Severity::Critical => critical += 1,
                        Severity::Warning => warning += 1,
                        Severity::Info => info += 1,
                    }
                }
                TriageStatus::Dismissed => dismissed += 1,
                TriageStatus::Deferred => deferred += 1,
            }
        }
        DoneState {
            run_id: run_id.to_string(),
            target_desc: target_desc.to_string(),
            consensus_label: report.consensus_label.clone(),
            degraded: report.degraded.clone(),
            requested_reviewers: report.verdicts.len() + report.degraded.len(),
            accepted,
            dismissed,
            deferred,
            accepted_severities: (critical, warning, info),
            report_path: format!(".reviewal/runs/{run_id}/report.md"),
            skill_installed,
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Option<Transition> {
        match key.code {
            KeyCode::Char('r') => Some(Transition::ReopenTriage {
                run_id: self.run_id.clone(),
            }),
            KeyCode::Char('n') => Some(Transition::Compose {
                target: None,
                open_spec_picker: false,
            }),
            KeyCode::Enter | KeyCode::Esc => Some(Transition::ToHome),
            KeyCode::Char('q') => Some(Transition::Quit),
            _ => None,
        }
    }
}

pub(crate) fn draw(f: &mut Frame, area: Rect, state: &DoneState, theme: &Theme) {
    let count_lines = build_count_lines(state, theme);
    let counts_height = count_lines.len() as u16;

    // Filler on top anchors the summary to the bottom of the screen.
    let [_filler, identity, banner, counts, report, next_box, hint] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(counts_height),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(area);

    f.render_widget(
        Paragraph::new(Line::styled(
            format!("{} — {}", state.run_id, state.target_desc),
            theme.dim_style(),
        )),
        identity,
    );

    f.render_widget(Paragraph::new(banner_line(state, theme)), banner);
    f.render_widget(Paragraph::new(count_lines), counts);
    f.render_widget(
        Paragraph::new(Line::styled(
            format!("report: {}", state.report_path),
            theme.dim_style(),
        )),
        report,
    );
    draw_next_box(f, next_box, state, theme);
    f.render_widget(
        Paragraph::new(theme.hints(&[
            ("r", "reopen triage"),
            ("n", "new review"),
            ("enter/esc", "home"),
            ("q", "quit"),
        ])),
        hint,
    );
}

fn banner_line(state: &DoneState, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("━━ {} ━━", state.consensus_label),
        theme.verdict(&state.consensus_label),
    )];
    if !state.degraded.is_empty() {
        let names = state.degraded.join(", ");
        let k = state.requested_reviewers - state.degraded.len();
        let n = state.requested_reviewers;
        spans.push(Span::styled(
            format!("  ⚠ {names} failed and was excluded ({k} of {n} reviewers)"),
            Style::default().fg(theme.severity_warning),
        ));
    }
    Line::from(spans)
}

fn build_count_lines(state: &DoneState, theme: &Theme) -> Vec<Line<'static>> {
    if state.accepted == 0 && state.dismissed == 0 && state.deferred == 0 {
        return vec![Line::styled(
            "no findings — all reviewers reported clean".to_string(),
            Style::default().fg(theme.status_done),
        )];
    }
    let mut lines = Vec::new();
    if state.accepted > 0 {
        let (critical, warning, info) = state.accepted_severities;
        lines.push(Line::from(vec![
            Span::styled(
                format!("✓ {} accepted", state.accepted),
                Style::default().fg(theme.status_done),
            ),
            Span::styled(
                format!(" ({critical} critical · {warning} warning · {info} info)"),
                theme.dim_style(),
            ),
            Span::styled(
                " → action items in the report".to_string(),
                theme.dim_style(),
            ),
        ]));
    }
    if state.dismissed > 0 {
        lines.push(Line::from(vec![
            Span::styled(
                format!("✗ {} dismissed", state.dismissed),
                Style::default().fg(theme.status_failed),
            ),
            Span::styled(
                " → kept with your notes so they won't be re-litigated".to_string(),
                theme.dim_style(),
            ),
        ]));
    }
    if state.deferred > 0 {
        lines.push(Line::from(vec![
            Span::styled(
                format!("· {} deferred", state.deferred),
                Style::default().fg(theme.severity_warning),
            ),
            Span::styled(
                " → carried forward, resurface on the next run".to_string(),
                theme.dim_style(),
            ),
        ]));
    }
    lines
}

fn draw_next_box(f: &mut Frame, area: Rect, state: &DoneState, theme: &Theme) {
    let style = if state.skill_installed {
        theme.accent_style()
    } else {
        Style::default().fg(theme.severity_warning)
    };
    let block = crate::ui::theme::bordered()
        .title(crate::ui::theme::inset_title(
            Line::from(Span::styled("next", style)),
            style,
        ))
        .border_style(style);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let line = if state.skill_installed {
        Line::from(vec![
            Span::raw("  in your authoring Claude session run  "),
            Span::styled("/reviewal-ingest", theme.title_style()),
            Span::raw("  to pull accepted findings"),
        ])
    } else {
        Line::from(vec![
            Span::raw("  ingest skill not installed — run "),
            Span::styled(
                "reviewal init",
                Style::default()
                    .fg(theme.severity_warning)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(", then /reviewal-ingest in Claude"),
        ])
    };
    f.render_widget(Paragraph::new(line), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::model::Verdict;
    use crate::engine::store::TriageEntry;
    use crate::engine::synthesis::{Confidence, Finding, ReportFinding};
    use crate::ui::app::{render_to_buffer, render_to_text};
    use crate::ui::test_keys::{key, key_code};
    use std::collections::BTreeMap;

    fn finding(severity: Severity, status: TriageStatus) -> ReportFinding {
        ReportFinding {
            finding: Finding {
                id: "id".into(),
                severity,
                title: "t".into(),
                detail: "d".into(),
                file: None,
                line: None,
                fix: None,
                reporters: vec![],
                validators: vec![],
                challengers: vec![],
                confidence: Confidence::Solo,
            },
            triage: TriageEntry {
                status,
                note: None,
                touched: false,
            },
        }
    }

    fn report_with(
        accepted: &[Severity],
        dismissed: usize,
        deferred: usize,
        degraded: &[&str],
        verdict_count: usize,
    ) -> Report {
        let mut findings: Vec<ReportFinding> = accepted
            .iter()
            .map(|s| finding(*s, TriageStatus::Accepted))
            .collect();
        findings.extend((0..dismissed).map(|_| finding(Severity::Info, TriageStatus::Dismissed)));
        findings.extend((0..deferred).map(|_| finding(Severity::Info, TriageStatus::Deferred)));
        let verdicts = (0..verdict_count)
            .map(|i| (format!("persona{i}"), Verdict::Approve))
            .collect::<BTreeMap<_, _>>();
        Report {
            consensus_label: "SHIP (unanimous, 2/2)".into(),
            consensus_score: 1.0,
            verdicts,
            summaries: BTreeMap::new(),
            degraded: degraded.iter().map(|s| s.to_string()).collect(),
            findings,
        }
    }

    fn plain_done_state() -> DoneState {
        DoneState {
            run_id: "2026-07-10T09-00-00Z-diff-head".into(),
            target_desc: "diff vs HEAD (uncommitted)".into(),
            consensus_label: "SHIP (unanimous, 2/2)".into(),
            degraded: vec![],
            requested_reviewers: 2,
            accepted: 1,
            dismissed: 1,
            deferred: 1,
            accepted_severities: (0, 1, 0),
            report_path: ".reviewal/runs/r/report.md".into(),
            skill_installed: true,
        }
    }

    fn done_state_with_counts(accepted: usize, dismissed: usize, deferred: usize) -> DoneState {
        DoneState {
            accepted,
            dismissed,
            deferred,
            ..plain_done_state()
        }
    }

    #[test]
    fn done_shows_identity_severity_breakdown_and_hints() {
        let report = report_with(
            &[
                Severity::Critical,
                Severity::Warning,
                Severity::Warning,
                Severity::Warning,
                Severity::Info,
            ],
            2,
            3,
            &["breaker"],
            2,
        );
        let s = DoneState::from_report(
            "2026-07-10T09-12-44Z-diff-head",
            "diff vs HEAD (uncommitted)",
            &report,
            true,
        );
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &s, &Theme::default()));
        assert!(
            text.contains("2026-07-10T09-12-44Z-diff-head — diff vs HEAD (uncommitted)"),
            "{text}"
        );
        assert!(
            text.contains("⚠ breaker failed and was excluded (2 of 3 reviewers)"),
            "{text}"
        );
        assert!(
            text.contains("✓ 5 accepted (1 critical · 3 warning · 1 info)"),
            "{text}"
        );
        assert!(text.contains("→ action items in the report"), "{text}");
        assert!(
            text.contains("report: .reviewal/runs/2026-07-10T09-12-44Z-diff-head/report.md"),
            "{text}"
        );
        assert!(
            text.contains("run  /reviewal-ingest  to pull accepted findings"),
            "{text}"
        );
        assert!(
            text.contains("r reopen triage · n new review · enter/esc home · q quit"),
            "{text}"
        );
    }

    #[test]
    fn missing_skill_swaps_the_next_box() {
        let s = DoneState {
            skill_installed: false,
            ..plain_done_state()
        };
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &s, &Theme::default()));
        assert!(
            text.contains("ingest skill not installed — run reviewal init"),
            "{text}"
        );
    }

    #[test]
    fn clean_run_celebrates() {
        let s = done_state_with_counts(0, 0, 0);
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &s, &Theme::default()));
        assert!(
            text.contains("no findings — all reviewers reported clean"),
            "{text}"
        );
        assert!(!text.contains("deferred"), "{text}");
    }

    #[test]
    fn keys_reopen_compose_home_quit() {
        let mut s = plain_done_state();
        assert!(matches!(
            s.handle_key(key('r')),
            Some(Transition::ReopenTriage { .. })
        ));
        assert!(matches!(
            s.handle_key(key('n')),
            Some(Transition::Compose { .. })
        ));
        assert!(matches!(
            s.handle_key(key_code(KeyCode::Enter)),
            Some(Transition::ToHome)
        ));
        assert!(matches!(
            s.handle_key(key_code(KeyCode::Esc)),
            Some(Transition::ToHome)
        ));
        assert!(matches!(s.handle_key(key('q')), Some(Transition::Quit)));
    }

    #[test]
    fn from_report_counts_statuses_and_severities_exhaustively() {
        let report = report_with(&[Severity::Critical], 1, 1, &[], 2);
        let s = DoneState::from_report("r", "diff vs HEAD", &report, true);
        assert_eq!((s.accepted, s.dismissed, s.deferred), (1, 1, 1));
        assert_eq!(s.accepted_severities, (1, 0, 0));
        assert_eq!(s.consensus_label, "SHIP (unanimous, 2/2)");
        assert_eq!(s.requested_reviewers, 2);
        assert_eq!(s.report_path, ".reviewal/runs/r/report.md");
    }

    #[test]
    fn banner_has_flanking_rule_in_verdict_color() {
        let s = plain_done_state();
        let buffer = render_to_buffer(100, 20, |f| draw(f, f.area(), &s, &Theme::default()));
        let mut found = false;
        for y in 0..20u16 {
            for x in 0..100u16 {
                if buffer[(x, y)].symbol() == "━" {
                    let style = buffer[(x, y)].style();
                    assert_eq!(style.fg, Some(ratatui::style::Color::Green));
                    assert!(style.add_modifier.contains(ratatui::style::Modifier::BOLD));
                    found = true;
                }
            }
        }
        assert!(found, "flanking rule rendered somewhere");
    }
}
