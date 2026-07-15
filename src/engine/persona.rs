use crate::engine::model::TargetKind;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PersonaTarget {
    Code,
    Spec,
    Both,
}

impl PersonaTarget {
    pub(crate) fn matches(&self, kind: TargetKind) -> bool {
        matches!(
            (self, kind),
            (PersonaTarget::Both, _)
                | (PersonaTarget::Code, TargetKind::Code)
                | (PersonaTarget::Spec, TargetKind::Spec)
        )
    }
}

/// A custom persona (loaded from a persona dir) overrides a builtin of the
/// same `name`.
#[derive(Debug, Clone)]
pub struct Persona {
    pub name: String,
    pub title: String,
    pub lens: String,
    pub target: PersonaTarget,
    pub system: String,
    pub builtin: bool,
    /// Raw frontmatter color, unvalidated — the engine never parses it into
    /// a UI type; `ui::theme` does, warning on invalid values.
    pub color: Option<String>,
    /// Path this persona was loaded from; `None` for builtins.
    pub source: Option<PathBuf>,
}

/// A persona-dir file that failed to read or parse. Structured so the
/// composer can render it as an invalid row; `Display` is the CLI's
/// "path: error" warning string.
#[derive(Debug, Clone, PartialEq)]
pub struct PersonaLoadError {
    pub path: PathBuf,
    pub error: String,
}

impl std::fmt::Display for PersonaLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.error)
    }
}

#[derive(Deserialize)]
struct FrontMatter {
    name: String,
    title: String,
    lens: String,
    target: PersonaTarget,
    color: Option<String>,
}

pub(crate) fn parse_persona(text: &str, builtin: bool) -> Result<Persona, String> {
    let rest = text
        .strip_prefix("+++")
        .ok_or("persona file must start with a +++ TOML frontmatter block")?;
    let (front, body) = rest
        .split_once("+++")
        .ok_or("unterminated +++ frontmatter block")?;
    let fm: FrontMatter = toml::from_str(front).map_err(|e| format!("invalid frontmatter: {e}"))?;
    // Defense-in-depth: the name is interpolated into on-disk filenames
    // (round1/<name>.json etc.), so reject anything that could escape the
    // run directory or produce a weird path.
    if fm.name.is_empty()
        || !fm
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "persona name {:?} must be a non-empty slug of [a-z0-9-_] (it becomes a filename)",
            fm.name
        ));
    }
    let system = body.trim().to_string();
    if system.is_empty() {
        return Err("persona body (system prompt) is empty".into());
    }
    Ok(Persona {
        name: fm.name,
        title: fm.title,
        lens: fm.lens,
        target: fm.target,
        system,
        builtin,
        color: fm.color,
        source: None,
    })
}

/// # Panics
///
/// Panics if a builtin persona asset fails to parse — a build-time bug that is
/// unreachable at runtime with the shipped assets.
///
/// # Examples
///
/// ```
/// let personas = reviewal::builtins();
/// assert!(!personas.is_empty());
/// assert!(personas.iter().all(|p| p.builtin));
/// ```
pub fn builtins() -> Vec<Persona> {
    BUILTIN_FILES
        .iter()
        .map(|(name, text)| {
            parse_persona(text, true)
                .unwrap_or_else(|e| panic!("builtin persona '{name}' failed to parse: {e}"))
        })
        .collect()
}

/// Later dirs override earlier ones by `name`. Failures are returned per-file
/// so one bad `*.md` never aborts the whole load.
pub fn load_custom(dirs: &[PathBuf]) -> (Vec<Persona>, Vec<PersonaLoadError>) {
    let (mut personas, mut failures) = (Vec::<Persona>::new(), Vec::new());
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut paths: Vec<_> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        paths.sort();
        for path in paths {
            match std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|t| parse_persona(&t, false))
            {
                Ok(mut p) => {
                    p.source = Some(path.clone());
                    personas.retain(|existing| existing.name != p.name);
                    personas.push(p);
                }
                Err(e) => failures.push(PersonaLoadError { path, error: e }),
            }
        }
    }
    (personas, failures)
}

/// Kept as raw `include_str!` text (not parsed values) so materialization
/// can write a byte-exact copy.
const BUILTIN_FILES: [(&str, &str); 6] = [
    ("prover", include_str!("../../assets/personas/prover.md")),
    (
        "breaker",
        include_str!("../../assets/personas/breaker.md"),
    ),
    (
        "steward",
        include_str!("../../assets/personas/steward.md"),
    ),
    ("skeptic", include_str!("../../assets/personas/skeptic.md")),
    (
        "stickler",
        include_str!("../../assets/personas/stickler.md"),
    ),
    (
        "advocate",
        include_str!("../../assets/personas/advocate.md"),
    ),
];

/// Byte-exact embedded source of the builtin named `name`, for materialization.
pub fn builtin_source(name: &str) -> Option<&'static str> {
    BUILTIN_FILES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, t)| *t)
}

