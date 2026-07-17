//! Shared persona-management component: the state and verbs behind both the
//! composer's reviewer checklist and the home screen's personas tab. The
//! wrappers own movement, layout, and their screen-specific keys (the
//! composer's enable toggle, the home tab bar); everything that touches
//! persona files — view, edit, create, duplicate, delete, and the `$EDITOR`
//! round-trip — lives here so the two screens cannot drift apart.

use crate::engine::model::TargetKind;
use crate::engine::persona::{available, available_all, Persona};
use crate::ui::theme::{Theme, BUILTIN_SLOTS};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;
use std::path::{Path, PathBuf};

pub(crate) struct PersonaChoice {
    pub persona: Persona,
    pub enabled: bool,
}

/// One unparseable persona file: row `personas.len() + index`.
pub(crate) struct InvalidRow {
    pub path: PathBuf,
    pub error: String,
}

pub(crate) struct PagerState {
    pub title: String,
    pub text: String,
    pub scroll: u16,
}

/// `created` is true ONLY if the staging operation itself wrote the file —
/// materialize onto an existing path stages false, so a nonzero editor
/// exit can never delete a pre-existing user file.
#[derive(Debug, PartialEq)]
pub(crate) struct EditorRequest {
    pub path: PathBuf,
    pub created: bool,
    pub persona_name: String,
    /// Enabled flag to reapply by path after reload (edit/materialize).
    pub prior_enabled: Option<bool>,
    /// `n`/`d`: enable the resulting persona on successful return.
    pub auto_enable: bool,
}

pub(crate) enum ScopeOp {
    Materialize { name: String },
    New,
    Duplicate { row: usize },
}

/// A custom persona shadowing a builtin (same name as one of
/// [`BUILTIN_SLOTS`]) stays default-ON, same as the builtin it replaces; a
/// non-shadowing custom persona defaults off.
pub(crate) fn default_enabled(p: &Persona) -> bool {
    BUILTIN_SLOTS.contains(&p.name.as_str())
}

pub(crate) struct PersonaManager {
    pub root: PathBuf,
    /// Injected from [`Config::persona_dirs`] at construction — this state
    /// never reads ambient environment, which is what keeps every test
    /// hermetic with a `Config::default()`.
    pub persona_dirs: Vec<PathBuf>,
    /// Injected from [`Config`], never ambient env; `None` under
    /// `Config::default()`.
    pub global_persona_dir: Option<PathBuf>,
    /// `Some(kind)` = the composer checklist, filtered to the run's target;
    /// `None` = the whole library (home personas tab).
    pub filter: Option<TargetKind>,
    pub personas: Vec<PersonaChoice>,
    pub invalid: Vec<InvalidRow>,
    pub cursor: usize,
    pub pager: Option<PagerState>,
    /// A file the `run_tui` loop must open in $EDITOR after this keypress.
    pub pending_editor: Option<EditorRequest>,
    pub scope_prompt: Option<ScopeOp>,
    /// One-shot message for the wrapper's notice row; the wrapper clears it
    /// at the top of its key handling.
    pub notice: Option<String>,
    pub warnings: Vec<String>,
    /// The row awaiting a confirming `x`: `Some(row)` while armed. Any key
    /// other than a matching `x` or `Esc` disarms it (and, except for
    /// `Esc`, still acts normally).
    pub armed_delete: Option<usize>,
    /// True when the armed row shadows BOTH a builtin slot AND a global
    /// copy: deleting resurfaces the global copy, not the builtin, so the
    /// footer must say so. Computed at arm time so tests can force it.
    pub armed_delete_shadows_global: bool,
}

impl PersonaManager {
    pub(crate) fn new(
        root: &Path,
        config: &crate::config::Config,
        filter: Option<TargetKind>,
    ) -> Self {
        let mut mgr = PersonaManager {
            root: root.to_path_buf(),
            persona_dirs: config.persona_dirs(root),
            global_persona_dir: config.global_persona_dir.clone(),
            filter,
            personas: Vec::new(),
            invalid: Vec::new(),
            cursor: 0,
            pager: None,
            pending_editor: None,
            scope_prompt: None,
            notice: None,
            warnings: Vec::new(),
            armed_delete: None,
            armed_delete_shadows_global: false,
        };
        // With `personas` still empty, rebuild's by-name preservation map is
        // empty, so every persona falls through to `default_enabled` — the
        // seeding behavior construction needs.
        mgr.rebuild();
        mgr
    }

