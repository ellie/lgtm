//! Fetch PR data via the `gh` CLI, piggybacking on the user's `gh auth`.

use anyhow::{anyhow, bail, Context, Result};
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
    pub base_ref_name: String,
    pub head_ref_name: String,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
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
        "number,title,author,state,url,baseRefName,headRefName,additions,deletions,changedFiles",
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

pub fn fetch_patch(loc: &PrLocator) -> Result<String> {
    gh(&[
        "pr",
        "diff",
        &loc.number.to_string(),
        "--repo",
        &loc.repo_slug(),
    ])
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
