use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Severity {
    Critical,
    Warning,
    Info,
}

impl Severity {
    pub(crate) fn rank(self) -> u8 {
        match self {
            Severity::Critical => 0,
            Severity::Warning => 1,
            Severity::Info => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Verdict {
    Approve,
    Conditional,
    Reject,
}

impl Verdict {
    pub(crate) fn score(self) -> f64 {
        match self {
            Verdict::Approve => 1.0,
            Verdict::Conditional => 0.5,
            Verdict::Reject => -1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum TargetKind {
    Code,
    Spec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RawFinding {
    pub severity: Severity,
    pub file: Option<String>,
    pub line: Option<i64>,
    pub title: String,
    pub detail: String,
    pub fix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FindingBrief {
    pub severity: Severity,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Round1Review {
    pub persona: String,
    pub verdict: Verdict,
    pub summary: String,
    pub findings: Vec<RawFinding>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CrossEntry {
    pub from: String,
    pub title: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Round2Review {
    pub persona: String,
    pub validate: Vec<CrossEntry>,
    pub challenge: Vec<CrossEntry>,
    pub added: Vec<RawFinding>,
}

pub(crate) fn norm_title(t: &str) -> String {
    let collapsed = t
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    collapsed
        .trim_end_matches(['.', ':', ',', ';', '!', '?'])
        .to_string()
}

pub(crate) fn finding_id(title: &str, file: Option<&str>, line: Option<i64>) -> String {
    use std::fmt::Write;
    let key = format!(
        "{}|{}|{}",
        norm_title(title),
        file.unwrap_or(""),
        line.map(|n| n.to_string()).unwrap_or_default()
    );
    let digest = Sha256::digest(key.as_bytes());
    let mut id = String::with_capacity(12);
    for b in &digest[..6] {
        let _ = write!(id, "{b:02x}");
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&Severity::Critical).unwrap(),
            "\"critical\""
        );
    }

    #[test]
    fn verdict_deserializes_from_lowercase() {
        let v: Verdict = serde_json::from_str("\"conditional\"").unwrap();
        assert_eq!(v, Verdict::Conditional);
    }

    #[test]
    fn severity_rejects_unknown_variant() {
        assert!(serde_json::from_str::<Severity>("\"fatal\"").is_err());
    }

    #[test]
    fn boundary_enums_wire_format_unchanged() {
        // #[non_exhaustive] must not alter serialization of the boundary enums.
        assert_eq!(
            serde_json::to_string(&Severity::Critical).unwrap(),
            "\"critical\""
        );
        assert_eq!(
            serde_json::to_string(&Verdict::Conditional).unwrap(),
            "\"conditional\""
        );
        assert_eq!(
            serde_json::to_string(&TargetKind::Code).unwrap(),
            "\"code\""
        );
    }

    #[test]
    fn round1_review_roundtrip_with_nulls() {
        let json = r#"{"persona":"prover","verdict":"approve","summary":"ok","findings":[
            {"severity":"info","file":null,"line":null,"title":"t","detail":"d","fix":null}]}"#;
        let r: Round1Review = serde_json::from_str(json).unwrap();
        assert_eq!(r.findings[0].file, None);
        assert_eq!(r.verdict.score(), 1.0);
        assert_eq!(r.findings[0].severity.rank(), 2);
    }

    #[test]
    fn norm_title_collapses_and_strips() {
        assert_eq!(
            norm_title("  SQL   Injection in query builder.  "),
            "sql injection in query builder"
        );
    }

    #[test]
    fn finding_id_stable_and_normalized() {
        let a = finding_id("SQL Injection", Some("db.py"), Some(22));
        let b = finding_id("sql   injection.", Some("db.py"), Some(22));
        assert_eq!(a, b);
        assert_eq!(a.len(), 12);
        assert_ne!(a, finding_id("SQL Injection", Some("db.py"), Some(23)));
    }
}
