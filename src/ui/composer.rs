use crate::config::Config;
use crate::engine::model::TargetKind;
use crate::engine::run::RunSpec;
use crate::engine::target::{detect_targets, DetectedTarget, Target};
use crate::ui::app::Transition;
use crate::ui::format::{truncate_end, truncate_path_start};
use crate::ui::theme::Theme;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthStr;

pub(crate) const SKIP_DIRS: [&str; 10] = [
    ".git",
    "node_modules",
    "target",
    ".reviewal",
    ".claude",
    ".venv",
    "venv",
    "vendor",
    "dist",
    "build",
];

/// Aliases (unlike pinned model ids) cannot go stale, so they are safe to
/// hardcode.
const MODEL_ALIASES: [(&str, &str); 4] = [
    ("fable", "latest Fable — most capable"),
    ("opus", "latest Opus"),
    ("sonnet", "latest Sonnet — balanced"),
    ("haiku", "latest Haiku — fastest"),
];

/// One past the last alias row: row 0 is "default", rows
/// `1..=MODEL_ALIASES.len()` are the aliases.
fn model_custom_index() -> usize {
    MODEL_ALIASES.len() + 1
}

pub(crate) fn collect_spec_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_dir(root, root, &mut out);
    out.sort();
    out
}

fn walk_dir(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            let name = entry.file_name();
            if SKIP_DIRS.contains(&name.to_string_lossy().as_ref()) {
                continue;
            }
            walk_dir(root, &path, out);
        } else if file_type.is_file() && path.extension().is_some_and(|e| e == "md") {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_path_buf());
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    Target,
    Reviewers,
    Model,
    CrossReview,
    Start,
}

const FIELD_ORDER: [Field; 5] = [
    Field::Target,
    Field::Reviewers,
    Field::Model,
    Field::CrossReview,
    Field::Start,
];

const VALUE_FIELDS: [Field; 4] = [
    Field::Target,
    Field::Reviewers,
    Field::Model,
    Field::CrossReview,
];

fn field_index(f: Field) -> usize {
    FIELD_ORDER
        .iter()
        .position(|x| *x == f)
        .expect("FIELD_ORDER covers every Field variant")
}

pub(crate) struct PickerState {
    pub filter: String,
    pub cursor: usize,
    pub selected: BTreeSet<PathBuf>,
}

impl PickerState {
    fn new() -> Self {
        PickerState {
            filter: String::new(),
            cursor: 0,
            selected: BTreeSet::new(),
        }
    }
}

use crate::ui::personas::{invalid_tag, provenance_tag, PersonaChoice, PersonaManager};

pub(crate) struct ComposerState {
    root: PathBuf,
    timeout_secs: u64,
    claude_bin: String,
    config_model: Option<String>,
    pub targets: Vec<DetectedTarget>,
    /// `Some(i)` = `targets[i]`; `None` = spec files.
    pub target_choice: Option<usize>,
    pub spec_files: Vec<PathBuf>,
    pub chosen_specs: Vec<PathBuf>,
    /// Cursor inside the target editor: `0..targets.len()` are detected
    /// diff targets, `targets.len()` is the spec-files row.
    pub target_cursor: usize,
    /// 0 = default (config/CLI), `1..=MODEL_ALIASES.len()` = aliases,
    /// `model_custom_index()` = custom.
    pub model_idx: usize,
    pub model_custom: String,
    /// `true` while the custom-model row is capturing free-text input.
    pub model_input: bool,
    /// `true` while the focused field's inline editor is open and capturing
    /// j/k/space; fields never expand on focus alone.
    pub editing: bool,
    pub cross_review: bool,
    pub field: Field,
    pub picker: Option<PickerState>,
    /// Persona rows + management verbs, shared with the home personas tab;
    /// carries the persona dirs, pager, staged editor request, scope
    /// prompt, notice, and armed-delete state.
    pub mgr: PersonaManager,
    pub error: Option<String>,
}

impl ComposerState {
    pub(crate) fn new(
        root: &Path,
        config: &Config,
        seed_target: Option<Target>,
        open_spec_picker: bool,
    ) -> Self {
        let targets = detect_targets(root);
        let spec_files = collect_spec_files(root);
        let (target_choice, chosen_specs) = if open_spec_picker {
            (None, Vec::new())
        } else {
            match &seed_target {
                Some(Target::SpecFiles(files)) => (None, files.clone()),
                Some(t) => {
                    let matched = targets.iter().position(|dt| &dt.target == t);
                    (
                        matched.or_else(|| (!targets.is_empty()).then_some(0)),
                        Vec::new(),
                    )
                }
                None => ((!targets.is_empty()).then_some(0), Vec::new()),
            }
        };
        let kind = match target_choice {
            Some(_) => TargetKind::Code,
            None => TargetKind::Spec,
        };
        let (model_idx, model_custom) = match config.model.as_deref() {
            None => (0, String::new()),
            Some(m) => match MODEL_ALIASES.iter().position(|(a, _)| *a == m) {
                Some(i) => (i + 1, String::new()),
                None => (model_custom_index(), m.to_string()),
            },
        };
        ComposerState {
            root: root.to_path_buf(),
            timeout_secs: config.timeout_secs,
            claude_bin: config.claude_bin.clone(),
            config_model: config.model.clone(),
            targets,
            target_choice,
            spec_files,
            chosen_specs,
            target_cursor: 0,
            model_idx,
            model_custom,
            model_input: false,
            editing: false,
            cross_review: false,
            field: Field::Target,
            picker: if open_spec_picker {
                Some(PickerState::new())
            } else {
                None
            },
            mgr: PersonaManager::new(root, config, Some(kind)),
            error: None,
        }
    }

    fn rebuild_personas_for(&mut self, kind: TargetKind) {
        self.mgr.rebuild_for(kind);
    }

    /// Total checklist rows: valid personas, then invalid files.
    pub(crate) fn row_count(&self) -> usize {
        self.mgr.row_count()
    }

    pub(crate) fn current_kind(&self) -> TargetKind {
        match self.target_choice {
            Some(_) => TargetKind::Code,
            None => TargetKind::Spec,
        }
    }

    /// `None` means "let the claude CLI use its own default".
    pub(crate) fn chosen_model(&self) -> Option<String> {
        match self.model_idx {
            0 => self.config_model.clone(),
            i if i <= MODEL_ALIASES.len() => Some(MODEL_ALIASES[i - 1].0.to_string()),
            _ => {
                let custom = self.model_custom.trim();
                if custom.is_empty() {
                    self.config_model.clone()
                } else {
                    Some(custom.to_string())
                }
            }
        }
    }

    /// Split so the border render can dim the prose but keep the call count
    /// at normal weight.
    pub(crate) fn cost_parts(&self) -> (String, String) {
        let n = self.mgr.personas.iter().filter(|c| c.enabled).count();
        let r = 1 + usize::from(self.cross_review);
        let s = if r == 1 { "" } else { "s" };
        (
            format!("{n} reviewers \u{d7} {r} round{s} = "),
            format!("{} model calls", n * r),
        )
    }

    fn build_spec(&self) -> RunSpec {
        let target = match self.target_choice {
            Some(i) => self.targets[i].target.clone(),
            None => Target::SpecFiles(self.chosen_specs.clone()),
        };
        let personas = self
            .mgr
            .personas
            .iter()
            .filter(|c| c.enabled)
            .map(|c| c.persona.clone())
            .collect();
        RunSpec {
            root: self.root.clone(),
            target,
            personas,
            model: self.chosen_model(),
            cross_review: self.cross_review,
            timeout_secs: self.timeout_secs,
            claude_bin: self.claude_bin.clone(),
            now_utc: crate::ui::app::now_rfc3339(),
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Option<Transition> {
        self.mgr.notice = None;
        if self.picker.is_some() {
            return self.handle_picker_key(key);
        }
        if self.mgr.route_key(key) {
            return None;
        }
        if self.model_input {
            return self.handle_model_input_key(key);
        }
        if self.editing {
            return self.handle_edit_key(key);
        }
        self.handle_fields_key(key)
    }

    fn handle_fields_key(&mut self, key: KeyEvent) -> Option<Transition> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_field(1);
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_field(-1);
                None
            }
            KeyCode::Char(' ') | KeyCode::Enter => self.activate_field(),
            KeyCode::Char('t') => {
                self.cycle_target();
                None
            }
            KeyCode::Esc => Some(Transition::ToHome),
            _ => None,
        }
    }

    fn move_field(&mut self, delta: i32) {
        let i = (field_index(self.field) as i32 + delta).clamp(0, FIELD_ORDER.len() as i32 - 1);
        self.field = FIELD_ORDER[i as usize];
    }

    fn activate_field(&mut self) -> Option<Transition> {
        self.error = None;
        match self.field {
            Field::Target => {
                self.target_cursor = match self.target_choice {
                    Some(i) => i,
                    None => self.targets.len(),
                };
                self.editing = true;
                None
            }
            Field::Reviewers => {
                self.editing = true;
                None
            }
            Field::Model => {
                self.editing = true;
                None
            }
            Field::CrossReview => {
                self.cross_review = !self.cross_review;
                None
            }
            Field::Start => self.try_start(),
        }
    }

    /// j/k clamp at the list's edges (they never fall through to the next
    /// field); Enter and Esc both close the editor — toggles applied inside
    /// it stick either way.
    fn handle_edit_key(&mut self, key: KeyEvent) -> Option<Transition> {
        match key.code {
            KeyCode::Char('v' | 'e' | 'n' | 'd' | 'x') if self.field == Field::Reviewers => {
                self.mgr.handle_verb_key(key);
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_edit_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_edit_cursor(-1),
            KeyCode::Char(' ') => match self.field {
                Field::Reviewers => {
                    // Rows `personas.len()..row_count()` are invalid-file
                    // rows: space is a no-op there, not a toggle.
                    if let Some(c) = self.mgr.personas.get_mut(self.mgr.cursor) {
                        c.enabled = !c.enabled;
                    }
                }
                Field::Target => self.commit_target_choice(),
                _ => {
                    if self.model_idx == model_custom_index() {
                        self.model_input = true;
                    }
                }
            },
            KeyCode::Enter => match self.field {
                Field::Model if self.model_idx == model_custom_index() => {
                    self.model_input = true;
                }
                Field::Target => self.commit_target_choice(),
                _ => self.editing = false,
            },
            KeyCode::Esc => self.editing = false,
            _ => {}
        }
        None
    }

    fn move_edit_cursor(&mut self, delta: i32) {
        match self.field {
            Field::Reviewers => {
                let last = self.row_count().saturating_sub(1) as i32;
                self.mgr.cursor = (self.mgr.cursor as i32 + delta).clamp(0, last) as usize;
            }
            Field::Target => {
                let last = self.targets.len() as i32; // the spec-files row
                self.target_cursor = (self.target_cursor as i32 + delta).clamp(0, last) as usize;
            }
            _ => {
                self.model_idx =
                    (self.model_idx as i32 + delta).clamp(0, model_custom_index() as i32) as usize;
            }
        }
    }

    fn commit_target_choice(&mut self) {
        self.editing = false;
        self.error = None;
        if self.target_cursor < self.targets.len() {
            self.target_choice = Some(self.target_cursor);
            self.rebuild_personas_for(TargetKind::Code);
        } else {
            self.target_choice = None;
            self.rebuild_personas_for(TargetKind::Spec);
            if self.chosen_specs.is_empty() {
                self.open_picker();
            }
        }
    }

    /// `Some(0) -> ... -> Some(last) -> None (specs) -> Some(0)`.
    fn cycle_target(&mut self) {
        let n = self.targets.len();
        self.target_choice = if n == 0 {
            None
        } else {
            match self.target_choice {
                Some(i) if i + 1 < n => Some(i + 1),
                Some(_) => None,
                None => Some(0),
            }
        };
        self.rebuild_personas_for(self.current_kind());
        self.error = None;
        if self.target_choice.is_none() && self.chosen_specs.is_empty() {
            self.open_picker();
        }
    }

    fn open_picker(&mut self) {
        self.picker = Some(PickerState::new());
    }

    fn try_start(&mut self) -> Option<Transition> {
        if self.target_choice.is_none() && self.chosen_specs.is_empty() {
            self.error = Some("select at least one spec file \u{2014} edit target".into());
            return None;
        }
        if self.mgr.personas.iter().filter(|c| c.enabled).count() < 2 {
            self.error = Some("need at least 2 reviewers".into());
            return None;
        }
        self.error = None;
        Some(Transition::StartRun(self.build_spec()))
    }

    fn handle_model_input_key(&mut self, key: KeyEvent) -> Option<Transition> {
        match key.code {
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.model_custom.push(c);
            }
            KeyCode::Backspace => {
                self.model_custom.pop();
            }
            KeyCode::Enter => {
                // Accept commits the custom model and closes the whole model
                // editor: the cursor is still on the custom row, so staying
                // in the list would make the next Enter reopen the input
                // instead of ever closing the editor.
                self.model_input = false;
                self.editing = false;
                self.error = None;
            }
            KeyCode::Esc => self.model_input = false,
            _ => {}
        }
        None
    }

    fn filtered_spec_files(&self) -> Vec<&PathBuf> {
        let needle = self
            .picker
            .as_ref()
            .map(|p| p.filter.to_lowercase())
            .unwrap_or_default();
        self.spec_files
            .iter()
            .filter(|p| needle.is_empty() || p.to_string_lossy().to_lowercase().contains(&needle))
            .collect()
    }

    fn clamp_picker_cursor(&mut self) {
        let len = self.filtered_spec_files().len();
        if let Some(picker) = self.picker.as_mut() {
            picker.cursor = picker.cursor.min(len.saturating_sub(1));
        }
    }

    fn toggle_hovered_spec(&mut self) {
        let cursor = self.picker.as_ref().map_or(0, |p| p.cursor);
        let hovered = self.filtered_spec_files().get(cursor).map(|p| (*p).clone());
        if let (Some(path), Some(picker)) = (hovered, self.picker.as_mut()) {
            if !picker.selected.insert(path.clone()) {
                picker.selected.remove(&path);
            }
        }
    }

    /// If nothing was toggled, Enter selects the hovered row first, so a
    /// single keypress on a freshly-opened picker commits its match.
    fn commit_picker(&mut self) {
        let nothing_toggled = self.picker.as_ref().is_some_and(|p| p.selected.is_empty());
        if nothing_toggled {
            self.toggle_hovered_spec();
        }
        if let Some(picker) = self.picker.take() {
            self.chosen_specs = picker.selected.into_iter().collect();
            self.target_choice = None;
            self.error = None;
        }
    }

    fn cancel_picker(&mut self) {
        self.picker = None;
        if self.chosen_specs.is_empty() && !self.targets.is_empty() {
            self.target_choice = Some(0);
            self.rebuild_personas_for(TargetKind::Code);
        }
    }

    fn handle_picker_key(&mut self, key: KeyEvent) -> Option<Transition> {
        match key.code {
            KeyCode::Enter => {
                self.commit_picker();
                None
            }
            KeyCode::Esc => {
                self.cancel_picker();
                None
            }
            KeyCode::Up => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.cursor = picker.cursor.saturating_sub(1);
                }
                None
            }
            KeyCode::Down => {
                let len = self.filtered_spec_files().len();
                if let Some(picker) = self.picker.as_mut() {
                    if len > 0 {
                        picker.cursor = (picker.cursor + 1).min(len - 1);
                    }
                }
                None
            }
            KeyCode::Char(' ') => {
                self.toggle_hovered_spec();
                None
            }
            KeyCode::Backspace => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.filter.pop();
                }
                self.clamp_picker_cursor();
                None
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(picker) = self.picker.as_mut() {
                    picker.filter.push(c);
                }
                self.clamp_picker_cursor();
                None
            }
            _ => None,
        }
    }
}