/// Skeleton written by the composer's `n` key. Must parse as-is so saving
/// it unchanged in the editor cannot error.
pub const NEW_PERSONA_TEMPLATE: &str = r#"+++
name = "new-persona"
title = "New Persona"
lens = "One line: what this reviewer hunts for"
target = "both" # code | spec | both
# color = "cyan" # optional; named ANSI color for the checklist
+++
You are the **New Persona**, one reviewer in an adversarial review.

Describe this reviewer's lens: what it hunts for, what it deliberately
ignores (stay out of the other reviewers' lanes), and how it weighs
severity. Be specific — vague lenses produce vague findings.
"#;

/// Replaces the frontmatter `name` line, leaving everything else — comments,
/// field order, body — byte-identical.
pub fn rewrite_frontmatter_name(text: &str, new_name: &str) -> Result<String, String> {
    let rest = text
        .strip_prefix("+++")
        .ok_or("persona file must start with a +++ TOML frontmatter block")?;
    let (front, _body) = rest
        .split_once("+++")
        .ok_or("unterminated +++ frontmatter block")?;

    let is_name_line = |l: &str| {
        l.trim_start()
            .strip_prefix("name")
            .map(|r| r.trim_start().starts_with('='))
            .unwrap_or(false)
    };

    // Locate the name line by byte offset, not text content: a substring
    // `replacen` would also mutate an earlier line that happens to contain
    // the same literal text (e.g. a comment).
    let mut offset = 0usize;
    let mut found: Option<(usize, &str)> = None;
    for segment in front.split_inclusive('\n') {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        if is_name_line(line) {
            found = Some((offset, line));
            break;
        }
        offset += segment.len();
    }
    let (line_offset, name_line) = found.ok_or("frontmatter has no name field")?;

    let front_start = 3; // after the leading +++
    let name_start = front_start + line_offset;
    let name_end = name_start + name_line.len();

    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..name_start]);
    out.push_str(&format!("name = \"{new_name}\""));
    out.push_str(&text[name_end..]);
    Ok(out)
}

pub fn unique_slug(base: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == base) {
        return base.to_string();
    }
    let mut i = 2;
    loop {
        let cand = format!("{base}-{i}");
        if !taken.contains(&cand) {
            return cand;
        }
        i += 1;
    }
}

