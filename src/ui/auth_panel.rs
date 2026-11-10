//! Bottom-docked approval presentation. Screen owns input and routes the decision.

use std::io::Write;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::{cursor, terminal};

use crate::config::Defaults;
use crate::ui::theme::{Color, Theme};
use crate::util;

pub use crate::auth::guard::Decision;
const TITLE: &str = "需要授权";
const ALLOW_LABEL: &str = "1. 允许本次";
const DENY_LABEL: &str = "2. 拒绝本次";
const SESSION_LABEL: &str = "3. 本会话允许相同操作";

pub(crate) struct PanelState {
    body: String,
    offset: usize,
    selected: usize,
    top: Option<usize>,
}

impl PanelState {
    /// The only constructor: the body is escaped here, so no caller can hand the panel
    /// text that would move the cursor, recolour the frame or hide a choice — and nothing
    /// the user is asked to approve is hidden from them either.
    pub(crate) fn new(body: &str) -> Self {
        PanelState {
            body: util::review(body),
            offset: 0,
            selected: 0,
            top: None,
        }
    }
}

impl PanelState {
    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Option<Decision> {
        if key.kind == crossterm::event::KeyEventKind::Release {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Some(Decision::Deny),
            KeyCode::Char('c') if ctrl => return Some(Decision::Stop),
            KeyCode::Enter => {
                return Some(
                    [Decision::Allow, Decision::Deny, Decision::AllowSession][self.selected],
                );
            }
            KeyCode::Char('a') | KeyCode::Char('1') => return Some(Decision::Allow),
            KeyCode::Char('n') | KeyCode::Char('2') => return Some(Decision::Deny),
            KeyCode::Char('3') => return Some(Decision::AllowSession),
            KeyCode::Char('q') => return Some(Decision::Deny),
            KeyCode::Down | KeyCode::Tab => self.selected = (self.selected + 1) % 3,
            KeyCode::Up | KeyCode::BackTab => self.selected = (self.selected + 2) % 3,
            KeyCode::Char('k') => self.offset = self.offset.saturating_sub(1),
            KeyCode::Home => self.offset = 0,
            KeyCode::Char('j') => self.offset += 1,
            KeyCode::Char(' ') => self.offset += 1,
            KeyCode::End => self.offset = usize::MAX,
            _ => {}
        }
        None
    }

