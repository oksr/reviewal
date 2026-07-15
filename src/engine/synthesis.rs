//! Deterministic synthesis of the per-persona reviews into one report. No
//! model is consulted here, on purpose: an LLM consensus pass would add spend,
//! one more way for a run to die, and the very bias all the reviewers share.
use crate::engine::model::{
    finding_id, norm_title, RawFinding, Round1Review, Round2Review, Severity, Verdict,
};
use crate::engine::store::{Triage, TriageEntry, TriageStatus};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Confidence {
    Disputed,
    CrossValidated,
    Consensus,
    Solo,
}

impl Confidence {
    pub(crate) fn rank(&self) -> u8 {
        match self {
            Confidence::Disputed => 0,
            Confidence::CrossValidated => 1,
            Confidence::Consensus => 2,
            Confidence::Solo => 3,
        }
    }

    pub(crate) fn section_title(&self) -> &'static str {
        match self {
            Confidence::CrossValidated => {
                "### Cross-validated — reported independently by more than one reviewer"
            }
            Confidence::Consensus => "### Consensus — one reporter, seconded in cross-review",
            Confidence::Disputed => "### Disputed — challenged in cross-review",
            Confidence::Solo => "### Solo — a single reviewer's read, uncorroborated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attribution {
    pub persona: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub severity: Severity,
    pub title: String,
    pub detail: String,
    pub file: Option<String>,
    pub line: Option<i64>,
    pub fix: Option<String>,
    pub reporters: Vec<String>,
    pub validators: Vec<Attribution>,
    pub challengers: Vec<Attribution>,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Synthesis {
    pub findings: Vec<Finding>,
    pub verdicts: BTreeMap<String, Verdict>,
    pub summaries: BTreeMap<String, String>,
    pub consensus_label: String,
    pub consensus_score: f64,
    pub degraded: Vec<String>,
}

/// The `#[serde(flatten)]` pins the on-disk `report.json` shape: finding
/// fields at the top level, `"triage": {"status", "note"}` alongside.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ReportFinding {
    #[serde(flatten)]
    pub finding: Finding,
    pub triage: TriageEntry,
}

/// The typed shape of `report.json` — the finalized-run boundary read back by
/// `load_done_state` and by external tooling (the reviewal-ingest skill).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Report {
    pub consensus_label: String,
    pub consensus_score: f64,
    pub verdicts: BTreeMap<String, Verdict>,
    pub summaries: BTreeMap<String, String>,
    pub degraded: Vec<String>,
    pub findings: Vec<ReportFinding>,
}

/// Matching is two-tier. An exact hit (same normalized title, same file, same
/// line) is preferred; failing that, a location-compatible hit merges — same
/// title where either side lacks a file, or the same file at a drifting line.
/// The same title in two *different* files is two findings, never one.
fn upsert(findings: &mut Vec<Finding>, persona: &str, raw: &RawFinding) {
    let norm = norm_title(&raw.title);
    let exact = findings
        .iter()
        .position(|f| norm_title(&f.title) == norm && f.file == raw.file && f.line == raw.line);
    let compatible = || {
        findings.iter().position(|f| {
            norm_title(&f.title) == norm
                && (f.file.is_none() || raw.file.is_none() || f.file == raw.file)
        })
    };
    match exact.or_else(compatible) {
        Some(i) => absorb(&mut findings[i], persona, raw),
        None => findings.push(open_finding(persona, raw)),
    }
}

/// Merge policy for a repeated finding: the roster gains the new reporter
/// once, the worst severity wins, the fullest explanation wins, and the
/// first concrete fix/file/line sticks — later reports fill gaps but never
/// overwrite what's already there.
fn absorb(f: &mut Finding, persona: &str, raw: &RawFinding) {
    if !f.reporters.iter().any(|r| r == persona) {
        f.reporters.push(persona.to_string());
    }
    if raw.severity.rank() < f.severity.rank() {
        f.severity = raw.severity;
    }
    if raw.detail.len() > f.detail.len() {
        f.detail = raw.detail.clone();
    }
    if f.fix.is_none() {
        f.fix = raw.fix.clone();
    }
    if f.file.is_none() {
        f.file = raw.file.clone();
    }
    if f.line.is_none() {
        f.line = raw.line;
    }
}

