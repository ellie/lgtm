mod theme;

use diff_core::{DiffRow, FileStatus, PrDiff};
use gpui::{
    actions, div, prelude::*, px, size, uniform_list, App, Application, Bounds, Context,
    FocusHandle, HighlightStyle, Hsla, KeyBinding, Keystroke, ListHorizontalSizingBehavior,
    ScrollStrategy, SharedString, StyledText, TitlebarOptions, UniformListScrollHandle, Window,
    WindowBounds, WindowOptions,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    kbd::Kbd,
    scroll::Scrollbar,
    tag::Tag,
    IconName, Root, Sizable as _, TitleBar,
};
use std::ops::Range;

const MONO: &str = "Menlo";
const ROW_HEIGHT: f32 = 22.0;
const TEXT_SIZE: f32 = 13.0;

actions!(review, [NextFile, PrevFile, NextHunk, PrevHunk, GoToTop, GoToBottom, ToggleView, Quit]);

fn main() {
    let arg = match std::env::args().nth(1) {
        Some(arg) => arg,
        None => {
            eprintln!("usage: review <owner/repo#123 | PR URL | PR number>");
            std::process::exit(2);
        }
    };
    let locator = gh::resolve_pr_arg(&arg).unwrap_or_else(die);
    eprintln!(
        "fetching {}#{} via gh...",
        locator.repo_slug(),
        locator.number
    );
    let meta_loc = locator.clone();
    let meta_thread = std::thread::spawn(move || gh::fetch_meta(&meta_loc));
    let patch = gh::fetch_patch(&locator).unwrap_or_else(die);
    let meta = meta_thread.join().unwrap().unwrap_or_else(die);
    let diff = diff_core::parse_patch(&patch);

    let window_title: SharedString = format!(
        "review — {}#{}: {}",
        locator.repo_slug(),
        locator.number,
        meta.title
    )
    .into();

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
                KeyBinding::new("cmd-q", Quit, None),
            ]);
            cx.on_action(|_: &Quit, cx| cx.quit());

            let bounds = Bounds::centered(None, size(px(1280.), px(860.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some(window_title.clone()),
                        ..TitleBar::title_bar_options()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| ReviewApp::new(meta, diff, cx));
                    window.focus(&view.read(cx).focus_handle);
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )
            .unwrap();
            cx.activate(true);
        });
}

