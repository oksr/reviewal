use crate::engine::model::Severity;
use crate::engine::store::RunStatus;
use crate::engine::synthesis::Confidence;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::str::FromStr;

/// Builtin persona names in color-slot order (not builtins() load order);
/// a test pins that the two cover the same set of names.
pub(crate) const BUILTIN_SLOTS: [&str; 6] = [
    "prover", "breaker", "skeptic", "stickler", "steward", "advocate",
];

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// One-time warning pass over loaded personas (composer entry). Resolution
/// itself silently falls through on invalid colors; this is where the user
/// hears about them.
pub(crate) fn validate_persona_colors(personas: &[crate::engine::persona::Persona]) -> Vec<String> {
    personas
        .iter()
        .filter_map(|p| {
            let raw = p.color.as_deref()?;
            match Color::from_str(raw) {
                Ok(_) => None,
                Err(_) => Some(format!("persona {}: invalid color {raw:?}", p.name)),
            }
        })
        .collect()
}

/// Candidate persona colors, in slot order. The accent is filtered out at
/// load time so no reviewer wears the app's color by default.
pub(crate) const DEFAULT_POOL: [Color; 7] = [
    Color::Cyan,
    Color::Magenta,
    Color::Blue,
    Color::Yellow,
    Color::Green,
    Color::LightBlue,
    Color::LightMagenta,
];

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// Truecolor mirror of [`DEFAULT_POOL`], slot for slot, so builtin personas
/// keep their hue family when the terminal upgrades to 24-bit color.
pub(crate) const TRUECOLOR_POOL: [Color; 7] = [
    rgb(0x7dcfff), // cyan slot
    rgb(0xbb9af7), // magenta slot
    rgb(0x7aa2f7), // blue slot — the default accent, filtered out by default
    rgb(0xe0af68), // yellow slot
    rgb(0x9ece6a), // green slot
    rgb(0x2ac3de), // light-blue slot
    rgb(0xff9ac1), // light-magenta slot
];

/// Single source of truth for the scalar color roles: `Theme`, its `Default`,
/// `truecolor`, `monochrome`, `apply_color_overrides`, and config's
/// `ThemeOverrides` all expand this table, so adding or renaming a role here
/// updates every site at once; the non-scalar `persona_pool`/`mono` and the
/// `truecolor` config flag are handled explicitly. Each role carries its ANSI
/// default and the 24-bit default used on truecolor terminals.
macro_rules! for_each_color_role {
    ($apply:ident) => {
        $apply! {
            accent: Blue = 0x7aa2f7,
            // ANSI Gray (7), not DarkGray (bright-black): DarkGray is nearly
            // invisible on most dark terminal themes. Gray reads as muted
            // next to the default foreground while staying legible.
            dim: Gray = 0x565f89,
            // Background tint for the selected row; DarkGray's fg-legibility
            // problem doesn't apply to backgrounds, where it reads as a
            // subtle bar under the default foreground.
            selection_bg: DarkGray = 0x2a2f41,
            error: Red = 0xf7768e,
            status_pending: Gray = 0x565f89,
            status_retrying: Yellow = 0xe0af68,
            status_done: Green = 0x9ece6a,
            status_failed: Red = 0xf7768e,
            run_status_running: Cyan = 0x7dcfff,
            run_status_reviews_complete: Yellow = 0xe0af68,
            run_status_finalized: Green = 0x9ece6a,
            run_status_aborted: Red = 0xf7768e,
            run_status_stale: Gray = 0x565f89,
            severity_critical: Red = 0xf7768e,
            severity_warning: Yellow = 0xe0af68,
            severity_info: Blue = 0x7aa2f7,
            confidence_cross_validated: Green = 0x9ece6a,
            confidence_consensus: Cyan = 0x7dcfff,
            confidence_disputed: Yellow = 0xe0af68,
            confidence_solo: Gray = 0x565f89,
            verdict_ship: Green = 0x9ece6a,
            verdict_caveats: Yellow = 0xe0af68,
            verdict_hold: LightRed = 0xff9e64,
            verdict_block: Red = 0xf7768e,
        }
    };
}
pub(crate) use for_each_color_role;

