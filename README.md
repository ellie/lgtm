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

### Build from source

```sh
cargo run --release
```

GPUI supports macOS and Linux natively. On Linux you'll need a few system libraries:

- Debian/Ubuntu: `sudo apt install libxcb1 libxkbcommon-x11-0 libfontconfig1`
- Fedora: `sudo dnf install libxcb libxkbcommon-x11 fontconfig`
- Arch: `sudo pacman -S libxcb libxkbcommon-x11 fontconfig`

The monospace font falls back from Menlo (macOS) to DejaVu Sans Mono on Linux automatically. Modifier keys also remap: `cmd-*` on macOS, `ctrl-*` on Linux.

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