    /// Re-populates `personas` for the current filter, preserving each
    /// persona's `enabled` by name and using [`default_enabled`] only for
    /// newly-appearing names.
    pub(crate) fn rebuild(&mut self) {
        let (personas, failures) = match self.filter {
            Some(kind) => available(kind, &self.persona_dirs),
            None => available_all(&self.persona_dirs),
        };
        let mut warnings: Vec<String> = failures.iter().map(|f| f.to_string()).collect();
        warnings.extend(crate::ui::theme::validate_persona_colors(&personas));
        self.warnings = warnings;
        self.invalid = failures
            .into_iter()
            .map(|f| InvalidRow {
                path: f.path,
                error: f.error,
            })
            .collect();
        let prev: std::collections::HashMap<String, bool> = self
            .personas
            .iter()
            .map(|c| (c.persona.name.clone(), c.enabled))
            .collect();
        self.personas = personas
            .into_iter()
            .map(|p| {
                let enabled = prev
                    .get(&p.name)
                    .copied()
                    .unwrap_or_else(|| default_enabled(&p));
                PersonaChoice {
                    enabled,
                    persona: p,
                }
            })
            .collect();
        self.cursor = self.cursor.min(self.row_count().saturating_sub(1));
    }

    pub(crate) fn rebuild_for(&mut self, kind: TargetKind) {
        self.filter = Some(kind);
        self.rebuild();
    }

    /// Total rows: valid personas, then invalid files.
    pub(crate) fn row_count(&self) -> usize {
        self.personas.len() + self.invalid.len()
    }

    pub(crate) fn move_cursor(&mut self, delta: i32) {
        let last = self.row_count().saturating_sub(1) as i32;
        self.cursor = (self.cursor as i32 + delta).clamp(0, last) as usize;
    }