/// Column where field values and descriptions start: the 16-column label
/// cell plus one unstyled gap column, so the focused label's selection
/// tint never abuts the value text.
const VALUE_COL: usize = 17;

fn label_cell(label: &str, focused: bool, theme: &Theme) -> Span<'static> {
    if focused {
        Span::styled(
            format!("\u{25b8} {label:<14}"),
            Style::default()
                .fg(theme.accent)
                .patch(theme.selection_style()),
        )
    } else {
        Span::raw(format!("  {label:<14}"))
    }
}

fn label_for(field: Field) -> &'static str {
    match field {
        Field::Target => "target",
        Field::Reviewers => "reviewers",
        Field::Model => "model",
        Field::CrossReview => "cross-review",
        Field::Start => "start review",
    }
}

fn target_expansion(state: &ComposerState, theme: &Theme) -> Vec<Line<'static>> {
    let mut labels: Vec<String> = state
        .targets
        .iter()
        .map(|t| {
            format!(
                "{} \u{2014} {} files \u{b7} +{} \u{2212}{}",
                t.label,
                t.files.len(),
                t.additions,
                t.deletions
            )
        })
        .collect();
    let k = state.chosen_specs.len();
    labels.push(if k == 0 {
        "spec files \u{2014} pick\u{2026}".to_string()
    } else {
        format!("spec files \u{2014} {k} selected")
    });
    labels
        .into_iter()
        .enumerate()
        .map(|(i, label)| {
            let style = if state.target_cursor == i {
                Style::default()
                    .fg(theme.accent)
                    .patch(theme.selection_style())
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::raw(" ".repeat(VALUE_COL)),
                Span::styled(label, style),
            ])
        })
        .collect()
}

fn target_value(state: &ComposerState) -> String {
    match state.target_choice {
        Some(i) => state.targets[i].label.clone(),
        None => format!("spec files \u{2014} {} selected", state.chosen_specs.len()),
    }
}

fn target_description(state: &ComposerState, budget: usize) -> String {
    match state.target_choice {
        Some(i) => {
            let t = &state.targets[i];
            format!(
                "{} files \u{b7} +{} \u{2212}{}",
                t.files.len(),
                t.additions,
                t.deletions
            )
        }
        None if state.chosen_specs.is_empty() => "none selected \u{2014} space to pick".to_string(),
        None => spec_paths_description(&state.chosen_specs, budget),
    }
}

fn spec_paths_description(paths: &[PathBuf], budget: usize) -> String {
    let k = paths.len();
    let k_shown = k.min(2);
    let sep = " \u{b7} ";
    let sep_cols = if k_shown == 2 { sep.width() } else { 0 };
    let suffix = if k > k_shown {
        format!(" +{} more", k - k_shown)
    } else {
        String::new()
    };
    // Filenames only: the directory chain is noise here (the picker shows
    // full paths), and the filename is the distinguishing part.
    let name_of = |p: &PathBuf| -> String {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.display().to_string())
    };
    let share = budget.saturating_sub(sep_cols + suffix.width()) / k_shown.max(1);
    if share < 8 && k > 1 {
        let suffix = format!(" +{} more", k - 1);
        let avail = budget.saturating_sub(suffix.width());
        let first = truncate_path_start(&name_of(&paths[0]), avail);
        return format!("{first}{suffix}");
    }
    let shown: Vec<String> = paths
        .iter()
        .take(k_shown)
        .map(|p| truncate_path_start(&name_of(p), share))
        .collect();
    format!("{}{suffix}", shown.join(sep))
}

/// Enabled persona names as colored spans. Overflow degrades by WHOLE
/// names plus a dim ` +N` count — never a mid-name cut (value lines must
/// not clip at the border).
fn reviewer_value_spans(state: &ComposerState, theme: &Theme, budget: usize) -> Vec<Span<'static>> {
    if budget == 0 {
        return Vec::new();
    }
    let enabled: Vec<&PersonaChoice> = state.mgr.personas.iter().filter(|c| c.enabled).collect();
    let width_for = |m: usize| -> usize {
        let names: usize = enabled[..m]
            .iter()
            .map(|c| c.persona.name.as_str().width())
            .sum();
        let seps = 3 * m.saturating_sub(1); // " · " between names
        let suffix = if m < enabled.len() {
            format!(" +{}", enabled.len() - m).width()
        } else {
            0
        };
        names + seps + suffix
    };
    let mut m = enabled.len();
    while m > 0 && width_for(m) > budget {
        m -= 1;
    }
    let mut spans = Vec::new();
    for (i, c) in enabled[..m].iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" \u{b7} ", theme.dim_style()));
        }
        let color = theme.persona_color(&c.persona.name, c.persona.color.as_deref());
        spans.push(Span::styled(
            c.persona.name.clone(),
            Style::default().fg(color),
        ));
    }
    if m < enabled.len() {
        spans.push(Span::styled(
            format!(" +{}", enabled.len() - m),
            theme.dim_style(),
        ));
    }
    spans
}

fn reviewers_description(state: &ComposerState) -> String {
    let enabled = state.mgr.personas.iter().filter(|c| c.enabled).count();
    format!("{enabled} of {} personas on", state.mgr.personas.len())
}

fn model_value(state: &ComposerState) -> String {
    state
        .chosen_model()
        .unwrap_or_else(|| "default".to_string())
}

fn model_description(state: &ComposerState) -> String {
    match state.model_idx {
        0 if state.config_model.is_some() => "from config".to_string(),
        0 => "use config/CLI default".to_string(),
        i if i <= MODEL_ALIASES.len() => MODEL_ALIASES[i - 1].1.to_string(),
        _ => "custom model id".to_string(),
    }
}

fn cross_review_description(state: &ComposerState) -> String {
    if state.cross_review {
        "adds a validation round \u{2014} 2\u{d7} calls".to_string()
    } else {
        "reviewers work blind".to_string()
    }
}

fn value_line(state: &ComposerState, field: Field, theme: &Theme, budget: usize) -> Line<'static> {
    let focused = state.field == field;
    let mut spans = vec![label_cell(label_for(field), focused, theme), Span::raw(" ")];
    match field {
        Field::Target => spans.push(Span::raw(truncate_end(&target_value(state), budget))),
        Field::Reviewers => spans.extend(reviewer_value_spans(state, theme, budget)),
        Field::Model => spans.push(Span::raw(truncate_end(&model_value(state), budget))),
        Field::CrossReview => spans.push(Span::raw(if state.cross_review { "on" } else { "off" })),
        // The start row renders via `start_row`, never through here.
        Field::Start => {}
    }
    Line::from(spans)
}

fn start_row(state: &ComposerState, theme: &Theme) -> Line<'static> {
    let focused = state.field == Field::Start;
    let (marker, style) = if focused {
        (
            "\u{25b8}",
            Style::default()
                .fg(theme.accent)
                .patch(theme.selection_style()),
        )
    } else {
        (" ", theme.accent_style())
    };
    Line::from(Span::styled(format!("{marker} start review"), style))
}

fn description_line(
    state: &ComposerState,
    field: Field,
    theme: &Theme,
    budget: usize,
) -> Line<'static> {
    let text = match field {
        Field::Target => target_description(state, budget),
        Field::Reviewers => reviewers_description(state),
        Field::Model => model_description(state),
        Field::CrossReview => cross_review_description(state),
        Field::Start => String::new(), // renders via `start_row`, never here
    };
    Line::from(vec![
        Span::raw(" ".repeat(VALUE_COL)),
        Span::styled(truncate_end(&text, budget), theme.dim_style()),
    ])
}

