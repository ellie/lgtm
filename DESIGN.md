# review — a fast, native code review app

A desktop app for reviewing code, built in Rust on [gpui](https://www.gpui.rs/)
(Zed's GPU-accelerated UI framework). The core bet: diff rendering that is
*better and faster than anything else*, with GitHub PRs loading in one command
via the `gh` CLI. AI review, LSP, and commenting come later — the design below
leaves seams for them without building abstractions early.

## Goals

1. Render text diffs insanely well: syntax highlighting, word-level intra-line
   highlights, unified + split views, buttery scrolling on 10k-line PRs.
2. `review owner/repo#123` (or paste a URL in-app) → diff on screen in well
   under a second after `gh` responds.
3. Stay simple: one purpose-built viewer, not a general editor.

Non-goals for v1: writing review comments, editing code, non-GitHub forges,
soft-wrapping diff lines (horizontal scroll instead — it keeps row heights
uniform, which is what makes rendering fast).

## Stack

| Concern | Choice | Why |
|---|---|---|
| UI | `gpui` (crates.io, 0.2.x) | GPU-accelerated, retained text system, `uniform_list` virtualization. Pre-1.0: pin the exact version; expect breakage on upgrade. Fall back to a git dep on `zed-industries/zed` if we need something newer. |
| Line diff | `imara-diff` | Git's histogram algorithm; fastest Rust diff library by a wide margin, produces the "human" diffs git users expect. |
| Intra-line diff | `imara-diff` (Myers) over tokens | Word-level highlights within changed line pairs. |
| Syntax highlighting | `tree-sitter` + `tree-sitter-highlight` + per-language grammars | Fast, accurate, same ecosystem Zed uses. Grammar set behind cargo features (start: rust, ts/tsx, js, python, go, json, toml, yaml, markdown, c, c++). |
| GitHub | `gh` CLI subprocess | Zero auth code — piggyback on the user's `gh auth`. JSON via `--json` / `gh api`. |
| JSON | `serde` / `serde_json` | |
| CLI | `clap` (derive, minimal) | `review <pr-ref>` |

## Workspace layout

```
crates/
  diff-core/    # no UI deps: diff model, patch parser, intra-line diff, highlighting
  gh/           # gh CLI wrapper: PR metadata, patch, blob fetch, disk cache
  git/          # git CLI wrapper: local repo diffs (branch vs default remote head)
  app/          # gpui binary: views, theme, keymap
```

`diff-core`, `gh`, and `git` stay UI-free so the diff pipeline is unit-testable
and benchable with `cargo bench` against real PR corpora. No traits between the
crates — `app` calls concrete functions.

## Data model (`diff-core`)

```rust
pub struct PrDiff {
    pub files: Vec<FileDiff>,
}

pub struct FileDiff {
    pub old_path: Option<String>,   // None = added
    pub new_path: Option<String>,   // None = deleted; both Some + different = rename
    pub status: FileStatus,         // Added | Deleted | Modified | Renamed | Binary
    pub language: Option<Language>,
    pub hunks: Vec<Hunk>,
    pub stats: (u32, u32),          // additions, deletions
}

pub struct Hunk {
    pub old_range: Range<u32>,
    pub new_range: Range<u32>,
    pub rows: Vec<DiffRow>,
}

pub enum DiffRow {
    Context { old_no: u32, new_no: u32, text: LineText },
    Added   { new_no: u32, text: LineText, intra: Vec<Range<usize>> },
    Removed { old_no: u32, text: LineText, intra: Vec<Range<usize>> },
    // Split view pairs Removed/Added rows; unified view emits them in order.
}

pub struct LineText {
    pub text: SharedString,             // gpui-friendly, cheap to clone
    pub highlights: Vec<(Range<usize>, HighlightTag)>,  // from tree-sitter
}
```

Every row carries `(path, side, line_number)` — that triple is the stable
anchor that AI comments, LSP diagnostics, and human review threads will attach
to later. Rendering already consults a `Vec<Decoration>` on the view state
(empty in v1), so overlays land without reshaping the pipeline.

## Loading a PR: two-phase for speed

**Phase 1 — instant paint from the patch.** One call:

```
gh pr diff 123 --repo owner/repo          # unified patch, all files
gh pr view 123 --repo owner/repo --json number,title,author,state,url,\
    baseRefName,headRefName,baseRefOid,headRefOid,additions,deletions,changedFiles
```

Both run concurrently in background tasks. We parse the unified patch directly
into the `FileDiff` model (the patch already contains every changed line plus
3 lines of context). Syntax highlighting runs on the hunk text standalone —
slightly degraded (no cross-hunk parser state) but visually fine. This is what
gets the diff on screen fast: no per-file API calls, no blob downloads.

**Phase 2 — full-context upgrade, per file, lazily.** When a file is selected
(plus prefetch of the next few files in the tree), fetch both blobs:

```
gh api repos/owner/repo/git/blobs/<sha>       # base64 content
```

(file→blob SHAs come from `gh api repos/owner/repo/pulls/123/files --paginate`,
fetched once in the background). With full old/new contents we:

- normalize line endings (CRLF→LF) on both sides, then re-diff with imara-diff
  histogram (interned lines) — authoritative hunks, and no wall-of-changes when
  a file merely flipped line endings (Zed does the same normalization),
- re-highlight with tree-sitter over the *whole file* — correct parser state,
- enable **expand context** (click a hunk separator to reveal hidden lines,
  or reveal the whole file).

The upgraded `FileDiff` atomically replaces the patch-derived one; row keys are
line numbers so scroll position is preserved.

**Blob cache**: `~/.cache/review/blobs/<sha>` — content-addressed, immutable,
never invalidated. Re-opening a PR you've reviewed is fully offline for
contents. PR metadata is not cached (always fresh).

All subprocess work runs on `cx.background_executor()`; blocking
`std::process::Command` in a background task is fine and keeps it simple.

## Intra-line (word-level) diff

For each maximal run of Removed lines followed by Added lines within a hunk:

1. Pair lines up: equal counts pair positionally; unequal counts pair by
   best similarity (normalized token overlap), leaving the rest unpaired.
2. Tokenize each paired line with `unicode-segmentation` word bounds
   (identifiers/numbers as units, each punctuation char its own token,
   whitespace preserved as tokens).
3. Myers diff over the token streams → changed byte ranges on each side.
4. Guardrails: skip pairs with similarity < 0.3 (highlighting everything is
   noise) or > 1000 tokens; cap total intra-line work per file.

Rendered as a stronger background tint inside the row's add/remove tint —
the GitHub/Zed convention people already read fluently.

## Rendering (the part that has to be excellent)

**One `uniform_list`, fixed row height.** The whole PR — file headers, hunk
headers, code rows — is flattened into one `Vec<DisplayRow>`. Monospace font,
no soft wrap, so every row is exactly one line-height tall. `uniform_list`
gives perfect virtualization: only visible rows are laid out and painted,
scroll cost is independent of PR size.

```rust
enum DisplayRow {
    FileHeader { file_ix: usize },        // sticky at top while file is in view
    HunkHeader { file_ix: usize, hunk_ix: usize },   // also the "expand context" affordance
    Line(LineRowData),                    // the common case
}
```

**Split view is the same list.** Instead of two synchronized scrollables
(a permanent source of jank and bugs), split view emits *paired* rows — each
`DisplayRow::Line` holds an optional left cell and optional right cell, and the
row's element paints two half-width columns with a center divider. One list,
one scrollbar, sync is free by construction.

**Row status invariant**: a *row* is only ever Context, Added, or Removed —
"modified" exists only at the hunk level (a modified hunk renders as removed
rows followed by added rows). Zed enforces the same invariant with a
`debug_panic!`; it keeps row painting a dumb three-way match.

**Per-row painting.** Each visible row paints, in order:

1. Background quad — full-width add/remove tint (GPU quads, ~free). Runs of
   adjacent same-tint rows coalesce into a single quad before painting.
2. Intra-line quads — stronger tint behind changed byte ranges.
3. Gutter — old/new line numbers + `+`/`-` marker, right-aligned, dim.
4. Text — one shaped line via gpui's text system, with tree-sitter highlight
   ranges mapped to theme colors as `TextRun`s.

gpui's `LineLayoutCache` already memoizes shaping across frames (Zed's editor
keeps no line cache of its own and just re-requests visible lines each frame).
Start by leaning on that; add our own persistent LRU only if profiling shows
reshaping in steady-state scroll.

**Minimap** (supersedes the earlier scrollbar-markers plan): a narrow column
(~100px) beside the diff showing the whole item zoomed out — one thin bar per
row (or per N rows when downsampled), width proportional to line length,
colored by row kind (add green / remove red / context dim; header ticks in
accent), mirrored half-columns in split mode. Painted as coalesced GPU quads
via a canvas element — no text rendering, so it's cheap at any size. A
viewport rectangle tracks the scroll position; click/drag scrolls. Rows are
static per diff load, so the quad list is computed once, not per frame.

**Start with `uniform_list` of styled elements; keep a trapdoor.** If
per-row element overhead ever shows in profiles, `DiffView` drops to a single
custom `Element` that paints the visible row range directly (quads + shaped
lines, no per-row element tree). The row model doesn't change — only the paint
strategy. Don't build this until profiling demands it.

**Long lines**: horizontal scroll shared by the whole pane; shape once at full
width (cap shaping at ~10k chars/line, tail rendered plain).

**Big-diff guardrails**: files with >20k changed lines or >5 MB render without
tree-sitter/intra-line (plain tinted text — still fast); binary files render a
stat row; generated/vendored files (linguist heuristics later) start collapsed.

**Budgets**: first paint < 300 ms after `gh pr diff` returns on a 100-file PR;
scrolling at display refresh rate with zero shaping in steady state; file
switch < 50 ms from cache.

## App structure (gpui)

The app is a small workspace: many review items open at once, one visible.

```
ReviewApp (root view)
├── TitleBar        — active item: PR title/#/author/branches/state +
│                     open-in-browser, or local repo dir/branch ← base;
│                     app name only while loading / with no items;
│                     subtle "reloading…" note while a refresh is in flight
├── Sidebar (cmd-b) — ~260px left panel: open-input at the top (cmd-t focuses
│                     it from anywhere), then one entry per item — status dot
│                     (PR open/merged/closed colors, blue for local),
│                     owner/repo#N or branch, title/dir secondary line,
│                     +N −M when loaded, ✕ to close (cmd-w closes active),
│                     ctrl-tab / ctrl-shift-tab cycle items
└── DiffView        — the uniform_list described above, per-item
```

State: `ReviewApp` owns `Vec<ReviewItem>` + the active index. Each item is a
source (`Pr(gh::PrLocator)` or `Local(git::LocalSource)` — a plain enum, no
provider traits) plus a state machine: `Loading | Ready(ItemData) |
Failed(msg)`. `ItemData` holds everything the pane needs for one diff — meta,
`PrDiff`, built display rows, cursor, view mode, and its own scroll handle, so
per-item scroll position/mode survive switching. Fetching runs through
`cx.spawn(async |this, cx| …)` with the blocking subprocess work and
tree-sitter row building inside `cx.background_spawn(…)`; completion swaps
Loading→Ready (or Failed with the gh/git stderr) via `this.update` +
`cx.notify()`. `r` refreshes the active item in place — old data stays visible
and scroll/cursor/mode are preserved when the new diff lands.

**Keymap (v1)**: `j/k` line, `n/p` next/prev hunk, `]/[` next/prev file,
`v` unified↔split, `x` expand context, `cmd-t` open input, `cmd-b` sidebar,
`cmd-w` close item, `ctrl-tab` cycle items, `r` refresh, `cmd-c` copy,
`o` open file@line on GitHub, `-/=` font size.

## Highlighting & theming (it has to be pretty)

**Engine**: `tree-sitter` + `tree-sitter-highlight`, driven by **Helix's
highlight query files**, vendored per language (MPL-2.0, attribution kept).
Query quality is what separates flat two-color highlighting from rich, precise
coloring — Helix's queries are among the best-maintained anywhere and emit
*scoped* capture names (`keyword.control.flow`, `function.builtin`,
`string.special.url`, `punctuation.delimiter`, …), which is what lets a theme
be subtle instead of shouty.

**Theme model**: a plain `Theme` struct with three groups —

- `syntax`: map of capture scope → style (color, italic, bold). Lookup is
  longest-dotted-prefix: `keyword.control.flow` falls back to
  `keyword.control`, then `keyword`. This is exactly Helix's resolution rule,
  which means…
- `ui`: surfaces, borders, file tree, headers, gutter, selection, scrollbar.
- `diff`: add/remove/modify hues + word-level emphasis variants.

**Palettes**: because our scopes and resolution match Helix, we can **load any
Helix theme TOML** (`~/.config/review/themes/*.toml` + vendored built-ins) —
that's hundreds of well-crafted palettes for free, including their
`diff.plus/minus/delta` colors so diffs look native in every theme. Built-ins
shipped in the binary: **Catppuccin Mocha** (default dark), **Catppuccin
Latte** (default light), Tokyo Night, Gruvbox, Rosé Pine. Follow the OS
dark/light preference by default.

**Diff tint math** (the detail that makes or breaks legibility): row
backgrounds are the theme's diff hue blended toward the editor background at
low strength (~12–15%), word-level ranges at roughly double that, so syntax
colors always stay readable *on top of* the tint. If a theme omits diff
colors, derive them: green/red nudged toward the background's luminance. Never
paint opaque red/green rows — that's how diffs get ugly.

**Typography**: gpui's text system is Zed's renderer, so glyph quality is
already excellent. Defaults: platform mono font (configurable, ligatures on
when the font has them), line height ~1.4 for reading (denser than an editor's
default is wrong for review — you read diffs more than you write them),
slightly dimmed line numbers, intra-line highlight rects with small corner
radii, hunk headers as quiet full-width separators rather than loud bars.

## CLI entry

```
review owner/repo#123 …    # any number of sources; each opens as an item
review 123                 # repo from cwd origin remote
review <github pr url>
review . ./sub /abs/path   # any existing directory → local diff of that repo
review                     # cwd in a git repo → local diff of it;
                           # otherwise open empty ("⌘T to open a PR or path")
```

The window opens immediately; every source loads asynchronously as a sidebar
item. Local diffs show the branch's divergence from the default remote head
(merge-base two-dot diff + staged + unstaged + untracked — the PR you'd open),
falling back to a working-tree-only diff when there's no remote. Errors
surface in-app with the `gh`/`git` stderr attached ("gh not found", "not
authenticated — run `gh auth login`", 404, rate limit).

## Future seams (not built now, not blocked either)

Everything below anchors at `(path, side, line)` and renders through the
decoration list — the pipeline doesn't reshape for any of it.

- **AI review & Q&A**: consumes `PrDiff` + blob cache (both UI-free, already
  loaded); emits `Decoration`s. Inline findings render as annotation rows
  beneath a line (a `DisplayRow` variant — annotations get their own rows, so
  uniform height is preserved). A chat/ask panel is another gpui view beside
  `DiffView` sharing `ReviewState`; "ask about this selection" ships the
  selected anchors + text as context.
- **LSP**: requires real files, not blobs — switch the head side to a cached
  `git fetch` + worktree per head SHA (base side can stay blob-based).
  Diagnostics and hover become decorations/popovers (popovers are floating
  gpui overlays anchored to row bounds — any height, list unaffected).
  Go-to-definition can land outside the diff: a plain file is a degenerate
  diff (all Context rows, zero hunks), so the existing row renderer doubles
  as the file viewer for free. Expect degraded servers for languages that
  need installed deps; that's inherent to remote review.
- **Comments/threads**: same anchor triple; GitHub review API via `gh api`.
- **Local diffs** — ✅ shipped: `review .` (or any repo path) renders the
  branch's diff against the default remote head via the `git` crate; the rest
  of the pipeline is shared with PRs, exactly as predicted.

**Selection is v1, not a seam**: "ask AI about this" and plain cmd-c both need
cross-row text selection, and `uniform_list` rows are independent elements —
so selection is `DiffView`-level state (anchor→cursor in row/column space),
painted as quads on visible rows, text collected from the row model on copy.
Build it in milestone 2, not retrofitted.

## Prior art: how Zed renders diffs (studied, not copied)

We read Zed's git-diff implementation (`buffer_diff`, `multi_buffer`, `editor`,
`git_ui`) before committing to this design. What matters:

**Where we converge (independent validation):**

- Zed also uses **imara-diff with Histogram**, hard-coded, over interned lines
  with terminators — our diff-engine choice matches the state of the art.
- Zed's renderer ultimately reduces every row to a **per-row diff status +
  fixed line height**, computes the visible window with O(1) arithmetic, and
  lets variable-height content live in separate "block" rows. That's exactly
  our `DisplayRow` + `uniform_list` model, minus their indirection.
- Deleted text is highlighted from a real parse of the **base file**, not from
  patch fragments — same as our Phase-2 whole-file tree-sitter pass.
- Sticky file headers, per-file +/- stats, scrollbar hunk markers, hunk
  next/prev navigation — table stakes we share.

**Where we deliberately differ, and why:**

- **Zed diffs live, editable buffers; we diff immutable snapshots.** That one
  difference drives nearly all of Zed's complexity: anchor-based hunk ranges
  in SumTrees (hunks must survive edits before the background re-diff lands),
  `DiffTransform` trees that splice deleted rows into a multibuffer, excerpt
  reconciliation (~250 lines of hand-written SumTree cursor merging with
  trailing-newline bookkeeping), and sort-order encoded into synthetic
  NUL-prefixed path keys. A PR snapshot never edits itself — plain `Vec`s of
  rows with plain line numbers are *correct* for us, not a shortcut.
- **Zed's split view is two editors** (a second read-only multibuffer of base
  text, kept row-aligned via a companion display map and injected "balancing
  blocks", with auto-collapse below a minimum width). It works, but syncing
  two scrollables is a permanent tax. Our split view is one list of paired
  rows — alignment is by construction, and it's the design decision this
  study most strongly reinforced.
- **Zed's word-level diff is deliberately minimal**: only on modified hunks
  with *equal* line counts on both sides, capped at 5 lines — because it
  recomputes on every keystroke. We diff once per PR load, so we can afford
  similarity-based line pairing and much larger caps (GitHub-quality
  intra-line highlights). Their equal-line-count case is still a good fast
  path: pair positionally, skip the similarity matrix.
- Zed reuses its entire editor stack (display maps, blocks, staging, LSP) and
  pays for that generality. We render read-only text and keep the trapdoor to
  a fully custom element instead.

**Details adopted from the study** (already folded in above): CRLF
normalization before diffing, the row-status invariant (rows are never
"modified"), background-quad coalescing across row runs, scrollbar hunk
markers, and trusting gpui's `LineLayoutCache` before building our own.

## Milestones

1. **Walking skeleton** — clap → `gh pr diff` + `gh pr view` → patch parser →
   unified view in `uniform_list` with add/remove tints and gutter. No
   highlighting. *Proves the render path end-to-end.*
2. **Make it beautiful** — tree-sitter highlighting, intra-line word diff,
   split view, dark/light themes, file tree + keyboard nav, text selection +
   copy.
3. **Make it complete** — Phase-2 blob upgrade, expand context, blob cache,
   PR picker, guardrails for huge files/binaries.
4. **Make it fast (verified)** — bench corpus of real gnarly PRs, profile,
   shaped-line LRU tuning, custom-Element trapdoor only if profiles demand it.
