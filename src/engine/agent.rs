use serde_json::Value;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

pub(crate) struct AgentConfig {
    pub claude_bin: String,
    pub model: Option<String>,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub enum AgentActivity {
    TextDelta(String),
    ToolUse(String),
    /// `exact: false` counts are chars-derived estimates — the stream reports
    /// real usage only at message boundaries. Counts are cumulative within one
    /// subprocess invocation and reset with the next (a retry, or round 2).
    Tokens { count: u64, exact: bool },
}

#[derive(Debug)]
pub(crate) struct AgentOutcome {
    pub result_text: String,
    pub duration: Duration,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AgentError {
    #[error("agent not found: {0} — is the claude CLI on PATH?")]
    Spawn(String),
    #[error("timed out after {0:.1?}")]
    Timeout(Duration),
    #[error("agent exited with code {code}: {stderr}")]
    NonZeroExit { code: i32, stderr: String },
    #[error("agent produced no result")]
    EmptyResult,
    #[error(
        "agent output had no recognized stream-json event types — the claude CLI output format may have changed"
    )]
    UnrecognizedStream,
    #[error("failed reading agent output: {0}")]
    Read(#[from] std::io::Error),
}

/// The claude CLI's `stream-json` format is unversioned, and this list is the
/// single place that assumption lives: a format change would otherwise read
/// as "the model returned nothing" ([`AgentError::UnrecognizedStream`]).
const KNOWN_EVENT_TYPES: [&str; 4] = ["system", "stream_event", "assistant", "result"];

/// Rough chars-per-token ratio for the live counter estimate, used because
/// the stream reports real usage only at message boundaries.
const CHARS_PER_TOKEN_ESTIMATE: u64 = 4;

/// Observed name of the `--json-schema` answer block: its input_json_delta
/// stream IS the review, feeding the panel and token estimate. If the CLI
/// renames this tool, the counter regresses to 0 until each persona finishes.
const STRUCTURED_OUTPUT_TOOL: &str = "StructuredOutput";

/// The stream carries real usage only at the final `message_delta`; mid-stream
/// assistant snapshots just echo the message_start bootstrap value, so the
/// counter is estimated from streamed chars and snapped to usage on arrival.
#[derive(Default)]
struct TokenProgress {
    usage_tokens: u64,
    streamed_chars: u64,
    emitted: u64,
}

impl TokenProgress {
    fn on_streamed_text(&mut self, text: &str) -> Option<AgentActivity> {
        self.streamed_chars += text.chars().count() as u64;
        let estimate = self.streamed_chars / CHARS_PER_TOKEN_ESTIMATE;
        (estimate > self.emitted).then(|| {
            self.emitted = estimate;
            AgentActivity::Tokens {
                count: estimate,
                exact: false,
            }
        })
    }

    /// End-of-message usage is authoritative: snap the counter to it even
    /// when the estimate overshot.
    fn on_final_usage(&mut self, tokens: u64) -> AgentActivity {
        self.usage_tokens = self.usage_tokens.max(tokens);
        self.emitted = self.usage_tokens;
        AgentActivity::Tokens {
            count: self.usage_tokens,
            exact: true,
        }
    }

