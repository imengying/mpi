//! The authorization panel: a full-width strip docked at the bottom of the screen.
//!
//! It is deliberately not a floating dialog. It sits flush with the last row, separated
//! from the transcript by a rule, so the decision always appears in the same place and
//! the transcript stays readable. The title is fixed ("需要授权") — a category label used to
//! live there and was removed because the body already says what is being asked, and the
//! tool name (`bash`) carries no information on any platform.
//!
//! The body is the thing being asked about and nothing else: the command, or the arguments
//! for a file tool. A policy reason used to sit above it, which was the same mistake the
//! title's category label was — a sentence explaining what the reader is already looking at.
//!
//! There is no timeout: waiting forever is the point.

use std::io::{IsTerminal, Write};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::{cursor, terminal};

use crate::config::Defaults;
use crate::ui::theme::{Color, Theme};
use crate::util;

/// What the panel shows: the call itself, with no commentary around it. The body is plain
/// text; every control character in it has already been escaped so a crafted payload cannot
/// move the cursor or recolour the UI.
pub struct PanelRequest {
    pub body: String,
}

const TITLE: &str = "需要授权";
const ALLOW_LABEL: &str = "1. 允许本次操作";
const DENY_LABEL: &str = "2. 拒绝并停止";

/// What the user chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

/// Show the panel and block until the user decides. Returns `Deny` if there is no
/// terminal to draw on: a headless run must never silently approve anything.
pub fn ask(request: PanelRequest) -> Decision {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Decision::Deny;
    }
    if crate::ui::terminal::ensure_raw_mode().is_err() {
        return Decision::Deny;
    }
    let theme = Theme::default();
    let mut state = PanelState::new(&request.body);
    let mut out = std::io::stdout();
    // Nothing on the panel is a text position: the cursor has no place to sit while the
    // question is up. Leaving it visible parks it on the last drawn row — under the choices,
    // at the very bottom of the screen — where it blinks as if it were waiting for typing,
    // which is exactly the wrong reading of "waiting for your decision".
    let _ = crossterm::execute!(out, cursor::Hide);
    let decision = loop {
        if state.draw(&mut out, &theme).is_err() {
            break Decision::Deny;
        }
        match event::read() {
            Ok(Event::Key(key)) => {
                if let Some(decision) = state.handle_key(key) {
                    break decision;
                }
            }
            Ok(_) => {}
            Err(_) => break Decision::Deny,
        }
    };
    state.clear(&mut out);
    let _ = crossterm::execute!(out, cursor::Show);
    let _ = out.flush();
    decision
}

struct PanelState {
    body: String,
    offset: usize,
    allow_selected: bool,
    top: Option<usize>,
}

impl PanelState {
    /// The only constructor: the body is escaped here, so no caller can hand the panel
    /// text that would move the cursor, recolour the frame or hide a choice — and nothing
    /// the user is asked to approve is hidden from them either.
    fn new(body: &str) -> Self {
        PanelState {
            body: util::review(body),
            offset: 0,
            allow_selected: true,
            top: None,
        }
    }
}

impl PanelState {
    fn handle_key(&mut self, key: KeyEvent) -> Option<Decision> {
        if key.kind == crossterm::event::KeyEventKind::Release {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Some(Decision::Deny),
            KeyCode::Char('c') if ctrl => return Some(Decision::Deny),
            KeyCode::Enter => {
                return Some(if self.allow_selected {
                    Decision::Allow
                } else {
                    Decision::Deny
                });
            }
            KeyCode::Char('a') | KeyCode::Char('1') => return Some(Decision::Allow),
            KeyCode::Char('n') | KeyCode::Char('2') => return Some(Decision::Deny),
            KeyCode::Char('q') => return Some(Decision::Deny),
            KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::BackTab => {
                self.allow_selected = !self.allow_selected;
            }
            KeyCode::PageUp => self.offset = self.offset.saturating_sub(1),
            KeyCode::Char('k') => self.offset = self.offset.saturating_sub(1),
            KeyCode::Home => self.offset = 0,
            KeyCode::PageDown | KeyCode::Char('j') => self.offset += 1,
            KeyCode::Char(' ') => self.offset += 1,
            KeyCode::End => self.offset = usize::MAX,
            _ => {}
        }
        None
    }

