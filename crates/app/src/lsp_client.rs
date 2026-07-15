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

/// Budget for the extra `textDocument/definition` lookup that decides whether a
/// hover sits on its own declaration. Kept short so it can't stall (or, on
/// timeout, discard) a hover that already resolved.
const SUPPRESSION_TIMEOUT: Duration = Duration::from_secs(2);

/// How long to wait for rust-analyzer to finish loading + indexing during
/// warmup before giving up and letting hovers resolve lazily. Bounded so a huge
/// project (or an unresolvable warmup symbol) can't hang session startup.
const RA_WARMUP_DEADLINE: Duration = Duration::from_secs(90);

/// Which language server backs hover / go-to-definition. Bifrost (embedded,
/// tree-sitter, build-free) is the default and works on any repo, including
/// unbuilt PR worktrees, but only resolves symbols lexically. rust-analyzer
/// adds real type inference — so method calls (`x.unwrap()`) and std/dep
/// symbols resolve — at the cost of needing a buildable Rust project.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LspBackend {
    #[default]
    Bifrost,
    RustAnalyzer,
}

impl LspBackend {
    /// Short name for the status chip.
    pub fn label(self) -> &'static str {
        match self {
            LspBackend::Bifrost => "Bifrost",
            LspBackend::RustAnalyzer => "rust-analyzer",
        }
    }

    /// The other backend, for the click-to-switch toggle.
    pub fn toggled(self) -> Self {
        match self {
            LspBackend::Bifrost => LspBackend::RustAnalyzer,
            LspBackend::RustAnalyzer => LspBackend::Bifrost,
        }
    }
}

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
    backend: LspBackend,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    progress: Arc<Mutex<LspProgress>>,
    /// Files already sent via `textDocument/didOpen` (rust-analyzer only).
    open_docs: std::collections::HashSet<String>,
    /// rust-analyzer has reported `quiescent: true` via `experimental/
    /// serverStatus` — i.e. it has finished loading + indexing and hovers now
    /// resolve. Stays false for Bifrost (which is ready immediately).
    ra_quiescent: bool,
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
        backend: LspBackend,
        progress: Arc<Mutex<LspProgress>>,
        warmup: Option<LspPosition>,
    ) -> Result<Self> {
        // Canonicalize up front so `self.root` matches the form the server
        // sees: request URIs go through `file_uri` (which canonicalizes) and
        // response URIs are mapped back by stripping `self.root`. A symlinked
        // component (common on macOS: /tmp, /var, cache dirs) would otherwise
        // fail to strip and silently drop every definition.
        let root = root.canonicalize().unwrap_or(root);
        let (bin, args) = server_command(backend, &root)?;
        let mut child = Command::new(&bin)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| {
                format!("failed to start {} LSP: {}", backend.label(), bin.display())
            })?;
        let stdin = child
            .stdin
            .take()
            .with_context(|| format!("{} LSP stdin unavailable", backend.label()))?;
        let stdout = child
            .stdout
            .take()
            .with_context(|| format!("{} LSP stdout unavailable", backend.label()))?;
        let mut this = Self {
            root,
            backend,
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            progress,
            open_docs: std::collections::HashSet::new(),
            ra_quiescent: false,
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
        // Suppress the tooltip only when hovering a symbol's own declaration.
        // This is a best-effort refinement on top of a hover that already
        // succeeded, so it must never discard it: bound the extra lookup to a
        // short budget and treat a slow/failed definition as "not a
        // declaration" rather than propagating the error (which would drop the
        // hover — worst under rust-analyzer, whose definition can be slow while
        // indexing).
        let definitions = self
            .definition_with_timeout(pos, timeout.min(SUPPRESSION_TIMEOUT))
            .unwrap_or_default();
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
        self.ensure_open(&pos.path)?;
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
        self.ensure_open(&pos.path)?;
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
        match (self.backend, pos) {
            // rust-analyzer answers nothing until it has loaded the workspace
            // and indexed; readiness is signalled by `experimental/serverStatus`
            // going `quiescent: true`. Pump incoming notifications until then
            // (or the deadline) so "ready" means hovers actually resolve — and
            // the status shows "Analyzing" meanwhile, not a stuck "100%".
            (LspBackend::RustAnalyzer, _) => {
                let deadline = std::time::Instant::now() + RA_WARMUP_DEADLINE;
                while !self.ra_quiescent && std::time::Instant::now() < deadline {
                    match self.read_message_timeout(Duration::from_millis(500)) {
                        Ok(msg) => {
                            self.handle_server_message(&msg)?;
                        }
                        // No message this tick (or a read hiccup): loop and
                        // re-check `ra_quiescent`/deadline.
                        Err(_) => {}
                    }
                }
            }
            (_, Some(pos)) => {
                let _ = self.definition_with_timeout(pos, Duration::from_secs(30))?;
            }
            (_, None) => {
                self.request(
                    "workspace/symbol",
                    json!({ "query": "" }),
                    Duration::from_secs(30),
                )?;
            }
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
                },
                // rust-analyzer extension: it pushes `experimental/serverStatus`
                // with `quiescent` so we know when it's actually ready, instead
                // of guessing from the indexing progress bar (which hits 100%
                // well before hovers resolve).
                "experimental": {
                    "serverStatusNotification": true
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
            Some("experimental/serverStatus") => {
                let quiescent = msg
                    .pointer("/params/quiescent")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.ra_quiescent = quiescent;
                let mut progress = self.progress.lock().unwrap();
                if quiescent {
                    progress.message = Some("Ready".to_string());
                    progress.percentage = Some(100);
                    progress.done = true;
                } else {
                    // Still loading/indexing: an honest label instead of a
                    // stale "100%" from a finished sub-phase.
                    progress.message = Some("Analyzing".to_string());
                    progress.percentage = None;
                }
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

    /// rust-analyzer answers hover/definition from its VFS, which is only
    /// populated for opened documents (or after a full workspace load) — a cold
    /// query on an unopened file errors with "file not found". So send
    /// `textDocument/didOpen` once per file with its on-disk content before the
    /// first request. Bifrost reads straight from disk, so this is skipped there.
    fn ensure_open(&mut self, rel: &str) -> Result<()> {
        if self.backend != LspBackend::RustAnalyzer || self.open_docs.contains(rel) {
            return Ok(());
        }
        let Ok(text) = std::fs::read_to_string(self.root.join(rel)) else {
            return Ok(()); // Missing/binary file: let the request fall through.
        };
        let uri = self.uri_for(rel)?;
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id_for(rel),
                    "version": 1,
                    "text": text,
                }
            }),
        )?;
        self.open_docs.insert(rel.to_string());
        Ok(())
    }

    fn definition_from_location(&self, loc: Value) -> Option<DefinitionTarget> {
        definition_target(&self.root, &loc)
    }
}

