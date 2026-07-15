// Compile-fence: pins reviewal's curated public API. Referencing every export
// makes this file fail to compile if an item is demoted — and, as an
// external-crate consumer, it cannot even name a crate-private internal.
use reviewal::{
    AgentActivity, Attribution, Bootstrap, Confidence, Config, Finding, InitReport, Persona,
    PersonaTarget, Phase, RunEvent, RunSpec, Severity, SkillOutcome, Synthesis, Target, TargetKind,
    Verdict,
};

#[test]
fn public_surface_is_reachable() {
    // Naming each type in a signature proves it is `pub`. Split into <=7-arg
    // groups so clippy::too_many_arguments stays quiet under -D warnings.
    fn _engine_types(
        _s: Synthesis,
        _f: Finding,
        _a: Attribution,
        _cf: Confidence,
        _re: RunEvent,
        _ph: Phase,
        _aa: AgentActivity,
    ) {
    }
    fn _model_types(
        _sev: Severity,
        _v: Verdict,
        _tk: TargetKind,
        _t: Target,
        _rs: RunSpec,
        _p: Persona,
        _pt: PersonaTarget,
    ) {
    }
    fn _app_types(_c: Config, _b: Bootstrap, _so: SkillOutcome, _ir: InitReport) {}
    // Binding each free fn as an item proves it is `pub`.
    let _ = reviewal::builtins;
    let _ = reviewal::available;
    let _ = reviewal::load_custom;
    let _ = reviewal::execute_run;
    let _ = reviewal::run_tui;
    let _ = reviewal::load_config;
    // Ambient env is read only inside `load_config`; a consumer building a
    // Config by hand gets hermetic, injected persona directories.
    let _ = Config::persona_dirs;
    let _ = reviewal::init_skill;
    let _ = reviewal::build_review_spec;
}
