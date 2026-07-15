use crate::engine::model::Severity;
use crate::engine::store::{Triage, TriageEntry, TriageStatus};
use crate::engine::synthesis::{Finding, Synthesis};
use crate::ui::app::Transition;
use crate::ui::theme::Theme;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::Frame;
use std::collections::BTreeMap;

pub(crate) enum Mode {
    List,
    FilterInput,
    /// Note editor. `dismiss: true` = the d-flow (sets Dismissed on submit);
    /// false = the n-flow (keeps the current status, only sets the note).
    NoteInput {
        text: String,
        dismiss: bool,
    },
    ConfirmFinalize,
    Help,
}

pub(crate) struct TriageState {
    pub run_id: String,
    pub target_desc: String,
    /// Personas that failed round 1 and are excluded from the consensus verdict.
    pub degraded: Vec<String>,
    pub synthesis: Synthesis,
    pub triage: Triage,
    pub selected: usize,
    pub filter: String,
    pub mode: Mode,
    pub detail_scroll: u16,
    /// (finding id, entry before the last mutation).
    pub undo: Vec<(String, TriageEntry)>,
    pub dirty: bool,
    /// name → raw frontmatter color for personas currently on disk; names
    /// from old runs missing here fall back to slot/hash resolution.
    pub persona_colors: BTreeMap<String, Option<String>>,
}

impl TriageState {
    pub(crate) fn new(
        run_id: String,
        target_desc: String,
        synthesis: Synthesis,
        mut triage: Triage,
    ) -> Self {
        for f in &synthesis.findings {
            triage.entry(f.id.clone()).or_insert_with(|| TriageEntry {
                status: TriageStatus::Deferred,
                note: None,
                touched: false,
            });
        }
        let degraded = synthesis.degraded.clone();
        TriageState {
            run_id,
            target_desc,
            degraded,
            synthesis,
            triage,
            // `visible()` stable-partitions untouched-first, so index 0 is
            // always the inbox frontier — no extra bookkeeping needed to
            // "restore" it on resume.
            selected: 0,
            filter: String::new(),
            mode: Mode::List,
            detail_scroll: 0,
            undo: Vec::new(),
            dirty: false,
            persona_colors: BTreeMap::new(),
        }
    }

    /// Untouched findings first, touched after, preserving synthesized order
    /// within each group.
    pub(crate) fn visible(&self) -> Vec<&Finding> {
        let filtered: Vec<&Finding> = if self.filter.is_empty() {
            self.synthesis.findings.iter().collect()
        } else {
            let needle = self.filter.to_lowercase();
            self.synthesis
                .findings
                .iter()
                .filter(|f| {
                    f.title.to_lowercase().contains(&needle)
                        || f.file
                            .as_deref()
                            .is_some_and(|file| file.to_lowercase().contains(&needle))
                })
                .collect()
        };
        let (mut untouched, touched): (Vec<&Finding>, Vec<&Finding>) =
            filtered.into_iter().partition(|f| !self.touched(&f.id));
        untouched.extend(touched);
        untouched
    }

    fn touched(&self, id: &str) -> bool {
        self.triage.get(id).is_some_and(|e| e.touched)
    }

    fn clamp_selected(&mut self) {
        self.selected = self.selected.min(self.visible().len().saturating_sub(1));
        self.detail_scroll = 0;
    }

    fn set_selected(&mut self, idx: usize) {
        self.selected = idx;
        self.detail_scroll = 0;
    }

    fn record_undo(&mut self, id: &str) {
        if let Some(entry) = self.triage.get(id) {
            self.undo.push((id.to_string(), entry.clone()));
        }
    }

    /// First untouched finding at or after `selected`, evaluated in the
    /// *current* `visible()` ordering — the just-decided entry has already
    /// sunk to the archive, shifting its old neighbours up into its slot.
    fn advance_to_next_untouched(&mut self) {
        let visible = self.visible();
        if visible.is_empty() {
            self.set_selected(0);
            return;
        }
        let anchor = self.selected;
        let next = visible
            .iter()
            .enumerate()
            .skip(anchor)
            .find(|(_, f)| !self.touched(&f.id))
            .map(|(i, _)| i)
            .or_else(|| visible.iter().position(|f| !self.touched(&f.id)));
        match next {
            Some(idx) => self.set_selected(idx),
            None => self.clamp_selected(),
        }
    }

    fn current_note(&self) -> Option<String> {
        let id = &self.visible().get(self.selected)?.id;
        self.triage.get(id)?.note.clone()
    }