    /// Pager scroll/close keys. Call only while `pager` is open; always
    /// consumes the key.
    pub(crate) fn handle_pager_key(&mut self, key: KeyEvent) {
        let Some(p) = &mut self.pager else {
            return;
        };
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                let max = p.text.lines().count().saturating_sub(1) as u16;
                p.scroll = p.scroll.saturating_add(1).min(max);
            }
            KeyCode::Char('k') | KeyCode::Up => p.scroll = p.scroll.saturating_sub(1),
            KeyCode::Esc | KeyCode::Char('v') => self.pager = None,
            _ => {}
        }
    }

    /// Armed-delete grammar: `x` on the armed row confirms, `esc` is
    /// consumed and only disarms; any other key disarms and must then act
    /// normally in the wrapper. Returns `true` when the key was consumed.
    pub(crate) fn handle_armed_key(&mut self, key: KeyEvent) -> bool {
        let Some(armed) = self.armed_delete.take() else {
            return false;
        };
        self.armed_delete_shadows_global = false;
        match key.code {
            KeyCode::Char('x') if armed == self.cursor => {
                self.confirm_delete(armed);
                true
            }
            KeyCode::Esc => true,
            _ => false,
        }
    }

    /// The management verbs (`v` view, `e` edit, `n` new, `d` duplicate,
    /// `x` delete). Returns `true` when the key was one of them; wrappers
    /// gate the call on their own context first.
    pub(crate) fn handle_verb_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('v') => self.open_pager(),
            KeyCode::Char('e') => self.begin_edit(),
            KeyCode::Char('n') => self.scope_prompt = Some(ScopeOp::New),
            KeyCode::Char('d') => {
                if self.cursor < self.personas.len() {
                    self.scope_prompt = Some(ScopeOp::Duplicate { row: self.cursor });
                }
            }
            KeyCode::Char('x') => self.arm_delete(),
            _ => return false,
        }
        true
    }

    /// Read-only source of the highlighted row — builtin embedded text or
    /// the file's contents. Never writes.
    pub(crate) fn open_pager(&mut self) {
        let i = self.cursor;
        let (title, text) = if let Some(c) = self.personas.get(i) {
            let text = match &c.persona.source {
                None => match crate::engine::persona::builtin_source(&c.persona.name) {
                    Some(t) => t.to_string(),
                    None => return,
                },
                Some(path) => match std::fs::read_to_string(path) {
                    Ok(t) => t,
                    Err(e) => {
                        self.notice = Some(format!("{}: {e}", path.display()));
                        return;
                    }
                },
            };
            (
                format!(
                    "{} \u{2014} {}",
                    c.persona.name,
                    provenance_tag(self, &c.persona)
                ),
                text,
            )
        } else if let Some(row) = self.invalid.get(i.saturating_sub(self.personas.len())) {
            match std::fs::read_to_string(&row.path) {
                Ok(t) => (
                    format!(
                        "{} \u{2014} {}",
                        row.path.display(),
                        invalid_tag(self, &row.path)
                    ),
                    t,
                ),
                Err(e) => {
                    self.notice = Some(format!("{}: {e}", row.path.display()));
                    return;
                }
            }
        } else {
            return;
        };
        self.pager = Some(PagerState {
            title,
            text,
            scroll: 0,
        });
    }

    /// `e`: existing files open directly; a builtin prompts for scope and
    /// materializes (write-if-absent) once a directory is chosen.
    pub(crate) fn begin_edit(&mut self) {
        let i = self.cursor;
        if let Some(c) = self.personas.get(i) {
            match &c.persona.source {
                Some(path) => {
                    self.pending_editor = Some(EditorRequest {
                        path: path.clone(),
                        created: false,
                        persona_name: c.persona.name.clone(),
                        prior_enabled: Some(c.enabled),
                        auto_enable: false,
                    });
                }
                None => {
                    self.scope_prompt = Some(ScopeOp::Materialize {
                        name: c.persona.name.clone(),
                    });
                }
            }
        } else if let Some(row) = self.invalid.get(i.saturating_sub(self.personas.len())) {
            let stem = row
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.pending_editor = Some(EditorRequest {
                path: row.path.clone(),
                created: false,
                persona_name: stem,
                prior_enabled: None,
                auto_enable: false,
            });
        }
    }

    /// Arms the highlighted row for delete, unless it's a pure builtin (no
    /// on-disk file to delete) — those get a notice instead.
    pub(crate) fn arm_delete(&mut self) {
        let i = self.cursor;
        if let Some(c) = self.personas.get(i) {
            if c.persona.source.is_some() {
                self.armed_delete_shadows_global = shadows_global_copy(self, &c.persona);
                self.armed_delete = Some(i);
            } else {
                self.notice = Some("built-in \u{2014} e edits a copy".into());
            }
        } else if i < self.row_count() {
            self.armed_delete_shadows_global = false; // invalid rows never shadow
            self.armed_delete = Some(i); // invalid rows always have a file
        }
    }

    /// Confirmed `x`: deletes the row's on-disk file. A custom persona
    /// shadowing a builtin resets to the builtin (it just reappears after
    /// the rebuild); a non-shadowing custom or invalid file is gone for good.
    fn confirm_delete(&mut self, row: usize) {
        let path = if let Some(c) = self.personas.get(row) {
            c.persona.source.clone()
        } else {
            self.invalid
                .get(row.saturating_sub(self.personas.len()))
                .map(|r| r.path.clone())
        };
        let Some(path) = path else { return };
        match std::fs::remove_file(&path) {
            Ok(()) => self.rebuild(),
            Err(e) => self.notice = Some(format!("delete failed: {e}")),
        }
    }

    /// Scope-overlay keys (`p`/`g`/`esc`). Call only while `scope_prompt`
    /// is open; always consumes the key.
    pub(crate) fn handle_scope_key(&mut self, key: KeyEvent) {
        let dir = match key.code {
            KeyCode::Char('p') => self.root.join(".reviewal").join("personas"),
            KeyCode::Char('g') => match self.global_persona_dir.clone() {
                Some(d) => d,
                None => {
                    self.scope_prompt = None;
                    self.notice = Some("no global config directory available".into());
                    return;
                }
            },
            KeyCode::Esc => {
                self.scope_prompt = None;
                return;
            }
            _ => return,
        };
        let Some(op) = self.scope_prompt.take() else {
            return;
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.notice = Some(format!("cannot create {}: {e}", dir.display()));
            return;
        }
        self.perform_scope_op(op, &dir);
    }

    fn taken_slugs(&self, dir: &Path) -> Vec<String> {
        let mut taken: Vec<String> = self
            .personas
            .iter()
            .map(|c| c.persona.name.clone())
            .collect();
        taken.extend(
            self.invalid
                .iter()
                .filter_map(|r| r.path.file_stem().map(|s| s.to_string_lossy().into_owned())),
        );
        if let Ok(entries) = std::fs::read_dir(dir) {
            taken.extend(entries.filter_map(|e| {
                let p = e.ok()?.path();
                (p.extension()? == "md").then(|| {
                    p.file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                })
            }));
        }
        taken
    }

    fn perform_scope_op(&mut self, op: ScopeOp, dir: &Path) {
        use crate::engine::persona::{
            builtin_source, rewrite_frontmatter_name, unique_slug, NEW_PERSONA_TEMPLATE,
        };
        match op {
            ScopeOp::Materialize { name } => {
                let path = dir.join(format!("{name}.md"));
                let created = if path.exists() {
                    false // open the existing (possibly broken) shadow as-is — never overwrite
                } else {
                    let Some(src) = builtin_source(&name) else {
                        self.notice = Some(format!("no builtin source for {name}"));
                        return;
                    };
                    if let Err(e) = std::fs::write(&path, src) {
                        self.notice = Some(format!("cannot write {}: {e}", path.display()));
                        return;
                    }
                    true
                };
                let prior = self
                    .personas
                    .iter()
                    .find(|c| c.persona.name == name)
                    .map(|c| c.enabled);
                self.pending_editor = Some(EditorRequest {
                    path,
                    created,
                    persona_name: name,
                    prior_enabled: prior,
                    auto_enable: false,
                });
            }
            ScopeOp::New => {
                let slug = unique_slug("new-persona", &self.taken_slugs(dir));
                let text = match rewrite_frontmatter_name(NEW_PERSONA_TEMPLATE, &slug) {
                    Ok(t) => t,
                    Err(e) => {
                        self.notice = Some(format!("template error: {e}"));
                        return;
                    }
                };
                let path = dir.join(format!("{slug}.md"));
                // taken_slugs() is not a freshness check: case-insensitive
                // filesystems can hide a same-named file from the string
                // match, and its read_dir errors are swallowed. Re-check the
                // exact path so a real file is never clobbered (its
                // `created: true` staging would delete it on a nonzero
                // editor exit).
                if path.exists() {
                    self.notice = Some(format!(
                        "{} already exists — not overwriting",
                        path.display()
                    ));
                    return;
                }
                if let Err(e) = std::fs::write(&path, text) {
                    self.notice = Some(format!("cannot write {}: {e}", path.display()));
                    return;
                }
                self.pending_editor = Some(EditorRequest {
                    path,
                    created: true,
                    persona_name: slug,
                    prior_enabled: None,
                    auto_enable: true,
                });
            }
            ScopeOp::Duplicate { row } => {
                let Some(c) = self.personas.get(row) else {
                    return;
                };
                let src_text = match &c.persona.source {
                    None => match builtin_source(&c.persona.name) {
                        Some(t) => t.to_string(),
                        None => return,
                    },
                    Some(p) => match std::fs::read_to_string(p) {
                        Ok(t) => t,
                        Err(e) => {
                            self.notice = Some(format!("{}: {e}", p.display()));
                            return;
                        }
                    },
                };
                let slug = unique_slug(&format!("{}-copy", c.persona.name), &self.taken_slugs(dir));
                let text = match rewrite_frontmatter_name(&src_text, &slug) {
                    Ok(t) => t,
                    Err(e) => {
                        self.notice = Some(format!("cannot rewrite name: {e}"));
                        return;
                    }
                };
                let path = dir.join(format!("{slug}.md"));
                // Same guard as ScopeOp::New: re-check the exact path before
                // writing.
                if path.exists() {
                    self.notice = Some(format!(
                        "{} already exists — not overwriting",
                        path.display()
                    ));
                    return;
                }
                if let Err(e) = std::fs::write(&path, text) {
                    self.notice = Some(format!("cannot write {}: {e}", path.display()));
                    return;
                }
                self.pending_editor = Some(EditorRequest {
                    path,
                    created: true,
                    persona_name: slug,
                    prior_enabled: None,
                    auto_enable: true,
                });
            }
        }
    }

    /// Post-edit pipeline: cleanup on cancelled creates, rename to match the
    /// frontmatter name, reload, then reapply identity by PATH (never by
    /// name — the user may have renamed in the same round-trip).
    pub(crate) fn on_editor_return(&mut self, req: EditorRequest, exit_ok: bool) {
        if !exit_ok && req.created {
            let _ = std::fs::remove_file(&req.path);
            self.rebuild();
            return;
        }

        // Rename pass: the frontmatter name is authoritative for the stem.
        let mut path = req.path.clone();
        let mut parsed: Option<(String, crate::engine::persona::PersonaTarget)> = None;
        let mut extra_warnings: Vec<String> = Vec::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(p) = crate::engine::persona::parse_persona(&text, false) {
                let stem = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if p.name != stem {
                    let dest = path.with_file_name(format!("{}.md", p.name));
                    if dest.exists() {
                        extra_warnings.push(format!(
                            "{}.md already exists — file kept as {}",
                            p.name,
                            path.display()
                        ));
                    } else if std::fs::rename(&path, &dest).is_ok() {
                        path = dest;
                    }
                }
                // Collision warnings, computed against the pre-rebuild rows.
                if p.name != req.persona_name {
                    if BUILTIN_SLOTS.contains(&p.name.as_str()) {
                        extra_warnings.push(format!("{} now shadows the built-in", p.name));
                    }
                    if let Some(other) = self.personas.iter().find(|c| {
                        c.persona.name == p.name
                            && c.persona.source.as_ref().is_some_and(|s| *s != path)
                    }) {
                        extra_warnings.push(format!(
                            "two personas named {}: {} and {} — load order decides which wins",
                            p.name,
                            other
                                .persona
                                .source
                                .as_ref()
                                .map(|s| s.display().to_string())
                                .unwrap_or_default(),
                            path.display(),
                        ));
                    }
                }
                parsed = Some((p.name, p.target));
            }
        }

        self.rebuild();
        self.warnings.extend(extra_warnings);

        // Identity by path: rebuild's by-name preservation missed a renamed
        // persona, so reapply the flag (or the n/d auto-enable) here.
        if let Some(idx) = self
            .personas
            .iter()
            .position(|c| c.persona.source.as_deref() == Some(path.as_path()))
        {
            if req.auto_enable {
                self.personas[idx].enabled = true;
            } else if let Some(en) = req.prior_enabled {
                self.personas[idx].enabled = en;
            }
            self.cursor = idx;
        } else if let Some(inv) = self.invalid.iter().position(|r| r.path == path) {
            // The edit broke the file: land on its row so `e` re-opens it.
            self.cursor = self.personas.len() + inv;
        } else if let Some((name, target)) = parsed {
            // Parsed fine but filtered out of this view: target drift.
            let now = match target {
                crate::engine::persona::PersonaTarget::Code => "code",
                crate::engine::persona::PersonaTarget::Spec => "spec",
                crate::engine::persona::PersonaTarget::Both => "both",
            };
            self.warnings
                .push(format!("{name} now targets {now} — hidden for this run"));
        }
    }
}