/// LSP `languageId` for a path, by extension. rust-analyzer only cares that
/// `.rs` maps to `rust`; the rest keep `didOpen` well-formed for other servers.
fn language_id_for(rel: &str) -> &'static str {
    match Path::new(rel).extension().and_then(|ext| ext.to_str()) {
        Some("rs") => "rust",
        Some("ts") | Some("tsx") => "typescript",
        Some("js") | Some("jsx") => "javascript",
        Some("py") => "python",
        Some("go") => "go",
        Some("c") | Some("h") => "c",
        Some("cc") | Some("cpp") | Some("cxx") | Some("hpp") => "cpp",
        _ => "plaintext",
    }
}

/// Parse one `Location`/`LocationLink` into a [`DefinitionTarget`]. The path is
/// repo-relative when the target lives under `root`, otherwise the absolute
/// on-disk path — so go-to-definition can open dependency and stdlib sources
/// (which rust-analyzer points at, outside the workspace).
fn definition_target(root: &Path, loc: &Value) -> Option<DefinitionTarget> {
    let uri = loc.get("uri").or_else(|| loc.get("targetUri"))?.as_str()?;
    // Prefer the name range (`targetSelectionRange`) over the whole-item range
    // (`targetRange`, which for a function spans its entire body): it makes
    // go-to-definition land on the name, and — used for the declaration-
    // suppression check — keeps calls made from inside a function's own body
    // (e.g. recursion) from being treated as its declaration. Plain `Location`
    // responses carry only `range`, which rust-analyzer already sets to the name.
    let range = loc
        .get("targetSelectionRange")
        .or_else(|| loc.get("range"))
        .or_else(|| loc.get("targetRange"))?;
    let start = range.get("start")?;
    let end = range.get("end")?;
    Some(DefinitionTarget {
        path: path_from_uri(root, uri)?,
        start_line: start.get("line")?.as_u64()? as u32,
        start_character: start.get("character")?.as_u64()? as u32,
        end_line: end.get("line")?.as_u64()? as u32,
        end_character: end.get("character")?.as_u64()? as u32,
    })
}