    /// Usage on a mid-stream assistant snapshot: a value below the running
    /// counter is the message_start bootstrap echoed back, not progress.
    fn on_snapshot_usage(&mut self, tokens: u64) -> Option<AgentActivity> {
        self.usage_tokens = self.usage_tokens.max(tokens);
        (self.usage_tokens > self.emitted).then(|| {
            self.emitted = self.usage_tokens;
            AgentActivity::Tokens {
                count: self.usage_tokens,
                exact: true,
            }
        })
    }
}

pub(crate) async fn invoke(
    cfg: &AgentConfig,
    system_prompt: &str,
    json_schema: &Value,
    prompt: &str,
    mut on_activity: impl FnMut(AgentActivity) + Send,
) -> Result<AgentOutcome, AgentError> {
    let start = Instant::now();
    let mut cmd = Command::new(&cfg.claude_bin);
    // Security invariant: --safe-mode, empty --tools, and no session
    // persistence — text inside the reviewed artifact must not be able to
    // trigger tools, hooks, or MCP.
    cmd.args([
        "-p",
        "--verbose",
        "--output-format",
        "stream-json",
        "--include-partial-messages",
        "--safe-mode",
        "--no-session-persistence",
        "--tools",
        "",
        "--system-prompt",
        system_prompt,
        "--json-schema",
        &json_schema.to_string(),
    ]);
    if let Some(model) = &cfg.model {
        cmd.args(["--model", model]);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| AgentError::Spawn(format!("{}: {e}", cfg.claude_bin)))?;

    let mut stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    // Drain stderr in its own task: a CLI writing more than a pipe buffer's
    // worth to stderr blocks until someone reads it — draining after
    // `child.wait()` would deadlock against the stdout loop.
    let stderr_reader = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf).await;
        buf
    });

    // Write the prompt, then close stdin so the CLI starts.
    let prompt_owned = prompt.to_string();
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(prompt_owned.as_bytes()).await;
    });

    let read_loop = async {
        let mut lines = BufReader::new(stdout).lines();
        let mut result_text: Option<String> = None;
        let mut fallback_text = String::new();
        let mut tokens = TokenProgress::default();
        // Blocks stream one at a time, so a bool suffices.
        let mut in_structured_output = false;
        let mut read_err: Option<std::io::Error> = None;
        let mut saw_json_event = false;
        let mut saw_known_event = false;
        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(e) => {
                    read_err = Some(e);
                    break;
                }
            };
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            saw_json_event = true;
            let event_type = event.get("type").and_then(Value::as_str);
            if event_type.is_some_and(|t| KNOWN_EVENT_TYPES.contains(&t)) {
                saw_known_event = true;
            }
            match event_type {
                Some("stream_event") => {
                    let ev = &event["event"];
                    match ev.get("type").and_then(Value::as_str) {
                        Some("content_block_delta") => {
                            // Under --json-schema a persona may stream only
                            // the answer block's input_json_delta — drop it
                            // and the persona reads as dead until the end.
                            let delta_text = if ev["delta"]["type"] == "text_delta" {
                                ev["delta"]["text"].as_str()
                            } else if ev["delta"]["type"] == "thinking_delta" {
                                ev["delta"]["thinking"].as_str()
                            } else if ev["delta"]["type"] == "input_json_delta"
                                && in_structured_output
                            {
                                ev["delta"]["partial_json"].as_str()
                            } else {
                                None
                            };
                            if let Some(t) = delta_text {
                                on_activity(AgentActivity::TextDelta(t.to_string()));
                                if let Some(a) = tokens.on_streamed_text(t) {
                                    on_activity(a);
                                }
                            }
                        }
                        Some("content_block_start") => {
                            let block = &ev["content_block"];
                            in_structured_output = block["type"] == "tool_use"
                                && block["name"] == STRUCTURED_OUTPUT_TOOL;
                            if block["type"] == "tool_use" {
                                if let Some(name) = block["name"].as_str() {
                                    on_activity(AgentActivity::ToolUse(name.to_string()));
                                }
                            }
                        }
                        Some("content_block_stop") => in_structured_output = false,
                        Some("message_delta") => {
                            if let Some(n) = ev["usage"]["output_tokens"].as_u64() {
                                on_activity(tokens.on_final_usage(n));
                            }
                        }
                        _ => {}
                    }
                }
                Some("assistant") => {
                    if let Some(n) = event["message"]["usage"]["output_tokens"].as_u64() {
                        if let Some(a) = tokens.on_snapshot_usage(n) {
                            on_activity(a);
                        }
                    }
                    if let Some(blocks) = event["message"]["content"].as_array() {
                        for b in blocks {
                            if b["type"] == "text" {
                                if let Some(t) = b["text"].as_str() {
                                    fallback_text.push_str(t);
                                }
                            }
                        }
                    }
                }
                Some("result") => {
                    if let Some(r) = event["result"].as_str() {
                        result_text = Some(r.to_string());
                    }
                }
                _ => {}
            }
        }
        (
            result_text,
            fallback_text,
            read_err,
            saw_json_event,
            saw_known_event,
        )
    };

    let (result_text, fallback_text, read_err, saw_json_event, saw_known_event) =
        match tokio::time::timeout(cfg.timeout, read_loop).await {
            Ok(out) => out,
            Err(_) => {
                let _ = child.kill().await;
                writer.abort();
                return Err(AgentError::Timeout(cfg.timeout));
            }
        };
    // stdout has closed, so the prompt can't matter anymore — abort rather
    // than await: if the child (or a fork inheriting its stdin) never
    // drained the pipe, the blocked write would stall here unboundedly.
    writer.abort();

    let status = match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(status)) => status,
        _ => {
            let _ = child.kill().await;
            return Err(AgentError::Timeout(Duration::from_secs(5)));
        }
    };

    if !status.success() {
        let err_buf = stderr_reader.await.unwrap_or_default();
        let mut stderr_trim: String = err_buf.trim().chars().take(400).collect();
        if stderr_trim.is_empty() {
            stderr_trim = "(no stderr)".into();
        }
        return Err(AgentError::NonZeroExit {
            code: status.code().unwrap_or(-1),
            stderr: stderr_trim,
        });
    }

    let text = result_text.unwrap_or(fallback_text);
    if text.trim().is_empty() {
        if let Some(e) = read_err {
            return Err(AgentError::Read(e));
        }
        if saw_json_event && !saw_known_event {
            return Err(AgentError::UnrecognizedStream);
        }
        return Err(AgentError::EmptyResult);
    }
    Ok(AgentOutcome {
        result_text: text,
        duration: start.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn script(dir: &std::path::Path, body: &str) -> String {
        let path = dir.join("fake-agent.sh");
        std::fs::write(&path, format!("#!/bin/bash\n{body}")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path.display().to_string()
    }

    fn cfg(bin: String, timeout_ms: u64) -> AgentConfig {
        AgentConfig {
            claude_bin: bin,
            model: Some("test-model".into()),
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    #[tokio::test]
    async fn happy_path_streams_activity_and_returns_result() {
        let dir = tempfile::tempdir().unwrap();
        let bin = script(
            dir.path(),
            r#"
cat > /dev/null
echo '{"type":"system","subtype":"init"}'
echo '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"pondering "}}}'
echo '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"thinking..."}}}'
echo '{"type":"stream_event","event":{"type":"message_delta","delta":{},"usage":{"output_tokens":7}}}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"{\"persona\":\"prover\"}"}],"usage":{"output_tokens":42}}}'
echo 'not json noise'
echo '{"type":"result","subtype":"success","result":"{\"persona\":\"prover\"}"}'
"#,
        );
        let mut activities = Vec::new();
        let out = invoke(
            // Generous timeout: the script finishes in milliseconds, but the
            // full parallel suite can starve subprocess tests for seconds.
            &cfg(bin, 30_000),
            "sys",
            &json!({"type":"object"}),
            "prompt",
            &mut |a| activities.push(a),
        )
        .await
        .unwrap();
        assert_eq!(out.result_text, "{\"persona\":\"prover\"}");
        assert!(
            activities.iter().any(|a| matches!(
                a,
                AgentActivity::Tokens {
                    count: 42,
                    exact: true
                }
            )),
            "assistant message usage drives the peak token counter"
        );
        assert!(activities
            .iter()
            .any(|a| matches!(a, AgentActivity::TextDelta(t) if t == "thinking...")));
        assert!(
            activities
                .iter()
                .any(|a| matches!(a, AgentActivity::TextDelta(t) if t == "pondering ")),
            "thinking deltas surface as stream text"
        );
        assert!(
            activities.iter().any(|a| matches!(
                a,
                AgentActivity::Tokens {
                    count: 7,
                    exact: true
                }
            )),
            "message_delta usage drives the token counter"
        );
    }

    #[test]
    fn token_progress_estimates_suppresses_bootstrap_and_snaps() {
        let mut tp = TokenProgress::default();
        assert!(matches!(
            tp.on_streamed_text(&"x".repeat(40)),
            Some(AgentActivity::Tokens {
                count: 10,
                exact: false
            })
        ));
        assert!(tp.on_snapshot_usage(5).is_none());
        assert!(tp.on_streamed_text("xy").is_none());
        assert!(matches!(
            tp.on_snapshot_usage(50),
            Some(AgentActivity::Tokens {
                count: 50,
                exact: true
            })
        ));
        tp.on_streamed_text(&"x".repeat(400));
        assert!(matches!(
            tp.on_final_usage(80),
            AgentActivity::Tokens {
                count: 80,
                exact: true
            }
        ));
    }

    #[tokio::test]
    async fn estimated_count_ignores_bootstrap_usage_and_snaps_to_final() {
        let dir = tempfile::tempdir().unwrap();
        let text = "x".repeat(400);
        let body = format!(
            r#"
cat > /dev/null
echo '{{"type":"stream_event","event":{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"{text}"}}}}}}'
echo '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{text}"}}],"usage":{{"output_tokens":5}}}}}}'
echo '{{"type":"stream_event","event":{{"type":"message_delta","delta":{{}},"usage":{{"output_tokens":80}}}}}}'
echo '{{"type":"result","subtype":"success","result":"done"}}'
"#
        );
        let bin = script(dir.path(), &body);
        let mut activities = Vec::new();
        invoke(
            &cfg(bin, 30_000),
            "sys",
            &json!({"type":"object"}),
            "prompt",
            &mut |a| activities.push(a),
        )
        .await
        .unwrap();
        let tokens: Vec<(u64, bool)> = activities
            .iter()
            .filter_map(|a| match a {
                AgentActivity::Tokens { count, exact } => Some((*count, *exact)),
                _ => None,
            })
            .collect();
        assert!(
            tokens.contains(&(100, false)),
            "streamed chars/4 estimate is emitted live: {tokens:?}"
        );
        assert!(
            !tokens.iter().any(|(count, _)| *count == 5),
            "bootstrap usage below the estimate is ignored: {tokens:?}"
        );
        assert_eq!(
            tokens.last(),
            Some(&(80, true)),
            "final message_delta usage snaps the counter to the real count"
        );
    }

    #[tokio::test]
    async fn structured_output_json_deltas_drive_counter_and_text() {
        let dir = tempfile::tempdir().unwrap();
        let json_chunk = "x".repeat(200);
        let foreign_chunk = "y".repeat(100);
        let body = format!(
            r#"
cat > /dev/null
echo '{{"type":"stream_event","event":{{"type":"content_block_start","content_block":{{"type":"tool_use","name":"StructuredOutput"}}}}}}'
echo '{{"type":"stream_event","event":{{"type":"content_block_delta","delta":{{"type":"input_json_delta","partial_json":"{json_chunk}"}}}}}}'
echo '{{"type":"stream_event","event":{{"type":"content_block_delta","delta":{{"type":"input_json_delta","partial_json":"{json_chunk}"}}}}}}'
echo '{{"type":"stream_event","event":{{"type":"content_block_stop"}}}}'
echo '{{"type":"stream_event","event":{{"type":"content_block_start","content_block":{{"type":"tool_use","name":"Bash"}}}}}}'
echo '{{"type":"stream_event","event":{{"type":"content_block_delta","delta":{{"type":"input_json_delta","partial_json":"{foreign_chunk}"}}}}}}'
echo '{{"type":"stream_event","event":{{"type":"message_delta","delta":{{}},"usage":{{"output_tokens":80}}}}}}'
echo '{{"type":"result","subtype":"success","result":"done"}}'
"#
        );
        let bin = script(dir.path(), &body);
        let mut activities = Vec::new();
        invoke(
            &cfg(bin, 30_000),
            "sys",
            &json!({"type":"object"}),
            "prompt",
            &mut |a| activities.push(a),
        )
        .await
        .unwrap();
        let tokens: Vec<(u64, bool)> = activities
            .iter()
            .filter_map(|a| match a {
                AgentActivity::Tokens { count, exact } => Some((*count, *exact)),
                _ => None,
            })
            .collect();
        assert!(
            tokens.contains(&(100, false)),
            "input_json_delta chars/4 drive the live estimate: {tokens:?}"
        );
        assert_eq!(
            tokens.last(),
            Some(&(80, true)),
            "final usage still snaps the counter"
        );
        assert!(
            activities
                .iter()
                .any(|a| matches!(a, AgentActivity::TextDelta(t) if t.starts_with("xxx"))),
            "streamed JSON surfaces as panel text so the persona shows life"
        );
        assert!(
            !activities
                .iter()
                .any(|a| matches!(a, AgentActivity::TextDelta(t) if t.contains("yyy"))),
            "argument bytes from a non-StructuredOutput tool stay off the panel"
        );
        assert!(
            !tokens.contains(&(125, false)),
            "foreign tool input does not advance the estimate: {tokens:?}"
        );
    }

    #[tokio::test]
    async fn falls_back_to_assistant_text_when_no_result_event() {
        let dir = tempfile::tempdir().unwrap();
        let bin = script(
            dir.path(),
            r#"
cat > /dev/null
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"fallback text"}],"usage":{"output_tokens":1}}}'
"#,
        );
        let out = invoke(&cfg(bin, 30_000), "s", &json!({}), "p", &mut |_| {})
            .await
            .unwrap();
        assert_eq!(out.result_text, "fallback text");
    }

    #[tokio::test]
    async fn unrecognized_event_vocabulary_surfaces_as_format_drift_not_empty() {
        let dir = tempfile::tempdir().unwrap();
        let bin = script(
            dir.path(),
            r#"cat > /dev/null
echo '{"type":"message_start_v2","payload":{}}'
echo '{"type":"result_v2","payload":{"text":"{}"}}'
"#,
        );
        let err = invoke(&cfg(bin, 30_000), "s", &json!({}), "p", &mut |_| {})
            .await
            .unwrap_err();
        assert!(
            matches!(err, AgentError::UnrecognizedStream),
            "expected UnrecognizedStream, got {err:?}"
        );
    }

    #[tokio::test]
    async fn silent_agent_is_still_empty_result_not_format_drift() {
        let dir = tempfile::tempdir().unwrap();
        let bin = script(dir.path(), "cat > /dev/null\n");
        let err = invoke(&cfg(bin, 30_000), "s", &json!({}), "p", &mut |_| {})
            .await
            .unwrap_err();
        assert!(
            matches!(err, AgentError::EmptyResult),
            "expected EmptyResult, got {err:?}"
        );
    }

    #[tokio::test]
    async fn timeout_kills_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        let bin = script(dir.path(), "cat > /dev/null\nsleep 30\n");
        let err = invoke(&cfg(bin, 200), "s", &json!({}), "p", &mut |_| {})
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::Timeout(_)));
    }

    #[tokio::test]
    async fn timeout_error_reports_actual_sub_second_budget() {
        let dir = tempfile::tempdir().unwrap();
        let bin = script(dir.path(), "cat > /dev/null\nsleep 30\n");
        let err = invoke(&cfg(bin, 200), "s", &json!({}), "p", &mut |_| {})
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "timed out after 200.0ms");
    }

    #[tokio::test]
    async fn read_error_before_output_surfaces_as_read_not_empty_result() {
        let dir = tempfile::tempdir().unwrap();
        // Invalid UTF-8 makes tokio's line reader error while the child exits 0.
        let bin = script(dir.path(), "cat > /dev/null\nprintf '\\xff\\xfe\\n'\n");
        let err = invoke(&cfg(bin, 30_000), "s", &json!({}), "p", &mut |_| {})
            .await
            .unwrap_err();
        assert!(
            matches!(err, AgentError::Read(_)),
            "expected Read, got {err:?}"
        );
    }

    #[tokio::test]
    async fn nonzero_exit_carries_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let bin = script(dir.path(), "cat > /dev/null\necho boom >&2\nexit 3\n");
        let err = invoke(&cfg(bin, 30_000), "s", &json!({}), "p", &mut |_| {})
            .await
            .unwrap_err();
        match err {
            AgentError::NonZeroExit { code, stderr } => {
                assert_eq!(code, 3);
                assert!(stderr.contains("boom"));
            }
            other => panic!("expected NonZeroExit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_binary_is_spawn_error() {
        let err = invoke(
            &cfg("/definitely/not/here".into(), 500),
            "s",
            &json!({}),
            "p",
            &mut |_| {},
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AgentError::Spawn(_)));
    }

    #[tokio::test]
    async fn large_stderr_is_drained_concurrently_and_does_not_hang() {
        let dir = tempfile::tempdir().unwrap();
        // ~200KB to stderr — well past a pipe buffer — before any stdout:
        // without concurrent draining the child blocks on the write and the
        // invocation times out instead of completing.
        let bin = script(
            dir.path(),
            r#"
cat > /dev/null
head -c 200000 /dev/zero >&2
echo '{"type":"result","subtype":"success","result":"ok"}'
"#,
        );
        let start = std::time::Instant::now();
        // cfg's 30s timeout is the failure-mode ceiling (a deadlocked child
        // only returns when it fires, panicking the unwrap); the 15s success
        // bound leaves parallel-suite headroom while staying clearly below it.
        let out = invoke(&cfg(bin, 30_000), "s", &json!({}), "p", &mut |_| {})
            .await
            .unwrap();
        assert_eq!(out.result_text, "ok");
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "invoke should not block on large stderr output, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn unread_stdin_does_not_block_completion_after_stdout_closes() {
        let dir = tempfile::tempdir().unwrap();
        // A forked helper inherits the stdin pipe and lingers, so a prompt
        // larger than the pipe buffer keeps the stdin write blocked until the
        // helper dies — invoke must not wait once stdout has closed. `<&0`
        // re-attaches the real stdin (backgrounded commands get /dev/null).
        let bin = script(
            dir.path(),
            r#"
sleep 30 <&0 >/dev/null 2>&1 &
echo '{"type":"result","subtype":"success","result":"ok"}'
"#,
        );
        let prompt = "x".repeat(300_000);
        let start = std::time::Instant::now();
        // Failure mode: invoke waits on the blocked stdin writer until its own
        // 20s timeout fires; the 15s bound stays clearly below that ceiling.
        let out = invoke(&cfg(bin, 20_000), "s", &json!({}), &prompt, &mut |_| {})
            .await
            .unwrap();
        assert_eq!(out.result_text, "ok");
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "invoke should not wait on the blocked stdin writer, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn hardened_argv_prefix_is_exact_and_tools_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let args_file = dir.path().join("args.txt");
        let bin = script(
            dir.path(),
            &format!(
                r#"
cat > /dev/null
for arg in "$@"; do printf '%s\n' "$arg" >> "{}"; done
echo '{{"type":"result","subtype":"success","result":"ok"}}'
"#,
                args_file.display()
            ),
        );
        invoke(
            &cfg(bin, 30_000),
            "sys-prompt",
            &json!({"type": "object"}),
            "p",
            &mut |_| {},
        )
        .await
        .unwrap();

        let contents = std::fs::read_to_string(&args_file).unwrap();
        let args: Vec<&str> = contents.lines().collect();
        assert_eq!(
            args,
            vec![
                "-p",
                "--verbose",
                "--output-format",
                "stream-json",
                "--include-partial-messages",
                "--safe-mode",
                "--no-session-persistence",
                "--tools",
                "",
                "--system-prompt",
                "sys-prompt",
                "--json-schema",
                "{\"type\":\"object\"}",
                "--model",
                "test-model",
            ]
        );
        // Explicitly pin that --tools is followed by an empty argument, not
        // omitted or merged with the next flag.
        let tools_idx = args.iter().position(|a| *a == "--tools").unwrap();
        assert_eq!(args[tools_idx + 1], "");
    }
}