    pub(crate) fn draw(
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
        let dock = height >= 6;
        let divider = height >= 8;
        let fixed = (if dock { 5 } else { 4 }) + usize::from(divider);
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
        if height >= 4 {
            lines.push(row(
                &format!("{TITLE} · Ctrl+C 停止本轮"),
                false,
                &|text| theme.fg(Color::Cyan, text),
            ));
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
        for (index, label) in [ALLOW_LABEL, DENY_LABEL, SESSION_LABEL].iter().enumerate() {
            if height >= 3 || self.selected == index {
                lines.push(choice(label, self.selected == index));
            }
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

    pub(crate) fn clear(&mut self, out: &mut impl Write) {
        let Some(top) = self.top.take() else {
            return;
        };
        let _ = crossterm::execute!(out, cursor::MoveTo(0, top as u16));
        let _ = crossterm::execute!(out, terminal::Clear(terminal::ClearType::FromCursorDown));
    }
}

/// Show the exact command or file arguments, escaping controls in PanelState.
pub(crate) fn panel_body(tool: &str, input: &serde_json::Value) -> String {
    if tool == "bash"
        && let Some(command) = input.get("command").and_then(|v| v.as_str())
    {
        return split_for_display(command);
    }
    serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string())
}

/// Break a command line onto one line per segment, for the panel to display.
///
/// Splitting is done on shell operators *outside* quotes, and it does not try to be a shell
/// parser: it never needs to be right about what the command means, only to put the parts on
/// their own lines. A quote that never closes therefore ends the scan rather than swallowing
/// the rest of the command — showing too much is fine, showing nothing is not.
fn split_for_display(command: &str) -> String {
    // Heredocs carry their own lines already; the body of one is data, not a command, and
    // re-indenting it would misrepresent what was written.
    if command.contains('\n') {
        return command.to_string();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut single = false;
    let mut double = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' if !single => {
                current.push(c);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '\'' if !double => {
                single = !single;
                current.push(c);
            }
            '"' if !single => {
                double = !double;
                current.push(c);
            }
            '&' | '|' | ';' if !single && !double => {
                // `2>&1` and `>&2` move a file descriptor; they are not separators, and a
                // `&` that follows `>` (or a digit-then-`>`) belongs to that redirection.
                if c == '&' && current.trim_end().ends_with('>') {
                    current.push(c);
                    continue;
                }
                let paired = chars.peek() == Some(&c) && c != ';';
                // A lone `&` backgrounds the command and is then followed by `)` or the end
                // of the line. Splitting there would produce a line like `& )`, which reads
                // as a command but is not one; the background marker stays with its command.
                let lone_background = c == '&'
                    && !paired
                    && chars
                        .peek()
                        .map(|next| next.is_whitespace() || *next == ')')
                        .unwrap_or(true);
                if lone_background {
                    current.push(c);
                    continue;
                }
                let mut op = c.to_string();
                if paired {
                    op.push(chars.next().expect("peeked"));
                }
                lines.push(current.trim().to_string());
                current = format!("{op} ");
                // After `&&` the next segment is a consequence; after `|` it is a different
                // program reading the first one's output; after `;` it merely follows.
            }
            _ => current.push(c),
        }
    }
    lines.push(current.trim().to_string());
    let lines: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() <= 1 {
        return command.to_string();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_remains_bottom_anchored_on_resize_and_in_tiny_windows() {
        let mut panel = state_with_rows(40);
        let nz = |value: u16| std::num::NonZeroU16::new(value).expect("never zero here");
        let mut terminal = vt100::Parser::new(nz(24), nz(40), 100);
        let theme = Theme::default();
        for (width, height) in [(40, 24), (20, 8), (4, 3), (1, 1), (60, 30)] {
            terminal.screen_mut().set_size(nz(height), nz(width));
            let mut frame = Vec::new();
            panel
                .draw(&mut frame, &theme, width as usize, height as usize)
                .unwrap();
            terminal.process(&frame);
            assert_eq!(terminal.screen().cursor_position().0, height - 1);
            assert!(panel.top.unwrap() < height as usize);
            assert!(
                height as usize - panel.top.unwrap() <= Defaults::AUTH_PANEL_MAX_HEIGHT as usize
            );
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
        panel.handle_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(
            panel.handle_key(KeyEvent::from(KeyCode::Enter)),
            Some(Decision::AllowSession)
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
        assert_eq!(state("x").handle_key(ctrl_c), Some(Decision::Stop));
    }

    #[test]
    fn tab_and_arrows_toggle_without_deciding() {
        let mut panel = state("x");
        assert_eq!(panel.handle_key(KeyEvent::from(KeyCode::Up)), None);
        assert_eq!(panel.selected, 2);
        panel.handle_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(panel.selected, 0);
        panel.handle_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(panel.selected, 1);
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
                80,
                24,
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
                80,
                24,
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

    #[test]
    fn every_row_fills_the_terminal_width() {
        // The bug this pins: the padding was computed from the *styled* string, so escape
        // sequences counted as columns and the selected row's background band stopped short
        // of the right edge — it looked like the highlight was a fixed-width box rather than
        // a full-width bar. Every row is exactly the terminal's width, selected or not.
        let width = 80;
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
    fn switching_the_choice_moves_the_marker() {
        let mut panel = state_with_rows(1);
        frame(&mut panel);
        panel.handle_key(KeyEvent::from(KeyCode::Tab));
        let lines = frame(&mut panel);
        assert!(lines.iter().any(|line| line == "1. 允许本次"), "{lines:?}");
        assert!(
            lines.iter().any(|line| line == "› 2. 拒绝本次"),
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
        assert!(last[last.len() - 3].starts_with("› 1."), "{last:?}");
        assert!(last[last.len() - 2].starts_with("2."), "{last:?}");
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
        assert!(lines.contains(&"› 1. 允许本次".to_string()), "{lines:?}");
    }
}
