//! UI-free diff model: parse `git diff` / `gh pr diff` unified patches into a
//! structured model, with word-level intra-line highlights on modified runs.

use imara_diff::intern::InternedInput;
use imara_diff::Algorithm;
use std::ops::Range;

#[derive(Debug, Default)]
pub struct PrDiff {
    pub files: Vec<FileDiff>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    Binary,
}

#[derive(Debug)]
pub struct FileDiff {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub status: FileStatus,
    pub hunks: Vec<Hunk>,
    pub additions: u32,
    pub deletions: u32,
}

impl FileDiff {
    pub fn display_path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("<unknown>")
    }
}

#[derive(Debug)]
pub struct Hunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    /// Function/section context after the trailing `@@`, if any.
    pub section: String,
    pub rows: Vec<DiffRow>,
}

/// A row is only ever Context, Added, or Removed — "modified" exists only at
/// the hunk level, as removed rows followed by added rows.
#[derive(Debug)]
pub enum DiffRow {
    Context {
        old_no: u32,
        new_no: u32,
        text: String,
    },
    Added {
        new_no: u32,
        text: String,
        /// Byte ranges within `text` that differ from the paired removed line.
        intra: Vec<Range<usize>>,
    },
    Removed {
        old_no: u32,
        text: String,
        intra: Vec<Range<usize>>,
    },
}

pub fn parse_patch(patch: &str) -> PrDiff {
    let mut files = Vec::new();
    let mut lines = patch.lines().peekable();

    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("diff --git ") else {
            continue;
        };
        let (old_guess, new_guess) = parse_git_paths(rest);
        let mut file = FileDiff {
            old_path: old_guess,
            new_path: new_guess,
            status: FileStatus::Modified,
            hunks: Vec::new(),
            additions: 0,
            deletions: 0,
        };
        let mut is_rename = false;

        // Extended header lines, up to the first hunk or the next file.
        while let Some(next) = lines.peek() {
            if next.starts_with("diff --git ") || next.starts_with("@@ ") {
                break;
            }
            let next = lines.next().unwrap();
            if next.starts_with("new file mode") {
                file.status = FileStatus::Added;
            } else if next.starts_with("deleted file mode") {
                file.status = FileStatus::Deleted;
            } else if let Some(p) = next.strip_prefix("rename from ") {
                file.old_path = Some(p.to_string());
                is_rename = true;
            } else if let Some(p) = next.strip_prefix("rename to ") {
                file.new_path = Some(p.to_string());
                is_rename = true;
            } else if next.starts_with("Binary files ") || next == "GIT binary patch" {
                file.status = FileStatus::Binary;
            } else if let Some(p) = next.strip_prefix("--- ") {
                if let Some(p) = parse_marker_path(p) {
                    file.old_path = Some(p);
                }
            } else if let Some(p) = next.strip_prefix("+++ ") {
                if let Some(p) = parse_marker_path(p) {
                    file.new_path = Some(p);
                }
            }
            // index/mode/similarity lines are ignored.
        }
        if is_rename && file.status == FileStatus::Modified {
            file.status = FileStatus::Renamed;
        }
        if file.status == FileStatus::Added {
            file.old_path = None;
        }
        if file.status == FileStatus::Deleted {
            file.new_path = None;
        }

        while let Some(next) = lines.peek() {
            if !next.starts_with("@@ ") {
                break;
            }
            let header = lines.next().unwrap();
            let Some(mut hunk) = parse_hunk_header(header) else {
                break;
            };
            let mut old_no = hunk.old_start;
            let mut new_no = hunk.new_start;
            while let Some(body) = lines.peek() {
                if body.starts_with("diff --git ") || body.starts_with("@@ ") {
                    break;
                }
                let body = lines.next().unwrap();
                if let Some(text) = body.strip_prefix('+') {
                    hunk.rows.push(DiffRow::Added {
                        new_no,
                        text: text.to_string(),
                        intra: Vec::new(),
                    });
                    new_no += 1;
                    file.additions += 1;
                } else if let Some(text) = body.strip_prefix('-') {
                    hunk.rows.push(DiffRow::Removed {
                        old_no,
                        text: text.to_string(),
                        intra: Vec::new(),
                    });
                    old_no += 1;
                    file.deletions += 1;
                } else if body.starts_with('\\') {
                    // "\ No newline at end of file"
                } else {
                    // Context. Tolerate a bare empty line (some tools strip the
                    // leading space from blank context lines).
                    let text = body.strip_prefix(' ').unwrap_or(body);
                    hunk.rows.push(DiffRow::Context {
                        old_no,
                        new_no,
                        text: text.to_string(),
                    });
                    old_no += 1;
                    new_no += 1;
                }
            }
            compute_intra_line(&mut hunk.rows);
            file.hunks.push(hunk);
        }
        files.push(file);
    }

    PrDiff { files }
}