macro_rules! declare_theme {
    ($($role:ident: $ansi:ident = $rgb:literal),* $(,)?) => {
        /// Semantic color roles. Screens never name a raw `Color::` — they ask the
        /// theme by meaning. Built once at startup; persona colors are computed on
        /// demand by `persona_color`, so nothing here depends on per-run data.
        pub(crate) struct Theme {
            $(pub $role: Color,)*
            pub persona_pool: Vec<Color>,
            pub mono: bool,
        }

        impl Default for Theme {
            fn default() -> Self {
                let mut theme = Theme {
                    $($role: Color::$ansi,)*
                    persona_pool: Vec::new(),
                    mono: false,
                };
                theme.persona_pool = DEFAULT_POOL
                    .iter()
                    .copied()
                    .filter(|c| *c != theme.accent)
                    .collect();
                theme
            }
        }

        impl Theme {
            /// The curated 24-bit palette, used when the terminal advertises
            /// truecolor. Same roles, softer related hues.
            pub(crate) fn truecolor() -> Theme {
                let mut theme = Theme {
                    $($role: rgb($rgb),)*
                    persona_pool: Vec::new(),
                    mono: false,
                };
                theme.persona_pool = TRUECOLOR_POOL
                    .iter()
                    .copied()
                    .filter(|c| *c != theme.accent)
                    .collect();
                theme
            }

            /// NO_COLOR mode: every color is `Reset`; bold/reversed modifiers survive.
            pub(crate) fn monochrome() -> Theme {
                Theme {
                    $($role: Color::Reset,)*
                    persona_pool: vec![],
                    mono: true,
                }
            }
        }

        fn apply_color_overrides(
            theme: &mut Theme,
            o: &crate::config::ThemeOverrides,
            warnings: &mut Vec<String>,
        ) {
            $( if let Some(v) = &o.$role {
                match Color::from_str(v) {
                    Ok(c) => theme.$role = c,
                    Err(_) => warnings.push(format!(
                        "theme: invalid color {v:?} for {}", stringify!($role)
                    )),
                }
            } )*
        }
    };
}

/// Shared box chrome: every bordered block in the app renders through this
/// so the screens read as one system.
pub(crate) fn bordered() -> ratatui::widgets::Block<'static> {
    ratatui::widgets::Block::bordered().border_type(ratatui::widgets::BorderType::Rounded)
}

/// Sets an already-styled title line into the border (`╭─ title ──…`), for
/// boxes whose titles carry their own spans or colors; plain-text titles go
/// through [`Theme::panel`] instead.
pub(crate) fn inset_title(mut title: Line<'static>, border_style: Style) -> Line<'static> {
    let mut spans = vec![Span::styled("\u{2500} ", border_style)];
    spans.append(&mut title.spans);
    spans.push(Span::raw(" "));
    Line::from(spans)
}

impl Theme {
    /// Focus-aware box: rounded, dim when unfocused, title set into the
    /// border line. Screens with styled titles compose [`bordered`] +
    /// [`inset_title`] directly.
    pub(crate) fn panel(&self, title: &str, focused: bool) -> ratatui::widgets::Block<'static> {
        let border_style = if focused {
            self.accent_style()
        } else {
            self.dim_style()
        };
        let title_style = if focused {
            self.title_style()
        } else {
            self.dim_style()
        };
        bordered().border_style(border_style).title(inset_title(
            Line::styled(title.to_string(), title_style),
            border_style,
        ))
    }
}
for_each_color_role!(declare_theme);

impl Theme {
    pub(crate) fn run_status(&self, s: &RunStatus) -> Color {
        match s {
            RunStatus::Running => self.run_status_running,
            RunStatus::ReviewsComplete => self.run_status_reviews_complete,
            RunStatus::Finalized => self.run_status_finalized,
            RunStatus::Aborted => self.run_status_aborted,
            RunStatus::Stale => self.run_status_stale,
        }
    }

    pub(crate) fn severity(&self, s: Severity) -> Style {
        match s {
            Severity::Critical => Style::default()
                .fg(self.severity_critical)
                .add_modifier(Modifier::BOLD),
            Severity::Warning => Style::default().fg(self.severity_warning),
            Severity::Info => Style::default().fg(self.severity_info),
        }
    }

    pub(crate) fn confidence(&self, c: &Confidence) -> Color {
        match c {
            Confidence::CrossValidated => self.confidence_cross_validated,
            Confidence::Consensus => self.confidence_consensus,
            Confidence::Disputed => self.confidence_disputed,
            Confidence::Solo => self.confidence_solo,
        }
    }