    fn accept_or_skip(&mut self, status: TriageStatus) {
        let Some(id) = self.visible().get(self.selected).map(|f| f.id.clone()) else {
            return;
        };
        self.record_undo(&id);
        let note = self.triage.get(&id).and_then(|e| e.note.clone());
        self.triage.insert(
            id,
            TriageEntry {
                status,
                note,
                touched: true,
            },
        );
        self.dirty = true;
        self.advance_to_next_untouched();
    }

    fn undo(&mut self) {
        let Some((id, entry)) = self.undo.pop() else {
            return;
        };
        self.triage.insert(id.clone(), entry);
        self.dirty = true;
        match self.visible().iter().position(|f| f.id == id) {
            Some(idx) => self.set_selected(idx),
            None => self.clamp_selected(),
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Option<Transition> {
        match &self.mode {
            Mode::List => self.handle_list_key(key),
            Mode::FilterInput => {
                self.handle_filter_key(key);
                None
            }
            Mode::NoteInput { .. } => self.handle_note_key(key),
            Mode::ConfirmFinalize => match key.code {
                KeyCode::Char('f') => Some(Transition::Finalize),
                KeyCode::Esc => {
                    self.mode = Mode::List;
                    None
                }
                // Swallowed: a confirmation overlay must not let a stray
                // keystroke (e.g. the `a`/`d` that would otherwise triage
                // the next finding) leak through to the list underneath.
                _ => None,
            },
            Mode::Help => {
                // Any key closes — same contract as every other `?` overlay.
                self.mode = Mode::List;
                None
            }
        }
    }

    /// `(accepted, dismissed, untriaged)`. `untriaged` is exactly
    /// `touched == false`: a touched-but-skipped (deferred) entry counts in
    /// neither `accepted`/`dismissed` nor `untriaged`.
    fn counts(&self) -> (usize, usize, usize) {
        let mut accepted = 0;
        let mut dismissed = 0;
        let mut untriaged = 0;
        for entry in self.triage.values() {
            if !entry.touched {
                untriaged += 1;
            } else if entry.status == TriageStatus::Accepted {
                accepted += 1;
            } else if entry.status == TriageStatus::Dismissed {
                dismissed += 1;
            }
        }
        (accepted, dismissed, untriaged)
    }

    fn handle_list_key(&mut self, key: KeyEvent) -> Option<Transition> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                let len = self.visible().len();
                if len > 0 {
                    let idx = (self.selected + 1).min(len - 1);
                    self.set_selected(idx);
                }
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let idx = self.selected.saturating_sub(1);
                self.set_selected(idx);
                None
            }
            KeyCode::Char('J') => {
                self.detail_scroll = self.detail_scroll.saturating_add(1);
                None
            }
            KeyCode::Char('K') => {
                self.detail_scroll = self.detail_scroll.saturating_sub(1);
                None
            }
            KeyCode::Char('a') => {
                self.accept_or_skip(TriageStatus::Accepted);
                None
            }
            KeyCode::Char('s') | KeyCode::Char(' ') => {
                self.accept_or_skip(TriageStatus::Deferred);
                None
            }
            KeyCode::Char('d') => {
                let text = self.current_note().unwrap_or_default();
                self.mode = Mode::NoteInput {
                    text,
                    dismiss: true,
                };
                None
            }
            KeyCode::Char('n') => {
                let text = self.current_note().unwrap_or_default();
                self.mode = Mode::NoteInput {
                    text,
                    dismiss: false,
                };
                None
            }
            KeyCode::Char('u') => {
                self.undo();
                None
            }
            KeyCode::Char('/') => {
                self.mode = Mode::FilterInput;
                None
            }
            KeyCode::Char('f') => {
                self.mode = Mode::ConfirmFinalize;
                None
            }
            KeyCode::Char('?') => {
                self.mode = Mode::Help;
                None
            }
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.clamp_selected();
                    None
                } else {
                    Some(Transition::ToHome)
                }
            }
            KeyCode::Char('q') => Some(Transition::Quit),
            _ => None,
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.filter.push(c);
                self.clamp_selected();
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.clamp_selected();
            }
            KeyCode::Enter => {
                self.mode = Mode::List;
            }
            KeyCode::Esc => {
                // Unlike List mode's esc-clears-then-esc-leaves two-step, esc
                // inside the input abandons the in-progress filter outright.
                self.filter.clear();
                self.mode = Mode::List;
                self.clamp_selected();
            }
            _ => {}
        }
    }

    fn handle_note_key(&mut self, key: KeyEvent) -> Option<Transition> {
        match key.code {
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Mode::NoteInput { text, .. } = &mut self.mode {
                    text.push(c);
                }
                None
            }
            KeyCode::Backspace => {
                if let Mode::NoteInput { text, .. } = &mut self.mode {
                    text.pop();
                }
                None
            }
            KeyCode::Enter => {
                let Mode::NoteInput { text, dismiss } =
                    std::mem::replace(&mut self.mode, Mode::List)
                else {
                    return None;
                };
                if let Some(id) = self.visible().get(self.selected).map(|f| f.id.clone()) {
                    self.record_undo(&id);
                    let current = self.triage.get(&id).cloned().unwrap_or(TriageEntry {
                        status: TriageStatus::Deferred,
                        note: None,
                        touched: false,
                    });
                    let status = if dismiss {
                        TriageStatus::Dismissed
                    } else {
                        current.status
                    };
                    let touched = if dismiss { true } else { current.touched };
                    let note = (!text.is_empty()).then_some(text);
                    self.triage.insert(
                        id,
                        TriageEntry {
                            status,
                            note,
                            touched,
                        },
                    );
                    self.dirty = true;
                    if dismiss {
                        self.advance_to_next_untouched();
                    }
                }
                None
            }
            KeyCode::Esc => {
                self.mode = Mode::List;
                None
            }
            _ => None,
        }
    }
}