/// Authoritative diff of two complete file contents into hunks with `context`
/// lines of surrounding context, merging hunks whose context overlaps or
/// touches (git semantics). Line endings are normalized first: lines are
/// compared without their terminators (`str::lines`, which strips both `\n`
/// and `\r\n`), so a CRLF↔LF flip produces no hunks. Consequence, chosen and
/// documented: a difference only in the presence of a trailing newline is
/// invisible to this diff — both `"a\nb"` and `"a\nb\n"` are the lines
/// `["a", "b"]`. Row texts never include terminators.
pub fn diff_texts(old: &str, new: &str, context: u32) -> Vec<Hunk> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    let mut input = InternedInput::default();
    input.update_before(old_lines.iter().copied());
    input.update_after(new_lines.iter().copied());

    // Changed regions, as 0-based line ranges on each side.
    let mut changes: Vec<(Range<u32>, Range<u32>)> = Vec::new();
    imara_diff::diff(
        Algorithm::Histogram,
        &input,
        |before: Range<u32>, after: Range<u32>| changes.push((before, after)),
    );
    if changes.is_empty() {
        return Vec::new();
    }

    // Group changes into hunks: two changes share a hunk when the context
    // lines around them overlap or touch (gap between them ≤ 2 * context).
    let mut groups: Vec<Range<usize>> = Vec::new();
    let mut start = 0;
    for ix in 1..changes.len() {
        if changes[ix].0.start - changes[ix - 1].0.end > 2 * context {
            groups.push(start..ix);
            start = ix;
        }
    }
    groups.push(start..changes.len());

    let mut hunks = Vec::new();
    for group in groups {
        let first = &changes[group.start];
        let last = &changes[group.end - 1];
        // Context clamps at file boundaries.
        let old_lo = first.0.start.saturating_sub(context);
        let old_hi = (last.0.end + context).min(old_lines.len() as u32);
        let new_lo = first.1.start.saturating_sub(context);
        let new_hi = (last.1.end + context).min(new_lines.len() as u32);

        let mut rows = Vec::new();
        let (mut old_no, mut new_no) = (old_lo, new_lo); // 0-based cursors
        for (before, after) in &changes[group] {
            // Shared context up to this change (identical on both sides).
            while old_no < before.start {
                rows.push(DiffRow::Context {
                    old_no: old_no + 1,
                    new_no: new_no + 1,
                    text: old_lines[old_no as usize].to_string(),
                });
                old_no += 1;
                new_no += 1;
            }
            for no in before.clone() {
                rows.push(DiffRow::Removed {
                    old_no: no + 1,
                    text: old_lines[no as usize].to_string(),
                    intra: Vec::new(),
                });
            }
            for no in after.clone() {
                rows.push(DiffRow::Added {
                    new_no: no + 1,
                    text: new_lines[no as usize].to_string(),
                    intra: Vec::new(),
                });
            }
            old_no = before.end;
            new_no = after.end;
        }
        while old_no < old_hi {
            rows.push(DiffRow::Context {
                old_no: old_no + 1,
                new_no: new_no + 1,
                text: old_lines[old_no as usize].to_string(),
            });
            old_no += 1;
            new_no += 1;
        }
        compute_intra_line(&mut rows);

        let old_count = old_hi - old_lo;
        let new_count = new_hi - new_lo;
        hunks.push(Hunk {
            // Git convention: a zero-count side records the line *before* the
            // hunk (0 when at file start), otherwise the 1-based first line.
            old_start: if old_count == 0 { old_lo } else { old_lo + 1 },
            old_count,
            new_start: if new_count == 0 { new_lo } else { new_lo + 1 },
            new_count,
            section: String::new(),
            rows,
        });
    }
    hunks
}

