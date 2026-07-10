//! Headless Q&A via the `claude` CLI (Claude Code), same subprocess
//! philosophy as the gh/git crates: spawn `claude -p … --output-format
//! stream-json`, parse the event stream line by line, no SDK, no auth code.
//!
//! Tool policy is locked down: never Bash/Edit/Write/WebFetch/WebSearch/Task.
//! With an exploration directory the model may Read/Glob/Grep inside it;
//! without one it gets no tools at all (`--tools ""`). `--permission-mode
//! dontAsk` denies anything that would otherwise prompt, so a headless run
//! can never hang on a permission question.

use anyhow::{anyhow, Result};
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Default)]
pub struct ChatOptions {
    /// Resume this session (`--resume`); None starts a fresh one.
    pub session: Option<String>,
    /// Appended to the default system prompt (`--append-system-prompt`).
    pub system_prompt: Option<String>,
    /// Directory the model may explore read-only (Read/Glob/Grep). None
    /// disables all tools.
    pub explore_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatEvent {
    /// One streamed chunk of assistant text.
    TextDelta(String),
    /// The final `result` event of a run.
    Completed {
        session_id: String,
        cost_usd: f64,
        is_error: bool,
        /// The complete response text (authoritative; deltas may be a
        /// strict prefix if the stream was interrupted).
        text: String,
    },
    /// The subprocess failed: nonzero exit, or the stream ended without a
    /// result event. Carries the stderr tail (or a description).
    Failed(String),
}

/// The exact argv (after the binary name) for one chat invocation. Split out
/// of [`chat`] so tests can assert flag assembly without running anything.
pub fn build_args(prompt: &str, opts: &ChatOptions) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        prompt,
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-mode",
        "dontAsk",
        // Never these, in either mode (variadic: space-separated names).
        "--disallowed-tools",
        "Bash",
        "Edit",
        "Write",
        "WebFetch",
        "WebSearch",
        "Task",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    if let Some(session) = &opts.session {
        args.push("--resume".into());
        args.push(session.clone());
    }
    if let Some(system_prompt) = &opts.system_prompt {
        args.push("--append-system-prompt".into());
        args.push(system_prompt.clone());
    }
    match &opts.explore_dir {
        Some(dir) => {
            args.push("--add-dir".into());
            args.push(dir.display().to_string());
            args.push("--allowed-tools".into());
            args.push("Read".into());
            args.push("Glob".into());
            args.push("Grep".into());
        }
        // `--tools ""` is the CLI's documented "disable all tools" form.
        None => {
            args.push("--tools".into());
            args.push(String::new());
        }
    }
    args
}

/// Parse one stream-json line into an event. Returns None for everything we
/// skip: system/assistant/user/rate_limit_event lines, non-text deltas
/// (thinking, tool input), and unparseable lines.
pub fn parse_line(line: &str) -> Option<ChatEvent> {
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    match value.get("type")?.as_str()? {
        "stream_event" => {
            let event = value.get("event")?;
            if event.get("type")?.as_str()? != "content_block_delta" {
                return None;
            }
            let delta = event.get("delta")?;
            if delta.get("type")?.as_str()? != "text_delta" {
                return None;
            }
            Some(ChatEvent::TextDelta(delta.get("text")?.as_str()?.to_string()))
        }
        "result" => Some(ChatEvent::Completed {
            session_id: value
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            cost_usd: value
                .get("total_cost_usd")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0),
            is_error: value
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            text: value
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
        }),
        _ => None,
    }
}

