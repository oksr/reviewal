use crate::engine::model::{Round1Review, Round2Review};
use crate::engine::target::Target;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub(crate) enum RunStatus {
    Running,
    ReviewsComplete,
    Finalized,
    Aborted,
    Stale,
}

impl RunStatus {
    /// Human-readable label, byte-identical to the serde kebab-case encoding
    /// (locked by `run_status_label_matches_serde_encoding`).
    pub(crate) fn label(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::ReviewsComplete => "reviews-complete",
            RunStatus::Finalized => "finalized",
            RunStatus::Aborted => "aborted",
            RunStatus::Stale => "stale",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RunRecord {
    pub id: String,
    pub created_at: String,
    pub target: Target,
    pub target_desc: String,
    pub personas: Vec<String>,
    pub model: Option<String>,
    pub cross_review: bool,
    pub status: RunStatus,
    pub degraded: Vec<String>,
    /// Synthesized finding count, set when the run reaches ReviewsComplete.
    #[serde(default)]
    pub findings_total: Option<usize>,
    /// Consensus label, set at finalize (Home history shows it verbatim).
    #[serde(default)]
    pub verdict_label: Option<String>,
    /// Accepted-finding count, set at finalize.
    #[serde(default)]
    pub accepted_count: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub(crate) enum TriageStatus {
    Accepted,
    Dismissed,
    Deferred,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct TriageEntry {
    pub status: TriageStatus,
    pub note: Option<String>,
    /// True once the user explicitly acted (accept/dismiss/skip). Untouched
    /// Deferred entries are the triage inbox; touched Deferred = "skipped".
    #[serde(default)]
    pub touched: bool,
}

pub(crate) type Triage = BTreeMap<String, TriageEntry>;

pub(crate) struct RunStore {
    root: PathBuf,
}

pub(crate) struct RunDir {
    pub path: PathBuf,
}

impl RunStore {
    pub(crate) fn new(project_root: &Path) -> RunStore {
        RunStore {
            root: project_root.join(".reviewal"),
        }
    }

    fn runs_dir(&self) -> PathBuf {
        self.root.join("runs")
    }

    pub(crate) fn new_run_id(slug: &str, now_utc: &str) -> String {
        format!("{}-{slug}", now_utc.replace(':', "-"))
    }

    pub(crate) fn create_run(&self, record: &RunRecord) -> anyhow::Result<RunDir> {
        let path = self.runs_dir().join(&record.id);
        std::fs::create_dir_all(path.join("round1"))
            .with_context(|| format!("creating {}", path.join("round1").display()))?;
        std::fs::create_dir_all(path.join("round2"))
            .with_context(|| format!("creating {}", path.join("round2").display()))?;
        let dir = RunDir { path };
        dir.save_record(record)?;
        Ok(dir)
    }

    pub(crate) fn open_run(&self, id: &str) -> anyhow::Result<RunDir> {
        let path = self.runs_dir().join(id);
        let run_json = path.join("run.json");
        anyhow::ensure!(
            run_json
                .try_exists()
                .with_context(|| format!("checking {}", run_json.display()))?,
            "run {id} not found"
        );
        Ok(RunDir { path })
    }

    pub(crate) fn list_runs(&self) -> (Vec<RunRecord>, Vec<String>) {
        let Ok(entries) = std::fs::read_dir(self.runs_dir()) else {
            return (vec![], vec![]);
        };
        let mut runs: Vec<RunRecord> = vec![];
        let mut warnings: Vec<String> = vec![];
        for e in entries.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
            let run_json = e.path().join("run.json");
            let run_id = e.file_name().to_string_lossy().to_string();
            let text = match std::fs::read_to_string(&run_json) {
                Ok(t) => t,
                Err(err) => {
                    warnings.push(format!("skipped unreadable run {}: {err}", run_id));
                    continue;
                }
            };
            match serde_json::from_str::<RunRecord>(&text) {
                Ok(rec) => runs.push(rec),
                Err(err) => {
                    warnings.push(format!("skipped corrupt run {}: {err}", run_id));
                }
            }
        }
        runs.sort_by(|a, b| b.id.cmp(&a.id));
        (runs, warnings)
    }

    /// Flips leftover Running runs to Stale, returning ONLY the save-failure
    /// warnings. The internal `list_runs` scan's corruption warnings are
    /// deliberately discarded here: the Home screen's own `list_runs` call is
    /// the single surface for those, so forwarding them would show each one
    /// twice per Home build.
    pub(crate) fn mark_stale(&self) -> Vec<String> {
        let mut warnings: Vec<String> = vec![];
        let (runs, _scan_warnings) = self.list_runs();
        for mut rec in runs {
            if rec.status == RunStatus::Running {
                rec.status = RunStatus::Stale;
                if let Ok(dir) = self.open_run(&rec.id) {
                    if let Err(e) = dir.save_record(&rec) {
                        warnings.push(format!("failed to mark run {} stale: {e:#}", rec.id));
                    }
                }
            }
        }
        warnings
    }

    pub(crate) fn set_latest(&self, id: &str) -> anyhow::Result<()> {
        std::fs::create_dir_all(self.runs_dir())?;
        write_atomic(&self.runs_dir().join("latest"), id.as_bytes())
            .context("writing latest pointer")
    }

    /// `runs/latest` is a plain-text pointer file whose production consumer is
    /// outside this crate (the reviewal-ingest skill reads it directly), so
    /// only tests call this in-crate.
    #[cfg(test)]
    pub(crate) fn latest(&self) -> Option<String> {
        std::fs::read_to_string(self.runs_dir().join("latest"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

impl RunDir {
    pub(crate) fn save_record(&self, record: &RunRecord) -> anyhow::Result<()> {
        write_json(&self.path.join("run.json"), record)
    }

    pub(crate) fn load_record(&self) -> anyhow::Result<RunRecord> {
        let p = self.path.join("run.json");
        let text =
            std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", p.display()))
    }

    pub(crate) fn save_source(&self, block: &str) -> anyhow::Result<()> {
        write_atomic(&self.path.join("source.txt"), block.as_bytes()).context("writing source.txt")
    }

    // Test-only sugar over `save_round`/`save_round_raw`; production calls
    // the round-generic methods directly.
    #[cfg(test)]
    pub(crate) fn save_round1(
        &self,
        persona: &str,
        parsed: &serde_json::Value,
        raw: &str,
    ) -> anyhow::Result<()> {
        self.save_round(1, persona, parsed, raw)
    }

    #[cfg(test)]
    pub(crate) fn save_round2(
        &self,
        persona: &str,
        parsed: &serde_json::Value,
        raw: &str,
    ) -> anyhow::Result<()> {
        self.save_round(2, persona, parsed, raw)
    }

    /// For a persona whose response failed validation: no `.json` is written,
    /// but the raw text is kept for post-mortem inspection.
    #[cfg(test)]
    pub(crate) fn save_round1_raw(&self, persona: &str, raw: &str) -> anyhow::Result<()> {
        self.save_round_raw(1, persona, raw)
    }

    #[cfg(test)]
    pub(crate) fn save_round2_raw(&self, persona: &str, raw: &str) -> anyhow::Result<()> {
        self.save_round_raw(2, persona, raw)
    }

    pub(crate) fn save_round_raw(&self, n: u8, persona: &str, raw: &str) -> anyhow::Result<()> {
        let dir = self.path.join(format!("round{n}"));
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        write_atomic(&dir.join(format!("{persona}.raw.txt")), raw.as_bytes())
    }

    pub(crate) fn save_round(
        &self,
        n: u8,
        persona: &str,
        parsed: &serde_json::Value,
        raw: &str,
    ) -> anyhow::Result<()> {
        let dir = self.path.join(format!("round{n}"));
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        write_json(&dir.join(format!("{persona}.json")), parsed)?;
        write_atomic(&dir.join(format!("{persona}.raw.txt")), raw.as_bytes())?;
        Ok(())
    }

    pub(crate) fn load_round1(&self) -> anyhow::Result<BTreeMap<String, Round1Review>> {
        self.load_round(1)
    }

    pub(crate) fn load_round2(&self) -> anyhow::Result<BTreeMap<String, Round2Review>> {
        self.load_round(2)
    }

    fn load_round<T: serde::de::DeserializeOwned>(
        &self,
        n: u8,
    ) -> anyhow::Result<BTreeMap<String, T>> {
        let dir = self.path.join(format!("round{n}"));
        let mut out = BTreeMap::new();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(out);
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                let persona = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                out.insert(
                    persona,
                    serde_json::from_str(&text)
                        .with_context(|| format!("parsing {}", path.display()))?,
                );
            }
        }
        Ok(out)
    }

    pub(crate) fn save_triage(&self, triage: &Triage) -> anyhow::Result<()> {
        write_json(&self.path.join("triage.json"), triage)
    }

    /// A missing `triage.json` is the normal first-run state (empty map); a
    /// present-but-corrupt file is an error so the caller does not silently
    /// overwrite a recoverable file on the next save.
    pub(crate) fn load_triage(&self) -> anyhow::Result<Triage> {
        let p = self.path.join("triage.json");
        match std::fs::read_to_string(&p) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parsing {}", p.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Triage::new()),
            Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
        }
    }

    pub(crate) fn write_report(&self, md: &str, json: &serde_json::Value) -> anyhow::Result<()> {
        write_atomic(&self.path.join("report.md"), md.as_bytes())?;
        write_json(&self.path.join("report.json"), json)
    }
}

/// Atomic replace: sibling temp file + fsync + rename, so a reader sees either
/// the old or the new complete file — never a truncated one. The temp name
/// embeds the target's file name, so concurrent writes of *different* files in
/// one directory never share a temp path. Two concurrent writers of the *same*
/// path are still unsafe — the crate never does that.
fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let file_name = path
        .file_name()
        .with_context(|| format!("path has no file name: {}", path.display()))?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    // Full file name, not `with_extension("tmp")`, which would collapse
    // report.md and report.json onto one temp path.
    let tmp = dir.join(format!(".{}.tmp", file_name.to_string_lossy()));
    {
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("syncing {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let bytes = serde_json::to_string_pretty(value)
        .with_context(|| format!("serializing {}", path.display()))?;
    write_atomic(path, bytes.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::model::*;
    use crate::engine::target::Target;
    use serde_json::json;

    fn record(id: &str, status: RunStatus) -> RunRecord {
        RunRecord {
            id: id.into(),
            created_at: "2026-07-07T12:00:00Z".into(),
            target: Target::GitDiff { base: None },
            target_desc: "diff vs HEAD (uncommitted)".into(),
            personas: vec!["prover".into()],
            model: None,
            cross_review: false,
            status,
            degraded: vec![],
            findings_total: None,
            verdict_label: None,
            accepted_count: None,
        }
    }

    fn open(store: &RunStore) -> RunDir {
        store
            .create_run(&record("2026-07-07T12-00-00Z-x", RunStatus::Running))
            .unwrap()
    }

    #[test]
    fn run_record_new_fields_default_when_absent_from_old_json() {
        let json = r#"{"id":"x","created_at":"2026-07-07T12:00:00Z",
            "target":{"GitDiff":{"base":null}},"target_desc":"diff vs HEAD (uncommitted)",
            "personas":["prover"],"model":null,"cross_review":false,
            "status":"finalized","degraded":[]}"#;
        let rec: RunRecord = serde_json::from_str(json).unwrap();
        assert_eq!(rec.findings_total, None);
        assert_eq!(rec.verdict_label, None);
        assert_eq!(rec.accepted_count, None);
    }

    #[test]
    fn triage_entry_touched_defaults_false() {
        let e: TriageEntry = serde_json::from_str(r#"{"status":"accepted","note":null}"#).unwrap();
        assert!(!e.touched);
    }

    #[test]
    fn list_runs_returns_corruption_warnings_instead_of_printing() {
        let dir = tempfile::tempdir().unwrap();
        let store = RunStore::new(dir.path());
        store
            .create_run(&record("2026-07-07T12-00-00Z-a", RunStatus::Finalized))
            .unwrap();
        let bad = dir.path().join(".reviewal/runs/2026-07-08T12-00-00Z-b");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("run.json"), "{not json").unwrap();
        let (runs, warnings) = store.list_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("2026-07-08T12-00-00Z-b"),
            "warning names the run: {warnings:?}"
        );
    }

    #[test]
    fn run_status_label_matches_serde_encoding() {
        for status in [
            RunStatus::Running,
            RunStatus::ReviewsComplete,
            RunStatus::Finalized,
            RunStatus::Aborted,
            RunStatus::Stale,
        ] {
            assert_eq!(
                format!("\"{}\"", status.label()),
                serde_json::to_string(&status).unwrap(),
                "label() must stay byte-identical to the serde encoding"
            );
        }
    }

    #[test]
    fn run_id_is_sortable_and_sluggy() {
        let id = RunStore::new_run_id("diff-main", "2026-07-07T12:30:05Z");
        assert_eq!(id, "2026-07-07T12-30-05Z-diff-main");
    }

    #[test]
    fn save_source_writes_source_txt() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = open(&RunStore::new(tmp.path()));
        dir.save_source("SRC").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path.join("source.txt")).unwrap(),
            "SRC"
        );
    }

    #[test]
    fn round1_saves_parsed_and_raw_then_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = open(&RunStore::new(tmp.path()));
        dir.save_round1(
            "prover",
            &json!({"persona":"prover","verdict":"approve","summary":"s","findings":[]}),
            "raw out",
        )
        .unwrap();
        let r1 = dir.load_round1().unwrap();
        assert_eq!(r1["prover"].verdict, Verdict::Approve);
        assert!(dir.path.join("round1/prover.raw.txt").exists());
    }

