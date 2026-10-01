//! Full-screen list pickers: `/model`, `/resume`, and the like.
//!
//! They run *inside* a turn, so each takes raw mode for its duration and gives the screen
//! back the way it found it. The choice is returned rather than acted on: what a picked
//! entry means is the caller's business.

use super::*;

impl Screen {
    /// Show a list next to the footer and let the user choose one entry.
    ///
    /// The highlighted entry starts on the first one, so Enter accept the obvious default.
    /// Returns `None` for Esc / Ctrl+C / `q`, and the index of the chosen entry otherwise.
    pub fn pick(&mut self, title: &str, items: &[String]) -> Option<usize> {
        self.pick_at(title, items, 0)
    }

    /// Like [`Screen::pick`], but the hint row says what Esc will do.
    ///
    /// The default hint promises "cancel", which is wrong where cancel *is* an action —
    /// `/resume` starts a new session on Esc, and a menu that said "cancel" while doing that
    /// would be lying about its own key.
    pub fn pick_with_hint(&mut self, title: &str, hint: &str, items: &[String]) -> Option<usize> {
        self.pick_hinted(title, items, 0, hint)
    }

    /// Like [`Screen::pick`], with the initial highlight on `initial` (clamped into range).
    /// The menu for `/model` uses this so the current choice starts highlighted and Enter
    /// keeps it.
    pub fn pick_at(&mut self, title: &str, items: &[String], initial: usize) -> Option<usize> {
        self.pick_hinted(title, items, initial, "↑↓ 选择 · Enter 确认 · Esc 取消")
    }

    pub(super) fn pick_hinted(
        &mut self,
        title: &str,
        items: &[String],
        initial: usize,
        hint: &str,
    ) -> Option<usize> {
        if items.is_empty() {
            return None;
        }
        if !self.interactive {
            // No terminal to choose on: fall back to the highlighted entry.
            return Some(initial.min(items.len() - 1));
        }
        let mut cursor = initial.min(items.len() - 1);
        if crate::ui::terminal::ensure_raw_mode().is_err() {
            return None;
        }
        let saved_footer = self.footer.clone();
        let saved_state = self.footer_state.take();
        let saved_editing = self.editing.take();
        // Clear the composer before presenting the bounded list.
        self.erase_live();
        let result = loop {
            // The menu replaces the footer so it always sits in the same place.
            self.footer = self.menu_lines(title, hint, items, cursor);
            self.draw_live();
            let event = match event::read() {
                Ok(event) => event,
                Err(_) => break None,
            };
            let Event::Key(key) = event else {
                if let Event::Resize(width, height) = event {
                    self.width = usize::from(width.max(1));
                    self.height = usize::from(height.max(1));
                }
                continue;
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match key.code {
                KeyCode::Enter => break Some(cursor),
                KeyCode::Esc => break None,
                KeyCode::Char('c') | KeyCode::Char('q')
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        || key.code == KeyCode::Char('q') =>
                {
                    break None;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    cursor = if cursor == 0 {
                        items.len() - 1
                    } else {
                        cursor - 1
                    };
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                    cursor = (cursor + 1) % items.len();
                }
                KeyCode::Home => cursor = 0,
                KeyCode::End => cursor = items.len() - 1,
                KeyCode::PageUp => {
                    cursor = cursor.saturating_sub(self.height.saturating_sub(3).clamp(1, 12))
                }
                KeyCode::PageDown => {
                    cursor =
                        (cursor + self.height.saturating_sub(3).clamp(1, 12)).min(items.len() - 1)
                }
                KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                    let index = c as usize - '1' as usize;
                    if index < items.len() {
                        break Some(index);
                    }
                }
                _ => {}
            }
        };
        self.footer = saved_footer;
        self.footer_state = saved_state;
        self.editing = saved_editing;
        // The menu shared the live region with the footer, so nothing has to be erased
        // beyond redrawing it.
        self.draw_live();
        result
    }

    pub(super) fn menu_lines(
        &self,
        title: &str,
        hint: &str,
        items: &[String],
        cursor: usize,
    ) -> Vec<Line> {
        let budget = self.height.saturating_sub(1).max(1);
        let cap = budget.saturating_sub(2).clamp(1, 12);
        let first = cursor
            .saturating_sub(cap / 2)
            .min(items.len().saturating_sub(cap));
        let end = (first + cap).min(items.len());
        let mut lines = Vec::new();
        if budget > 2 {
            lines.push(Line::new(
                util::truncate(
                    &format!("{title}  {}/{}", cursor + 1, items.len()),
                    self.width,
                    "…",
                ),
                Style::bold(Color::Cyan),
            ));
        }
        for (index, item) in items[first..end].iter().enumerate() {
            let selected = first + index == cursor;
            let marker = if selected {
                "› "
            } else if index == 0 && first > 0 {
                "↑ "
            } else if first + index + 1 == end && end < items.len() {
                "↓ "
            } else {
                "  "
            };
            let text = util::truncate(&format!("{marker}{item}"), self.width, "…");
            let style = if selected {
                Style {
                    bg: Bg::Selected,
                    ..Style::new(Color::Cyan)
                }
            } else {
                Style::plain()
            };
            let style = if title == "思考级别" {
                Style {
                    fg: Color::reasoning(item),
                    bold: selected,
                    ..style
                }
            } else {
                style
            };
            lines.push(Line::new(util::pad(&text, self.width), style));
        }
        if budget > 1 {
            lines.push(Line::new(
                util::truncate(hint, self.width, "…"),
                Style::new(Color::Dim),
            ));
        }
        lines
    }
}