pub(crate) fn provenance_tag(mgr: &PersonaManager, p: &Persona) -> String {
    let Some(path) = &p.source else {
        return "built-in".to_string();
    };
    let dir = source_dir_label(mgr, path);
    if BUILTIN_SLOTS.contains(&p.name.as_str()) {
        format!("edited ({dir})")
    } else {
        dir.to_string()
    }
}

pub(crate) fn invalid_tag(mgr: &PersonaManager, path: &Path) -> String {
    format!("invalid ({})", source_dir_label(mgr, path))
}

/// `project` when the file lives under `<root>/.reviewal/personas`, else
/// `global` — a pure path predicate so tests need no env vars.
fn source_dir_label(mgr: &PersonaManager, path: &Path) -> &'static str {
    if path.starts_with(mgr.root.join(".reviewal").join("personas")) {
        "project"
    } else {
        "global"
    }
}

/// Only project-source rows need the stat: a loaded GLOBAL-source row can
/// never be shadowed by a project file, since `load_custom` lets later dirs
/// win. Stats the injected `global_persona_dir`, never ambient env.
pub(crate) fn shadows_global_copy(mgr: &PersonaManager, p: &Persona) -> bool {
    if !BUILTIN_SLOTS.contains(&p.name.as_str()) {
        return false;
    }
    let Some(source) = &p.source else {
        return false;
    };
    if !source.starts_with(mgr.root.join(".reviewal").join("personas")) {
        return false;
    }
    mgr.global_persona_dir
        .as_ref()
        .is_some_and(|d| d.join(format!("{}.md", p.name)).is_file())
}

