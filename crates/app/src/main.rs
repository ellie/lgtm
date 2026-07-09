mod theme;

use anyhow::anyhow;
use diff_core::{DiffRow, FileStatus, PrDiff};
use fuzzy_matcher::{skim::SkimMatcherV2, FuzzyMatcher};
use gpui::{
    actions, div, prelude::*, px, size, uniform_list, App, Application, Bounds, Context,
    FocusHandle, HighlightStyle, Hsla, KeyBinding, Keystroke, ListHorizontalSizingBehavior,
    MouseButton, PathPromptOptions, ScrollStrategy, SharedString, StyledText, Subscription,
    TitlebarOptions, UniformListScrollHandle, Window, WindowBounds, WindowOptions,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    input::{Escape as InputEscape, Input, InputEvent, InputState},
    kbd::Kbd,
    scroll::Scrollbar,
    tag::Tag,
    IconName, Root, Sizable as _, TitleBar,
};
use std::ops::Range;
use std::path::Path;

const MONO: &str = "Menlo";
const ROW_HEIGHT: f32 = 22.0;
const TEXT_SIZE: f32 = 13.0;

actions!(
    review,
    [
        NextFile, PrevFile, NextHunk, PrevHunk, GoToTop, GoToBottom, ToggleView, Quit,
        ToggleSidebar, OpenInput, CloseItem, NextItem, PrevItem, Refresh, OpenPalette, PaletteUp,
        PaletteDown, PaletteBack
    ]
);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut sources = Vec::new();
    let mut errors = Vec::new();
    if args.is_empty() {
        // No args: review the repo we're standing in, or open empty.
        if let Ok(src) = git::resolve_local(Path::new(".")) {
            sources.push(Source::Local(src));
        }
    } else {
        for arg in &args {
            let parsed = if Path::new(arg).is_dir() {
                git::resolve_local(Path::new(arg)).map(Source::Local)
            } else {
                gh::resolve_pr_arg(arg).map(Source::Pr)
            };
            match parsed {
                Ok(source) => sources.push(source),
                Err(err) => {
                    eprintln!("error: {arg}: {err:#}");
                    errors.push(format!("{arg}: {err:#}"));
                }
            }
        }
    }

    Application::new()
        .with_assets(gpui_component_assets::Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            theme::apply_ui_theme(cx);
            cx.bind_keys([
                KeyBinding::new("]", NextFile, Some("ReviewApp")),
                KeyBinding::new("[", PrevFile, Some("ReviewApp")),
                KeyBinding::new("n", NextHunk, Some("ReviewApp")),
                KeyBinding::new("p", PrevHunk, Some("ReviewApp")),
                KeyBinding::new("home", GoToTop, Some("ReviewApp")),
                KeyBinding::new("end", GoToBottom, Some("ReviewApp")),
                KeyBinding::new("v", ToggleView, Some("ReviewApp")),
                KeyBinding::new("r", Refresh, Some("ReviewApp")),
                KeyBinding::new("ctrl-tab", NextItem, Some("ReviewApp")),
                KeyBinding::new("ctrl-shift-tab", PrevItem, Some("ReviewApp")),
                // Global (None context): must work while the open input is focused.
                KeyBinding::new("cmd-b", ToggleSidebar, None),
                KeyBinding::new("cmd-t", OpenInput, None),
                KeyBinding::new("cmd-w", CloseItem, None),
                KeyBinding::new("cmd-k", OpenPalette, None),
                KeyBinding::new("cmd-q", Quit, None),
                // Palette navigation. The `Palette > Input` variants are bound
                // after gpui_component::init, so at the input's dispatch depth
                // they take precedence over the Input's own up/down (which a
                // single-line input consumes without propagating).
                KeyBinding::new("up", PaletteUp, Some("Palette")),
                KeyBinding::new("down", PaletteDown, Some("Palette")),
                KeyBinding::new("escape", PaletteBack, Some("Palette")),
                KeyBinding::new("up", PaletteUp, Some("Palette > Input")),
                KeyBinding::new("down", PaletteDown, Some("Palette > Input")),
            ]);
            cx.on_action(|_: &Quit, cx| cx.quit());

            let bounds = Bounds::centered(None, size(px(1280.), px(860.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("review".into()),
                        ..TitleBar::title_bar_options()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| ReviewApp::new(sources, errors, window, cx));
                    window.focus(&view.read(cx).focus_handle);
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )
            .unwrap();
            cx.activate(true);
        });
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Unified,
    Split,
}

/// One side of a split row: line number, kind, text, word-level highlights,
/// and tree-sitter token spans.
struct Cell {
    no: u32,
    kind: LineKind,
    text: SharedString,
    intra: Vec<Range<usize>>,
    syntax: Vec<(Range<usize>, syntax::Token)>,
}

enum Row {
    Spacer,
    FileHeader {
        path: SharedString,
        old_path: Option<SharedString>,
        status: FileStatus,
        additions: u32,
        deletions: u32,
    },
    HunkHeader {
        label: SharedString,
    },
    Binary,
    Line {
        old_no: Option<u32>,
        new_no: Option<u32>,
        kind: LineKind,
        text: SharedString,
        intra: Vec<Range<usize>>,
        syntax: Vec<(Range<usize>, syntax::Token)>,
    },
    SplitLine {
        left: Option<Cell>,
        right: Option<Cell>,
    },
}

/// Guardrails: hunk sides bigger than this render without syntax highlighting.
const MAX_HUNK_SOURCE_BYTES: usize = 100 * 1024;
/// Individual lines longer than this stay plain even in a highlighted hunk.
const MAX_SYNTAX_LINE_BYTES: usize = 4096;

/// Patch-only highlighting: we have no full files, so highlight each hunk's
/// text standalone, per side — old_source is context+removed lines, new_source
/// is context+added — and hand each row its side's line spans (Context and
/// Added from new, Removed from old). Degraded (no cross-hunk parser state)
/// but visually fine. Returns one span list per hunk row, index-aligned.
fn hunk_syntax(
    lang: Option<&'static syntax::Language>,
    rows: &[DiffRow],
) -> Vec<Vec<(Range<usize>, syntax::Token)>> {
    let Some(lang) = lang else {
        return vec![Vec::new(); rows.len()];
    };
    let mut old_source = String::new();
    let mut new_source = String::new();
    // Per row: (takes spans from the old side, line index within that side).
    let mut side_lines = Vec::with_capacity(rows.len());
    let (mut old_line, mut new_line) = (0usize, 0usize);
    for row in rows {
        match row {
            DiffRow::Context { text, .. } => {
                old_source.push_str(text);
                old_source.push('\n');
                old_line += 1;
                new_source.push_str(text);
                new_source.push('\n');
                side_lines.push((false, new_line));
                new_line += 1;
            }
            DiffRow::Added { text, .. } => {
                new_source.push_str(text);
                new_source.push('\n');
                side_lines.push((false, new_line));
                new_line += 1;
            }
            DiffRow::Removed { text, .. } => {
                old_source.push_str(text);
                old_source.push('\n');
                side_lines.push((true, old_line));
                old_line += 1;
            }
        }
    }
    let highlight = |source: &str| {
        if source.is_empty() || source.len() > MAX_HUNK_SOURCE_BYTES {
            Vec::new()
        } else {
            syntax::highlight_lines(lang, source)
        }
    };
    let old_spans = highlight(&old_source);
    let new_spans = highlight(&new_source);
    rows.iter()
        .zip(side_lines)
        .map(|(row, (from_old, line))| {
            let text = match row {
                DiffRow::Context { text, .. }
                | DiffRow::Added { text, .. }
                | DiffRow::Removed { text, .. } => text,
            };
            if text.len() > MAX_SYNTAX_LINE_BYTES {
                return Vec::new();
            }
            let side = if from_old { &old_spans } else { &new_spans };
            side.get(line).cloned().unwrap_or_default()
        })
        .collect()
}

