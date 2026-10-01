//! Layout and painting of the bottom viewport.

use super::*;
use unicode_segmentation::UnicodeSegmentation;

impl Screen {
    pub(super) fn compose_live(&self) -> (Vec<Line>, Option<(usize, usize)>) {
        let budget = self.height.saturating_sub(1).max(1);
        let rendered_footer = self
            .footer_state
            .as_ref()
            .map(|state| crate::ui::footer::render(state, &self.theme, self.width));
        let footer = rendered_footer.as_ref().unwrap_or(&self.footer);
        let editing = self.editing.as_ref();
        let footer_cap = budget.saturating_sub(usize::from(editing.is_some()));
        let footer_start = footer.len().saturating_sub(footer_cap);
        let footer = &footer[footer_start..];
        let mut room = budget.saturating_sub(footer.len());
        let working = self.working.is_some() && room > usize::from(editing.is_some());
        room = room.saturating_sub(usize::from(working));
        let mut composer = Vec::new();
        let mut caret = None;
        if let Some(editor) = editing {
            let prefix = if self.width >= 3 { 2 } else { 0 };
            let rows = input_layout(&editor.text(), self.width, prefix);
            let (row, column) = input_caret(&rows, editor.caret());
            let cap = room.min((self.height / 3).clamp(1, 8)).max(1);
            let first = row.saturating_sub(cap - 1);
            let end = (first + cap).min(rows.len());
            for (index, shown) in rows[first..end].iter().enumerate() {
                let marker = if prefix == 0 {
                    ""
                } else if index == 0 && first > 0 {
                    "↑ "
                } else if index + 1 == end - first && end < rows.len() {
                    "↓ "
                } else {
                    &shown.prefix
                };
                composer.push(Line::spans(vec![
                    Span::new(marker, Style::new(Color::Cyan)),
                    Span::plain(shown.text.clone()),
                ]));
            }
            caret = Some((row - first, prefix + column));
        }
        room = room.saturating_sub(composer.len());
        if !self.pending_images.is_empty() && room > 0 {
            let label = if self.pending_images.len() == 1 {
                self.pending_images[0].label()
            } else {
                format!("已附加 {} 张图片", self.pending_images.len())
            };
            composer.push(Line::new(label, Style::new(Color::Magenta)));
            room -= 1;
        }
        if let Some(notice) = &self.notice
            && room > 0
        {
            composer.push(Line::new(util::one_line(notice), Style::new(Color::Yellow)));
            room -= 1;
        }
        let menu_cap = room.min(8);
        if !self.menu.is_empty() && menu_cap > 0 {
            let first = self
                .menu_selected
                .saturating_sub(menu_cap / 2)
                .min(self.menu.len().saturating_sub(menu_cap));
            let end = (first + menu_cap).min(self.menu.len());
            for (index, (name, help)) in self.menu[first..end].iter().enumerate() {
                let selected = first + index == self.menu_selected;
                let marker = if selected {
                    "› "
                } else if index == 0 && first > 0 {
                    "↑ "
                } else if first + index + 1 == end && end < self.menu.len() {
                    "↓ "
                } else {
                    "  "
                };
                let style = Style {
                    bold: selected,
                    ..Style::new(Color::Cyan)
                };
                composer.push(Line::spans(vec![
                    Span::new(marker, style),
                    Span::new(format!("/{name}"), style),
                    Span::new(format!("  {help}"), Style::new(Color::Dim)),
                ]));
            }
            room = room.saturating_sub(end - first);
        }
        let mut above = Vec::new();
        let queue_cap = room.min(2);
        if queue_cap > 0 && !self.pending.is_empty() {
            let first = self.pending.len().saturating_sub(queue_cap);
            for (index, queued) in self.pending[first..].iter().enumerate() {
                let marker = if index == 0 && first > 0 {
                    format!("… +{first} · ")
                } else {
                    "… ".into()
                };
                above.push(Line::dim(format!(
                    "{marker}{}",
                    util::one_line(queued.text())
                )));
            }
            room = room.saturating_sub(above.len());
        }
        let mut preview = Vec::new();
        if let Some(thinking) = &self.streaming_thinking
            && !self.thinking_committed
        {
            let wrapped = util::wrap(&util::one_line(thinking), self.width);
            let first = wrapped
                .len()
                .saturating_sub(Defaults::THINKING_PREVIEW_LINES);
            preview.extend(wrapped[first..].iter().map(|text| Line::dim(text.clone())));
            while preview.len() < Defaults::THINKING_PREVIEW_LINES {
                preview.push(Line::blank());
            }
        }
        if let Some(answer) = &self.streaming_answer {
            preview.extend(wrap_all(
                &crate::ui::markdown::render(&answer[self.streaming_committed..], self.width),
                self.width,
            ));
        }
        if let Some(spans) = &self.running_call {
            preview.extend(wrap_line(&Line::spans(spans.clone()), self.width));
            preview.push(Line::blank());
        }
        let first = preview.len().saturating_sub(room);
        let mut lines = preview.split_off(first);
        lines.extend(above);
        if working {
            lines.push(Line::spans(vec![
                Span::new(
                    WORKING_FRAMES[self.working_frame % WORKING_FRAMES.len()],
                    Style::new(Color::Cyan),
                ),
                Span::plain(" "),
                Span::new(self.working.clone().unwrap(), Style::new(Color::Blue)),
            ]));
        }
        let cursor = caret.map(|(row, column)| (lines.len() + row, column));
        lines.extend(composer);
        lines.extend(footer.iter().cloned());
        if lines.is_empty() {
            lines.push(Line::blank());
        }
        let cursor = cursor.map(|(row, column)| (row, column.min(self.width.saturating_sub(1))));
        (
            lines
                .iter()
                .map(|line| clip_line(line, self.width))
                .collect(),
            cursor,
        )
    }