/// `a/old b/new`, possibly with `"`-quoted paths. Best-effort: the definitive
/// paths come from `---`/`+++`/`rename` lines when present.
fn parse_git_paths(rest: &str) -> (Option<String>, Option<String>) {
    if rest.starts_with('"') {
        let mut parts = Vec::new();
        let mut chars = rest.char_indices();
        while let Some((start, ch)) = chars.next() {
            if ch != '"' {
                continue;
            }
            for (end, ch) in chars.by_ref() {
                if ch == '"' {
                    parts.push(&rest[start + 1..end]);
                    break;
                }
            }
        }
        if parts.len() == 2 {
            return (strip_side(parts[0]), strip_side(parts[1]));
        }
    }
    if let Some(pos) = rest.find(" b/") {
        let old = strip_side(&rest[..pos]);
        let new = strip_side(&rest[pos + 1..]);
        return (old, new);
    }
    // Git 2.45+ switched the default `diff --git` prefix from `a/`/`b/` to
    // `c/`/`w/` (commit/worktree) to avoid colliding with tracked paths that
    // happen to begin with `a/`. `git config diff.mnemonicPrefix` can force
    // the old form, but we accept both.
    if let Some(pos) = rest.find(" w/") {
        let old = strip_side(&rest[..pos]);
        let new = strip_side(&rest[pos + 1..]);
        return (old, new);
    }
    // `git diff --no-index` produces `1/path 2/path` (no `a/`/`b/` prefix).
    // Split on the first space between two `<digit>/…` tokens.
    let (old, new) = parse_noindex_paths(rest);
    if old.is_some() || new.is_some() {
        return (old, new);
    }
    (None, None)
}

/// Split `1/foo 2/foo` (or `1/foo 2/foo/bar`) into `(foo, foo)` /
/// `(foo, foo/bar)`. Both halves must start with `<single-digit>/` for this
/// form to apply — otherwise a path containing "1/" or "2/" mid-string could
/// match by accident.
fn parse_noindex_paths(rest: &str) -> (Option<String>, Option<String>) {
    let mut split = rest.splitn(2, ' ');
    let (Some(old), Some(new)) = (split.next(), split.next()) else {
        return (None, None);
    };
    if !looks_like_noindex_side(old) || !looks_like_noindex_side(new) {
        return (None, None);
    }
    (strip_side(old), strip_side(new))
}

fn looks_like_noindex_side(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_digit())
        && chars.next() == Some('/')
}

fn strip_side(path: &str) -> Option<String> {
    if looks_like_noindex_side(path) {
        // `git diff --no-index` prepends `1/` or `2/`, not `a/`/`b/`.
        return path.split_once('/').map(|(_, rest)| rest.to_string());
    }
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        // Git 2.45+ default prefixes: `c/` (commit) and `w/` (worktree).
        .or_else(|| path.strip_prefix("c/"))
        .or_else(|| path.strip_prefix("w/"))
        .map(str::to_string)
}

/// Path from a `---`/`+++` marker: `a/path`, `b/path`, `1/path` / `2/path`
/// (the latter only when `git diff --no-index` produced the patch), `/dev/null`,
/// maybe quoted.
fn parse_marker_path(p: &str) -> Option<String> {
    let p = p.trim_end();
    let p = p.strip_prefix('"').unwrap_or(p);
    let p = p.strip_suffix('"').unwrap_or(p);
    if p == "/dev/null" {
        return None;
    }
    strip_side(p).or_else(|| Some(p.to_string()))
}