/// Last `max` bytes of `text` (on a char boundary), for error surfacing.
fn tail(text: &str, max: usize) -> &str {
    let mut start = text.len().saturating_sub(max);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Run one prompt against the `claude` CLI, streaming events to `on_event`.
///
/// Blocking: call from a background thread/executor. Every run ends with a
/// terminal event — `Completed` (normal), or `Failed` (nonzero exit or a
/// stream that ended without a result) — except when `cancel` is set, which
/// kills the child and returns without a terminal event. `Err` is reserved
/// for not being able to spawn `claude` at all.
pub fn chat(
    prompt: &str,
    opts: &ChatOptions,
    cancel: &AtomicBool,
    mut on_event: impl FnMut(ChatEvent),
) -> Result<()> {
    let mut child = Command::new("claude")
        .args(build_args(prompt, opts))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| anyhow!("failed to run claude (is Claude Code installed?): {err}"))?;

    // Drain stderr on its own thread so a chatty child can't deadlock the
    // stdout reads.
    let mut stderr = child.stderr.take().expect("stderr was piped");
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        buf
    });

    let stdout = child.stdout.take().expect("stdout was piped");
    let mut completed = false;
    for line in BufReader::new(stdout).lines() {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stderr_thread.join();
            return Ok(());
        }
        let Ok(line) = line else { break };
        if let Some(event) = parse_line(&line) {
            completed |= matches!(event, ChatEvent::Completed { .. });
            on_event(event);
        }
    }

    let status = child.wait();
    let stderr_text = stderr_thread.join().unwrap_or_default();
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    let ok = status.as_ref().is_ok_and(|status| status.success());
    if !completed {
        let detail = tail(stderr_text.trim(), 2000);
        on_event(ChatEvent::Failed(if detail.is_empty() {
            if ok {
                "claude stream ended without a result event".to_string()
            } else {
                format!("claude exited with {:?}", status.map(|s| s.code()))
            }
        } else {
            detail.to_string()
        }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn parses_text_deltas() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}},"session_id":"s1"}"#;
        assert_eq!(parse_line(line), Some(ChatEvent::TextDelta("Hello".into())));
    }

    #[test]
    fn skips_non_text_deltas_and_other_stream_events() {
        let thinking = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}}"#;
        assert_eq!(parse_line(thinking), None);
        let start = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#;
        assert_eq!(parse_line(start), None);
        let input_json = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{"}}}"#;
        assert_eq!(parse_line(input_json), None);
    }

    #[test]
    fn parses_result_events() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1200,"result":"final text","session_id":"abc-123","total_cost_usd":0.0123}"#;
        assert_eq!(
            parse_line(line),
            Some(ChatEvent::Completed {
                session_id: "abc-123".into(),
                cost_usd: 0.0123,
                is_error: false,
                text: "final text".into(),
            })
        );
        // Error result: is_error true still parses as Completed.
        let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"boom","session_id":"x","total_cost_usd":0}"#;
        assert!(matches!(
            parse_line(line),
            Some(ChatEvent::Completed { is_error: true, .. })
        ));
    }

    #[test]
    fn skips_other_message_types_and_garbage() {
        for line in [
            r#"{"type":"system","subtype":"init","session_id":"s","tools":[]}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#,
            r#"{"type":"user","message":{"content":[]}}"#,
            r#"{"type":"rate_limit_event","rate_limit":{}}"#,
            "not json at all",
            "",
            r#"{"no_type_field":1}"#,
        ] {
            assert_eq!(parse_line(line), None, "line: {line}");
        }
    }

    #[test]
    fn args_plain_qa_has_no_tools_and_no_resume() {
        let args = build_args("what changed?", &ChatOptions::default());
        assert_eq!(args[0..2], ["-p".to_string(), "what changed?".to_string()]);
        let joined = args.join(" ");
        assert!(joined.contains("--output-format stream-json"));
        assert!(joined.contains("--verbose"));
        assert!(joined.contains("--include-partial-messages"));
        assert!(joined.contains("--permission-mode dontAsk"));
        assert!(joined.contains("--disallowed-tools Bash Edit Write WebFetch WebSearch Task"));
        // No tools at all: --tools with an empty value, and no explore flags.
        let tools_ix = args.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(args[tools_ix + 1], "");
        assert!(!args.contains(&"--resume".to_string()));
        assert!(!args.contains(&"--append-system-prompt".to_string()));
        assert!(!args.contains(&"--add-dir".to_string()));
        assert!(!args.contains(&"--allowed-tools".to_string()));
    }

    #[test]
    fn args_explore_mode_allows_read_only_tools() {
        let opts = ChatOptions {
            session: Some("sess-1".into()),
            system_prompt: Some("be brief".into()),
            explore_dir: Some(Path::new("/tmp/scratch").to_path_buf()),
        };
        let args = build_args("q", &opts);
        let joined = args.join(" ");
        assert!(joined.contains("--resume sess-1"));
        assert!(joined.contains("--append-system-prompt be brief"));
        assert!(joined.contains("--add-dir /tmp/scratch"));
        assert!(joined.contains("--allowed-tools Read Glob Grep"));
        assert!(joined.contains("--disallowed-tools Bash Edit Write WebFetch WebSearch Task"));
        // Explore mode must not disable the toolset wholesale.
        assert!(!args.contains(&"--tools".to_string()));
    }

    #[test]
    fn tail_respects_char_boundaries() {
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("abc", 10), "abc");
        // 'é' is two bytes; cutting inside it moves forward to a boundary.
        assert_eq!(tail("aéb", 2), "b");
    }
}
