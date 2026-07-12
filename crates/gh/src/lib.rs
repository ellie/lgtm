//! Fetch PR data via the `gh` CLI, piggybacking on the user's `gh auth`.

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct PrLocator {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl PrLocator {
    pub fn repo_slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

/// Accepts `owner/repo#123`, `#123`, `123`, or a GitHub PR URL. Bare numbers
/// resolve the repo from the current directory's git remote (via `gh`).
pub fn resolve_pr_arg(arg: &str) -> Result<PrLocator> {
    if let Some(rest) = arg
        .strip_prefix("https://github.com/")
        .or_else(|| arg.strip_prefix("http://github.com/"))
        .or_else(|| arg.strip_prefix("github.com/"))
    {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() >= 4 && parts[2] == "pull" {
            let digits: String = parts[3].chars().take_while(char::is_ascii_digit).collect();
            let number = digits
                .parse()
                .with_context(|| format!("no PR number in URL {arg}"))?;
            return Ok(PrLocator {
                owner: parts[0].to_string(),
                repo: parts[1].to_string(),
                number,
            });
        }
        bail!("unrecognized GitHub URL: {arg}");
    }

    if let Some((repo_part, number)) = arg.split_once('#') {
        let number = number
            .parse()
            .with_context(|| format!("invalid PR number in {arg}"))?;
        if repo_part.is_empty() {
            return locator_in_cwd_repo(number);
        }
        let (owner, repo) = repo_part
            .split_once('/')
            .with_context(|| format!("expected owner/repo before '#' in {arg}"))?;
        return Ok(PrLocator {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        });
    }

    if let Ok(number) = arg.parse() {
        return locator_in_cwd_repo(number);
    }

    bail!("could not parse {arg:?}; expected owner/repo#123, a PR URL, or a PR number")
}

fn locator_in_cwd_repo(number: u64) -> Result<PrLocator> {
    let out = gh(&[
        "repo",
        "view",
        "--json",
        "nameWithOwner",
        "--jq",
        ".nameWithOwner",
    ])
    .context("couldn't infer the repo from the current directory; use owner/repo#123")?;
    let (owner, repo) = out
        .trim()
        .split_once('/')
        .with_context(|| format!("unexpected gh repo view output: {out}"))?;
    Ok(PrLocator {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
    })
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrMeta {
    pub number: u64,
    pub title: String,
    pub author: Author,
    pub state: String,
    pub url: String,
    /// PR description (markdown); empty when the PR has none. Used as chat
    /// context, not rendered in the UI.
    #[serde(default)]
    pub body: String,
    pub base_ref_name: String,
    pub head_ref_name: String,
    #[serde(default)]
    pub base_ref_oid: String,
    #[serde(default)]
    pub head_ref_oid: String,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    /// "APPROVED", "CHANGES_REQUESTED", "REVIEW_REQUIRED", or "" (no
    /// required reviews and none given).
    #[serde(default)]
    pub review_decision: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Author {
    pub login: String,
}

pub fn fetch_meta(loc: &PrLocator) -> Result<PrMeta> {
    let json = gh(&[
        "pr",
        "view",
        &loc.number.to_string(),
        "--repo",
        &loc.repo_slug(),
        "--json",
        "number,title,author,state,url,body,baseRefName,headRefName,baseRefOid,headRefOid,\
         additions,deletions,changedFiles,reviewDecision",
    ])?;
    serde_json::from_str(&json).context("unexpected gh pr view JSON")
}

/// One row of `gh pr list` output, for the PR picker.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrSummary {
    pub number: u64,
    pub title: String,
    pub author: Author,
    pub state: String,
    pub is_draft: bool,
    pub head_ref_name: String,
    pub updated_at: String,
}

/// Open PRs for a repo, most recently updated first (gh's default order).
pub fn list_prs(owner: &str, repo: &str) -> Result<Vec<PrSummary>> {
    let json = gh(&[
        "pr",
        "list",
        "--repo",
        &format!("{owner}/{repo}"),
        "--state",
        "open",
        "--limit",
        "200",
        "--json",
        "number,title,author,state,isDraft,headRefName,updatedAt",
    ])?;
    serde_json::from_str(&json).context("unexpected gh pr list JSON")
}

/// One PR review comment from the REST pulls/comments API. Unlike `gh pr
/// view --json` (camelCase), this endpoint returns snake_case field names, so
/// no `rename_all` here.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ReviewComment {
    pub id: u64,
    pub path: String,
    /// Anchor line on `side` against the *current* diff; None = outdated
    /// (the code the comment was written against has since changed).
    pub line: Option<u64>,
    /// "RIGHT" (new side) or "LEFT" (old side).
    pub side: Option<String>,
    /// Multi-line comments start here and anchor at `line` (the end), like
    /// GitHub's own UI.
    pub start_line: Option<u64>,
    pub body: String,
    pub user: Author,
    pub created_at: String,
    pub in_reply_to_id: Option<u64>,
}

/// Every review comment on the PR. `--paginate --slurp` (gh ≥ 2.66; we
/// require it) wraps each page's JSON array into one array-of-arrays —
/// without `--slurp`, `--paginate` concatenates the arrays back-to-back
/// ("[…][…]"), which serde can't parse.
pub fn fetch_review_comments(loc: &PrLocator) -> Result<Vec<ReviewComment>> {
    let json = gh(&[
        "api",
        "--paginate",
        "--slurp",
        &format!(
            "repos/{}/{}/pulls/{}/comments?per_page=100",
            loc.owner, loc.repo, loc.number
        ),
    ])?;
    let pages: Vec<Vec<ReviewComment>> =
        serde_json::from_str(&json).context("unexpected gh pulls/comments JSON")?;
    Ok(pages.into_iter().flatten().collect())
}

/// Post a new top-level review comment anchored at (path, side, line) against
/// `commit_id` (the PR's head oid). Errors carry gh's stderr — a 403 usually
/// means a missing token scope, a 422 an unanchorable line.
pub fn post_review_comment(
    loc: &PrLocator,
    commit_id: &str,
    path: &str,
    side: &str,
    line: u64,
    body: &str,
) -> Result<()> {
    gh(&[
        "api",
        "-X",
        "POST",
        &format!(
            "repos/{}/{}/pulls/{}/comments",
            loc.owner, loc.repo, loc.number
        ),
        "-f",
        &format!("body={body}"),
        "-f",
        &format!("commit_id={commit_id}"),
        "-f",
        &format!("path={path}"),
        "-f",
        &format!("side={side}"),
        // -F, not -f: line must be a JSON integer, not a string.
        "-F",
        &format!("line={line}"),
    ])?;
    Ok(())
}

/// The verdict of a top-level PR review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewVerdict {
    Approve,
    RequestChanges,
    Comment,
}

/// Submit a top-level review on the PR. GitHub requires a non-empty body for
/// request-changes and comment reviews (the caller enforces this so the
/// error surfaces before a network round-trip); approvals may be bodyless.
pub fn submit_review(loc: &PrLocator, verdict: ReviewVerdict, body: &str) -> Result<()> {
    let number = loc.number.to_string();
    let slug = loc.repo_slug();
    let flag = match verdict {
        ReviewVerdict::Approve => "--approve",
        ReviewVerdict::RequestChanges => "--request-changes",
        ReviewVerdict::Comment => "--comment",
    };
    let mut args = vec!["pr", "review", &number, "--repo", &slug, flag];
    if !body.is_empty() {
        args.push("--body");
        args.push(body);
    }
    gh(&args)?;
    Ok(())
}

/// Reply to the review thread rooted at `comment_id`.
pub fn post_reply(loc: &PrLocator, comment_id: u64, body: &str) -> Result<()> {
    gh(&[
        "api",
        "-X",
        "POST",
        &format!(
            "repos/{}/{}/pulls/{}/comments/{}/replies",
            loc.owner, loc.repo, loc.number, comment_id
        ),
        "-f",
        &format!("body={body}"),
    ])?;
    Ok(())
}

pub fn fetch_patch(loc: &PrLocator) -> Result<String> {
    gh(&[
        "pr",
        "diff",
        &loc.number.to_string(),
        "--repo",
        &loc.repo_slug(),
    ])
}

/// Blob-size cap: PR review never needs multi-megabyte files, and the raw
/// contents API happily serves up to 100 MB.
const MAX_BLOB_BYTES: usize = 1024 * 1024;

/// Full contents of `path` at `commit_oid`, via the raw contents API.
/// `Ok(None)` means "leave this file un-upgraded": absent on that side (404),
/// non-UTF-8 (binary), or larger than [`MAX_BLOB_BYTES`]. A path at a commit
/// is immutable, so results — including negative ones — are cached on disk in
/// `~/.cache/lgtm/blobs/` (sha256 of `repo\0oid\0path`, with an `.absent`
/// sidecar marking negative entries).
pub fn fetch_file_at(loc: &PrLocator, commit_oid: &str, path: &str) -> Result<Option<String>> {
    let cache = cache_path(&loc.repo_slug(), commit_oid, path);
    if let Some(cache) = &cache {
        if cache.with_extension("absent").exists() {
            return Ok(None);
        }
        if let Ok(bytes) = std::fs::read(cache) {
            return Ok(String::from_utf8(bytes).ok());
        }
    }

    let endpoint = format!(
        "repos/{}/{}/contents/{}?ref={}",
        loc.owner,
        loc.repo,
        encode_path(path),
        commit_oid
    );
    let output = Command::new("gh")
        .args([
            "api",
            "-H",
            "Accept: application/vnd.github.raw+json",
            &endpoint,
        ])
        .output()
        .map_err(|err| anyhow!("failed to run gh (is the GitHub CLI installed?): {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("404") {
            mark_absent(cache);
            return Ok(None);
        }
        bail!("gh api {endpoint} failed: {}", stderr.trim());
    }
    if output.stdout.len() > MAX_BLOB_BYTES {
        mark_absent(cache);
        return Ok(None);
    }
    let Ok(text) = String::from_utf8(output.stdout) else {
        mark_absent(cache);
        return Ok(None);
    };
    if let Some(cache) = cache {
        let _ = std::fs::write(cache, &text);
    }
    Ok(Some(text))
}

fn mark_absent(cache: Option<PathBuf>) {
    if let Some(cache) = cache {
        let _ = std::fs::write(cache.with_extension("absent"), b"");
    }
}

/// `~/.cache/lgtm/blobs/<key>`, creating the directory; None when HOME is
/// unset or the directory can't be created (cache disabled, fetch still works).
fn cache_path(repo: &str, oid: &str, path: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("HOME")?)
        .join(".cache")
        .join("lgtm")
        .join("blobs");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(cache_key(repo, oid, path)))
}