fn reviewer_expansion(state: &ComposerState, theme: &Theme, budget: usize) -> Vec<Line<'static>> {
    // marker(3) + space + name column(12, truncate-enforced — `{:<12}` only
    // pads a short name, it never shrinks a long one) + "— "(2): everything
    // before the lens.
    const PREFIX: usize = 3 + 1 + 12 + 2;
    const TAG_GUTTER: usize = 2;
    let mut lines: Vec<Line<'static>> = state
        .mgr
        .personas
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let marker = if c.enabled { "[x]" } else { "[ ]" };
            let name = truncate_end(&c.persona.name, 12);
            let tag = provenance_tag(&state.mgr, &c.persona);
            let lens_budget = budget.saturating_sub(PREFIX + tag.width() + TAG_GUTTER);
            let lens = truncate_end(&c.persona.lens, lens_budget);
            // Right-align the tag; the lens is what truncates, so the tag
            // always survives.
            let pad = budget
                .saturating_sub(PREFIX + lens.width() + tag.width())
                .max(TAG_GUTTER);
            let mut spans = vec![Span::raw(" ".repeat(VALUE_COL))];
            if state.mgr.cursor == i {
                spans.push(Span::styled(
                    format!("{marker} {name:<12}\u{2014} {lens}{}{tag}", " ".repeat(pad)),
                    Style::default()
                        .fg(theme.accent)
                        .patch(theme.selection_style()),
                ));
            } else {
                let marker_style = if c.enabled {
                    Style::default().fg(theme.status_done)
                } else {
                    theme.dim_style()
                };
                let color = theme.persona_color(&c.persona.name, c.persona.color.as_deref());
                spans.extend([
                    Span::styled(marker, marker_style),
                    Span::raw(" "),
                    Span::styled(format!("{name:<12}"), Style::default().fg(color)),
                    Span::styled("\u{2014} ", theme.dim_style()),
                    Span::styled(lens, theme.dim_style()),
                    Span::styled(format!("{}{tag}", " ".repeat(pad)), theme.dim_style()),
                ]);
            }
            Line::from(spans)
        })
        .collect();
    for (j, row) in state.mgr.invalid.iter().enumerate() {
        let i = state.mgr.personas.len() + j;
        let stem = row
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let stem = truncate_end(&stem, 12);
        let tag = invalid_tag(&state.mgr, &row.path);
        let lens_budget = budget.saturating_sub(PREFIX + tag.width() + TAG_GUTTER);
        let err = truncate_end(&row.error, lens_budget);
        let pad = budget
            .saturating_sub(PREFIX + err.width() + tag.width())
            .max(TAG_GUTTER);
        let body = format!(" !  {stem:<12}\u{2014} {err}{}{tag}", " ".repeat(pad));
        let mut spans = vec![Span::raw(" ".repeat(VALUE_COL))];
        if state.mgr.cursor == i {
            spans.push(Span::styled(
                body,
                Style::default()
                    .fg(theme.error)
                    .patch(theme.selection_style()),
            ));
        } else {
            spans.push(Span::styled(body, Style::default().fg(theme.error)));
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn model_expansion(state: &ComposerState, theme: &Theme) -> Vec<Line<'static>> {
    let mut labels = vec!["default \u{2014} use config/CLI default".to_string()];
    labels.extend(
        MODEL_ALIASES
            .iter()
            .map(|(alias, desc)| format!("{alias} \u{2014} {desc}")),
    );
    labels.push(if state.model_input {
        format!("custom: {}\u{258f}", state.model_custom)
    } else {
        "custom\u{2026}".to_string()
    });
    labels
        .into_iter()
        .enumerate()
        .map(|(i, label)| {
            // Never reverse the leading indent.
            let style = if state.model_idx == i {
                Style::default()
                    .fg(theme.accent)
                    .patch(theme.selection_style())
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::raw(" ".repeat(VALUE_COL)),
                Span::styled(label, style),
            ])
        })
        .collect()
}

/// Layout density tiers, airiest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    Airy,
    Compact,
    Minimal,
}

fn build_setup_lines(
    state: &ComposerState,
    theme: &Theme,
    width: u16,
    tier: Tier,
) -> Vec<Line<'static>> {
    let budget = (width as usize).saturating_sub(VALUE_COL);
    let mut lines = Vec::new();
    if tier == Tier::Airy {
        lines.push(Line::raw(""));
    }
    for field in VALUE_FIELDS {
        lines.push(value_line(state, field, theme, budget));
        if state.editing && state.field == field {
            match field {
                Field::Target => lines.extend(target_expansion(state, theme)),
                Field::Reviewers => lines.extend(reviewer_expansion(state, theme, budget)),
                Field::Model => lines.extend(model_expansion(state, theme)),
                Field::CrossReview | Field::Start => {}
            }
        } else if tier != Tier::Minimal {
            lines.push(description_line(state, field, theme, budget));
        }
        if tier == Tier::Airy {
            lines.push(Line::raw(""));
        }
    }
    lines.push(start_row(state, theme));
    lines
}

fn draw_header(f: &mut Frame, area: Rect, theme: &Theme) {
    f.render_widget(
        Paragraph::new(Line::styled("new review", theme.title_style())),
        area,
    );
}

fn highlight_filter(text: &str, filter: &str, theme: &Theme) -> Vec<Span<'static>> {
    if filter.is_empty() {
        return vec![Span::raw(text.to_string())];
    }
    // `str::to_lowercase()` can change a string's byte (and even char)
    // length — `İ` (U+0130) case-folds to "i" plus a combining dot — so an
    // index found in a lowercased haystack can slice the original mid-char.
    // Compare one case-folded char per original char (first char of any
    // multi-char expansion, keeping the sequences the same length) and only
    // index `text` at positions its own `char_indices()` reports.
    let orig_chars: Vec<(usize, char)> = text.char_indices().collect();
    let lower_chars: Vec<char> = orig_chars
        .iter()
        .map(|(_, c)| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    let filter_chars: Vec<char> = filter
        .chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect();

    let n = lower_chars.len();
    let m = filter_chars.len();
    let found = (m > 0 && m <= n)
        .then(|| (0..=n - m).find(|&start| lower_chars[start..start + m] == filter_chars[..]))
        .flatten();

    match found {
        Some(start_idx) => {
            let end_idx = start_idx + m;
            let start_byte = orig_chars[start_idx].0;
            let end_byte = orig_chars
                .get(end_idx)
                .map(|(b, _)| *b)
                .unwrap_or(text.len());
            vec![
                Span::raw(text[..start_byte].to_string()),
                Span::styled(
                    text[start_byte..end_byte].to_string(),
                    Style::default().fg(theme.severity_warning),
                ),
                Span::raw(text[end_byte..].to_string()),
            ]
        }
        None => vec![Span::raw(text.to_string())],
    }
}