/// Flatten the diff into display rows plus the row indices of file headers and
/// hunk headers. Split mode pairs removed/added runs positionally into
/// two-cell rows; unequal runs leave one-sided rows.
fn build_rows(diff: &PrDiff, mode: ViewMode) -> (Vec<Row>, Vec<usize>, Vec<usize>) {
    let mut rows = Vec::new();
    let mut file_rows = Vec::new();
    let mut hunk_rows = Vec::new();

    for file in &diff.files {
        if !rows.is_empty() {
            rows.push(Row::Spacer);
        }
        file_rows.push(rows.len());
        rows.push(Row::FileHeader {
            path: file.display_path().to_string().into(),
            old_path: match file.status {
                FileStatus::Renamed => file.old_path.clone().map(Into::into),
                _ => None,
            },
            status: file.status,
            additions: file.additions,
            deletions: file.deletions,
        });
        if file.status == FileStatus::Binary {
            rows.push(Row::Binary);
            continue;
        }
        let lang = syntax::language_for_path(file.display_path());
        for hunk in &file.hunks {
            let syntax_spans = hunk_syntax(lang, &hunk.rows);
            hunk_rows.push(rows.len());
            let mut label = format!(
                "@@ -{},{} +{},{} @@",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            );
            if !hunk.section.is_empty() {
                label.push(' ');
                label.push_str(&hunk.section);
            }
            rows.push(Row::HunkHeader {
                label: label.into(),
            });
            match mode {
                ViewMode::Unified => {
                    for (ix, row) in hunk.rows.iter().enumerate() {
                        let syntax = syntax_spans[ix].clone();
                        rows.push(match row {
                            DiffRow::Context {
                                old_no,
                                new_no,
                                text,
                            } => Row::Line {
                                old_no: Some(*old_no),
                                new_no: Some(*new_no),
                                kind: LineKind::Context,
                                text: text.clone().into(),
                                intra: Vec::new(),
                                syntax,
                            },
                            DiffRow::Added {
                                new_no,
                                text,
                                intra,
                            } => Row::Line {
                                old_no: None,
                                new_no: Some(*new_no),
                                kind: LineKind::Added,
                                text: text.clone().into(),
                                intra: intra.clone(),
                                syntax,
                            },
                            DiffRow::Removed {
                                old_no,
                                text,
                                intra,
                            } => Row::Line {
                                old_no: Some(*old_no),
                                new_no: None,
                                kind: LineKind::Removed,
                                text: text.clone().into(),
                                intra: intra.clone(),
                                syntax,
                            },
                        });
                    }
                }
                ViewMode::Split => {
                    // Same run-scan shape as diff-core's compute_intra_line:
                    // a run of Removed immediately followed by a run of Added
                    // pairs positionally; the excess (and lone runs) render
                    // one-sided.
                    let hrows = &hunk.rows;
                    let mut i = 0;
                    while i < hrows.len() {
                        match &hrows[i] {
                            DiffRow::Context {
                                old_no,
                                new_no,
                                text,
                            } => {
                                let text: SharedString = text.clone().into();
                                let syntax = syntax_spans[i].clone();
                                rows.push(Row::SplitLine {
                                    left: Some(Cell {
                                        no: *old_no,
                                        kind: LineKind::Context,
                                        text: text.clone(),
                                        intra: Vec::new(),
                                        syntax: syntax.clone(),
                                    }),
                                    right: Some(Cell {
                                        no: *new_no,
                                        kind: LineKind::Context,
                                        text,
                                        intra: Vec::new(),
                                        syntax,
                                    }),
                                });
                                i += 1;
                            }
                            DiffRow::Added {
                                new_no,
                                text,
                                intra,
                            } => {
                                // Added run with no preceding Removed run.
                                rows.push(Row::SplitLine {
                                    left: None,
                                    right: Some(Cell {
                                        no: *new_no,
                                        kind: LineKind::Added,
                                        text: text.clone().into(),
                                        intra: intra.clone(),
                                        syntax: syntax_spans[i].clone(),
                                    }),
                                });
                                i += 1;
                            }
                            DiffRow::Removed { .. } => {
                                let start = i;
                                while i < hrows.len()
                                    && matches!(hrows[i], DiffRow::Removed { .. })
                                {
                                    i += 1;
                                }
                                let mid = i;
                                while i < hrows.len() && matches!(hrows[i], DiffRow::Added { .. })
                                {
                                    i += 1;
                                }
                                let (removed, added) = (mid - start, i - mid);
                                for pair in 0..removed.max(added) {
                                    let left = (pair < removed).then(|| {
                                        match &hrows[start + pair] {
                                            DiffRow::Removed {
                                                old_no,
                                                text,
                                                intra,
                                            } => Cell {
                                                no: *old_no,
                                                kind: LineKind::Removed,
                                                text: text.clone().into(),
                                                intra: intra.clone(),
                                                syntax: syntax_spans[start + pair].clone(),
                                            },
                                            _ => unreachable!(),
                                        }
                                    });
                                    let right = (pair < added).then(|| {
                                        match &hrows[mid + pair] {
                                            DiffRow::Added {
                                                new_no,
                                                text,
                                                intra,
                                            } => Cell {
                                                no: *new_no,
                                                kind: LineKind::Added,
                                                text: text.clone().into(),
                                                intra: intra.clone(),
                                                syntax: syntax_spans[mid + pair].clone(),
                                            },
                                            _ => unreachable!(),
                                        }
                                    });
                                    rows.push(Row::SplitLine { left, right });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    (rows, file_rows, hunk_rows)
}

/// Per-kind row tint, word-highlight tint, gutter marker, and marker color.
fn kind_style(kind: LineKind) -> (Option<gpui::Rgba>, Option<gpui::Rgba>, &'static str, gpui::Rgba) {
    match kind {
        LineKind::Context => (None, None, "", theme::overlay0()),
        LineKind::Added => (
            Some(theme::added_row_bg()),
            Some(theme::added_word_bg()),
            "+",
            theme::green(),
        ),
        LineKind::Removed => (
            Some(theme::removed_row_bg()),
            Some(theme::removed_word_bg()),
            "−",
            theme::red(),
        ),
    }
}

/// Overlay syntax color spans and intra word-diff background ranges into one
/// sorted, non-overlapping highlight list: ranges are split at every boundary
/// of either input, so an overlap gets the combined style (token foreground +
/// intra background). Both inputs are sorted and non-overlapping; all
/// boundaries come from them unchanged, so char-boundary safety is preserved.
fn merge_highlights(
    syntax: &[(Range<usize>, syntax::Token)],
    intra: &[Range<usize>],
    word_bg: Option<gpui::Rgba>,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut bounds = Vec::with_capacity(2 * (syntax.len() + intra.len()));
    for (range, _) in syntax {
        bounds.push(range.start);
        bounds.push(range.end);
    }
    for range in intra {
        bounds.push(range.start);
        bounds.push(range.end);
    }
    bounds.sort_unstable();
    bounds.dedup();

    let mut out: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let (mut si, mut ii) = (0, 0);
    for seg in bounds.windows(2) {
        let (start, end) = (seg[0], seg[1]);
        while si < syntax.len() && syntax[si].0.end <= start {
            si += 1;
        }
        while ii < intra.len() && intra[ii].end <= start {
            ii += 1;
        }
        let token = (si < syntax.len() && syntax[si].0.start <= start).then(|| syntax[si].1);
        let in_intra = ii < intra.len() && intra[ii].start <= start;
        if token.is_none() && !in_intra {
            continue;
        }
        let mut style = token.map(theme::token_style).unwrap_or_default();
        if in_intra {
            style.background_color = word_bg.map(Into::into);
        }
        match out.last_mut() {
            // Coalesce adjacent identically-styled segments.
            Some((prev, prev_style)) if prev.end == start && *prev_style == style => {
                prev.end = end
            }
            _ => out.push((start..end, style)),
        }
    }
    out
}

/// Line text with syntax colors overlaid with word-level highlight ranges,
/// shared by unified rows and split cells.
fn line_content(
    text: &SharedString,
    syntax: &[(Range<usize>, syntax::Token)],
    intra: &[Range<usize>],
    word_bg: Option<gpui::Rgba>,
) -> gpui::AnyElement {
    let highlights = merge_highlights(syntax, intra, word_bg);
    if highlights.is_empty() {
        div().child(text.clone()).into_any_element()
    } else {
        StyledText::new(text.clone())
            .with_highlights(highlights)
            .into_any_element()
    }
}

fn render_row(row: &Row) -> gpui::AnyElement {
    let row_height = px(ROW_HEIGHT);
    match row {
        Row::Spacer => div().h(row_height).into_any_element(),
        Row::FileHeader {
            path,
            old_path,
            status,
            additions,
            deletions,
        } => {
            let (status_label, status_color) = match status {
                FileStatus::Added => ("added", theme::green()),
                FileStatus::Deleted => ("deleted", theme::red()),
                FileStatus::Modified => ("modified", theme::blue()),
                FileStatus::Renamed => ("renamed", theme::mauve()),
                FileStatus::Binary => ("binary", theme::peach()),
            };
            let status: Hsla = status_color.into();
            let mut header = div()
                .h(row_height)
                .w_full()
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .bg(theme::mantle())
                .child(
                    Tag::custom(status.opacity(0.15), status, status.opacity(0.4))
                        .small()
                        .child(SharedString::from(status_label)),
                )
                .child(
                    div()
                        .text_color(theme::text())
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(path.clone()),
                );
            if let Some(old_path) = old_path {
                header = header.child(
                    div()
                        .text_color(theme::overlay0())
                        .child(SharedString::from(format!("← {old_path}"))),
                );
            }
            header
                .child(div().flex_1())
                .child(
                    div()
                        .text_color(theme::green())
                        .child(SharedString::from(format!("+{additions}"))),
                )
                .child(
                    div()
                        .text_color(theme::red())
                        .child(SharedString::from(format!("−{deletions}"))),
                )
                .into_any_element()
        }
        Row::HunkHeader { label } => div()
            .h(row_height)
            .w_full()
            .flex()
            .items_center()
            .px_3()
            .bg(theme::crust())
            .text_color(theme::overlay0())
            .child(label.clone())
            .into_any_element(),
        Row::Binary => div()
            .h(row_height)
            .flex()
            .items_center()
            .px_3()
            .text_color(theme::overlay0())
            .child(SharedString::from("binary file changed"))
            .into_any_element(),
        Row::Line {
            old_no,
            new_no,
            kind,
            text,
            intra,
            syntax,
        } => {
            let (row_bg, word_bg, marker, marker_color) = kind_style(*kind);
            let number = |no: Option<u32>| {
                div()
                    .w(px(44.))
                    .flex_shrink_0()
                    .text_color(theme::overlay0())
                    .flex()
                    .justify_end()
                    .child(SharedString::from(
                        no.map(|no| no.to_string()).unwrap_or_default(),
                    ))
            };
            let mut line = div().h(row_height).flex().items_center();
            if let Some(bg) = row_bg {
                line = line.bg(bg);
            }
            line.child(number(*old_no))
                .child(number(*new_no))
                .child(
                    div()
                        .w(px(28.))
                        .flex_shrink_0()
                        .flex()
                        .justify_center()
                        .text_color(marker_color)
                        .child(SharedString::from(marker)),
                )
                .child(
                    div()
                        .whitespace_nowrap()
                        .child(line_content(text, syntax, intra, word_bg)),
                )
                .into_any_element()
        }
        Row::SplitLine { left, right } => {
            let cell = |cell: &Option<Cell>| {
                let base = div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .h_full()
                    .flex()
                    .items_center();
                let Some(cell) = cell else {
                    return base.bg(theme::void_cell_bg());
                };
                let (row_bg, word_bg, marker, marker_color) = kind_style(cell.kind);
                let mut side = base;
                if let Some(bg) = row_bg {
                    side = side.bg(bg);
                }
                side.child(
                    div()
                        .w(px(44.))
                        .flex_shrink_0()
                        .text_color(theme::overlay0())
                        .flex()
                        .justify_end()
                        .child(SharedString::from(cell.no.to_string())),
                )
                .child(
                    div()
                        .w(px(28.))
                        .flex_shrink_0()
                        .flex()
                        .justify_center()
                        .text_color(marker_color)
                        .child(SharedString::from(marker)),
                )
                .child(
                    div()
                        .whitespace_nowrap()
                        .child(line_content(&cell.text, &cell.syntax, &cell.intra, word_bg)),
                )
            };
            // w_full is load-bearing: without a definite row width the row
            // sizes to fit-content and the flex_1 halves collapse to their
            // text width, putting the divider at a different x every row.
            div()
                .h(row_height)
                .w_full()
                .flex()
                .child(cell(left))
                .child(
                    div()
                        .w(px(6.))
                        .flex_shrink_0()
                        .h_full()
                        .bg(theme::crust())
                        .border_l_1()
                        .border_r_1()
                        .border_color(theme::surface0()),
                )
                .child(cell(right))
                .into_any_element()
        }
    }
}

/// Where a review item's diff comes from.
#[derive(Clone)]
enum Source {
    Pr(gh::PrLocator),
    Local(git::LocalSource),
}

enum ItemState {
    Loading,
    Ready(Box<ItemData>),
    Failed(String),
}

/// Everything the diff pane needs for one loaded item. Each item owns its
/// scroll handle so per-item scroll position survives switching.
struct ItemData {
    pr_meta: Option<gh::PrMeta>,
    diff: PrDiff,
    mode: ViewMode,
    rows: Vec<Row>,
    file_rows: Vec<usize>,
    hunk_rows: Vec<usize>,
    cursor: usize,
    scroll: UniformListScrollHandle,
    additions: u32,
    deletions: u32,
}

struct ReviewItem {
    id: u64,
    source: Source,
    state: ItemState,
    /// A refresh is in flight while the old data stays visible.
    reloading: bool,
    refresh_error: Option<SharedString>,
}

impl ReviewItem {
    fn primary(&self) -> SharedString {
        match &self.source {
            Source::Pr(loc) => format!("{}#{}", loc.repo_slug(), loc.number).into(),
            Source::Local(src) => src.branch.clone().into(),
        }
    }

    fn secondary(&self) -> SharedString {
        match &self.source {
            Source::Pr(_) => match &self.state {
                ItemState::Ready(data) => data
                    .pr_meta
                    .as_ref()
                    .map(|meta| meta.title.clone())
                    .unwrap_or_default()
                    .into(),
                _ => SharedString::default(),
            },
            Source::Local(src) => {
                format!("{} ← {}", dir_name(&src.repo_root), src.base_label).into()
            }
        }
    }

    fn dot_color(&self) -> gpui::Rgba {
        match &self.source {
            Source::Local(_) => theme::blue(),
            Source::Pr(_) => match &self.state {
                ItemState::Ready(data) => match data.pr_meta.as_ref().map(|m| m.state.as_str()) {
                    Some("OPEN") => theme::green(),
                    Some("MERGED") => theme::mauve(),
                    Some("CLOSED") => theme::red(),
                    _ => theme::overlay0(),
                },
                ItemState::Failed(_) => theme::red(),
                ItemState::Loading => theme::overlay0(),
            },
        }
    }

    /// Swap fetched data in. On refresh (already Ready) scroll position,
    /// cursor, and view mode are preserved.
    fn install(&mut self, loaded: Loaded) {
        let Loaded {
            meta,
            diff,
            mut rows,
            mut file_rows,
            mut hunk_rows,
            mode,
        } = loaded;
        let (additions, deletions) = diff
            .files
            .iter()
            .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
        let pr_meta = match meta {
            LoadedMeta::Pr(meta) => Some(meta),
            LoadedMeta::Local(src) => {
                // Branch/base may have moved since open; keep the label fresh.
                self.source = Source::Local(src);
                None
            }
        };
        self.refresh_error = None;
        match &mut self.state {
            ItemState::Ready(data) => {
                if data.mode != mode {
                    // The user toggled unified/split while the refresh ran.
                    (rows, file_rows, hunk_rows) = build_rows(&diff, data.mode);
                }
                if pr_meta.is_some() {
                    data.pr_meta = pr_meta;
                }
                data.diff = diff;
                data.rows = rows;
                data.file_rows = file_rows;
                data.hunk_rows = hunk_rows;
                data.cursor = data.cursor.min(data.rows.len().saturating_sub(1));
                data.additions = additions;
                data.deletions = deletions;
            }
            _ => {
                self.state = ItemState::Ready(Box::new(ItemData {
                    pr_meta,
                    diff,
                    mode,
                    rows,
                    file_rows,
                    hunk_rows,
                    cursor: 0,
                    scroll: UniformListScrollHandle::new(),
                    additions,
                    deletions,
                }));
            }
        }
    }
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

enum LoadedMeta {
    Pr(gh::PrMeta),
    Local(git::LocalSource),
}

struct Loaded {
    meta: LoadedMeta,
    diff: PrDiff,
    rows: Vec<Row>,
    file_rows: Vec<usize>,
    hunk_rows: Vec<usize>,
    mode: ViewMode,
}

/// Blocking fetch + parse + row building for one item; runs on the background
/// executor, so subprocess waits and tree-sitter work stay off the main thread.
fn fetch_item(source: &Source, mode: ViewMode) -> anyhow::Result<Loaded> {
    let (meta, patch) = match source {
        Source::Pr(loc) => {
            let meta_loc = loc.clone();
            let meta_thread = std::thread::spawn(move || gh::fetch_meta(&meta_loc));
            let patch = gh::fetch_patch(loc)?;
            let meta = meta_thread
                .join()
                .map_err(|_| anyhow!("gh metadata fetch panicked"))??;
            (LoadedMeta::Pr(meta), patch)
        }
        Source::Local(src) => {
            let src = git::resolve_local(&src.repo_root)?;
            let patch = git::diff_patch(&src)?;
            (LoadedMeta::Local(src), patch)
        }
    };
    let diff = diff_core::parse_patch(&patch);
    let (rows, file_rows, hunk_rows) = build_rows(&diff, mode);
    Ok(Loaded {
        meta,
        diff,
        rows,
        file_rows,
        hunk_rows,
        mode,
    })
}

fn centered_message(text: SharedString, color: gpui::Rgba) -> gpui::AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(color)
        .child(text)
        .into_any_element()
}

fn app_title(detail: Option<String>) -> gpui::AnyElement {
    let mut title = div()
        .flex()
        .items_center()
        .gap_2()
        .flex_1()
        .min_w_0()
        .child(
            div()
                .font_weight(gpui::FontWeight::BOLD)
                .child(SharedString::from("review")),
        );
    if let Some(detail) = detail {
        title = title.child(
            div()
                .text_color(theme::subtext())
                .truncate()
                .child(SharedString::from(detail)),
        );
    }
    title.into_any_element()
}

fn pr_titlebar_content(meta: &gh::PrMeta) -> gpui::AnyElement {
    let (state_color, state_label) = match meta.state.as_str() {
        "OPEN" => (theme::green(), "open"),
        "MERGED" => (theme::mauve(), "merged"),
        "CLOSED" => (theme::red(), "closed"),
        other => (theme::overlay0(), other),
    };
    let state: Hsla = state_color.into();
    let url = meta.url.clone();
    div()
        .flex()
        .items_center()
        .flex_1()
        .min_w_0()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .min_w_0()
                .flex_1()
                .child(
                    Tag::custom(state.opacity(0.15), state, state.opacity(0.4))
                        .small()
                        .child(SharedString::from(state_label.to_string())),
                )
                .child(
                    div()
                        .font_weight(gpui::FontWeight::BOLD)
                        .truncate()
                        .child(SharedString::from(meta.title.clone())),
                )
                .child(
                    div()
                        .text_color(theme::subtext())
                        .child(SharedString::from(format!("#{}", meta.number))),
                )
                .child(
                    div()
                        .text_color(theme::subtext())
                        .whitespace_nowrap()
                        .child(SharedString::from(format!("by {}", meta.author.login))),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .flex_shrink_0()
                .pr_3()
                .child(div().text_color(theme::overlay0()).child(SharedString::from(
                    format!("{} ← {}", meta.base_ref_name, meta.head_ref_name),
                )))
                .child(
                    div()
                        .text_color(theme::green())
                        .child(SharedString::from(format!("+{}", meta.additions))),
                )
                .child(
                    div()
                        .text_color(theme::red())
                        .child(SharedString::from(format!("−{}", meta.deletions))),
                )
                .child(
                    Button::new("open-in-browser")
                        .icon(IconName::ExternalLink)
                        .ghost()
                        .xsmall()
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                ),
        )
        .into_any_element()
}

fn local_titlebar_content(src: &git::LocalSource, data: &ItemData) -> gpui::AnyElement {
    let blue: Hsla = theme::blue().into();
    div()
        .flex()
        .items_center()
        .flex_1()
        .min_w_0()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .min_w_0()
                .flex_1()
                .child(
                    Tag::custom(blue.opacity(0.15), blue, blue.opacity(0.4))
                        .small()
                        .child(SharedString::from("local")),
                )
                .child(
                    div()
                        .font_weight(gpui::FontWeight::BOLD)
                        .truncate()
                        .child(SharedString::from(dir_name(&src.repo_root))),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .flex_shrink_0()
                .pr_3()
                .child(div().text_color(theme::overlay0()).child(SharedString::from(
                    format!("{} ← {}", src.base_label, src.branch),
                )))
                .child(
                    div()
                        .text_color(theme::green())
                        .child(SharedString::from(format!("+{}", data.additions))),
                )
                .child(
                    div()
                        .text_color(theme::red())
                        .child(SharedString::from(format!("−{}", data.deletions))),
                ),
        )
        .into_any_element()
}

/// The cmd-k command palette: a staged flow for opening things.
enum PaletteStep {
    /// Step 1: pick what to open.
    Sources { selected: usize },
    /// Step 2 (GitHub path): type `owner/repo`.
    RepoInput { error: Option<SharedString> },
    /// Step 3 (GitHub path): pick a PR from the repo's open list.
    PrList { repo: String, prs: PrListState },
}

enum PrListState {
    Loading,
    Loaded {
        all: Vec<gh::PrSummary>,
        /// Indices into `all`, fuzzy-filtered by the query, best match first.
        filtered: Vec<usize>,
        /// Index into `filtered`.
        selected: usize,
    },
    Failed(String),
}

const PALETTE_SOURCES: [&str; 2] = ["Open GitHub pull request", "Open local folder"];
const SOURCE_PR: usize = 0;
const SOURCE_FOLDER: usize = 1;
const PALETTE_ROW_HEIGHT: f32 = 30.0;

/// Step-1 options matching the query (case-insensitive substring), as indices
/// into PALETTE_SOURCES. Empty query keeps both.
fn filtered_sources(query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    PALETTE_SOURCES
        .iter()
        .enumerate()
        .filter(|(_, label)| q.is_empty() || label.to_lowercase().contains(&q))
        .map(|(ix, _)| ix)
        .collect()
}

/// Fuzzy-filter PRs against `#number title author branch`, best score first;
/// an empty query keeps gh's original (most recently updated) order.
fn filter_prs(all: &[gh::PrSummary], query: &str) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return (0..all.len()).collect();
    }
    let matcher = SkimMatcherV2::default();
    let mut scored: Vec<(i64, usize)> = all
        .iter()
        .enumerate()
        .filter_map(|(ix, pr)| {
            let haystack = format!(
                "#{} {} {} {}",
                pr.number, pr.title, pr.author.login, pr.head_ref_name
            );
            matcher.fuzzy_match(&haystack, query).map(|score| (score, ix))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, ix)| ix).collect()
}

/// One row of the palette's PR list: state dot, #number, title, author, head
/// branch. Clicking opens the PR just like enter does.
fn palette_pr_row(
    pr: &gh::PrSummary,
    pos: usize,
    selected: bool,
    entity: gpui::Entity<ReviewApp>,
) -> gpui::AnyElement {
    let dot = if pr.is_draft {
        theme::overlay0()
    } else {
        theme::green()
    };
    div()
        .id(("palette-pr", pos))
        .mx_1()
        .px_2()
        .h(px(PALETTE_ROW_HEIGHT))
        .rounded_md()
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .when(selected, |row| row.bg(theme::surface0()))
        .when(!selected, |row| {
            row.hover(|style| style.bg(Hsla::from(theme::surface0()).opacity(0.5)))
        })
        .on_click(move |_, window, cx| {
            entity.update(cx, |this, cx| this.palette_open_pr_row(pos, window, cx));
        })
        .child(
            div()
                .w(px(8.))
                .h(px(8.))
                .flex_shrink_0()
                .rounded_full()
                .bg(dot),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_color(theme::subtext())
                .child(SharedString::from(format!("#{}", pr.number))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(theme::text())
                .child(SharedString::from(pr.title.clone())),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_color(theme::subtext())
                .child(SharedString::from(pr.author.login.clone())),
        )
        .child(
            div()
                .flex_shrink_0()
                .max_w(px(140.))
                .truncate()
                .text_color(theme::overlay0())
                .child(SharedString::from(pr.head_ref_name.clone())),
        )
        .into_any_element()
}

fn parse_repo_slug(value: &str) -> Result<(String, String), &'static str> {
    let (owner, repo) = value.split_once('/').ok_or("expected owner/repo")?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return Err("expected owner/repo");
    }
    Ok((owner.to_string(), repo.to_string()))
}

struct ReviewApp {
    items: Vec<ReviewItem>,
    active: usize,
    sidebar_visible: bool,
    open_input: gpui::Entity<InputState>,
    open_error: Option<SharedString>,
    focus_handle: FocusHandle,
    next_id: u64,
    palette: Option<PaletteStep>,
    palette_input: gpui::Entity<InputState>,
    /// Bumped on every palette transition; an in-flight PR-list fetch only
    /// lands if the generation it captured is still current.
    palette_gen: u64,
    palette_scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl ReviewApp {
    fn new(
        sources: Vec<Source>,
        errors: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let open_input = cx
            .new(|cx| InputState::new(window, cx).placeholder("owner/repo#123, PR URL, or path"));
        let palette_input = cx.new(|cx| InputState::new(window, cx));
        let _subscriptions = vec![
            cx.subscribe_in(
                &open_input,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.submit_open(window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &palette_input,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.palette_confirm(window, cx),
                    InputEvent::Change => this.palette_query_changed(cx),
                    _ => {}
                },
            ),
        ];
        let mut this = Self {
            items: Vec::new(),
            active: 0,
            sidebar_visible: !errors.is_empty() || sources.len() != 1,
            open_input,
            open_error: errors.first().cloned().map(SharedString::from),
            focus_handle: cx.focus_handle(),
            next_id: 0,
            palette: None,
            palette_input,
            palette_gen: 0,
            palette_scroll: UniformListScrollHandle::new(),
            _subscriptions,
        };
        for source in sources {
            this.open_item(source, cx);
        }
        this.active = 0;
        this
    }

    fn active_item(&self) -> Option<&ReviewItem> {
        self.items.get(self.active)
    }

    fn active_data(&self) -> Option<&ItemData> {
        match &self.items.get(self.active)?.state {
            ItemState::Ready(data) => Some(data),
            _ => None,
        }
    }

    fn active_data_mut(&mut self) -> Option<&mut ItemData> {
        match &mut self.items.get_mut(self.active)?.state {
            ItemState::Ready(data) => Some(data),
            _ => None,
        }
    }

    fn open_item(&mut self, source: Source, cx: &mut Context<Self>) {
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(ReviewItem {
            id,
            source: source.clone(),
            state: ItemState::Loading,
            reloading: false,
            refresh_error: None,
        });
        self.active = self.items.len() - 1;
        Self::spawn_fetch(id, source, ViewMode::Split, cx);
        cx.notify();
    }

    fn spawn_fetch(id: u64, source: Source, mode: ViewMode, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { fetch_item(&source, mode) })
                .await;
            this.update(cx, |app, cx| {
                let Some(item) = app.items.iter_mut().find(|item| item.id == id) else {
                    return;
                };
                item.reloading = false;
                match fetched {
                    Ok(loaded) => item.install(loaded),
                    Err(err) => {
                        let msg = format!("{err:#}");
                        match &item.state {
                            // Refresh failure: keep the stale-but-useful data.
                            ItemState::Ready(_) => item.refresh_error = Some(msg.into()),
                            _ => item.state = ItemState::Failed(msg),
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn submit_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.open_input.read(cx).value().trim().to_string();
        if value.is_empty() {
            return;
        }
        let parsed = if Path::new(&value).is_dir() {
            git::resolve_local(Path::new(&value)).map(Source::Local)
        } else {
            gh::resolve_pr_arg(&value).map(Source::Pr)
        };
        match parsed {
            Ok(source) => {
                self.open_error = None;
                self.open_input
                    .update(cx, |state, cx| state.set_value("", window, cx));
                self.open_item(source, cx);
                window.focus(&self.focus_handle);
            }
            Err(err) => self.open_error = Some(format!("{err:#}").into()),
        }
        cx.notify();
    }

    /// Reset the palette's shared input for the step being entered and keep
    /// keyboard focus in it (the palette traps focus while open).
    fn set_palette_input(
        &mut self,
        value: &str,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.palette_input.update(cx, |state, cx| {
            state.set_value(value.to_string(), window, cx);
            state.set_placeholder(placeholder, window, cx);
            state.focus(window, cx);
        });
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = Some(PaletteStep::Sources { selected: 0 });
        self.palette_gen += 1;
        self.set_palette_input("", "type to filter…", window, cx);
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        self.palette_gen += 1;
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Esc: one step back, or close from step 1.
    fn palette_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.palette {
            None | Some(PaletteStep::Sources { .. }) => self.close_palette(window, cx),
            Some(PaletteStep::RepoInput { .. }) => {
                self.palette = Some(PaletteStep::Sources { selected: 0 });
                self.palette_gen += 1;
                self.set_palette_input("", "type to filter…", window, cx);
                cx.notify();
            }
            Some(PaletteStep::PrList { repo, .. }) => {
                let repo = repo.clone();
                self.palette = Some(PaletteStep::RepoInput { error: None });
                self.palette_gen += 1;
                self.set_palette_input(&repo, "owner/repo", window, cx);
                cx.notify();
            }
        }
    }

    fn palette_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let query = self.palette_input.read(cx).value().to_string();
        match &mut self.palette {
            Some(PaletteStep::Sources { selected }) => {
                let len = filtered_sources(&query).len();
                if len > 0 {
                    *selected = (*selected as isize + delta).clamp(0, len as isize - 1) as usize;
                }
            }
            Some(PaletteStep::PrList {
                prs: PrListState::Loaded { filtered, selected, .. },
                ..
            }) => {
                if !filtered.is_empty() {
                    *selected =
                        (*selected as isize + delta).clamp(0, filtered.len() as isize - 1) as usize;
                    self.palette_scroll
                        .scroll_to_item(*selected, ScrollStrategy::Top);
                }
            }
            _ => return,
        }
        cx.notify();
    }

    fn palette_query_changed(&mut self, cx: &mut Context<Self>) {
        let query = self.palette_input.read(cx).value().to_string();
        match &mut self.palette {
            Some(PaletteStep::Sources { selected }) => {
                let len = filtered_sources(&query).len();
                *selected = (*selected).min(len.saturating_sub(1));
            }
            Some(PaletteStep::RepoInput { error }) => *error = None,
            Some(PaletteStep::PrList {
                prs: PrListState::Loaded { all, filtered, selected },
                ..
            }) => {
                *filtered = filter_prs(all, &query);
                *selected = 0;
                self.palette_scroll.scroll_to_item(0, ScrollStrategy::Top);
            }
            _ => return,
        }
        cx.notify();
    }

    /// Enter, on whichever step is showing.
    fn palette_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.palette_input.read(cx).value().trim().to_string();
        match &self.palette {
            Some(PaletteStep::Sources { selected }) => {
                if let Some(&opt) = filtered_sources(&query).get(*selected) {
                    self.palette_activate_source(opt, window, cx);
                }
            }
            Some(PaletteStep::RepoInput { .. }) => match parse_repo_slug(&query) {
                Ok((owner, repo)) => self.palette_fetch_prs(owner, repo, window, cx),
                Err(msg) => {
                    if let Some(PaletteStep::RepoInput { error }) = &mut self.palette {
                        *error = Some(msg.into());
                        cx.notify();
                    }
                }
            },
            Some(PaletteStep::PrList {
                prs: PrListState::Loaded { selected, .. },
                ..
            }) => {
                let selected = *selected;
                self.palette_open_pr_row(selected, window, cx);
            }
            _ => {}
        }
    }

    fn palette_activate_source(&mut self, opt: usize, window: &mut Window, cx: &mut Context<Self>) {
        match opt {
            SOURCE_PR => {
                self.palette = Some(PaletteStep::RepoInput { error: None });
                self.palette_gen += 1;
                self.set_palette_input("", "owner/repo", window, cx);
                cx.notify();
            }
            SOURCE_FOLDER => {
                // The palette closes when the native dialog opens.
                self.close_palette(window, cx);
                self.prompt_open_folder(cx);
            }
            _ => {}
        }
    }

    /// Step 2 → step 3: show "loading…" and fetch the open-PR list on the
    /// background executor. The generation guard drops the result if the user
    /// closed the palette or navigated away before it landed.
    fn palette_fetch_prs(
        &mut self,
        owner: String,
        repo: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.palette_gen += 1;
        let gen = self.palette_gen;
        self.palette = Some(PaletteStep::PrList {
            repo: format!("{owner}/{repo}"),
            prs: PrListState::Loading,
        });
        self.set_palette_input("", "search pull requests…", window, cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { gh::list_prs(&owner, &repo) })
                .await;
            this.update(cx, |app, cx| {
                if app.palette_gen != gen {
                    return;
                }
                let query = app.palette_input.read(cx).value().to_string();
                let Some(PaletteStep::PrList { prs, .. }) = &mut app.palette else {
                    return;
                };
                *prs = match fetched {
                    Ok(all) => {
                        let filtered = filter_prs(&all, &query);
                        PrListState::Loaded {
                            all,
                            filtered,
                            selected: 0,
                        }
                    }
                    Err(err) => PrListState::Failed(format!("{err:#}")),
                };
                app.palette_scroll.scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open the PR at `pos` in the filtered list as a new review item.
    fn palette_open_pr_row(&mut self, pos: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(PaletteStep::PrList {
            repo,
            prs: PrListState::Loaded { all, filtered, .. },
        }) = &self.palette
        else {
            return;
        };
        let Some(&ix) = filtered.get(pos) else {
            return;
        };
        let Some((owner, repo)) = repo.split_once('/') else {
            return;
        };
        let source = Source::Pr(gh::PrLocator {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number: all[ix].number,
        });
        self.close_palette(window, cx);
        self.open_item(source, cx);
    }

    /// Native directory picker. The chosen path becomes a Local item through
    /// the normal add-item path; fetch_item re-resolves it on the background
    /// executor, so a non-repo directory surfaces as a Failed item.
    fn prompt_open_folder(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update(cx, |app, cx| {
                let source = Source::Local(git::LocalSource {
                    branch: dir_name(&path),
                    base_label: "…".to_string(),
                    repo_root: path,
                });
                app.open_item(source, cx);
            })
            .ok();
        })
        .detach();
    }

    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.items.len() {
            self.active = ix;
            window.focus(&self.focus_handle);
            cx.notify();
        }
    }

    fn close_item(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.items.len() {
            return;
        }
        self.items.remove(ix);
        if self.active > ix || self.active >= self.items.len() {
            self.active = self.active.saturating_sub(1);
        }
        cx.notify();
    }

    fn cycle_items(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.items.len() as isize;
        if len > 0 {
            self.active = (self.active as isize + delta).rem_euclid(len) as usize;
            cx.notify();
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.items.get_mut(self.active) else {
            return;
        };
        if matches!(item.state, ItemState::Loading) || item.reloading {
            return;
        }
        let mode = match &item.state {
            ItemState::Ready(data) => data.mode,
            _ => ViewMode::Split,
        };
        match item.state {
            ItemState::Failed(_) => item.state = ItemState::Loading,
            _ => item.reloading = true,
        }
        item.refresh_error = None;
        let (id, source) = (item.id, item.source.clone());
        Self::spawn_fetch(id, source, mode, cx);
        cx.notify();
    }

    fn toggle_view(&mut self, cx: &mut Context<Self>) {
        let Some(data) = self.active_data_mut() else {
            return;
        };
        // Best-effort position preservation: stay on the same file.
        let file_pos = data.file_rows.iter().rposition(|&ix| ix <= data.cursor);
        data.mode = match data.mode {
            ViewMode::Unified => ViewMode::Split,
            ViewMode::Split => ViewMode::Unified,
        };
        let (rows, file_rows, hunk_rows) = build_rows(&data.diff, data.mode);
        data.rows = rows;
        data.file_rows = file_rows;
        data.hunk_rows = hunk_rows;
        let target = file_pos
            .and_then(|pos| data.file_rows.get(pos).copied())
            .unwrap_or(0);
        self.jump(target, cx);
    }

    fn jump(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(data) = self.active_data_mut() {
            data.cursor = ix;
            data.scroll.scroll_to_item_strict(ix, ScrollStrategy::Top);
        }
        cx.notify();
    }

    fn jump_next(&mut self, targets: &[usize], cx: &mut Context<Self>) {
        let Some(cursor) = self.active_data().map(|data| data.cursor) else {
            return;
        };
        if let Some(&ix) = targets.iter().find(|&&ix| ix > cursor) {
            self.jump(ix, cx);
        }
    }

    fn jump_prev(&mut self, targets: &[usize], cx: &mut Context<Self>) {
        let Some(cursor) = self.active_data().map(|data| data.cursor) else {
            return;
        };
        if let Some(&ix) = targets.iter().rev().find(|&&ix| ix < cursor) {
            self.jump(ix, cx);
        }
    }

    fn render_titlebar(&self) -> impl IntoElement {
        let content: gpui::AnyElement = match self.active_item() {
            None => app_title(None),
            Some(item) => match &item.state {
                ItemState::Ready(data) => match &item.source {
                    Source::Pr(_) => match &data.pr_meta {
                        Some(meta) => pr_titlebar_content(meta),
                        None => app_title(None),
                    },
                    Source::Local(src) => local_titlebar_content(src, data),
                },
                ItemState::Loading => app_title(Some(format!("loading {}…", item.primary()))),
                ItemState::Failed(_) => app_title(Some(format!("{} — failed", item.primary()))),
            },
        };
        let note: Option<SharedString> = self.active_item().and_then(|item| {
            if item.reloading {
                Some("reloading…".into())
            } else {
                item.refresh_error
                    .as_ref()
                    .map(|err| SharedString::from(format!("refresh failed: {err}")))
            }
        });
        TitleBar::new()
            .text_size(px(13.))
            .child(content)
            .when_some(note, |bar, note| {
                bar.child(
                    div()
                        .max_w(px(280.))
                        .truncate()
                        .text_color(theme::overlay0())
                        .pr_3()
                        .child(note),
                )
            })
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div()
            .id("sidebar-items")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .py_1();
        for (ix, item) in self.items.iter().enumerate() {
            let active = ix == self.active;
            let dot: Hsla = item.dot_color().into();
            let status: gpui::AnyElement = match &item.state {
                ItemState::Ready(data) => div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .child(
                        div()
                            .text_color(theme::green())
                            .child(SharedString::from(format!("+{}", data.additions))),
                    )
                    .child(
                        div()
                            .text_color(theme::red())
                            .child(SharedString::from(format!("−{}", data.deletions))),
                    )
                    .into_any_element(),
                ItemState::Loading => div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(theme::overlay0())
                    .child(SharedString::from("loading…"))
                    .into_any_element(),
                ItemState::Failed(_) => div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(theme::red())
                    .child(SharedString::from("failed"))
                    .into_any_element(),
            };
            let secondary = item.secondary();
            let entry = div()
                .id(("item", ix))
                .group("sidebar-item")
                .mx_1()
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .when(active, |entry| entry.bg(theme::surface0()))
                .when(!active, |entry| {
                    entry.hover(|style| style.bg(Hsla::from(theme::surface0()).opacity(0.5)))
                })
                .on_click(cx.listener(move |this, _, window, cx| this.activate(ix, window, cx)))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .w(px(8.))
                                .h(px(8.))
                                .flex_shrink_0()
                                .rounded_full()
                                .bg(dot),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(theme::text())
                                .child(item.primary()),
                        )
                        .child(status)
                        .child(
                            div()
                                .flex_shrink_0()
                                .opacity(0.)
                                .group_hover("sidebar-item", |style| style.opacity(1.))
                                .child(
                                    Button::new(("close-item", ix))
                                        .icon(IconName::Close)
                                        .ghost()
                                        .xsmall()
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.close_item(ix, cx)
                                        })),
                                ),
                        ),
                )
                .when(!secondary.is_empty(), |entry| {
                    entry.child(
                        div()
                            .pl(px(16.))
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme::subtext())
                            .child(secondary),
                    )
                });
            list = list.child(entry);
        }
        div()
            .w(px(260.))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::mantle())
            .border_r_1()
            .border_color(theme::surface0())
            .text_size(px(12.))
            .child(
                div()
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(Input::new(&self.open_input).small())
                    .when_some(self.open_error.clone(), |area, err| {
                        area.child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme::red())
                                .child(err),
                        )
                    }),
            )
            .child(list)
    }

    fn render_palette(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(step) = &self.palette else {
            return div().into_any_element();
        };
        let query = self.palette_input.read(cx).value().to_string();

        let header: Option<SharedString> = match step {
            PaletteStep::Sources { .. } => None,
            PaletteStep::RepoInput { .. } => Some("Open GitHub pull request".into()),
            PaletteStep::PrList { repo, .. } => Some(repo.clone().into()),
        };

        let body: gpui::AnyElement = match step {
            PaletteStep::Sources { selected } => {
                let filtered = filtered_sources(&query);
                let selected = *selected;
                let mut list = div().py_1().flex().flex_col();
                if filtered.is_empty() {
                    list = list.child(
                        div()
                            .px_3()
                            .py_2()
                            .text_color(theme::overlay0())
                            .child(SharedString::from("no matches")),
                    );
                }
                for (pos, &opt) in filtered.iter().enumerate() {
                    list = list.child(
                        div()
                            .id(("palette-source", opt))
                            .mx_1()
                            .px_2()
                            .h(px(PALETTE_ROW_HEIGHT))
                            .rounded_md()
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .when(pos == selected, |row| row.bg(theme::surface0()))
                            .when(pos != selected, |row| {
                                row.hover(|style| {
                                    style.bg(Hsla::from(theme::surface0()).opacity(0.5))
                                })
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.palette_activate_source(opt, window, cx)
                            }))
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .child(SharedString::from(PALETTE_SOURCES[opt])),
                            ),
                    );
                }
                list.into_any_element()
            }
            PaletteStep::RepoInput { error } => {
                let (text, color): (SharedString, gpui::Rgba) = match error {
                    Some(err) => (err.clone(), theme::red()),
                    None => (
                        "enter to list open pull requests · esc to go back".into(),
                        theme::overlay0(),
                    ),
                };
                div()
                    .px_3()
                    .py_2()
                    .text_color(color)
                    .child(text)
                    .into_any_element()
            }
            PaletteStep::PrList { prs, .. } => match prs {
                PrListState::Loading => div()
                    .px_3()
                    .py_2()
                    .text_color(theme::overlay0())
                    .child(SharedString::from("loading pull requests…"))
                    .into_any_element(),
                PrListState::Failed(msg) => div()
                    .px_3()
                    .py_2()
                    .text_color(theme::red())
                    .child(SharedString::from(msg.clone()))
                    .into_any_element(),
                PrListState::Loaded { filtered, .. } if filtered.is_empty() => div()
                    .px_3()
                    .py_2()
                    .text_color(theme::overlay0())
                    .child(SharedString::from("no matching pull requests"))
                    .into_any_element(),
                PrListState::Loaded { filtered, .. } => {
                    let entity = cx.entity();
                    let count = filtered.len();
                    div()
                        .py_1()
                        .h(px(count.min(10) as f32 * PALETTE_ROW_HEIGHT + 8.))
                        .child(
                            uniform_list("palette-prs", count, move |range, _window, cx| {
                                let this = entity.read(cx);
                                let Some(PaletteStep::PrList {
                                    prs: PrListState::Loaded { all, filtered, selected },
                                    ..
                                }) = &this.palette
                                else {
                                    return Vec::new();
                                };
                                range
                                    .filter_map(|pos| Some((pos, &all[*filtered.get(pos)?])))
                                    .map(|(pos, pr)| {
                                        palette_pr_row(pr, pos, pos == *selected, entity.clone())
                                    })
                                    .collect()
                            })
                            .track_scroll(self.palette_scroll.clone())
                            .h_full(),
                        )
                        .into_any_element()
                }
            },
        };

        // Backdrop: dims and occludes everything (sidebar included); click
        // closes. The panel swallows its own mouse-downs so clicks inside
        // don't bubble to the backdrop.
        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(120.))
            .bg(theme::palette_backdrop())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.close_palette(window, cx);
                }),
            )
            .child(
                div()
                    .key_context("Palette")
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_action(cx.listener(|this, _: &PaletteUp, _, cx| this.palette_move(-1, cx)))
                    .on_action(cx.listener(|this, _: &PaletteDown, _, cx| this.palette_move(1, cx)))
                    .on_action(
                        cx.listener(|this, _: &PaletteBack, window, cx| {
                            this.palette_back(window, cx)
                        }),
                    )
                    // The palette input propagates Escape when it has nothing
                    // of its own to dismiss; intercept it here before the
                    // root's handler steals focus back to the diff.
                    .on_action(
                        cx.listener(|this, _: &InputEscape, window, cx| {
                            this.palette_back(window, cx)
                        }),
                    )
                    .w(px(560.))
                    .rounded_lg()
                    .border_1()
                    .border_color(theme::surface0())
                    .bg(theme::mantle())
                    .shadow_lg()
                    .text_size(px(13.))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .when_some(header, |panel, header| {
                        panel.child(
                            div()
                                .px_3()
                                .pt_2()
                                .text_size(px(11.))
                                .text_color(theme::overlay0())
                                .child(header),
                        )
                    })
                    .child(
                        div()
                            .p_2()
                            .border_b_1()
                            .border_color(theme::surface0())
                            .child(Input::new(&self.palette_input).small()),
                    )
                    .child(body),
            )
            .into_any_element()
    }

    fn render_footer(&self) -> impl IntoElement {
        let hint = |keys: &[&str], label: &'static str| {
            let mut hint = div().flex().items_center().gap_1();
            for key in keys {
                hint = hint.child(Kbd::new(Keystroke::parse(key).unwrap()));
            }
            hint.child(
                div()
                    .text_color(theme::overlay0())
                    .child(SharedString::from(label)),
            )
        };
        div()
            .h(px(28.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_4()
            .px_3()
            .bg(theme::mantle())
            .border_t_1()
            .border_color(theme::surface0())
            .text_size(px(12.))
            .child(hint(&["]", "["], "files"))
            .child(hint(&["n", "p"], "hunks"))
            .child(hint(&["v"], "unified/split"))
            .child(hint(&["home", "end"], "top/bottom"))
            .child(hint(&["cmd-k"], "palette"))
            .child(hint(&["cmd-t"], "open"))
            .child(hint(&["cmd-b"], "sidebar"))
            .child(hint(&["r"], "refresh"))
    }
}