/// Stable cache key: hex sha256 of `repo\0oid\0path`.
pub fn cache_key(repo: &str, oid: &str, path: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo.as_bytes());
    hasher.update([0]);
    hasher.update(oid.as_bytes());
    hasher.update([0]);
    hasher.update(path.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Percent-encode a repo path for the contents API, keeping `/` separators:
/// every byte outside RFC 3986 unreserved is encoded, per segment.
pub fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn gh(args: &[&str]) -> Result<String> {
    let output = Command::new("gh")
        .args(args)
        .output()
        .map_err(|err| anyhow!("failed to run gh (is the GitHub CLI installed?): {err}"))?;
    if !output.status.success() {
        bail!(
            "gh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("gh output was not UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_slug_form() {
        let loc = resolve_pr_arg("zed-industries/zed#12345").unwrap();
        assert_eq!(loc.owner, "zed-industries");
        assert_eq!(loc.repo, "zed");
        assert_eq!(loc.number, 12345);
    }

    #[test]
    fn parses_url_form() {
        let loc = resolve_pr_arg("https://github.com/rust-lang/rust/pull/99999/files").unwrap();
        assert_eq!(loc.owner, "rust-lang");
        assert_eq!(loc.repo, "rust");
        assert_eq!(loc.number, 99999);
    }

    #[test]
    fn rejects_garbage() {
        assert!(resolve_pr_arg("not-a-pr").is_err());
    }

    #[test]
    fn encodes_paths_per_segment_keeping_slashes() {
        assert_eq!(encode_path("src/main.rs"), "src/main.rs");
        assert_eq!(
            encode_path("dir with space/naïve+file#1.rs"),
            "dir%20with%20space/na%C3%AFve%2Bfile%231.rs"
        );
        assert_eq!(encode_path("a?b&c=d/e%f"), "a%3Fb%26c%3Dd/e%25f");
        assert_eq!(encode_path("A-Z_a.z~0/9"), "A-Z_a.z~0/9");
    }

    #[test]
    fn cache_key_is_stable() {
        // Pinned: changing this constant silently invalidates every user's
        // on-disk cache. The components are NUL-separated so `("a/b", "c")`
        // and `("a", "b/c")` can't collide.
        assert_eq!(
            cache_key(
                "BurntSushi/ripgrep",
                "f16ea0a8cfd0fbb0328b8348972356d532b921d0",
                "crates/core/main.rs"
            ),
            "1b21e76f45d6d948cf7b44c696608c64bdaeee714ac61b21dce21750ad9cb6bc"
        );
        assert_ne!(
            cache_key("o/r", "oid", "a/b"),
            cache_key("o/r/a", "oid", "b")
        );
    }

    #[test]
    fn deserializes_pr_meta_with_oids() {
        let json = r#"{
            "number": 1, "title": "t", "author": {"login": "a"}, "state": "OPEN",
            "url": "https://github.com/o/r/pull/1",
            "baseRefName": "main", "headRefName": "feat",
            "baseRefOid": "abc123", "headRefOid": "def456",
            "additions": 1, "deletions": 2, "changedFiles": 3, "reviewDecision": "CHANGES_REQUESTED"
        }"#;
        let meta: PrMeta = serde_json::from_str(json).unwrap();
        assert_eq!(meta.base_ref_oid, "abc123");
        assert_eq!(meta.head_ref_oid, "def456");
        assert_eq!(meta.review_decision, "CHANGES_REQUESTED");

        // Older gh output without the field still deserializes.
        let json = json.replace(r#", "reviewDecision": "CHANGES_REQUESTED""#, "");
        let meta: PrMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(meta.review_decision, "");
    }

    #[test]
    fn deserializes_review_comments() {
        // Shaped like `gh api --paginate --slurp`: one array per page. The
        // REST payload is snake_case and carries fields we ignore.
        let json = r#"[[
            {
                "id": 100,
                "node_id": "x",
                "path": "src/main.rs",
                "line": 42,
                "side": "RIGHT",
                "start_line": 40,
                "start_side": "RIGHT",
                "body": "top-level comment",
                "user": {"login": "alice", "id": 1},
                "created_at": "2026-07-01T12:00:00Z",
                "in_reply_to_id": null
            },
            {
                "id": 101,
                "path": "src/main.rs",
                "line": 42,
                "side": "RIGHT",
                "start_line": null,
                "body": "a reply",
                "user": {"login": "bob"},
                "created_at": "2026-07-02T08:30:00Z",
                "in_reply_to_id": 100
            }
        ], [
            {
                "id": 102,
                "path": "old.rs",
                "line": null,
                "side": null,
                "start_line": null,
                "body": "outdated",
                "user": {"login": "carol"},
                "created_at": "2026-06-01T00:00:00Z"
            }
        ]]"#;
        let pages: Vec<Vec<ReviewComment>> = serde_json::from_str(json).unwrap();
        let comments: Vec<ReviewComment> = pages.into_iter().flatten().collect();
        assert_eq!(comments.len(), 3);
        let top = &comments[0];
        assert_eq!(top.id, 100);
        assert_eq!(top.path, "src/main.rs");
        assert_eq!(top.line, Some(42));
        assert_eq!(top.side.as_deref(), Some("RIGHT"));
        assert_eq!(top.start_line, Some(40));
        assert_eq!(top.user.login, "alice");
        assert_eq!(top.in_reply_to_id, None);
        let reply = &comments[1];
        assert_eq!(reply.in_reply_to_id, Some(100));
        assert_eq!(reply.start_line, None);
        // Outdated: null line/side, and a missing in_reply_to_id key.
        let outdated = &comments[2];
        assert_eq!(outdated.line, None);
        assert_eq!(outdated.side, None);
        assert_eq!(outdated.in_reply_to_id, None);
    }

    #[test]
    fn deserializes_pr_list_json() {
        let json = r#"[
            {
                "number": 3468,
                "title": "printer: add --field-name-terminator flag",
                "author": {"id": "x", "is_bot": false, "login": "alice", "name": "Alice"},
                "state": "OPEN",
                "isDraft": false,
                "headRefName": "field-name-terminator",
                "updatedAt": "2026-07-01T12:34:56Z"
            },
            {
                "number": 3470,
                "title": "wip: experiment",
                "author": {"login": "bob"},
                "state": "OPEN",
                "isDraft": true,
                "headRefName": "bob/wip",
                "updatedAt": "2026-06-30T08:00:00Z"
            }
        ]"#;
        let prs: Vec<PrSummary> = serde_json::from_str(json).unwrap();
        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0].number, 3468);
        assert_eq!(prs[0].author.login, "alice");
        assert_eq!(prs[0].state, "OPEN");
        assert!(!prs[0].is_draft);
        assert_eq!(prs[0].head_ref_name, "field-name-terminator");
        assert_eq!(prs[0].updated_at, "2026-07-01T12:34:56Z");
        assert!(prs[1].is_draft);
    }
}