fn open_finding(persona: &str, raw: &RawFinding) -> Finding {
    Finding {
        id: finding_id(&raw.title, raw.file.as_deref(), raw.line),
        severity: raw.severity,
        title: raw.title.trim().to_string(),
        detail: raw.detail.clone(),
        file: raw.file.clone(),
        line: raw.line,
        fix: raw.fix.clone(),
        reporters: vec![persona.to_string()],
        validators: vec![],
        challengers: vec![],
        confidence: Confidence::Solo,
    }
}

fn consensus_label(verdicts: &BTreeMap<String, Verdict>) -> String {
    let total = verdicts.len();
    let ships = verdicts
        .values()
        .filter(|v| matches!(v, Verdict::Approve | Verdict::Conditional))
        .count();
    let blocks = verdicts
        .values()
        .filter(|v| matches!(v, Verdict::Reject))
        .count();
    let has_conditional = verdicts.values().any(|v| matches!(v, Verdict::Conditional));
    // ships + blocks == total (every verdict is exactly one of ship/block), so
    // blocks == 0 means "all ship" and ships == 0 means "all block". "split"
    // means the two printed counts are equal. The split arm is checked first so
    // an empty verdict set (0 == 0) still reads HOLD.
    match (ships, blocks) {
        (s, b) if s == b => {
            format!("HOLD — split decision ({s}/{total} ship, {b}/{total} block)")
        }
        (s, 0) if !has_conditional => format!("SHIP (unanimous, {s}/{total})"),
        (0, b) => format!("BLOCK (unanimous, {b}/{total})"),
        (s, b) if s > b => {
            let kind = if has_conditional {
                "SHIP-WITH-CAVEATS"
            } else {
                "SHIP"
            };
            format!("{kind} ({s}/{total} ship, {b}/{total} block)")
        }
        (s, b) => format!("BLOCK ({b}/{total} block, {s}/{total} ship)"),
    }
}

pub(crate) fn synthesize(
    round1: &BTreeMap<String, Round1Review>,
    round2: &BTreeMap<String, Round2Review>,
    failed: &[String],
) -> Synthesis {
    let mut findings: Vec<Finding> = Vec::new();

    for (persona, review) in round1 {
        for raw in &review.findings {
            upsert(&mut findings, persona, raw);
        }
    }
    for (persona, cross) in round2 {
        for raw in &cross.added {
            upsert(&mut findings, persona, raw);
        }
    }

    for (persona, cross) in round2 {
        for (entries, is_validate) in [(&cross.validate, true), (&cross.challenge, false)] {
            for entry in entries {
                let norm = norm_title(&entry.title);
                let Some(f) = findings.iter_mut().find(|f| norm_title(&f.title) == norm) else {
                    continue;
                };
                if f.reporters.iter().any(|r| r == persona) {
                    continue; // self-validation does not count
                }
                let bucket = if is_validate {
                    &mut f.validators
                } else {
                    &mut f.challengers
                };
                if bucket.iter().any(|a| a.persona == *persona) {
                    continue; // one attribution per persona per finding
                }
                bucket.push(Attribution {
                    persona: persona.clone(),
                    reason: entry.reason.trim().to_string(),
                });
            }
        }
    }

    for f in &mut findings {
        f.confidence = grade(f);
    }

    findings.sort_by(|a, b| {
        a.severity
            .rank()
            .cmp(&b.severity.rank())
            .then(a.confidence.rank().cmp(&b.confidence.rank()))
            .then(a.title.to_lowercase().cmp(&b.title.to_lowercase()))
    });

    let verdicts: BTreeMap<String, Verdict> =
        round1.iter().map(|(p, r)| (p.clone(), r.verdict)).collect();
    let summaries: BTreeMap<String, String> = round1
        .iter()
        .map(|(p, r)| (p.clone(), r.summary.chars().take(300).collect()))
        .collect();
    let score = if verdicts.is_empty() {
        0.0
    } else {
        verdicts.values().map(|v| v.score()).sum::<f64>() / verdicts.len() as f64
    };

    Synthesis {
        consensus_label: consensus_label(&verdicts),
        consensus_score: score,
        findings,
        verdicts,
        summaries,
        degraded: failed.to_vec(),
    }
}

