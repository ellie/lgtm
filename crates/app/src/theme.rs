//! Catppuccin Mocha, hardcoded for now. Helix-theme loading comes later.

use gpui::{rgb, rgba, App, FontStyle, HighlightStyle, Hsla, Rgba};
use gpui_component::{Theme, ThemeMode};
use syntax::Token;

pub fn base() -> Rgba {
    rgb(0x1e1e2e)
}
pub fn mantle() -> Rgba {
    rgb(0x181825)
}
pub fn crust() -> Rgba {
    rgb(0x11111b)
}
pub fn surface0() -> Rgba {
    rgb(0x313244)
}
pub fn text() -> Rgba {
    rgb(0xcdd6f4)
}
pub fn subtext() -> Rgba {
    rgb(0xa6adc8)
}
pub fn overlay0() -> Rgba {
    rgb(0x6c7086)
}
pub fn green() -> Rgba {
    rgb(0xa6e3a1)
}
pub fn red() -> Rgba {
    rgb(0xf38ba8)
}
pub fn blue() -> Rgba {
    rgb(0x89b4fa)
}
pub fn mauve() -> Rgba {
    rgb(0xcba6f7)
}
pub fn peach() -> Rgba {
    rgb(0xfab387)
}

/// Override gpui-component's theme (dark mode, default shadcn palette) with
/// Catppuccin Mocha. Call after `gpui_component::init`.
pub fn apply_ui_theme(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);

    let base: Hsla = base().into();
    let mantle: Hsla = mantle().into();
    let crust: Hsla = crust().into();
    let surface0: Hsla = surface0().into();
    let text: Hsla = text().into();
    let overlay0: Hsla = overlay0().into();
    let green: Hsla = green().into();
    let red: Hsla = red().into();
    let blue: Hsla = blue().into();
    let peach: Hsla = peach().into();

    let theme = Theme::global_mut(cx);
    theme.background = base;
    theme.foreground = text;
    theme.muted = surface0;
    theme.muted_foreground = overlay0;
    theme.border = surface0;
    theme.input = surface0;
    theme.ring = blue;
    theme.primary = blue;
    theme.primary_hover = blue.opacity(0.9);
    theme.primary_active = blue.opacity(0.8);
    theme.primary_foreground = crust;
    theme.secondary = surface0;
    theme.secondary_hover = surface0.opacity(0.8);
    theme.secondary_active = surface0.opacity(0.6);
    theme.secondary_foreground = text;
    theme.accent = surface0;
    theme.accent_foreground = text;
    theme.danger = red;
    theme.danger_hover = red.opacity(0.9);
    theme.danger_active = red.opacity(0.8);
    theme.danger_foreground = crust;
    theme.success = green;
    theme.success_hover = green.opacity(0.9);
    theme.success_active = green.opacity(0.8);
    theme.success_foreground = crust;
    theme.warning = peach;
    theme.warning_hover = peach.opacity(0.9);
    theme.warning_active = peach.opacity(0.8);
    theme.warning_foreground = crust;
    theme.info = blue;
    theme.info_hover = blue.opacity(0.9);
    theme.info_active = blue.opacity(0.8);
    theme.info_foreground = crust;
    theme.link = blue;
    theme.link_hover = blue.opacity(0.9);
    theme.link_active = blue.opacity(0.8);
    theme.popover = mantle;
    theme.popover_foreground = text;
    theme.title_bar = mantle;
    theme.title_bar_border = surface0;
    theme.sidebar = mantle;
    theme.sidebar_foreground = text;
    theme.sidebar_border = surface0;
    theme.caret = text;
    theme.selection = blue.opacity(0.3);
    theme.scrollbar = crust.opacity(0.6);
    theme.scrollbar_thumb = overlay0.opacity(0.5);
    theme.scrollbar_thumb_hover = overlay0;
    theme.window_border = surface0;
}

/// Catppuccin Mocha syntax palette for tree-sitter tokens. Variable and
/// Embedded map to the plain text color (syntax spans for them are not
/// emitted, so this is belt-and-braces).
pub fn token_style(token: Token) -> HighlightStyle {
    let (color, italic) = match token {
        Token::Keyword => (0xcba6f7, false),                 // mauve
        Token::Function => (0x89b4fa, false),                // blue
        Token::Type => (0xf9e2af, false),                    // yellow
        Token::String => (0xa6e3a1, false),                  // green
        Token::Number | Token::Constant => (0xfab387, false), // peach
        Token::Comment => (0x6c7086, true),                  // overlay0
        Token::Property => (0xb4befe, false),                // lavender
        Token::Variable | Token::Embedded => (0xcdd6f4, false), // text
        Token::Parameter => (0xeba0ac, true),                // maroon
        Token::Operator => (0x89dceb, false),                // sky
        Token::Punctuation => (0x9399b2, false),             // overlay2
        Token::Attribute | Token::Label => (0xf9e2af, false), // yellow
        Token::Namespace => (0xfab387, true),                // peach
    };
    HighlightStyle {
        color: Some(rgb(color).into()),
        font_style: italic.then_some(FontStyle::Italic),
        ..Default::default()
    }
}

/// Dimming layer behind the command palette.
pub fn palette_backdrop() -> Rgba {
    rgba(0x11111baa)
}

/// Split view: background for the absent side of a one-sided row — darker
/// than any content row so it clearly reads as "nothing here".
pub fn void_cell_bg() -> Rgba {
    rgba(0x11111b99)
}

/// Low-alpha tints: syntax/text must stay readable on top. Never opaque.
pub fn added_row_bg() -> Rgba {
    rgba(0xa6e3a120)
}
pub fn removed_row_bg() -> Rgba {
    rgba(0xf38ba820)
}
pub fn added_word_bg() -> Rgba {
    rgba(0xa6e3a148)
}
pub fn removed_word_bg() -> Rgba {
    rgba(0xf38ba848)
}