/// Load failures are returned unfiltered by `kind` — an unparseable file has
/// no knowable target, so it is always surfaced.
pub fn available(
    kind: TargetKind,
    custom_dirs: &[PathBuf],
) -> (Vec<Persona>, Vec<PersonaLoadError>) {
    let (custom, failures) = load_custom(custom_dirs);
    let mut personas = builtins();
    for c in custom {
        personas.retain(|p| p.name != c.name);
        personas.push(c);
    }
    personas.retain(|p| p.target.matches(kind));
    (personas, failures)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::model::TargetKind;

    #[test]
    fn frontmatter_color_round_trips_and_defaults_to_none() {
        let with = "+++\nname = \"redteam\"\ntitle = \"Red Team\"\nlens = \"attack\"\ntarget = \"both\"\ncolor = \"light red\"\n+++\nbody";
        let p = parse_persona(with, false).unwrap();
        assert_eq!(p.color.as_deref(), Some("light red"));

        let without =
            "+++\nname = \"plain\"\ntitle = \"Plain\"\nlens = \"l\"\ntarget = \"both\"\n+++\nbody";
        assert_eq!(parse_persona(without, false).unwrap().color, None);
    }

    #[test]
    fn builtins_load_six_personas() {
        let all = builtins();
        assert_eq!(all.len(), 6);
        let prover = all.iter().find(|p| p.name == "prover").unwrap();
        assert!(prover.system.contains("concrete failing input"));
        assert!(prover.target.matches(TargetKind::Code));
        assert!(!prover.target.matches(TargetKind::Spec));
        let skeptic = all.iter().find(|p| p.name == "skeptic").unwrap();
        assert!(skeptic.target.matches(TargetKind::Spec));
    }

    #[test]
    fn parse_persona_rejects_unsafe_name() {
        // The name becomes a filename — path separators and traversal must be
        // rejected at the parse boundary.
        assert!(parse_persona(
            "+++\nname = \"../evil\"\ntitle = \"t\"\nlens = \"l\"\ntarget = \"both\"\n+++\nbody",
            false
        )
        .is_err());
        assert!(parse_persona(
            "+++\nname = \"\"\ntitle = \"t\"\nlens = \"l\"\ntarget = \"both\"\n+++\nbody",
            false
        )
        .is_err());
    }

    #[test]
    fn parse_persona_rejects_missing_frontmatter() {
        assert!(parse_persona("no frontmatter here", false).is_err());
        assert!(parse_persona("+++\nname = \"x\"\n+++\nbody", false).is_err()); // missing keys
    }

    #[test]
    fn custom_dir_overrides_builtin_by_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("prover.md"),
            "+++\nname = \"prover\"\ntitle = \"My Prover\"\nlens = \"custom\"\ntarget = \"both\"\n+++\ncustom body",
        )
        .unwrap();
        std::fs::write(dir.path().join("broken.md"), "not a persona").unwrap();
        let (personas, warnings) = available(TargetKind::Code, &[dir.path().to_path_buf()]);
        let prover = personas.iter().find(|p| p.name == "prover").unwrap();
        assert_eq!(prover.title, "My Prover");
        assert!(!prover.builtin);
        assert_eq!(warnings.len(), 1);
        assert!(personas.iter().all(|p| p.target.matches(TargetKind::Code)));
    }

    #[test]
    fn load_custom_returns_structured_failures_and_source_paths() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("mine.md");
        std::fs::write(
            &good,
            "+++\nname = \"mine\"\ntitle = \"Mine\"\nlens = \"l\"\ntarget = \"both\"\n+++\nbody",
        )
        .unwrap();
        let bad = dir.path().join("broken.md");
        std::fs::write(&bad, "not a persona").unwrap();

        let (personas, failures) = load_custom(&[dir.path().to_path_buf()]);
        assert_eq!(personas.len(), 1);
        assert_eq!(personas[0].source.as_deref(), Some(good.as_path()));
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].path, bad);
        assert!(!failures[0].error.is_empty());
        assert_eq!(
            failures[0].to_string(),
            format!("{}: {}", bad.display(), failures[0].error)
        );
    }

    #[test]
    fn available_passes_failures_through_unfiltered_and_builtins_have_no_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.md"), "junk").unwrap();
        let (personas, failures) = available(TargetKind::Code, &[dir.path().to_path_buf()]);
        assert_eq!(failures.len(), 1, "failure survives the Code-kind filter");
        let (_, failures_spec) = available(TargetKind::Spec, &[dir.path().to_path_buf()]);
        assert_eq!(failures_spec.len(), 1, "and the Spec-kind filter");
        assert!(personas
            .iter()
            .filter(|p| p.builtin)
            .all(|p| p.source.is_none()));
    }

    #[test]
    fn builtin_source_is_byte_exact_embedded_text() {
        // Byte equality, not parse-equivalence: a re-serialized copy that drops
        // comments or reorders fields must fail this test.
        assert_eq!(
            builtin_source("prover").unwrap(),
            include_str!("../../assets/personas/prover.md")
        );
        assert!(builtin_source("no-such-persona").is_none());
        // Every builtin is reachable by the name its frontmatter declares.
        for p in builtins() {
            assert!(
                builtin_source(&p.name).is_some(),
                "missing source for {}",
                p.name
            );
        }
    }

    #[test]
    fn new_persona_template_round_trips() {
        let p = parse_persona(NEW_PERSONA_TEMPLATE, false).unwrap();
        assert_eq!(p.name, "new-persona");
        assert!(p.target.matches(TargetKind::Code) && p.target.matches(TargetKind::Spec));
    }

    #[test]
    fn rewrite_frontmatter_name_touches_only_the_frontmatter() {
        let text = "+++\nname = \"old\"\ntitle = \"T\"\nlens = \"l\"\ntarget = \"both\"\n# a comment\n+++\nbody mentions name = \"old\" too";
        let out = rewrite_frontmatter_name(text, "fresh").unwrap();
        let p = parse_persona(&out, false).unwrap();
        assert_eq!(p.name, "fresh");
        assert!(out.contains("# a comment"), "comments survive");
        assert!(
            out.contains("body mentions name = \"old\" too"),
            "body untouched"
        );
        assert!(rewrite_frontmatter_name("no frontmatter", "x").is_err());
    }

    #[test]
    fn rewrite_frontmatter_name_ignores_earlier_lines_with_matching_text() {
        // Only the real `name` field, found by position, may change — not the
        // earlier comment line containing the same literal text.
        let text = "+++\n# stale note: name = \"old\" was renamed last week\nname = \"old\"\ntitle = \"T\"\nlens = \"l\"\ntarget = \"both\"\n+++\nBODY";
        let out = rewrite_frontmatter_name(text, "fresh").unwrap();
        let p = parse_persona(&out, false).unwrap();
        assert_eq!(p.name, "fresh");
        assert!(
            out.contains("# stale note: name = \"old\" was renamed last week"),
            "comment must remain unchanged, got: {out}"
        );
        assert!(out.ends_with("+++\nBODY"), "body must be untouched");
    }

    #[test]
    fn rewrite_frontmatter_name_errors_when_no_name_field() {
        let text = "+++\ntitle = \"T\"\nlens = \"l\"\ntarget = \"both\"\n+++\nbody";
        let err = rewrite_frontmatter_name(text, "fresh").unwrap_err();
        assert!(err.contains("no name field"), "unexpected error: {err}");
    }

    #[test]
    fn unique_slug_appends_numeric_suffix() {
        let taken = vec!["a".to_string(), "a-2".to_string()];
        assert_eq!(unique_slug("a", &taken), "a-3");
        assert_eq!(unique_slug("b", &taken), "b");
    }
}
