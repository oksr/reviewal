use clap::{Parser, Subcommand};
use reviewal::engine::persona::{self, PersonaTarget};
use reviewal::ui::app::Bootstrap;
use reviewal::{config, skill, ui};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "reviewal",
    version,
    about = "Adversarial multi-agent review TUI"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a review immediately, skipping the composer screen
    Review {
        /// Review the git diff (optionally vs BASE, e.g. --diff main)
        // The nested Option models the tri-state natively: None = flag
        // absent, Some(None) = `--diff` (HEAD), Some(Some(b)) = `--diff b`.
        #[arg(long, num_args = 0..=1)]
        diff: Option<Option<String>>,
        /// Review spec/plan files
        #[arg(long, num_args = 1..)]
        spec: Vec<PathBuf>,
        /// Comma-separated persona names (default: all built-ins for the target kind)
        #[arg(long)]
        personas: Option<String>,
        /// Run the round-2 cross-review (default: off)
        #[arg(long)]
        cross_review: bool,
        /// Model override (default: config, else the claude CLI's default)
        #[arg(long)]
        model: Option<String>,
    },
    /// Install the reviewal-ingest skill and scaffold .reviewal/
    Init {
        /// Overwrite a locally modified skill
        #[arg(long)]
        force: bool,
    },
    /// List available personas
    Personas,
}

fn now_utc() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn target_label(t: PersonaTarget) -> &'static str {
    match t {
        PersonaTarget::Code => "code",
        PersonaTarget::Spec => "spec",
        PersonaTarget::Both => "both",
    }
}

fn cmd_personas(root: &Path, config: &config::Config) {
    let dirs = config.persona_dirs(root);
    let (custom, failures) = persona::load_custom(&dirs);
    let mut personas = persona::builtins();
    for c in custom {
        personas.retain(|p| p.name != c.name);
        personas.push(c);
    }
    for w in &failures {
        eprintln!("warning: {w}");
    }
    println!("{:<12} {:<6} {:<52} source", "name", "target", "lens");
    for p in &personas {
        println!(
            "{:<12} {:<6} {:<52} {}",
            p.name,
            target_label(p.target),
            p.lens,
            if p.builtin { "builtin" } else { "custom" }
        );
    }
}

fn cmd_init(root: &Path, force: bool) {
    match skill::init(root, force) {
        Ok(report) => {
            let skill_line = match report.skill {
                skill::SkillOutcome::Installed => "installed".to_string(),
                skill::SkillOutcome::Upgraded => "upgraded".to_string(),
                skill::SkillOutcome::UpToDate => "up-to-date".to_string(),
                skill::SkillOutcome::SkippedModified => {
                    "skipped (locally modified — use --force)".to_string()
                }
            };
            println!("skill: {skill_line} ({})", report.skill_path.display());
            println!(
                "config: {}",
                if report.config_created {
                    "created"
                } else {
                    "exists"
                }
            );
            println!(
                ".gitignore: {}",
                if report.gitignore_updated {
                    "updated"
                } else {
                    "ok"
                }
            );
        }
        Err(e) => {
            eprintln!("error: init failed: {e}");
            std::process::exit(1);
        }
    }
}

/// reviewal is an interactive TUI, not a pipe-friendly CLI: bail loudly
/// instead of letting ratatui fail deep inside terminal setup with a much
/// less legible error. Checked right before each `run_tui` call rather than
/// once up front so `Cmd::Review`'s preflight (which must run — and report —
/// even when stdout is a pipe) gets a chance to fail first.
fn guard_terminal() {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        eprintln!("error: reviewal is an interactive TUI and needs a terminal");
        std::process::exit(2);
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let root = std::env::current_dir()?;
    let (config, config_warnings) = config::load(&root);
    for w in &config_warnings {
        eprintln!("warning: {w}");
    }

    match cli.command {
        None => {
            guard_terminal();
            ui::run_tui(root, config, Bootstrap::Home)
        }
        Some(Cmd::Personas) => {
            cmd_personas(&root, &config);
            Ok(())
        }
        Some(Cmd::Init { force }) => {
            cmd_init(&root, force);
            Ok(())
        }
        Some(Cmd::Review {
            diff,
            spec,
            personas,
            cross_review,
            model,
        }) => {
            let run_spec = match reviewal::engine::run::build_review_spec(
                &root,
                &config,
                diff,
                spec,
                personas,
                cross_review,
                model,
                now_utc(),
            ) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }
            };
            // Preflight before the terminal guard: a `--diff`/`--spec`
            // target error or a missing/stale `claude` binary must print to
            // stderr and exit 2 even when stdout is redirected — the operator
            // running this from a script or CI needs that even without a tty.
            let errors =
                reviewal::engine::preflight::preflight(&config.claude_bin, &run_spec.target, &root);
            if !errors.is_empty() {
                for e in &errors {
                    eprintln!("error: {e}");
                }
                std::process::exit(2);
            }
            guard_terminal();
            ui::run_tui(root, config, Bootstrap::Run(run_spec))
        }
    }
}

#[cfg(test)]
mod tests {
    use reviewal::engine::run::build_review_spec;
    use reviewal::{Config, Target};

    // A base literally named "__HEAD__" must map as a real base — no sentinel.
    #[test]
    fn diff_flag_tristate_maps_without_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::default();
        let build = |diff: Option<Option<String>>| {
            build_review_spec(
                dir.path(),
                &config,
                diff,
                vec![],
                None,
                false,
                None,
                "2026-01-01T00:00:00Z".into(),
            )
            .expect("a diff target with default personas builds")
        };
        assert!(matches!(
            build(Some(None)).target,
            Target::GitDiff { base: None }
        ));
        assert!(matches!(
            build(Some(Some("main".into()))).target,
            Target::GitDiff { base: Some(b) } if b == "main"
        ));
        assert!(matches!(
            build(Some(Some("__HEAD__".into()))).target,
            Target::GitDiff { base: Some(b) } if b == "__HEAD__"
        ));
    }
}