/// `global_copy_exists` must already be gated to project-source,
/// builtin-slot rows by [`shadows_global_copy`]; this function stays pure so
/// it can be unit-tested without touching the filesystem or environment.
pub(crate) fn armed_delete_label(name: &str, global_copy_exists: bool) -> String {
    if global_copy_exists {
        format!("again reveals the global copy of {name} \u{2014} any other key cancels")
    } else if BUILTIN_SLOTS.contains(&name) {
        format!("again resets {name} to built-in \u{2014} any other key cancels")
    } else {
        format!("again deletes {name} \u{2014} any other key cancels")
    }
}

/// The `v` pager: full source of the highlighted persona, centered modal.
pub(crate) fn draw_pager(f: &mut Frame, area: Rect, mgr: &PersonaManager, theme: &Theme) {
    let Some(p) = &mgr.pager else { return };
    let rect = crate::ui::overlay::centered(area, 80, area.height.saturating_sub(4).max(5));
    f.render_widget(Clear, rect);
    let block = theme
        .panel(&p.title, true)
        .title_bottom(
            theme
                .hints(&[("j/k", "scroll"), ("esc", "close")])
                .right_aligned(),
        )
        .border_style(theme.accent_style());
    f.render_widget(
        Paragraph::new(p.text.as_str())
            .block(block)
            .scroll((p.scroll, 0)),
        rect,
    );
}

