//! Local git diffs via the `git` CLI: "the PR I'd open from here" — everything
//! since the merge-base with the default branch (committed + staged + unstaged
//! + untracked), as one unified patch for diff-core to parse.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// How many untracked files to inline into the patch before giving up.
const MAX_UNTRACKED_FILES: usize = 200;

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

    let untracked = git(&src.repo_root, &["ls-files", "--others", "--exclude-standard"])?;
    for file in untracked.lines().take(MAX_UNTRACKED_FILES) {
        // `--no-index` against /dev/null renders an untracked file as an
        // added-file diff; it exits 1 when the sides differ, which is success
        // here (0 would mean an empty file — also fine, git emits a header).
        let output = Command::new("git")
            .arg("-C")
            .arg(&src.repo_root)
            .args(["diff", "--no-color", "--no-ext-diff", "--no-index", "--", "/dev/null"])
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
        assert_eq!(src.repo_root.canonicalize().unwrap(), dir.canonicalize().unwrap());
        assert_eq!(src.branch, "main");
        assert_eq!(src.base_label, "HEAD");

        let patch = diff_patch(&src).unwrap();
        let diff = diff_core::parse_patch(&patch);
        let by_path: Vec<(&str, FileStatus)> = diff
            .files
            .iter()
            .map(|f| (f.display_path(), f.status))
            .collect();
        assert!(by_path.contains(&("a.rs", FileStatus::Modified)), "{by_path:?}");
        assert!(by_path.contains(&("new.txt", FileStatus::Added)), "{by_path:?}");
        assert!(by_path.contains(&("blob.bin", FileStatus::Binary)), "{by_path:?}");
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
            &["git", "clone", upstream.to_str().unwrap(), clone.to_str().unwrap()],
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
        assert_eq!(file_at_base(&src, "a.rs").as_deref(), Some("fn main() {}\n"));
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
            &["git", "clone", upstream.to_str().unwrap(), clone.to_str().unwrap()],
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
        assert_eq!(file_at_base(&src, "lib.rs").as_deref(), Some("pub fn one() {}\n"));
    }

    #[test]
    fn resolve_rejects_non_repo() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(resolve_local(tmp.path()).is_err());
    }
}
