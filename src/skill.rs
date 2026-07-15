use anyhow::Context;
use std::path::{Path, PathBuf};

const SKILL_TEMPLATE: &str = include_str!("../assets/SKILL.md");
const GITIGNORE_LINE: &str = ".reviewal/runs/";
const DEFAULT_CONFIG: &str = "# reviewal project config\n# model = \"opus\"        # default: claude CLI's own default model\n# timeout_secs = 600\n";

#[derive(Debug)]
pub enum SkillOutcome {
    Installed,
    Upgraded,
    UpToDate,
    SkippedModified,
}

#[derive(Debug)]
pub struct InitReport {
    pub skill: SkillOutcome,
    pub skill_path: PathBuf,
    pub gitignore_updated: bool,
    pub config_created: bool,
}

fn parse_version(s: &str) -> Option<Vec<u64>> {
    // Drop any SemVer pre-release (`-rc.1`) or build-metadata (`+build.7`) suffix.
    let core = s.trim().split(['-', '+']).next().unwrap_or_default();
    let parts: Vec<u64> = core
        .split('.')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    (!parts.is_empty()).then_some(parts)
}

fn installed_version(text: &str) -> Option<Vec<u64>> {
    text.lines()
        .find_map(|l| l.trim().strip_prefix("reviewal-version:"))
        .and_then(parse_version)
}

fn install_skill(path: &Path, force: bool) -> anyhow::Result<SkillOutcome> {
    let rendered = SKILL_TEMPLATE.replace("{VERSION}", env!("CARGO_PKG_VERSION"));
    let write = |outcome: SkillOutcome| -> anyhow::Result<SkillOutcome> {
        std::fs::create_dir_all(path.parent().expect("skill path has parent"))?;
        std::fs::write(path, &rendered)?;
        Ok(outcome)
    };
    let Ok(existing) = std::fs::read_to_string(path) else {
        return write(SkillOutcome::Installed);
    };
    if force {
        return write(SkillOutcome::Installed);
    }
    let Some(current) = parse_version(env!("CARGO_PKG_VERSION")) else {
        // A crate version we can't parse — treat ours as newest and (re)write the skill.
        return write(SkillOutcome::Upgraded);
    };
    match installed_version(&existing) {
        None => Ok(SkillOutcome::SkippedModified),
        Some(v) if v < current => write(SkillOutcome::Upgraded),
        Some(_) => Ok(SkillOutcome::UpToDate),
    }
}

fn ensure_gitignore(root: &Path) -> anyhow::Result<bool> {
    let path = root.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == GITIGNORE_LINE) {
        return Ok(false);
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(GITIGNORE_LINE);
    updated.push('\n');
    std::fs::write(&path, updated).context("updating .gitignore")?;
    Ok(true)
}

fn ensure_config(root: &Path) -> anyhow::Result<bool> {
    let path = root.join(".reviewal/config.toml");
    if path.exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(path.parent().expect("has parent"))?;
    std::fs::write(&path, DEFAULT_CONFIG)?;
    Ok(true)
}

pub(crate) fn skill_md_path(root: &Path) -> PathBuf {
    root.join(".claude/skills/reviewal-ingest/SKILL.md")
}

#[allow(dead_code)]
pub(crate) fn ingest_skill_installed(root: &Path) -> bool {
    skill_md_path(root).is_file()
}

pub fn init(project_root: &Path, force: bool) -> anyhow::Result<InitReport> {
    let skill_path = skill_md_path(project_root);
    let skill = install_skill(&skill_path, force)?;
    let config_created = ensure_config(project_root)?;
    let gitignore_updated = ensure_gitignore(project_root)?;
    Ok(InitReport {
        skill,
        skill_path,
        gitignore_updated,
        config_created,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_tolerates_prerelease_and_build_metadata() {
        assert_eq!(parse_version("0.2.0-rc.1"), Some(vec![0, 2, 0]));
        assert_eq!(parse_version("1.4.0+build.7"), Some(vec![1, 4, 0]));
        assert_eq!(parse_version("0.1.0"), Some(vec![0, 1, 0]));
        assert_eq!(parse_version("not-a-version"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn skill_frames_report_content_as_untrusted() {
        // Security posture: report.json carries reviewer-model output over a
        // possibly hostile artifact, and the skill feeds it into a
        // code-editing session. The template must tell that session to treat
        // finding text as claims to evaluate, never instructions to follow.
        assert!(
            SKILL_TEMPLATE.contains("Trust boundary"),
            "SKILL.md must frame report.json content as untrusted"
        );
        assert!(SKILL_TEMPLATE.contains("not instructions"));
    }

    #[test]
    fn fresh_install_writes_everything() {
        let dir = tempfile::tempdir().unwrap();
        let report = init(dir.path(), false).unwrap();
        assert!(matches!(report.skill, SkillOutcome::Installed));
        let skill = std::fs::read_to_string(report.skill_path).unwrap();
        assert!(skill.contains(env!("CARGO_PKG_VERSION")));
        assert!(!skill.contains("{VERSION}"));
        assert!(report.config_created);
        assert!(report.gitignore_updated);
        assert!(std::fs::read_to_string(dir.path().join(".gitignore"))
            .unwrap()
            .contains(".reviewal/runs/"));
    }

    #[test]
    fn second_init_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path(), false).unwrap();
        let report = init(dir.path(), false).unwrap();
        assert!(matches!(report.skill, SkillOutcome::UpToDate));
        assert!(!report.gitignore_updated);
        assert!(!report.config_created);
        let gi = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert_eq!(gi.matches(".reviewal/runs/").count(), 1);
    }

    #[test]
    fn older_marker_upgrades_and_modified_skips() {
        let dir = tempfile::tempdir().unwrap();
        let report = init(dir.path(), false).unwrap();
        let path = report.skill_path.clone();

        let old = std::fs::read_to_string(&path)
            .unwrap()
            .replace(env!("CARGO_PKG_VERSION"), "0.0.1");
        std::fs::write(&path, old).unwrap();
        assert!(matches!(
            init(dir.path(), false).unwrap().skill,
            SkillOutcome::Upgraded
        ));

        std::fs::write(&path, "# user rewrote this entirely\n").unwrap();
        assert!(matches!(
            init(dir.path(), false).unwrap().skill,
            SkillOutcome::SkippedModified
        ));
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("user rewrote"));

        assert!(matches!(
            init(dir.path(), true).unwrap().skill,
            SkillOutcome::Installed
        ));
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("reviewal-version"));
    }

    #[test]
    fn ingest_skill_installed_reflects_skill_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!ingest_skill_installed(dir.path()));
        init(dir.path(), false).unwrap();
        assert!(ingest_skill_installed(dir.path()));
    }
}