fn severity_dot(theme: &Theme, s: Severity) -> Span<'static> {
    Span::styled("●", theme.severity(s))
}

fn render_detail(
    f: &Finding,
    theme: &Theme,
    persona_colors: &BTreeMap<String, Option<String>>,
) -> Vec<Line<'static>> {
    let pcolor =
        |name: &str| theme.persona_color(name, persona_colors.get(name).and_then(|c| c.as_deref()));

    let mut lines = vec![Line::styled(
        f.title.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    )];

    let mut line2: Vec<Span> = vec![
        Span::styled(format!("{:?}", f.severity), theme.severity(f.severity)),
        Span::raw(" · "),
        Span::styled(
            format!("{:?}", f.confidence),
            Style::default().fg(theme.confidence(&f.confidence)),
        ),
    ];
    match (&f.file, f.line) {
        (Some(file), Some(line)) => {
            line2.push(Span::styled(format!(" · {file}:{line}"), theme.dim_style()))
        }
        (Some(file), None) => line2.push(Span::styled(format!(" · {file}"), theme.dim_style())),
        _ => {}
    }
    lines.push(Line::from(line2));

    lines.push(Line::raw(""));
    lines.push(Line::raw(f.detail.clone()));

    if let Some(fix) = &f.fix {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(
                "→ fix  ",
                Style::default()
                    .fg(theme.status_done)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(fix.clone()),
        ]));
    }

    lines.push(Line::raw(""));
    // Attribution always renders (no expanded gate): challenger evidence in
    // a disputed adjudication must never be hidden behind a toggle.
    let mut attribution: Vec<Span> = vec![Span::raw("found by ")];
    for (i, r) in f.reporters.iter().enumerate() {
        if i > 0 {
            attribution.push(Span::raw(" + "));
        }
        attribution.push(Span::styled(r.clone(), Style::default().fg(pcolor(r))));
    }
    for v in &f.validators {
        attribution.push(Span::raw(" · "));
        attribution.push(Span::styled(
            "validated by ",
            Style::default().fg(theme.status_done),
        ));
        attribution.push(Span::styled(
            v.persona.clone(),
            Style::default().fg(pcolor(&v.persona)),
        ));
        attribution.push(Span::raw(format!(": {}", v.reason)));
    }
    for c in &f.challengers {
        attribution.push(Span::raw(" · "));
        attribution.push(Span::styled(
            "challenged by ",
            Style::default().fg(theme.status_failed),
        ));
        attribution.push(Span::styled(
            c.persona.clone(),
            Style::default().fg(pcolor(&c.persona)),
        ));
        attribution.push(Span::raw(format!(": {}", c.reason)));
    }
    lines.push(Line::from(attribution));

    lines
}

