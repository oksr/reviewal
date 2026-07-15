//! Fallback behind `--json-schema`: agents behind wrappers can still emit
//! fenced/wrapped/bannered JSON around the object. Malformed JSON is
//! deliberately not repaired — retry-with-feedback is the contract.
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct ParseError(pub String);

pub(crate) fn extract_json(stdout: &str) -> Result<Value, ParseError> {
    if stdout.trim().is_empty() {
        return Err(ParseError("agent emitted no output".into()));
    }
    if let Some(v) = parse_document(stdout) {
        return Ok(v);
    }
    // Hunt for an embedded object: every `{` is a candidate start, and serde's
    // stream deserializer reads exactly one complete value — interior braces,
    // escaped quotes, and prose go through a real parser, not a scanner.
    for (idx, _) in stdout.match_indices('{') {
        let mut stream =
            serde_json::Deserializer::from_str(&stdout[idx..]).into_iter::<Value>();
        if let Some(Ok(v)) = stream.next() {
            return Ok(peel(v));
        }
    }
    let hint = if stdout.contains('{') {
        "output contains '{' but no parseable JSON object"
    } else {
        "no JSON object in agent output"
    };
    Err(ParseError(format!("{hint}\n---\n{}", clip(stdout))))
}

fn parse_document(text: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(unfence(text)).ok()?;
    Some(peel(v))
}

fn unfence(text: &str) -> &str {
    let doc = text.trim();
    let Some(opened) = doc.strip_prefix("```") else {
        return doc;
    };
    let body = opened.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    body.strip_suffix("```").map_or(doc, str::trim)
}

/// Unwraps the envelopes agents actually produce: a top-level JSON string,
/// Claude Code's `{"result": "<json>"}`, and the API content-block array.
/// One level only; an envelope whose payload isn't valid JSON is returned as-is.
fn peel(v: Value) -> Value {
    if let Value::String(inner) = &v {
        return serde_json::from_str(unfence(inner)).unwrap_or(v);
    }
    match envelope_payload(&v) {
        Some(payload) => serde_json::from_str(unfence(payload)).unwrap_or(v),
        None => v,
    }
}

fn envelope_payload(v: &Value) -> Option<&str> {
    if let Some(s) = v.get("result").and_then(Value::as_str) {
        return Some(s);
    }
    v.get("content")?
        .as_array()?
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .find_map(|b| b.get("text").and_then(Value::as_str))
}

fn clip(s: &str) -> String {
    match s.char_indices().nth(500) {
        None => s.to_string(),
        Some((cut, _)) => format!("{}…", &s[..cut]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every file in the contract-fixture directory must extract to the same
    /// round-1 object. Adding a fixture file extends this suite.
    #[test]
    fn contract_fixtures_all_extract() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/agent-output");
        let mut seen = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let raw = std::fs::read_to_string(&path).unwrap();
            let v = extract_json(&raw).unwrap_or_else(|e| panic!("fixture {path:?}: {e}"));
            assert_eq!(v["persona"], "prover", "fixture {path:?}");
            seen += 1;
        }
        assert!(seen >= 5, "expected at least 5 fixtures, found {seen}");
    }

    #[test]
    fn bare_object_passes_through() {
        let v = extract_json(r#"{"persona":"prover","findings":[]}"#).unwrap();
        assert_eq!(v["persona"], "prover");
    }

    #[test]
    fn top_level_json_string_is_parsed_as_its_content() {
        let v = extract_json(r#""{\"persona\":\"prover\"}""#).unwrap();
        assert_eq!(v["persona"], "prover");
    }

    #[test]
    fn result_envelope_is_peeled_once() {
        let v = extract_json(r#"{"result":"{\"persona\":\"prover\"}"}"#).unwrap();
        assert_eq!(v["persona"], "prover");
    }

    #[test]
    fn content_block_envelope_is_peeled() {
        let v = extract_json(
            r#"{"content":[{"type":"tool_use"},{"type":"text","text":"{\"persona\":\"prover\"}"}]}"#,
        )
        .unwrap();
        assert_eq!(v["persona"], "prover");
    }

    #[test]
    fn object_after_prose_with_interior_braces_is_found() {
        let v = extract_json(concat!(
            "spawning reviewer } ok\n",
            r#"{"persona":"prover","detail":"see {inner} and }"}"#,
        ))
        .unwrap();
        assert_eq!(v["persona"], "prover");
        assert_eq!(v["detail"], "see {inner} and }");
    }

    #[test]
    fn escaped_quotes_inside_strings_survive_the_hunt() {
        // Leading prose defeats the whole-document parse; the \" inside
        // "detail" is content, and the } after it must not close the object.
        let v = extract_json("boot }\n{\"persona\":\"prover\",\"detail\":\"a \\\" b }\"}")
            .unwrap();
        assert_eq!(v["detail"], "a \" b }");
    }

    #[test]
    fn unbalanced_brace_in_prose_does_not_mask_the_object() {
        let v = extract_json("progress { still going\n{\"persona\":\"prover\"}").unwrap();
        assert_eq!(v["persona"], "prover");
    }

    #[test]
    fn later_object_wins_when_an_earlier_candidate_is_invalid() {
        let v = extract_json("cfg = {broken json}\n{\"persona\":\"prover\"}").unwrap();
        assert_eq!(v["persona"], "prover");
    }

    #[test]
    fn blank_input_is_an_error() {
        assert!(extract_json(" \n\t ").is_err());
    }

    #[test]
    fn braceless_garbage_errors_with_a_preview() {
        let err = extract_json("plain words, nothing else").unwrap_err();
        assert!(err.0.contains("no JSON object"));
        assert!(err.0.contains("plain words"));
    }

    #[test]
    fn long_garbage_preview_is_clipped() {
        let noise = "x".repeat(2_000);
        let err = extract_json(&noise).unwrap_err();
        assert!(err.0.len() < 700, "preview not clipped: {} bytes", err.0.len());
        assert!(err.0.contains('…'));
    }
}