impl Render for ReviewApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let pane: gpui::AnyElement = match self.active_item() {
            None => centered_message("⌘T to open a PR or path".into(), theme::overlay0()),
            Some(item) => match &item.state {
                ItemState::Loading => centered_message("loading…".into(), theme::overlay0()),
                ItemState::Failed(msg) => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p_8()
                    .child(
                        div()
                            .max_w(px(720.))
                            .text_color(theme::red())
                            .child(SharedString::from(msg.clone())),
                    )
                    .into_any_element(),
                ItemState::Ready(data) => div()
                    .size_full()
                    .relative()
                    .font_family(MONO)
                    .text_size(px(TEXT_SIZE))
                    .line_height(px(ROW_HEIGHT))
                    .child(
                        uniform_list("diff", data.rows.len(), move |range, _window, cx| {
                            let this = entity.read(cx);
                            match this.active_data() {
                                Some(data) => {
                                    range.filter_map(|ix| data.rows.get(ix)).map(render_row).collect()
                                }
                                None => Vec::new(),
                            }
                        })
                        .track_scroll(data.scroll.clone())
                        .with_horizontal_sizing_behavior(match data.mode {
                            ViewMode::Unified => ListHorizontalSizingBehavior::Unconstrained,
                            ViewMode::Split => ListHorizontalSizingBehavior::FitList,
                        })
                        .h_full(),
                    )
                    .child(Scrollbar::new(&data.scroll))
                    .into_any_element(),
            },
        };
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme::base())
            .text_color(theme::text())
            .on_action(cx.listener(|this, _: &OpenPalette, window, cx| {
                if this.palette.is_some() {
                    this.close_palette(window, cx);
                } else {
                    this.open_palette(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextFile, _, cx| {
                let targets = this.active_data().map(|d| d.file_rows.clone()).unwrap_or_default();
                this.jump_next(&targets, cx)
            }))
            .on_action(cx.listener(|this, _: &PrevFile, _, cx| {
                let targets = this.active_data().map(|d| d.file_rows.clone()).unwrap_or_default();
                this.jump_prev(&targets, cx)
            }))
            .on_action(cx.listener(|this, _: &NextHunk, _, cx| {
                let targets = this.active_data().map(|d| d.hunk_rows.clone()).unwrap_or_default();
                this.jump_next(&targets, cx)
            }))
            .on_action(cx.listener(|this, _: &PrevHunk, _, cx| {
                let targets = this.active_data().map(|d| d.hunk_rows.clone()).unwrap_or_default();
                this.jump_prev(&targets, cx)
            }))
            .on_action(cx.listener(|this, _: &GoToTop, _, cx| this.jump(0, cx)))
            .on_action(cx.listener(|this, _: &GoToBottom, _, cx| {
                if let Some(last) = this.active_data().map(|d| d.rows.len().saturating_sub(1)) {
                    this.jump(last, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleView, _, cx| this.toggle_view(cx)))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.refresh(cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar_visible = !this.sidebar_visible;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenInput, window, cx| {
                // The sidebar input can't take focus under the palette.
                this.palette = None;
                this.palette_gen += 1;
                this.sidebar_visible = true;
                this.open_input
                    .update(cx, |state, cx| state.focus(window, cx));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CloseItem, _, cx| {
                let active = this.active;
                this.close_item(active, cx)
            }))
            .on_action(cx.listener(|this, _: &NextItem, _, cx| this.cycle_items(1, cx)))
            .on_action(cx.listener(|this, _: &PrevItem, _, cx| this.cycle_items(-1, cx)))
            // The open input propagates Escape when it has nothing of its own
            // to dismiss: hand focus back to the diff.
            .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                window.focus(&this.focus_handle);
                cx.notify();
            }))
            .child(self.render_titlebar())
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .when(self.sidebar_visible, |main| {
                        main.child(self.render_sidebar(cx))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .key_context("ReviewApp")
                            .track_focus(&self.focus_handle)
                            .child(pane),
                    ),
            )
            .child(self.render_footer())
            .when(self.palette.is_some(), |root| {
                root.child(self.render_palette(cx))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diff_core::{FileDiff, Hunk};

    fn ctx(old_no: u32, new_no: u32, text: &str) -> DiffRow {
        DiffRow::Context {
            old_no,
            new_no,
            text: text.to_string(),
        }
    }

    fn add(new_no: u32, text: &str, intra: Vec<Range<usize>>) -> DiffRow {
        DiffRow::Added {
            new_no,
            text: text.to_string(),
            intra,
        }
    }

    fn rem(old_no: u32, text: &str, intra: Vec<Range<usize>>) -> DiffRow {
        DiffRow::Removed {
            old_no,
            text: text.to_string(),
            intra,
        }
    }

    fn hunk(old_start: u32, new_start: u32, rows: Vec<DiffRow>) -> Hunk {
        Hunk {
            old_start,
            old_count: 0,
            new_start,
            new_count: 0,
            section: String::new(),
            rows,
        }
    }

    fn sample_diff() -> PrDiff {
        PrDiff {
            files: vec![
                FileDiff {
                    old_path: Some("a.rs".into()),
                    new_path: Some("a.rs".into()),
                    status: FileStatus::Modified,
                    hunks: vec![
                        // Equal-count modified run, flanked by context.
                        hunk(
                            1,
                            1,
                            vec![
                                ctx(1, 1, "ctx"),
                                rem(2, "old1", vec![0..3]),
                                rem(3, "old2", Vec::new()),
                                add(2, "new1", vec![0..3]),
                                add(3, "new2", Vec::new()),
                                ctx(4, 4, "tail"),
                            ],
                        ),
                        // Unequal run (2 removed, 1 added) + a lone added run.
                        hunk(
                            10,
                            10,
                            vec![
                                rem(10, "r1", Vec::new()),
                                rem(11, "r2", Vec::new()),
                                add(10, "a1", Vec::new()),
                                ctx(12, 11, "c"),
                                add(12, "lone", Vec::new()),
                            ],
                        ),
                    ],
                    additions: 4,
                    deletions: 4,
                },
                FileDiff {
                    old_path: Some("b.png".into()),
                    new_path: Some("b.png".into()),
                    status: FileStatus::Binary,
                    hunks: Vec::new(),
                    additions: 0,
                    deletions: 0,
                },
            ],
        }
    }

    fn cell(cell: &Option<Cell>) -> (u32, LineKind, &str, &[Range<usize>]) {
        let cell = cell.as_ref().expect("expected a cell");
        (cell.no, cell.kind, cell.text.as_ref(), &cell.intra)
    }

    #[test]
    fn split_context_fills_both_cells() {
        let (rows, _, _) = build_rows(&sample_diff(), ViewMode::Split);
        // rows[0] = FileHeader, rows[1] = HunkHeader, rows[2] = first context.
        match &rows[2] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (1, LineKind::Context, "ctx", &[][..]));
                assert_eq!(cell(right), (1, LineKind::Context, "ctx", &[][..]));
            }
            _ => panic!("expected split line"),
        }
    }

    #[test]
    fn split_pairs_equal_runs_positionally() {
        let (rows, _, _) = build_rows(&sample_diff(), ViewMode::Split);
        match &rows[3] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (2, LineKind::Removed, "old1", &[0..3][..]));
                assert_eq!(cell(right), (2, LineKind::Added, "new1", &[0..3][..]));
            }
            _ => panic!("expected split line"),
        }
        match &rows[4] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (3, LineKind::Removed, "old2", &[][..]));
                assert_eq!(cell(right), (3, LineKind::Added, "new2", &[][..]));
            }
            _ => panic!("expected split line"),
        }
        // Equal run + 2 context rows: 4 split lines for a 6-row hunk.
        match &rows[5] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left).2, "tail");
                assert_eq!(cell(right).2, "tail");
            }
            _ => panic!("expected split line"),
        }
    }

    #[test]
    fn split_unequal_and_lone_runs_are_one_sided() {
        let (rows, _, hunk_rows) = build_rows(&sample_diff(), ViewMode::Split);
        let h2 = hunk_rows[1];
        // 2 removed / 1 added: first row paired, second left-only.
        match &rows[h2 + 1] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (10, LineKind::Removed, "r1", &[][..]));
                assert_eq!(cell(right), (10, LineKind::Added, "a1", &[][..]));
            }
            _ => panic!("expected split line"),
        }
        match &rows[h2 + 2] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (11, LineKind::Removed, "r2", &[][..]));
                assert!(right.is_none());
            }
            _ => panic!("expected split line"),
        }
        // Lone added run after context: right-only.
        match &rows[h2 + 4] {
            Row::SplitLine { left, right } => {
                assert!(left.is_none());
                assert_eq!(cell(right), (12, LineKind::Added, "lone", &[][..]));
            }
            _ => panic!("expected split line"),
        }
    }

    use syntax::Token;

    fn style(token: Token) -> HighlightStyle {
        theme::token_style(token)
    }

    fn style_bg(token: Option<Token>) -> HighlightStyle {
        let mut style = token.map(style).unwrap_or_default();
        style.background_color = Some(theme::added_word_bg().into());
        style
    }

    #[test]
    fn merge_syntax_only() {
        let syntax = [(0..2, Token::Keyword), (3..7, Token::Function)];
        assert_eq!(
            merge_highlights(&syntax, &[], None),
            vec![(0..2, style(Token::Keyword)), (3..7, style(Token::Function))]
        );
    }

    #[test]
    fn merge_intra_only() {
        let bg = Some(theme::added_word_bg());
        assert_eq!(
            merge_highlights(&[], &[2..5], bg),
            vec![(2..5, style_bg(None))]
        );
    }

    #[test]
    fn merge_partial_overlap() {
        let bg = Some(theme::added_word_bg());
        let syntax = [(0..6, Token::String)];
        assert_eq!(
            merge_highlights(&syntax, &[4..8], bg),
            vec![
                (0..4, style(Token::String)),
                (4..6, style_bg(Some(Token::String))),
                (6..8, style_bg(None)),
            ]
        );
    }

    #[test]
    fn merge_intra_spanning_multiple_tokens() {
        let bg = Some(theme::added_word_bg());
        let syntax = [(0..3, Token::Keyword), (5..8, Token::Number)];
        assert_eq!(
            merge_highlights(&syntax, &[1..7], bg),
            vec![
                (0..1, style(Token::Keyword)),
                (1..3, style_bg(Some(Token::Keyword))),
                (3..5, style_bg(None)),
                (5..7, style_bg(Some(Token::Number))),
                (7..8, style(Token::Number)),
            ]
        );
    }

    #[test]
    fn merge_adjacent_ranges() {
        // Same style across a shared boundary coalesces; different styles
        // stay split exactly at the boundary.
        let syntax = [(0..2, Token::Keyword), (2..4, Token::Keyword)];
        assert_eq!(
            merge_highlights(&syntax, &[], None),
            vec![(0..4, style(Token::Keyword))]
        );
        let syntax = [(0..2, Token::Keyword), (2..4, Token::Type)];
        assert_eq!(
            merge_highlights(&syntax, &[], None),
            vec![(0..2, style(Token::Keyword)), (2..4, style(Token::Type))]
        );
        let bg = Some(theme::added_word_bg());
        assert_eq!(
            merge_highlights(&[], &[0..2, 2..4], bg),
            vec![(0..4, style_bg(None))]
        );
    }

    #[test]
    fn hunk_syntax_takes_spans_from_the_right_side() {
        let lang = syntax::language_for_path("x.rs");
        let rows = vec![
            ctx(1, 1, "fn f() {"),
            rem(2, "// gone", Vec::new()),
            add(2, "    let b = 2;", Vec::new()),
            ctx(3, 3, "}"),
        ];
        let spans = hunk_syntax(lang, &rows);
        assert_eq!(spans.len(), 4);
        // Context: from the new side.
        assert!(spans[0].contains(&(0..2, Token::Keyword)));
        // Removed: highlighted as part of old_source — a comment.
        assert_eq!(spans[1], vec![(0..7, Token::Comment)]);
        // Added: highlighted as part of new_source — `let` keyword.
        assert!(spans[2].contains(&(4..7, Token::Keyword)));
    }

    #[test]
    fn hunk_syntax_guardrails() {
        let rows = vec![ctx(1, 1, "fn f() {}")];
        // No language → no spans.
        assert_eq!(hunk_syntax(None, &rows), vec![Vec::new()]);
        // Over-long line stays plain even when the hunk is highlighted.
        let lang = syntax::language_for_path("x.rs");
        let long = format!("// {}", "x".repeat(5000));
        let rows = vec![ctx(1, 1, "fn f() {}"), ctx(2, 2, &long)];
        let spans = hunk_syntax(lang, &rows);
        assert!(!spans[0].is_empty());
        assert!(spans[1].is_empty());
    }

    fn pr(number: u64, title: &str, author: &str, branch: &str) -> gh::PrSummary {
        gh::PrSummary {
            number,
            title: title.to_string(),
            author: gh::Author {
                login: author.to_string(),
            },
            state: "OPEN".to_string(),
            is_draft: false,
            head_ref_name: branch.to_string(),
            updated_at: "2026-07-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn pr_filter_empty_query_keeps_original_order() {
        let all = vec![
            pr(3, "fix crash", "alice", "fix-crash"),
            pr(1, "add feature", "bob", "feat"),
            pr(2, "docs", "carol", "docs"),
        ];
        assert_eq!(filter_prs(&all, ""), vec![0, 1, 2]);
        assert_eq!(filter_prs(&all, "   "), vec![0, 1, 2]);
    }

    #[test]
    fn pr_filter_matches_number_title_author_and_branch() {
        let all = vec![
            pr(3, "fix crash", "alice", "fix-crash"),
            pr(1, "add feature", "bob", "feat"),
        ];
        assert_eq!(filter_prs(&all, "#3"), vec![0]);
        assert_eq!(filter_prs(&all, "bob"), vec![1]);
        assert_eq!(filter_prs(&all, "crash"), vec![0]);
        assert!(filter_prs(&all, "zzzqqq").is_empty());
    }

    #[test]
    fn repo_slug_parsing() {
        assert_eq!(
            parse_repo_slug("BurntSushi/ripgrep"),
            Ok(("BurntSushi".to_string(), "ripgrep".to_string()))
        );
        assert!(parse_repo_slug("ripgrep").is_err());
        assert!(parse_repo_slug("/ripgrep").is_err());
        assert!(parse_repo_slug("BurntSushi/").is_err());
        assert!(parse_repo_slug("a/b/c").is_err());
    }

    #[test]
    fn source_options_filter_by_substring() {
        assert_eq!(filtered_sources(""), vec![0, 1]);
        assert_eq!(filtered_sources("pull"), vec![0]);
        assert_eq!(filtered_sources("FOLDER"), vec![1]);
        assert_eq!(filtered_sources("open"), vec![0, 1]);
        assert!(filtered_sources("nope").is_empty());
    }

    #[test]
    fn header_indices_are_correct_in_both_modes() {
        let diff = sample_diff();
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let (rows, file_rows, hunk_rows) = build_rows(&diff, mode);
            assert_eq!(file_rows.len(), 2);
            assert_eq!(hunk_rows.len(), 2);
            for &ix in &file_rows {
                assert!(matches!(rows[ix], Row::FileHeader { .. }));
            }
            for &ix in &hunk_rows {
                assert!(matches!(rows[ix], Row::HunkHeader { .. }));
            }
            // Binary file: header immediately followed by the binary row.
            assert!(matches!(rows[file_rows[1] + 1], Row::Binary));
        }
        // Unified emits one row per diff row; split collapses the equal run.
        let (unified, _, _) = build_rows(&diff, ViewMode::Unified);
        let (split, _, _) = build_rows(&diff, ViewMode::Split);
        let unified_lines = unified.iter().filter(|r| matches!(r, Row::Line { .. })).count();
        let split_lines = split
            .iter()
            .filter(|r| matches!(r, Row::SplitLine { .. }))
            .count();
        assert_eq!(unified_lines, 11);
        assert_eq!(split_lines, 8); // 4 (hunk 1) + 4 (hunk 2)
    }
}
