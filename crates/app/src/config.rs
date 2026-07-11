//! User configuration. Loaded once at startup from `~/.config/lgtm/config.toml`.
//!
//! The file is optional. A missing file is fine and produces the default
//! config; a malformed file prints a warning to stderr and also falls back to
//! the default (rather than refusing to start). The config struct is small
//! today — adding new keys only means adding a field with `#[serde(default)]`
//! and a default value, so old config files keep loading.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    pub font: FontConfig,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct FontConfig {
    /// Monospace font family used for the diff pane, the file-tree gutter,
    /// and the footer hint labels. Empty string (the default) means
    /// "use the per-OS default" (`Menlo` on macOS, `DejaVu Sans Mono` on
    /// Linux).
    pub mono_family: String,
    /// Diff pane text size in pixels. Empty config / `None` falls back to
    /// the built-in default (13 px).
    pub mono_text_size: Option<f32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font: FontConfig {
                mono_family: String::new(),
                mono_text_size: None,
            },
        }
    }
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            mono_family: String::new(),
            mono_text_size: None,
        }
    }
}

impl Config {
    /// Read `~/.config/lgtm/config.toml`. Missing file returns the default
    /// config silently. A parse error prints to stderr and also returns the
    /// default — the user can fix the file and relaunch without losing any
    /// state.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            // ENOENT and friends: not having a config file is normal.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(err) => {
                eprintln!("lgtm: could not read {}: {err}", path.display());
                return Self::default();
            }
        };
        let text = match std::str::from_utf8(&bytes) {
            Ok(t) => t,
            Err(err) => {
                eprintln!("lgtm: {} is not valid UTF-8: {err}", path.display());
                return Self::default();
            }
        };
        match toml::from_str(text) {
            Ok(cfg) => cfg,
            Err(err) => {
                eprintln!("lgtm: could not parse {}: {err}", path.display());
                Self::default()
            }
        }
    }
}

/// `~/.config/lgtm/config.toml`, or `$XDG_CONFIG_HOME/lgtm/config.toml` when
/// that variable is set. None means HOME is unset (very unusual) — we just
/// run with defaults.
fn config_path() -> Option<std::path::PathBuf> {
    let mut dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    dir.push("lgtm");
    dir.push("config.toml");
    Some(dir)
}

/// The default monospace family when the config doesn't specify one. macOS
/// ships Menlo by default; on Linux we ask for DejaVu Sans Mono (almost
/// always present) and let fontconfig fall back to whatever monospace is
/// installed.
pub fn default_mono_family() -> &'static str {
    if cfg!(target_os = "macos") {
        "Menlo"
    } else {
        "DejaVu Sans Mono"
    }
}

pub fn default_mono_text_size() -> f32 {
    13.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_uses_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.font.mono_family, "");
        assert_eq!(cfg.font.mono_text_size, None);
    }

    #[test]
    fn parses_font_section() {
        let cfg: Config = toml::from_str(
            r#"
            [font]
            mono_family = "JetBrains Mono"
            mono_text_size = 14.0
            "#,
        )
        .unwrap();
        assert_eq!(cfg.font.mono_family, "JetBrains Mono");
        assert_eq!(cfg.font.mono_text_size, Some(14.0));
    }

    #[test]
    fn missing_sections_use_defaults() {
        let cfg: Config = toml::from_str(
            r#"
            [font]
            mono_family = "Iosevka"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.font.mono_family, "Iosevka");
        assert_eq!(cfg.font.mono_text_size, None);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        // Forward-compat: users on a newer version shouldn't fail to load an
        // older config with unknown keys (well, the reverse is the real
        // concern, but tolerating extras is harmless and friendlier).
        let cfg: Config = toml::from_str(
            r#"
            [font]
            mono_family = "Fira Code"
            experimental_color = "red"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.font.mono_family, "Fira Code");
    }

    #[test]
    fn defaults_match_current_hardcoded_values() {
        // Sanity: if someone changes the defaults, the diff pane shouldn't
        // silently look different. Test pinning it down.
        assert_eq!(default_mono_text_size(), 13.0);
        if cfg!(target_os = "macos") {
            assert_eq!(default_mono_family(), "Menlo");
        } else {
            assert_eq!(default_mono_family(), "DejaVu Sans Mono");
        }
    }
}

/// What fontconfig picks for `family`. Returns the matched family name (which
/// may differ from the requested one when the user typo'd and fontconfig
/// silently fell back), or `None` if fontconfig isn't installed (typical on
/// stock macOS without brew'd fontconfig).
///
/// We prefer `fc-match` over GPUI's `resolve_font` for detection because
/// `resolve_font` caches one `FontId` per requested `Font` — even when both
/// the bogus and the default family end up rendering the same underlying
/// typeface, their cache entries have different ids, so an equality check
/// on the ids lies. `fc-match` always returns the resolved family name,
/// which is the actual signal we want.
pub fn fc_match(family: &str) -> Option<String> {
    let output = std::process::Command::new("fc-match")
        .arg(family)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    // Output format: "Family Name:style stuff" e.g. "DejaVu Sans Mono:style=Book"
    Some(
        stdout
            .trim()
            .split(':')
            .next()
            .unwrap_or("")
            .trim()
            .to_string(),
    )
}