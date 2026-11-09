//! List pickers share the event loop with streaming output and tool execution.

use super::*;

/// Columns between the widest label and the detail column beside it.
const LABEL_GAP: usize = 2;

/// Below this there is no room for a detail column, so the detail is left out entirely.
/// Fourteen covers the shortest real one ("直接执行，不再询问" is 9) with room for an
/// ellipsis, and keeps a squeezed terminal from printing a stub no one can read.
const MIN_DETAIL_WIDTH: usize = 14;

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

pub(super) struct Picker {
    pub(super) title: String,
    pub(super) hint: String,
    pub(super) choices: Vec<Choice>,
    pub(super) selected: usize,
}

impl Screen {
    /// Show a list next to the footer and let the user choose one entry.
    ///
    /// The highlighted entry starts on the first one, so Enter accept the obvious default.
    /// Returns `None` for Esc / Ctrl+C / `q`, and the index of the chosen entry otherwise.
    pub fn pick(&mut self, title: &str, items: &[String]) -> Option<usize> {
        self.pick_at(title, items, 0)
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
        if crate::ui::terminal::ensure_raw_mode().is_err() {
            return None;
        }
        self.open_picker(title, hint, choices.to_vec(), initial);
        let selected = loop {
            if crate::ui::terminal::exit_signal().is_some() {
                break None;
            }
            self.flush_pending_frame();
            match event::poll(super::redraw::FRAME_INTERVAL) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => break None,
            }
            let Ok(event) = event::read() else { break None };
            if let Some(Action::Selection(selected)) = self.absorb_event(event) {
                break selected;
            }
        };
        self.picker = None;
        self.draw_live();
        selected
    }

    /// Open a picker without blocking the request or tool currently running.
    pub(crate) fn open_picker(
        &mut self,
        title: &str,
        hint: &str,
        choices: Vec<Choice>,
        initial: usize,
    ) {
        if choices.is_empty() {
            return;
        }
        self.close_views();
        self.erase_live();
        self.picker = Some(Picker {
            title: title.into(),
            hint: hint.into(),
            selected: initial.min(choices.len() - 1),
            choices,
        });
        self.render();
    }

    pub(super) fn picker_event(&mut self, event: Event) -> Option<Action> {
        if let Event::Resize(width, height) = event {
            self.width = usize::from(width.max(1));
            self.height = usize::from(height.max(1));
            return None;
        }
        let key = match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => key,
            Event::Mouse(mouse) => match mouse.kind {
                event::MouseEventKind::ScrollUp => KeyCode::Up.into(),
                event::MouseEventKind::ScrollDown => KeyCode::Down.into(),
                _ => return None,
            },
            _ => return None,
        };
        let picker = self.picker.as_mut().expect("open picker");
        let count = picker.choices.len();
        let selected = match key.code {
            KeyCode::Enter => Some(Some(picker.selected)),
            KeyCode::Esc => Some(None),
            KeyCode::Char('c' | 'q')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.code == KeyCode::Char('q') =>
            {
                Some(None)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                picker.selected = (picker.selected + count - 1) % count;
                None
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                picker.selected = (picker.selected + 1) % count;
                None
            }
            KeyCode::Home => {
                picker.selected = 0;
                None
            }
            KeyCode::End => {
                picker.selected = count - 1;
                None
            }
            KeyCode::PageUp => {
                picker.selected = picker
                    .selected
                    .saturating_sub(self.height.saturating_sub(3).clamp(1, 12));
                None
            }
            KeyCode::PageDown => {
                picker.selected =
                    (picker.selected + self.height.saturating_sub(3).clamp(1, 12)).min(count - 1);
                None
            }
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                let index = c as usize - '1' as usize;
                (index < count).then_some(Some(index))
            }
            _ => None,
        };
        if let Some(selected) = selected {
            self.picker = None;
            return Some(Action::Selection(selected));
        }
        None
    }

    pub(super) fn choice_lines(
        &self,
        title: &str,
        hint: &str,
        choices: &[Choice],
        cursor: usize,
    ) -> Vec<Line> {
        // One row per entry, as every other picker has. The detail is a second *column*
        // rather than a second row: the two things being compared are side by side, so the
        // eye reads them against each other, and a two-entry menu still fits in two rows.
        let title_rows = usize::from(self.height >= 3);
        let hint_rows = usize::from(self.height >= 2);
        let room = self.height.saturating_sub(title_rows + hint_rows).max(1);
        let cap = room.clamp(1, 12);
        let first = cursor
            .saturating_sub(cap / 2)
            .min(choices.len().saturating_sub(cap));
        let end = (first + cap).min(choices.len());

        // Where the detail column starts. It is one column past the widest label, so the
        // entries line up whatever their labels are; `LABEL_GAP` keeps the two apart when a
        // label is as wide as the terminal allows.
        let marker_width = 2;
        let labels_wide = choices[first..end]
            .iter()
            .map(|choice| util::width(&choice.label))
            .max()
            .unwrap_or(0);
        let detail_at = marker_width + labels_wide + LABEL_GAP;
        // With no room left for a column, the detail is dropped rather than wrapped: a
        // second line per entry is the layout this one replaced, and a truncated detail
        // beside a label it cannot be told apart from is worse than no detail.
        let detail_room = self.width.saturating_sub(detail_at);
        let show_details = detail_at < self.width && detail_room >= MIN_DETAIL_WIDTH;

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

            if !show_details {
                let text = util::truncate(&format!("{marker}{}", choice.label), self.width, "…");
                lines.push(Line::new(util::pad(&text, self.width), style));
                continue;
            }

            // Two spans on one row: the label, padded out to the column, and the detail
            // in its own dim style. The selection covers both — it marks the entry, and an
            // entry is its label and its explanation together.
            let mut spans = vec![
                Span::new(
                    util::pad(&format!("{marker}{}", choice.label), detail_at),
                    style,
                ),
                Span::new(
                    util::truncate(choice.detail.as_deref().unwrap_or(""), detail_room, "…"),
                    Style {
                        fg: Color::Dim,
                        bg: style.bg,
                        ..Style::new(Color::Dim)
                    },
                ),
            ];
            // Paint the rest of the row so the transcript cannot show through a selected
            // entry that is shorter than the terminal is wide.
            let used = detail_at + spans.last().map(|s| util::width(&s.text)).unwrap_or(0);
            if used < self.width {
                spans.push(Span::new(" ".repeat(self.width - used), style));
            }
            lines.push(Line::spans(spans));
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