fn die<E: std::fmt::Display, T>(err: E) -> T {
    eprintln!("error: {err}");
    std::process::exit(1);
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

/// One side of a split row: line number, kind, text, and word-level highlights.
struct Cell {
    no: u32,
    kind: LineKind,
    text: SharedString,
    intra: Vec<Range<usize>>,
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
    },
    SplitLine {
        left: Option<Cell>,
        right: Option<Cell>,
    },
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
        for hunk in &file.hunks {
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
                    for row in &hunk.rows {
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
                                rows.push(Row::SplitLine {
                                    left: Some(Cell {
                                        no: *old_no,
                                        kind: LineKind::Context,
                                        text: text.clone(),
                                        intra: Vec::new(),
                                    }),
                                    right: Some(Cell {
                                        no: *new_no,
                                        kind: LineKind::Context,
                                        text,
                                        intra: Vec::new(),
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

/// Line text with word-level highlight ranges, shared by unified rows and
/// split cells.
fn line_content(
    text: &SharedString,
    intra: &[Range<usize>],
    word_bg: Option<gpui::Rgba>,
) -> gpui::AnyElement {
    if intra.is_empty() {
        div().child(text.clone()).into_any_element()
    } else {
        let highlights = intra.iter().map(|range| {
            (
                range.clone(),
                HighlightStyle {
                    background_color: Some(word_bg.unwrap().into()),
                    ..Default::default()
                },
            )
        });
        StyledText::new(text.clone())
            .with_highlights(highlights)
            .into_any_element()
    }
}

struct ReviewApp {
    meta: gh::PrMeta,
    diff: PrDiff,
    mode: ViewMode,
    rows: Vec<Row>,
    file_rows: Vec<usize>,
    hunk_rows: Vec<usize>,
    cursor: usize,
    scroll: UniformListScrollHandle,
    focus_handle: FocusHandle,
}

impl ReviewApp {
    fn new(meta: gh::PrMeta, diff: PrDiff, cx: &mut Context<Self>) -> Self {
        let mode = ViewMode::Unified;
        let (rows, file_rows, hunk_rows) = build_rows(&diff, mode);
        Self {
            meta,
            diff,
            mode,
            rows,
            file_rows,
            hunk_rows,
            cursor: 0,
            scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
        }
    }

    fn toggle_view(&mut self, cx: &mut Context<Self>) {
        // Best-effort position preservation: stay on the same file.
        let file_pos = self.file_rows.iter().rposition(|&ix| ix <= self.cursor);
        self.mode = match self.mode {
            ViewMode::Unified => ViewMode::Split,
            ViewMode::Split => ViewMode::Unified,
        };
        let (rows, file_rows, hunk_rows) = build_rows(&self.diff, self.mode);
        self.rows = rows;
        self.file_rows = file_rows;
        self.hunk_rows = hunk_rows;
        let target = file_pos
            .and_then(|pos| self.file_rows.get(pos).copied())
            .unwrap_or(0);
        self.jump(target, cx);
    }

    fn jump(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.cursor = ix;
        self.scroll.scroll_to_item_strict(ix, ScrollStrategy::Top);
        cx.notify();
    }

    fn jump_next(&mut self, targets: &[usize], cx: &mut Context<Self>) {
        if let Some(&ix) = targets.iter().find(|&&ix| ix > self.cursor) {
            self.jump(ix, cx);
        }
    }

    fn jump_prev(&mut self, targets: &[usize], cx: &mut Context<Self>) {
        if let Some(&ix) = targets.iter().rev().find(|&&ix| ix < self.cursor) {
            self.jump(ix, cx);
        }
    }

    fn render_row(&self, ix: usize) -> gpui::AnyElement {
        let row_height = px(ROW_HEIGHT);
        match &self.rows[ix] {
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
                    .child(div().whitespace_nowrap().child(line_content(text, intra, word_bg)))
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
                        return base.bg(theme::mantle());
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
                            .child(line_content(&cell.text, &cell.intra, word_bg)),
                    )
                };
                div()
                    .h(row_height)
                    .flex()
                    .child(cell(left))
                    .child(
                        cell(right)
                            .border_l_1()
                            .border_color(theme::surface0()),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_titlebar(&self) -> impl IntoElement {
        let meta = &self.meta;
        let (state_color, state_label) = match meta.state.as_str() {
            "OPEN" => (theme::green(), "open"),
            "MERGED" => (theme::mauve(), "merged"),
            "CLOSED" => (theme::red(), "closed"),
            other => (theme::overlay0(), other),
        };
        let state: Hsla = state_color.into();
        let url = meta.url.clone();
        TitleBar::new()
            .text_size(px(13.))
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
    }
}

impl Render for ReviewApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::base())
            .text_color(theme::text())
            .track_focus(&self.focus_handle)
            .key_context("ReviewApp")
            .on_action(cx.listener(|this, _: &NextFile, _, cx| {
                let targets = this.file_rows.clone();
                this.jump_next(&targets, cx)
            }))
            .on_action(cx.listener(|this, _: &PrevFile, _, cx| {
                let targets = this.file_rows.clone();
                this.jump_prev(&targets, cx)
            }))
            .on_action(cx.listener(|this, _: &NextHunk, _, cx| {
                let targets = this.hunk_rows.clone();
                this.jump_next(&targets, cx)
            }))
            .on_action(cx.listener(|this, _: &PrevHunk, _, cx| {
                let targets = this.hunk_rows.clone();
                this.jump_prev(&targets, cx)
            }))
            .on_action(cx.listener(|this, _: &GoToTop, _, cx| this.jump(0, cx)))
            .on_action(cx.listener(|this, _: &GoToBottom, _, cx| {
                let last = this.rows.len().saturating_sub(1);
                this.jump(last, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleView, _, cx| this.toggle_view(cx)))
            .child(self.render_titlebar())
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .font_family(MONO)
                    .text_size(px(TEXT_SIZE))
                    .line_height(px(ROW_HEIGHT))
                    .child(
                        uniform_list("diff", self.rows.len(), move |range, _window, cx| {
                            let this = entity.read(cx);
                            range.map(|ix| this.render_row(ix)).collect()
                        })
                        .track_scroll(self.scroll.clone())
                        .with_horizontal_sizing_behavior(match self.mode {
                            ViewMode::Unified => ListHorizontalSizingBehavior::Unconstrained,
                            ViewMode::Split => ListHorizontalSizingBehavior::FitList,
                        })
                        .h_full(),
                    )
                    .child(Scrollbar::new(&self.scroll)),
            )
            .child(self.render_footer())
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
