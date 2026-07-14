//! Local git diffs via the `git` CLI: "the PR I'd open from here" — everything
//! since the merge-base with the default branch (committed + staged + unstaged
//! + untracked), as one unified patch for diff-core to parse.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

/// How many untracked files to inline into the patch before giving up.
const MAX_UNTRACKED_FILES: usize = 200;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct SshProfile {
    pub command: Vec<String>,
    pub destination: Option<String>,
    pub remote_command_separator: Option<String>,
}

#[derive(Clone, Default)]
pub struct SshConnectionManager {
    sessions: Arc<Mutex<HashMap<SshProfile, Arc<Mutex<SshConnection>>>>>,
}

impl fmt::Debug for SshConnectionManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshConnectionManager")
            .finish_non_exhaustive()
    }
}

impl SshConnectionManager {
    pub fn run(&self, profile: &SshProfile, remote_command: &str) -> Result<Output> {
        let session = {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| anyhow!("SSH connection manager is poisoned"))?;
            if let Some(session) = sessions.get(profile) {
                Arc::clone(session)
            } else {
                let session = Arc::new(Mutex::new(SshConnection::spawn(profile)?));
                sessions.insert(profile.clone(), Arc::clone(&session));
                session
            }
        };
        let result = session
            .lock()
            .map_err(|_| anyhow!("SSH connection is poisoned"))?
            .run(remote_command);
        if result.is_err() {
            if let Ok(mut sessions) = self.sessions.lock() {
                sessions.remove(profile);
            }
        }
        result
    }
}

struct SshConnection {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr: Arc<Mutex<Vec<u8>>>,
    next_marker: u64,
}

