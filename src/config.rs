use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    pub model: Option<String>,
    pub timeout_secs: u64,
    pub claude_bin: String,
    /// Populated only by [`load`] — the one place ambient environment
    /// (XDG_CONFIG_HOME/HOME) is read. Everything downstream receives it
    /// through this field, so a `Config::default()` (no global dir) makes
    /// tests hermetic by construction instead of by env manipulation. Not a
    /// config.toml key: it derives from where the config itself lives.
    pub global_persona_dir: Option<PathBuf>,
    pub(crate) theme: ThemeOverrides,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            model: None,
            timeout_secs: 600,
            claude_bin: "claude".into(),
            global_persona_dir: None,
            theme: ThemeOverrides::default(),
        }
    }
}

impl Config {
    /// Persona directories in load order — later dirs win on name collision.
    pub fn persona_dirs(&self, project_root: &Path) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Some(g) = &self.global_persona_dir {
            dirs.push(g.clone());
        }
        dirs.push(project_root.join(".reviewal/personas"));
        dirs
    }
}

// The color-role list is generated from `theme.rs`'s `for_each_color_role!`
// table, so adding a role there adds the field here automatically.
macro_rules! declare_theme_overrides {
    ($($role:ident: $default:ident),* $(,)?) => {
        /// Raw `[theme]` values; parsing into ratatui colors happens in
        /// `ui::theme::Theme::load` so this module stays UI-free. Unknown keys
        /// reject the whole file with a warning, so a typo'd role name cannot
        /// silently do nothing.
        #[derive(Debug, Clone, Default, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub(crate) struct ThemeOverrides {
            $(pub $role: Option<String>,)*
            pub persona_pool: Option<Vec<String>>,
        }

        /// The [theme] table merges per-field across the global → project layers.
        /// Replacing the whole sub-struct would silently drop global overrides the
        /// project file doesn't mention.
        fn apply_theme(t: &mut ThemeOverrides, p: ThemeOverrides) {
            $( if p.$role.is_some() { t.$role = p.$role; } )*
            if p.persona_pool.is_some() {
                t.persona_pool = p.persona_pool;
            }
        }
    };
}
crate::ui::theme::for_each_color_role!(declare_theme_overrides);

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Partial {
    model: Option<String>,
    timeout_secs: Option<u64>,
    claude_bin: Option<String>,
    theme: Option<ThemeOverrides>,
}

fn read_partial(path: Option<&Path>, warnings: &mut Vec<String>) -> Partial {
    let Some(p) = path else {
        return Partial::default();
    };
    let text = match std::fs::read_to_string(p) {
        Ok(t) => t,
        // Absent config is the normal case — stay silent.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Partial::default(),
        Err(e) => {
            warnings.push(format!("{}: {e}", p.display()));
            return Partial::default();
        }
    };
    match toml::from_str(&text) {
        Ok(partial) => partial,
        Err(e) => {
            warnings.push(format!("{}: {e}", p.display()));
            Partial::default()
        }
    }
}

fn apply(config: &mut Config, partial: Partial, warnings: &mut Vec<String>) {
    if partial.model.is_some() {
        config.model = partial.model;
    }
    match partial.timeout_secs {
        Some(0) => warnings.push(format!(
            "timeout_secs must be greater than 0; keeping {}s",
            config.timeout_secs
        )),
        Some(t) => config.timeout_secs = t,
        None => {}
    }
    if let Some(b) = partial.claude_bin {
        config.claude_bin = b;
    }
    if let Some(t) = partial.theme {
        apply_theme(&mut config.theme, t);
    }
}

pub(crate) fn load_from(global: Option<&Path>, project: Option<&Path>) -> (Config, Vec<String>) {
    let mut config = Config::default();
    let mut warnings = Vec::new();
    let g = read_partial(global, &mut warnings);
    apply(&mut config, g, &mut warnings);
    let mut p = read_partial(project, &mut warnings);
    // Security boundary: the project layer ships with the reviewed checkout, so a
    // hostile repo must not be able to choose which binary reviewal executes.
    // claude_bin is honored only from the user-owned global config.
    if p.claude_bin.take().is_some() {
        let loc = project.map(Path::display);
        warnings.push(format!(
            "{}: ignoring claude_bin — only the global config may set the executable",
            loc.map(|d| d.to_string()).unwrap_or_default()
        ));
    }
    apply(&mut config, p, &mut warnings);
    (config, warnings)
}

fn global_config_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("reviewal"));
        }
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/reviewal"))
}

