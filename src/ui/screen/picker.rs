//! Full-screen list pickers: `/model`, `/resume`, and the like.
//!
//! They run *inside* a turn, so each takes raw mode for its duration and gives the screen
//! back the way it found it. The choice is returned rather than acted on: what a picked
//! entry means is the caller's business.

use super::*;

/// One entry in a picker: a label, and an optional second row explaining it.
///
/// The second row exists for the menus where the label alone does not say what picking it
/// does — `/permissions` is the case that asked for it. Lists whose entries are self-evident
/// (`/model`, `/resume`) keep the one-row form, because a description invented to fill the
/// space would be read every time and learned never.
#[derive(Debug, Clone)]
pub struct Choice {
    pub label: String,
    pub detail: Option<String>,
}

impl Choice {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: None,
        }
    }

    pub fn with_detail(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: Some(detail.into()),
        }
    }
}

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

    /// [`Screen::pick_at`] for entries that carry a line of explanation each.
    ///
    /// The chooser stacks them the way the authorization panel stacks its two answers: one
    /// entry per block, the selected block painted across the full width. A choice that
    /// changes what the agent may do without asking deserves to be read as a choice, not as
    /// a line of prose with an em dash in it.
    pub fn pick_choices(
        &mut self,
        title: &str,
        hint: &str,
        choices: &[Choice],
        initial: usize,
    ) -> Option<usize> {
        self.pick_choices_at(title, choices, initial, hint)
    }

    pub(super) fn pick_hinted(
        &mut self,
        title: &str,
        items: &[String],
        initial: usize,
        hint: &str,
    ) -> Option<usize> {
        let choices: Vec<Choice> = items.iter().map(Choice::new).collect();
        self.pick_choices_at(title, &choices, initial, hint)
    }

    fn pick_choices_at(
        &mut self,
        title: &str,
        choices: &[Choice],
        initial: usize,
        hint: &str,
    ) -> Option<usize> {
        if choices.is_empty() {
            return None;
        }
        if !self.interactive {
            // No terminal to choose on: fall back to the highlighted entry.
            return Some(initial.min(choices.len() - 1));
        }
        let mut cursor = initial.min(choices.len() - 1);
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
            self.footer = self.choice_lines(title, hint, choices, cursor);
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
                        choices.len() - 1
                    } else {
                        cursor - 1
                    };
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                    cursor = (cursor + 1) % choices.len();
                }
                KeyCode::Home => cursor = 0,
                KeyCode::End => cursor = choices.len() - 1,
                KeyCode::PageUp => {
                    cursor = cursor.saturating_sub(self.height.saturating_sub(3).clamp(1, 12))
                }
                KeyCode::PageDown => {
                    cursor =
                        (cursor + self.height.saturating_sub(3).clamp(1, 12)).min(choices.len() - 1)
                }
                KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                    let index = c as usize - '1' as usize;
                    if index < choices.len() {
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

    pub(super) fn choice_lines(
        &self,
        title: &str,
        hint: &str,
        choices: &[Choice],
        cursor: usize,
    ) -> Vec<Line> {
        // Rows are the scarce resource, not entries: a described entry is two rows, so the
        // window is planned in rows and the details are spent out of what is left. Reserving
        // first and filling second is what keeps a two-row entry from drawing past the
        // bottom of a one-row terminal.
        let title_rows = usize::from(self.height >= 3);
        let hint_rows = usize::from(self.height >= 2);
        let room = self.height.saturating_sub(title_rows + hint_rows).max(1);
        // One row per entry is the floor; anything beyond that can go to details.
        let cap = room.clamp(1, 12);
        let first = cursor
            .saturating_sub(cap / 2)
            .min(choices.len().saturating_sub(cap));
        let end = (first + cap).min(choices.len());
        let mut spare = room.saturating_sub(end - first);

        // Which entries get to show their second row. The selected one first: that is the
        // explanation currently being read, and it is the one that must not be the one
        // dropped when the space runs out.
        let selected_local = cursor
            .checked_sub(first)
            .filter(|index| *index < end - first);
        let mut explained = vec![false; end - first];
        let spend_detail = |index: usize, explained: &mut Vec<bool>, spare: &mut usize| {
            let local = index - first;
            if *spare > 0 && explained[local].eq(&false) && choices[index].detail.is_some() {
                explained[local] = true;
                *spare -= 1;
            }
        };
        if let Some(local) = selected_local {
            spend_detail(first + local, &mut explained, &mut spare);
        }
        for index in first..end {
            spend_detail(index, &mut explained, &mut spare);
        }

        let mut lines = Vec::new();
        let all_visible = first == 0 && end == choices.len();
        // The counter says where you are in a list that does not fit. When every entry is
        // already on screen it is a number to read for nothing.
        if title_rows == 1 {
            let label = if all_visible {
                title.to_string()
            } else {
                format!("{title}  {}/{}", cursor + 1, choices.len())
            };
            lines.push(Line::new(
                util::truncate(&label, self.width, "…"),
                Style::bold(Color::Cyan),
            ));
        }
        for (index, choice) in choices[first..end].iter().enumerate() {
            let selected = first + index == cursor;
            let marker = if selected {
                "› "
            } else if index == 0 && first > 0 {
                "↑ "
            } else if first + index + 1 == end && end < choices.len() {
                "↓ "
            } else {
                "  "
            };
            let text = util::truncate(&format!("{marker}{}", choice.label), self.width, "…");
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
                    fg: Color::reasoning(&choice.label),
                    bold: selected,
                    ..style
                }
            } else {
                style
            };
            lines.push(Line::new(util::pad(&text, self.width), style));
            if explained[index]
                && let Some(detail) = &choice.detail
            {
                // Aligned under the label, and never highlighted: the background is what
                // marks the entry being chosen, and a second painted row reads as a second
                // choice.
                let text = util::truncate(&format!("  {detail}"), self.width, "…");
                lines.push(Line::new(
                    util::pad(&text, self.width),
                    Style::new(Color::Dim),
                ));
            }
        }
        if hint_rows == 1 {
            lines.push(Line::new(
                util::truncate(hint, self.width, "…"),
                Style::new(Color::Dim),
            ));
        }
        debug_assert!(
            lines.len() <= self.height.max(1),
            "a picker drew {} rows into {}",
            lines.len(),
            self.height
        );
        lines
    }
}