    /// Colors a consensus label. Prefix order matters: SHIP-WITH-CAVEATS
    /// starts with "SHIP", so it must be checked first. Unknown labels get
    /// bold in the default foreground — never a wrong color.
    pub(crate) fn verdict(&self, label: &str) -> Style {
        let style = Style::default().add_modifier(Modifier::BOLD);
        if label.starts_with("SHIP-WITH-CAVEATS") {
            style.fg(self.verdict_caveats)
        } else if label.starts_with("SHIP") {
            style.fg(self.verdict_ship)
        } else if label.starts_with("HOLD") {
            style.fg(self.verdict_hold)
        } else if label.starts_with("BLOCK") {
            style.fg(self.verdict_block)
        } else if self.mono {
            style.fg(Color::Reset)
        } else {
            style
        }
    }

    pub(crate) fn accent_style(&self) -> Style {
        Style::default().fg(self.accent)
    }

    pub(crate) fn title_style(&self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    pub(crate) fn dim_style(&self) -> Style {
        Style::default().fg(self.dim)
    }

    /// Row-selection treatment: a soft background tint, so per-cell colors
    /// survive selection. Monochrome falls back to reverse video — a bg
    /// tint would be invisible without color.
    pub(crate) fn selection_style(&self) -> Style {
        if self.mono {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().bg(self.selection_bg)
        }
    }

    pub(crate) fn hint_spans(&self, pairs: &[(&str, &str)]) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        for (i, (key, desc)) in pairs.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" · ".to_string(), self.dim_style()));
            }
            spans.push(Span::styled((*key).to_string(), self.accent_style()));
            spans.push(Span::styled(format!(" {desc}"), self.dim_style()));
        }
        spans
    }

    pub(crate) fn hints(&self, pairs: &[(&str, &str)]) -> Line<'static> {
        Line::from(self.hint_spans(pairs))
    }

    /// Resolution order: monochrome → frontmatter color → builtin slot →
    /// stable name hash. Pure; safe to call at draw time for names from
    /// persisted runs whose persona files no longer exist (hash fallback).
    pub(crate) fn persona_color(&self, name: &str, frontmatter: Option<&str>) -> Color {
        if self.mono {
            return Color::Reset;
        }
        if let Some(raw) = frontmatter {
            if let Ok(c) = Color::from_str(raw) {
                return c;
            }
        }
        if let Some(i) = BUILTIN_SLOTS.iter().position(|n| *n == name) {
            return self.persona_pool[i % self.persona_pool.len()];
        }
        let h = fnv1a_64(name.as_bytes());
        self.persona_pool[(h % self.persona_pool.len() as u64) as usize]
    }

    /// Pure function of the config: NO_COLOR and COLORTERM are read once by
    /// `config::load` and arrive as `env_no_color`/`env_truecolor`, so a
    /// `Config::default()` yields the same theme in every environment.
    pub(crate) fn load(config: &crate::config::Config) -> (Theme, Vec<String>) {
        Theme::load_with(config, config.env_no_color, config.env_truecolor)
    }

    /// Env-free core of [`load`](Theme::load). Never fails — invalid color
    /// overrides are dropped and reported in the returned warnings, not errored.
    /// `truecolor` is the terminal capability as detected; the config's
    /// `[theme] truecolor` key overrides it in either direction.
    pub(crate) fn load_with(
        config: &crate::config::Config,
        no_color: bool,
        truecolor: bool,
    ) -> (Theme, Vec<String>) {
        if no_color {
            return (Theme::monochrome(), vec![]);
        }
        let o = &config.theme;
        let use_rgb = o.truecolor.unwrap_or(truecolor);
        let base_pool: &[Color] = if use_rgb {
            &TRUECOLOR_POOL
        } else {
            &DEFAULT_POOL
        };
        let mut theme = if use_rgb {
            Theme::truecolor()
        } else {
            Theme::default()
        };
        let mut warnings = Vec::new();
        apply_color_overrides(&mut theme, o, &mut warnings);
        let mut pool: Vec<Color> = match &o.persona_pool {
            None => base_pool.to_vec(),
            Some(names) => names
                .iter()
                .filter_map(|n| match Color::from_str(n) {
                    Ok(c) => Some(c),
                    Err(_) => {
                        warnings.push(format!("theme: invalid color {n:?} in persona_pool"));
                        None
                    }
                })
                .collect(),
        };
        pool.retain(|c| *c != theme.accent);
        if pool.len() < 2 {
            warnings.push(
                "theme: persona_pool has fewer than 2 usable colors; using the default pool"
                    .to_string(),
            );
            pool = base_pool
                .iter()
                .copied()
                .filter(|c| *c != theme.accent)
                .collect();
        }
        theme.persona_pool = pool;
        (theme, warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    #[test]
    fn default_theme_is_blue_accent_with_filtered_pool() {
        let t = Theme::default();
        assert_eq!(t.accent, Color::Blue);
        assert!(!t.mono);
        assert!(!t.persona_pool.contains(&Color::Blue), "accent excluded");
        assert_eq!(t.persona_pool.len(), 6);
    }

    #[test]
    fn verdict_matches_caveats_before_ship() {
        let t = Theme::default();
        assert_eq!(
            t.verdict("SHIP-WITH-CAVEATS (3/3 ship, 0/3 block)").fg,
            Some(Color::Yellow)
        );
        assert_eq!(t.verdict("SHIP (unanimous, 2/2)").fg, Some(Color::Green));
        assert_eq!(
            t.verdict("HOLD — split decision (1/2 ship, 1/2 block)").fg,
            Some(Color::LightRed)
        );
        assert_eq!(
            t.verdict("BLOCK (2/2 block, 0/2 ship)").fg,
            Some(Color::Red)
        );
        let unknown = t.verdict("SOMETHING ELSE");
        assert_eq!(unknown.fg, None, "unknown label keeps default fg");
        assert!(unknown.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn monochrome_resets_every_color() {
        let t = Theme::monochrome();
        assert!(t.mono);
        assert_eq!(t.accent, Color::Reset);
        assert_eq!(t.status_failed, Color::Reset);
        assert_eq!(t.verdict("SHIP").fg, Some(Color::Reset));
    }

    fn config_with_theme(theme: crate::config::ThemeOverrides) -> crate::config::Config {
        crate::config::Config {
            theme,
            ..Default::default()
        }
    }

    #[test]
    fn load_applies_valid_overrides_including_hex() {
        let o = crate::config::ThemeOverrides {
            accent: Some("magenta".into()),
            severity_critical: Some("#ff5555".into()),
            ..Default::default()
        };
        let (t, warnings) = Theme::load_with(&config_with_theme(o), false, false);
        assert_eq!(t.accent, Color::Magenta);
        assert_eq!(t.severity_critical, Color::Rgb(0xff, 0x55, 0x55));
        assert!(warnings.is_empty());
    }

    #[test]
    fn every_color_role_round_trips_from_toml_key_to_theme_field() {
        // Generated from the same table as the production sites: proves each
        // config key actually lands on its Theme field end-to-end, so a serde
        // rename or a special-cased field can't quietly break one role.
        macro_rules! check_round_trip {
            ($($role:ident: $ansi:ident = $rgb:literal),* $(,)?) => {
                $(
                    let o: crate::config::ThemeOverrides =
                        toml::from_str(concat!(stringify!($role), " = \"red\"")).unwrap();
                    let (t, warnings) = Theme::load_with(&config_with_theme(o), false, false);
                    assert_eq!(
                        t.$role,
                        Color::Red,
                        "TOML key `{}` must reach Theme::{}",
                        stringify!($role),
                        stringify!($role)
                    );
                    assert!(warnings.is_empty(), "{}: {warnings:?}", stringify!($role));
                )*
            };
        }
        for_each_color_role!(check_round_trip);
    }

    #[test]
    fn readme_theme_defaults_match_the_shipped_defaults() {
        // The README shows every role's default as a commented-out override.
        // Uncomment the block, load it, and the result must be exactly
        // Theme::default() — otherwise a user "uncommenting the defaults"
        // would silently change their theme. Also requires every table role
        // to be documented at all.
        let readme = include_str!("../../README.md");
        let start = readme
            .find("```toml\n[theme]")
            .expect("README documents [theme] in a toml fence");
        let end = readme[start + 7..].find("```").expect("fence closes") + start + 7;
        let uncommented: String = readme[start..end]
            .lines()
            .filter_map(|l| l.strip_prefix("# "))
            .collect::<Vec<_>>()
            .join("\n");
        let o: crate::config::ThemeOverrides =
            toml::from_str(&uncommented).expect("README block is valid TOML when uncommented");
        let (t, warnings) = Theme::load_with(&config_with_theme(o), false, false);
        assert!(warnings.is_empty(), "{warnings:?}");
        let d = Theme::default();
        macro_rules! check_matches_default {
            ($($role:ident: $ansi:ident = $rgb:literal),* $(,)?) => {
                $(
                    assert!(
                        uncommented.contains(stringify!($role)),
                        "role `{}` is missing from the README theming block",
                        stringify!($role)
                    );
                    assert_eq!(
                        t.$role,
                        d.$role,
                        "README documents a default for `{}` that differs from Theme::default()",
                        stringify!($role)
                    );
                )*
            };
        }
        for_each_color_role!(check_matches_default);
        assert_eq!(t.persona_pool, d.persona_pool, "persona_pool doc drifted");
    }

    #[test]
    fn load_warns_and_keeps_default_on_invalid_value() {
        let o = crate::config::ThemeOverrides {
            accent: Some("blurple".into()),
            ..Default::default()
        };
        let (t, warnings) = Theme::load_with(&config_with_theme(o), false, false);
        assert_eq!(t.accent, Color::Blue, "default kept");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("blurple") && warnings[0].contains("accent"));
    }

    #[test]
    fn load_filters_overridden_accent_from_pool() {
        let o = crate::config::ThemeOverrides {
            accent: Some("magenta".into()),
            ..Default::default()
        };
        let (t, _) = Theme::load_with(&config_with_theme(o), false, false);
        assert!(!t.persona_pool.contains(&Color::Magenta));
        assert!(
            t.persona_pool.contains(&Color::Blue),
            "blue back in the pool"
        );
        assert_eq!(
            t.persona_color("prover", None),
            Color::Cyan,
            "slot 0 unchanged"
        );
    }

    #[test]
    fn load_falls_back_when_user_pool_too_small() {
        let o = crate::config::ThemeOverrides {
            persona_pool: Some(vec!["blue".into()]), // == accent → filtered to zero
            ..Default::default()
        };
        let (t, warnings) = Theme::load_with(&config_with_theme(o), false, false);
        assert_eq!(
            t.persona_pool.len(),
            6,
            "default accent-filtered pool restored"
        );
        assert!(warnings.iter().any(|w| w.contains("persona_pool")));
    }

    #[test]
    fn no_color_returns_monochrome_with_no_warnings() {
        let o = crate::config::ThemeOverrides {
            accent: Some("blurple".into()), // invalid — but must not be validated
            ..Default::default()
        };
        let (t, warnings) = Theme::load_with(&config_with_theme(o), true, false);
        assert!(t.mono);
        assert!(
            warnings.is_empty(),
            "overrides neither applied nor validated"
        );
    }

    #[test]
    fn truecolor_flag_selects_the_rgb_palette() {
        let (t, warnings) = Theme::load_with(&crate::config::Config::default(), false, true);
        assert!(warnings.is_empty());
        assert_eq!(t.accent, Color::Rgb(0x7a, 0xa2, 0xf7));
        assert_eq!(t.dim, Color::Rgb(0x56, 0x5f, 0x89));
        assert_eq!(t.persona_pool.len(), 6, "accent filtered from RGB pool");
        assert!(!t.persona_pool.contains(&t.accent));
        assert_eq!(
            t.persona_color("prover", None),
            Color::Rgb(0x7d, 0xcf, 0xff),
            "builtin slot order preserved in the RGB pool"
        );
    }

    #[test]
    fn config_truecolor_key_overrides_detection_both_ways() {
        let forced_on = config_with_theme(crate::config::ThemeOverrides {
            truecolor: Some(true),
            ..Default::default()
        });
        let (t, _) = Theme::load_with(&forced_on, false, false);
        assert_eq!(t.accent, Color::Rgb(0x7a, 0xa2, 0xf7));

        let forced_off = config_with_theme(crate::config::ThemeOverrides {
            truecolor: Some(false),
            ..Default::default()
        });
        let (t, _) = Theme::load_with(&forced_off, false, true);
        assert_eq!(t.accent, Color::Blue, "detection ignored when forced off");
    }

    #[test]
    fn no_color_beats_truecolor() {
        let (t, _) = Theme::load_with(&crate::config::Config::default(), true, true);
        assert!(t.mono);
    }

    #[test]
    fn role_overrides_apply_on_top_of_the_truecolor_base() {
        let o = crate::config::ThemeOverrides {
            accent: Some("magenta".into()),
            ..Default::default()
        };
        let (t, warnings) = Theme::load_with(&config_with_theme(o), false, true);
        assert!(warnings.is_empty());
        assert_eq!(t.accent, Color::Magenta, "override wins");
        assert_eq!(t.dim, Color::Rgb(0x56, 0x5f, 0x89), "base stays RGB");
        assert!(
            t.persona_pool.contains(&Color::Rgb(0x7a, 0xa2, 0xf7)),
            "RGB blue rejoins the pool once the accent moves off it"
        );
    }

    #[test]
    fn selection_style_is_bg_tint_normally_and_reversed_in_mono() {
        let t = Theme::default();
        let s = t.selection_style();
        assert_eq!(s.bg, Some(Color::DarkGray));
        assert!(!s.add_modifier.contains(Modifier::REVERSED));

        let m = Theme::monochrome().selection_style();
        assert_eq!(m.bg, None);
        assert!(m.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn builtin_slots_cover_exactly_the_builtin_personas() {
        let mut names: Vec<String> = crate::engine::persona::builtins()
            .into_iter()
            .map(|p| p.name)
            .collect();
        names.sort();
        let mut slots: Vec<String> = BUILTIN_SLOTS.iter().map(|s| s.to_string()).collect();
        slots.sort();
        assert_eq!(names, slots);
    }

    #[test]
    fn builtins_get_distinct_stable_slot_colors() {
        let t = Theme::default();
        let colors: Vec<Color> = BUILTIN_SLOTS
            .iter()
            .map(|n| t.persona_color(n, None))
            .collect();
        assert_eq!(colors[0], Color::Cyan, "prover");
        assert_eq!(colors[1], Color::Magenta, "breaker");
        assert_eq!(colors[2], Color::Yellow, "skeptic");
        assert_eq!(colors[3], Color::Green, "stickler");
        assert_eq!(colors[4], Color::LightBlue, "steward");
        assert_eq!(colors[5], Color::LightMagenta, "advocate");
        let mut dedup = colors.clone();
        dedup.sort_by_key(|c| format!("{c:?}"));
        dedup.dedup();
        assert_eq!(dedup.len(), 6, "pairwise distinct");
    }

    #[test]
    fn custom_persona_hash_is_deterministic_and_in_pool() {
        let t = Theme::default();
        let a = t.persona_color("greybeard", None);
        let b = t.persona_color("greybeard", None);
        assert_eq!(a, b);
        assert!(t.persona_pool.contains(&a));
        assert_ne!(a, t.accent, "pool is accent-filtered");
    }

    #[test]
    fn frontmatter_color_wins_even_over_accent_rule() {
        let t = Theme::default();
        assert_eq!(
            t.persona_color("prover", Some("light red")),
            Color::LightRed
        );
        assert_eq!(
            t.persona_color("anyone", Some("blue")),
            Color::Blue,
            "explicit accent allowed"
        );
    }

    #[test]
    fn invalid_frontmatter_falls_through_to_slot() {
        let t = Theme::default();
        assert_eq!(t.persona_color("prover", Some("blurple")), Color::Cyan);
    }

    #[test]
    fn monochrome_persona_color_is_reset_even_with_frontmatter() {
        let t = Theme::monochrome();
        assert_eq!(t.persona_color("prover", Some("red")), Color::Reset);
        assert_eq!(t.persona_color("custom", None), Color::Reset);
    }

    #[test]
    fn validate_persona_colors_reports_only_invalid() {
        let mk = |name: &str, color: Option<&str>| crate::engine::persona::Persona {
            name: name.into(),
            title: name.into(),
            lens: "l".into(),
            target: crate::engine::persona::PersonaTarget::Both,
            system: "s".into(),
            builtin: false,
            color: color.map(String::from),
            source: None,
        };
        let personas = vec![
            mk("ok", Some("cyan")),
            mk("bad", Some("blurple")),
            mk("none", None),
        ];
        let warnings = validate_persona_colors(&personas);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("bad") && warnings[0].contains("blurple"));
    }

    #[test]
    fn hints_render_same_text_as_before() {
        let t = Theme::default();
        let line = t.hints(&[
            ("n", "new review"),
            ("enter", "open"),
            ("j/k", "move"),
            ("q", "quit"),
        ]);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "n new review · enter open · j/k move · q quit");
    }
}