fn draw_header(f: &mut Frame, area: Rect, state: &TriageState, theme: &Theme) {
    let mut spans = vec![
        Span::styled("triage", theme.accent_style().add_modifier(Modifier::BOLD)),
        Span::raw(format!(" — {}", state.target_desc)),
    ];
    if !state.degraded.is_empty() {
        let k = state.synthesis.verdicts.len();
        let n = k + state.degraded.len();
        spans.push(Span::styled(
            format!(
                " · degraded: {} excluded ({k}/{n})",
                state.degraded.join(", ")
            ),
            Style::default().fg(theme.severity_warning),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);

    let touched = state.triage.values().filter(|e| e.touched).count();
    let total = state.triage.len();
    let (filled, rest) = crate::ui::format::progress_bar(touched, total, 10);
    let right = Line::from(vec![
        Span::styled(filled, Style::default().fg(theme.status_done)),
        Span::styled(rest, theme.dim_style()),
        Span::raw(format!(" {touched}/{total} triaged")),
    ]);
    f.render_widget(Paragraph::new(right).alignment(Alignment::Right), area);
}

fn draw_left(f: &mut Frame, area: Rect, state: &TriageState, theme: &Theme) {
    let visible = state.visible();
    let split = visible
        .iter()
        .position(|fnd| state.touched(&fnd.id))
        .unwrap_or(visible.len());
    let untouched = &visible[..split];
    let archived = &visible[split..];

    let selected_style = || {
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::REVERSED)
    };

    let mut lines: Vec<Line> = Vec::with_capacity(visible.len() + 2);
    for (i, finding) in untouched.iter().enumerate() {
        let is_selected = i == state.selected;
        let marker = if is_selected { "▸" } else { " " };
        let mut spans = vec![
            Span::raw(format!("{marker} ")),
            severity_dot(theme, finding.severity),
            Span::raw(format!(" {}", finding.title)),
        ];
        if finding.reporters.len() > 1 {
            spans.push(Span::styled(
                format!(" ×{}", finding.reporters.len()),
                theme.dim_style(),
            ));
        }
        if !finding.challengers.is_empty() {
            spans.push(Span::styled(
                format!(" ⚡{}", finding.challengers.len()),
                Style::default().fg(theme.confidence_disputed),
            ));
        }
        let mut line = Line::from(spans);
        if is_selected {
            line = line.style(selected_style());
        }
        lines.push(line);
    }

    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!("triaged — {}", archived.len()),
        theme.dim_style(),
    ));
    for (j, finding) in archived.iter().enumerate() {
        let idx = split + j;
        let is_selected = idx == state.selected;
        let status = state.triage.get(&finding.id).map(|e| e.status);
        let (marker, marker_style) = match status {
            Some(TriageStatus::Accepted) => ("✓", Style::default().fg(theme.status_done)),
            Some(TriageStatus::Dismissed) => ("✗", Style::default().fg(theme.status_failed)),
            _ => ("·", theme.dim_style()),
        };
        let spans = vec![
            Span::styled(marker, marker_style),
            Span::styled(format!(" {}", finding.title), theme.dim_style()),
        ];
        let mut line = Line::from(spans);
        if is_selected {
            line = line.style(selected_style());
        }
        lines.push(line);
    }

    if !state.filter.is_empty() {
        let hidden = state.synthesis.findings.len().saturating_sub(visible.len());
        if hidden > 0 {
            lines.push(Line::styled(
                format!("  {hidden} findings hidden by filter"),
                theme.dim_style(),
            ));
        }
    }

    let title = if !state.filter.is_empty() {
        Line::from(vec![
            Span::styled("inbox · filter ", theme.accent_style()),
            Span::styled(format!("\"{}\"", state.filter), theme.accent_style()),
            Span::styled(
                format!(" {}/{}", visible.len(), state.synthesis.findings.len()),
                theme.dim_style(),
            ),
            Span::styled(" · esc clears", theme.dim_style()),
        ])
    } else if untouched.is_empty() {
        Line::styled("inbox — clear", Style::default().fg(theme.status_done))
    } else {
        Line::styled(
            format!("inbox — {} to go", untouched.len()),
            theme.accent_style(),
        )
    };
    let block = Block::bordered()
        .title(title)
        .border_style(theme.dim_style());
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Rows `lines` occupy once ratatui's `Wrap { trim: false }` wraps them at
/// `width` columns. Spans are concatenated first — word-wrap flows across
/// span boundaries — and a naive `ceil(chars / width)` undercounts wrapped rows.
fn estimated_lines(lines: &[Line], width: u16) -> u16 {
    let width = width.max(1) as usize;
    let total: usize = lines
        .iter()
        .map(|line| {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            crate::ui::format::word_fit_rows(&text, width)
        })
        .sum();
    total.min(u16::MAX as usize) as u16
}

fn draw_right(f: &mut Frame, area: Rect, state: &TriageState, theme: &Theme) {
    let visible = state.visible();
    let mut title = if visible.is_empty() {
        "detail — 0 of 0".to_string()
    } else {
        format!("detail — {} of {}", state.selected + 1, visible.len())
    };
    let lines = visible
        .get(state.selected)
        .map(|finding| render_detail(finding, theme, &state.persona_colors))
        .unwrap_or_else(|| vec![Line::raw("no findings")]);

    let inner_width = area.width.saturating_sub(2);
    let inner_height = area.height.saturating_sub(2);
    if state.detail_scroll > 0 {
        title.push_str(" · ▲ more above");
    }
    let estimated = estimated_lines(&lines, inner_width);
    if estimated > inner_height.saturating_add(state.detail_scroll) {
        title.push_str(" · ▼ more below");
    }

    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .title(title)
                    .border_style(theme.dim_style()),
            )
            .wrap(Wrap { trim: false })
            .scroll((state.detail_scroll, 0)),
        area,
    );
}