/// Loads the layered config. This is the composition root for ambient
/// environment: XDG_CONFIG_HOME/HOME are resolved here (config files AND
/// `global_persona_dir`) and nowhere else — UI and engine code must take
/// persona directories from [`Config::persona_dirs`], never re-read env.
pub fn load(project_root: &Path) -> (Config, Vec<String>) {
    let global_dir = global_config_dir();
    let global = global_dir.as_ref().map(|d| d.join("config.toml"));
    let project = project_root.join(".reviewal/config.toml");
    let (mut config, warnings) = load_from(global.as_deref(), Some(&project));
    config.global_persona_dir = global_dir.map(|d| d.join("personas"));
    (config, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_hermetic_no_global_persona_dir() {
        // persona_dirs is a pure function of the struct + project root, so
        // tests built on a default Config are hermetic by construction.
        let c = Config::default();
        assert!(c.global_persona_dir.is_none());
        let root = Path::new("/proj");
        assert_eq!(c.persona_dirs(root), vec![root.join(".reviewal/personas")]);
    }

    #[test]
    fn persona_dirs_orders_global_before_project() {
        let c = Config {
            global_persona_dir: Some(PathBuf::from("/g/personas")),
            ..Config::default()
        };
        assert_eq!(
            c.persona_dirs(Path::new("/p")),
            vec![
                PathBuf::from("/g/personas"),
                Path::new("/p").join(".reviewal/personas")
            ],
            "global first, project last — later dirs win on name collision"
        );
    }

    #[test]
    fn defaults_when_no_files() {
        let (c, warnings) = load_from(None, None);
        assert_eq!(c.timeout_secs, 600);
        assert_eq!(c.claude_bin, "claude");
        assert_eq!(c.model, None);
        assert!(warnings.is_empty());
    }

    #[test]
    fn theme_table_merges_per_field_not_whole_table() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let project = dir.path().join("project.toml");
        std::fs::write(&global, "[theme]\ndim = \"gray\"\naccent = \"magenta\"\n").unwrap();
        std::fs::write(&project, "[theme]\naccent = \"cyan\"\n").unwrap();
        let (c, _) = load_from(Some(&global), Some(&project));
        assert_eq!(
            c.theme.accent.as_deref(),
            Some("cyan"),
            "project wins same key"
        );
        assert_eq!(
            c.theme.dim.as_deref(),
            Some("gray"),
            "global key survives a project table that omits it — whole-table replace is a bug"
        );
        assert_eq!(c.theme.error, None, "unset keys stay None");
    }

    #[test]
    fn project_overrides_global_and_bad_toml_warns() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let project = dir.path().join("project.toml");
        std::fs::write(&global, "model = \"opus\"\ntimeout_secs = 120\n").unwrap();
        std::fs::write(&project, "model = \"sonnet\"\n").unwrap();
        let (c, warnings) = load_from(Some(&global), Some(&project));
        assert_eq!(c.model.as_deref(), Some("sonnet"));
        assert_eq!(c.timeout_secs, 120);
        assert!(warnings.is_empty());

        std::fs::write(&project, "{{{{ not toml").unwrap();
        let (c, warnings) = load_from(Some(&global), Some(&project));
        assert_eq!(c.model.as_deref(), Some("opus"));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("project.toml"));
    }

    #[test]
    fn timeout_zero_keeps_default_and_warns() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project.toml");
        std::fs::write(&project, "timeout_secs = 0\n").unwrap();
        let (c, warnings) = load_from(None, Some(&project));
        assert_eq!(
            c.timeout_secs, 600,
            "0 must be rejected and the default kept"
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("timeout_secs"));
    }

    #[test]
    fn project_config_cannot_override_claude_bin() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let project = dir.path().join("project.toml");
        std::fs::write(&global, "claude_bin = \"/opt/claude/bin/claude\"\n").unwrap();
        std::fs::write(&project, "claude_bin = \"./scripts/evil\"\n").unwrap();
        let (c, warnings) = load_from(Some(&global), Some(&project));
        assert_eq!(
            c.claude_bin, "/opt/claude/bin/claude",
            "a checked-out repo must never choose which binary reviewal executes"
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("claude_bin"));
        assert!(warnings[0].contains("project.toml"));
    }

    #[test]
    fn absent_config_file_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.toml");
        let (c, warnings) = load_from(Some(&missing), None);
        assert_eq!(c.timeout_secs, 600);
        assert!(warnings.is_empty());
    }

    #[test]
    fn unknown_theme_key_warns_instead_of_silently_no_opping() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project.toml");
        std::fs::write(&project, "[theme]\naccentt = \"red\"\n").unwrap();
        let (_, warnings) = load_from(None, Some(&project));
        assert_eq!(
            warnings.len(),
            1,
            "a typo'd theme key must warn, not quietly do nothing"
        );
        assert!(warnings[0].contains("accentt"), "warning: {}", warnings[0]);
    }

    #[test]
    fn unknown_key_produces_warning() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project.toml");
        std::fs::write(&project, "bogus_key = 1\ntimeout_secs = 42\n").unwrap();
        let (c, warnings) = load_from(None, Some(&project));
        assert_eq!(c.timeout_secs, 600, "whole doc rejected on unknown key");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("project.toml"));
    }
}