/// A finding's confidence, judged from who stood where after cross-review: a
/// standing challenge always marks it disputed; independent co-reporting
/// outranks a validation; a lone report with neither is solo.
fn grade(f: &Finding) -> Confidence {
    let unchallenged = f.challengers.is_empty();
    match (unchallenged, f.reporters.len() >= 2, !f.validators.is_empty()) {
        (false, _, _) => Confidence::Disputed,
        (true, true, _) => Confidence::CrossValidated,
        (true, false, true) => Confidence::Consensus,
        (true, false, false) => Confidence::Solo,
    }
}

fn severity_marker(s: Severity) -> &'static str {
    match s {
        Severity::Critical => "🔴",
        Severity::Warning => "🟡",
        Severity::Info => "🔵",
    }
}

fn triage_status(triage: &Triage, id: &str) -> TriageStatus {
    triage
        .get(id)
        .map(|e| e.status)
        .unwrap_or(TriageStatus::Deferred)
}

fn render_finding(f: &Finding, lines: &mut Vec<String>) {
    let mut loc = String::new();
    if let Some(file) = &f.file {
        loc = match f.line {
            Some(n) => format!(" — `{file}:{n}`"),
            None => format!(" — `{file}`"),
        };
    }
    let sev = format!("{:?}", f.severity).to_lowercase();
    lines.push(format!(
        "#### {} `{}` {}{}",
        severity_marker(f.severity),
        sev,
        f.title,
        loc
    ));
    lines.push(String::new());
    lines.push(format!("_Filed by {}_", f.reporters.join(", ")));
    lines.push(String::new());
    lines.push(f.detail.clone());
    if let Some(fix) = &f.fix {
        lines.push(String::new());
        lines.push(format!("**Suggested fix:** {fix}"));
    }
    for v in &f.validators {
        lines.push(String::new());
        lines.push(format!("> ✅ seconded by **{}**: {}", v.persona, v.reason));
    }
    for c in &f.challengers {
        lines.push(String::new());
        lines.push(format!("> ⚠️ challenged by **{}**: {}", c.persona, c.reason));
    }
    lines.push(String::new());
}

pub(crate) fn render_markdown(syn: &Synthesis, triage: &Triage, title: &str) -> String {
    let mut lines = vec![format!("# {title}"), String::new()];
    let count = |s: Severity| syn.findings.iter().filter(|f| f.severity == s).count();
    lines.push(format!("**Consensus:** {}  ", syn.consensus_label));
    lines.push(format!(
        "**Findings:** {} critical, {} warning, {} info — {} total from {} reviewers",
        count(Severity::Critical),
        count(Severity::Warning),
        count(Severity::Info),
        syn.findings.len(),
        syn.verdicts.len()
    ));
    lines.push(String::new());
    lines.push("## Reviewer verdicts".into());
    lines.push(String::new());
    lines.push("| Reviewer | Verdict | Summary |".into());
    lines.push("|---|---|---|".into());
    for (p, v) in &syn.verdicts {
        let summary = syn
            .summaries
            .get(p)
            .cloned()
            .unwrap_or_default()
            .replace('|', "\\|");
        let verdict = format!("{v:?}").to_lowercase();
        lines.push(format!("| {p} | {verdict} | {summary} |"));
    }
    if !syn.degraded.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "> **Degraded run:** no usable review came back from: {}.",
            syn.degraded.join(", ")
        ));
    }
    lines.push(String::new());

    if syn.findings.is_empty() {
        lines.push("## Findings".into());
        lines.push(String::new());
        lines.push("_No findings — every reviewer came back clean._".into());
        return lines.join("\n") + "\n";
    }

    lines.push("## Accepted findings".into());
    lines.push(String::new());
    let mut any_accepted = false;
    for conf in [
        Confidence::CrossValidated,
        Confidence::Consensus,
        Confidence::Disputed,
        Confidence::Solo,
    ] {
        let group: Vec<&Finding> = syn
            .findings
            .iter()
            .filter(|f| {
                f.confidence == conf && triage_status(triage, &f.id) == TriageStatus::Accepted
            })
            .collect();
        if group.is_empty() {
            continue;
        }
        any_accepted = true;
        lines.push(conf.section_title().into());
        lines.push(String::new());
        for f in group {
            render_finding(f, &mut lines)
        }
    }
    if !any_accepted {
        lines.push("_None accepted._".into());
        lines.push(String::new());
    }

    let deferred: Vec<&Finding> = syn
        .findings
        .iter()
        .filter(|f| triage_status(triage, &f.id) == TriageStatus::Deferred)
        .collect();
    if !deferred.is_empty() {
        lines.push("## Deferred findings".into());
        lines.push(String::new());
        for f in deferred {
            render_finding(f, &mut lines)
        }
    }

    let dismissed: Vec<&Finding> = syn
        .findings
        .iter()
        .filter(|f| triage_status(triage, &f.id) == TriageStatus::Dismissed)
        .collect();
    if !dismissed.is_empty() {
        lines.push("## Dismissed during triage".into());
        lines.push(String::new());
        lines.push(
            "_Ruled out by the human reviewer; kept for context — do not re-raise without new evidence._"
                .into(),
        );
        lines.push(String::new());
        for f in dismissed {
            let note = triage
                .get(&f.id)
                .and_then(|e| e.note.clone())
                .unwrap_or_default();
            lines.push(format!(
                "- **{}** — {}",
                f.title,
                if note.is_empty() {
                    "(no note)".into()
                } else {
                    note
                }
            ));
        }
        lines.push(String::new());
    }

    lines.join("\n").trim_end().to_string() + "\n"
}

