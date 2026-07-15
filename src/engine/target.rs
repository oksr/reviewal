//! Review targets — spec files or a git diff — collected into the single
//! source block every reviewer receives.
use crate::engine::model::TargetKind;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) const MAX_TOTAL_BYTES: usize = 250_000;
pub(crate) const MAX_FILE_BYTES: usize = 30_000;

#[derive(Debug, thiserror::Error)]
pub enum TargetError {
    #[error("failed to run git — is git installed?")]
    GitUnavailable(#[source] std::io::Error),
    #[error("git {cmd}: {stderr}")]
    GitFailed { cmd: String, stderr: String },
    #[error("--diff requires a git repository")]
    NotGitRepo,
    #[error("no spec files given")]
    NoSpecFiles,
    #[error("cannot read spec file {path}: {source}")]
    SpecRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the git diff is empty — nothing to review")]
    EmptyDiff,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Target {
    SpecFiles(Vec<PathBuf>),
    GitDiff { base: Option<String> },
}

impl Target {
    pub fn kind(&self) -> TargetKind {
        match self {
            Target::SpecFiles(_) => TargetKind::Spec,
            Target::GitDiff { .. } => TargetKind::Code,
        }
    }

    pub(crate) fn describe(&self) -> String {
        match self {
            Target::SpecFiles(paths) => format!(
                "spec: {}",
                paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Target::GitDiff { base: None } => "diff vs HEAD (uncommitted)".to_string(),
            Target::GitDiff { base: Some(b) } => format!("diff vs {b}"),
        }
    }
}

pub(crate) struct SourceBundle {
    pub block: String,
    #[cfg(test)]
    pub files: Vec<String>,
}

fn run_git(root: &Path, args: &[&str]) -> Result<String, TargetError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(TargetError::GitUnavailable)?;
    if !out.status.success() {
        return Err(TargetError::GitFailed {
            cmd: args.join(" "),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn truncate_bytes(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

fn collect_spec_files(paths: &[PathBuf], root: &Path) -> Result<SourceBundle, TargetError> {
    if paths.is_empty() {
        return Err(TargetError::NoSpecFiles);
    }
    let mut block = String::new();
    let mut files = Vec::new();
    for (i, rel) in paths.iter().enumerate() {
        let abs = root.join(rel);
        let text = std::fs::read_to_string(&abs).map_err(|source| TargetError::SpecRead {
            path: abs.clone(),
            source,
        })?;
        let (body, clipped) = truncate_bytes(&text, MAX_FILE_BYTES);
        let name = rel.display().to_string();
        let clip_note = if clipped {
            format!(" [clipped at {MAX_FILE_BYTES} bytes]")
        } else {
            String::new()
        };
        let section = format!("--- BEGIN FILE {name}{clip_note} ---\n{body}\n--- END FILE {name} ---\n");
        if block.len() + section.len() > MAX_TOTAL_BYTES {
            let omitted = paths.len() - i;
            block.push_str(&format!(
                "--- BUDGET EXHAUSTED: {omitted} file(s) omitted to stay under {MAX_TOTAL_BYTES} bytes ---\n"
            ));
            break;
        }
        block.push_str(&section);
        files.push(name);
    }
    Ok(SourceBundle {
        block,
        #[cfg(test)]
        files,
    })
}

fn collect_diff(base: Option<&str>, root: &Path) -> Result<SourceBundle, TargetError> {
    // Any git failure here maps to NotGitRepo: the caller-facing contract is
    // "--diff requires a git repository".
    run_git(root, &["rev-parse", "--is-inside-work-tree"]).map_err(|_| TargetError::NotGitRepo)?;
    let range = base.map(|b| format!("{b}...HEAD"));
    let scope = range.as_deref().unwrap_or("HEAD");
    let diff = run_git(root, &["diff", scope])?;
    if diff.trim().is_empty() {
        return Err(TargetError::EmptyDiff);
    }
    // Underscore-prefixed because only the `#[cfg(test)]` field of
    // `SourceBundle` consumes it; the git call itself stays unconditional.
    let _files: Vec<String> = run_git(root, &["diff", "--name-only", scope])?
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    let (body, clipped) = truncate_bytes(&diff, MAX_TOTAL_BYTES);
    let tail = if clipped {
        format!("\n[diff clipped at {MAX_TOTAL_BYTES} bytes]\n")
    } else {
        String::new()
    };
    let block = format!(
        "Unified git diff of the change under review. Judge what the diff adds, removes, \
         or modifies; unchanged context lines are there for orientation only.\n\n\
         ```diff\n{body}{tail}\n```\n"
    );
    Ok(SourceBundle {
        block,
        #[cfg(test)]
        files: _files,
    })
}

/// Cheap diff preflight — no formatted review block is built. Emptiness of
/// `git diff --name-only` is equivalent to `collect_diff`'s full-diff check,
/// and the guards (`NotGitRepo`, `EmptyDiff`) must stay in sync with it.
pub(crate) fn precheck_diff(base: Option<&str>, root: &Path) -> Result<(), TargetError> {
    run_git(root, &["rev-parse", "--is-inside-work-tree"]).map_err(|_| TargetError::NotGitRepo)?;
    let range = base.map(|b| format!("{b}...HEAD"));
    let scope = range.as_deref().unwrap_or("HEAD");
    if run_git(root, &["diff", "--name-only", scope])?.trim().is_empty() {
        return Err(TargetError::EmptyDiff);
    }
    Ok(())
}

pub(crate) fn collect(target: &Target, root: &Path) -> Result<SourceBundle, TargetError> {
    match target {
        Target::SpecFiles(paths) => collect_spec_files(paths, root),
        Target::GitDiff { base } => collect_diff(base.as_deref(), root),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DetectedTarget {
    pub target: Target,
    pub label: String,
    pub files: Vec<String>,
    pub additions: u64,
    pub deletions: u64,
}

/// Binary files report `-` in numstat; they count as a file with 0/0 lines.
fn diff_stats(root: &Path, range_args: &[&str]) -> Result<(Vec<String>, u64, u64), TargetError> {
    let mut args = vec!["diff", "--numstat"];
    args.extend_from_slice(range_args);
    let out = run_git(root, &args)?;
    let (mut files, mut add, mut del) = (Vec::new(), 0u64, 0u64);
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        let mut cols = line.splitn(3, '\t');
        let a = cols.next().unwrap_or("-");
        let d = cols.next().unwrap_or("-");
        let Some(name) = cols.next() else { continue };
        add += a.parse::<u64>().unwrap_or(0);
        del += d.parse::<u64>().unwrap_or(0);
        files.push(name.to_string());
    }
    Ok((files, add, del))
}

/// Detection is based on the same view `collect` uses (`git diff HEAD`, no
/// `git add -N`) so an untracked-only tree is never mis-detected as reviewable
/// and then rejected at preflight; a candidate base only counts once it both
/// exists and HEAD is ahead of its merge-base.
pub(crate) fn detect_targets(root: &Path) -> Vec<DetectedTarget> {
    let mut out = Vec::new();
    if run_git(root, &["rev-parse", "--is-inside-work-tree"]).is_err() {
        return out;
    }
    if let Ok((files, additions, deletions)) = diff_stats(root, &["HEAD"]) {
        if !files.is_empty() {
            out.push(DetectedTarget {
                target: Target::GitDiff { base: None },
                label: "diff vs HEAD (uncommitted)".into(),
                files,
                additions,
                deletions,
            });
        }
    }
    for base in ["main", "master"] {
        if run_git(root, &["rev-parse", "--verify", base]).is_err() {
            continue;
        }
        let (Ok(head), Ok(base_rev)) = (
            run_git(root, &["rev-parse", "HEAD"]),
            run_git(root, &["rev-parse", base]),
        ) else {
            break;
        };
        if head.trim() == base_rev.trim() {
            continue; // on this base branch itself — try the next base
        }
        let Ok(merge_base) = run_git(root, &["merge-base", base, "HEAD"]) else {
            break;
        };
        if merge_base.trim() == head.trim() {
            continue; // not ahead of this base — try the next
        }
        let range = format!("{base}...HEAD");
        if let Ok((files, additions, deletions)) = diff_stats(root, &[&range]) {
            if !files.is_empty() {
                out.push(DetectedTarget {
                    target: Target::GitDiff {
                        base: Some(base.to_string()),
                    },
                    label: format!("diff vs {base}"),
                    files,
                    additions,
                    deletions,
                });
                break; // first base we're AHEAD OF wins
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let st = Command::new("git")
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

    fn init_repo(dir: &std::path::Path) {
        git(dir, &["init", "-b", "main"]);
        git(dir, &["config", "user.email", "t@t"]);
        git(dir, &["config", "user.name", "t"]);
    }

    #[test]
    fn spec_block_includes_file_header_and_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "alpha spec").unwrap();
        let t = Target::SpecFiles(vec!["a.md".into()]);
        let b = collect(&t, dir.path()).unwrap();
        assert!(b.block.contains("--- BEGIN FILE a.md ---\nalpha spec"));
        assert!(b.block.contains("--- END FILE a.md ---"));
        assert_eq!(b.files, vec!["a.md"]);
    }

    #[test]
    fn spec_block_truncates_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.md"), "x".repeat(MAX_FILE_BYTES + 100)).unwrap();
        let b = collect(&Target::SpecFiles(vec!["big.md".into()]), dir.path()).unwrap();
        assert!(b.block.contains("[clipped at"));
    }

    #[test]
    fn spec_target_kind_is_spec() {
        let t = Target::SpecFiles(vec!["a.md".into()]);
        assert_eq!(t.kind(), crate::engine::model::TargetKind::Spec);
    }

    #[test]
    fn truncate_bytes_backs_off_to_char_boundary() {
        // "é" (U+00E9) is 2 bytes in UTF-8, so byte boundaries sit at 0,2,4,6,8.
        // max = 5 lands mid-codepoint and must walk back to byte 4.
        let s = "é".repeat(4); // 8 bytes, 4 codepoints
        let (out, truncated) = truncate_bytes(&s, 5);
        assert!(truncated, "8-byte string over a 5-byte cap must truncate");
        assert_eq!(out, "éé", "cut walks back from byte 5 to the boundary at 4");
        assert_eq!(
            out.len(),
            4,
            "result is the largest whole-codepoint prefix ≤ 5 bytes"
        );
        assert!(out.len() <= 5);
        // Slicing at a non-boundary would have panicked; reaching here proves it did not,
        // and the &str is valid UTF-8 by construction.
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn spec_files_truncated_when_total_budget_exceeded() {
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(MAX_FILE_BYTES);
        let n = MAX_TOTAL_BYTES / MAX_FILE_BYTES + 2; // enough files to blow the total budget
        let mut names = Vec::new();
        for i in 0..n {
            let name = format!("f{i}.md");
            std::fs::write(dir.path().join(&name), &big).unwrap();
            names.push(std::path::PathBuf::from(name));
        }
        let t = Target::SpecFiles(names);
        let b = collect(&t, dir.path()).unwrap();
        assert!(
            b.block.contains("--- BUDGET EXHAUSTED:"),
            "total-budget branch must emit the omitted-files marker"
        );
        assert!(
            b.files.len() < n,
            "not every supplied file should be emitted"
        );
    }

    #[test]
    fn missing_spec_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let t = Target::SpecFiles(vec!["nope.md".into()]);
        assert!(collect(&t, dir.path()).is_err());
    }

    #[test]
    fn diff_on_clean_tree_errors() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("f.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "c1"]);
        assert!(
            collect(&Target::GitDiff { base: None }, dir.path()).is_err(),
            "clean tree → empty diff → error"
        );
    }

    #[test]
    fn diff_collects_uncommitted_changes() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("f.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "c1"]);
        std::fs::write(dir.path().join("f.txt"), "two\n").unwrap();
        let b = collect(&Target::GitDiff { base: None }, dir.path()).unwrap();
        assert!(b.block.contains("```diff"));
        assert!(b.block.contains("-one"));
        assert_eq!(b.files, vec!["f.txt"]);
    }

    #[test]
    fn empty_diff_is_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("f.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "c1"]);

        assert!(matches!(
            collect(&Target::GitDiff { base: None }, dir.path()),
            Err(TargetError::EmptyDiff)
        ));
    }

    #[test]
    fn precheck_diff_matches_collect_guards() {
        // Non-git dir → Err (same NotGitRepo guard as collect_diff).
        let plain = tempfile::tempdir().unwrap();
        assert!(matches!(
            precheck_diff(None, plain.path()),
            Err(TargetError::NotGitRepo)
        ));

        // Initialized repo, clean tree → Err (same EmptyDiff guard).
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("f.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "c1"]);
        assert!(matches!(
            precheck_diff(None, dir.path()),
            Err(TargetError::EmptyDiff)
        ));

        // Same repo after an uncommitted edit → Ok, matching collect's accept.
        std::fs::write(dir.path().join("f.txt"), "two\n").unwrap();
        assert!(precheck_diff(None, dir.path()).is_ok());
        assert!(collect(&Target::GitDiff { base: None }, dir.path()).is_ok());
    }

    #[test]
    fn untracked_only_repo_not_detected_as_uncommitted_diff() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        // A commit must exist so HEAD is valid (mirrors collect_diff's `git diff HEAD`).
        std::fs::write(dir.path().join("tracked.txt"), "base\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "c1"]);

        std::fs::write(dir.path().join("untracked.txt"), "new\n").unwrap();

        // Detection must not claim a reviewable uncommitted diff: `git diff HEAD`
        // (what collection uses) excludes untracked files.
        assert!(detect_targets(dir.path()).is_empty());

        // Collection agrees — the same scope errors.
        let t = Target::GitDiff { base: None };
        assert!(collect(&t, dir.path()).is_err());
    }

    #[test]
    fn detect_targets_reports_stats_for_uncommitted_diff() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-b", "main"]);
        git(root, &["config", "user.email", "t@t"]);
        git(root, &["config", "user.name", "t"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "init"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();

        let targets = detect_targets(root);
        assert_eq!(targets.len(), 1);
        let t = &targets[0];
        assert_eq!(t.target, Target::GitDiff { base: None });
        assert_eq!(t.label, "diff vs HEAD (uncommitted)");
        assert_eq!(t.files, vec!["a.txt".to_string()]);
        assert_eq!((t.additions, t.deletions), (2, 0));
    }

    #[test]
    fn detect_targets_reports_branch_diff_vs_main() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-b", "main"]);
        git(root, &["config", "user.email", "t@t"]);
        git(root, &["config", "user.name", "t"]);
        std::fs::write(root.join("a.txt"), "base\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "init"]);
        git(root, &["checkout", "-b", "feature"]);
        std::fs::write(root.join("b.txt"), "new\nnew\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "feat"]);

        let targets = detect_targets(root);
        assert_eq!(targets.len(), 1, "clean tree: only the branch diff");
        let t = &targets[0];
        assert_eq!(
            t.target,
            Target::GitDiff {
                base: Some("main".into())
            }
        );
        assert_eq!(t.label, "diff vs main");
        assert_eq!(t.files, vec!["b.txt".to_string()]);
        assert_eq!((t.additions, t.deletions), (2, 0));
    }

    #[test]
    fn detect_targets_lists_both_when_dirty_on_a_branch() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-b", "main"]);
        git(root, &["config", "user.email", "t@t"]);
        git(root, &["config", "user.name", "t"]);
        std::fs::write(root.join("a.txt"), "base\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "init"]);
        git(root, &["checkout", "-b", "feature"]);
        std::fs::write(root.join("b.txt"), "new\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "feat"]);
        std::fs::write(root.join("a.txt"), "dirty\n").unwrap();

        let targets = detect_targets(root);
        let labels: Vec<&str> = targets.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, vec!["diff vs HEAD (uncommitted)", "diff vs main"]);
    }

    #[test]
    fn detect_targets_empty_outside_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_targets(dir.path()).is_empty());
    }

    #[test]
    fn detect_targets_falls_through_to_master_when_head_is_main() {
        // Both `main` and `master` exist; HEAD sits on `main` (HEAD == main's
        // rev, so that base is unusable) but is ahead of `master`. The base
        // loop must fall through to `master` instead of giving up after `main`
        // — first base you're AHEAD OF wins, not first base that exists.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-b", "master"]);
        git(root, &["config", "user.email", "t@t"]);
        git(root, &["config", "user.name", "t"]);
        std::fs::write(root.join("a.txt"), "base\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "init"]);
        git(root, &["checkout", "-b", "main"]);
        std::fs::write(root.join("b.txt"), "new\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "second"]);

        let targets = detect_targets(root);
        assert_eq!(targets.len(), 1, "must find the diff vs master");
        let t = &targets[0];
        assert_eq!(
            t.target,
            Target::GitDiff {
                base: Some("master".into())
            }
        );
        assert_eq!(t.label, "diff vs master");
        assert_eq!(t.files, vec!["b.txt".to_string()]);
    }
}