    #[test]
    fn round2_saves_parsed_and_raw_then_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = open(&RunStore::new(tmp.path()));
        dir.save_round2(
            "prover",
            &json!({"persona":"prover","validate":[],"challenge":[],"added":[]}),
            "raw out",
        )
        .unwrap();
        let r2 = dir.load_round2().unwrap();
        assert_eq!(r2["prover"].persona, "prover");
        assert!(dir.path.join("round2/prover.raw.txt").exists());
    }

    #[test]
    fn round2_raw_only_omits_json() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = open(&RunStore::new(tmp.path()));
        dir.save_round2_raw("breaker", "not valid json").unwrap();
        assert!(dir.path.join("round2/breaker.raw.txt").exists());
        assert!(!dir.path.join("round2/breaker.json").exists());
        assert!(dir.load_round2().unwrap().is_empty());
    }

    #[test]
    fn triage_saves_and_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = open(&RunStore::new(tmp.path()));
        let mut triage = Triage::new();
        triage.insert(
            "abc".into(),
            TriageEntry {
                status: TriageStatus::Accepted,
                note: None,
                touched: false,
            },
        );
        dir.save_triage(&triage).unwrap();
        assert_eq!(
            dir.load_triage().unwrap()["abc"].status,
            TriageStatus::Accepted
        );
    }

    #[test]
    fn write_report_creates_md_and_json() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = open(&RunStore::new(tmp.path()));
        dir.write_report("# md", &json!({"ok":true})).unwrap();
        assert!(dir.path.join("report.md").exists());
        assert!(dir.path.join("report.json").exists());
    }

    #[test]
    fn set_latest_points_to_run_id() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RunStore::new(tmp.path());
        let rec = record("2026-07-07T12-00-00Z-x", RunStatus::Running);
        store.create_run(&rec).unwrap();
        store.set_latest(&rec.id).unwrap();
        assert_eq!(store.latest().unwrap(), rec.id);
    }

    #[test]
    fn list_runs_returns_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RunStore::new(tmp.path());
        store
            .create_run(&record("2026-07-07T10-00-00Z-a", RunStatus::Finalized))
            .unwrap();
        store
            .create_run(&record("2026-07-07T11-00-00Z-b", RunStatus::Running))
            .unwrap();
        let (runs, _warnings) = store.list_runs();
        assert_eq!(runs[0].id, "2026-07-07T11-00-00Z-b");
        assert_eq!(runs[1].id, "2026-07-07T10-00-00Z-a");
    }

    #[test]
    fn mark_stale_touches_only_running_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RunStore::new(tmp.path());
        store
            .create_run(&record("2026-07-07T10-00-00Z-a", RunStatus::Finalized))
            .unwrap();
        store
            .create_run(&record("2026-07-07T11-00-00Z-b", RunStatus::Running))
            .unwrap();
        let _warnings = store.mark_stale();
        let (runs, _) = store.list_runs(); // newest first: b, a
        assert_eq!(runs[0].status, RunStatus::Stale, "Running → Stale");
        assert_eq!(runs[1].status, RunStatus::Finalized, "Finalized untouched");
    }

    #[test]
    fn load_round_excludes_persona_with_only_raw_output() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RunStore::new(tmp.path());
        let rec = record("2026-07-07T12-00-00Z-degraded", RunStatus::Running);
        let dir = store.create_run(&rec).unwrap();

        dir.save_round1_raw("breaker", "not valid json — model went off the rails")
            .unwrap();
        dir.save_round1(
            "prover",
            &json!({"persona":"prover","verdict":"approve","summary":"s","findings":[]}),
            "raw out",
        )
        .unwrap();

        // Raw dump kept for post-mortem; only the validated persona loads.
        assert!(dir.path.join("round1/breaker.raw.txt").exists());
        let r1 = dir.load_round1().unwrap();
        assert_eq!(r1.len(), 1);
        assert!(r1.contains_key("prover"));
        assert!(!r1.contains_key("breaker"));
    }

    #[test]
    fn open_run_missing_run_json_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RunStore::new(tmp.path());
        std::fs::create_dir_all(store.runs_dir().join("bare-run")).unwrap();

        // `.err()` instead of `.unwrap_err()`: RunDir has no Debug impl.
        let err = store
            .open_run("bare-run")
            .err()
            .expect("open_run must fail without run.json");
        assert!(err.to_string().contains("not found"));
    }

    fn assert_no_temp_files(dir: &Path) {
        for entry in std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            assert!(!name.ends_with(".tmp"), "leftover temp file: {name}");
        }
    }

    #[test]
    fn atomic_write_roundtrips_and_leaves_no_temp() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RunStore::new(tmp.path());
        let rec = record("2026-07-07T12-00-00Z-atomic", RunStatus::Running);
        let dir = store.create_run(&rec).unwrap();

        dir.save_round1(
            "prover",
            &json!({"persona":"prover","verdict":"approve","summary":"s","findings":[]}),
            "raw out",
        )
        .unwrap();
        dir.write_report("# md", &json!({"ok":true})).unwrap();

        assert!(dir.path.join("round1/prover.json").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path.join("round1/prover.raw.txt")).unwrap(),
            "raw out"
        );
        // report.md and report.json must hold their *distinct* contents —
        // guards the per-target temp-name scheme against collisions.
        assert_eq!(
            std::fs::read_to_string(dir.path.join("report.md")).unwrap(),
            "# md"
        );
        let report: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path.join("report.json")).unwrap())
                .unwrap();
        assert_eq!(report, json!({"ok":true}));
        assert_no_temp_files(&dir.path);
        assert_no_temp_files(&dir.path.join("round1"));
    }

    #[test]
    fn atomic_write_is_unique_per_target_under_concurrency() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RunStore::new(tmp.path());
        let rec = record("2026-07-07T12-00-00Z-conc", RunStatus::Running);
        let dir = std::sync::Arc::new(store.create_run(&rec).unwrap());

        // Concurrent writers into round1/: the temp path must be unique per
        // target or writers clobber each other.
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let dir = std::sync::Arc::clone(&dir);
                std::thread::spawn(move || {
                    let persona = format!("persona{i}");
                    dir.save_round1(
                        &persona,
                        &json!({
                            "persona": persona,
                            "verdict": "approve",
                            "summary": format!("summary {i}"),
                            "findings": [],
                        }),
                        &format!("raw {i}"),
                    )
                    .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let r1 = dir.load_round1().unwrap();
        assert_eq!(r1.len(), 8);
        for i in 0..8 {
            let persona = format!("persona{i}");
            assert_eq!(r1[&persona].summary, format!("summary {i}"));
            assert_eq!(
                std::fs::read_to_string(dir.path.join(format!("round1/{persona}.raw.txt")))
                    .unwrap(),
                format!("raw {i}")
            );
        }
        assert_no_temp_files(&dir.path.join("round1"));
    }

    #[test]
    fn load_record_missing_file_reports_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = RunDir {
            path: tmp.path().to_path_buf(),
        };
        let err = dir.load_record().unwrap_err();
        assert!(
            err.to_string().contains("run.json"),
            "error should name the missing file, got: {err:#}"
        );
    }

    #[test]
    fn list_runs_skips_corrupt_run_json() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RunStore::new(tmp.path());
        store
            .create_run(&record("2026-07-07T10-00-00Z-good", RunStatus::Finalized))
            .unwrap();
        let corrupt = store.runs_dir().join("2026-07-07T11-00-00Z-bad");
        std::fs::create_dir_all(&corrupt).unwrap();
        std::fs::write(corrupt.join("run.json"), "not json{").unwrap();

        let (runs, _warnings) = store.list_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, "2026-07-07T10-00-00Z-good");
    }

    #[test]
    fn load_triage_missing_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RunStore::new(tmp.path());
        let rec = record("2026-07-07T12-00-00Z-fresh", RunStatus::ReviewsComplete);
        let dir = store.create_run(&rec).unwrap();
        assert!(dir.load_triage().unwrap().is_empty());
    }

    #[test]
    fn load_triage_corrupt_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RunStore::new(tmp.path());
        let rec = record("2026-07-07T12-00-00Z-corrupt", RunStatus::ReviewsComplete);
        let dir = store.create_run(&rec).unwrap();
        std::fs::write(dir.path.join("triage.json"), "not json{").unwrap();
        let err = dir.load_triage().unwrap_err();
        assert!(
            err.to_string().contains("triage.json"),
            "error should name the corrupt file, got: {err:#}"
        );
    }
}