pub(crate) fn build_report(syn: &Synthesis, triage: &Triage) -> Report {
    let findings = syn
        .findings
        .iter()
        .map(|f| ReportFinding {
            finding: f.clone(),
            triage: triage.get(&f.id).cloned().unwrap_or(TriageEntry {
                status: TriageStatus::Deferred,
                note: None,
                touched: false,
            }),
        })
        .collect();
    Report {
        consensus_label: syn.consensus_label.clone(),
        consensus_score: syn.consensus_score,
        verdicts: syn.verdicts.clone(),
        summaries: syn.summaries.clone(),
        degraded: syn.degraded.clone(),
        findings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::model::*;
    use crate::engine::store::{Triage, TriageEntry, TriageStatus};
    use std::collections::BTreeMap;

    fn finding(sev: Severity, title: &str, detail: &str) -> RawFinding {
        RawFinding {
            severity: sev,
            file: Some("a.rs".into()),
            line: Some(1),
            title: title.into(),
            detail: detail.into(),
            fix: None,
        }
    }

    fn review(
        persona: &str,
        verdict: Verdict,
        findings: Vec<RawFinding>,
    ) -> (String, Round1Review) {
        (
            persona.to_string(),
            Round1Review {
                persona: persona.into(),
                verdict,
                summary: format!("{persona} summary"),
                findings,
            },
        )
    }

    #[test]
    fn merge_escalates_severity_and_keeps_longest_detail() {
        let round1 = BTreeMap::from([
            review(
                "prover",
                Verdict::Reject,
                vec![finding(Severity::Warning, "SQL Injection", "short")],
            ),
            review(
                "breaker",
                Verdict::Reject,
                vec![finding(
                    Severity::Critical,
                    "sql   injection.",
                    "much longer detail text",
                )],
            ),
        ]);
        let syn = synthesize(&round1, &BTreeMap::new(), &[]);
        assert_eq!(syn.findings.len(), 1);
        let f = &syn.findings[0];
        assert_eq!(f.severity, Severity::Critical);
        assert_eq!(f.detail, "much longer detail text");
        assert_eq!(f.reporters.len(), 2);
        assert_eq!(f.confidence, Confidence::CrossValidated);
        assert_eq!(f.id, finding_id("SQL Injection", Some("a.rs"), Some(1)));
    }

    #[test]
    fn validation_and_challenge_set_confidence() {
        let round1 = BTreeMap::from([
            review(
                "prover",
                Verdict::Approve,
                vec![finding(Severity::Warning, "Solo one", "d")],
            ),
            review(
                "breaker",
                Verdict::Approve,
                vec![finding(Severity::Warning, "Contested", "d")],
            ),
        ]);
        let round2 = BTreeMap::from([
            (
                "steward".to_string(),
                Round2Review {
                    persona: "steward".into(),
                    validate: vec![CrossEntry {
                        from: "prover".into(),
                        title: "Solo one".into(),
                        reason: "agree".into(),
                    }],
                    challenge: vec![CrossEntry {
                        from: "breaker".into(),
                        title: "Contested".into(),
                        reason: "overstated".into(),
                    }],
                    added: vec![],
                },
            ),
            // self-validation must not count
            (
                "prover".to_string(),
                Round2Review {
                    persona: "prover".into(),
                    validate: vec![CrossEntry {
                        from: "prover".into(),
                        title: "Solo one".into(),
                        reason: "me too".into(),
                    }],
                    challenge: vec![],
                    added: vec![finding(Severity::Info, "Added later", "from cross-review")],
                },
            ),
        ]);
        let syn = synthesize(&round1, &round2, &[]);
        let by_title: BTreeMap<_, _> = syn.findings.iter().map(|f| (f.title.clone(), f)).collect();
        assert_eq!(by_title["Solo one"].confidence, Confidence::Consensus);
        assert_eq!(by_title["Solo one"].validators.len(), 1); // self-validate dropped
        assert_eq!(by_title["Contested"].confidence, Confidence::Disputed);
        assert_eq!(by_title["Added later"].confidence, Confidence::Solo);
    }

    #[test]
    fn duplicate_cross_entries_from_one_persona_are_deduped() {
        let round1 = BTreeMap::from([review(
            "prover",
            Verdict::Approve,
            vec![finding(Severity::Warning, "Solo one", "d")],
        )]);
        let round2 = BTreeMap::from([(
            "steward".to_string(),
            Round2Review {
                persona: "steward".into(),
                validate: vec![
                    CrossEntry {
                        from: "prover".into(),
                        title: "Solo one".into(),
                        reason: "agree".into(),
                    },
                    CrossEntry {
                        from: "prover".into(),
                        title: "Solo one".into(),
                        reason: "again".into(),
                    },
                ],
                challenge: vec![],
                added: vec![],
            },
        )]);
        let syn = synthesize(&round1, &round2, &[]);
        let f = syn.findings.iter().find(|f| f.title == "Solo one").unwrap();
        assert_eq!(
            f.validators.len(),
            1,
            "one persona validates a finding at most once"
        );
        assert_eq!(f.confidence, Confidence::Consensus);
    }

    fn label(verdicts: &[Verdict]) -> String {
        let round1: BTreeMap<_, _> = verdicts
            .iter()
            .enumerate()
            .map(|(i, v)| review(&format!("p{i}"), *v, vec![]))
            .collect();
        synthesize(&round1, &BTreeMap::new(), &[]).consensus_label
    }

    #[test]
    fn label_unanimous_ship() {
        assert!(label(&[Verdict::Approve, Verdict::Approve]).starts_with("SHIP (unanimous"));
    }

    #[test]
    fn label_unanimous_block() {
        assert!(label(&[Verdict::Reject, Verdict::Reject]).starts_with("BLOCK (unanimous"));
    }

    #[test]
    fn label_split_is_a_true_tie() {
        assert_eq!(
            label(&[Verdict::Approve, Verdict::Reject]),
            "HOLD — split decision (1/2 ship, 1/2 block)"
        );
    }

    #[test]
    fn label_ship_with_caveats() {
        assert!(
            label(&[Verdict::Approve, Verdict::Conditional, Verdict::Reject])
                .starts_with("SHIP-WITH-CAVEATS")
        );
    }

    #[test]
    fn label_block_non_unanimous() {
        assert_eq!(
            label(&[Verdict::Reject, Verdict::Reject, Verdict::Conditional]),
            "BLOCK (2/3 block, 1/3 ship)"
        );
    }

    // A conditional-majority slate averages to score 0.0 but is a 2-vs-1 ship
    // majority; it must NOT read "split decision".
    #[test]
    fn label_conditional_majority_is_not_split() {
        assert_eq!(
            label(&[Verdict::Conditional, Verdict::Conditional, Verdict::Reject]),
            "SHIP-WITH-CAVEATS (2/3 ship, 1/3 block)"
        );
    }

    #[test]
    fn sort_order_and_degraded() {
        let round1 = BTreeMap::from([review(
            "prover",
            Verdict::Approve,
            vec![
                finding(Severity::Info, "zzz info", "d"),
                finding(Severity::Critical, "crit", "d"),
            ],
        )]);
        let syn = synthesize(&round1, &BTreeMap::new(), &["steward".to_string()]);
        assert_eq!(syn.findings[0].title, "crit");
        assert_eq!(syn.degraded, vec!["steward"]);
    }

    #[test]
    fn markdown_and_json_reflect_triage() {
        let round1 = BTreeMap::from([review(
            "prover",
            Verdict::Conditional,
            vec![
                finding(Severity::Critical, "Take me", "accepted detail"),
                finding(Severity::Warning, "Drop me", "dismissed detail"),
            ],
        )]);
        let syn = synthesize(&round1, &BTreeMap::new(), &[]);
        let mut triage = Triage::new();
        let accepted_id = syn
            .findings
            .iter()
            .find(|f| f.title == "Take me")
            .unwrap()
            .id
            .clone();
        let dismissed_id = syn
            .findings
            .iter()
            .find(|f| f.title == "Drop me")
            .unwrap()
            .id
            .clone();
        triage.insert(
            accepted_id,
            TriageEntry {
                status: TriageStatus::Accepted,
                note: None,
                touched: true,
            },
        );
        triage.insert(
            dismissed_id,
            TriageEntry {
                status: TriageStatus::Dismissed,
                note: Some("false positive".into()),
                touched: true,
            },
        );

        let md = render_markdown(&syn, &triage, "Adversarial Review");
        let accepted_pos = md.find("Take me").unwrap();
        let dismissed_pos = md.find("Drop me").unwrap();
        assert!(accepted_pos < dismissed_pos);
        assert!(md.contains("## Dismissed during triage"));
        assert!(md.contains("false positive"));

        let report = build_report(&syn, &triage);
        let statuses: Vec<TriageStatus> = report.findings.iter().map(|f| f.triage.status).collect();
        assert!(
            statuses.contains(&TriageStatus::Accepted)
                && statuses.contains(&TriageStatus::Dismissed)
        );
    }

    #[test]
    fn verdict_table_lowercases_verdict_but_preserves_summary_case() {
        let round1 = BTreeMap::from([(
            "prover".to_string(),
            Round1Review {
                persona: "prover".into(),
                verdict: Verdict::Reject,
                summary: "NaN in FooBar::new".into(),
                findings: vec![],
            },
        )]);
        let syn = synthesize(&round1, &BTreeMap::new(), &[]);
        let md = render_markdown(&syn, &Triage::new(), "Adversarial Review");
        assert!(
            md.contains("NaN in FooBar::new"),
            "mixed-case summary should survive verbatim:\n{md}"
        );
        assert!(
            md.contains("| prover | reject | NaN in FooBar::new |"),
            "verdict token should be lowercased, summary untouched:\n{md}"
        );
    }

    #[test]
    fn deferred_is_the_default_triage_status() {
        let round1 = BTreeMap::from([review(
            "prover",
            Verdict::Approve,
            vec![finding(
                Severity::Warning,
                "Untriaged finding",
                "detail body",
            )],
        )]);
        let syn = synthesize(&round1, &BTreeMap::new(), &[]);
        let triage = Triage::new();

        let md = render_markdown(&syn, &triage, "Adversarial Review");
        let deferred_header = md
            .find("## Deferred findings")
            .expect("deferred section must be present");
        let finding_pos = md
            .find("Untriaged finding")
            .expect("finding body must be rendered");
        assert!(
            deferred_header < finding_pos,
            "untriaged finding must render under the Deferred section:\n{md}"
        );

        // Serialize the typed report so the assertion still pins the on-disk
        // JSON encoding ("deferred"), not just the in-memory enum.
        let json = serde_json::to_value(build_report(&syn, &triage)).unwrap();
        let status = json["findings"][0]["triage"]["status"].as_str().unwrap();
        assert_eq!(status, "deferred");
    }

    #[test]
    fn no_accepted_findings_emits_none_accepted_placeholder() {
        let round1 = BTreeMap::from([review(
            "prover",
            Verdict::Approve,
            vec![finding(Severity::Warning, "Only deferred", "detail")],
        )]);
        let syn = synthesize(&round1, &BTreeMap::new(), &[]);
        let md = render_markdown(&syn, &Triage::new(), "Adversarial Review");
        assert!(
            md.contains("_None accepted._"),
            "expected the none-accepted placeholder:\n{md}"
        );
    }

    #[test]
    fn empty_findings_emits_clean_report() {
        let round1 = BTreeMap::from([review("prover", Verdict::Approve, vec![])]);
        let syn = synthesize(&round1, &BTreeMap::new(), &[]);
        assert!(syn.findings.is_empty());
        let md = render_markdown(&syn, &Triage::new(), "Adversarial Review");
        assert!(
            md.contains("_No findings — every reviewer came back clean._"),
            "expected the clean-run message:\n{md}"
        );
        assert!(
            !md.contains("## Deferred findings"),
            "clean report must not emit a Deferred section:\n{md}"
        );
        assert!(
            !md.contains("## Accepted findings"),
            "clean report must not emit an Accepted section:\n{md}"
        );
    }

    #[test]
    fn fallback_keeps_same_title_different_file_separate() {
        let round1 = BTreeMap::from([(
            "prover".to_string(),
            Round1Review {
                persona: "prover".into(),
                verdict: Verdict::Reject,
                summary: "prover summary".into(),
                findings: vec![
                    RawFinding {
                        severity: Severity::Warning,
                        file: Some("a.rs".into()),
                        line: Some(1),
                        title: "Missing error handling".into(),
                        detail: "in a.rs".into(),
                        fix: None,
                    },
                    RawFinding {
                        severity: Severity::Warning,
                        file: Some("b.rs".into()),
                        line: Some(5),
                        title: "Missing error handling".into(),
                        detail: "in b.rs".into(),
                        fix: None,
                    },
                ],
            },
        )]);
        let syn = synthesize(&round1, &BTreeMap::new(), &[]);
        assert_eq!(syn.findings.len(), 2, "distinct files must stay separate");
        for f in &syn.findings {
            assert_eq!(f.reporters.len(), 1);
            assert_eq!(f.confidence, Confidence::Solo);
        }
    }

    #[test]
    fn fallback_merges_same_title_different_line() {
        let round1 = BTreeMap::from([
            (
                "prover".to_string(),
                Round1Review {
                    persona: "prover".into(),
                    verdict: Verdict::Reject,
                    summary: "prover summary".into(),
                    findings: vec![RawFinding {
                        severity: Severity::Warning,
                        file: Some("a.rs".into()),
                        line: Some(10),
                        title: "Unchecked unwrap".into(),
                        detail: "d".into(),
                        fix: None,
                    }],
                },
            ),
            (
                "breaker".to_string(),
                Round1Review {
                    persona: "breaker".into(),
                    verdict: Verdict::Reject,
                    summary: "breaker summary".into(),
                    findings: vec![RawFinding {
                        severity: Severity::Warning,
                        file: Some("a.rs".into()),
                        line: Some(42),
                        title: "Unchecked unwrap".into(),
                        detail: "d".into(),
                        fix: None,
                    }],
                },
            ),
        ]);
        let syn = synthesize(&round1, &BTreeMap::new(), &[]);
        assert_eq!(
            syn.findings.len(),
            1,
            "same file, drifting line should merge"
        );
        assert_eq!(syn.findings[0].reporters.len(), 2);
        assert_eq!(syn.findings[0].confidence, Confidence::CrossValidated);
    }

    #[test]
    fn upsert_backfills_none_location_from_later_reporter() {
        let round1 = BTreeMap::from([
            (
                "prover".to_string(),
                Round1Review {
                    persona: "prover".into(),
                    verdict: Verdict::Reject,
                    summary: "prover summary".into(),
                    findings: vec![RawFinding {
                        severity: Severity::Warning,
                        file: None,
                        line: None,
                        title: "Race condition".into(),
                        detail: "d".into(),
                        fix: None,
                    }],
                },
            ),
            (
                "breaker".to_string(),
                Round1Review {
                    persona: "breaker".into(),
                    verdict: Verdict::Reject,
                    summary: "breaker summary".into(),
                    findings: vec![RawFinding {
                        severity: Severity::Warning,
                        file: Some("worker.rs".into()),
                        line: Some(7),
                        title: "Race condition".into(),
                        detail: "d".into(),
                        fix: Some("add a mutex".into()),
                    }],
                },
            ),
        ]);
        let syn = synthesize(&round1, &BTreeMap::new(), &[]);
        assert_eq!(syn.findings.len(), 1);
        let f = &syn.findings[0];
        assert_eq!(f.file.as_deref(), Some("worker.rs"));
        assert_eq!(f.line, Some(7));
        assert_eq!(f.fix.as_deref(), Some("add a mutex"));
        assert_eq!(f.reporters.len(), 2);
    }
}
