use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

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
}

impl LspClient {
    pub fn start(root: PathBuf) -> Result<Self> {
        let bin = bifrost_bin();
        let mut child = Command::new(&bin)
            .arg("--root")
            .arg(&root)
            .arg("--lsp")
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
        };
        this.initialize()?;
        Ok(this)
    }

    pub fn hover(&mut self, pos: &LspPosition) -> Result<Option<HoverResult>> {
        let params = json!({
            "textDocument": { "uri": self.uri_for(&pos.path)? },
            "position": { "line": pos.line, "character": pos.character },
        });
        let Some(result) = self.request("textDocument/hover", params)? else {
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
        let params = json!({
            "textDocument": { "uri": self.uri_for(&pos.path)? },
            "position": { "line": pos.line, "character": pos.character },
        });
        let Some(result) = self.request("textDocument/definition", params)? else {
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

    fn initialize(&mut self) -> Result<()> {
        let root_uri = file_uri(&self.root)?;
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {},
            "workspaceFolders": [{ "uri": root_uri, "name": "lgtm" }]
        });
        self.request("initialize", params)?;
        self.notify("initialized", json!({}))?;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Option<Value>> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        loop {
            let msg = self.read_message()?;
            if msg.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(err) = msg.get("error") {
                bail!("LSP {method} failed: {err}");
            }
            return Ok(msg.get("result").cloned());
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
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

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.notify("exit", json!({}));
        let _ = self.child.kill();
    }
}

fn bifrost_bin() -> PathBuf {
    std::env::var_os("LGTM_BIFROST")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let local = PathBuf::from("../bifrost/target/debug/bifrost");
            if local.exists() {
                local
            } else {
                PathBuf::from("bifrost")
            }
        })
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