fn draw_picker(f: &mut Frame, area: Rect, state: &ComposerState, theme: &Theme) {
    let Some(picker) = &state.picker else {
        return;
    };
    let filtered = state.filtered_spec_files();
    let total = filtered.len();
    // filter line + one row per match + a blank spacer + footer hints, plus
    // the block's top/bottom border.
    let desired_height = total as u16 + 5;
    let rect = crate::ui::overlay::centered(area, 70, desired_height);
    f.render_widget(Clear, rect);

    // A Paragraph has no scroll offset — it renders from the top and
    // silently drops what doesn't fit — so without a windowed subset the
    // cursor and footer hints could land below the visible area. Reserve 3
    // fixed lines (filter + blank spacer + footer hints); the rest is a
    // list window constructed to always contain the cursor.
    let inner_height = rect.height.saturating_sub(2) as usize;
    let list_capacity = inner_height.saturating_sub(3);
    let cursor = picker.cursor.min(total.saturating_sub(1));
    let start = if list_capacity == 0 || total <= list_capacity {
        0
    } else {
        cursor
            .saturating_sub(list_capacity - 1)
            .min(total - list_capacity)
    };
    let end = (start + list_capacity).min(total);

    let mut lines = vec![Line::styled(
        format!("  / {}\u{258f}", picker.filter),
        theme.accent_style(),
    )];
    for (i, path) in filtered.iter().enumerate().skip(start).take(end - start) {
        let marker = if picker.selected.contains(*path) {
            "[x]"
        } else {
            "[ ]"
        };
        let mut spans = vec![Span::raw(format!("  {marker} "))];
        spans.extend(highlight_filter(
            &path.display().to_string(),
            &picker.filter,
            theme,
        ));
        let mut line = Line::from(spans);
        if picker.cursor == i {
            line = line.patch_style(theme.selection_style());
        }
        lines.push(line);
    }
    lines.push(Line::raw(""));
    lines.push(theme.hints(&[
        ("type", "to filter"),
        ("space", "select"),
        ("enter", "done"),
        ("esc", "cancel"),
    ]));

    let title = format!(
        "pick spec files \u{2014} {} selected",
        picker.selected.len()
    );
    let block = theme.panel(&title, true);
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

pub(crate) fn draw(f: &mut Frame, area: Rect, state: &ComposerState, theme: &Theme) {
    let inner_width = area.width.saturating_sub(2);
    // Header, error, and hint rows are always reserved — the box gets the
    // rest, and the layout degrades airy → compact → minimal to fit it.
    let max_box = area.height.saturating_sub(3);
    let mut rows = Vec::new();
    for tier in [Tier::Airy, Tier::Compact, Tier::Minimal] {
        rows = build_setup_lines(state, theme, inner_width, tier);
        if rows.len() as u16 + 2 <= max_box {
            break;
        }
    }
    let box_height = (rows.len() as u16 + 2).min(max_box);
    let [header_area, box_area, error_area, hint_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(box_height),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(f, header_area, theme);
    let (cost_prose, cost_calls) = state.cost_parts();
    f.render_widget(
        Paragraph::new(rows).block(
            theme
                .panel("setup", true)
                .title_bottom(
                    Line::from(vec![
                        Span::styled(cost_prose, theme.dim_style()),
                        Span::raw(cost_calls),
                    ])
                    .right_aligned(),
                )
                .border_style(theme.accent_style()),
        ),
        box_area,
    );

    if let Some(err) = &state.error {
        f.render_widget(
            Paragraph::new(err.as_str()).style(Style::default().fg(theme.error)),
            error_area,
        );
    } else if let Some(n) = &state.mgr.notice {
        f.render_widget(
            Paragraph::new(n.as_str()).style(Style::default().fg(theme.severity_warning)),
            error_area,
        );
    } else if !state.mgr.warnings.is_empty() {
        f.render_widget(
            Paragraph::new(state.mgr.warnings.join("; "))
                .style(Style::default().fg(theme.severity_warning))
                .wrap(Wrap { trim: false }),
            error_area,
        );
    }

    // Enter's meaning shifts per mode — the hint line must always state the
    // active one.
    let hints = if state.model_input {
        theme.hints(&[
            ("type", "full model name"),
            ("enter", "accept"),
            ("esc", "cancel"),
        ])
    } else if state.editing {
        match state.field {
            Field::Reviewers => {
                if let Some(label) = state.mgr.armed_footer_label() {
                    theme.hints(&[("x", label.as_str())])
                } else if state.row_count() == 0 {
                    theme.hints(&[("n", "new"), ("enter", "done")])
                } else {
                    theme.hints(&[
                        ("space", "toggle"),
                        ("enter", "done"),
                        ("v/e/n/d/x", "view\u{b7}edit\u{b7}new\u{b7}dup\u{b7}del"),
                    ])
                }
            }
            _ => theme.hints(&[("enter", "select"), ("esc", "back")]),
        }
    } else {
        let enter_verb = match state.field {
            Field::Target | Field::Reviewers | Field::Model => "edit",
            Field::CrossReview => "toggle",
            Field::Start => "start run",
        };
        theme.hints(&[("enter", enter_verb), ("esc", "home")])
    };
    f.render_widget(Paragraph::new(hints), hint_area);

    if state.picker.is_some() {
        // Full clear: the setup box is taller and wider than the picker
        // rect, so without it fragments leak around the picker's edges.
        f.render_widget(Clear, area);
        draw_picker(f, area, state, theme);
    }

    draw_pager(f, area, state, theme);
    draw_scope(f, area, state, theme);
}

fn draw_pager(f: &mut Frame, area: Rect, state: &ComposerState, theme: &Theme) {
    crate::ui::personas::draw_pager(f, area, &state.mgr, theme);
}

fn draw_scope(f: &mut Frame, area: Rect, state: &ComposerState, theme: &Theme) {
    crate::ui::personas::draw_scope(f, area, &state.mgr, theme);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::app::render_to_text;
    use crate::ui::personas::{armed_delete_label, EditorRequest, ScopeOp};
    use crate::ui::test_keys::{key, key_code};
    use std::fs;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            st.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&st.stderr)
        );
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-b", "main"]);
        git(dir, &["config", "user.email", "t@t"]);
        git(dir, &["config", "user.name", "t"]);
    }

    /// A git repo with one committed file, then an uncommitted edit — the
    /// simplest scenario `detect_targets` reports one `GitDiff { base: None }`
    /// row for.
    fn seed_uncommitted_change(dir: &Path) {
        init_repo(dir);
        fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "c1"]);
        fs::write(dir.join("a.txt"), "two\n").unwrap();
    }

    fn repo_with_uncommitted_change() -> (PathBuf, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        seed_uncommitted_change(dir.path());
        (dir.path().to_path_buf(), dir)
    }

    /// A repo with a clean feature branch ahead of main, plus an uncommitted
    /// edit — `detect_targets` reports two rows (both `TargetKind::Code`).
    fn repo_with_uncommitted_change_and_branch() -> (PathBuf, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        fs::write(root.join("a.txt"), "base\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "init"]);
        git(root, &["checkout", "-b", "feature"]);
        fs::write(root.join("b.txt"), "new\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "feat"]);
        fs::write(root.join("a.txt"), "dirty\n").unwrap();
        (root.to_path_buf(), dir)
    }

    fn dir_with_specs(names: &[&str]) -> (PathBuf, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        for n in names {
            fs::write(dir.path().join(n), "x").unwrap();
        }
        (dir.path().to_path_buf(), dir)
    }

    #[test]
    fn happy_path_jjjj_enter_starts_run_from_start_row() {
        let (root, _g) = repo_with_uncommitted_change();
        let mut c = ComposerState::new(&root, &Config::default(), None, false);
        for _ in 0..4 {
            c.handle_key(key('j'));
        }
        assert_eq!(c.field, Field::Start);
        match c.handle_key(key_code(KeyCode::Enter)) {
            Some(Transition::StartRun(spec)) => {
                assert_eq!(spec.target, Target::GitDiff { base: None });
                assert!(!spec.cross_review);
                assert!(spec.personas.len() >= 2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn enter_on_fields_opens_editors_and_toggles_never_starts_run() {
        let (root, _g) = repo_with_uncommitted_change();
        let mut c = ComposerState::new(&root, &Config::default(), None, false);

        assert!(c.handle_key(key_code(KeyCode::Enter)).is_none());
        assert!(c.editing, "enter on target opens its editor");
        assert_eq!(c.target_cursor, 0);
        c.handle_key(key_code(KeyCode::Esc));

        c.field = Field::Reviewers;
        assert!(c.handle_key(key_code(KeyCode::Enter)).is_none());
        assert!(c.editing, "enter on reviewers opens the checklist");
        c.handle_key(key_code(KeyCode::Esc));

        c.field = Field::Model;
        assert!(c.handle_key(key_code(KeyCode::Enter)).is_none());
        assert!(c.editing, "enter on model opens the list");
        c.handle_key(key_code(KeyCode::Esc));

        c.field = Field::CrossReview;
        assert!(c.handle_key(key_code(KeyCode::Enter)).is_none());
        assert!(c.cross_review, "enter on cross-review toggles it");
        assert!(!c.editing);
    }

    #[test]
    fn target_editor_selects_diff_and_preserves_personas() {
        let (root, _g) = repo_with_uncommitted_change_and_branch(); // 2 diff targets
        let mut c = ComposerState::new(&root, &Config::default(), None, false);
        c.field = Field::Reviewers;
        c.handle_key(key(' ')); // open checklist
        c.handle_key(key(' ')); // toggle first persona off
        c.handle_key(key_code(KeyCode::Esc));
        let disabled = c.mgr.personas.iter().filter(|p| !p.enabled).count();
        assert_eq!(disabled, 1);

        c.field = Field::Target;
        c.handle_key(key_code(KeyCode::Enter));
        assert!(c.editing);
        c.handle_key(key('j'));
        c.handle_key(key_code(KeyCode::Enter));
        assert!(!c.editing, "selecting a target closes the editor");
        assert_eq!(c.target_choice, Some(1));
        assert_eq!(
            c.mgr.personas.iter().filter(|p| !p.enabled).count(),
            1,
            "diff→diff keeps reviewer choices"
        );
    }

    #[test]
    fn target_editor_specs_row_opens_picker_when_none_chosen() {
        let (root, _g) = repo_with_uncommitted_change(); // 1 diff target + no specs chosen
        let mut c = ComposerState::new(&root, &Config::default(), None, false);
        c.handle_key(key_code(KeyCode::Enter)); // open target editor
        c.handle_key(key('j')); // move to the spec-files row
        assert_eq!(c.target_cursor, c.targets.len());
        c.handle_key(key_code(KeyCode::Enter));
        assert!(!c.editing);
        assert_eq!(c.target_choice, None);
        assert!(
            c.picker.is_some(),
            "specs row with nothing chosen opens the picker"
        );
    }

    #[test]
    fn target_cycle_preserves_persona_selections_for_same_kind() {
        let (root, _g) = repo_with_uncommitted_change_and_branch(); // 2 diff targets
        let mut c = ComposerState::new(&root, &Config::default(), None, false);
        c.field = Field::Reviewers;
        c.handle_key(key(' ')); // open the reviewer editor
        c.handle_key(key(' ')); // toggle cursored (first) persona off
        c.handle_key(key_code(KeyCode::Esc)); // close the editor
        let disabled: Vec<String> = c
            .mgr
            .personas
            .iter()
            .filter(|p| !p.enabled)
            .map(|p| p.persona.name.clone())
            .collect();
        assert!(!disabled.is_empty(), "the toggle must have taken effect");
        c.handle_key(key('t'));
        let still_disabled: Vec<String> = c
            .mgr
            .personas
            .iter()
            .filter(|p| !p.enabled)
            .map(|p| p.persona.name.clone())
            .collect();
        assert_eq!(
            disabled, still_disabled,
            "cycling targets must not wipe reviewer choices"
        );
    }

    #[test]
    fn fewer_than_two_reviewers_blocks_with_inline_error() {
        let (root, _g) = repo_with_uncommitted_change();
        let mut c = ComposerState::new(&root, &Config::default(), None, false);
        c.field = Field::Reviewers;
        c.handle_key(key(' ')); // open the reviewer editor
        let n = c.mgr.personas.len();
        assert!(n >= 2, "fixture must ship at least 2 builtins to disable");
        for i in 1..n {
            c.mgr.cursor = i;
            c.handle_key(key(' '));
        }
        assert_eq!(c.mgr.personas.iter().filter(|p| p.enabled).count(), 1);
        c.handle_key(key_code(KeyCode::Enter)); // close the editor
        assert!(
            !c.editing,
            "enter in the editor closes it, never starts the run"
        );
        c.field = Field::Start;
        let t = c.handle_key(key_code(KeyCode::Enter));
        assert!(t.is_none());
        assert!(
            c.error
                .as_deref()
                .is_some_and(|e| e.contains("need at least 2 reviewers")),
            "{:?}",
            c.error
        );
    }

    #[test]
    fn no_specs_selected_blocks_with_inline_error() {
        let (root, _g) = dir_with_specs(&["a.md"]);
        let mut c = ComposerState::new(&root, &Config::default(), None, false);
        assert_eq!(
            c.target_choice, None,
            "not a git repo: specs is the only option"
        );
        c.field = Field::Start;
        let t = c.handle_key(key_code(KeyCode::Enter));
        assert!(t.is_none());
        assert!(
            c.error
                .as_deref()
                .is_some_and(|e| e.contains("select at least one spec file")),
            "{:?}",
            c.error
        );
    }

    #[test]
    fn picker_enter_selects_hovered_when_nothing_toggled() {
        let (root, _g) = dir_with_specs(&["a.md", "b.md"]);
        let mut c = ComposerState::new(&root, &Config::default(), None, true);
        assert!(c.picker.is_some());
        c.handle_key(key_code(KeyCode::Enter)); // nothing toggled → hovered row selected + committed
        assert_eq!(c.chosen_specs.len(), 1);
        assert!(c.picker.is_none());
    }

    #[test]
    fn picker_type_to_filter_ignores_control_chords() {
        let (root, _g) = dir_with_specs(&["store.md", "other.md"]);
        let mut c = ComposerState::new(&root, &Config::default(), None, true);
        for ch in "store".chars() {
            c.handle_key(key(ch));
        }
        let ctrl_u = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        c.handle_key(ctrl_u);
        assert_eq!(
            c.picker.as_ref().unwrap().filter,
            "store",
            "Ctrl+U must not type a literal u"
        );
    }

    #[test]
    fn picker_filter_narrows_visible_list() {
        let (root, _g) = dir_with_specs(&["alpha.md", "beta.md"]);
        let mut c = ComposerState::new(&root, &Config::default(), None, true);
        for ch in "beta".chars() {
            c.handle_key(key(ch));
        }
        assert_eq!(c.filtered_spec_files(), vec![&PathBuf::from("beta.md")]);
    }

    #[test]
    fn picker_renders_windowed_list_keeping_cursor_and_footer_visible() {
        // 30 spec files in a small (100x20) terminal: the overlay's desired
        // height (30 rows + chrome) can't fit, so without a scroll window
        // the cursor could sit below whatever a non-scrolling Paragraph
        // happens to render — invisible, yet still committable on Enter.
        let names: Vec<String> = (0..30).map(|i| format!("spec{i:02}.md")).collect();
        let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let (root, _g) = dir_with_specs(&name_refs);
        let mut c = ComposerState::new(&root, &Config::default(), None, true);
        assert_eq!(c.spec_files.len(), 30);
        for _ in 0..20 {
            c.handle_key(key_code(KeyCode::Down));
        }
        assert_eq!(c.picker.as_ref().unwrap().cursor, 20);

        let text = render_to_text(100, 20, |f| draw(f, f.area(), &c, &Theme::default()));
        assert!(
            text.contains("spec20.md"),
            "cursored row must be visible: {text}"
        );
        assert!(
            !text.contains("spec00.md"),
            "scrolled-off row must not render: {text}"
        );
        assert!(
            text.contains("type to filter"),
            "footer hints must stay visible: {text}"
        );
        assert!(text.contains("space select"), "{text}");
        assert!(text.contains("enter done"), "{text}");
        assert!(text.contains("esc cancel"), "{text}");
    }

    #[test]
    fn picker_renders_as_clean_modal_hiding_the_composer() {
        let (root, _g) = dir_with_specs(&["a.md", "b.md"]);
        let c = ComposerState::new(&root, &Config::default(), None, true);
        assert!(c.picker.is_some());
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &c, &Theme::default()));
        assert!(text.contains("pick spec files"), "{text}");
        assert!(
            !text.contains("cross-review") && !text.contains("model calls"),
            "composer content must not leak around the picker modal: {text}"
        );
    }

    #[test]
    fn picker_esc_without_selection_falls_back_to_first_target() {
        let (root, _g) = repo_with_uncommitted_change();
        let mut c = ComposerState::new(&root, &Config::default(), None, false);
        c.field = Field::Target;
        c.handle_key(key('t')); // only 1 diff target → cycles straight to specs, opens picker
        assert!(c.picker.is_some());
        c.handle_key(key_code(KeyCode::Esc));
        assert!(c.picker.is_none());
        assert_eq!(
            c.target_choice,
            Some(0),
            "esc with nothing selected falls back to the first detected target"
        );
    }

    #[test]
    fn collect_spec_files_skips_venv_and_dist() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".venv")).unwrap();
        fs::write(dir.path().join(".venv/x.md"), "x").unwrap();
        fs::create_dir_all(dir.path().join("dist")).unwrap();
        fs::write(dir.path().join("dist/y.md"), "x").unwrap();
        fs::write(dir.path().join("real.md"), "x").unwrap();
        let files = collect_spec_files(dir.path());
        assert_eq!(files, vec![PathBuf::from("real.md")]);
    }

    #[test]
    fn spec_walk_skips_every_noise_dir_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("b.md"), "x").unwrap();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/a.md"), "x").unwrap();
        for noise in SKIP_DIRS {
            let noisy_dir = dir.path().join(noise);
            fs::create_dir_all(&noisy_dir).unwrap();
            fs::write(noisy_dir.join("ignored.md"), "x").unwrap();
        }
        let files = collect_spec_files(dir.path());
        assert_eq!(
            files,
            vec![PathBuf::from("b.md"), PathBuf::from("sub/a.md")]
        );
    }

    #[test]
    fn render_shows_cost_estimate_in_bottom_border_and_expanded_reviewers() {
        let (root, _g) = repo_with_uncommitted_change();
        let mut c = ComposerState::new(&root, &Config::default(), None, false);
        c.field = Field::Reviewers;
        c.editing = true;
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &c, &Theme::default()));
        assert!(
            text.contains("3 reviewers \u{d7} 1 round = 3 model calls"),
            "{text}"
        );
        let header = text.lines().next().unwrap();
        assert!(header.contains("new review"), "{header}");
        assert!(
            !header.contains("model calls"),
            "cost must leave the header for the bottom border: {header}"
        );
        assert!(text.contains("[x] prover"), "{text}");
        assert!(!text.contains("(custom)"), "{text}");
        assert!(
            !text.contains("3 of 3 personas on"),
            "the open editor replaces the description line: {text}"
        );

        c.cross_review = true;
        let text2 = render_to_text(100, 30, |f| draw(f, f.area(), &c, &Theme::default()));
        assert!(text2.contains("\u{d7} 2 rounds = 6 model calls"), "{text2}");
    }

    #[test]
    fn focusing_a_field_does_not_expand_it_until_space() {
        let (root, _g) = repo_with_uncommitted_change();
        let mut c = ComposerState::new(&root, &Config::default(), None, false);
        c.field = Field::Reviewers;
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &c, &Theme::default()));
        assert!(
            !text.contains("[x]"),
            "focus alone must not expand the checklist: {text}"
        );
        c.handle_key(key(' '));
        assert!(c.editing);
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &c, &Theme::default()));
        assert!(text.contains("[x] prover"), "{text}");

        c.handle_key(key_code(KeyCode::Esc));
        c.field = Field::Model;
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &c, &Theme::default()));
        assert!(
            !text.contains("latest Sonnet"),
            "model list must stay collapsed: {text}"
        );
    }

    #[test]
    fn jk_in_fields_mode_hops_fields_without_descending_into_lists() {
        let (root, _g) = repo_with_uncommitted_change();
        let mut c = ComposerState::new(&root, &Config::default(), None, false);
        assert_eq!(c.field, Field::Target);
        c.handle_key(key('j'));
        assert_eq!(c.field, Field::Reviewers);
        c.handle_key(key('j'));
        assert_eq!(
            c.field,
            Field::Model,
            "j hops straight past the reviewer checklist"
        );
        c.handle_key(key('j'));
        assert_eq!(c.field, Field::CrossReview);
        c.handle_key(key('j'));
        assert_eq!(c.field, Field::Start);
        c.handle_key(key('j'));
        assert_eq!(c.field, Field::Start, "j clamps at the start-review row");
    }

    #[test]
    fn edit_mode_captures_jk_clamped_and_closes_on_enter_or_esc() {
        let (root, _g) = repo_with_uncommitted_change();
        let mut c = ComposerState::new(&root, &Config::default(), None, false);
        c.field = Field::Reviewers;
        c.handle_key(key(' '));
        assert!(c.editing);
        c.handle_key(key('k'));
        assert_eq!(
            c.field,
            Field::Reviewers,
            "k at the top must not leak to the target field"
        );
        assert_eq!(c.mgr.cursor, 0);
        c.handle_key(key('j'));
        assert_eq!(c.mgr.cursor, 1);
        for _ in 0..10 {
            c.handle_key(key('j'));
        }
        assert_eq!(
            c.mgr.cursor,
            c.mgr.personas.len() - 1,
            "j clamps at the last reviewer"
        );
        assert_eq!(
            c.field,
            Field::Reviewers,
            "j at the bottom must not leak to the model field"
        );
        let t = c.handle_key(key_code(KeyCode::Enter));
        assert!(
            t.is_none(),
            "enter in the editor closes it, never starts the run"
        );
        assert!(!c.editing);

        c.handle_key(key(' '));
        assert!(c.editing);
        let t = c.handle_key(key_code(KeyCode::Esc));
        assert!(
            t.is_none(),
            "esc in the editor closes it without leaving the composer"
        );
        assert!(!c.editing);
        assert!(
            matches!(
                c.handle_key(key_code(KeyCode::Esc)),
                Some(Transition::ToHome)
            ),
            "esc in fields mode still goes home"
        );
    }

    #[test]
    fn value_and_description_render_on_separate_lines() {
        let (root, _g) = repo_with_uncommitted_change();
        let c = ComposerState::new(&root, &Config::default(), None, false);
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &c, &Theme::default()));
        let cross = text
            .lines()
            .find(|l| l.contains("cross-review"))
            .expect("cross-review value line");
        assert!(cross.contains("off"), "{cross}");
        assert!(
            !cross.contains("reviewers work blind"),
            "prose must leave the value line: {cross}"
        );
        assert!(
            text.contains("reviewers work blind"),
            "description line: {text}"
        );
        assert!(text.contains("3 of 3 personas on"), "{text}");
        assert!(text.contains("use config/CLI default"), "{text}");
    }

    #[test]
    fn spec_description_shows_filename_only_and_truncates_overlong_names() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir
            .path()
            .join("docs/very/deep/nested/directory/chain/for/testing");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("the-important-spec-name.md"), "x").unwrap();
        let mut c = ComposerState::new(dir.path(), &Config::default(), None, false);
        c.chosen_specs = c.spec_files.clone();
        let text = render_to_text(60, 30, |f| draw(f, f.area(), &c, &Theme::default()));
        assert!(text.contains("the-important-spec-name.md"), "{text}");
        assert!(
            !text.contains("docs/very"),
            "directories are noise in the description: {text}"
        );

        // A filename wider than the budget still end-preserving-truncates.
        c.chosen_specs = vec![PathBuf::from(
            "an-extremely-long-spec-file-name-that-cannot-possibly-fit-2026.md",
        )];
        let text = render_to_text(60, 30, |f| draw(f, f.area(), &c, &Theme::default()));
        assert!(text.contains('\u{2026}'), "{text}");
        assert!(
            text.contains("2026.md"),
            "filename end must survive: {text}"
        );
    }

    #[test]
    fn many_reviewers_degrade_to_whole_names_plus_count() {
        let (root, _g) = repo_with_uncommitted_change();
        let mut c = ComposerState::new(&root, &Config::default(), None, false);
        for p in c.mgr.personas.iter_mut() {
            p.enabled = true;
        }
        // width 40 → 38 inner → 22-column value budget: three builtin names
        // + separators exceed it, so whole names drop in favor of " +N".
        let text = render_to_text(40, 30, |f| draw(f, f.area(), &c, &Theme::default()));
        // Skip the header: its cost string ("3 reviewers × …") also contains
        // "reviewers" and renders above the setup rows.
        let line = text
            .lines()
            .find(|l| l.contains("reviewers") && !l.contains("model calls"))
            .expect("reviewers value line");
        assert!(
            line.contains(" +"),
            "overflow must degrade to a +N count: {line}"
        );
    }

    /// Line distance between the first `target` and `cross-review` value
    /// rows (three field blocks apart): airy = 9, compact = 6, minimal = 3.
    /// These anchors are chosen because "reviewers" or "model" would match
    /// the cost string first and underflow the subtraction.
    fn tier_gap(text: &str) -> usize {
        let lines: Vec<&str> = text.lines().collect();
        let ia = lines.iter().position(|l| l.contains("target")).unwrap();
        let ib = lines
            .iter()
            .position(|l| l.contains("cross-review"))
            .unwrap();
        ib - ia
    }

    #[test]
    fn short_terminals_fall_back_to_denser_tiers() {
        let (root, _g) = repo_with_uncommitted_change();
        let c = ComposerState::new(&root, &Config::default(), None, false);

        // 30 rows: airy — value + description + blank per block.
        let airy = render_to_text(100, 30, |f| draw(f, f.area(), &c, &Theme::default()));
        assert_eq!(tier_gap(&airy), 9, "{airy}");
        assert!(airy.contains("reviewers work blind"), "{airy}");

        // 16 rows (airy needs 18): compact — descriptions, no blanks.
        let compact = render_to_text(100, 16, |f| draw(f, f.area(), &c, &Theme::default()));
        assert_eq!(tier_gap(&compact), 6, "{compact}");
        assert!(compact.contains("reviewers work blind"), "{compact}");

        // 11 rows (compact needs 13): minimal — values only, hints intact.
        let minimal = render_to_text(100, 11, |f| draw(f, f.area(), &c, &Theme::default()));
        assert_eq!(tier_gap(&minimal), 3, "{minimal}");
        assert!(!minimal.contains("reviewers work blind"), "{minimal}");
        assert!(
            minimal.contains("esc home"),
            "hints must survive: {minimal}"
        );
    }

    #[test]
    fn degenerate_sizes_never_panic() {
        let (root, _g) = repo_with_uncommitted_change();
        let mut c = ComposerState::new(&root, &Config::default(), None, false);
        for (w, h) in [(17, 30), (10, 8), (4, 4), (2, 2), (100, 5), (100, 3)] {
            let _ = render_to_text(w, h, |f| draw(f, f.area(), &c, &Theme::default()));
        }
        // and while an editor is open on a tiny screen
        c.field = Field::Model;
        c.editing = true;
        let _ = render_to_text(30, 8, |f| draw(f, f.area(), &c, &Theme::default()));
    }

    #[test]
    fn empty_spec_selection_prompts_next_action() {
        let (root, _g) = dir_with_specs(&["a.md"]);
        let mut c = ComposerState::new(&root, &Config::default(), None, false);
        c.picker = None; // defensive: nothing selected, no picker overlay
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &c, &Theme::default()));
        assert!(
            text.contains("none selected \u{2014} space to pick"),
            "{text}"
        );
    }

    #[test]
    fn model_prefill_from_config() {
        let dir = tempfile::tempdir().unwrap();

        let c = ComposerState::new(dir.path(), &Config::default(), None, false);
        assert_eq!(c.model_idx, 0);
        assert_eq!(c.chosen_model(), None);

        let config = Config {
            model: Some("sonnet".into()),
            ..Config::default()
        };
        let c = ComposerState::new(dir.path(), &config, None, false);
        assert_eq!(c.model_idx, 3, "default, fable, opus, sonnet");
        assert_eq!(c.chosen_model(), Some("sonnet".into()));

        let config = Config {
            model: Some("my-pinned-model".into()),
            ..Config::default()
        };
        let c = ComposerState::new(dir.path(), &config, None, false);
        assert_eq!(c.model_idx, model_custom_index());
        assert_eq!(c.model_custom, "my-pinned-model");
        assert_eq!(c.chosen_model(), Some("my-pinned-model".into()));
    }

    #[test]
    fn new_collects_invalid_persona_color_warnings() {
        let dir = tempfile::tempdir().unwrap();
        let personas_dir = dir.path().join(".reviewal/personas");
        fs::create_dir_all(&personas_dir).unwrap();
        fs::write(
            personas_dir.join("bad.md"),
            "+++\nname = \"bad\"\ntitle = \"Bad\"\nlens = \"l\"\ntarget = \"both\"\ncolor = \"blurple\"\n+++\nbody",
        )
        .unwrap();
        let c = ComposerState::new(dir.path(), &Config::default(), None, false);
        assert!(
            c.mgr
                .warnings
                .iter()
                .any(|w| w.contains("bad") && w.contains("blurple")),
            "warnings: {:?}",
            c.mgr.warnings
        );
    }

    #[test]
    fn default_enabled_customs_and_shadowing_builtins() {
        let dir = tempfile::tempdir().unwrap();
        seed_uncommitted_change(dir.path());
        let personas_dir = dir.path().join(".reviewal/personas");
        fs::create_dir_all(&personas_dir).unwrap();
        fs::write(
            personas_dir.join("prover.md"),
            "+++\nname = \"prover\"\ntitle = \"Prover\"\nlens = \"custom lens\"\ntarget = \"code\"\n+++\nbody",
        )
        .unwrap();
        fs::write(
            personas_dir.join("newbie.md"),
            "+++\nname = \"newbie\"\ntitle = \"Newbie\"\nlens = \"l\"\ntarget = \"code\"\n+++\nbody",
        )
        .unwrap();

        let c = ComposerState::new(dir.path(), &Config::default(), None, false);
        let prover = c
            .mgr
            .personas
            .iter()
            .find(|p| p.persona.name == "prover")
            .unwrap();
        assert!(
            !prover.persona.builtin,
            "the custom file shadowed the builtin"
        );
        assert!(
            prover.enabled,
            "a custom persona shadowing a builtin stays default-ON"
        );
        let newbie = c
            .mgr
            .personas
            .iter()
            .find(|p| p.persona.name == "newbie")
            .unwrap();
        assert!(
            !newbie.enabled,
            "a non-shadowing custom persona defaults off"
        );
    }

    #[test]
    fn draw_renders_without_panicking_in_every_field() {
        let (root, _g) = repo_with_uncommitted_change();
        let mut c = ComposerState::new(&root, &Config::default(), None, false);
        for (field, verb) in [
            (Field::Target, "enter edit"),
            (Field::Reviewers, "enter edit"),
            (Field::Model, "enter edit"),
            (Field::CrossReview, "enter toggle"),
            (Field::Start, "enter start run"),
        ] {
            c.field = field;
            let text = render_to_text(100, 30, |f| draw(f, f.area(), &c, &Theme::default()));
            assert!(text.contains("setup"), "{text}");
            assert!(text.contains(verb), "{text}");
            assert!(text.contains("esc home"), "{text}");
            assert!(text.contains("start review"), "{text}");
        }
    }

    #[test]
    fn model_custom_row_opens_input_and_accepts_free_text() {
        let (root, _g) = repo_with_uncommitted_change();
        let mut c = ComposerState::new(&root, &Config::default(), None, false);
        c.field = Field::Model;
        c.handle_key(key(' ')); // open the model editor
        assert!(c.editing, "space on the model field opens its editor");
        c.model_idx = model_custom_index();
        c.handle_key(key_code(KeyCode::Enter));
        assert!(c.model_input, "enter on the custom row opens text input");
        for ch in "claude-fable-5".chars() {
            c.handle_key(key(ch));
        }
        c.handle_key(key_code(KeyCode::Enter));
        assert!(!c.model_input, "enter on the open input closes it again");
        assert!(
            !c.editing,
            "accepting a custom model also closes the editor"
        );
        assert_eq!(c.chosen_model(), Some("claude-fable-5".into()));
    }

    // Enter's meaning changes while the custom-model input is capturing text
    // (accept, not start-run); the hint line must say so — every screen
    // states what Enter does, including this sub-mode.
    #[test]
    fn model_input_hint_reflects_accept_not_start_run() {
        let (root, _g) = repo_with_uncommitted_change();
        let mut c = ComposerState::new(&root, &Config::default(), None, false);
        c.field = Field::Model;
        c.model_idx = model_custom_index();
        c.model_input = true;
        let text = render_to_text(100, 30, |f| draw(f, f.area(), &c, &Theme::default()));
        assert!(text.contains("enter accept"), "{text}");
        assert!(!text.contains("enter start run"), "{text}");
    }

    // `İ` (U+0130, LATIN CAPITAL LETTER I WITH DOT ABOVE) lowercases to TWO
    // chars in Rust's Unicode case folding ("i" + combining dot above,
    // U+0307) — 2 bytes growing to 3. A naive `lower_text.find(&lower_filter)`
    // returns a byte offset into the LOWERCASED haystack, which no longer
    // lines up with the ORIGINAL string's byte layout once such a char is
    // involved; slicing `text` at that offset can land mid-char and panic.
    #[test]
    fn highlight_filter_handles_length_changing_lowercase_chars() {
        let theme = Theme::default();
        let text = "İşlem-spec.md";

        let spans = highlight_filter(text, "şlem", &theme);
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, text);
        assert_eq!(spans.len(), 3, "{spans:?}");
        assert_eq!(spans[1].content.as_ref(), "şlem");

        let spans2 = highlight_filter(text, "işlem", &theme);
        let joined2: String = spans2.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined2, text);
        assert_eq!(spans2.len(), 3, "{spans2:?}");
        assert_eq!(spans2[1].content.as_ref(), "İşlem");
    }

    #[test]
    fn invalid_files_become_error_rows_with_tags_on_initial_load() {
        let dir = tempfile::tempdir().unwrap();
        let pdir = dir.path().join(".reviewal/personas");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(pdir.join("broken.md"), "not a persona").unwrap();
        let c = ComposerState::new(dir.path(), &Config::default(), None, false);
        assert_eq!(
            c.mgr.invalid.len(),
            1,
            "broken file surfaces on load, not only after an edit"
        );
        assert_eq!(c.row_count(), c.mgr.personas.len() + 1);
        assert!(c.mgr.warnings.iter().any(|w| w.contains("broken.md")));
    }

    #[test]
    fn provenance_tags_exact_strings() {
        let dir = tempfile::tempdir().unwrap();
        let pdir = dir.path().join(".reviewal/personas");
        std::fs::create_dir_all(&pdir).unwrap();
        // Shadow a builtin + a novel custom, both in the project dir.
        std::fs::write(
            pdir.join("skeptic.md"),
            "+++\nname = \"skeptic\"\ntitle = \"S\"\nlens = \"l\"\ntarget = \"both\"\n+++\nb",
        )
        .unwrap();
        std::fs::write(
            pdir.join("novel.md"),
            "+++\nname = \"novel\"\ntitle = \"N\"\ntarget = \"both\"\nlens = \"l\"\n+++\nb",
        )
        .unwrap();
        std::fs::write(pdir.join("junk.md"), "junk").unwrap();
        let c = ComposerState::new(dir.path(), &Config::default(), None, false);

        let tag_of = |name: &str| {
            let p = &c
                .mgr
                .personas
                .iter()
                .find(|x| x.persona.name == name)
                .unwrap()
                .persona;
            provenance_tag(&c.mgr, p)
        };
        assert_eq!(tag_of("advocate"), "built-in");
        assert_eq!(tag_of("skeptic"), "edited (project)");
        assert_eq!(tag_of("novel"), "project");
        assert_eq!(
            invalid_tag(&c.mgr, &c.mgr.invalid[0].path),
            "invalid (project)"
        );

        // Global variants are pure functions of the path — no env needed.
        let mut p = c
            .mgr
            .personas
            .iter()
            .find(|x| x.persona.name == "novel")
            .unwrap()
            .persona
            .clone();
        p.source = Some(std::path::PathBuf::from(
            "/somewhere/global/personas/novel.md",
        ));
        assert_eq!(provenance_tag(&c.mgr, &p), "global");
        p.name = "skeptic".into();
        assert_eq!(provenance_tag(&c.mgr, &p), "edited (global)");
    }

    #[test]
    fn checklist_renders_tags_and_invalid_rows_and_space_noops_on_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let pdir = dir.path().join(".reviewal/personas");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(pdir.join("zz-broken.md"), "junk").unwrap();
        let mut c = ComposerState::new(dir.path(), &Config::default(), None, false);
        c.field = Field::Reviewers;
        c.editing = true;
        let theme = crate::ui::theme::Theme::default();
        let text = crate::ui::app::render_to_text(100, 40, |f| draw(f, f.area(), &c, &theme));
        assert!(text.contains("built-in"), "builtin rows tagged: {text}");
        assert!(
            text.contains("zz-broken"),
            "invalid row shows the file stem"
        );
        assert!(text.contains("invalid (project)"), "invalid row tagged");

        let last = c.row_count() - 1;
        c.mgr.cursor = last;
        let before: Vec<bool> = c.mgr.personas.iter().map(|p| p.enabled).collect();
        c.handle_key(crate::ui::test_keys::key(' '));
        let after: Vec<bool> = c.mgr.personas.iter().map(|p| p.enabled).collect();
        assert_eq!(before, after, "space on an invalid row toggles nothing");
    }

    #[test]
    fn v_opens_pager_with_builtin_source_and_esc_closes() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = ComposerState::new(dir.path(), &Config::default(), None, false);
        c.field = Field::Reviewers;
        c.editing = true;
        c.mgr.cursor = 0;
        c.handle_key(crate::ui::test_keys::key('v'));
        let pager = c.mgr.pager.as_ref().expect("pager opens");
        let name = c.mgr.personas[0].persona.name.clone();
        assert!(pager.title.contains(&name));
        assert_eq!(
            pager.text,
            crate::engine::persona::builtin_source(&name).unwrap(),
            "pager shows the raw embedded source, no file written"
        );
        assert!(
            !dir.path().join(".reviewal/personas").exists(),
            "viewing never writes"
        );
        c.handle_key(crate::ui::test_keys::key('j'));
        assert_eq!(c.mgr.pager.as_ref().unwrap().scroll, 1);
        c.handle_key(crate::ui::test_keys::key_code(KeyCode::Esc));
        assert!(c.mgr.pager.is_none());
        assert!(c.editing, "esc closed the pager, not the checklist");
    }

    #[test]
    fn reviewers_editor_opens_when_empty_and_hints_adapt() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = ComposerState::new(dir.path(), &Config::default(), None, false);
        c.mgr.personas.clear();
        c.mgr.invalid.clear();
        c.field = Field::Reviewers;
        c.handle_key(crate::ui::test_keys::key_code(KeyCode::Enter));
        assert!(c.editing, "guard relaxed: empty checklist still opens");
        let theme = crate::ui::theme::Theme::default();
        let text = crate::ui::app::render_to_text(100, 30, |f| draw(f, f.area(), &c, &theme));
        // NB: don't assert `contains("new")` — the header says "new review".
        assert!(
            !text.contains("toggle"),
            "no space-toggle hint on an empty list: {text}"
        );
        assert!(
            !text.contains("v/e/n/d/x"),
            "full manage hint hidden when empty"
        );
        assert!(text.contains("done"), "enter done still offered");
    }

    #[test]
    fn full_manage_hint_shows_in_reviewer_edit_mode() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = ComposerState::new(dir.path(), &Config::default(), None, false);
        c.field = Field::Reviewers;
        c.editing = true;
        let theme = crate::ui::theme::Theme::default();
        let text = crate::ui::app::render_to_text(120, 30, |f| draw(f, f.area(), &c, &theme));
        assert!(
            text.contains("v/e/n/d/x")
                && text.contains("view\u{b7}edit\u{b7}new\u{b7}dup\u{b7}del"),
            "one condensed manage pair: {text}"
        );
    }

    // `{:<12}` is a MINIMUM width, never a truncation — a long custom
    // persona name blows the PREFIX budget the tag/lens math assumes,
    // pushing the provenance tag off the row entirely.
    #[test]
    fn checklist_truncates_long_persona_name_so_tag_survives() {
        let dir = tempfile::tempdir().unwrap();
        let pdir = dir.path().join(".reviewal/personas");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("security-focused-reviewer.md"),
            "+++\nname = \"security-focused-reviewer\"\ntitle = \"S\"\nlens = \"l\"\ntarget = \"both\"\n+++\nb",
        )
        .unwrap();
        let mut c = ComposerState::new(dir.path(), &Config::default(), None, false);
        c.field = Field::Reviewers;
        c.editing = true;
        let theme = crate::ui::theme::Theme::default();
        let text = crate::ui::app::render_to_text(100, 40, |f| draw(f, f.area(), &c, &theme));
        assert!(
            text.contains("project"),
            "the long-named custom persona's row still shows its provenance tag: {text}"
        );
        assert!(
            !text.contains("security-focused-reviewer"),
            "name column truncates to 12 columns instead of letting the tag get pushed off-row: {text}"
        );
    }

    fn reviewers_editing(dir: &std::path::Path) -> ComposerState {
        let mut c = ComposerState::new(dir, &Config::default(), None, false);
        c.field = Field::Reviewers;
        c.editing = true;
        c
    }

    #[test]
    fn e_on_custom_stages_edit_request_without_prompting() {
        let dir = tempfile::tempdir().unwrap();
        let pdir = dir.path().join(".reviewal/personas");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("mine.md"),
            "+++\nname = \"mine\"\ntitle = \"M\"\nlens = \"l\"\ntarget = \"both\"\n+++\nb",
        )
        .unwrap();
        let mut c = reviewers_editing(dir.path());
        let i = c
            .mgr
            .personas
            .iter()
            .position(|p| p.persona.name == "mine")
            .unwrap();
        c.mgr.cursor = i;
        c.mgr.personas[i].enabled = true;
        c.handle_key(crate::ui::test_keys::key('e'));
        assert!(
            c.mgr.scope_prompt.is_none(),
            "existing file: no [p]/[g] prompt"
        );
        let req = c.mgr.pending_editor.as_ref().expect("request staged");
        assert_eq!(req.path, pdir.join("mine.md"));
        assert!(!req.created, "edit of an existing file never marks created");
        assert_eq!(req.prior_enabled, Some(true));
        assert!(!req.auto_enable);
    }

    #[test]
    fn e_on_builtin_materializes_via_scope_prompt_created_true_only_when_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = reviewers_editing(dir.path());
        let name = c.mgr.personas[0].persona.name.clone();
        c.handle_key(crate::ui::test_keys::key('e'));
        assert!(
            matches!(c.mgr.scope_prompt, Some(ScopeOp::Materialize { .. })),
            "builtin prompts for scope"
        );
        c.handle_key(crate::ui::test_keys::key('p'));
        let path = dir
            .path()
            .join(".reviewal/personas")
            .join(format!("{name}.md"));
        let req = c.mgr.pending_editor.take().expect("request staged");
        assert_eq!(req.path, path);
        assert!(req.created, "fresh materialize wrote the file");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            crate::engine::persona::builtin_source(&name).unwrap(),
            "byte-exact copy"
        );

        // Second materialize onto the now-existing file: opened as-is, NOT
        // rewritten, and created must be false.
        std::fs::write(&path, "user edits, currently broken").unwrap();
        c.mgr.scope_prompt = None;
        c.mgr.cursor = 0;
        // Rebuild happens naturally in the app; simulate by finding the builtin row again.
        let mut c2 = reviewers_editing(dir.path());
        // The file shadows but is broken → builtin row is back; e on it → prompt → p.
        let bi = c2
            .mgr
            .personas
            .iter()
            .position(|p| p.persona.name == name && p.persona.builtin);
        if let Some(bi) = bi {
            c2.mgr.cursor = bi;
            c2.handle_key(crate::ui::test_keys::key('e'));
            c2.handle_key(crate::ui::test_keys::key('p'));
            let req2 = c2.mgr.pending_editor.take().expect("request staged");
            assert!(
                !req2.created,
                "materialize onto an existing file must stage created: false"
            );
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "user edits, currently broken",
                "existing file never overwritten with pristine builtin source"
            );
        } else {
            panic!("broken shadow should un-shadow the builtin");
        }
    }

    #[test]
    fn n_writes_deduped_template_and_d_copies_with_rewritten_name() {
        let dir = tempfile::tempdir().unwrap();
        let pdir = dir.path().join(".reviewal/personas");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("new-persona.md"),
            "+++\nname = \"new-persona\"\ntitle = \"T\"\nlens = \"l\"\ntarget = \"both\"\n+++\nb",
        )
        .unwrap();
        let mut c = reviewers_editing(dir.path());
        c.handle_key(crate::ui::test_keys::key('n'));
        assert!(matches!(c.mgr.scope_prompt, Some(ScopeOp::New)));
        c.handle_key(crate::ui::test_keys::key('p'));
        let req = c.mgr.pending_editor.take().unwrap();
        assert_eq!(
            req.persona_name, "new-persona-2",
            "slug deduped against the existing file"
        );
        assert!(req.created && req.auto_enable);
        let written = std::fs::read_to_string(&req.path).unwrap();
        let p = crate::engine::persona::parse_persona(&written, false).unwrap();
        assert_eq!(
            p.name, "new-persona-2",
            "template written with the deduped name"
        );

        // Duplicate a builtin.
        let bi = c
            .mgr
            .personas
            .iter()
            .position(|p| p.persona.builtin)
            .unwrap();
        c.mgr.cursor = bi;
        let base = c.mgr.personas[bi].persona.name.clone();
        c.handle_key(crate::ui::test_keys::key('d'));
        c.handle_key(crate::ui::test_keys::key('p'));
        let req = c.mgr.pending_editor.take().unwrap();
        assert_eq!(req.persona_name, format!("{base}-copy"));
        assert!(req.created && req.auto_enable);
        let copy = crate::engine::persona::parse_persona(
            &std::fs::read_to_string(&req.path).unwrap(),
            false,
        )
        .unwrap();
        assert_eq!(copy.name, format!("{base}-copy"));
    }

    #[test]
    fn scope_esc_cancels_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = reviewers_editing(dir.path());
        c.handle_key(crate::ui::test_keys::key('n'));
        c.handle_key(crate::ui::test_keys::key_code(KeyCode::Esc));
        assert!(c.mgr.scope_prompt.is_none() && c.mgr.pending_editor.is_none());
        assert!(
            !dir.path().join(".reviewal/personas").exists(),
            "nothing written on cancel"
        );
        assert!(c.editing, "checklist still open");
    }

    #[test]
    fn nonzero_exit_removes_only_created_files() {
        let dir = tempfile::tempdir().unwrap();
        let pdir = dir.path().join(".reviewal/personas");
        std::fs::create_dir_all(&pdir).unwrap();
        let pre_existing = pdir.join("skeptic.md");
        std::fs::write(&pre_existing, "user's broken edits").unwrap();
        let mut c = reviewers_editing(dir.path());

        // created:false (materialize-onto-existing / plain edit) → file survives :cq
        c.mgr.on_editor_return(
            EditorRequest {
                path: pre_existing.clone(),
                created: false,
                persona_name: "skeptic".into(),
                prior_enabled: Some(true),
                auto_enable: false,
            },
            false,
        );
        assert!(
            pre_existing.exists(),
            "nonzero exit must not delete a pre-existing file"
        );

        // created:true → cancelled create is removed
        let fresh = pdir.join("fresh.md");
        std::fs::write(
            &fresh,
            "+++\nname = \"fresh\"\ntitle = \"F\"\nlens = \"l\"\ntarget = \"both\"\n+++\nb",
        )
        .unwrap();
        c.mgr.on_editor_return(
            EditorRequest {
                path: fresh.clone(),
                created: true,
                persona_name: "fresh".into(),
                prior_enabled: None,
                auto_enable: true,
            },
            false,
        );
        assert!(!fresh.exists(), "cancelled create removed");
    }

    #[test]
    fn rename_carries_enabled_by_path_and_auto_enable_lands_on_new_name() {
        let dir = tempfile::tempdir().unwrap();
        let pdir = dir.path().join(".reviewal/personas");
        std::fs::create_dir_all(&pdir).unwrap();
        let mut c = reviewers_editing(dir.path());

        // Simulate `n` + the user renaming the template in the editor.
        let path = pdir.join("new-persona.md");
        std::fs::write(
            &path,
            "+++\nname = \"redteam\"\ntitle = \"R\"\nlens = \"l\"\ntarget = \"both\"\n+++\nb",
        )
        .unwrap();
        c.mgr.on_editor_return(
            EditorRequest {
                path: path.clone(),
                created: true,
                persona_name: "new-persona".into(),
                prior_enabled: None,
                auto_enable: true,
            },
            true,
        );
        assert!(!path.exists(), "file renamed to match frontmatter");
        assert!(pdir.join("redteam.md").exists());
        let row = c
            .mgr
            .personas
            .iter()
            .find(|x| x.persona.name == "redteam")
            .expect("loaded");
        assert!(row.enabled, "auto-enable lands on the post-rename persona");
        let idx = c
            .mgr
            .personas
            .iter()
            .position(|x| x.persona.name == "redteam")
            .unwrap();
        assert_eq!(c.mgr.cursor, idx, "cursor follows the edited persona");
    }

    #[test]
    fn rename_collision_keeps_filename_and_warns() {
        let dir = tempfile::tempdir().unwrap();
        let pdir = dir.path().join(".reviewal/personas");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("taken.md"),
            "+++\nname = \"taken\"\ntitle = \"T\"\nlens = \"l\"\ntarget = \"both\"\n+++\nb",
        )
        .unwrap();
        let mine = pdir.join("mine.md");
        std::fs::write(
            &mine,
            "+++\nname = \"taken\"\ntitle = \"M\"\nlens = \"l\"\ntarget = \"both\"\n+++\nb",
        )
        .unwrap();
        let mut c = reviewers_editing(dir.path());
        c.mgr.on_editor_return(
            EditorRequest {
                path: mine.clone(),
                created: false,
                persona_name: "mine".into(),
                prior_enabled: Some(false),
                auto_enable: false,
            },
            true,
        );
        assert!(mine.exists(), "taken.md exists → mine.md keeps its stem");
        assert!(
            c.mgr.warnings.iter().any(|w| w.contains("taken")),
            "warning names the collision: {:?}",
            c.mgr.warnings
        );
    }

    #[test]
    fn target_drift_warns_and_bad_edit_moves_cursor_to_invalid_row() {
        let dir = tempfile::tempdir().unwrap();
        let pdir = dir.path().join(".reviewal/personas");
        std::fs::create_dir_all(&pdir).unwrap();
        let path = pdir.join("codey.md");
        std::fs::write(
            &path,
            "+++\nname = \"codey\"\ntitle = \"C\"\nlens = \"l\"\ntarget = \"both\"\n+++\nb",
        )
        .unwrap();
        let mut c = reviewers_editing(dir.path());
        assert!(
            c.current_kind() == TargetKind::Spec,
            "no git targets in a bare tempdir"
        );

        // Edit flips target to code → filtered out of a spec run → warning, not silence.
        std::fs::write(
            &path,
            "+++\nname = \"codey\"\ntitle = \"C\"\nlens = \"l\"\ntarget = \"code\"\n+++\nb",
        )
        .unwrap();
        c.mgr.on_editor_return(
            EditorRequest {
                path: path.clone(),
                created: false,
                persona_name: "codey".into(),
                prior_enabled: Some(true),
                auto_enable: false,
            },
            true,
        );
        assert!(
            c.mgr
                .warnings
                .iter()
                .any(|w| w.contains("codey") && w.contains("hidden")),
            "{:?}",
            c.mgr.warnings
        );

        // Edit breaks the file → cursor lands on its invalid row.
        std::fs::write(&path, "no longer a persona").unwrap();
        c.mgr.on_editor_return(
            EditorRequest {
                path: path.clone(),
                created: false,
                persona_name: "codey".into(),
                prior_enabled: Some(true),
                auto_enable: false,
            },
            true,
        );
        let inv = c
            .mgr
            .invalid
            .iter()
            .position(|r| r.path == path)
            .expect("invalid row built");
        assert_eq!(
            c.mgr.cursor,
            c.mgr.personas.len() + inv,
            "cursor on the broken row so e re-opens it"
        );
    }

    #[test]
    fn x_arms_confirms_and_reset_to_builtin_resurfaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let pdir = dir.path().join(".reviewal/personas");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("skeptic.md"),
            "+++\nname = \"skeptic\"\ntitle = \"Mine\"\nlens = \"l\"\ntarget = \"both\"\n+++\nb",
        )
        .unwrap();
        let mut c = reviewers_editing(dir.path());
        let i = c
            .mgr
            .personas
            .iter()
            .position(|p| p.persona.name == "skeptic")
            .unwrap();
        assert!(!c.mgr.personas[i].persona.builtin, "custom file shadows");
        c.mgr.cursor = i;
        c.handle_key(crate::ui::test_keys::key('x'));
        assert_eq!(c.mgr.armed_delete, Some(i));
        let theme = crate::ui::theme::Theme::default();
        let text = crate::ui::app::render_to_text(120, 40, |f| draw(f, f.area(), &c, &theme));
        assert!(text.contains("reset") && text.contains("skeptic"), "{text}");
        c.handle_key(crate::ui::test_keys::key('x'));
        assert!(!pdir.join("skeptic.md").exists(), "file deleted");
        let back = c
            .mgr
            .personas
            .iter()
            .find(|p| p.persona.name == "skeptic")
            .unwrap();
        assert!(back.persona.builtin, "builtin resurfaced");
    }

    #[test]
    fn esc_while_armed_only_disarms_other_keys_disarm_then_act() {
        let dir = tempfile::tempdir().unwrap();
        let pdir = dir.path().join(".reviewal/personas");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(pdir.join("zz.md"), "junk").unwrap(); // deletable invalid row
        let mut c = reviewers_editing(dir.path());
        c.mgr.cursor = c.row_count() - 1; // the invalid row
        c.handle_key(crate::ui::test_keys::key('x'));
        assert!(c.mgr.armed_delete.is_some());
        c.handle_key(crate::ui::test_keys::key_code(KeyCode::Esc));
        assert!(c.mgr.armed_delete.is_none(), "esc disarms");
        assert!(c.editing, "…and is consumed: the checklist did NOT close");

        c.handle_key(crate::ui::test_keys::key('x'));
        let before = c.mgr.cursor;
        c.handle_key(crate::ui::test_keys::key('k'));
        assert!(c.mgr.armed_delete.is_none());
        assert_eq!(c.mgr.cursor, before - 1, "k still moved the cursor");

        // Confirm deletes the broken file.
        c.mgr.cursor = c.row_count() - 1;
        c.handle_key(crate::ui::test_keys::key('x'));
        c.handle_key(crate::ui::test_keys::key('x'));
        assert!(!pdir.join("zz.md").exists());
        assert!(c.mgr.invalid.is_empty());
    }

    #[test]
    fn x_on_pure_builtin_is_a_noop_with_notice() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = reviewers_editing(dir.path());
        c.mgr.cursor = 0;
        c.handle_key(crate::ui::test_keys::key('x'));
        assert!(c.mgr.armed_delete.is_none());
        assert_eq!(
            c.mgr.notice.as_deref(),
            Some("built-in \u{2014} e edits a copy")
        );
    }

    /// A Config carrying an injected global persona dir — the seam that
    /// makes global-dir behavior hermetically testable without touching the
    /// developer's real ~/.config/reviewal/personas.
    fn config_with_global(global: &Path) -> Config {
        Config {
            global_persona_dir: Some(global.to_path_buf()),
            ..Config::default()
        }
    }

    #[test]
    fn injected_global_dir_loads_personas_with_global_tag() {
        let dir = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        std::fs::write(
            global.path().join("housekeeper.md"),
            "+++\nname = \"housekeeper\"\ntitle = \"H\"\nlens = \"l\"\ntarget = \"both\"\n+++\nb",
        )
        .unwrap();
        let c = ComposerState::new(dir.path(), &config_with_global(global.path()), None, false);
        let row = c
            .mgr
            .personas
            .iter()
            .find(|p| p.persona.name == "housekeeper")
            .expect("persona from the injected global dir is loaded");
        assert_eq!(provenance_tag(&c.mgr, &row.persona), "global");
    }

    #[test]
    fn scope_g_writes_into_the_injected_global_dir() {
        let dir = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let mut c = ComposerState::new(dir.path(), &config_with_global(global.path()), None, false);
        c.field = Field::Reviewers;
        c.editing = true;
        c.handle_key(crate::ui::test_keys::key('n'));
        c.handle_key(crate::ui::test_keys::key('g'));
        let req = c.mgr.pending_editor.take().expect("template staged");
        assert!(
            req.path.starts_with(global.path()),
            "written under the injected global dir, not ambient env: {}",
            req.path.display()
        );
        assert!(req.path.exists());
    }

    #[test]
    fn arm_delete_detects_double_shadow_through_injected_global_dir() {
        let dir = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let body =
            "+++\nname = \"skeptic\"\ntitle = \"S\"\nlens = \"l\"\ntarget = \"both\"\n+++\nb";
        let pdir = dir.path().join(".reviewal/personas");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(pdir.join("skeptic.md"), body).unwrap();
        std::fs::write(global.path().join("skeptic.md"), body).unwrap();
        let mut c = ComposerState::new(dir.path(), &config_with_global(global.path()), None, false);
        c.field = Field::Reviewers;
        c.editing = true;
        let i = c
            .mgr
            .personas
            .iter()
            .position(|p| p.persona.name == "skeptic")
            .unwrap();
        assert!(
            c.mgr.personas[i]
                .persona
                .source
                .as_ref()
                .unwrap()
                .starts_with(&pdir),
            "project file wins the load over the global copy"
        );
        c.mgr.cursor = i;
        c.handle_key(crate::ui::test_keys::key('x'));
        assert!(
            c.mgr.armed_delete_shadows_global,
            "arm_delete's real stat found the global copy via the injected dir"
        );
    }

    // The double-shadow case is covered at three layers: the label wording
    // as a pure function, this render test with the bool forced (proving
    // the footer reads it), and the injected-global-dir test exercising
    // `arm_delete`'s real fs stat end-to-end.
    #[test]
    fn armed_footer_names_the_global_reveal_when_double_shadowed() {
        assert_eq!(
            armed_delete_label("skeptic", true),
            "again reveals the global copy of skeptic \u{2014} any other key cancels"
        );
        assert_eq!(
            armed_delete_label("skeptic", false),
            "again resets skeptic to built-in \u{2014} any other key cancels",
            "no global copy: still the plain builtin-reset wording"
        );

        // A project-dir file shadowing a builtin, armed for delete, with
        // `armed_delete_shadows_global` forced rather than fs-derived.
        let dir = tempfile::tempdir().unwrap();
        let pdir = dir.path().join(".reviewal/personas");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("skeptic.md"),
            "+++\nname = \"skeptic\"\ntitle = \"Mine\"\nlens = \"l\"\ntarget = \"both\"\n+++\nb",
        )
        .unwrap();
        let mut c = reviewers_editing(dir.path());
        let i = c
            .mgr
            .personas
            .iter()
            .position(|p| p.persona.name == "skeptic")
            .unwrap();
        c.mgr.cursor = i;
        c.handle_key(crate::ui::test_keys::key('x'));
        assert_eq!(c.mgr.armed_delete, Some(i), "Option<usize> shape unchanged");
        c.mgr.armed_delete_shadows_global = true; // forced, not fs-derived

        let theme = crate::ui::theme::Theme::default();
        let text = crate::ui::app::render_to_text(120, 40, |f| draw(f, f.area(), &c, &theme));
        assert!(
            text.contains("reveals the global copy of skeptic"),
            "footer must not promise a builtin reset when a global copy survives: {text}"
        );
        assert!(!text.contains("resets"), "{text}");
    }

    #[test]
    fn armed_footer_plain_custom_delete_label_is_untested_no_longer() {
        let dir = tempfile::tempdir().unwrap();
        let pdir = dir.path().join(".reviewal/personas");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("novel.md"),
            "+++\nname = \"novel\"\ntitle = \"N\"\nlens = \"l\"\ntarget = \"both\"\n+++\nb",
        )
        .unwrap();
        let mut c = reviewers_editing(dir.path());
        let i = c
            .mgr
            .personas
            .iter()
            .position(|p| p.persona.name == "novel")
            .unwrap();
        c.mgr.cursor = i;
        c.handle_key(crate::ui::test_keys::key('x'));
        assert_eq!(c.mgr.armed_delete, Some(i));
        assert!(
            !c.mgr.armed_delete_shadows_global,
            "novel isn't a builtin slot at all"
        );

        let theme = crate::ui::theme::Theme::default();
        let text = crate::ui::app::render_to_text(120, 40, |f| draw(f, f.area(), &c, &theme));
        assert!(
            text.contains("again deletes novel \u{2014} any other key cancels"),
            "{text}"
        );
        assert!(
            !text.contains("resets") && !text.contains("reveals"),
            "{text}"
        );
    }
}
