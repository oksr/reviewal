//! reviewal: an adversarial multi-agent code-review TUI.

pub mod config;
pub mod engine;
pub mod skill;
pub mod ui;

// Curated public API. Prefer these paths over reaching into module internals;
// `tests/public_api.rs` compile-fences this exact set.
pub use config::{load as load_config, Config};
pub use engine::agent::AgentActivity;
pub use engine::model::{Severity, TargetKind, Verdict};
pub use engine::persona::{available, builtins, load_custom, Persona, PersonaTarget};
pub use engine::run::{build_review_spec, execute_run, Phase, RunEvent, RunSpec};
pub use engine::synthesis::{Attribution, Confidence, Finding, Synthesis};
pub use engine::target::Target;
pub use skill::{init as init_skill, InitReport, SkillOutcome};
pub use ui::app::Bootstrap;
pub use ui::run_tui;