fn draw_footer(f: &mut Frame, area: Rect, theme: &Theme) {
    let hints = theme.hint_spans(&[
        ("a", "accept"),
        ("d", "dismiss"),
        ("s", "skip"),
        ("u", "undo"),
        ("/", "filter"),
        ("?", "more"),
        ("f", "finalize"),
    ]);
    f.render_widget(Paragraph::new(Line::from(hints)), area);
}

/// `overlay::draw_help` pads its `key` column to a fixed 8 chars, which would
/// scatter multi-char keys like `J/K` far from their descriptions; folding
/// `key + " " + desc` into one string (empty key column) keeps the pairing.
const HELP_ENTRIES: &[(&str, &str)] = &[
    ("", "a accept"),
    ("", "d dismiss + note"),
    ("", "s/space skip"),
    ("", "n note"),
    ("", "u undo"),
    ("", "j/k move"),
    ("", "J/K scroll detail"),
    ("", "/ filter"),
    ("", "f finalize"),
    ("", "esc back"),
    ("", "q quit"),
];

fn draw_confirm_finalize(f: &mut Frame, area: Rect, state: &TriageState, theme: &Theme) {
    let (accepted, dismissed, untriaged) = state.counts();
    let warn = Style::default().fg(theme.severity_warning);

    let mut counts_line = vec![Span::raw(format!(
        "  ✓ {accepted} accepted · ✗ {dismissed} dismissed"
    ))];
    if untriaged > 0 {
        counts_line.push(Span::styled(
            format!(" · {untriaged} untriaged — they will be marked deferred"),
            warn,
        ));
    } else {
        counts_line.push(Span::styled(" · all findings triaged", theme.dim_style()));
    }

    let lines = vec![
        Line::raw(""),
        Line::from(counts_line),
        Line::raw(""),
        Line::styled(
            "  writes report.md and marks the run finalized — you can reopen triage later with r",
            theme.dim_style(),
        ),
        Line::raw(""),
        Line::from(theme.hint_spans(&[("f", "finalize"), ("esc", "keep triaging")])),
    ];

    // `centered`'s width doesn't depend on the height argument, so probe it
    // first and size the box from the WRAPPED row count — a naive
    // `lines.len() + 2` clips the hints row off the bottom at 80 cols.
    let probe_width = crate::ui::overlay::centered(area, 70, 1).width;
    let wrapped_rows = estimated_lines(&lines, probe_width.saturating_sub(2));
    let rect = crate::ui::overlay::centered(area, 70, wrapped_rows + 2);
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .title(Span::styled("finalize review?", warn))
        .border_style(warn);
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        rect,
    );
}

