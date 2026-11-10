//! Layout and painting of the inline viewport.

use super::*;

impl Screen {
    pub(super) fn compose_live(&self) -> (Vec<HistoryRow<Line>>, Option<(usize, usize)>) {
        if let Some(picker) = &self.picker {
            return (
                self.choice_lines(
                    &picker.title,
                    &picker.hint,
                    &picker.choices,
                    picker.selected,
                )
                .into_iter()
                .map(HistoryRow::hard)
                .collect(),
                None,
            );
        }
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
                let mut marker = if index == 0 && first > 0 {
                    format!("待处理（另 {first} 条）")
                } else {
                    "待处理".into()
                };
                if first + index + 1 == self.pending.len() {
                    marker.push_str(" · Alt+↑ 编辑");
                }
                let mut text = util::one_line(&queued.text);
                if !queued.images.is_empty() {
                    text.push_str(&format!(" [{} 张图片]", queued.images.len()));
                }
                above.push(Line::dim(format!("{marker}：{text}")));
            }
            room = room.saturating_sub(above.len());
        }
        let mut preview = Vec::new();
        if let Some(answer) = &self.streaming_answer {
            preview.extend(
                self.answer_layout
                    .tail(answer, self.width, self.height)
                    .iter()
                    .cloned(),
            );
        }
        if let Some(thinking) = &self.streaming_thinking
            && self.thinking_active
        {
            let wrapped = util::wrap(&util::one_line(thinking), self.width);
            let first = wrapped
                .len()
                .saturating_sub(Defaults::THINKING_PREVIEW_LINES);
            let thinking_rows = wrapped.len() - first;
            preview.extend(
                wrapped[first..]
                    .iter()
                    .map(|text| HistoryRow::hard(Line::dim(text.clone()))),
            );
            for _ in thinking_rows..Defaults::THINKING_PREVIEW_LINES {
                preview.push(HistoryRow::hard(Line::blank()));
            }
        }
        for spans in &self.running_calls {
            let mut suffix = Vec::new();
            if let Some(started) = self.running_started {
                let elapsed = started.elapsed().as_secs();
                if elapsed >= 1 {
                    suffix.push(Span::new(format!(" · {elapsed}s"), Style::new(Color::Dim)));
                }
            }
            if self.running_log.is_some() && self.width >= 40 {
                suffix.push(Span::new(" · Ctrl+O 日志", Style::new(Color::Dim)));
            }
            let suffix = Line::spans(suffix);
            let mut line =
                Line::spans(spans.clone()).truncate(self.width.saturating_sub(suffix.width()));
            line.spans.extend(suffix.spans);
            preview.push(HistoryRow::hard(line.truncate(self.width)));
        }
        if !self.running_calls.is_empty() {
            preview.push(HistoryRow::hard(Line::blank()));
        }
        let first = preview.len().saturating_sub(room);
        let mut lines = preview.split_off(first);
        lines.extend(above.into_iter().map(HistoryRow::hard));
        if working {
            lines.push(HistoryRow::hard(Line::spans(vec![
                Span::new(
                    WORKING_FRAMES[self.working_frame % WORKING_FRAMES.len()],
                    Style::new(Color::Cyan),
                ),
                Span::plain(" "),
                Span::new(self.working.clone().unwrap(), Style::new(Color::Blue)),
            ])));
        }
        let cursor = caret.map(|(row, column)| (lines.len() + row, column));
        lines.extend(composer.into_iter().map(HistoryRow::hard));
        lines.extend(footer.iter().cloned().map(HistoryRow::hard));
        if lines.is_empty() {
            lines.push(HistoryRow::hard(Line::blank()));
        }
        let cursor = cursor.map(|(row, column)| (row, column.min(self.width.saturating_sub(1))));
        (
            lines
                .iter()
                .map(|row| HistoryRow {
                    line: row.line.truncate(self.width),
                    wrapped: row.wrapped,
                })
                .collect(),
            cursor,
        )
    }

    pub(super) fn commit(&mut self) {
        if self.interactive {
            self.draw_live();
        } else {
            for row in self.fresh_history() {
                let _ = write!(self.out, "{}", row.line.text());
                if !row.wrapped {
                    let _ = writeln!(self.out);
                }
            }
        }
    }

    fn fresh_history(&mut self) -> Vec<HistoryRow<Line>> {
        let mut lines = Vec::new();
        for index in self.printed..self.blocks.len() {
            let rows = self.layout.rows(index, self.width, &self.blocks[index]);
            lines.extend(rows.iter().cloned());
        }
        self.printed = self.blocks.len();
        lines
    }

    pub(super) fn erase_live(&mut self) {
        if let Some(view) = self.transcript_view.as_mut() {
            view.rendered = None;
        }
        if self.interactive && self.detail.is_none() {
            let _ = write!(self.out, "{}", self.viewport.erase());
        }
    }

    pub fn render(&mut self) {
        self.redraw.request();
        self.flush_pending_frame();
    }

    pub(super) fn flush_pending_frame(&mut self) {
        if self.redraw.ready(std::time::Instant::now()) {
            self.draw_live();
        }
    }

    pub(super) fn draw_live(&mut self) {
        self.redraw.drawn(std::time::Instant::now());
        if self.interactive && !self.alternate_screen {
            let _ = crate::ui::terminal::ensure_raw_mode();
            let _ = crossterm::execute!(
                self.out,
                terminal::EnterAlternateScreen,
                event::EnableMouseCapture,
                terminal::Clear(terminal::ClearType::All),
                cursor::MoveTo(0, 0)
            );
            self.alternate_screen = true;
            self.viewport.clear();
        }
        if self.detail.is_some() {
            self.draw_detail();
            return;
        }
        if let Some((panel, _)) = self.approval.as_mut() {
            if panel.draw(&mut self.out, &self.theme).is_err()
                && let Some((_, sender)) = self.approval.take()
            {
                let _ = sender.send(crate::auth::guard::Decision::Deny);
            }
            return;
        }
        if !self.interactive {
            self.commit();
            return;
        }
        if self.transcript_view.is_some() {
            self.draw_transcript();
            return;
        }
        let frame = self.live_frame();
        let _ = write!(self.out, "{frame}");
        let _ = self.out.flush();
    }

    pub(super) fn live_frame(&mut self) -> String {
        let (lines, cursor) = self.compose_live();
        let reflow = if self
            .viewport
            .needs_reflow(self.width, self.height, lines.len())
        {
            // The viewport only repaints its visible tail. Stop once that tail is filled.
            let mut tail = std::collections::VecDeque::new();
            let mut count = 0;
            for index in (0..self.printed).rev() {
                let rows = self.layout.rows(index, self.width, &self.blocks[index]);
                count += rows.len();
                tail.push_front(rows);
                if count >= self.height {
                    break;
                }
            }
            tail.iter()
                .flat_map(|rows| rows.iter())
                .skip(count.saturating_sub(self.height))
                .map(|row| HistoryRow {
                    line: self.paint(&row.line),
                    wrapped: row.wrapped,
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let history = self
            .fresh_history()
            .iter()
            .map(|row| HistoryRow {
                line: self.paint(&row.line),
                wrapped: row.wrapped,
            })
            .collect::<Vec<_>>();
        let rows = lines
            .iter()
            .map(|row| HistoryRow {
                line: self.paint(&row.line),
                wrapped: row.wrapped,
            })
            .collect();
        self.viewport
            .draw((self.width, self.height), rows, cursor, &history, &reflow)
    }

    pub(super) fn paint(&self, line: &Line) -> String {
        let mut out = String::new();
        for span in &line.spans {
            let text = self.theme.fg(span.style.fg, &span.text);
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
            let text = match span.style.bg {
                Bg::Added => self.theme.bg_added(&text),
                Bg::Removed => self.theme.bg_removed(&text),
                Bg::Selected => self.theme.bg_selected(&text),
                Bg::None => text,
            };
            out.push_str(&text);
        }
        out
    }
}