impl LspSession {
    pub fn start(
        root: PathBuf,
        backend: LspBackend,
        progress: Arc<Mutex<LspProgress>>,
        warmup: Option<LspPosition>,
    ) -> Result<Self> {
        let mut client = LspClient::start(root, backend, progress, warmup)?;
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

fn server_command(
    backend: LspBackend,
    root: &Path,
) -> Result<(PathBuf, Vec<std::ffi::OsString>)> {
    match backend {
        LspBackend::Bifrost => {
            if let Some(bin) = std::env::var_os("LGTM_BIFROST") {
                return Ok((
                    PathBuf::from(bin),
                    vec!["--root".into(), root.as_os_str().to_owned(), "--lsp".into()],
                ));
            }
            Ok((
                std::env::current_exe()
                    .context("locating LGTM executable for embedded Bifrost LSP")?,
                vec!["--bifrost-lsp-server".into(), root.as_os_str().to_owned()],
            ))
        }
        // rust-analyzer speaks LSP over stdio with no args; it takes the
        // workspace from `rootUri` in the initialize handshake. Overridable via
        // LGTM_RUST_ANALYZER, else found on PATH.
        LspBackend::RustAnalyzer => {
            let bin = std::env::var_os("LGTM_RUST_ANALYZER")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("rust-analyzer"));
            Ok((bin, Vec::new()))
        }
    }
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

/// Repo-relative path when the URI is under `root`, else its absolute on-disk
/// path (dependency / stdlib sources that rust-analyzer resolves to live
/// outside the workspace, but are still present on disk and worth opening).
fn path_from_uri(root: &Path, uri: &str) -> Option<String> {
    if let Some(rel) = rel_path_from_uri(root, uri) {
        return Some(rel);
    }
    let raw = uri.strip_prefix("file://")?;
    percent_decode(raw)
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
    fn definition_target_prefers_name_range_over_item_range() {
        let root = Path::new("/repo");
        // LocationLink: the name range (targetSelectionRange) must win over the
        // whole-item targetRange, so suppression and navigation key off the
        // declaration name, not the entire function body.
        let link = json!({
            "targetUri": "file:///repo/src/lib.rs",
            "targetRange": {
                "start": { "line": 10, "character": 0 },
                "end": { "line": 40, "character": 1 },
            },
            "targetSelectionRange": {
                "start": { "line": 10, "character": 3 },
                "end": { "line": 10, "character": 8 },
            },
        });
        let target = definition_target(root, &link).expect("link maps under root");
        assert_eq!(target.path, "src/lib.rs");
        assert_eq!((target.start_line, target.start_character), (10, 3));
        assert_eq!((target.end_line, target.end_character), (10, 8));
    }

    #[test]
    fn definition_target_maps_in_repo_relative_and_external_absolute() {
        let root = Path::new("/repo");
        let loc = json!({
            "uri": "file:///repo/src/main.rs",
            "range": {
                "start": { "line": 2, "character": 4 },
                "end": { "line": 2, "character": 7 },
            },
        });
        let target = definition_target(root, &loc).expect("location maps under root");
        assert_eq!(target.path, "src/main.rs");
        assert_eq!(target.start_line, 2);
        // A definition outside the workspace root (dependency / stdlib) keeps its
        // absolute on-disk path so go-to-definition can still open it.
        let external = json!({
            "uri": "file:///home/user/.cargo/registry/foo/lib.rs",
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 1 },
            },
        });
        let ext = definition_target(root, &external).expect("external target still resolves");
        assert_eq!(ext.path, "/home/user/.cargo/registry/foo/lib.rs");
    }

