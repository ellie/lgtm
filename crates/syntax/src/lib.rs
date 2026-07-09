//! UI-free tree-sitter syntax highlighting: language detection by file
//! extension, and per-line token spans over standalone source text (hunk text
//! in patch-only mode, whole files later). No gpui dependencies — mapping
//! tokens to colors is the app's job.

use std::ops::Range;
use std::sync::OnceLock;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// Semantic token classes, deliberately coarse: capture names from each
/// grammar's bundled highlight query are folded onto these via longest-prefix
/// matching (tree-sitter-highlight's own resolution rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    Keyword,
    Function,
    Type,
    String,
    Number,
    Comment,
    Constant,
    Property,
    Variable,
    Parameter,
    Operator,
    Punctuation,
    Attribute,
    Namespace,
    Label,
    Embedded,
}

/// Recognized capture names and the token each maps to. tree-sitter-highlight
/// matches a query's capture names against this list by longest dot-separated
/// prefix, so e.g. `function.builtin` and `function.method` land on
/// `function`, while `string.special.symbol` (Elixir atoms) gets its own
/// entry. Bare `variable` is deliberately absent: plain variables render as
/// default text, and dropping the capture keeps highlight lists small.
const CAPTURES: &[(&str, Token)] = &[
    ("attribute", Token::Attribute),
    ("boolean", Token::Constant),
    ("charset", Token::Keyword),
    ("comment", Token::Comment),
    ("constant", Token::Constant),
    ("constructor", Token::Type),
    ("delimiter", Token::Punctuation),
    ("embedded", Token::Embedded),
    ("escape", Token::Constant),
    ("function", Token::Function),
    ("import", Token::Keyword),
    ("keyframes", Token::Keyword),
    ("keyword", Token::Keyword),
    ("label", Token::Label),
    ("media", Token::Keyword),
    ("module", Token::Namespace),
    ("namespace", Token::Namespace),
    ("number", Token::Number),
    ("operator", Token::Operator),
    ("property", Token::Property),
    ("punctuation", Token::Punctuation),
    ("string", Token::String),
    ("string.escape", Token::Constant),
    ("string.special", Token::String),
    ("string.special.symbol", Token::Constant),
    ("supports", Token::Keyword),
    ("tag", Token::Function),
    ("type", Token::Type),
    ("variable.builtin", Token::Constant),
    ("variable.parameter", Token::Parameter),
];

/// A lazily-built highlight configuration for one language. Building compiles
/// the grammar's highlight query, so it happens once, on first use.
pub struct Language {
    config: OnceLock<Option<HighlightConfiguration>>,
    build: fn() -> Option<HighlightConfiguration>,
}

impl Language {
    const fn new(build: fn() -> Option<HighlightConfiguration>) -> Self {
        Self {
            config: OnceLock::new(),
            build,
        }
    }

    fn config(&self) -> Option<&HighlightConfiguration> {
        self.config
            .get_or_init(|| {
                let mut config = (self.build)()?;
                let names: Vec<&str> = CAPTURES.iter().map(|&(name, _)| name).collect();
                config.configure(&names);
                Some(config)
            })
            .as_ref()
    }
}

// Injection queries are skipped everywhere (we pass no injection callback, so
// embedded languages — doc-comment markdown, Elixir inside HEEx, JS inside
// HTML — render as plain text). Locals queries are included where the grammar
// crate bundles one; they make `variable.parameter` accurate.

