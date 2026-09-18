//! Rendering: the theme, the two-line footer, compact tool output and the
//! authorization panel. Nothing here changes what the model sees.

#[cfg(test)]
pub(crate) fn plain(lines: &[screen::Line]) -> Vec<String> {
    lines.iter().map(screen::Line::text).collect()
}

pub mod auth_panel;
pub mod compact;
pub mod diff;
pub mod footer;
pub mod screen;
pub mod theme;