pub(crate) fn draw(f: &mut Frame, area: Rect, state: &TriageState, theme: &Theme) {
    let show_input = matches!(state.mode, Mode::FilterInput | Mode::NoteInput { .. });
    let layout = if show_input {
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area)
    } else {
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area)
    };
    let header_area = layout[0];
    let main_area = layout[1];
    let (input_area, footer_area) = if show_input {
        (Some(layout[2]), layout[3])
    } else {
        (None, layout[2])
    };

    draw_header(f, header_area, state, theme);

    let [left, right] =
        Layout::horizontal([Constraint::Percentage(46), Constraint::Percentage(54)])
            .areas(main_area);
    draw_left(f, left, state, theme);
    draw_right(f, right, state, theme);

    if let Some(input_area) = input_area {
        let line = match &state.mode {
            Mode::FilterInput => format!("filter: {}▏", state.filter),
            Mode::NoteInput {
                text,
                dismiss: true,
            } => format!("dismiss note: {text}▏"),
            Mode::NoteInput {
                text,
                dismiss: false,
            } => format!("note: {text}▏"),
            _ => String::new(),
        };
        f.render_widget(Paragraph::new(line).style(theme.accent_style()), input_area);
    }

    draw_footer(f, footer_area, theme);

    match state.mode {
        Mode::ConfirmFinalize => draw_confirm_finalize(f, area, state, theme),
        Mode::Help => crate::ui::overlay::draw_help(f, area, HELP_ENTRIES, theme),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::synthesis::{Attribution, Confidence};
    use crate::ui::app::render_to_text;
    use crate::ui::test_keys::{key, key_code};

    fn syn_with(titles: &[&str]) -> Synthesis {
        use crate::engine::model::*;
        let findings = titles
            .iter()
            .map(|t| RawFinding {
                severity: Severity::Warning,
                file: Some("a.rs".into()),
                line: Some(1),
                title: t.to_string(),
                detail: "detail".into(),
                fix: None,
            })
            .collect();
        let round1 = BTreeMap::from([(
            "prover".to_string(),
            Round1Review {
                persona: "prover".into(),
                verdict: Verdict::Conditional,
                summary: "s".into(),
                findings,
            },
        )]);
        crate::engine::synthesis::synthesize(&round1, &Default::default(), &[])
    }

    fn synthesis_n(n: usize) -> Synthesis {
        let findings = (0..n)
            .map(|i| Finding {
                id: format!("f{i}"),
                severity: Severity::Warning,
                title: format!("Finding {i}"),
                detail: "detail".into(),
                file: Some("a.rs".into()),
                line: Some(1),
                fix: None,
                reporters: vec!["prover".into()],
                validators: vec![],
                challengers: vec![],
                confidence: Confidence::Solo,
            })
            .collect();
        Synthesis {
            findings,
            verdicts: BTreeMap::from([(
                "prover".to_string(),
                crate::engine::model::Verdict::Conditional,
            )]),
            summaries: BTreeMap::new(),
            consensus_label: "SHIP".into(),
            consensus_score: 1.0,
            degraded: vec![],
        }
    }

    fn tstate(n: usize) -> TriageState {
        TriageState::new(
            "r".into(),
            "diff vs HEAD".into(),
            synthesis_n(n),
            Triage::new(),
        )
    }

    fn tstate_titled(titles: &[&str]) -> TriageState {
        TriageState::new(
            "r".into(),
            "diff vs HEAD".into(),
            syn_with(titles),
            Triage::new(),
        )
    }

    /// Detail long enough to overflow the 30-row test frame's detail pane.
    fn tstate_long_detail() -> TriageState {
        let finding = Finding {
            id: "f0".into(),
            severity: Severity::Warning,
            title: "Long finding".into(),
            detail: "word ".repeat(2000),
            file: Some("a.rs".into()),
            line: Some(1),
            fix: None,
            reporters: vec!["prover".into()],
            validators: vec![],
            challengers: vec![],
            confidence: Confidence::Solo,
        };
        let synthesis = Synthesis {
            findings: vec![finding],
            verdicts: BTreeMap::from([(
                "prover".to_string(),
                crate::engine::model::Verdict::Conditional,
            )]),
            summaries: BTreeMap::new(),
            consensus_label: "SHIP".into(),
            consensus_score: 1.0,
            degraded: vec![],
        };
        TriageState::new("r".into(), "diff vs HEAD".into(), synthesis, Triage::new())
    }

    fn tstate_disputed() -> TriageState {
        let finding = Finding {
            id: "f0".into(),
            severity: Severity::Critical,
            title: "Risky change".into(),
            detail: "detail".into(),
            file: Some("a.rs".into()),
            line: Some(1),
            fix: None,
            reporters: vec!["prover".into()],
            validators: vec![],
            challengers: vec![Attribution {
                persona: "breaker".into(),
                reason: "not risky".into(),
            }],
            confidence: Confidence::Disputed,
        };
        let synthesis = Synthesis {
            findings: vec![finding],
            verdicts: BTreeMap::from([(
                "prover".to_string(),
                crate::engine::model::Verdict::Conditional,
            )]),
            summaries: BTreeMap::new(),
            consensus_label: "HOLD".into(),
            consensus_score: 0.0,
            degraded: vec![],
        };
        TriageState::new("r".into(), "diff vs HEAD".into(), synthesis, Triage::new())
    }

    fn render(state: &TriageState) -> String {
        render_to_text(160, 30, |f| draw(f, f.area(), state, &Theme::default()))
    }

    #[test]
    fn verbs_touch_advance_and_sink_to_archive() {
        let mut s = tstate(3);
        s.handle_key(key('a'));
        assert!(s.triage.values().filter(|e| e.touched).count() == 1);
        assert_eq!(
            s.selected, 0,
            "archive sank the accepted one; selection stays on the inbox frontier"
        );
        let ids: Vec<&str> = s.visible().iter().map(|f| f.id.as_str()).collect();
        assert_eq!(
            ids.last().copied(),
            Some("f0"),
            "accepted finding moved to the archive tail"
        );
        let text = render(&s);
        assert!(text.contains("inbox — 2 to go"), "{text}");
        assert!(text.contains("triaged — 1"), "{text}");
        assert!(text.contains("1/3 triaged"), "{text}");
    }

    #[test]
    fn accept_preserves_existing_note_and_n_edits_without_status_change() {
        let mut s = tstate(1);
        s.handle_key(key('d'));
        for c in "false positive".chars() {
            s.handle_key(key(c));
        }
        s.handle_key(key_code(KeyCode::Enter));
        s.handle_key(key('k')); // back to it (it's archived; selection clamps)
        s.handle_key(key('a'));
        let e = s.triage.get("f0").unwrap();
        assert_eq!(e.status, TriageStatus::Accepted);
        assert_eq!(
            e.note.as_deref(),
            Some("false positive"),
            "status flip must not wipe the note"
        );
        s.handle_key(key('n'));
        for c in " — but fix only the X part".chars() {
            s.handle_key(key(c));
        }
        s.handle_key(key_code(KeyCode::Enter));
        let e = s.triage.get("f0").unwrap();
        assert_eq!(e.status, TriageStatus::Accepted);
        assert!(e.note.as_deref().unwrap().ends_with("X part"));
    }

    #[test]
    fn undo_restores_previous_entry() {
        let mut s = tstate(2);
        s.handle_key(key('a'));
        s.handle_key(key('u'));
        let e = s.triage.get("f0").unwrap();
        assert_eq!(
            (e.status, e.touched),
            (TriageStatus::Deferred, false),
            "undo returns it to the inbox"
        );
    }

    #[test]
    fn skip_counts_as_triaged_but_stays_deferred() {
        let mut s = tstate(2);
        s.handle_key(key('s'));
        let e = s.triage.get("f0").unwrap();
        assert_eq!((e.status, e.touched), (TriageStatus::Deferred, true));
    }

    #[test]
    fn detail_always_shows_challenger_evidence_and_scrolls() {
        let s = tstate_disputed();
        let text = render(&s);
        assert!(
            text.contains("challenged by"),
            "no hidden adjudication evidence: {text}"
        );
        assert!(!text.contains("🔴"), "emoji replaced by themed dot: {text}");
    }

    #[test]
    fn resume_selects_first_untouched() {
        let mut triage = Triage::new();
        triage.insert(
            "f0".into(),
            TriageEntry {
                status: TriageStatus::Accepted,
                note: None,
                touched: true,
            },
        );
        let s = TriageState::new("r".into(), "diff vs HEAD".into(), synthesis_n(3), triage);
        assert_eq!(s.visible()[s.selected].id, "f1");
    }

    #[test]
    fn header_shows_degraded_banner() {
        let mut s = tstate(1);
        s.degraded = vec!["breaker".into()];
        let text = render(&s);
        assert!(text.contains("degraded: breaker excluded"), "{text}");
    }

    #[test]
    fn q_quits_and_esc_goes_home() {
        let mut s = tstate(1);
        assert!(matches!(s.handle_key(key('q')), Some(Transition::Quit)));
        assert!(matches!(
            s.handle_key(key_code(KeyCode::Esc)),
            Some(Transition::ToHome)
        ));
    }

    #[test]
    fn dirty_flag_set_on_accept_and_undo() {
        let mut s = tstate(1);
        assert!(!s.dirty);
        s.handle_key(key('a'));
        assert!(s.dirty, "accept must mark triage dirty for eager save");
        s.dirty = false;
        s.handle_key(key('u'));
        assert!(s.dirty, "undo must also mark triage dirty for eager save");
    }

    #[test]
    fn filter_narrows_visible() {
        let mut state = TriageState::new(
            "r".into(),
            "diff vs HEAD".into(),
            syn_with(&["Alpha issue", "Beta issue"]),
            Triage::new(),
        );
        state.handle_key(key('/'));
        for c in "beta".chars() {
            state.handle_key(key(c));
        }
        state.handle_key(key_code(KeyCode::Enter));
        assert_eq!(state.visible().len(), 1);
        assert_eq!(state.visible()[0].title, "Beta issue");
    }

    #[test]
    fn finalize_emits_transition() {
        let mut state = TriageState::new(
            "r".into(),
            "diff vs HEAD".into(),
            syn_with(&["One"]),
            Triage::new(),
        );
        assert!(
            state.handle_key(key('f')).is_none(),
            "first f only opens the confirmation"
        );
        assert!(matches!(state.mode, Mode::ConfirmFinalize));
        assert!(matches!(
            state.handle_key(key('f')),
            Some(Transition::Finalize)
        ));
    }

    #[test]
    fn detail_colors_severity_and_confidence() {
        let state = TriageState::new(
            "r".into(),
            "diff vs HEAD".into(),
            syn_with(&["One"]),
            Triage::new(),
        );
        let visible = state.visible();
        let lines = render_detail(visible[0], &Theme::default(), &BTreeMap::new());
        let sev_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("Warning")))
            .expect("severity line");
        // Line::from(spans) carries per-span styles, not a line-level style;
        // find the span itself to check its color.
        let sev_span = sev_line
            .spans
            .iter()
            .find(|s| s.content.contains("Warning"))
            .expect("severity span");
        assert_eq!(
            sev_span.style.fg,
            Some(ratatui::style::Color::Yellow),
            "warning severity colored yellow"
        );
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("One"), "title preserved");
        assert!(text.contains("found by"), "attribution preserved");
    }

    #[test]
    fn sticky_filter_is_visible_with_counts_and_hidden_line() {
        let mut s = tstate_titled(&["walk_dir cycles", "walk depth cap", "other thing"]);
        s.handle_key(key('/'));
        for c in "walk".chars() {
            s.handle_key(key(c));
        }
        s.handle_key(key_code(KeyCode::Enter));
        let text = render(&s);
        assert!(text.contains("filter \"walk\" 2/3"), "{text}");
        assert!(text.contains("esc clears"), "{text}");
        assert!(text.contains("1 findings hidden by filter"), "{text}");
    }

    #[test]
    fn esc_clears_filter_before_leaving() {
        let mut s = tstate_titled(&["walk_dir cycles", "other"]);
        s.handle_key(key('/'));
        s.handle_key(key('w'));
        s.handle_key(key_code(KeyCode::Enter));
        assert!(
            s.handle_key(key_code(KeyCode::Esc)).is_none(),
            "first esc clears the filter"
        );
        assert!(s.filter.is_empty());
        assert!(
            matches!(
                s.handle_key(key_code(KeyCode::Esc)),
                Some(Transition::ToHome)
            ),
            "second esc leaves"
        );
    }

    #[test]
    fn filter_input_esc_clears_enter_keeps() {
        let mut s = tstate_titled(&["a", "b"]);
        s.handle_key(key('/'));
        s.handle_key(key('a'));
        s.handle_key(key_code(KeyCode::Esc));
        assert!(s.filter.is_empty(), "esc in the input abandons the filter");
    }

    #[test]
    fn finalize_confirms_with_untriaged_count() {
        let mut s = tstate(5);
        s.handle_key(key('a'));
        s.handle_key(key('d'));
        s.handle_key(key_code(KeyCode::Enter)); // dismiss, empty note
        assert!(
            s.handle_key(key('f')).is_none(),
            "f opens the confirmation, not Finalize"
        );
        let text = render(&s);
        assert!(text.contains("finalize review?"), "{text}");
        assert!(text.contains("✓ 1 accepted · ✗ 1 dismissed"), "{text}");
        assert!(
            text.contains("3 untriaged — they will be marked deferred"),
            "{text}"
        );
        assert!(text.contains("reopen triage later with r"), "{text}");
        assert!(
            s.handle_key(key_code(KeyCode::Esc)).is_none(),
            "esc keeps triaging"
        );
        s.handle_key(key('f'));
        assert!(
            matches!(s.handle_key(key('f')), Some(Transition::Finalize)),
            "second f finalizes"
        );
    }

    #[test]
    fn help_overlay_lists_hidden_keys() {
        let mut s = tstate(1);
        s.handle_key(key('?'));
        let text = render(&s);
        assert!(text.contains("J/K scroll detail"), "{text}");
        assert!(text.contains("u undo"), "{text}");
        assert!(s.handle_key(key('x')).is_none(), "any key closes");
        assert!(matches!(s.mode, Mode::List));
    }

    #[test]
    fn detail_scroll_markers_reflect_position() {
        let s = tstate_long_detail();
        let text = render(&s);
        assert!(text.contains("▼ more below"), "{text}");
        let mut s = s;
        s.handle_key(key('J'));
        let text = render(&s);
        assert!(text.contains("▲ more above"), "{text}");
    }

    #[test]
    fn confirm_finalize_hints_survive_wrap_at_80_cols() {
        let mut s = tstate(5);
        s.handle_key(key('a'));
        s.handle_key(key('d'));
        s.handle_key(key_code(KeyCode::Enter));
        s.handle_key(key('f'));
        let text = render_to_text(80, 24, |f| draw(f, f.area(), &s, &Theme::default()));
        assert!(text.contains("f finalize · esc keep triaging"), "{text}");
    }
}
