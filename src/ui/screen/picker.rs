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
        pub fn pick_with_hint(
            &mut self,
            title: &str,
            hint: &str,
            items: &[String],
        ) -> Option<usize> {
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
            let guard = match RawGuard::enter() {
                Ok(guard) => guard,
                Err(_) => return None,
            };
            let saved_footer = self.footer.clone();
            let saved_editing = self.editing.take();
            // Take the live region down before the panel draws. The panel writes straight to the
            // terminal and moves the cursor itself, so `live_rows`/`cursor_row` — this struct's
            // record of where the region is — stop describing the screen the moment it runs. The
            // next erase would then climb from the wrong row and leave part of the old region
            // behind, which is how a finished tool call kept a stale `●` line above it.
            //
            // Clearing first means the panel starts on a clean row, and the redraw at the end
            // puts the region back from a known position.
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
                    if matches!(event, Event::Resize(_, _)) {
                        self.refresh_size();
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
                        if key.modifiers.contains(KeyModifiers::CONTROL) || key.code == KeyCode::Char('q') =>
                    {
                        break None
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        cursor = if cursor == 0 { items.len() - 1 } else { cursor - 1 };
                    }
                    KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                        cursor = (cursor + 1) % items.len();
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
            self.editing = saved_editing;
            drop(guard);
            // The menu shared the live region with the footer, so nothing has to be erased
            // beyond redrawing it.
            self.draw_live();
            result
        }


        pub(super) fn menu_lines(&self, title: &str, hint: &str, items: &[String], cursor: usize) -> Vec<Line> {
            let mut lines = vec![Line::new(title, Style::bold(Color::Cyan))];
            for (index, item) in items.iter().enumerate() {
                let selected = index == cursor;
                let marker = if selected { "› " } else { "  " };
                let text = util::truncate(&format!("{marker}{item}"), self.width, "…");
                let style = if selected {
                    Style { bg: Bg::Selected, ..Style::new(Color::Cyan) }
                } else {
                    Style::plain()
                };
                lines.push(Line::new(util::pad(&text, self.width), style));
            }
            lines.push(Line::new(hint, Style::new(Color::Dim)));
            lines
        }
}
