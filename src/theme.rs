//! Color themes. `default` uses the terminal's own palette; the others paint
//! their own background. Pick one with `theme = "…"` in the config file or
//! `--theme`, or cycle through them with `t` in the app.

use std::sync::atomic::{AtomicUsize, Ordering};

use ratatui::{
    style::{Color, Style, Styled},
    widgets::{Block, BorderType},
};

pub struct Theme {
    pub name: &'static str,
    /// Background and text of the whole screen (`Reset` = terminal default).
    pub bg: Color,
    pub text: Color,
    /// Secondary text (labels, hints). `None` = the terminal's dim attribute.
    subtle: Option<Color>,
    pub border: Color,
    /// Tabs, dialog borders.
    pub accent: Color,
    /// Descriptions, progress bars, level history.
    pub info: Color,
    pub ok: Color,
    pub warn: Color,
    pub err: Color,
    /// Inactive things: muted channel, finished playback.
    pub off: Color,
    /// Empty part of gauges.
    pub track: Color,
    /// Text input background.
    pub field: Color,
    /// Selected row background.
    pub highlight: Color,
    /// Text on a colored background (active tab, badges).
    pub on_color: Color,
    pub key_fg: Color,
    pub key_bg: Color,
}

impl Theme {
    pub fn subtle(&self) -> Style {
        match self.subtle {
            Some(c) => Style::new().fg(c),
            None => Style::new().dim(),
        }
    }

    /// Style for the whole screen / popup backgrounds.
    pub fn base(&self) -> Style {
        Style::new().bg(self.bg).fg(self.text)
    }

    /// Key chip, e.g. ` space `.
    pub fn key(&self) -> Style {
        Style::new().fg(self.key_fg).bg(self.key_bg)
    }
}

/// Meaning of a status message; the color comes from the current theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Warn,
    Err,
}

impl Tone {
    pub fn color(self) -> Color {
        let t = get();
        match self {
            Tone::Ok => t.ok,
            Tone::Warn => t.warn,
            Tone::Err => t.err,
        }
    }
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// Catppuccin flavor from its palette.
const fn catppuccin(
    name: &'static str,
    [
        base,
        crust,
        text,
        subtext0,
        overlay0,
        surface0,
        surface1,
        surface2,
    ]: [u32; 8],
    [green, yellow, red, mauve, sky]: [u32; 5],
) -> Theme {
    Theme {
        name,
        bg: rgb(base),
        text: rgb(text),
        subtle: Some(rgb(subtext0)),
        border: rgb(surface2),
        accent: rgb(mauve),
        info: rgb(sky),
        ok: rgb(green),
        warn: rgb(yellow),
        err: rgb(red),
        off: rgb(overlay0),
        track: rgb(surface0),
        field: rgb(surface0),
        highlight: rgb(surface1),
        on_color: rgb(crust),
        key_fg: rgb(text),
        key_bg: rgb(surface1),
    }
}

pub static THEMES: &[Theme] = &[
    Theme {
        name: "default",
        bg: Color::Reset,
        text: Color::Reset,
        subtle: None,
        border: Color::Reset,
        accent: Color::Cyan,
        info: Color::Cyan,
        ok: Color::Green,
        warn: Color::Yellow,
        err: Color::Red,
        off: Color::DarkGray,
        track: Color::Black,
        field: Color::Rgb(40, 40, 48),
        highlight: Color::DarkGray,
        on_color: Color::Black,
        key_fg: Color::Black,
        key_bg: Color::Gray,
    },
    catppuccin(
        "catppuccin-mocha",
        [
            0x1e1e2e, 0x11111b, 0xcdd6f4, 0xa6adc8, 0x6c7086, 0x313244, 0x45475a, 0x585b70,
        ],
        [0xa6e3a1, 0xf9e2af, 0xf38ba8, 0xcba6f7, 0x89dceb],
    ),
    catppuccin(
        "catppuccin-macchiato",
        [
            0x24273a, 0x181926, 0xcad3f5, 0xa5adcb, 0x6e738d, 0x363a4f, 0x494d64, 0x5b6078,
        ],
        [0xa6da95, 0xeed49f, 0xed8796, 0xc6a0f6, 0x91d7e3],
    ),
    catppuccin(
        "catppuccin-frappe",
        [
            0x303446, 0x232634, 0xc6d0f5, 0xa5adce, 0x737994, 0x414559, 0x51576d, 0x626880,
        ],
        [0xa6d189, 0xe5c890, 0xe78284, 0xca9ee6, 0x99d1db],
    ),
    catppuccin(
        "catppuccin-latte",
        [
            0xeff1f5, 0xdce0e8, 0x4c4f69, 0x6c6f85, 0x9ca0b0, 0xccd0da, 0xbcc0cc, 0xacb0be,
        ],
        [0x40a02b, 0xdf8e1d, 0xd20f39, 0x8839ef, 0x04a5e5],
    ),
    Theme {
        name: "gruvbox-dark",
        bg: rgb(0x282828),
        text: rgb(0xebdbb2),
        subtle: Some(rgb(0xa89984)),
        border: rgb(0x665c54),
        accent: rgb(0xfe8019),
        info: rgb(0x83a598),
        ok: rgb(0xb8bb26),
        warn: rgb(0xfabd2f),
        err: rgb(0xfb4934),
        off: rgb(0x928374),
        track: rgb(0x3c3836),
        field: rgb(0x3c3836),
        highlight: rgb(0x504945),
        on_color: rgb(0x282828),
        key_fg: rgb(0xebdbb2),
        key_bg: rgb(0x504945),
    },
    Theme {
        name: "nord",
        bg: rgb(0x2e3440),
        text: rgb(0xd8dee9),
        subtle: Some(rgb(0x9aa5b8)),
        border: rgb(0x4c566a),
        accent: rgb(0x88c0d0),
        info: rgb(0x81a1c1),
        ok: rgb(0xa3be8c),
        warn: rgb(0xebcb8b),
        err: rgb(0xbf616a),
        off: rgb(0x616e88),
        track: rgb(0x3b4252),
        field: rgb(0x3b4252),
        highlight: rgb(0x434c5e),
        on_color: rgb(0x2e3440),
        key_fg: rgb(0xeceff4),
        key_bg: rgb(0x4c566a),
    },
    Theme {
        name: "tokyo-night",
        bg: rgb(0x1a1b26),
        text: rgb(0xc0caf5),
        subtle: Some(rgb(0x9aa5ce)),
        border: rgb(0x3b4261),
        accent: rgb(0xbb9af7),
        info: rgb(0x7dcfff),
        ok: rgb(0x9ece6a),
        warn: rgb(0xe0af68),
        err: rgb(0xf7768e),
        off: rgb(0x565f89),
        track: rgb(0x24283b),
        field: rgb(0x24283b),
        highlight: rgb(0x292e42),
        on_color: rgb(0x1a1b26),
        key_fg: rgb(0xc0caf5),
        key_bg: rgb(0x3b4261),
    },
];

static CURRENT: AtomicUsize = AtomicUsize::new(0);

/// The current theme.
pub fn get() -> &'static Theme {
    &THEMES[CURRENT.load(Ordering::Relaxed) % THEMES.len()]
}

