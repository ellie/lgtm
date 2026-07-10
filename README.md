# review

A fast, native code-review app in Rust on [gpui](https://www.gpui.rs/).
See [DESIGN.md](DESIGN.md) for the full design.

## Usage

Requires the [GitHub CLI](https://cli.github.com/) (`gh auth login` first).

```
review owner/repo#123
review 123                      # repo inferred from the cwd's git remote
review https://github.com/owner/repo/pull/123
```

Open several things at once: `review owner/repo#123 other/repo#7 .`
A bare path (or no args inside a git repo) shows the local pre-push
preview — working tree vs merge-base with the default branch.

## Keys

| Key | Action |
|---|---|
| `cmd-k` | open palette (GitHub PR picker / folder) |
| `cmd-t` / `cmd-w` / `cmd-b` | quick-open input / close item / toggle sidebar |
| `ctrl-tab` | cycle open items |
| `]` / `[` | next / previous file |
| `n` / `p` | next / previous hunk |
| `v` | unified ↔ split view |
| `/` | fuzzy file filter |
| `m` | toggle minimap |
| `r` | refresh active item |
| `home` / `end` | top / bottom |
| `cmd-c` | copy selection |
| `cmd-q` | quit |

## Status

Working: unified + split views, tree-sitter highlighting (18 languages),
word-level intra-line diffs, multi-item sidebar with file tree, cmd-k
palette with fuzzy PR picker, local repo diffs, full-content blob
upgrade with expand-context and offline cache, text selection, minimap.
Next: Helix theme loading, then AI/LSP — see DESIGN.md.
