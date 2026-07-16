use crate::engine::agent::AgentActivity;
use crate::engine::run::{Phase, RunEvent};
use crate::engine::store::Triage;
use crate::engine::synthesis::Synthesis;
use crate::ui::app::Transition;
use crate::ui::theme::Theme;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;
use std::collections::BTreeMap;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentStatus {
    Pending,
    Running,
    Retrying,
    Done,
    Failed,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentPanel {
    pub status: AgentStatus,
    pub last_text: String,
    pub tokens: u64,
    /// False while `tokens` is a chars/4 estimate from the stream (rendered
    /// with a `~`); true once it comes from a real usage field.
    pub tokens_exact: bool,
    pub duration_secs: Option<u64>,
    pub findings_count: usize,
    pub error: Option<String>,
    pub started_at: Option<Instant>,
    /// Snapshotted `"r1: {n} findings · {d}s"` taken right before a round-2
    /// `AgentStarted` resets this panel's live status — so round-1 results
    /// stay visible on the strip while round 2 runs.
    pub round1_line: Option<String>,
    /// Whether the round's output made it to disk before `AgentDone` fired
    /// (mirrors the event's `saved` flag).
    pub saved: bool,
}

impl AgentPanel {
    fn new() -> Self {
        AgentPanel {
            status: AgentStatus::Pending,
            last_text: String::new(),
            tokens: 0,
            tokens_exact: true,
            duration_secs: None,
            findings_count: 0,
            error: None,
            started_at: None,
            round1_line: None,
            saved: false,
        }
    }

    fn elapsed_clock(&self) -> String {
        let secs = self.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        format!("{:02}:{:02}", secs / 60, secs % 60)
    }

    fn spinner(&self) -> char {
        const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let ms = self
            .started_at
            .map(|t| t.elapsed().as_millis())
            .unwrap_or(0);
        FRAMES[(ms / 120) as usize % FRAMES.len()]
    }

    /// Appends stream text, keeping only the last 200 chars (truncated from
    /// the front).
    fn push_text(&mut self, text: &str) {
        self.last_text.push_str(text);
        // Byte index of the 200th-from-last char; everything before it goes.
        if let Some((idx, _)) = self.last_text.char_indices().rev().nth(199) {
            if idx > 0 {
                self.last_text.drain(..idx);
            }
        }
    }
}

pub(crate) struct TickerItem {
    pub persona: String,
    pub severity: crate::engine::model::Severity,
    pub title: String,
}

pub(crate) struct PendingTriage {
    pub run_id: String,
    pub synthesis: Synthesis,
}

pub(crate) struct DashboardState {
    pub personas: Vec<String>,
    pub agents: BTreeMap<String, AgentPanel>,
    pub phase: Phase,
    pub failure: Option<String>,
    pub failure_run_id: Option<String>,
    /// name → raw frontmatter color for the run's personas; names missing
    /// here (old tests, resumed runs) fall back to slot/hash resolution.
    pub persona_colors: BTreeMap<String, Option<String>>,
    pub target_desc: String,
    pub model_label: String,
    pub cross_review: bool,
    pub started: Instant,
    pub ticker: Vec<TickerItem>,
    /// True after a first `c` while the run is still live: the progress line
    /// is replaced by the armed confirmation line, and every key but a
    /// second `c` disarms without side effects.
    pub cancel_armed: bool,
    /// Set once `RunCancelled` lands: `(kept_reviews, resumable, run_id)`.
    pub cancelled: Option<(usize, bool, String)>,
    /// Set on a degraded `RunCompleted` (some requested personas failed):
    /// the dashboard pauses on a summary box instead of auto-advancing,
    /// guaranteeing the failure gets painted for at least one frame instead
    /// of being drained between polls and never shown.
    pub summary: Option<PendingTriage>,
    /// Set once a terminal event lands: freezes the header clock at that
    /// instant instead of letting `elapsed` keep ticking after the run ends.
    pub finished_at: Option<Instant>,
}

impl DashboardState {
    pub(crate) fn new(
        personas: Vec<String>,
        target_desc: String,
        model_label: String,
        cross_review: bool,
    ) -> Self {
        let agents = personas
            .iter()
            .map(|p| (p.clone(), AgentPanel::new()))
            .collect();
        DashboardState {
            personas,
            agents,
            phase: Phase::Collecting,
            failure: None,
            failure_run_id: None,
            persona_colors: BTreeMap::new(),
            target_desc,
            model_label,
            cross_review,
            started: Instant::now(),
            ticker: Vec::new(),
            cancel_armed: false,
            cancelled: None,
            summary: None,
            finished_at: None,
        }
    }

    /// Returns a transition only when the run finished successfully —
    /// failure is surfaced via the `failure` field, never a transition.
    pub(crate) fn handle_run_event(&mut self, event: RunEvent) -> Option<Transition> {
        match event {
            RunEvent::PhaseChanged(phase) => {
                // Snapshot each Done panel's round-1 summary before round 2's
                // AgentStarted overwrites its live status.
                if phase == Phase::Round2 {
                    for panel in self.agents.values_mut() {
                        if panel.status == AgentStatus::Done {
                            panel.round1_line = Some(format!(
                                "r1: {} findings · {}s",
                                panel.findings_count,
                                panel.duration_secs.unwrap_or(0)
                            ));
                        }
                    }
                }
                self.phase = phase;
                None
            }
            RunEvent::AgentStarted { persona } => {
                if let Some(panel) = self.agents.get_mut(&persona) {
                    panel.status = AgentStatus::Running;
                    panel.started_at = Some(Instant::now());
                    // Token counts are cumulative per subprocess invocation
                    // (see AgentActivity::Tokens): round 2 starts a new one.
                    panel.tokens = 0;
                    panel.tokens_exact = true;
                }
                None
            }
            RunEvent::AgentActivity { persona, activity } => {
                if let Some(panel) = self.agents.get_mut(&persona) {
                    match activity {
                        AgentActivity::TextDelta(text) => {
                            panel.status = AgentStatus::Running;
                            panel.push_text(&text);
                        }
                        AgentActivity::ToolUse(name) => {
                            panel.status = AgentStatus::Running;
                            panel.push_text(&format!("[tool: {name}]"));
                        }
                        AgentActivity::Tokens { count, exact } => {
                            panel.tokens = count;
                            panel.tokens_exact = exact;
                        }
                    }
                }
                None
            }
            RunEvent::AgentRetrying { persona, error } => {
                if let Some(panel) = self.agents.get_mut(&persona) {
                    panel.status = AgentStatus::Retrying;
                    panel.error = Some(error);
                    // A retry re-invokes the subprocess, so its token count
                    // starts over as well.
                    panel.tokens = 0;
                    panel.tokens_exact = true;
                }
                None
            }
            RunEvent::AgentDone {
                persona,
                duration_secs,
                findings,
                saved,
            } => {
                for f in &findings {
                    self.ticker.push(TickerItem {
                        persona: persona.clone(),
                        severity: f.severity,
                        title: f.title.clone(),
                    });
                }
                if let Some(panel) = self.agents.get_mut(&persona) {
                    panel.status = AgentStatus::Done;
                    panel.duration_secs = Some(duration_secs);
                    panel.findings_count = findings.len();
                    panel.error = None; // clear any stale retry error now that it succeeded
                    panel.saved = saved;
                }
                None
            }
            RunEvent::AgentFailed { persona, error } => {
                if let Some(panel) = self.agents.get_mut(&persona) {
                    panel.status = AgentStatus::Failed;
                    panel.error = Some(error);
                }
                None
            }
            RunEvent::RunCompleted { run_id, synthesis } => {
                self.finished_at = Some(Instant::now());
                if synthesis.degraded.is_empty() {
                    Some(Transition::ToTriage {
                        run_id,
                        target_desc: self.target_desc.clone(),
                        synthesis,
                        triage: Triage::new(),
                    })
                } else {
                    self.summary = Some(PendingTriage { run_id, synthesis });
                    // Same defensive stand-down as RunFailed/RunCancelled: an
                    // armed confirmation's run no longer exists to cancel.
                    self.cancel_armed = false;
                    None
                }
            }
            RunEvent::RunFailed { run_id, message } => {
                self.finished_at = Some(Instant::now());
                self.failure = Some(message);
                self.failure_run_id = run_id;
                // A failure landing while a cancel confirmation was armed
                // must stand the confirmation down — the run it would have
                // cancelled no longer exists.
                self.cancel_armed = false;
                None
            }
            RunEvent::RunCancelled {
                run_id,
                kept_reviews,
                resumable,
            } => {
                self.finished_at = Some(Instant::now());
                self.cancelled = Some((kept_reviews, resumable, run_id));
                self.cancel_armed = false;
                None
            }
            // Surfaced on the App-level status line; nothing panel-scoped to do.
            RunEvent::Warning { .. } => None,
        }
    }

    /// True while the engine is still working this run. `app.rs` uses this
    /// to decide whether Ctrl+C routes through the same two-step cancel
    /// confirmation as plain `c` instead of quitting the app outright.
    pub(crate) fn run_live(&self) -> bool {
        self.failure.is_none() && self.cancelled.is_none() && self.summary.is_none()
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Option<Transition> {
        if self.run_live() {
            if key.code == KeyCode::Char('c') {
                if self.cancel_armed {
                    // Second `c`: confirm. Stay armed — the engine answers
                    // with `RunCancelled`, which is what actually clears it.
                    return Some(Transition::CancelRun);
                }
                self.cancel_armed = true;
                return None;
            }
            if self.cancel_armed {
                // Any other key while armed: stand down, swallow the key.
                self.cancel_armed = false;
                return None;
            }
        }
        match key.code {
            KeyCode::Enter => {
                // Only a resumable cancel offers `enter triage`.
                if let Some((_, true, run_id)) = &self.cancelled {
                    return Some(Transition::ReopenTriage {
                        run_id: run_id.clone(),
                    });
                }
                // Acknowledging the degraded summary builds exactly the
                // `ToTriage` a clean completion would have auto-built.
                self.summary.take().map(|pending| Transition::ToTriage {
                    run_id: pending.run_id,
                    target_desc: self.target_desc.clone(),
                    synthesis: pending.synthesis,
                    triage: Triage::new(),
                })
            }
            KeyCode::Esc
                if self.cancelled.is_some() || self.failure.is_some() || self.summary.is_some() =>
            {
                Some(Transition::ToHome)
            }
            KeyCode::Char('q')
                if self.cancelled.is_some() || self.failure.is_some() || self.summary.is_some() =>
            {
                Some(Transition::Quit)
            }
            _ => None,
        }
    }
}

/// Middle-ellipsizes `s` to at most `width` chars: head + `…` + tail, with
/// the tail getting the larger share so a path's discriminating suffix
/// survives tight widths. Char-based, not display columns.
fn middle_ellipsize(s: &str, width: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let keep = width - 1; // one char reserved for the ellipsis
    let head = keep / 4;
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

/// Renders one persona strip's content lines under a hard row budget. The
/// strip's interior is only `interior_height` rows tall and ratatui's
/// `Paragraph` silently drops whatever wraps past it — which, when the
/// round-1 snapshot sits ABOVE a wrapped status line, would be the status
/// line's tail. So lines are admitted in PRIORITY order against the budget,
/// counting wrapped rows with
/// [`format::word_fit_rows`](crate::ui::format::word_fit_rows); a
/// lower-priority line that would push the total past the budget is skipped.
fn panel_lines(
    panel: &AgentPanel,
    pcolor: Color,
    theme: &Theme,
    interior_width: u16,
    interior_height: u16,
    raw_path: Option<&str>,
) -> Vec<Line<'static>> {
    let width = (interior_width as usize).max(1);
    let budget = interior_height as usize;
    let rows = |s: &str| crate::ui::format::word_fit_rows(s, width);

    // Priority 1: the live status line — always included, whatever it costs.
    let (status_text, status_style) = match panel.status {
        AgentStatus::Done => {
            let mut text = format!(
                "✓ done · {} findings · {}s",
                panel.findings_count,
                panel.duration_secs.unwrap_or(0)
            );
            if panel.saved {
                text.push_str(" — saved");
            }
            (text, Style::default().fg(theme.status_done))
        }
        AgentStatus::Running => (
            format!(
                "{} working · {}{} tok · {}",
                panel.spinner(),
                if panel.tokens_exact { "" } else { "~" },
                panel.tokens,
                panel.elapsed_clock()
            ),
            Style::default().fg(pcolor),
        ),
        AgentStatus::Retrying => (
            format!("↻ retrying · {}", panel.elapsed_clock()),
            Style::default().fg(theme.status_retrying),
        ),
        AgentStatus::Pending => (
            "○ pending".to_string(),
            Style::default().fg(theme.status_pending),
        ),
        AgentStatus::Failed => (
            format!(
                "✗ failed · {}",
                panel.error.as_deref().unwrap_or("unknown error")
            ),
            Style::default().fg(theme.status_failed),
        ),
    };
    let mut used = rows(&status_text);

    // Priority 2: the retry error detail.
    let error_line = match (&panel.status, &panel.error) {
        (AgentStatus::Retrying, Some(err)) if used + rows(err) <= budget => {
            used += rows(err);
            Some(Line::styled(err.clone(), Style::default().fg(theme.error)))
        }
        _ => None,
    };

    // Priority 3: a failed panel's raw-output pointer, middle-ellipsized to
    // the interior width so it costs exactly one row.
    let raw_line = match raw_path {
        // `used < budget`: the ellipsized pointer costs exactly one row.
        Some(path) if panel.status == AgentStatus::Failed && used < budget => {
            used += 1;
            Some(Line::styled(
                middle_ellipsize(path, width),
                theme.dim_style(),
            ))
        }
        _ => None,
    };

    // Priority 4: the round-1 snapshot. This is the one line rendered ABOVE
    // the status, so admitting it when it doesn't fit is exactly what would
    // push the status's tail off the strip.
    let round1 = match &panel.round1_line {
        Some(r1) if used + rows(r1) <= budget => {
            used += rows(r1);
            Some(Line::styled(r1.clone(), theme.dim_style()))
        }
        _ => None,
    };

    // Priority 5: the activity snippet, truncated to the rows still free
    // (keeping the newest chars — it is a live stream tail).
    let snippet = if panel.last_text.is_empty() || used >= budget {
        None
    } else {
        let keep = (budget - used) * width;
        let total = panel.last_text.chars().count();
        let text: String = panel
            .last_text
            .chars()
            .skip(total.saturating_sub(keep))
            .collect();
        Some(Line::styled(text, theme.dim_style()))
    };

    // Display order differs from priority order: the round-1 snapshot
    // renders above the status line.
    let mut lines = Vec::new();
    if let Some(l) = round1 {
        lines.push(l);
    }
    lines.push(Line::styled(status_text, status_style));
    if let Some(l) = error_line {
        lines.push(l);
    }
    if let Some(l) = raw_line {
        lines.push(l);
    }
    if let Some(l) = snippet {
        lines.push(l);
    }
    lines
}

fn draw_header(f: &mut Frame, area: Rect, state: &DashboardState, theme: &Theme) {
    let total_rounds = 1 + usize::from(state.cross_review);
    let round = if state.phase == Phase::Round2 { 2 } else { 1 };
    let line = Line::from(vec![
        Span::styled(
            "reviewing",
            theme.accent_style().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " {} · {} · round {round} of {total_rounds}",
            state.target_desc, state.model_label
        )),
    ]);
    f.render_widget(Paragraph::new(line), area);

    let clock = match state.finished_at {
        Some(finished) => {
            let secs = finished.duration_since(state.started).as_secs();
            format!("finished in {:02}:{:02}", secs / 60, secs % 60)
        }
        None => {
            let secs = state.started.elapsed().as_secs();
            format!("elapsed {:02}:{:02}", secs / 60, secs % 60)
        }
    };
    f.render_widget(
        Paragraph::new(Line::styled(clock, theme.dim_style())).alignment(Alignment::Right),
        area,
    );
}

fn draw_ticker(f: &mut Frame, area: Rect, state: &DashboardState, theme: &Theme) {
    let block = theme.panel(&format!("findings so far — {}", state.ticker.len()), false);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let capacity = inner.height as usize;
    let overflow = state.ticker.len().saturating_sub(capacity);
    let lines: Vec<Line> = state
        .ticker
        .iter()
        .skip(overflow)
        .map(|item| {
            let pcolor = theme.persona_color(
                &item.persona,
                state
                    .persona_colors
                    .get(&item.persona)
                    .and_then(|c| c.as_deref()),
            );
            // "● " (2 cols) + title + persona name; whatever's left pads
            // between the title and the right-aligned persona name.
            let used = 2 + item.title.chars().count() + item.persona.chars().count();
            let pad = (inner.width as usize).saturating_sub(used);
            Line::from(vec![
                Span::styled("●", theme.severity(item.severity)),
                Span::raw(" "),
                Span::raw(item.title.clone()),
                Span::raw(" ".repeat(pad)),
                Span::styled(item.persona.clone(), Style::default().fg(pcolor)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

/// Below 80 columns the `c cancel` hint moves to the front of the line —
/// Paragraph clips from the right, so the hint must not be the thing
/// sacrificed when the line doesn't fit.
fn draw_progress(f: &mut Frame, area: Rect, state: &DashboardState, theme: &Theme) {
    let total = state.agents.len();
    let done = state
        .agents
        .values()
        .filter(|p| matches!(p.status, AgentStatus::Done | AgentStatus::Failed))
        .count();
    let (filled, rest) = crate::ui::format::progress_bar(done, total, 12);
    let count_text = if state.phase == Phase::Round2 {
        format!("round 2: {done}/{total}")
    } else {
        format!("{done}/{total}")
    };
    let promise = "reviewers done — triage opens automatically when all finish";
    let hints = theme.hint_spans(&[("c", "cancel")]);

    let mut spans: Vec<Span> = Vec::new();
    if area.width < 80 {
        spans.extend(hints);
        spans.push(Span::styled(" · ", theme.dim_style()));
        spans.push(Span::styled(filled, Style::default().fg(theme.status_done)));
        spans.push(Span::styled(rest, theme.dim_style()));
        spans.push(Span::raw(format!(" {count_text} {promise}")));
    } else {
        spans.push(Span::styled(filled, Style::default().fg(theme.status_done)));
        spans.push(Span::styled(rest, theme.dim_style()));
        spans.push(Span::raw(format!(" {count_text} {promise} · ")));
        spans.extend(hints);
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Deliberately `severity_warning`, never `theme.error` — this is an
/// in-flight confirmation, not a failure.
fn draw_armed_line(f: &mut Frame, area: Rect, state: &DashboardState, theme: &Theme) {
    let warn = Style::default().fg(theme.severity_warning);
    let kept = state
        .agents
        .values()
        .filter(|p| p.status == AgentStatus::Done)
        .count();
    let lost: Vec<&str> = state
        .personas
        .iter()
        .filter(|p| {
            state
                .agents
                .get(*p)
                .is_none_or(|panel| panel.status != AgentStatus::Done)
        })
        .map(String::as_str)
        .collect();

    let review_word = if kept == 1 { "review" } else { "reviews" };
    let mut spans = vec![
        Span::styled("cancel?", warn.add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {kept} finished {review_word} kept"), warn),
    ];
    if !lost.is_empty() {
        spans.push(Span::styled(" · ", theme.dim_style()));
        spans.push(Span::styled(
            format!("{}'s work lost", lost.join(", ")),
            warn,
        ));
    }
    spans.push(Span::styled(" · ", theme.dim_style()));
    spans.push(Span::styled("c", theme.accent_style()));
    spans.push(Span::styled(" confirm", warn));
    spans.push(Span::styled(" · ", theme.dim_style()));
    spans.push(Span::styled("any key", theme.accent_style()));
    spans.push(Span::styled(" keep running", warn));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Deliberately NEUTRAL (dim), never `theme.error` red — a cancel is an
/// operator choice rather than a failure.
fn draw_cancelled(f: &mut Frame, area: Rect, kept: usize, resumable: bool, theme: &Theme) {
    let block = theme.panel("run cancelled", false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let review_word = if kept == 1 { "review" } else { "reviews" };
    let line = if resumable {
        format!("  {kept} finished {review_word} kept — this run can be triaged")
    } else {
        format!("  {kept} {review_word} finished — not enough to synthesize (need 2)")
    };
    f.render_widget(Paragraph::new(Line::styled(line, theme.dim_style())), inner);
}

fn draw_summary(
    f: &mut Frame,
    area: Rect,
    state: &DashboardState,
    pending: &PendingTriage,
    theme: &Theme,
) {
    let warn = Style::default().fg(theme.severity_warning);
    let block = crate::ui::theme::bordered()
        .title(crate::ui::theme::inset_title(
            Line::from(Span::styled("reviews complete — degraded", warn)),
            warn,
        ))
        .border_style(warn);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let synthesis = &pending.synthesis;
    let findings_total = synthesis.findings.len();

    let mut line1: Vec<Span> = vec![Span::raw(format!("  {findings_total} findings from "))];
    for (i, persona) in synthesis.verdicts.keys().enumerate() {
        if i > 0 {
            line1.push(Span::raw(" + "));
        }
        let pcolor = theme.persona_color(
            persona,
            state.persona_colors.get(persona).and_then(|c| c.as_deref()),
        );
        line1.push(Span::styled(persona.clone(), Style::default().fg(pcolor)));
    }
    line1.push(Span::raw(" · "));
    // Failed names join with the same ` + ` as the contributors, so the two
    // name lists on this line read as one visual grammar.
    line1.push(Span::styled(synthesis.degraded.join(" + "), warn));
    line1.push(Span::raw(" failed and is excluded"));

    let k = synthesis.verdicts.len();
    let requested = k + synthesis.degraded.len();
    let line2 = Line::styled(
        format!("  the consensus verdict below counts {k} of {requested} requested reviewers"),
        theme.dim_style(),
    );

    f.render_widget(
        Paragraph::new(vec![Line::from(line1), line2]).wrap(Wrap { trim: false }),
        inner,
    );
}

/// The side-by-side per-persona status columns, shared by every dashboard
/// state INCLUDING the failure view. The failed panel's raw-output pointer
/// is built here — the only place that knows both the persona name and
/// `failure_run_id` — and handed to [`panel_lines`] for row-budgeting.
fn draw_panels(f: &mut Frame, area: Rect, state: &DashboardState, theme: &Theme) {
    let n = state.personas.len().max(1) as u32;
    let columns = Layout::horizontal((0..n).map(|_| Constraint::Ratio(1, n))).split(area);

    let empty_panel = AgentPanel::new();
    for (i, persona) in state.personas.iter().enumerate() {
        let panel = state.agents.get(persona).unwrap_or(&empty_panel);
        let pcolor = theme.persona_color(
            persona,
            state.persona_colors.get(persona).and_then(|c| c.as_deref()),
        );
        // Running elements always wear the persona color — agent-status
        // Running has no themed color of its own.
        let border = match panel.status {
            AgentStatus::Running => Style::default().fg(pcolor),
            AgentStatus::Retrying => Style::default().fg(theme.status_retrying),
            AgentStatus::Failed => Style::default().fg(theme.status_failed),
            AgentStatus::Pending | AgentStatus::Done => theme.dim_style(),
        };
        let raw_path = match (&panel.status, &state.failure_run_id) {
            (AgentStatus::Failed, Some(run_id)) => Some(format!(
                "raw output: .reviewal/runs/{run_id}/round1/{persona}.raw.txt"
            )),
            _ => None,
        };
        let lines = panel_lines(
            panel,
            pcolor,
            theme,
            columns[i].width.saturating_sub(2),
            columns[i].height.saturating_sub(2),
            raw_path.as_deref(),
        );
        f.render_widget(
            Paragraph::new(lines)
                .block(crate::ui::theme::bordered().border_style(border).title(
                    crate::ui::theme::inset_title(
                        Line::from(Span::styled(
                            persona.as_str().to_owned(),
                            Style::default().fg(pcolor).add_modifier(Modifier::BOLD),
                        )),
                        border,
                    ),
                ))
                .wrap(Wrap { trim: false }),
            columns[i],
        );
    }
}

pub(crate) fn draw(f: &mut Frame, area: Rect, state: &DashboardState, theme: &Theme) {
    if let Some(err) = &state.failure {
        // The banner sits ABOVE the same persona-panel band the live
        // dashboard draws, not instead of it. Its height is sized to the
        // wrapped message so a long failure text is never silently clipped.
        let body = format!("Run failed\n{err}");
        let interior_width = area.width.saturating_sub(2).max(1) as usize;
        let rows: usize = body
            .lines()
            .map(|line| crate::ui::format::word_fit_rows(line, interior_width))
            .sum();
        let banner_height = (rows as u16).saturating_add(2); // + top/bottom border

        let [header, banner, strips, _rest, hint] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(banner_height),
            Constraint::Length(6),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);

        draw_header(f, header, state, theme);
        f.render_widget(
            Paragraph::new(body)
                .style(Style::default().fg(theme.error))
                .block(
                    crate::ui::theme::bordered()
                        .title(crate::ui::theme::inset_title(
                            Line::raw("run failed"),
                            Style::default().fg(theme.error),
                        ))
                        .border_style(Style::default().fg(theme.error)),
                )
                .wrap(Wrap { trim: false }),
            banner,
        );
        draw_panels(f, strips, state, theme);
        f.render_widget(
            Paragraph::new(theme.hints(&[("esc", "home"), ("q", "quit")])),
            hint,
        );
        return;
    }

    // Persona strips are side-by-side columns capped at a 6-row band:
    // stacked full-width bands would overflow a 24-row terminal (6 personas
    // × 6 rows) before the ticker even starts.
    let [header, strips, ticker_area, bottom] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(6),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(f, header, state, theme);
    draw_panels(f, strips, state, theme);

    if let Some(pending) = &state.summary {
        draw_summary(f, ticker_area, state, pending, theme);
        let findings_total = pending.synthesis.findings.len();
        let hints = theme.hints(&[
            ("enter", &format!("triage {findings_total} findings")),
            ("esc", "home"),
        ]);
        f.render_widget(Paragraph::new(hints), bottom);
    } else if let Some((kept, resumable, _run_id)) = &state.cancelled {
        draw_cancelled(f, ticker_area, *kept, *resumable, theme);
        let hints = if *resumable {
            theme.hints(&[("enter", "triage"), ("esc", "home"), ("q", "quit")])
        } else {
            theme.hints(&[("esc", "home"), ("q", "quit")])
        };
        f.render_widget(Paragraph::new(hints), bottom);
    } else {
        draw_ticker(f, ticker_area, state, theme);
        if state.cancel_armed {
            draw_armed_line(f, bottom, state, theme);
        } else {
            draw_progress(f, bottom, state, theme);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::app::render_to_text;
    use crate::ui::test_keys::{key, key_code};

    #[test]
    fn events_drive_panels_and_completion() {
        let mut state = DashboardState::new(
            vec!["prover".into(), "skeptic".into()],
            "diff vs HEAD".into(),
            "opus".into(),
            false,
        );
        assert!(state
            .handle_run_event(RunEvent::PhaseChanged(Phase::Round1))
            .is_none());
        state.handle_run_event(RunEvent::AgentStarted {
            persona: "prover".into(),
        });
        state.handle_run_event(RunEvent::AgentActivity {
            persona: "prover".into(),
            activity: AgentActivity::TextDelta("hello".into()),
        });
        assert_eq!(state.agents["prover"].status, AgentStatus::Running);
        assert!(state.agents["prover"].last_text.contains("hello"));
        state.handle_run_event(RunEvent::AgentDone {
            persona: "prover".into(),
            duration_secs: 7,
            findings: vec![
                crate::engine::model::FindingBrief {
                    severity: crate::engine::model::Severity::Critical,
                    title: "find1".into(),
                },
                crate::engine::model::FindingBrief {
                    severity: crate::engine::model::Severity::Warning,
                    title: "find2".into(),
                },
                crate::engine::model::FindingBrief {
                    severity: crate::engine::model::Severity::Info,
                    title: "find3".into(),
                },
            ],
            saved: true,
        });
        assert_eq!(state.agents["prover"].status, AgentStatus::Done);
        assert_eq!(state.agents["prover"].findings_count, 3);

        let syn =
            crate::engine::synthesis::synthesize(&Default::default(), &Default::default(), &[]);
        let t = state.handle_run_event(RunEvent::RunCompleted {
            run_id: "r".into(),
            synthesis: syn,
        });
        assert!(matches!(t, Some(Transition::ToTriage { .. })));
    }

    #[test]
    fn last_text_truncates_by_chars_not_bytes() {
        let mut state = DashboardState::new(
            vec!["prover".into()],
            "diff vs HEAD".into(),
            "opus".into(),
            false,
        );
        // 300 three-byte chars: byte-based truncation would keep only ~66 chars.
        state.handle_run_event(RunEvent::AgentActivity {
            persona: "prover".into(),
            activity: AgentActivity::TextDelta("—".repeat(300)),
        });
        state.handle_run_event(RunEvent::AgentActivity {
            persona: "prover".into(),
            activity: AgentActivity::TextDelta("end!".into()),
        });
        let text = &state.agents["prover"].last_text;
        assert_eq!(text.chars().count(), 200);
        assert!(text.ends_with("end!"), "most recent chars kept");
        assert!(text.starts_with('—'), "older stream tail kept");
    }

    #[test]
    fn running_panel_shows_clock_and_spinner() {
        let mut state = DashboardState::new(
            vec!["prover".into()],
            "diff vs HEAD".into(),
            "opus".into(),
            false,
        );
        state.handle_run_event(RunEvent::AgentStarted {
            persona: "prover".into(),
        });
        let lines = panel_lines(
            &state.agents["prover"],
            ratatui::style::Color::Cyan,
            &Theme::default(),
            40,
            4,
            None,
        );
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("working"), "{text}");
        assert!(text.contains("00:0"), "elapsed clock rendered: {text}");
    }

    #[test]
    fn token_counter_marks_estimates_with_tilde_and_resets_per_invoke() {
        let mut state = DashboardState::new(
            vec!["prover".into()],
            "diff vs HEAD".into(),
            "opus".into(),
            false,
        );
        state.handle_run_event(RunEvent::AgentStarted {
            persona: "prover".into(),
        });
        state.handle_run_event(RunEvent::AgentActivity {
            persona: "prover".into(),
            activity: AgentActivity::Tokens {
                count: 123,
                exact: false,
            },
        });
        let text = render_to_text(80, 20, |f| draw(f, f.area(), &state, &Theme::default()));
        assert!(text.contains("~123 tok"), "estimate gets a tilde: {text}");
        state.handle_run_event(RunEvent::AgentActivity {
            persona: "prover".into(),
            activity: AgentActivity::Tokens {
                count: 120,
                exact: true,
            },
        });
        let text = render_to_text(80, 20, |f| draw(f, f.area(), &state, &Theme::default()));
        assert!(text.contains(" 120 tok"), "{text}");
        assert!(
            !text.contains("~120"),
            "exact count drops the tilde: {text}"
        );

        state.handle_run_event(RunEvent::AgentStarted {
            persona: "prover".into(),
        });
        assert_eq!(
            state.agents["prover"].tokens, 0,
            "AgentStarted resets the counter"
        );
        assert!(state.agents["prover"].tokens_exact);
        state.handle_run_event(RunEvent::AgentActivity {
            persona: "prover".into(),
            activity: AgentActivity::Tokens {
                count: 7,
                exact: false,
            },
        });
        state.handle_run_event(RunEvent::AgentRetrying {
            persona: "prover".into(),
            error: "invalid json".into(),
        });
        assert_eq!(
            state.agents["prover"].tokens, 0,
            "a retry re-invokes, so it resets the counter too"
        );
    }

    #[test]
    fn run_failure_sets_state_guards_keys_and_renders_banner() {
        let mut state = DashboardState::new(
            vec!["prover".into()],
            "diff vs HEAD".into(),
            "opus".into(),
            false,
        );

        assert!(state.handle_key(key_code(KeyCode::Esc)).is_none());
        assert!(state.handle_key(key('q')).is_none());

        assert!(state
            .handle_run_event(RunEvent::RunFailed {
                run_id: Some("r".into()),
                message: "boom".into(),
            })
            .is_none());
        assert_eq!(state.failure.as_deref(), Some("boom"));
        assert_eq!(
            state.failure_run_id.as_deref(),
            Some("r"),
            "RunFailed captures the run id"
        );

        assert!(matches!(
            state.handle_key(key_code(KeyCode::Esc)),
            Some(Transition::ToHome)
        ));
        assert!(matches!(state.handle_key(key('q')), Some(Transition::Quit)));

        let text = render_to_text(60, 12, |f| draw(f, f.area(), &state, &Theme::default()));
        assert!(text.contains("Run failed"));
        assert!(text.contains("boom"));
        assert!(text.contains("esc home · q quit"), "{text}");
    }

    #[test]
    fn failed_panel_border_is_red_and_running_border_is_persona_color() {
        let mut state = DashboardState::new(
            vec!["prover".into(), "skeptic".into()],
            "diff vs HEAD".into(),
            "opus".into(),
            false,
        );
        state.handle_run_event(RunEvent::AgentStarted {
            persona: "prover".into(),
        });
        state.handle_run_event(RunEvent::AgentFailed {
            persona: "skeptic".into(),
            error: "boom".into(),
        });
        let buffer = crate::ui::app::render_to_buffer(120, 30, |f| {
            draw(f, f.area(), &state, &Theme::default())
        });
        // The header (Length(1)) occupies row 0, so the side-by-side strip
        // borders start at row 1.
        // Column 1 (prover, running) top-left corner: persona color = Cyan.
        assert_eq!(
            buffer[(0, 1)].style().fg,
            Some(ratatui::style::Color::Cyan),
            "running border wears the persona color"
        );
        // Column 2 (skeptic, failed) starts at x = 60 on a 120-wide frame.
        assert_eq!(
            buffer[(60, 1)].style().fg,
            Some(ratatui::style::Color::Red),
            "failed border is status_failed"
        );
    }

    #[test]
    fn draw_shows_persona_strips() {
        let state = DashboardState::new(
            vec!["prover".into(), "skeptic".into()],
            "diff vs HEAD".into(),
            "opus".into(),
            false,
        );
        let text = render_to_text(120, 30, |f| draw(f, f.area(), &state, &Theme::default()));
        assert!(text.contains("prover") && text.contains("skeptic"));
    }

    fn state3() -> DashboardState {
        DashboardState::new(
            vec!["prover".into(), "breaker".into(), "steward".into()],
            "diff vs HEAD (uncommitted)".into(),
            "opus".into(),
            false,
        )
    }

    fn state3_cross() -> DashboardState {
        DashboardState::new(
            vec!["prover".into(), "breaker".into(), "steward".into()],
            "diff vs HEAD (uncommitted)".into(),
            "opus".into(),
            true,
        )
    }

    fn brief(
        sev: crate::engine::model::Severity,
        title: &str,
    ) -> crate::engine::model::FindingBrief {
        crate::engine::model::FindingBrief {
            severity: sev,
            title: title.into(),
        }
    }

    #[test]
    fn agent_done_feeds_ticker_and_saved_suffix() {
        use crate::engine::model::Severity;
        // Two personas, not state3(): the full done line (33 chars) wraps
        // inside a third-width column at width 100, which would break the
        // contiguous-string assertion below; a half-width column fits it.
        let mut s = DashboardState::new(
            vec!["prover".into(), "steward".into()],
            "diff vs HEAD (uncommitted)".into(),
            "opus".into(),
            false,
        );
        s.handle_run_event(RunEvent::AgentDone {
            persona: "steward".into(),
            duration_secs: 58,
            saved: true,
            findings: vec![brief(Severity::Critical, "walk_dir follows symlink cycles")],
        });
        assert_eq!(s.ticker.len(), 1);
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &s, &Theme::default()));
        assert!(text.contains("findings so far — 1"), "{text}");
        assert!(text.contains("● walk_dir follows symlink cycles"), "{text}");
        assert!(text.contains("✓ done · 1 findings · 58s — saved"), "{text}");
    }

    #[test]
    fn header_names_target_model_and_round_count() {
        let s = state3();
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &s, &Theme::default()));
        assert!(
            text.contains("reviewing diff vs HEAD (uncommitted) · opus · round 1 of 1"),
            "{text}"
        );
        assert!(text.contains("elapsed 00:0"), "{text}");
        assert!(
            !text.contains("round 2"),
            "no phantom round-2 for cross_review=false: {text}"
        );
    }

    #[test]
    fn round2_keeps_round1_results_visible() {
        use crate::engine::model::Severity;
        let mut s = state3_cross();
        s.handle_run_event(RunEvent::AgentDone {
            persona: "prover".into(),
            duration_secs: 74,
            saved: true,
            findings: vec![brief(Severity::Info, "x"); 5],
        });
        s.handle_run_event(RunEvent::PhaseChanged(Phase::Round2));
        s.handle_run_event(RunEvent::AgentStarted {
            persona: "prover".into(),
        });
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &s, &Theme::default()));
        assert!(
            text.contains("r1: 5 findings · 74s"),
            "round-1 result preserved: {text}"
        );
        assert!(text.contains("round 2 of 2"), "{text}");
    }

    #[test]
    fn progress_line_promises_the_transition() {
        let s = state3();
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &s, &Theme::default()));
        assert!(
            text.contains("0/3 reviewers done — triage opens automatically when all finish"),
            "{text}"
        );
        assert!(text.contains("c cancel"), "{text}");
    }

    #[test]
    fn middle_ellipsize_keeps_exact_fits_and_prefers_the_tail() {
        assert_eq!(middle_ellipsize("0123456789", 10), "0123456789");
        // Tight fit: the tail keeps the larger share of the budget.
        assert_eq!(middle_ellipsize("0123456789", 5), "0…789");
        // The load-bearing case: at a ~31-col column interior the 80-char
        // raw-output pointer keeps its whole round1/{persona}.raw.txt tail
        // — the run id in the middle is what gives way.
        let path =
            "raw output: .reviewal/runs/2026-07-10T09-00-00Z-diff-head/round1/prover.raw.txt";
        let e = middle_ellipsize(path, 31);
        assert_eq!(e.chars().count(), 31);
        assert!(e.contains('…'), "{e}");
        assert!(e.ends_with("round1/prover.raw.txt"), "{e}");
        assert_eq!(middle_ellipsize("0123456789", 2), "…9");
        assert_eq!(middle_ellipsize("0123456789", 1), "…");
        // Empty input never grows (and width 0 never panics).
        assert_eq!(middle_ellipsize("", 5), "");
        assert_eq!(middle_ellipsize("abc", 0), "");
    }

    /// One panel Done with `saved: true` and a round-1 snapshot — content
    /// that overflows a narrow column's 4-row interior.
    fn state6_done_saved_with_r1() -> DashboardState {
        use crate::engine::model::Severity;
        let mut s = DashboardState::new(
            vec![
                "prover".into(),
                "breaker".into(),
                "skeptic".into(),
                "stickler".into(),
                "steward".into(),
                "advocate".into(),
            ],
            "diff vs HEAD (uncommitted)".into(),
            "opus".into(),
            true,
        );
        s.handle_run_event(RunEvent::AgentDone {
            persona: "prover".into(),
            duration_secs: 12,
            saved: true,
            findings: vec![brief(Severity::Info, "x"); 3],
        });
        s.agents.get_mut("prover").unwrap().round1_line = Some("r1: 3 findings · 12s".into());
        s
    }

    #[test]
    fn narrow_panel_sacrifices_round1_line_before_the_saved_suffix() {
        // 6 personas at width 100: each column's interior is ~14 chars, so
        // the wrapped done line (3 rows) plus the round-1 snapshot (2 rows)
        // overflows the 4-row interior, and ratatui's Paragraph silently
        // drops the overflow — the tail of the status line (`— saved`).
        let s = state6_done_saved_with_r1();
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &s, &Theme::default()));
        assert!(text.contains("saved"), "status line survives: {text}");
        assert!(
            !text.contains("r1: 3 findings"),
            "round-1 snapshot sacrificed first: {text}"
        );
    }

    #[test]
    fn word_wrap_waste_cannot_readmit_the_round1_line() {
        // Same state at width 120: interior 18. A char-count estimate calls
        // the done line 2 rows (33/18), but ratatui WORD-wraps it to 3 —
        // word breaks waste end-of-line space, so char-count is only a lower
        // bound; an under-counting estimator readmits the r1 snapshot and
        // `saved` drops again.
        let s = state6_done_saved_with_r1();
        let text = render_to_text(120, 30, |f| draw(f, f.area(), &s, &Theme::default()));
        assert!(text.contains("saved"), "status line survives: {text}");
        assert!(
            !text.contains("r1: 3 findings"),
            "round-1 snapshot sacrificed first: {text}"
        );
    }

    #[test]
    fn persona_strips_are_side_by_side_columns_not_stacked_bands() {
        // One column per persona: all names land on the SAME frame row;
        // stacked full-width bands would put each name on its own row.
        let s = state3();
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &s, &Theme::default()));
        let row = text
            .lines()
            .find(|l| l.contains("prover"))
            .expect("a row names prover");
        assert!(
            row.contains("breaker") && row.contains("steward"),
            "all persona titles share one row: {row}"
        );
    }

    #[test]
    fn cancel_is_two_step_and_any_key_disarms() {
        let mut s = state3();
        assert!(s.handle_key(key('c')).is_none(), "first c arms");
        assert!(s.cancel_armed);
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &s, &Theme::default()));
        assert!(text.contains("cancel?"), "{text}");
        assert!(text.contains("c confirm"), "{text}");
        assert!(s.handle_key(key('j')).is_none(), "other key disarms");
        assert!(!s.cancel_armed);
        s.handle_key(key('c'));
        assert!(
            matches!(s.handle_key(key('c')), Some(Transition::CancelRun)),
            "second c confirms"
        );
    }

    #[test]
    fn armed_line_counts_kept_and_lost() {
        let mut s = state3();
        s.handle_run_event(RunEvent::AgentDone {
            persona: "breaker".into(),
            duration_secs: 134,
            saved: true,
            findings: vec![],
        });
        s.handle_run_event(RunEvent::AgentDone {
            persona: "steward".into(),
            duration_secs: 58,
            saved: true,
            findings: vec![],
        });
        s.handle_key(key('c'));
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &s, &Theme::default()));
        assert!(text.contains("2 finished reviews kept"), "{text}");
        assert!(text.contains("prover's work lost"), "{text}");
    }

    #[test]
    fn armed_line_comma_joins_multiple_lost_names() {
        let mut s = state3();
        s.handle_run_event(RunEvent::AgentDone {
            persona: "steward".into(),
            duration_secs: 58,
            saved: true,
            findings: vec![],
        });
        s.handle_key(key('c'));
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &s, &Theme::default()));
        assert!(text.contains("1 finished review kept"), "{text}");
        assert!(!text.contains("1 finished reviews kept"), "{text}");
        assert!(text.contains("prover, breaker's work lost"), "{text}");
    }

    #[test]
    fn cancelled_state_is_neutral_and_resumable_offers_triage() {
        use crate::ui::app::{assert_no_cell_with_fg, buffer_text, render_to_buffer};

        let mut s = state3();
        s.handle_run_event(RunEvent::RunCancelled {
            run_id: "r1".into(),
            kept_reviews: 2,
            resumable: true,
        });
        let buf = render_to_buffer(100, 30, |f| draw(f, f.area(), &s, &Theme::default()));
        let text = buffer_text(&buf);
        assert!(text.contains("run cancelled"), "{text}");
        assert!(text.contains("2 finished reviews kept"), "{text}");
        // Neutral: the words "run cancelled" must not be error-red.
        assert_no_cell_with_fg(&buf, "run cancelled", Color::Red);
        assert!(matches!(
            s.handle_key(key_code(KeyCode::Enter)),
            Some(Transition::ReopenTriage { .. })
        ));
    }

    #[test]
    fn cancelled_box_singularizes_a_single_kept_review() {
        let mut s = state3();
        s.handle_run_event(RunEvent::RunCancelled {
            run_id: "r1".into(),
            kept_reviews: 1,
            resumable: true,
        });
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &s, &Theme::default()));
        assert!(text.contains("1 finished review kept"), "{text}");
        assert!(!text.contains("1 finished reviews kept"), "{text}");
    }

    #[test]
    fn cancelled_box_singularizes_the_not_enough_to_synthesize_line() {
        let mut s = state3();
        s.handle_run_event(RunEvent::RunCancelled {
            run_id: "r1".into(),
            kept_reviews: 1,
            resumable: false,
        });
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &s, &Theme::default()));
        assert!(text.contains("1 review finished"), "{text}");
        assert!(!text.contains("1 reviews finished"), "{text}");
    }

    fn synthesis_clean() -> Synthesis {
        Synthesis {
            findings: vec![],
            verdicts: BTreeMap::new(),
            summaries: BTreeMap::new(),
            consensus_label: "SHIP (unanimous, 2/2)".into(),
            consensus_score: 1.0,
            degraded: vec![],
        }
    }

    fn synthesis_degraded_with(n: usize, contributors: &[&str], failed: &[&str]) -> Synthesis {
        use crate::engine::model::Verdict;
        use crate::engine::synthesis::{Confidence, Finding};

        let findings = (0..n)
            .map(|i| Finding {
                id: format!("f{i}"),
                severity: crate::engine::model::Severity::Info,
                title: format!("finding {i}"),
                detail: "detail".into(),
                file: None,
                line: None,
                fix: None,
                reporters: vec![],
                validators: vec![],
                challengers: vec![],
                confidence: Confidence::Solo,
            })
            .collect();
        let verdicts = contributors
            .iter()
            .map(|p| (p.to_string(), Verdict::Approve))
            .collect();
        let summaries = contributors
            .iter()
            .map(|p| (p.to_string(), "s".to_string()))
            .collect();
        Synthesis {
            findings,
            verdicts,
            summaries,
            consensus_label: "SHIP (unanimous, 2/2)".into(),
            consensus_score: 1.0,
            degraded: failed.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn clean_completion_still_auto_advances() {
        let mut s = state3();
        let t = s.handle_run_event(RunEvent::RunCompleted {
            run_id: "r".into(),
            synthesis: synthesis_clean(),
        });
        assert!(matches!(t, Some(Transition::ToTriage { .. })));
    }

    #[test]
    fn degraded_completion_pauses_on_summary() {
        let mut s = state3();
        let t = s.handle_run_event(RunEvent::RunCompleted {
            run_id: "r".into(),
            synthesis: synthesis_degraded_with(10, &["prover", "steward"], &["breaker"]),
        });
        assert!(t.is_none(), "degraded runs must not auto-advance");
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &s, &Theme::default()));
        assert!(text.contains("reviews complete — degraded"), "{text}");
        assert!(text.contains("10 findings from prover + steward"), "{text}");
        assert!(text.contains("breaker failed and is excluded"), "{text}");
        assert!(
            text.contains("the consensus verdict below counts 2 of 3 requested reviewers"),
            "{text}"
        );
        assert!(text.contains("enter triage 10 findings"), "{text}");
        assert!(matches!(
            s.handle_key(key_code(KeyCode::Enter)),
            Some(Transition::ToTriage { .. })
        ));
    }

    #[test]
    fn failure_banner_keeps_panels_and_raw_paths() {
        let mut s = state3();
        s.handle_run_event(RunEvent::AgentFailed {
            persona: "prover".into(),
            error: "timed out after 600s".into(),
        });
        s.handle_run_event(RunEvent::AgentFailed {
            persona: "breaker".into(),
            error: "schema validation failed".into(),
        });
        s.handle_run_event(RunEvent::RunFailed {
            run_id: Some("2026-07-10T09-00-00Z-diff-head".into()),
            message: "fewer than 2 reviewers produced valid output — synthesis aborted".into(),
        });
        let text = render_to_text(100, 40, |f| draw(f, f.area(), &s, &Theme::default()));
        assert!(text.contains("synthesis aborted"), "{text}");
        assert!(
            text.contains("timed out after 600s"),
            "panels survive the banner: {text}"
        );
        assert!(text.contains("round1/prover.raw.txt"), "{text}");
        assert!(
            text.contains("…"),
            "path is ellipsized, not clipped: {text}"
        );
    }

    #[test]
    fn six_failed_panels_at_80x24_keep_columns_and_status_lines() {
        let personas = [
            "prover", "breaker", "skeptic", "stickler", "steward", "advocate",
        ];
        let mut s = DashboardState::new(
            personas.iter().map(|p| p.to_string()).collect(),
            "diff vs HEAD (uncommitted)".into(),
            "opus".into(),
            false,
        );
        for p in personas {
            s.handle_run_event(RunEvent::AgentFailed {
                persona: p.into(),
                error: "timed out after 600s".into(),
            });
        }
        s.handle_run_event(RunEvent::RunFailed {
            run_id: Some("2026-07-10T09-00-00Z-diff-head".into()),
            message: "fewer than 2 reviewers produced valid output — synthesis aborted".into(),
        });
        let text = render_to_text(80, 24, |f| draw(f, f.area(), &s, &Theme::default()));
        assert!(text.contains("synthesis aborted"), "banner visible: {text}");
        assert_eq!(
            text.matches("✗ failed").count(),
            6,
            "every panel's status line renders: {text}"
        );
        let row = text
            .lines()
            .find(|l| l.contains("prover"))
            .expect("a row names prover");
        assert!(
            row.contains("breaker") && row.contains("advocate"),
            "columns, not stacked bands: {row}"
        );
    }

    #[test]
    fn degraded_summary_joins_multiple_failed_names_with_plus() {
        let mut s = state3();
        s.handle_run_event(RunEvent::RunCompleted {
            run_id: "r".into(),
            synthesis: synthesis_degraded_with(5, &["prover"], &["breaker", "steward"]),
        });
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &s, &Theme::default()));
        assert!(text.contains("5 findings from prover"), "{text}");
        assert!(
            text.contains("breaker + steward failed and is excluded"),
            "failed names join with the same ` + ` as contributors: {text}"
        );
        assert!(
            text.contains("the consensus verdict below counts 1 of 3 requested reviewers"),
            "{text}"
        );
    }
}
