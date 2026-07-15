# lgtm

![](./screenies/lgtm.jpeg)

A fast, native code-review app in Rust, built with [gpui](https://www.gpui.rs/).

Requires the [GitHub CLI](https://cli.github.com/) (`gh auth login` first), otherwise you can only review code locally.

This is 100% vibe coded. I have not read the code. 

## Setup

Grab [LGTM.dmg](https://github.com/ellie/lgtm/releases/download/latest/LGTM.dmg) from the latest release (built from every commit on main, Apple Silicon only), open it, and drag LGTM to Applications.

The app is unsigned, so on first launch macOS will complain — right-click the app and choose Open, or:

```sh
xattr -d com.apple.quarantine /Applications/LGTM.app
```

Or build from source:

```sh
cargo run --release
```

Note that you will need Xcode to build it from source because the metal dev
tools are bundled with it. Otherwise the build fails with the following error:

```
cargo:rerun-if-changed=./src/platform/mac/shaders.metal
cargo::error=metal shader compilation failed:
xcrun: error: unable to find utility "metal", not a developer tool or in PATH
```

## Features
- unified + split views
- tree-sitter highlighting (18 languages),
- word-level intra-line diffs, 
- multi-item sidebar with file tree
- cmd-k palette with fuzzy PR picker, 
- local repo diffs
- minimap,
- inline GitHub review comments (reading + posting, hover a line for +)
- submitting reviews (approve / request changes / comment, `cmd-enter` or the Review button)

Coming: LSP, AI inline review annotations


## Keymap
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
| `c` | toggle inline comments |
| `cmd-j` | chat with Claude Code |
| `r` | refresh active item |
| `cmd-enter` | submit review (approve / request changes / comment) |
| `home` / `end` | top / bottom |
| `cmd-c` | copy selection |
| `cmd-q` | quit |