/// Index of the theme called `name` (`catppuccin` = `catppuccin-mocha`).
pub fn find(name: &str) -> Option<usize> {
    let name = match name.trim().to_ascii_lowercase() {
        n if n == "catppuccin" => "catppuccin-mocha".to_string(),
        n => n,
    };
    THEMES.iter().position(|t| t.name == name)
}

pub fn names() -> String {
    THEMES.iter().map(|t| t.name).collect::<Vec<_>>().join(", ")
}

/// Selects the theme called `name`.
pub fn set(name: &str) -> anyhow::Result<()> {
    let i = find(name)
        .ok_or_else(|| anyhow::anyhow!("unknown theme {name:?} (available: {})", names()))?;
    CURRENT.store(i, Ordering::Relaxed);
    Ok(())
}

/// Switches to the next theme and returns it.
pub fn cycle() -> &'static Theme {
    let next = (CURRENT.load(Ordering::Relaxed) + 1) % THEMES.len();
    CURRENT.store(next, Ordering::Relaxed);
    get()
}

/// A rounded, bordered panel in the theme's colors.
pub fn panel() -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(get().border))
}

/// `.subtle()`: secondary text in the current theme (replaces `.dim()`).
pub trait Themed: Styled<Item = Self> + Sized {
    fn subtle(self) -> Self {
        let s = self.style().patch(get().subtle());
        self.set_style(s)
    }
}

impl<T: Styled<Item = T>> Themed for T {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_resolve() {
        assert_eq!(find("default"), Some(0));
        assert_eq!(find("Catppuccin"), find("catppuccin-mocha"));
        assert!(find("catppuccin-latte").is_some());
        assert!(find("nope").is_none());
    }

    #[test]
    fn names_are_unique() {
        for (i, t) in THEMES.iter().enumerate() {
            assert_eq!(find(t.name), Some(i));
        }
    }
}
