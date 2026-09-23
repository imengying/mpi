//! The single dark theme. Colours are codex's semantics, hard-coded on purpose:
//! pi has no theme switcher.

/// `truecolor` vs 256-colour fallback is decided once per process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    True,
    Ansi256,
}

impl ColorMode {
    pub fn detect() -> Self {
        let env = std::env::var("COLORTERM").unwrap_or_default().to_lowercase();
        if env.contains("truecolor") || env.contains("24bit") {
            return ColorMode::True;
        }
        match std::env::var("TERM").unwrap_or_default().as_str() {
            "dumb" => ColorMode::Ansi256,
            _ => ColorMode::True,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// The terminal's own foreground: no escape at all.
    ///
    /// This is what body text is drawn in, and it is the difference between a theme that
    /// reads as one dark wash and one that reads as the user's terminal. Painting every
    /// line an explicit grey overrides the foreground they chose, and everything that is
    /// not an accent ends up the same colour.
    Text,
    /// Tool output. Also the terminal's own foreground: a command's output is the main
    /// thing the user reads, and greying it out made the transcript look uniformly dim.
    Output,
    Dim,     // #808080 — hints, notes, anything the eye should skip
    Green,   // usage stats, working directory
    Magenta, // git branch, bash `$`
    Cyan,    // model name, panel title and choices, command names
    Yellow,  // context warning
    Red,     // context error
    DiffAddedText,
    DiffRemovedText,
    /// Code inside a fenced block. The three syntax colours are codex's, and they are the
    /// only colours in the program that are not a UI role: a code block is read by
    /// scanning it, and shape is what makes it scannable.
    SyntaxKeyword,
    SyntaxString,
    SyntaxNumber,
}

impl Color {
    /// `(r, g, b, ansi256_index)`, or `None` to leave the terminal's colour alone.
    fn rgb(self) -> Option<(u8, u8, u8, u8)> {
        Some(match self {
            Color::Text => return None,
            Color::Green => (0x8c, 0xcf, 0x7e, 114),
            Color::Magenta => (0xc5, 0x86, 0xc0, 175),
            Color::Cyan => (0x73, 0xc2, 0xcf, 80),
            Color::Output => return None,
            Color::Yellow => (0xd6, 0xbb, 0x7a, 180),
            Color::Red => (0xf0, 0x8a, 0x83, 210),
            Color::Dim => (0x80, 0x80, 0x80, 244),
            Color::DiffAddedText => (0xa3, 0xd9, 0xa5, 151),
            Color::DiffRemovedText => (0xf2, 0xa2, 0x9a, 216),
            // codex's syntax roles: keyword blue-grey, string green, number amber. The 256
            // indices are the nearest cube entries to the truecolor values.
            Color::SyntaxKeyword => (0xaf, 0xc5, 0xde, 152),
            Color::SyntaxString => (0xa7, 0xc4, 0x9f, 151),
            Color::SyntaxNumber => (0xc9, 0xb8, 0x91, 180),
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub mode: ColorMode,
}

impl Default for Theme {
    fn default() -> Self {
        Theme { mode: ColorMode::detect() }
    }
}

impl Theme {
    pub fn fg(&self, color: Color, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        // Body text is returned untouched so it renders in the terminal's own colour.
        let Some((r, g, b, idx)) = color.rgb() else {
            return text.to_string();
        };
        match self.mode {
            ColorMode::True => format!("\u{1b}[38;2;{r};{g};{b}m{text}\u{1b}[39m"),
            ColorMode::Ansi256 => format!("\u{1b}[38;5;{idx}m{text}\u{1b}[39m"),
        }
    }

    pub fn bold(&self, text: &str) -> String {
        format!("\u{1b}[1m{text}\u{1b}[22m")
    }

    pub fn italic(&self, text: &str) -> String {
        format!("\u{1b}[3m{text}\u{1b}[23m")
    }

    pub fn underline(&self, text: &str) -> String {
        format!("\u{1b}[4m{text}\u{1b}[24m")
    }

    /// Diff row background: codex's dark tints, with a 256-colour fallback.
    pub fn bg_removed(&self, text: &str) -> String {
        match self.mode {
            ColorMode::True => format!("\u{1b}[48;2;74;34;29m{text}\u{1b}[49m"),
            ColorMode::Ansi256 => format!("\u{1b}[48;5;52m{text}\u{1b}[49m"),
        }
    }

    pub fn bg_added(&self, text: &str) -> String {
        match self.mode {
            ColorMode::True => format!("\u{1b}[48;2;33;58;43m{text}\u{1b}[49m"),
            ColorMode::Ansi256 => format!("\u{1b}[48;5;22m{text}\u{1b}[49m"),
        }
    }

    pub fn bg_selected(&self, text: &str) -> String {
        format!("\u{1b}[48;2;62;62;62m{text}\u{1b}[49m")
    }

    /// The context gauge changes colour as it fills.
    pub fn context(&self, percent: f64) -> Color {
        if percent > 90.0 {
            Color::Red
        } else if percent > 70.0 {
            Color::Yellow
        } else {
            Color::Green
        }
    }
}

/// Nerd Fonts database-outline glyph used by the cache-hit field.
pub const CACHE_ICON: &str = "\u{f1632}";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truecolor_uses_24bit_sgr() {
        let theme = Theme { mode: ColorMode::True };
        assert_eq!(theme.fg(Color::Cyan, "x"), "\u{1b}[38;2;115;194;207mx\u{1b}[39m");
    }

    #[test]
    fn fallback_uses_256_colour_sgr() {
        let theme = Theme { mode: ColorMode::Ansi256 };
        assert_eq!(theme.fg(Color::Cyan, "x"), "\u{1b}[38;5;80mx\u{1b}[39m");
    }

    #[test]
    fn body_text_keeps_the_terminals_own_colour() {
        // Painting body text an explicit grey overrides the foreground the user chose and
        // makes every non-accent line the same colour — the transcript read as one dark
        // wash. Body text and tool output therefore carry no escape at all.
        let theme = Theme { mode: ColorMode::True };
        assert_eq!(theme.fg(Color::Text, "hello"), "hello");
        assert_eq!(theme.fg(Color::Output, "command output"), "command output");
        // An empty string stays empty rather than emitting a lone escape pair.
        assert_eq!(theme.fg(Color::Text, ""), "");

        // The accents are still painted; this is not "stop colouring anything".
        for color in [Color::Green, Color::Magenta, Color::Cyan, Color::Dim] {
            assert!(
                theme.fg(color, "x").contains("\u{1b}[38"),
                "{color:?} lost its colour"
            );
        }
    }
}