/// The `[p]/[g]` scope prompt for materialize/new/duplicate.
pub(crate) fn draw_scope(f: &mut Frame, area: Rect, mgr: &PersonaManager, theme: &Theme) {
    let Some(op) = &mgr.scope_prompt else {
        return;
    };
    let title = match op {
        ScopeOp::Materialize { name } => format!("copy {name} to:"),
        ScopeOp::New => "new persona in:".to_string(),
        ScopeOp::Duplicate { row } => format!(
            "copy {} to:",
            mgr.personas
                .get(*row)
                .map(|c| c.persona.name.as_str())
                .unwrap_or("persona")
        ),
    };
    let rect = crate::ui::overlay::centered(area, 50, 5);
    f.render_widget(Clear, rect);
    let lines = vec![
        Line::from(vec![
            Span::styled("p", theme.accent_style()),
            Span::styled("  project   .reviewal/personas/", theme.dim_style()),
        ]),
        Line::from(vec![
            Span::styled("g", theme.accent_style()),
            Span::styled(
                "  global    ~/.config/reviewal/personas/",
                theme.dim_style(),
            ),
        ]),
        theme.hints(&[("esc", "cancel")]),
    ];
    f.render_widget(Paragraph::new(lines).block(theme.panel(&title, true)), rect);
}