impl SshConnection {
    fn spawn(profile: &SshProfile) -> Result<Self> {
        let (program, args) = profile
            .command
            .split_first()
            .context("SSH command is empty")?;
        let mut command = Command::new(program);
        command
            .args(args)
            .args(profile.destination.iter())
            .args(profile.remote_command_separator.iter())
            .args(["sh", "-s"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|err| anyhow!("failed to start SSH connection: {err}"))?;
        let stdin = child.stdin.take().context("SSH stdin is unavailable")?;
        let stdout = child.stdout.take().context("SSH stdout is unavailable")?;
        let stderr = child.stderr.take().context("SSH stderr is unavailable")?;
        let stderr_buffer = Arc::new(Mutex::new(Vec::new()));
        let stderr_output = Arc::clone(&stderr_buffer);
        thread::spawn(move || {
            let mut stderr = stderr;
            let mut bytes = [0u8; 4096];
            loop {
                let count = match stderr.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => count,
                };
                if let Ok(mut output) = stderr_output.lock() {
                    output.extend_from_slice(&bytes[..count]);
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr: stderr_buffer,
            next_marker: 0,
        })
    }

    fn run(&mut self, remote_command: &str) -> Result<Output> {
        if self.child.try_wait()?.is_some() {
            bail!("SSH connection exited before running the remote command");
        }
        let marker = format!("__LGTM_COMMAND_{}__", self.next_marker);
        self.next_marker += 1;
        if let Ok(mut stderr) = self.stderr.lock() {
            stderr.clear();
        }
        write!(
            self.stdin,
            "{remote_command}\nprintf '\\n%s %s\\n' {} \"$?\"\n",
            shell_quote(&marker)
        )?;
        self.stdin.flush()?;

        let prefix = format!("{marker} ").into_bytes();
        let mut stdout = Vec::new();
        let status_code = loop {
            let mut line = Vec::new();
            if self.stdout.read_until(b'\n', &mut line)? == 0 {
                let stderr = self.take_stderr();
                bail!(
                    "SSH connection closed while running remote command: {}",
                    String::from_utf8_lossy(&stderr).trim()
                );
            }
            if let Some(index) = find_bytes(&line, &prefix) {
                stdout.extend_from_slice(&line[..index]);
                if stdout.last() == Some(&b'\n') {
                    stdout.pop();
                }
                let status = &line[index + prefix.len()..];
                break String::from_utf8_lossy(status)
                    .trim()
                    .parse::<i32>()
                    .context("invalid SSH command status")?;
            }
            stdout.extend(line);
        };
        Ok(Output {
            status: exit_status(status_code),
            stdout,
            stderr: self.take_stderr(),
        })
    }

    fn take_stderr(&self) -> Vec<u8> {
        self.stderr
            .lock()
            .map(|mut stderr| std::mem::take(&mut *stderr))
            .unwrap_or_default()
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

impl Drop for SshConnection {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn exit_status(code: i32) -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(code as u32)
    }
}

/// Parse a pasted connection command such as `ssh devbox` or a custom wrapper.
pub fn parse_ssh_command(input: &str) -> Result<SshProfile> {
    let tokens = shlex::split(input).ok_or_else(|| anyhow!("invalid SSH command quoting"))?;
    if tokens.len() < 2 {
        bail!("expected an SSH command and destination, for example `ssh devbox`");
    }

    // Some wrappers supply the destination themselves and require `--` before
    // the command sent to the remote shell.
    let is_wrapper = tokens.last().is_some_and(|token| token == "ssh")
        || (tokens.len() >= 2
            && tokens[tokens.len() - 2] == "ssh"
            && tokens.last().is_some_and(|token| token == "--"));
    if is_wrapper {
        let command_len = if tokens.last().is_some_and(|token| token == "--") {
            tokens.len() - 1
        } else {
            tokens.len()
        };
        return Ok(SshProfile {
            command: tokens[..command_len].to_vec(),
            destination: None,
            remote_command_separator: Some("--".into()),
        });
    }

    let destination = tokens.last().cloned().unwrap();
    if destination.starts_with('-') || destination.chars().any(char::is_whitespace) {
        bail!("SSH destination is missing");
    }
    Ok(SshProfile {
        command: tokens[..tokens.len() - 1].to_vec(),
        destination: Some(destination),
        remote_command_separator: None,
    })
}

impl SshProfile {
    pub fn display(&self) -> String {
        let mut parts = self.command.clone();
        if let Some(destination) = &self.destination {
            parts.push(destination.clone());
        }
        if let Some(separator) = &self.remote_command_separator {
            parts.push(separator.clone());
        }
        parts.join(" ")
    }

    pub fn label(&self) -> String {
        self.destination
            .clone()
            .unwrap_or_else(|| self.command.join(" "))
    }
}

#[derive(Debug, Clone)]
pub struct RemoteSource {
    pub connections: SshConnectionManager,
    pub profile: SshProfile,
    pub repo_root: String,
    pub branch: String,
    pub base_ref: Option<String>,
    pub base_label: String,
    pub base_oid: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LocalSource {
    pub repo_root: PathBuf,
    pub branch: String,
    /// A user-selected base ref. None means use the repository default
    /// heuristic on refresh.
    pub base_ref: Option<String>,
    /// Human name of the diff base: "origin/main"-style ref when one exists
    /// and shares history with HEAD, otherwise "HEAD" (working-tree-only diff).
    pub base_label: String,
    /// Commit oid of the diff base, captured at resolve time: the merge-base
    /// with the selected base ref, or HEAD itself. None only in a repo with no
    /// commits yet (old side of every file is then absent).
    pub base_oid: Option<String>,
}

/// Resolve a path inside a git repo to its root, current branch, and diff base.
pub fn resolve_local(path: &Path) -> Result<LocalSource> {
    resolve_local_with_base(path, None)
}

/// Resolve a path inside a git repo using a specific base ref when provided.
pub fn resolve_local_with_base(path: &Path, base_ref: Option<&str>) -> Result<LocalSource> {
    let repo_root = PathBuf::from(
        git(path, &["rev-parse", "--show-toplevel"])
            .with_context(|| format!("{} is not inside a git repository", path.display()))?
            .trim(),
    );
    let branch = git(&repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string();

    if let Some(base_ref) = base_ref {
        let base_ref = base_ref.trim();
        if base_ref == "HEAD" {
            return Ok(LocalSource {
                repo_root,
                branch,
                base_ref: Some(base_ref.to_string()),
                base_label: "HEAD".to_string(),
                base_oid: git(path, &["rev-parse", "HEAD"])
                    .ok()
                    .map(|oid| oid.trim().to_string()),
            });
        }
        let oid = git(&repo_root, &["merge-base", "HEAD", base_ref])
            .with_context(|| format!("{base_ref} does not share history with HEAD"))?;
        return Ok(LocalSource {
            repo_root,
            branch,
            base_ref: Some(base_ref.to_string()),
            base_label: base_ref.to_string(),
            base_oid: Some(oid.trim().to_string()),
        });
    }

    // Default branch: prefer recorded remote HEAD symrefs, then conventional
    // fork/upstream names, then local main/master. A candidate only counts if
    // it shares a merge-base with HEAD; otherwise fall back to HEAD.
    let mut base_oid = None;
    let mut base_label = "HEAD".to_string();
    for cand in default_base_candidates(&repo_root) {
        if cand == branch {
            continue;
        }
        if let Ok(oid) = git(&repo_root, &["merge-base", "HEAD", &cand]) {
            base_oid = Some(oid.trim().to_string());
            base_label = cand;
            break;
        }
    }
    if base_oid.is_none() {
        // Working-tree-only diff; a repo with zero commits has no HEAD oid.
        base_oid = git(&repo_root, &["rev-parse", "HEAD"])
            .ok()
            .map(|oid| oid.trim().to_string());
    }

    Ok(LocalSource {
        repo_root,
        branch,
        base_ref: None,
        base_label,
        base_oid,
    })
}

/// Refs suitable for choosing a local diff base, in UI order.
pub fn list_base_refs(path: &Path) -> Result<Vec<String>> {
    let repo_root = PathBuf::from(
        git(path, &["rev-parse", "--show-toplevel"])
            .with_context(|| format!("{} is not inside a git repository", path.display()))?
            .trim(),
    );
    let branch = git(&repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string();
    let mut candidates = Vec::new();
    for cand in default_base_candidates(&repo_root) {
        push_unique(&mut candidates, cand);
    }
    push_unique(&mut candidates, "HEAD".to_string());
    let refs = git(
        &repo_root,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads",
            "refs/remotes",
        ],
    )?;
    for cand in refs.lines().map(str::trim).filter(|cand| !cand.is_empty()) {
        if cand == branch || cand.ends_with("/HEAD") {
            continue;
        }
        push_unique(&mut candidates, cand.to_string());
    }
    Ok(candidates
        .into_iter()
        .filter(|cand| cand == "HEAD" || git(&repo_root, &["merge-base", "HEAD", cand]).is_ok())
        .collect())
}

fn default_base_candidates(repo_root: &Path) -> Vec<String> {
    let mut candidates = Vec::new();
    push_remote_head(repo_root, "origin", &mut candidates);
    push_remote_head(repo_root, "upstream", &mut candidates);
    push_unique(&mut candidates, "origin/main".to_string());
    push_unique(&mut candidates, "upstream/main".to_string());
    push_unique(&mut candidates, "origin/master".to_string());
    push_unique(&mut candidates, "upstream/master".to_string());
    push_unique(&mut candidates, "main".to_string());
    push_unique(&mut candidates, "master".to_string());
    candidates
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn push_remote_head(repo_root: &Path, remote: &str, candidates: &mut Vec<String>) {
    if let Ok(symref) = git(
        repo_root,
        &["symbolic-ref", &format!("refs/remotes/{remote}/HEAD")],
    ) {
        if let Some(name) = symref.trim().strip_prefix("refs/remotes/") {
            push_unique(candidates, name.to_string());
        }
    }
}

pub fn resolve_remote(src: &RemoteSource) -> Result<RemoteSource> {
    let repo_root = remote_git(
        &src.connections,
        &src.profile,
        &src.repo_root,
        &["rev-parse", "--show-toplevel"],
    )?
        .trim()
        .to_string();
    let branch = remote_git(
        &src.connections,
        &src.profile,
        &repo_root,
        &["rev-parse", "--abbrev-ref", "HEAD"],
    )?
        .trim()
        .to_string();

    if let Some(base_ref) = src.base_ref.as_deref() {
        let base_ref = base_ref.trim();
        let base_oid = if base_ref == "HEAD" {
            remote_git(
                &src.connections,
                &src.profile,
                &repo_root,
                &["rev-parse", "HEAD"],
            )
                .ok()
                .map(|oid| oid.trim().to_string())
        } else {
            Some(
                remote_git(
                    &src.connections,
                    &src.profile,
                    &repo_root,
                    &["merge-base", "HEAD", base_ref],
                )?
                    .trim()
                    .to_string(),
            )
        };
        return Ok(RemoteSource {
            connections: src.connections.clone(),
            profile: src.profile.clone(),
            repo_root,
            branch,
            base_ref: Some(base_ref.to_string()),
            base_label: base_ref.to_string(),
            base_oid,
        });
    }

    let mut base_oid = None;
    let mut base_label = "HEAD".to_string();
    for candidate in remote_default_base_candidates(&src.connections, &src.profile, &repo_root) {
        if candidate == branch {
            continue;
        }
        if let Ok(oid) = remote_git(
            &src.connections,
            &src.profile,
            &repo_root,
            &["merge-base", "HEAD", &candidate],
        ) {
            base_oid = Some(oid.trim().to_string());
            base_label = candidate;
            break;
        }
    }
    if base_oid.is_none() {
        base_oid = remote_git(
            &src.connections,
            &src.profile,
            &repo_root,
            &["rev-parse", "HEAD"],
        )
            .ok()
            .map(|oid| oid.trim().to_string());
    }

    Ok(RemoteSource {
        connections: src.connections.clone(),
        profile: src.profile.clone(),
        repo_root,
        branch,
        base_ref: None,
        base_label,
        base_oid,
    })
}

pub fn list_remote_base_refs(src: &RemoteSource) -> Result<Vec<String>> {
    let repo_root = remote_git(
        &src.connections,
        &src.profile,
        &src.repo_root,
        &["rev-parse", "--show-toplevel"],
    )?
        .trim()
        .to_string();
    let branch = remote_git(
        &src.connections,
        &src.profile,
        &repo_root,
        &["rev-parse", "--abbrev-ref", "HEAD"],
    )?
        .trim()
        .to_string();
    let mut candidates =
        remote_default_base_candidates(&src.connections, &src.profile, &repo_root);
    push_unique(&mut candidates, "HEAD".to_string());
    let refs = remote_git(
        &src.connections,
        &src.profile,
        &repo_root,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads",
            "refs/remotes",
        ],
    )?;
    for candidate in refs
        .lines()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if candidate == branch || candidate.ends_with("/HEAD") {
            continue;
        }
        push_unique(&mut candidates, candidate.to_string());
    }
    Ok(candidates)
}

pub fn remote_diff_patch(src: &RemoteSource) -> Result<String> {
    let base = src.base_oid.as_deref().unwrap_or("HEAD");
    let mut patch = remote_git(
        &src.connections,
        &src.profile,
        &src.repo_root,
        &["diff", "-M", "--no-color", "--no-ext-diff", base],
    )?;
    let untracked = remote_git(
        &src.connections,
        &src.profile,
        &src.repo_root,
        &["ls-files", "--others", "--exclude-standard"],
    )?;
    for file in untracked.lines().take(MAX_UNTRACKED_FILES) {
        let output = src.connections.run(
            &src.profile,
            &remote_git_command(
                &src.repo_root,
                &[
                    "diff".to_string(),
                    "--no-color".to_string(),
                    "--no-ext-diff".to_string(),
                    "--no-index".to_string(),
                    "--".to_string(),
                    "/dev/null".to_string(),
                    file.to_string(),
                ],
            ),
        )?;
        if !matches!(output.status.code(), Some(0) | Some(1)) {
            bail!(
                "remote git diff --no-index {file} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        patch.push_str(&String::from_utf8_lossy(&output.stdout));
    }
    Ok(patch)
}

pub fn remote_file_at_base(src: &RemoteSource, path: &str) -> Option<String> {
    let oid = src.base_oid.as_deref()?;
    remote_git(
        &src.connections,
        &src.profile,
        &src.repo_root,
        &["show", &format!("{oid}:{path}")],
    )
    .ok()
}

pub fn remote_file_at_worktree(src: &RemoteSource, path: &str) -> Option<String> {
    let output = src.connections.run(
        &src.profile,
        &remote_shell_command(
            "cat",
            &[format!("{}/{}", src.repo_root.trim_end_matches('/'), path)],
        ),
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn remote_default_base_candidates(
    connections: &SshConnectionManager,
    profile: &SshProfile,
    repo_root: &str,
) -> Vec<String> {
    let mut candidates = Vec::new();
    for remote in ["origin", "upstream"] {
        if let Ok(symref) = remote_git(
            connections,
            profile,
            repo_root,
            &["symbolic-ref", &format!("refs/remotes/{remote}/HEAD")],
        ) {
            if let Some(name) = symref.trim().strip_prefix("refs/remotes/") {
                push_unique(&mut candidates, name.to_string());
            }
        }
    }
    for candidate in [
        "origin/main",
        "upstream/main",
        "origin/master",
        "upstream/master",
        "main",
        "master",
    ] {
        push_unique(&mut candidates, candidate.to_string());
    }
    candidates
}

fn remote_git(
    connections: &SshConnectionManager,
    profile: &SshProfile,
    repo_root: &str,
    args: &[&str],
) -> Result<String> {
    let args = args
        .iter()
        .map(|arg| (*arg).to_string())
        .collect::<Vec<_>>();
    let output = connections.run(profile, &remote_git_command(repo_root, &args))?;
    if !output.status.success() {
        bail!(
            "remote git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn remote_git_command(root: &str, args: &[String]) -> String {
    let mut command = vec!["git".to_string(), "-C".to_string(), root.to_string()];
    command.extend(args.iter().cloned());
    remote_shell_command_parts(&command)
}

fn remote_shell_command(program: &str, args: &[String]) -> String {
    let mut command = vec![program.to_string()];
    command.extend(args.iter().cloned());
    remote_shell_command_parts(&command)
}

fn remote_shell_command_parts(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Full contents of `path` at the captured diff base, for the Phase-2 upgrade.
/// None means "old side absent or unusable": untracked/added files, binary or
/// non-UTF-8 content, or no base commit. Errors collapse to None too — the
/// caller keeps that file's patch-derived view.
pub fn file_at_base(src: &LocalSource, path: &str) -> Option<String> {
    let oid = src.base_oid.as_deref()?;
    let output = Command::new("git")
        .arg("-C")
        .arg(&src.repo_root)
        .args(["show", &format!("{oid}:{path}")])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Unified patch of everything that would go into a PR opened from here:
/// merge-base(HEAD, base)..working-tree (two-dot, so committed + staged +
/// unstaged), plus untracked files appended as added-file diffs.
pub fn diff_patch(src: &LocalSource) -> Result<String> {
    // The oid captured at resolve time; "HEAD" only in a zero-commit repo,
    // where the committed-diff half is empty anyway.
    let base = src.base_oid.clone().unwrap_or_else(|| "HEAD".to_string());
    let mut patch = git(
        &src.repo_root,
        &["diff", "-M", "--no-color", "--no-ext-diff", &base],
    )?;

    let untracked = git(
        &src.repo_root,
        &["ls-files", "--others", "--exclude-standard"],
    )?;
    for file in untracked.lines().take(MAX_UNTRACKED_FILES) {
        // `--no-index` against /dev/null renders an untracked file as an
        // added-file diff; it exits 1 when the sides differ, which is success
        // here (0 would mean an empty file — also fine, git emits a header).
        let output = Command::new("git")
            .arg("-C")
            .arg(&src.repo_root)
            .args([
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-index",
                "--",
                "/dev/null",
            ])
            .arg(file)
            .output()
            .map_err(|err| anyhow!("failed to run git: {err}"))?;
        if !matches!(output.status.code(), Some(0) | Some(1)) {
            bail!(
                "git diff --no-index /dev/null {file} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        patch.push_str(&String::from_utf8_lossy(&output.stdout));
    }
    Ok(patch)
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|err| anyhow!("failed to run git (is git installed?): {err}"))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use diff_core::FileStatus;
    use std::fs;

    fn run(dir: &Path, args: &[&str]) {
        let output = Command::new(args[0])
            .args(&args[1..])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo(dir: &Path) {
        run(dir, &["git", "init", "-b", "main"]);
        run(dir, &["git", "config", "user.email", "test@example.com"]);
        run(dir, &["git", "config", "user.name", "Test"]);
        run(dir, &["git", "config", "commit.gpgsign", "false"]);
    }

    #[test]
    fn parses_pasted_ssh_commands() {
        assert_eq!(
            parse_ssh_command("ssh -p 2222 user@example.com").unwrap(),
            SshProfile {
                command: vec!["ssh".into(), "-p".into(), "2222".into()],
                destination: Some("user@example.com".into()),
                remote_command_separator: None,
            }
        );
        assert_eq!(
            parse_ssh_command("custom ssh --profile devbox devbox").unwrap(),
            SshProfile {
                command: vec![
                    "custom".into(),
                    "ssh".into(),
                    "--profile".into(),
                    "devbox".into()
                ],
                destination: Some("devbox".into()),
                remote_command_separator: None,
            }
        );
        assert_eq!(
            parse_ssh_command("custom ssh").unwrap(),
            SshProfile {
                command: vec!["custom".into(), "ssh".into()],
                destination: None,
                remote_command_separator: Some("--".into()),
            }
        );
        assert_eq!(parse_ssh_command("custom ssh").unwrap().display(), "custom ssh --");
        assert!(parse_ssh_command("ssh devbox 'git status'").is_err());
        assert!(parse_ssh_command("ssh").is_err());
    }

    #[test]
    fn remote_commands_quote_paths_and_arguments() {
        assert_eq!(
            remote_git_command(
                "/tmp/repo with space",
                &["show".into(), "HEAD:it's here.rs".into()]
            ),
            "'git' '-C' '/tmp/repo with space' 'show' 'HEAD:it'\\''s here.rs'"
        );
        assert_eq!(
            remote_shell_command("cat", &["/tmp/repo/it's here.rs".into()]),
            "'cat' '/tmp/repo/it'\\''s here.rs'"
        );
    }

    #[test]
    fn wrapper_profiles_separate_remote_commands() {
        let profile = SshProfile {
            command: vec!["/bin/sh".into(), "-c".into(), "exec \"$@\"".into()],
            destination: None,
            remote_command_separator: Some("--".into()),
        };
        let manager = SshConnectionManager::default();
        let first = manager
            .run(&profile, "printf '%s\\n' \"$$\"")
            .unwrap();
        let no_newline = manager.run(&profile, "printf 'first'").unwrap();
        let second = manager
            .run(&profile, "printf '%s\\n' \"$$\"")
            .unwrap();
        assert_eq!(first.status.code(), Some(0));
        assert_eq!(second.status.code(), Some(0));
        assert_eq!(first.stdout, second.stdout);
        assert_eq!(no_newline.stdout, b"first");
    }

    #[test]
    fn no_remote_main_branch_diffs_working_tree_against_head() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
        run(dir, &["git", "add", "."]);
        run(dir, &["git", "commit", "-m", "init"]);

        // Unstaged edit + untracked text file + untracked binary file.
        fs::write(dir.join("a.rs"), "fn main() { println!(); }\n").unwrap();
        fs::write(dir.join("new.txt"), "hello\n").unwrap();
        fs::write(dir.join("blob.bin"), [0u8, 159, 146, 150]).unwrap();

        let src = resolve_local(dir).unwrap();
        assert_eq!(
            src.repo_root.canonicalize().unwrap(),
            dir.canonicalize().unwrap()
        );
        assert_eq!(src.branch, "main");
        assert_eq!(src.base_label, "HEAD");

        let patch = diff_patch(&src).unwrap();
        let diff = diff_core::parse_patch(&patch);
        let by_path: Vec<(&str, FileStatus)> = diff
            .files
            .iter()
            .map(|f| (f.display_path(), f.status))
            .collect();
        assert!(
            by_path.contains(&("a.rs", FileStatus::Modified)),
            "{by_path:?}"
        );
        assert!(
            by_path.contains(&("new.txt", FileStatus::Added)),
            "{by_path:?}"
        );
        assert!(
            by_path.contains(&("blob.bin", FileStatus::Binary)),
            "{by_path:?}"
        );
    }

    #[test]
    fn no_remote_feature_branch_diffs_against_local_main() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        fs::write(dir.join("lib.rs"), "pub fn one() {}\n").unwrap();
        run(dir, &["git", "add", "."]);
        run(dir, &["git", "commit", "-m", "init"]);
        run(dir, &["git", "checkout", "-b", "feature"]);
        fs::write(dir.join("lib.rs"), "pub fn one() {}\npub fn two() {}\n").unwrap();
        run(dir, &["git", "commit", "-am", "add two"]);

        let src = resolve_local(dir).unwrap();
        assert_eq!(src.branch, "feature");
        assert_eq!(src.base_label, "main");

        let patch = diff_patch(&src).unwrap();
        let diff = diff_core::parse_patch(&patch);
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].display_path(), "lib.rs");
        assert_eq!((diff.files[0].additions, diff.files[0].deletions), (1, 0));
    }

    #[test]
    fn explicit_base_ref_overrides_default_base() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        fs::write(dir.join("lib.rs"), "pub fn one() {}\n").unwrap();
        run(dir, &["git", "add", "."]);
        run(dir, &["git", "commit", "-m", "init"]);
        run(dir, &["git", "checkout", "-b", "release"]);
        fs::write(dir.join("lib.rs"), "pub fn one() {}\npub fn release() {}\n").unwrap();
        run(dir, &["git", "commit", "-am", "release"]);
        run(dir, &["git", "checkout", "main"]);
        run(dir, &["git", "checkout", "-b", "feature"]);
        fs::write(dir.join("lib.rs"), "pub fn one() {}\npub fn feature() {}\n").unwrap();
        run(dir, &["git", "commit", "-am", "feature"]);

        let auto = resolve_local(dir).unwrap();
        assert_eq!(auto.base_label, "main");
        assert_eq!(auto.base_ref, None);

        let explicit = resolve_local_with_base(dir, Some("release")).unwrap();
        assert_eq!(explicit.base_label, "release");
        assert_eq!(explicit.base_ref.as_deref(), Some("release"));
        assert_eq!(
            explicit.base_oid.as_deref(),
            Some(git(dir, &["merge-base", "HEAD", "release"]).unwrap().trim())
        );
    }

    #[test]
    fn list_base_refs_includes_head_and_local_branches() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        fs::write(dir.join("lib.rs"), "pub fn one() {}\n").unwrap();
        run(dir, &["git", "add", "."]);
        run(dir, &["git", "commit", "-m", "init"]);
        run(dir, &["git", "checkout", "-b", "release"]);
        run(dir, &["git", "checkout", "main"]);
        run(dir, &["git", "checkout", "-b", "feature"]);

        let refs = list_base_refs(dir).unwrap();
        assert!(refs.iter().any(|base| base == "HEAD"), "{refs:?}");
        assert!(refs.iter().any(|base| base == "main"), "{refs:?}");
        assert!(refs.iter().any(|base| base == "release"), "{refs:?}");
        assert!(!refs.iter().any(|base| base == "feature"), "{refs:?}");
    }

    #[test]
    fn branch_diffs_against_origin_default_head() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("upstream");
        fs::create_dir(&upstream).unwrap();
        init_repo(&upstream);
        fs::write(upstream.join("lib.rs"), "pub fn one() {}\n").unwrap();
        run(&upstream, &["git", "add", "."]);
        run(&upstream, &["git", "commit", "-m", "init"]);

        let clone = tmp.path().join("clone");
        run(
            tmp.path(),
            &[
                "git",
                "clone",
                upstream.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        run(&clone, &["git", "config", "user.email", "test@example.com"]);
        run(&clone, &["git", "config", "user.name", "Test"]);
        run(&clone, &["git", "config", "commit.gpgsign", "false"]);
        run(&clone, &["git", "checkout", "-b", "feature"]);
        fs::write(clone.join("lib.rs"), "pub fn one() {}\npub fn two() {}\n").unwrap();
        run(&clone, &["git", "commit", "-am", "add two"]);
        // Plus an uncommitted edit on top: two-dot diff must include it.
        fs::write(
            clone.join("lib.rs"),
            "pub fn one() {}\npub fn two() {}\npub fn three() {}\n",
        )
        .unwrap();

        let src = resolve_local(&clone).unwrap();
        assert_eq!(src.branch, "feature");
        assert_eq!(src.base_label, "origin/main");

        let patch = diff_patch(&src).unwrap();
        let diff = diff_core::parse_patch(&patch);
        assert_eq!(diff.files.len(), 1);
        let file = &diff.files[0];
        assert_eq!(file.display_path(), "lib.rs");
        assert_eq!(file.status, FileStatus::Modified);
        // Committed line + uncommitted line, both present.
        assert_eq!((file.additions, file.deletions), (2, 0));
    }

    #[test]
    fn fork_branch_diffs_against_upstream_main_when_origin_main_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("upstream");
        fs::create_dir(&upstream).unwrap();
        init_repo(&upstream);
        fs::write(upstream.join("lib.rs"), "pub fn one() {}\n").unwrap();
        run(&upstream, &["git", "add", "."]);
        run(&upstream, &["git", "commit", "-m", "init"]);

        let fork = tmp.path().join("fork");
        run(
            tmp.path(),
            &["git", "clone", upstream.to_str().unwrap(), fork.to_str().unwrap()],
        );
        run(&fork, &["git", "remote", "rename", "origin", "upstream"]);
        let empty_origin = tmp.path().join("origin");
        fs::create_dir(&empty_origin).unwrap();
        init_repo(&empty_origin);
        run(&fork, &["git", "remote", "add", "origin", empty_origin.to_str().unwrap()]);
        run(&fork, &["git", "config", "user.email", "test@example.com"]);
        run(&fork, &["git", "config", "user.name", "Test"]);
        run(&fork, &["git", "config", "commit.gpgsign", "false"]);
        run(&fork, &["git", "checkout", "-b", "feature"]);
        fs::write(fork.join("lib.rs"), "pub fn one() {}\npub fn two() {}\n").unwrap();
        run(&fork, &["git", "commit", "-am", "add two"]);

        let src = resolve_local(&fork).unwrap();
        assert_eq!(src.branch, "feature");
        assert_eq!(src.base_label, "upstream/main");
    }

    #[test]
    fn file_at_base_reads_committed_content() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        init_repo(dir);
        fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.join("blob.bin"), [0u8, 159, 146, 150]).unwrap();
        run(dir, &["git", "add", "."]);
        run(dir, &["git", "commit", "-m", "init"]);
        fs::write(dir.join("a.rs"), "fn main() { changed(); }\n").unwrap();
        fs::write(dir.join("new.txt"), "untracked\n").unwrap();

        let src = resolve_local(dir).unwrap();
        // HEAD base still captures a concrete oid.
        assert_eq!(src.base_label, "HEAD");
        assert!(src.base_oid.is_some());

        // Old side = committed content, not the working tree.
        assert_eq!(
            file_at_base(&src, "a.rs").as_deref(),
            Some("fn main() {}\n")
        );
        // Untracked: absent at base. Binary: non-UTF-8 → None.
        assert_eq!(file_at_base(&src, "new.txt"), None);
        assert_eq!(file_at_base(&src, "blob.bin"), None);
        assert_eq!(file_at_base(&src, "no/such/file.rs"), None);
    }

    #[test]
    fn base_oid_is_merge_base_with_remote_head() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("upstream");
        fs::create_dir(&upstream).unwrap();
        init_repo(&upstream);
        fs::write(upstream.join("lib.rs"), "pub fn one() {}\n").unwrap();
        run(&upstream, &["git", "add", "."]);
        run(&upstream, &["git", "commit", "-m", "init"]);

        let clone = tmp.path().join("clone");
        run(
            tmp.path(),
            &[
                "git",
                "clone",
                upstream.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        run(&clone, &["git", "config", "user.email", "test@example.com"]);
        run(&clone, &["git", "config", "user.name", "Test"]);
        run(&clone, &["git", "config", "commit.gpgsign", "false"]);
        run(&clone, &["git", "checkout", "-b", "feature"]);
        fs::write(clone.join("lib.rs"), "pub fn one() {}\npub fn two() {}\n").unwrap();
        run(&clone, &["git", "commit", "-am", "add two"]);

        let src = resolve_local(&clone).unwrap();
        assert_eq!(src.base_label, "origin/main");
        let expected = git(&clone, &["merge-base", "HEAD", "origin/main"]).unwrap();
        assert_eq!(src.base_oid.as_deref(), Some(expected.trim()));
        // Old side comes from the merge-base commit, before the feature edit.
        assert_eq!(
            file_at_base(&src, "lib.rs").as_deref(),
            Some("pub fn one() {}\n")
        );
    }

    #[test]
    fn resolve_rejects_non_repo() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(resolve_local(tmp.path()).is_err());
    }
}