    fn draw(&mut self, out: &mut impl Write, theme: &Theme) -> std::io::Result<()> {
        let (width, rows) = terminal::size().unwrap_or((80, 24));
        self.draw_at(
            out,
            theme,
            usize::from(width.max(1)),
            usize::from(rows.max(1)),
        )
    }

    fn draw_at(
        &mut self,
        out: &mut impl Write,
        theme: &Theme,
        width: usize,
        rows: usize,
    ) -> std::io::Result<()> {
        let height = rows.min(Defaults::AUTH_PANEL_MAX_HEIGHT as usize);
        let inset = usize::from(width >= 3);
        let inner = width.saturating_sub(inset * 2).max(1);
        let content = util::wrap(&self.body, inner);
        let dock = height >= 5;
        let divider = height >= 8;
        let fixed = (if dock { 4 } else { 3 }) + usize::from(divider);
        let page = height.saturating_sub(fixed);
        let max_offset = content.len().saturating_sub(page);
        self.offset = self.offset.min(max_offset);
        let visible = &content[self.offset..(self.offset + page).min(content.len())];

        let rule = theme.fg(Color::Dim, &"─".repeat(width));
        // A row is built from its *plain* text, and the padding is computed before any style
        // is applied. Measuring a styled string counts escape sequences as characters — a
        // colour is worth a dozen columns of "width" — which is why the selected row's band
        // stopped short of the right edge: `util::pad` thought it had already filled the
        // line. The visible text is the only thing that has a width.
        let row = |plain: &str, selected: bool, style: &dyn Fn(&str) -> String| -> String {
            let visible = util::truncate(plain, inner, "…");
            let pad = " ".repeat(inner.saturating_sub(util::width(&visible)));
            let edge = " ".repeat(inset);
            let line = format!("{edge}{}{pad}{edge}", style(&visible));
            if selected {
                theme.bg_selected(&line)
            } else {
                line
            }
        };

        let mut lines: Vec<String> = Vec::new();
        if dock {
            lines.push(rule.clone());
        }
        if height >= 3 {
            lines.push(row(TITLE, false, &|text| theme.fg(Color::Cyan, text)));
        }
        for line in visible {
            lines.push(row(line, false, &|text| theme.fg(Color::Text, text)));
        }
        if divider {
            lines.push(rule.clone());
        }
        let choice = |label: &str, selected: bool| -> String {
            let marker = if selected { "› " } else { "  " };
            let text = format!("{marker}{label}");
            row(&text, selected, &|visible| {
                let styled = if selected {
                    theme.bold(visible)
                } else {
                    visible.to_string()
                };
                theme.fg(Color::Cyan, &styled)
            })
        };
        if height > 1 || self.allow_selected {
            lines.push(choice(ALLOW_LABEL, self.allow_selected));
        }
        if height > 1 || !self.allow_selected {
            lines.push(choice(DENY_LABEL, !self.allow_selected));
        }

        // Paint every cell so the transcript cannot bleed through between rows. The rows are
        // already exactly `width` columns of *visible* text; the escapes are not counted,
        // which is the whole reason for measuring the stripped form here.
        let top = rows.saturating_sub(lines.len());
        let clear = self.top.map_or(top, |old| old.min(top));
        let mut frame = format!(
            "\u{1b}[?2026h\u{1b}[{};1H\u{1b}[J\u{1b}[{};1H",
            clear + 1,
            top + 1
        );
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                frame.push_str("\r\n");
            }
            let width_used = util::width(&util::strip_ansi(line));
            frame.push_str(line);
            frame.push_str(&" ".repeat(width.saturating_sub(width_used)));
            frame.push_str("\u{1b}[0m");
        }
        frame.push_str("\u{1b}[?2026l");
        out.write_all(frame.as_bytes())?;
        out.flush()?;
        self.top = Some(top);
        Ok(())
    }

    fn clear(&mut self, out: &mut impl Write) {
        let Some(top) = self.top.take() else {
            return;
        };
        let _ = crossterm::execute!(out, cursor::MoveTo(0, top as u16));
        let _ = crossterm::execute!(out, terminal::Clear(terminal::ClearType::FromCursorDown));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_remains_bottom_anchored_on_resize_and_in_tiny_windows() {
        let mut panel = state_with_rows(40);
        let mut terminal = vt100::Parser::new(24, 40, 100);
        let theme = Theme::default();
        for (width, height) in [(40, 24), (20, 8), (4, 3), (1, 1), (60, 30)] {
            terminal.screen_mut().set_size(height, width);
            let mut frame = Vec::new();
            panel
                .draw_at(&mut frame, &theme, width as usize, height as usize)
                .unwrap();
            terminal.process(&frame);
            assert_eq!(terminal.screen().cursor_position().0, height - 1);
            assert!(panel.top.unwrap() < height as usize);
            if width >= 20 {
                assert!(terminal.screen().contents().contains(ALLOW_LABEL));
                assert!(terminal.screen().contents().contains(DENY_LABEL));
            }
        }
        assert!(
            panel
                .handle_key(KeyEvent::new_with_kind(
                    KeyCode::Enter,
                    KeyModifiers::NONE,
                    crossterm::event::KeyEventKind::Release
                ))
                .is_none()
        );
    }

    fn state(body: &str) -> PanelState {
        PanelState::new(body)
    }

    /// A panel whose body is `count` numbered rows.
    fn state_with_rows(count: usize) -> PanelState {
        state(
            &(0..count)
                .map(|i| format!("body line {i}\n"))
                .collect::<String>(),
        )
    }

    #[test]
    fn enter_confirms_the_selected_choice() {
        let mut panel = state("cat x");
        assert_eq!(
            panel.handle_key(KeyEvent::from(KeyCode::Enter)),
            Some(Decision::Allow)
        );
        panel.handle_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(
            panel.handle_key(KeyEvent::from(KeyCode::Enter)),
            Some(Decision::Deny)
        );
    }

    #[test]
    fn shortcuts_decide_immediately() {
        assert_eq!(
            state("x").handle_key(KeyEvent::from(KeyCode::Char('a'))),
            Some(Decision::Allow)
        );
        assert_eq!(
            state("x").handle_key(KeyEvent::from(KeyCode::Char('1'))),
            Some(Decision::Allow)
        );
        assert_eq!(
            state("x").handle_key(KeyEvent::from(KeyCode::Char('n'))),
            Some(Decision::Deny)
        );
        assert_eq!(
            state("x").handle_key(KeyEvent::from(KeyCode::Char('2'))),
            Some(Decision::Deny)
        );
        assert_eq!(
            state("x").handle_key(KeyEvent::from(KeyCode::Char('q'))),
            Some(Decision::Deny)
        );
        assert_eq!(
            state("x").handle_key(KeyEvent::from(KeyCode::Esc)),
            Some(Decision::Deny)
        );
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(state("x").handle_key(ctrl_c), Some(Decision::Deny));
    }

    #[test]
    fn tab_and_arrows_toggle_without_deciding() {
        let mut panel = state("x");
        assert_eq!(panel.handle_key(KeyEvent::from(KeyCode::Up)), None);
        assert!(!panel.allow_selected);
        panel.handle_key(KeyEvent::from(KeyCode::Down));
        assert!(panel.allow_selected);
        panel.handle_key(KeyEvent::from(KeyCode::Tab));
        assert!(!panel.allow_selected);
    }

    #[test]
    fn scrolling_moves_the_body_but_stays_in_range() {
        let mut panel = state(&(0..100).map(|i| format!("line {i}\n")).collect::<String>());
        panel.handle_key(KeyEvent::from(KeyCode::End));
        panel.offset = panel.offset.min(50);
        // The panel clamps during draw; a scroll past the end must not panic.
        for _ in 0..5 {
            panel.handle_key(KeyEvent::from(KeyCode::Char('j')));
        }
        assert!(panel.offset > 0);
        panel.handle_key(KeyEvent::from(KeyCode::Home));
        assert_eq!(panel.offset, 0);
    }

    #[test]
    fn the_first_frame_selects_allow() {
        let panel = state("x");
        assert!(panel.allow_selected);
    }

    /// Render one frame into a buffer and strip the escape codes, so the layout can be
    /// asserted without a terminal. The panel's own body is used as-is.
    fn frame(panel: &mut PanelState) -> Vec<String> {
        let mut out: Vec<u8> = Vec::new();
        panel
            .draw(
                &mut out,
                &Theme {
                    mode: crate::ui::theme::ColorMode::True,
                },
            )
            .unwrap();
        let text = String::from_utf8_lossy(&out).to_string();
        text.split("\r\n")
            .map(|line| {
                let mut plain = String::new();
                let mut chars = line.chars().peekable();
                while let Some(c) = chars.next() {
                    if c != '\u{1b}' {
                        plain.push(c);
                        continue;
                    }
                    if chars.peek() == Some(&'[') {
                        chars.next();
                        for c in chars.by_ref() {
                            if ('\u{40}'..='\u{7e}').contains(&c) {
                                break;
                            }
                        }
                    }
                }
                plain.trim().to_string()
            })
            // Drop the empty rows produced by the frame's trailing newline.
            .filter(|line| !line.is_empty())
            .collect()
    }

    /// One frame, as `(visible text, visible width)` per row — no trimming, because the
    /// trailing padding is exactly what has to be measured.
    fn frame_widths(panel: &mut PanelState) -> Vec<(String, usize)> {
        let mut out: Vec<u8> = Vec::new();
        panel
            .draw(
                &mut out,
                &Theme {
                    mode: crate::ui::theme::ColorMode::True,
                },
            )
            .unwrap();
        let text = String::from_utf8_lossy(&out).to_string();
        text.split("\r\n")
            .filter(|line| !line.is_empty())
            .map(|line| {
                let plain = util::strip_ansi(line);
                (plain.clone(), util::width(&plain))
            })
            .collect()
    }

    /// The terminal width the panel will draw at, as the panel itself asks for it.
    fn terminal_width() -> usize {
        terminal::size()
            .map(|(cols, _)| cols as usize)
            .unwrap_or(80)
    }

    #[test]
    fn every_row_fills_the_terminal_width() {
        // The bug this pins: the padding was computed from the *styled* string, so escape
        // sequences counted as columns and the selected row's background band stopped short
        // of the right edge — it looked like the highlight was a fixed-width box rather than
        // a full-width bar. Every row is exactly the terminal's width, selected or not.
        let width = terminal_width();
        let mut panel = state_with_rows(3);
        for selected_allow in [true, false] {
            if !selected_allow {
                panel.handle_key(KeyEvent::from(KeyCode::Tab));
            }
            for (text, drawn) in frame_widths(&mut panel) {
                assert_eq!(drawn, width, "{drawn} columns, expected {width}: {text:?}");
            }
        }
    }

    #[test]
    fn the_selected_row_is_padded_like_the_rest() {
        // The selected row is the one that carries a background, so it is the row where a
        // short pad is visible as a box that ends early. Its visible text must be the same
        // width as the unselected rows' — the highlight covers the full strip.
        let width = terminal_width();
        let mut panel = state_with_rows(1);
        let rows = frame_widths(&mut panel);
        let find = |prefix: &str| {
            rows.iter()
                .find(|(text, _)| text.trim_start().starts_with(prefix))
                .unwrap_or_else(|| panic!("no row starts with {prefix:?}: {rows:?}"))
        };
        let selected = find("› 1.");
        let other = find("2.");
        assert_eq!(selected.1, other.1, "{rows:?}");
        assert_eq!(selected.1, width, "{rows:?}");
    }

    #[test]
    fn the_rendered_panel_has_the_fixed_title_and_both_choices() {
        let mut panel = state_with_rows(3);
        let lines = frame(&mut panel);
        // Docked strip: a rule, the fixed title, the body, a rule, then the two choices.
        assert!(lines[0].starts_with('─'), "{lines:?}");
        assert_eq!(lines[1], TITLE, "{lines:?}");
        assert_eq!(
            TITLE, "需要授权",
            "the title is short and fixed: the choices say the rest"
        );
        // No category label and no tool name anywhere in the frame: the body already says
        // what is being asked, and `bash` is the same on every platform.
        for line in &lines {
            assert!(!line.contains("bash"), "{lines:?}");
            assert!(!line.contains("危险操作"), "{lines:?}");
        }
        assert!(
            lines.contains(&"› 1. 允许本次操作".to_string()),
            "{lines:?}"
        );
        assert!(lines.contains(&"2. 拒绝并停止".to_string()), "{lines:?}");
        // The choices are the last two rows.
        assert!(lines[lines.len() - 2].starts_with("› 1."), "{lines:?}");
        assert!(lines[lines.len() - 1].starts_with("2."), "{lines:?}");
    }

    #[test]
    fn switching_the_choice_moves_the_marker() {
        let mut panel = state_with_rows(1);
        frame(&mut panel);
        panel.handle_key(KeyEvent::from(KeyCode::Tab));
        let lines = frame(&mut panel);
        assert!(
            lines.iter().any(|line| line == "1. 允许本次操作"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line == "› 2. 拒绝并停止"),
            "{lines:?}"
        );
    }

    #[test]
    fn the_body_scrolls_but_the_choices_never_do() {
        let mut panel = state_with_rows(100);
        let first = frame(&mut panel);
        assert!(first[2].contains("body line 0"), "{first:?}");
        panel.handle_key(KeyEvent::from(KeyCode::End));
        let last = frame(&mut panel);
        assert!(
            !last[2].contains("body line 0"),
            "the view should have scrolled: {last:?}"
        );
        // The decision rows are always the final two, however far the body has scrolled.
        assert!(last[last.len() - 2].starts_with("› 1."), "{last:?}");
        assert!(last[last.len() - 1].starts_with("2."), "{last:?}");
    }

    #[test]
    fn a_crafted_body_cannot_escape_the_panel() {
        // Control characters and bidi overrides are escaped, so a payload cannot move the
        // cursor, recolour the frame or hide a choice.
        // The panel escapes whatever it is handed, so an injected payload cannot clear the
        // screen, ring the bell, or reverse the text.
        let payload = "\u{1b}[2Jclear\u{7}bell\u{202e}reversed";
        let panel = state(payload);
        assert!(
            panel.body.contains("\\u001b"),
            "ESC must be escaped: {:?}",
            panel.body
        );
        assert!(
            panel.body.contains("\\u0007"),
            "BEL must be escaped: {:?}",
            panel.body
        );
        assert!(
            panel.body.contains("\\u202e"),
            "RTL override must be escaped: {:?}",
            panel.body
        );
        assert!(!panel.body.contains('\u{1b}'), "{:?}", panel.body);
        // The payload's own text survives, so the display is faithful.
        assert!(panel.body.contains("clear"), "{:?}", panel.body);
        assert!(panel.body.contains("reversed"), "{:?}", panel.body);
        let mut panel = panel;
        let lines = frame(&mut panel);
        assert!(
            lines.iter().all(|line| !line.contains('\u{1b}')),
            "{lines:?}"
        );
        assert!(
            lines.iter().all(|line| !line.contains('\u{7}')),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"› 1. 允许本次操作".to_string()),
            "{lines:?}"
        );
    }

    #[test]
    fn the_panel_never_exceeds_its_height_cap() {
        let mut panel = state_with_rows(200);
        let lines = frame(&mut panel);
        assert!(
            lines.len() <= Defaults::AUTH_PANEL_MAX_HEIGHT as usize,
            "panel drew {} rows",
            lines.len()
        );
    }
}
