use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(unix)]
use std::os::fd::AsRawFd;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspPosition {
    pub path: String,
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefinitionTarget {
    pub path: String,
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoverResult {
    pub text: String,
}

pub struct LspClient {
    root: PathBuf,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    progress: Arc<Mutex<LspProgress>>,
}

#[derive(Clone)]
pub struct LspSession {
    commands: mpsc::Sender<LspCommand>,
}

enum LspCommand {
    Hover {
        position: LspPosition,
        canceled: Arc<AtomicBool>,
        reply: mpsc::Sender<Result<Option<HoverResult>>>,
    },
    Definition {
        position: LspPosition,
        reply: mpsc::Sender<Result<Vec<DefinitionTarget>>>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LspProgress {
    pub title: Option<String>,
    pub message: Option<String>,
    pub percentage: Option<u32>,
    pub done: bool,
}

pub fn trace(message: impl std::fmt::Display) {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    if *ENABLED.get_or_init(|| std::env::var_os("LGTM_LSP_TRACE").is_some()) {
        eprintln!("lgtm:lsp {message}");
    }
}

impl LspClient {
    pub fn start(
        root: PathBuf,
        progress: Arc<Mutex<LspProgress>>,
        warmup: Option<LspPosition>,
    ) -> Result<Self> {
        let (bin, args) = bifrost_server_command(&root)?;
        let mut child = Command::new(&bin)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to start Bifrost LSP: {}", bin.display()))?;
        let stdin = child
            .stdin
            .take()
            .context("Bifrost LSP stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("Bifrost LSP stdout unavailable")?;
        let mut this = Self {
            root,
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            progress,
        };
        this.initialize()?;
        this.warm_up(warmup.as_ref())?;
        Ok(this)
    }

    pub fn hover(&mut self, pos: &LspPosition) -> Result<Option<HoverResult>> {
        self.useful_hover_with_timeout(pos, Duration::from_secs(5))
    }

    fn useful_hover_with_timeout(
        &mut self,
        pos: &LspPosition,
        timeout: Duration,
    ) -> Result<Option<HoverResult>> {
        let hover = self.hover_with_timeout(pos, timeout)?;
        let Some(hover) = hover else {
            return Ok(None);
        };
        let definitions = self.definition_with_timeout(pos, timeout)?;
        if definitions
            .iter()
            .any(|target| target_contains_position(target, pos))
        {
            trace(format_args!(
                "hover suppressed declaration {}:{}:{}",
                pos.path,
                pos.line + 1,
                pos.character + 1
            ));
            return Ok(None);
        }
        Ok(Some(hover))
    }

    fn hover_with_timeout(
        &mut self,
        pos: &LspPosition,
        timeout: Duration,
    ) -> Result<Option<HoverResult>> {
        let params = json!({
            "textDocument": { "uri": self.uri_for(&pos.path)? },
            "position": { "line": pos.line, "character": pos.character },
        });
        let Some(result) = self.request("textDocument/hover", params, timeout)? else {
            return Ok(None);
        };
        let text = match result.get("contents") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Object(obj)) => obj
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|part| match part {
                    Value::String(s) => Some(s.clone()),
                    Value::Object(obj) => {
                        obj.get("value").and_then(Value::as_str).map(str::to_string)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
            _ => String::new(),
        };
        let text = strip_markdown_fences(&text);
        Ok((!text.trim().is_empty()).then_some(HoverResult { text }))
    }

    pub fn definition(&mut self, pos: &LspPosition) -> Result<Vec<DefinitionTarget>> {
        self.definition_with_timeout(pos, Duration::from_secs(5))
    }

    fn definition_with_timeout(
        &mut self,
        pos: &LspPosition,
        timeout: Duration,
    ) -> Result<Vec<DefinitionTarget>> {
        let params = json!({
            "textDocument": { "uri": self.uri_for(&pos.path)? },
            "position": { "line": pos.line, "character": pos.character },
        });
        let Some(result) = self.request("textDocument/definition", params, timeout)? else {
            return Ok(Vec::new());
        };
        let locations: Vec<Value> = match result {
            Value::Array(items) => items,
            Value::Object(obj) if obj.contains_key("targetUri") => vec![Value::Object(obj)],
            Value::Object(obj) if obj.contains_key("uri") => vec![Value::Object(obj)],
            _ => Vec::new(),
        };
        Ok(locations
            .into_iter()
            .filter_map(|loc| self.definition_from_location(loc))
            .collect())
    }

    fn warm_up(&mut self, pos: Option<&LspPosition>) -> Result<()> {
        {
            let mut progress = self.progress.lock().unwrap();
            progress.message = Some("Warming LSP queries".to_string());
            progress.percentage = Some(99);
            progress.done = false;
        }
        if let Some(pos) = pos {
            let _ = self.definition_with_timeout(pos, Duration::from_secs(30))?;
        } else {
            self.request(
                "workspace/symbol",
                json!({ "query": "" }),
                Duration::from_secs(30),
            )?;
        }
        {
            let mut progress = self.progress.lock().unwrap();
            progress.message = Some("Ready".to_string());
            progress.percentage = Some(100);
            progress.done = true;
        }
        Ok(())
    }

    fn initialize(&mut self) -> Result<()> {
        let root_uri = file_uri(&self.root)?;
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "window": {
                    "workDoneProgress": true
                }
            },
            "workspaceFolders": [{ "uri": root_uri, "name": "lgtm" }]
        });
        self.request("initialize", params, Duration::from_secs(30))?;
        self.notify("initialized", json!({}))?;
        self.read_startup_progress()?;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Option<Value>> {
        let id = self.next_id;
        self.next_id += 1;
        let started = std::time::Instant::now();
        trace(format_args!("wire send id={id} method={method}"));
        self.write(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        loop {
            let msg = self
                .read_message_timeout(timeout)
                .with_context(|| format!("LSP {method} timed out after {timeout:?}"))?;
            if self.handle_server_message(&msg)? {
                continue;
            }
            if msg.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(err) = msg.get("error") {
                trace(format_args!(
                    "wire error id={id} method={method} elapsed={:?} error={err}",
                    started.elapsed()
                ));
                bail!("LSP {method} failed: {err}");
            }
            let result = msg.get("result").cloned();
            trace(format_args!(
                "wire response id={id} method={method} elapsed={:?} result={}",
                started.elapsed(),
                if result.as_ref().is_none_or(Value::is_null) {
                    "null"
                } else {
                    "value"
                }
            ));
            return Ok(result);
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
    }

    fn read_startup_progress(&mut self) -> Result<()> {
        loop {
            if self.progress.lock().unwrap().done {
                return Ok(());
            }
            let msg = match self.read_message_timeout(Duration::from_secs(30)) {
                Ok(msg) => msg,
                Err(err) => {
                    let mut progress = self.progress.lock().unwrap();
                    progress.message = Some(format!("Indexing status unavailable: {err:#}"));
                    progress.done = true;
                    return Ok(());
                }
            };
            self.handle_server_message(&msg)?;
        }
    }

    fn handle_server_message(&mut self, msg: &Value) -> Result<bool> {
        match msg.get("method").and_then(Value::as_str) {
            Some("window/workDoneProgress/create") => {
                let id = msg
                    .get("id")
                    .cloned()
                    .context("progress create missing id")?;
                self.write(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": null,
                }))?;
                Ok(true)
            }
            Some("$/progress") => {
                self.apply_progress(msg);
                Ok(true)
            }
            Some(_) => {
                if let Some(id) = msg.get("id").cloned() {
                    self.write(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": "Method not found"
                        }
                    }))?;
                }
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn apply_progress(&self, msg: &Value) {
        let Some(value) = msg.pointer("/params/value") else {
            return;
        };
        let mut progress = self.progress.lock().unwrap();
        match value.get("kind").and_then(Value::as_str) {
            Some("begin") => {
                progress.title = value
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                progress.message = value
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                progress.percentage = value
                    .get("percentage")
                    .and_then(Value::as_u64)
                    .map(|n| n as u32);
                progress.done = false;
            }
            Some("report") => {
                progress.message = value
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| progress.message.clone());
                progress.percentage = value
                    .get("percentage")
                    .and_then(Value::as_u64)
                    .map(|n| n as u32)
                    .or(progress.percentage);
            }
            Some("end") => {
                progress.message = value
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| Some("Indexing complete".to_string()));
                progress.percentage = Some(100);
                progress.done = true;
            }
            _ => {}
        }
    }

    fn write(&mut self, value: Value) -> Result<()> {
        let body = serde_json::to_vec(&value)?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())?;
        self.stdin.write_all(&body)?;
        self.stdin.flush()?;
        Ok(())
    }

    fn read_message(&mut self) -> Result<Value> {
        let mut len = None;
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line)?;
            if n == 0 {
                bail!("Bifrost LSP exited");
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
                len = Some(rest.trim().parse::<usize>()?);
            }
        }
        let len = len.context("LSP message missing Content-Length")?;
        let mut body = vec![0; len];
        std::io::Read::read_exact(&mut self.stdout, &mut body)?;
        Ok(serde_json::from_slice(&body)?)
    }

    fn read_message_timeout(&mut self, timeout: Duration) -> Result<Value> {
        self.wait_for_stdout(timeout)?;
        self.read_message()
    }

    #[cfg(unix)]
    fn wait_for_stdout(&self, timeout: Duration) -> Result<()> {
        if !self.stdout.buffer().is_empty() {
            return Ok(());
        }
        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let mut fd = libc::pollfd {
            fd: self.stdout.get_ref().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let n = unsafe { libc::poll(&mut fd, 1, timeout_ms) };
        if n < 0 {
            return Err(std::io::Error::last_os_error()).context("polling LSP stdout");
        }
        if n == 0 {
            bail!("no message received");
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn wait_for_stdout(&self, _timeout: Duration) -> Result<()> {
        Ok(())
    }

    fn uri_for(&self, rel: &str) -> Result<String> {
        file_uri(&self.root.join(rel))
    }

    fn definition_from_location(&self, loc: Value) -> Option<DefinitionTarget> {
        let uri = loc.get("uri").or_else(|| loc.get("targetUri"))?.as_str()?;
        let range = loc.get("range").or_else(|| loc.get("targetRange"))?;
        let start = range.get("start")?;
        let end = range.get("end")?;
        Some(DefinitionTarget {
            path: rel_path_from_uri(&self.root, uri)?,
            start_line: start.get("line")?.as_u64()? as u32,
            start_character: start.get("character")?.as_u64()? as u32,
            end_line: end.get("line")?.as_u64()? as u32,
            end_character: end.get("character")?.as_u64()? as u32,
        })
    }
}

impl LspSession {
    pub fn start(
        root: PathBuf,
        progress: Arc<Mutex<LspProgress>>,
        warmup: Option<LspPosition>,
    ) -> Result<Self> {
        let mut client = LspClient::start(root, progress, warmup)?;
        let (commands, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("bifrost-lsp".to_string())
            .spawn(move || {
                while let Ok(command) = receiver.recv() {
                    match command {
                        LspCommand::Hover {
                            position,
                            canceled,
                            reply,
                        } => {
                            let was_canceled = canceled.load(Ordering::Acquire);
                            trace(format_args!(
                                "worker hover {}:{}:{} canceled={was_canceled}",
                                position.path,
                                position.line + 1,
                                position.character + 1
                            ));
                            let result = if was_canceled {
                                Ok(None)
                            } else {
                                client.hover(&position)
                            };
                            trace(format_args!(
                                "worker hover done outcome={}{}",
                                match &result {
                                    Ok(Some(_)) => "content",
                                    Ok(None) => "none",
                                    Err(_) => "error",
                                },
                                match &result {
                                    Err(err) => format!(": {err:#}"),
                                    _ => String::new(),
                                }
                            ));
                            let _ = reply.send(result);
                        }
                        LspCommand::Definition { position, reply } => {
                            trace(format_args!(
                                "worker definition {}:{}:{}",
                                position.path,
                                position.line + 1,
                                position.character + 1
                            ));
                            let _ = reply.send(client.definition(&position));
                        }
                    }
                }
            })
            .context("starting Bifrost LSP worker")?;
        Ok(Self { commands })
    }

    pub fn hover(
        &self,
        position: LspPosition,
        canceled: Arc<AtomicBool>,
    ) -> Result<Option<HoverResult>> {
        let (reply, response) = mpsc::channel();
        trace(format_args!(
            "queue hover {}:{}:{}",
            position.path,
            position.line + 1,
            position.character + 1
        ));
        self.commands
            .send(LspCommand::Hover {
                position,
                canceled,
                reply,
            })
            .context("Bifrost LSP worker stopped")?;
        response.recv().context("Bifrost LSP worker stopped")?
    }

    pub fn definition(&self, position: LspPosition) -> Result<Vec<DefinitionTarget>> {
        let (reply, response) = mpsc::channel();
        trace(format_args!(
            "queue definition {}:{}:{}",
            position.path,
            position.line + 1,
            position.character + 1
        ));
        self.commands
            .send(LspCommand::Definition { position, reply })
            .context("Bifrost LSP worker stopped")?;
        response.recv().context("Bifrost LSP worker stopped")?
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.notify("exit", json!({}));
        let _ = self.child.kill();
    }
}

fn bifrost_server_command(root: &Path) -> Result<(PathBuf, Vec<std::ffi::OsString>)> {
    if let Some(bin) = std::env::var_os("LGTM_BIFROST") {
        return Ok((
            PathBuf::from(bin),
            vec!["--root".into(), root.as_os_str().to_owned(), "--lsp".into()],
        ));
    }
    Ok((
        std::env::current_exe().context("locating LGTM executable for embedded Bifrost LSP")?,
        vec!["--bifrost-lsp-server".into(), root.as_os_str().to_owned()],
    ))
}

fn file_uri(path: &Path) -> Result<String> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut out = String::from("file://");
    let s = path
        .to_str()
        .ok_or_else(|| anyhow!("path is not valid UTF-8: {}", path.display()))?;
    if !s.starts_with('/') {
        out.push('/');
    }
    out.push_str(&percent_encode(s));
    Ok(out)
}

fn rel_path_from_uri(root: &Path, uri: &str) -> Option<String> {
    let raw = uri.strip_prefix("file://")?;
    let path = PathBuf::from(percent_decode(raw)?);
    let rel = path.strip_prefix(root).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

fn percent_encode(input: &str) -> String {
    let mut out = String::new();
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = hex(bytes.get(i + 1).copied()?)?;
            let lo = hex(bytes.get(i + 2).copied()?)?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn strip_markdown_fences(text: &str) -> String {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim_start().starts_with("```") || line.trim() == "---" {
            continue;
        }
        out.push(line);
    }
    out.join("\n").trim().to_string()
}

fn target_contains_position(target: &DefinitionTarget, pos: &LspPosition) -> bool {
    if target.path != pos.path || pos.line < target.start_line || pos.line > target.end_line {
        return false;
    }
    let after_start = pos.line > target.start_line || pos.character >= target.start_character;
    let before_end = pos.line < target.end_line || pos.character < target.end_character;
    after_start && before_end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position_of(root: &Path, rel: &str, needle: &str) -> LspPosition {
        let text = std::fs::read_to_string(root.join(rel)).expect("read smoke-test source file");
        for (line, row) in text.lines().enumerate() {
            if let Some(col) = row.find(needle) {
                return LspPosition {
                    path: rel.to_string(),
                    line: line as u32,
                    character: col as u32,
                };
            }
        }
        panic!("could not find {needle:?} in {rel}");
    }

    fn position_in_line(
        root: &Path,
        rel: &str,
        line_needle: &str,
        token: &str,
        token_offset: usize,
    ) -> LspPosition {
        let text = std::fs::read_to_string(root.join(rel)).expect("read smoke-test source file");
        for (line, row) in text.lines().enumerate() {
            if row.contains(line_needle) {
                let start = row.find(token).expect("find smoke-test token");
                return LspPosition {
                    path: rel.to_string(),
                    line: line as u32,
                    character: (start + token_offset) as u32,
                };
            }
        }
        panic!("could not find line {line_needle:?} in {rel}");
    }

    #[test]
    #[ignore = "requires LGTM_LSP_SMOKE_ROOT pointing at a checkout and a Bifrost binary"]
    fn bifrost_lsp_smoke_hover_and_definition() {
        let root = PathBuf::from(
            std::env::var_os("LGTM_LSP_SMOKE_ROOT")
                .expect("LGTM_LSP_SMOKE_ROOT must point at a checkout"),
        );
        let progress = Arc::new(Mutex::new(LspProgress::default()));
        let rel = "crates/app/src/main.rs";
        let definition_pos = position_of(&root, rel, "mono_family(&cfg)");
        let mut client = LspClient::start(root.clone(), progress, Some(definition_pos.clone()))
            .expect("start Bifrost LSP");

        let targets = client
            .definition(&definition_pos)
            .expect("definition request should finish");
        assert!(
            targets
                .iter()
                .any(|target| target.path == "crates/app/src/main.rs"),
            "definition targets should include main.rs, got {targets:?}"
        );

        let call_hover = client
            .hover(&definition_pos)
            .expect("function call hover should finish")
            .expect("function call should have useful hover content");
        assert!(
            call_hover.text.contains("fn mono_family"),
            "function call hover should contain its signature, got {:?}",
            call_hover.text
        );

        let probes = [
            (
                "mono_family declaration",
                position_in_line(&root, rel, "fn mono_family(cfg:", "mono_family", 0),
                true,
                None,
            ),
            (
                "cfg parameter",
                position_in_line(&root, rel, "fn mono_family(cfg:", "cfg", 0),
                false,
                None,
            ),
            (
                "cfg reference",
                position_in_line(&root, rel, "if cfg.font.mono_family", "cfg.font", 0),
                false,
                Some("cfg: &config::Config"),
            ),
            (
                "font field",
                position_in_line(&root, rel, "if cfg.font.mono_family", "cfg.font", 4),
                false,
                Some("pub font: FontConfig"),
            ),
            (
                "mono_family field",
                position_in_line(
                    &root,
                    rel,
                    "if cfg.font.mono_family",
                    "cfg.font.mono_family",
                    9,
                ),
                false,
                Some("pub mono_family: String"),
            ),
        ];
        for (label, position, must_be_suppressed, expected_text) in probes {
            let started = std::time::Instant::now();
            let hover = client
                .useful_hover_with_timeout(&position, Duration::from_secs(30))
                .unwrap_or_else(|err| panic!("{label} hover should finish: {err:#}"));
            eprintln!(
                "{label}: hover={:?}, elapsed={:?}",
                hover.as_ref().map(|hover| &hover.text),
                started.elapsed()
            );
            if must_be_suppressed {
                assert!(
                    hover.is_none(),
                    "{label} should not show tautological hover"
                );
            }
            if let Some(expected_text) = expected_text {
                let hover = hover.unwrap_or_else(|| panic!("{label} should have useful hover"));
                assert!(
                    hover.text.contains(expected_text),
                    "{label} hover should contain {expected_text:?}, got {:?}",
                    hover.text
                );
            }
        }
    }

    #[test]
    #[ignore = "requires LGTM_LSP_SMOKE_ROOT pointing at a checkout and a Bifrost binary"]
    fn bifrost_lsp_session_smoke() {
        let root = PathBuf::from(
            std::env::var_os("LGTM_LSP_SMOKE_ROOT")
                .expect("LGTM_LSP_SMOKE_ROOT must point at a checkout"),
        );
        let rel = "crates/app/src/main.rs";
        let call = position_of(&root, rel, "mono_family(&cfg)");
        let declaration = position_in_line(&root, rel, "fn mono_family(cfg:", "mono_family", 0);
        let cfg_reference = position_in_line(&root, rel, "if cfg.font.mono_family", "cfg.font", 0);
        let progress = Arc::new(Mutex::new(LspProgress::default()));
        let session = LspSession::start(root, progress, Some(call.clone()))
            .expect("start Bifrost LSP session");

        let definitions = session
            .definition(call.clone())
            .expect("queued definition should finish");
        assert!(
            definitions.iter().any(|target| target.path == rel),
            "queued definition should resolve inside main.rs, got {definitions:?}"
        );

        let hover = session
            .hover(call, Arc::new(AtomicBool::new(false)))
            .expect("queued call hover should finish")
            .expect("queued call hover should have useful content");
        assert!(hover.text.contains("fn mono_family"));

        let cfg_hover = session
            .hover(cfg_reference, Arc::new(AtomicBool::new(false)))
            .expect("queued cfg hover should finish")
            .expect("queued cfg hover should have useful content");
        assert!(cfg_hover.text.contains("cfg: &config::Config"));

        let declaration_hover = session
            .hover(declaration, Arc::new(AtomicBool::new(false)))
            .expect("queued declaration hover should finish");
        assert!(
            declaration_hover.is_none(),
            "declaration hover should be suppressed"
        );
    }
}