static RUST: Language = Language::new(|| {
    HighlightConfiguration::new(
        tree_sitter_rust::LANGUAGE.into(),
        "rust",
        tree_sitter_rust::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .ok()
});

static GLEAM: Language = Language::new(|| {
    HighlightConfiguration::new(
        tree_sitter_gleam::LANGUAGE.into(),
        "gleam",
        tree_sitter_gleam::HIGHLIGHTS_QUERY,
        "",
        tree_sitter_gleam::LOCALS_QUERY,
    )
    .ok()
});

static ELIXIR: Language = Language::new(|| {
    HighlightConfiguration::new(
        tree_sitter_elixir::LANGUAGE.into(),
        "elixir",
        tree_sitter_elixir::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .ok()
});

static HEEX: Language = Language::new(|| {
    HighlightConfiguration::new(
        tree_sitter_heex::LANGUAGE.into(),
        "heex",
        tree_sitter_heex::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .ok()
});

static JAVASCRIPT: Language = Language::new(|| {
    // JSX patterns first: earlier patterns win in tree-sitter-highlight, and
    // this matches the upstream query order. Harmless for plain .js.
    let highlights = format!(
        "{}{}",
        tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
        tree_sitter_javascript::HIGHLIGHT_QUERY
    );
    HighlightConfiguration::new(
        tree_sitter_javascript::LANGUAGE.into(),
        "javascript",
        &highlights,
        "",
        tree_sitter_javascript::LOCALS_QUERY,
    )
    .ok()
});

static TYPESCRIPT: Language = Language::new(|| {
    let highlights = format!(
        "{}{}",
        tree_sitter_typescript::HIGHLIGHTS_QUERY,
        tree_sitter_javascript::HIGHLIGHT_QUERY
    );
    let locals = format!(
        "{}{}",
        tree_sitter_typescript::LOCALS_QUERY,
        tree_sitter_javascript::LOCALS_QUERY
    );
    HighlightConfiguration::new(
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "typescript",
        &highlights,
        "",
        &locals,
    )
    .ok()
});

static TSX: Language = Language::new(|| {
    let highlights = format!(
        "{}{}{}",
        tree_sitter_typescript::HIGHLIGHTS_QUERY,
        tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
        tree_sitter_javascript::HIGHLIGHT_QUERY
    );
    let locals = format!(
        "{}{}",
        tree_sitter_typescript::LOCALS_QUERY,
        tree_sitter_javascript::LOCALS_QUERY
    );
    HighlightConfiguration::new(
        tree_sitter_typescript::LANGUAGE_TSX.into(),
        "tsx",
        &highlights,
        "",
        &locals,
    )
    .ok()
});

static PYTHON: Language = Language::new(|| {
    HighlightConfiguration::new(
        tree_sitter_python::LANGUAGE.into(),
        "python",
        tree_sitter_python::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .ok()
});

static GO: Language = Language::new(|| {
    HighlightConfiguration::new(
        tree_sitter_go::LANGUAGE.into(),
        "go",
        tree_sitter_go::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .ok()
});

static C: Language = Language::new(|| {
    HighlightConfiguration::new(
        tree_sitter_c::LANGUAGE.into(),
        "c",
        tree_sitter_c::HIGHLIGHT_QUERY,
        "",
        "",
    )
    .ok()
});

static CPP: Language = Language::new(|| {
    let highlights = format!(
        "{}{}",
        tree_sitter_cpp::HIGHLIGHT_QUERY,
        tree_sitter_c::HIGHLIGHT_QUERY
    );
    HighlightConfiguration::new(
        tree_sitter_cpp::LANGUAGE.into(),
        "cpp",
        &highlights,
        "",
        "",
    )
    .ok()
});

static JAVA: Language = Language::new(|| {
    HighlightConfiguration::new(
        tree_sitter_java::LANGUAGE.into(),
        "java",
        tree_sitter_java::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .ok()
});

static RUBY: Language = Language::new(|| {
    HighlightConfiguration::new(
        tree_sitter_ruby::LANGUAGE.into(),
        "ruby",
        tree_sitter_ruby::HIGHLIGHTS_QUERY,
        "",
        tree_sitter_ruby::LOCALS_QUERY,
    )
    .ok()
});

static BASH: Language = Language::new(|| {
    HighlightConfiguration::new(
        tree_sitter_bash::LANGUAGE.into(),
        "bash",
        tree_sitter_bash::HIGHLIGHT_QUERY,
        "",
        "",
    )
    .ok()
});

static JSON: Language = Language::new(|| {
    HighlightConfiguration::new(
        tree_sitter_json::LANGUAGE.into(),
        "json",
        tree_sitter_json::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .ok()
});

static TOML: Language = Language::new(|| {
    HighlightConfiguration::new(
        tree_sitter_toml_ng::LANGUAGE.into(),
        "toml",
        tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .ok()
});

static YAML: Language = Language::new(|| {
    HighlightConfiguration::new(
        tree_sitter_yaml::LANGUAGE.into(),
        "yaml",
        tree_sitter_yaml::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .ok()
});

static HTML: Language = Language::new(|| {
    HighlightConfiguration::new(
        tree_sitter_html::LANGUAGE.into(),
        "html",
        tree_sitter_html::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .ok()
});

static CSS: Language = Language::new(|| {
    HighlightConfiguration::new(
        tree_sitter_css::LANGUAGE.into(),
        "css",
        tree_sitter_css::HIGHLIGHTS_QUERY,
        "",
        "",
    )
    .ok()
});

/// Resolve a language from a path's extension. `None` means "render plain".
pub fn language_for_path(path: &str) -> Option<&'static Language> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let (_, ext) = name.rsplit_once('.')?;
    let lang = match ext.to_ascii_lowercase().as_str() {
        "rs" => &RUST,
        "gleam" => &GLEAM,
        "ex" | "exs" => &ELIXIR,
        "heex" => &HEEX,
        "js" | "mjs" | "cjs" | "jsx" => &JAVASCRIPT,
        "ts" | "mts" | "cts" => &TYPESCRIPT,
        "tsx" => &TSX,
        "py" | "pyi" => &PYTHON,
        "go" => &GO,
        "c" | "h" => &C,
        "cc" | "cpp" | "cxx" | "c++" | "hh" | "hpp" | "hxx" => &CPP,
        "java" => &JAVA,
        "rb" | "rake" | "gemspec" => &RUBY,
        "sh" | "bash" | "zsh" => &BASH,
        "json" => &JSON,
        "toml" => &TOML,
        "yml" | "yaml" => &YAML,
        "html" | "htm" => &HTML,
        "css" => &CSS,
        _ => return None,
    };
    Some(lang)
}

/// Highlight `source` standalone and return, per line, sorted non-overlapping
/// byte-range → token spans with ranges relative to that line (newline
/// excluded). Ranges come from tree-sitter byte offsets, so they always sit on
/// UTF-8 character boundaries. Errors (bad query at build time, parse-level
/// failures) degrade to empty spans, never panic.
pub fn highlight_lines(lang: &Language, source: &str) -> Vec<Vec<(Range<usize>, Token)>> {
    let mut line_starts = vec![0usize];
    for (ix, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            line_starts.push(ix + 1);
        }
    }
    let mut out: Vec<Vec<(Range<usize>, Token)>> = vec![Vec::new(); line_starts.len()];
    let Some(config) = lang.config() else {
        return out;
    };
    let mut highlighter = Highlighter::new();
    let Ok(events) = highlighter.highlight(config, source.as_bytes(), None, |_| None) else {
        return out;
    };
    // End of a line's content (its newline excluded).
    let line_end = |l: usize| line_starts.get(l + 1).map_or(source.len(), |&next| next - 1);

    let mut stack: Vec<Token> = Vec::new();
    let mut line = 0usize;
    for event in events {
        let Ok(event) = event else { break };
        match event {
            HighlightEvent::HighlightStart(h) => stack.push(CAPTURES[h.0].1),
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                let Some(&token) = stack.last() else { continue };
                // Source events arrive in order, so only ever advance.
                while line + 1 < line_starts.len() && line_starts[line + 1] <= start {
                    line += 1;
                }
                let mut l = line;
                loop {
                    let ls = line_starts[l];
                    let le = line_end(l);
                    let seg = start.max(ls)..end.min(le);
                    if seg.start < seg.end {
                        let rel = seg.start - ls..seg.end - ls;
                        // Coalesce with an adjacent same-token span.
                        match out[l].last_mut() {
                            Some((prev, t)) if *t == token && prev.end == rel.start => {
                                prev.end = rel.end
                            }
                            _ => out[l].push((rel, token)),
                        }
                    }
                    // Done once the span ends by this line's newline.
                    if end <= le + 1 || l + 1 >= line_starts.len() {
                        break;
                    }
                    l += 1;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_languages_build() {
        let all: &[(&str, &Language)] = &[
            ("rust", &RUST),
            ("gleam", &GLEAM),
            ("elixir", &ELIXIR),
            ("heex", &HEEX),
            ("javascript", &JAVASCRIPT),
            ("typescript", &TYPESCRIPT),
            ("tsx", &TSX),
            ("python", &PYTHON),
            ("go", &GO),
            ("c", &C),
            ("cpp", &CPP),
            ("java", &JAVA),
            ("ruby", &RUBY),
            ("bash", &BASH),
            ("json", &JSON),
            ("toml", &TOML),
            ("yaml", &YAML),
            ("html", &HTML),
            ("css", &CSS),
        ];
        for (name, lang) in all {
            assert!(lang.config().is_some(), "{name} config failed to build");
        }
    }

    #[test]
    fn detects_language_by_extension() {
        assert!(language_for_path("crates/app/src/main.rs").is_some());
        assert!(language_for_path("lib/foo.gleam").is_some());
        assert!(language_for_path("lib/foo/bar.ex").is_some());
        assert!(language_for_path("mix.EXS").is_some()); // case-insensitive
        assert!(language_for_path("index.d.ts").is_some());
        assert!(language_for_path("Makefile").is_none());
        assert!(language_for_path("logo.png").is_none());
        assert!(language_for_path(".gitignore").is_none());
    }

    fn spans_at(lines: &[Vec<(Range<usize>, Token)>], line: usize) -> &[(Range<usize>, Token)] {
        &lines[line]
    }

    #[test]
    fn rust_tokens_land_on_the_right_lines() {
        let lang = language_for_path("x.rs").unwrap();
        let lines = highlight_lines(lang, "fn main() {\n    let x = 1; // hi\n}");
        assert_eq!(lines.len(), 3);
        // Line 0: `fn` keyword, `main` function.
        assert!(spans_at(&lines, 0).contains(&(0..2, Token::Keyword)));
        assert!(spans_at(&lines, 0).contains(&(3..7, Token::Function)));
        // Line 1: `let` keyword, `1` (rust query: @constant.builtin), comment.
        assert!(spans_at(&lines, 1).contains(&(4..7, Token::Keyword)));
        assert!(spans_at(&lines, 1).contains(&(12..13, Token::Constant)));
        assert!(spans_at(&lines, 1).contains(&(15..20, Token::Comment)));
    }

    #[test]
    fn spans_are_sorted_nonoverlapping_and_line_relative() {
        let lang = language_for_path("x.rs").unwrap();
        let src = "/* a\nmulti-line comment\n*/ fn f() {}\n";
        let lines = highlight_lines(lang, src);
        // The block comment spans lines 0..=2, clipped per line.
        assert_eq!(lines[0], vec![(0..4, Token::Comment)]);
        assert_eq!(lines[1], vec![(0..18, Token::Comment)]);
        assert_eq!(lines[1][0].1, Token::Comment);
        for line in &lines {
            for pair in line.windows(2) {
                assert!(pair[0].0.end <= pair[1].0.start, "overlapping spans");
            }
        }
    }

    #[test]
    fn gleam_and_elixir_highlight() {
        let gleam = language_for_path("x.gleam").unwrap();
        let lines = highlight_lines(gleam, "pub fn add(a: Int, b: Int) -> Int {\n  a + b\n}");
        assert!(lines[0].iter().any(|&(_, t)| t == Token::Keyword));
        assert!(lines[0].iter().any(|&(_, t)| t == Token::Type));

        let elixir = language_for_path("x.ex").unwrap();
        let lines = highlight_lines(elixir, "defmodule Foo do\n  def bar, do: :ok\nend");
        assert!(lines[0].iter().any(|&(_, t)| t == Token::Keyword));
        assert!(lines[0].iter().any(|&(_, t)| t == Token::Namespace));
        // :ok is an atom → string.special.symbol → Constant.
        assert!(lines[1].iter().any(|&(_, t)| t == Token::Constant));
    }

    #[test]
    fn plain_variables_emit_no_spans() {
        let lang = language_for_path("x.rs").unwrap();
        let lines = highlight_lines(lang, "fn f(x: u32) -> u32 { x }");
        assert!(!lines[0].iter().any(|&(_, t)| t == Token::Variable));
    }

    #[test]
    fn empty_source_is_one_empty_line() {
        let lang = language_for_path("x.rs").unwrap();
        assert_eq!(highlight_lines(lang, ""), vec![Vec::new()]);
    }
}