    #[test]
    #[cfg(unix)]
    fn canonical_root_strips_symlinked_paths() {
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().join(format!("lgtm-lsp-canon-{}", std::process::id()));
        let real = base.join("real");
        std::fs::create_dir_all(real.join("src")).unwrap();
        std::fs::write(real.join("src/lib.rs"), "").unwrap();
        let link = base.join("link");
        let _ = std::fs::remove_file(&link);
        symlink(&real, &link).unwrap();

        // The server reports canonical URIs (`file_uri` canonicalizes). Stripping
        // the raw, symlinked root fails; the canonical root (what
        // `LspClient::start` now stores) strips cleanly.
        let canonical_uri = file_uri(&link.join("src/lib.rs")).unwrap();
        assert!(rel_path_from_uri(&link, &canonical_uri).is_none());
        let canonical_root = link.canonicalize().unwrap();
        assert_eq!(
            rel_path_from_uri(&canonical_root, &canonical_uri).as_deref(),
            Some("src/lib.rs")
        );
        std::fs::remove_dir_all(&base).ok();
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
        let mut client = LspClient::start(
            root.clone(),
            LspBackend::Bifrost,
            progress,
            Some(definition_pos.clone()),
        )
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
        let session = LspSession::start(root, LspBackend::Bifrost, progress, Some(call.clone()))
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

    #[test]
    #[ignore = "requires rust-analyzer on PATH + rust-src; proves type-aware resolution"]
    fn rust_analyzer_backend_resolves_methods_and_external_defs() {
        // A tiny buildable crate whose `s.m()` needs the receiver's type
        // inferred — exactly what Bifrost's lexical resolver can't do and
        // rust-analyzer can. This is the reason the "LSP" chip offers the swap.
        let dir = std::env::temp_dir().join(format!("lgtm-ra-smoke-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"smoke\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let src = "pub struct S;\n\
                   impl S {\n\
                   \x20   pub fn m(&self) -> i32 { 1 }\n\
                   }\n\
                   pub fn caller(s: &S) -> Option<i32> {\n\
                   \x20   let _ = s.m();\n\
                   \x20   None\n\
                   }\n";
        std::fs::write(dir.join("src/lib.rs"), src).unwrap();

        let at = |needle: &str, offset: u32| {
            let (line, col) = src
                .lines()
                .enumerate()
                .find_map(|(i, l)| l.find(needle).map(|c| (i as u32, c as u32 + offset)))
                .unwrap_or_else(|| panic!("{needle:?} present"));
            LspPosition {
                path: "src/lib.rs".to_string(),
                line,
                character: col,
            }
        };
        // `m` inside `s.m()`, and `Option` in the return type (ASCII → byte==char).
        let method_pos = at("s.m()", 2);
        let option_pos = at("Option<i32>", 0);

        let progress = Arc::new(Mutex::new(LspProgress::default()));
        let session = LspSession::start(
            dir.clone(),
            LspBackend::RustAnalyzer,
            progress,
            Some(method_pos.clone()),
        )
        .expect("start rust-analyzer session");

        // Type inference resolves the method call.
        let hover = session
            .hover(method_pos, Arc::new(AtomicBool::new(false)))
            .expect("hover request should finish")
            .expect("rust-analyzer should resolve the method call");
        assert!(
            hover.text.contains("fn m"),
            "method hover should include the signature, got {:?}",
            hover.text
        );

        // Go-to-definition on a std type resolves outside the workspace and now
        // keeps its absolute on-disk path (the dependency/stdlib navigation fix).
        let defs = session
            .definition(option_pos)
            .expect("definition request should finish");
        let ext = defs
            .iter()
            .find(|d| Path::new(&d.path).is_absolute())
            .unwrap_or_else(|| panic!("expected an out-of-workspace target, got {defs:?}"));
        assert!(
            !ext.path.starts_with(dir.to_string_lossy().as_ref()),
            "std definition should live outside the crate, got {}",
            ext.path
        );
        assert!(
            ext.path.ends_with("option.rs"),
            "Option should resolve into core's option.rs, got {}",
            ext.path
        );
        assert!(
            std::path::Path::new(&ext.path).exists(),
            "the resolved dependency source should be on disk: {}",
            ext.path
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