    pub(super) fn commit(&mut self) {
        if self.interactive {
            self.draw_live();
        } else {
            let lines = self.fresh_lines();
            for line in lines {
                let _ = writeln!(self.out, "{}", line.text());
            }
        }
    }

    fn fresh_lines(&mut self) -> Vec<Line> {
        let mut lines: Vec<_> = self.blocks[self.printed..]
            .iter()
            .flat_map(|block| block.render(self.width))
            .collect();
        lines.append(&mut self.updates);
        self.printed = self.blocks.len();
        lines
    }

    pub(super) fn erase_live(&mut self) {
        if self.interactive {
            let _ = write!(self.out, "{}", self.viewport.erase());
        }
    }

    pub fn render(&mut self) {
        self.draw_live();
    }

    pub(super) fn draw_live(&mut self) {
        if !self.interactive {
            self.commit();
            return;
        }
        let frame = self.live_frame();
        let _ = write!(self.out, "{frame}");
        let _ = self.out.flush();
    }

    pub(super) fn live_frame(&mut self) -> String {
        self.commit_stream_prefix();
        let (lines, cursor) = self.compose_live();
        let reflow = if self.viewport.needs_reflow(self.width, self.height) {
            self.blocks[..self.printed]
                .iter()
                .flat_map(|block| block.render(self.width))
                .map(|line| self.paint(&line, self.width))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let history = self
            .fresh_lines()
            .iter()
            .map(|line| self.paint(line, self.width))
            .collect::<Vec<_>>();
        let rows = lines
            .iter()
            .map(|line| self.paint(line, self.width))
            .collect();
        self.viewport
            .draw((self.width, self.height), rows, cursor, &history, &reflow)
    }

    fn commit_stream_prefix(&mut self) {
        let Some(answer) = &self.streaming_answer else {
            return;
        };
        let start = self.streaming_committed;
        let end = start + crate::ui::markdown::stable_prefix(&answer[start..]);
        if end == start {
            return;
        }
        let prefix = &answer[start..end];
        if prefix.trim().is_empty() {
            self.streaming_committed = end;
            return;
        }
        if !self.thinking_committed
            && self
                .streaming_thinking
                .as_ref()
                .is_some_and(|text| !text.trim().is_empty())
        {
            self.blocks
                .push(Block::lines(crate::ui::compact::thinking_done_lines()));
            self.thinking_committed = true;
        }
        self.blocks.push(Block::markdown(prefix));
        self.streaming_committed = end;
    }

    pub(super) fn paint(&self, line: &Line, pad_to: usize) -> String {
        let mut out = String::new();
        let mut used = 0usize;
        for span in &line.spans {
            let background = match span.style.bg {
                Bg::None => None,
                Bg::Added => Some(Bg::Added),
                Bg::Removed => Some(Bg::Removed),
                Bg::Selected => Some(Bg::Selected),
            };
            // A non-breaking space is a wrapping instruction, not a glyph: it kept a list
            // marker with its text while the line was being broken, and the terminal gets a
            // plain space now that the decision is made.
            let text = if span.text.contains('\u{a0}') {
                span.text.replace('\u{a0}', " ")
            } else {
                span.text.clone()
            };
            let text = self.theme.fg(span.style.fg, &text);
            // Order matters: SGR 1/3/4 are attributes that 22/23/24 turn off, and the colour
            // reset is 39. Nesting them the other way round would have the colour reset also
            // clear the weight of a bold heading.
            let text = if span.style.italic {
                self.theme.italic(&text)
            } else {
                text
            };
            let text = if span.style.crossed_out {
                format!("\u{1b}[9m{text}\u{1b}[29m")
            } else {
                text
            };
            let text = if span.style.underline {
                self.theme.underline(&text)
            } else {
                text
            };
            let text = if span.style.bold {
                self.theme.bold(&text)
            } else {
                text
            };
            let text = match background {
                Some(Bg::Added) => self.theme.bg_added(&text),
                Some(Bg::Removed) => self.theme.bg_removed(&text),
                Some(Bg::Selected) => self.theme.bg_selected(&text),
                _ => text,
            };
            used += util::width(&span.text);
            out.push_str(&text);
        }
        if let Some(fill) = line.spans.first().map(|span| span.style.bg)
            && pad_to > used
            && fill != Bg::None
        {
            let padding = " ".repeat(pad_to - used);
            out.push_str(&match fill {
                Bg::Added => self.theme.bg_added(&padding),
                Bg::Removed => self.theme.bg_removed(&padding),
                Bg::Selected => self.theme.bg_selected(&padding),
                Bg::None => padding,
            });
        }
        out
    }
}

fn clip_line(line: &Line, width: usize) -> Line {
    let mut spans = Vec::new();
    let mut used = 0;
    for span in &line.spans {
        let mut text = String::new();
        for grapheme in span.text.graphemes(true) {
            let cells = util::width(grapheme);
            if used + cells > width {
                break;
            }
            used += cells;
            text.push_str(grapheme);
        }
        spans.push(Span::new(text, span.style));
        if used >= width {
            break;
        }
    }
    Line::spans(spans)
}