/// `@@ -old_start[,old_count] +new_start[,new_count] @@[ section]`
fn parse_hunk_header(line: &str) -> Option<Hunk> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, rest) = rest.split_once(" @@")?;
    let parse_pair = |s: &str| -> Option<(u32, u32)> {
        match s.split_once(',') {
            Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let (old_start, old_count) = parse_pair(old)?;
    let (new_start, new_count) = parse_pair(new)?;
    Some(Hunk {
        old_start,
        old_count,
        new_start,
        new_count,
        section: rest.trim_start().to_string(),
        rows: Vec::new(),
    })
}

// --- Intra-line (word-level) diff -------------------------------------------

const MAX_PAIR_RUN: usize = 32;
const MAX_LINE_BYTES: usize = 4096;
const MAX_LINE_TOKENS: usize = 512;
/// If more than this fraction of a line changed, highlighting it all is noise.
const MAX_CHANGED_FRACTION: f32 = 0.7;

/// For each run of removed lines immediately followed by an equal number of
/// added lines, pair the lines positionally and compute word-level diffs.
/// (Equal-count positional pairing is the cheap, high-confidence case;
/// similarity-based pairing for unequal runs can come later.)
fn compute_intra_line(rows: &mut [DiffRow]) {
    let mut i = 0;
    while i < rows.len() {
        if !matches!(rows[i], DiffRow::Removed { .. }) {
            i += 1;
            continue;
        }
        let start = i;
        while i < rows.len() && matches!(rows[i], DiffRow::Removed { .. }) {
            i += 1;
        }
        let mid = i;
        while i < rows.len() && matches!(rows[i], DiffRow::Added { .. }) {
            i += 1;
        }
        let removed = mid - start;
        let added = i - mid;
        if removed != added || removed > MAX_PAIR_RUN {
            continue;
        }
        for pair in 0..removed {
            let old_text = match &rows[start + pair] {
                DiffRow::Removed { text, .. } => text.clone(),
                _ => unreachable!(),
            };
            let new_text = match &rows[mid + pair] {
                DiffRow::Added { text, .. } => text.clone(),
                _ => unreachable!(),
            };
            if old_text.len() > MAX_LINE_BYTES || new_text.len() > MAX_LINE_BYTES {
                continue;
            }
            let (old_ranges, new_ranges) = word_diff(&old_text, &new_text);
            if let DiffRow::Removed { intra, .. } = &mut rows[start + pair] {
                *intra = old_ranges;
            }
            if let DiffRow::Added { intra, .. } = &mut rows[mid + pair] {
                *intra = new_ranges;
            }
        }
    }
}

/// Byte ranges of tokens: identifier/number runs, whitespace runs, or single
/// punctuation chars.
fn token_ranges(s: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut chars = s.char_indices().peekable();
    while let Some((start, ch)) = chars.next() {
        let class = char_class(ch);
        let mut end = start + ch.len_utf8();
        if class != CharClass::Punct {
            while let Some(&(next_ix, next_ch)) = chars.peek() {
                if char_class(next_ch) != class {
                    break;
                }
                end = next_ix + next_ch.len_utf8();
                chars.next();
            }
        }
        out.push(start..end);
    }
    out
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum CharClass {
    Word,
    Space,
    Punct,
}

fn char_class(ch: char) -> CharClass {
    if ch.is_alphanumeric() || ch == '_' {
        CharClass::Word
    } else if ch.is_whitespace() {
        CharClass::Space
    } else {
        CharClass::Punct
    }
}

/// Word-level diff of two lines → (changed byte ranges in old, in new).
/// Returns empty ranges when the lines are too different for highlights to help.
fn word_diff(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let old_tokens = token_ranges(old);
    let new_tokens = token_ranges(new);
    if old_tokens.len() > MAX_LINE_TOKENS || new_tokens.len() > MAX_LINE_TOKENS {
        return (Vec::new(), Vec::new());
    }

    let mut input = InternedInput::default();
    input.update_before(old_tokens.iter().map(|r| &old[r.clone()]));
    input.update_after(new_tokens.iter().map(|r| &new[r.clone()]));

    let mut old_ranges: Vec<Range<usize>> = Vec::new();
    let mut new_ranges: Vec<Range<usize>> = Vec::new();
    imara_diff::diff(
        Algorithm::Histogram,
        &input,
        |before: Range<u32>, after: Range<u32>| {
            if before.start < before.end {
                let start = old_tokens[before.start as usize].start;
                let end = old_tokens[before.end as usize - 1].end;
                old_ranges.push(start..end);
            }
            if after.start < after.end {
                let start = new_tokens[after.start as usize].start;
                let end = new_tokens[after.end as usize - 1].end;
                new_ranges.push(start..end);
            }
        },
    );

    let changed = |ranges: &[Range<usize>], len: usize| {
        len > 0 && ranges.iter().map(|r| r.len()).sum::<usize>() as f32 / len as f32
            > MAX_CHANGED_FRACTION
    };
    if changed(&old_ranges, old.trim_end().len()) || changed(&new_ranges, new.trim_end().len()) {
        return (Vec::new(), Vec::new());
    }
    (old_ranges, new_ranges)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,4 +1,5 @@ fn main()
 use std::io;
-let foo = 1;
+let bar = 1;
+let baz = 2;
 println!();
@@ -10,2 +11,2 @@
 // tail
-old line
+new line
diff --git a/README.md b/README.md
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/README.md
@@ -0,0 +1,1 @@
+# hello
\\ No newline at end of file
diff --git a/logo.png b/logo.png
index 4444444..5555555 100644
Binary files a/logo.png and b/logo.png differ
diff --git a/old.rs b/renamed.rs
similarity index 90%
rename from old.rs
rename to renamed.rs
index 6666666..7777777 100644
--- a/old.rs
+++ b/renamed.rs
@@ -1,1 +1,1 @@
-x
+y
";

    #[test]
    fn parses_mnemonic_prefix_diff_for_git_245_plus() {
        // Git 2.45+ default prefixes: `c/` (commit side) and `w/`
        // (worktree side). The path must come out bare, no `c/`/`w/` prefix.
        let sample = "\
diff --git c/src/main.rs w/src/main.rs
index 1111111..2222222 100644
--- c/src/main.rs
+++ w/src/main.rs
@@ -1,1 +1,1 @@
-let foo = 1;
+let bar = 1;
";
        let diff = parse_patch(sample);
        assert_eq!(diff.files.len(), 1);
        let f = &diff.files[0];
        assert_eq!(f.display_path(), "src/main.rs");
        assert_eq!(f.old_path.as_deref(), Some("src/main.rs"));
        assert_eq!(f.new_path.as_deref(), Some("src/main.rs"));
        assert_eq!(f.status, FileStatus::Modified);
    }

    #[test]
    fn parses_noindex_diff_for_untracked_files() {
        // git diff --no-index -- /dev/null new.txt emits this header form.
        let noindex = "\
diff --git 1/new.txt 2/new.txt
new file mode 100644
index 0000000..ce01362
--- /dev/null
+++ 2/new.txt
@@ -0,0 +1,1 @@
+hello
";
        let diff = parse_patch(noindex);
        assert_eq!(diff.files.len(), 1);
        let f = &diff.files[0];
        // Header's `1/` and `2/` prefixes must be stripped, not stored.
        assert_eq!(f.display_path(), "new.txt");
        assert_eq!(f.old_path, None);
        assert_eq!(f.new_path.as_deref(), Some("new.txt"));
        assert_eq!(f.status, FileStatus::Added);
        assert_eq!((f.additions, f.deletions), (1, 0));
    }

    #[test]
    fn parses_files_hunks_and_rows() {
        let diff = parse_patch(SAMPLE);
        assert_eq!(diff.files.len(), 4);

        let main = &diff.files[0];
        assert_eq!(main.display_path(), "src/main.rs");
        assert_eq!(main.status, FileStatus::Modified);
        assert_eq!(main.hunks.len(), 2);
        assert_eq!((main.additions, main.deletions), (3, 2));
        assert_eq!(main.hunks[0].section, "fn main()");
        assert_eq!(main.hunks[0].rows.len(), 5);
        match &main.hunks[0].rows[1] {
            DiffRow::Removed { old_no, text, .. } => {
                assert_eq!(*old_no, 2);
                assert_eq!(text, "let foo = 1;");
            }
            other => panic!("expected removed row, got {other:?}"),
        }
        match &main.hunks[0].rows[4] {
            DiffRow::Context { old_no, new_no, .. } => assert_eq!((*old_no, *new_no), (3, 4)),
            other => panic!("expected context row, got {other:?}"),
        }

        let readme = &diff.files[1];
        assert_eq!(readme.status, FileStatus::Added);
        assert_eq!(readme.old_path, None);
        assert_eq!(readme.display_path(), "README.md");

        let logo = &diff.files[2];
        assert_eq!(logo.status, FileStatus::Binary);
        assert_eq!(logo.display_path(), "logo.png");
        assert!(logo.hunks.is_empty());

        let renamed = &diff.files[3];
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.old_path.as_deref(), Some("old.rs"));
        assert_eq!(renamed.new_path.as_deref(), Some("renamed.rs"));
    }

    #[test]
    fn word_diff_highlights_changed_identifier() {
        let (old_ranges, new_ranges) = word_diff("let foo = 1;", "let bar = 1;");
        assert_eq!(old_ranges, vec![4..7]);
        assert_eq!(new_ranges, vec![4..7]);
    }

    #[test]
    fn word_diff_skips_total_rewrites() {
        let (old_ranges, new_ranges) =
            word_diff("completely different content", "nothing shared at all!");
        assert!(old_ranges.is_empty());
        assert!(new_ranges.is_empty());
    }

    #[test]
    fn intra_line_set_on_paired_run() {
        let diff = parse_patch(SAMPLE);
        match &diff.files[0].hunks[1].rows[1] {
            DiffRow::Removed { intra, .. } => assert_eq!(intra, &vec![0..3]),
            other => panic!("expected removed row, got {other:?}"),
        }
        match &diff.files[0].hunks[1].rows[2] {
            DiffRow::Added { intra, .. } => assert_eq!(intra, &vec![0..3]),
            other => panic!("expected added row, got {other:?}"),
        }
    }

    fn lines(n: Range<u32>) -> String {
        n.map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn diff_texts_identical_produces_no_hunks() {
        let text = lines(1..20);
        assert!(diff_texts(&text, &text, 3).is_empty());
        assert!(diff_texts("", "", 3).is_empty());
    }

    #[test]
    fn diff_texts_single_change_mid_file() {
        let old = lines(1..21);
        let new = old.replace("line 10\n", "line ten\n");
        let hunks = diff_texts(&old, &new, 3);
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!((h.old_start, h.old_count, h.new_start, h.new_count), (7, 7, 7, 7));
        assert_eq!(h.rows.len(), 8); // 3 ctx + 1 removed + 1 added + 3 ctx
        match &h.rows[0] {
            DiffRow::Context { old_no, new_no, text } => {
                assert_eq!((*old_no, *new_no), (7, 7));
                assert_eq!(text, "line 7");
            }
            other => panic!("expected context, got {other:?}"),
        }
        match &h.rows[3] {
            DiffRow::Removed { old_no, text, intra } => {
                assert_eq!(*old_no, 10);
                assert_eq!(text, "line 10");
                // Intra-line pass ran on the paired change.
                assert!(!intra.is_empty());
            }
            other => panic!("expected removed, got {other:?}"),
        }
        match &h.rows[4] {
            DiffRow::Added { new_no, text, .. } => {
                assert_eq!(*new_no, 10);
                assert_eq!(text, "line ten");
            }
            other => panic!("expected added, got {other:?}"),
        }
    }

    #[test]
    fn diff_texts_merges_close_changes_and_splits_far_ones() {
        let old = lines(1..30);
        // Changes at lines 10 and 16: gap of 5 unchanged lines ≤ 2*3 → merge.
        let new = old
            .replace("line 10\n", "line X\n")
            .replace("line 16\n", "line Y\n");
        let hunks = diff_texts(&old, &new, 3);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 7);
        assert_eq!(hunks[0].old_count, 13); // lines 7..=19

        // Changes at lines 5 and 20: gap of 14 > 6 → two hunks.
        let new = old
            .replace("line 5\n", "line X\n")
            .replace("line 20\n", "line Y\n");
        let hunks = diff_texts(&old, &new, 3);
        assert_eq!(hunks.len(), 2);
        assert_eq!((hunks[0].old_start, hunks[0].old_count), (2, 7));
        assert_eq!((hunks[1].old_start, hunks[1].old_count), (17, 7));
    }

    #[test]
    fn diff_texts_clamps_context_at_file_edges() {
        let old = lines(1..10);
        let new = old.replace("line 1\n", "line one\n");
        let hunks = diff_texts(&old, &new, 3);
        assert_eq!(hunks.len(), 1);
        assert_eq!((hunks[0].old_start, hunks[0].old_count), (1, 4)); // 1 change + 3 trailing ctx

        let new = old.replace("line 9\n", "line nine\n");
        let hunks = diff_texts(&old, &new, 3);
        assert_eq!(hunks.len(), 1);
        assert_eq!((hunks[0].old_start, hunks[0].old_count), (6, 4)); // 3 leading ctx + 1 change
    }

    #[test]
    fn diff_texts_crlf_produces_no_phantom_hunks() {
        let old = "alpha\r\nbeta\r\ngamma\r\n";
        let new = "alpha\nbeta\ngamma\n";
        assert!(diff_texts(old, new, 3).is_empty());
        // Mixed endings plus a real change: exactly the real change, no \r in texts.
        let new = "alpha\nbeta!\ngamma\n";
        let hunks = diff_texts(old, new, 3);
        assert_eq!(hunks.len(), 1);
        for row in &hunks[0].rows {
            let text = match row {
                DiffRow::Context { text, .. }
                | DiffRow::Added { text, .. }
                | DiffRow::Removed { text, .. } => text,
            };
            assert!(!text.contains('\r'), "phantom CR in {text:?}");
        }
    }

    #[test]
    fn diff_texts_trailing_newline_semantics() {
        // Documented choice: trailing-newline-only differences are invisible.
        assert!(diff_texts("a\nb\n", "a\nb", 3).is_empty());
        // A real change in a file without a trailing newline doesn't panic and
        // still includes the last line.
        let hunks = diff_texts("a\nb\nc", "a\nB\nc", 3);
        assert_eq!(hunks.len(), 1);
        assert_eq!((hunks[0].old_start, hunks[0].old_count), (1, 3));
        match hunks[0].rows.last().unwrap() {
            DiffRow::Context { old_no, new_no, text } => {
                assert_eq!((*old_no, *new_no), (3, 3));
                assert_eq!(text, "c");
            }
            other => panic!("expected context, got {other:?}"),
        }
    }

    #[test]
    fn diff_texts_added_and_deleted_files() {
        // Old side empty (added file): zero-count side records line 0.
        let hunks = diff_texts("", "a\nb\n", 3);
        assert_eq!(hunks.len(), 1);
        assert_eq!(
            (hunks[0].old_start, hunks[0].old_count, hunks[0].new_start, hunks[0].new_count),
            (0, 0, 1, 2)
        );
        assert!(hunks[0].rows.iter().all(|r| matches!(r, DiffRow::Added { .. })));
        // Deleted file mirrors it.
        let hunks = diff_texts("a\nb\n", "", 3);
        assert_eq!(
            (hunks[0].old_start, hunks[0].old_count, hunks[0].new_start, hunks[0].new_count),
            (1, 2, 0, 0)
        );
    }

    #[test]
    fn hunk_header_without_counts() {
        let hunk = parse_hunk_header("@@ -5 +7 @@").unwrap();
        assert_eq!(
            (hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count),
            (5, 1, 7, 1)
        );
    }
}
