//! The live region: what is drawn below the transcript, and how it is redrawn.
//!
//! The region is the streaming preview, the prompt and the footer. Redrawing is where the
//! caret is at risk, so the rules here are strict: a tick may repaint the spinner and the
//! rows above it, and nothing below.

use super::*;

impl Screen {

        /// Live rows: the prompt while editing, the command menu, and the footer — plus where
        /// the terminal cursor belongs among them.
        pub(super) fn compose_live(&self) -> (Vec<Line>, Option<(usize, usize)>) {
            let mut cursor = None;
            let mut lines: Vec<Line> = Vec::new();
            if let Some(thinking) = &self.streaming_thinking {
                // The preview occupies a **fixed** block, padded with blanks, so the rows below it
                // never move. Sizing the block to the text instead made the input line slide down
                // a row each time the thinking grew past a line — and since thinking arrives a few
                // characters at a time, the caret the user is typing at wandered up and down the
                // screen for the whole turn. A blank row is invisible; a moving caret is not.
                let plain = util::sanitize(thinking);
                let wrapped = util::wrap(&util::one_line(&plain), self.width);
                let tail = wrapped.len().saturating_sub(Defaults::THINKING_PREVIEW_LINES);
                for line in &wrapped[tail..] {
                    lines.push(Line::new(line.clone(), Style::new(Color::Dim)));
                }
                self.pad_preview(&mut lines);
                self.trim_live(&mut lines);
            }
            if let Some(answer) = &self.streaming_answer {
                // The live preview re-renders on every token, so it lays out at the current
                // width; whatever it draws is thrown away and re-rendered when the stream ends.
                //
                // `markdown::render` returns **unwrapped** lines — wrapping is the caller's
                // job, since only the caller knows the width to wrap to. Skipping it here meant
                // any line past the right edge was wrapped by the *terminal* instead, which
                // knows nothing about this code's row count: every draw then erased one row
                // fewer than it drew, the region crept down a row per frame, and the old copy
                // stayed on screen — the same line repeated down the screen, cut off at the
                // edge, looking exactly like the model repeating itself.
                let body = wrap_all(&crate::ui::markdown::render(answer, self.width), self.width);
                lines.extend(body);
                self.trim_live(&mut lines);
            }
            if let Some(spans) = &self.running_call {
                lines.extend(wrap_line(&Line::spans(spans.clone()), self.width));
                lines.push(Line::blank());
                self.trim_live(&mut lines);
            }
            // The spinner sits directly above the input, at its left, the way pi draws it at the
            // top of the editor. It is part of the live region, so it disappears with it.
            if let Some(label) = &self.working {
                let frame = WORKING_FRAMES[self.working_frame % WORKING_FRAMES.len()];
                lines.push(Line::spans(vec![
                    Span::new(frame, Style::new(Color::Cyan)),
                    Span::plain(" "),
                    Span::new(label.clone(), Style::new(Color::Blue)),
                ]));
            }
            if let Some(editing) = &self.editing {
                // The buffer wraps onto as many rows as it needs, so a long line stays fully
                // readable — scrolling sideways would hide the beginning of what was typed and
                // give no way to get back to it. The prompt occupies the first two columns of
                // the first row only.
                let prompt_width = 2;
                let width = self.width.max(prompt_width + 1);
                let rows = input_layout(&editing.text(), width, prompt_width);
                let base = lines.len();
                for (index, row) in rows.iter().enumerate() {
                    // The prefix is drawn on every row: on continuation rows it is spaces, and
                    // skipping it would lose the alignment that makes the wrap readable.
                    let style = if index == 0 {
                        Style::new(Color::Cyan)
                    } else {
                        Style::plain()
                    };
                    lines.push(Line::spans(vec![
                        Span::new(row.prefix.clone(), style),
                        Span::plain(row.text.clone()),
                    ]));
                }
                // The caret is wherever the buffer left it, not necessarily at the end: the whole
                // point of Left/Right is to put it in the middle and type there. The column is a
                // display column, so a CJK character counts as the two cells it occupies.
                let (row, column) = input_caret(&rows, editing.caret());
                cursor = Some((base + row, prompt_width + column));
            }
            // Lines waiting for the turn in flight. They sit directly above the input line, where
            // the user just typed them, so it is obvious they were taken and are queued rather
            // than lost.
            for queued in &self.pending {
                // Wrapped for the same reason the answer is: an unwrapped row is wrapped by the
                // terminal instead, and the row count this code erases by is then wrong.
                lines.extend(wrap_line(
                    &Line::spans(vec![
                        Span::new("… ", Style::new(Color::Dim)),
                        Span::new(util::one_line(queued.text()), Style::new(Color::Dim)),
                    ]),
                    self.width,
                ));
            }
            // Pending images and one-off notices sit between the input and the menu: they are
            // about what is being composed, so they belong next to it.
            for image in &self.pending_images {
                lines.push(Line::new(image.label(), Style::new(Color::Magenta)));
            }
            if let Some(notice) = &self.notice {
                lines.extend(wrap_line(&Line::new(notice.clone(), Style::new(Color::Yellow)), self.width));
            }
            // The menu goes directly under the input line, above the footer.
            if !self.menu.is_empty() {
                for (index, (name, help)) in self.menu.iter().enumerate() {
                    let selected = index == self.menu_selected;
                    let marker = if selected { "› " } else { "  " };
                    // The command name is what is being picked, so it keeps the accent colour and
                    // only gains weight when highlighted. Painting the whole row grey made the
                    // list read as disabled text: the names are the point, the descriptions are
                    // the aside.
                    let name_style = if selected {
                        Style { bold: true, ..Style::new(Color::Cyan) }
                    } else {
                        Style::new(Color::Cyan)
                    };
                    lines.push(Line::spans(vec![
                        Span::new(marker, name_style),
                        Span::new(format!("/{name}"), name_style),
                        Span::new("  ", Style::plain()),
                        Span::new(util::truncate(help, self.width.saturating_sub(6).min(60), "…"), Style::new(Color::Dim)),
                    ]));
                }
            }
            lines.extend(self.footer.iter().cloned());
            (lines, cursor)
        }


        /// Keep the live region smaller than the screen, dropping the oldest preview rows.
        pub(super) fn trim_live(&self, lines: &mut Vec<Line>) {
            let budget = self.height.saturating_sub(self.footer.len() + 2);
            if lines.len() > budget {
                let drop = lines.len() - budget;
                lines.drain(..drop);
            }
        }


        /// Top the thinking preview up to its full height with blank rows.
        ///
        /// The preview is a box of a fixed number of rows that fills in from the top as the text
        /// arrives. That is what keeps everything under it — the input line, its caret, the footer
        /// — in the same place for the whole turn. A preview that grew with its text dragged the
        /// caret down the screen a row at a time.
        pub(super) fn pad_preview(&self, lines: &mut Vec<Line>) {
            while lines.len() < Defaults::THINKING_PREVIEW_LINES {
                lines.push(Line::blank());
            }
        }


        /// Commit finished transcript blocks to the terminal.
        ///
        /// Only new blocks are written, one line at a time, so the terminal's own scrollback
        /// holds the conversation. Committed output is never redrawn — that is what keeps the
        /// history usable with the terminal's scroll keys.
        /// Write any block that has not been printed yet.
        pub(super) fn commit(&mut self) {
            if self.printed >= self.blocks.len() {
                return;
            }
            let fresh: Vec<Line> = self.blocks[self.printed..]
                .iter()
                .flat_map(|block| block.render(self.width))
                .collect();
            self.printed = self.blocks.len();
            self.write_lines(&fresh, true);
        }


        /// Write lines to the terminal, stepping out of the live region first so committed rows
        /// never land on top of it.
        pub(super) fn write_lines(&mut self, lines: &[Line], below_live: bool) {
            if lines.is_empty() {
                return;
            }
            if self.interactive && below_live {
                self.erase_live();
            }
            let mut buffer = String::new();
            for line in lines {
                buffer.push_str(&self.paint(line, self.width));
                buffer.push_str("\r\n");
            }
            let _ = write!(self.out, "{buffer}");
        }


        /// Remove the live region, leaving the cursor where the region started.
        ///
        /// The cursor may be parked *inside* the region (on the input row) or just past its end
        /// (while streaming), so the distance back to the first live row differs between the two.
        /// Getting this wrong is not merely cosmetic: erasing from the wrong row leaves the rest
        /// of the old frame on screen, and the next frame is then drawn below it, which pushes
        /// the transcript up by however many rows were missed — once per redraw.
        ///
        /// This only queues the clear. The caller flushes, so a redraw can send the clear and the
        /// new frame together. Flushing the clear on its own is the blank frame that flickered
        /// under the spinner.
        pub(super) fn erase_live(&mut self) {
            if !self.interactive || self.live_rows == 0 {
                return;
            }
            // Rows to climb to reach the top of the region.
            //
            // The cursor is either parked inside it (row `r`, so `r` rows below the top) or left
            // on the last drawn row, which is `live_rows - 1` below the top. Moving up *this*
            // many rows lands on the first live row; moving up any more would climb past it and
            // clear committed transcript instead, one row of it per redraw.
            let up = match self.cursor_row {
                Some(row) => row,
                None => self.live_rows.saturating_sub(1),
            };
            if up > 0 {
                let _ = queue!(self.out, cursor::MoveToPreviousLine(up as u16));
            }
            let _ = queue!(
                self.out,
                cursor::MoveToColumn(0),
                terminal::Clear(terminal::ClearType::FromCursorDown)
            );
            self.live_rows = 0;
            self.cursor_row = None;
            self.cursor_col = 0;
            self.last_live.clear();
        }


        /// Re-render the live region: the streaming preview (if any), the prompt (if editing)
        /// and the footer. Everything above it is left alone.
        pub fn render(&mut self) {
            self.draw_live();
        }


        /// Show or hide the terminal cursor, remembering the state so a redraw that does not
        /// change it does not write the escape again.
        pub(super) fn set_cursor_visible(&mut self, visible: bool) {
            if !self.interactive || self.cursor_shown == Some(visible) {
                return;
            }
            // Queued, not executed: `execute` flushes, and a flush here would reveal the cleared
            // region before the new frame is written.
            if visible {
                let _ = queue!(self.out, cursor::Show);
            } else {
                let _ = queue!(self.out, cursor::Hide);
            }
            self.cursor_shown = Some(visible);
        }


        pub(super) fn draw_live(&mut self) {
            if !self.interactive {
                self.commit();
                return;
            }
            let (lines, cursor) = self.compose_live();
            // One frame: the clear and the replacement go out together. Terminals that understand
            // synchronized updates hold the old picture until the frame ends, so the footer does
            // not blink off between them.
            let _ = queue!(self.out, BeginSynchronizedUpdate);
            self.commit();
            if self.live_rows > 0 {
                self.erase_live();
            }
            let mut buffer = String::new();
            // Separate rows with CRLF but do **not** end the last one with it. A trailing newline
            // leaves the cursor on a row that has nothing in it, and that empty row is the blank
            // line under the footer: the live region is one row taller than what it draws.
            //
            // After the loop the cursor sits at the start of the row *after* the last drawn one
            // (or on it, if the last row exactly filled the width). Both `erase_live` and the park
            // below are written against that position.
            for (index, line) in lines.iter().enumerate() {
                if index > 0 {
                    buffer.push_str("\r\n");
                }
                buffer.push_str(&self.paint(line, self.width));
            }
            // Park the cursor inside the input row.
            //
            // Having drawn `lines.len()` rows without a final newline, the cursor is one row below
            // the last drawn row, so it needs `lines.len() - 1 - row` steps up — one fewer than
            // the number of rows. With no cursor target (streaming, no input line) it is left on
            // that row, which is where `erase_live` expects to find it.
            if let Some((row, column)) = cursor {
                let up = lines.len().saturating_sub(1).saturating_sub(row);
                if up > 0 {
                    buffer.push_str(&format!("\u{1b}[{up}A"));
                }
                buffer.push_str(&format!("\u{1b}[{}G", column + 1));
            }
            // The caret is shown only where it marks something: the position the next character
            // of the input line will go. That is true for the whole session now — the composer
            // stays armed through a turn, so the caret is there to type into while the answer
            // streams — and false only when there is no input line at all, as between a command
            // tearing the transcript down and the next prompt arming it. A terminal cursor left
            // over from the last write sits at the bottom of the region blinking at nothing, and
            // "waiting for you" versus "waiting for the model" is the one thing the screen has to
            // make obvious.
            self.set_cursor_visible(cursor.is_some());
            let _ = write!(self.out, "{buffer}");
            let _ = queue!(self.out, EndSynchronizedUpdate);
            let _ = self.out.flush();
            self.live_rows = lines.len();
            // Where the cursor was left, so the next erase starts from the right row. With no
            // cursor target it rests on the last drawn row, which is `live_rows - 1` rows below
            // the top — `erase_live` derives that from `None` rather than storing it.
            self.cursor_row = cursor.map(|(row, _)| row);
            self.cursor_col = cursor.map(|(_, column)| column).unwrap_or(0);
            self.last_live = lines;
        }


        /// Rewrite the rows that changed, when nothing below the spinner did.
        ///
        /// A turn ticks this every 80ms. Redrawing the input line and the footer on each tick
        /// clears them and paints them again, which reads as flicker — and it takes the caret
        /// with it, because the caret lives in the input row. So the tick is allowed to touch
        /// only the rows at or above the spinner: the thinking preview above it changes on every
        /// tick, and the spinner itself changes with each frame, while everything below stays put.
        pub(super) fn repaint_spinner(&mut self) -> bool {
            if !self.interactive || self.live_rows == 0 || self.last_live.is_empty() {
                return false;
            }
            let label = self.working.clone().unwrap_or_default();
            let (lines, cursor) = self.compose_live();
            if lines.len() != self.live_rows || lines.len() != self.last_live.len() {
                return false;
            }
            let Some(spinner) = spinner_row(&lines, &label) else {
                return false;
            };
            let Some(top) = dirty_top(&self.last_live, &lines, spinner) else {
                return false;
            };
            // Climb from the caret to the first row that needs repainting.
            let from = match self.cursor_row {
                Some(row) if row >= spinner => row,
                _ => return false,
            };
            let up = from - top;
            // The frame is built as a string first: it is a single write to the terminal, and it
            // is a value the tests can inspect without a terminal to write to.
            let frame = self.spinner_frame(&lines[top..=spinner], up, from - spinner, cursor);
            let _ = queue!(self.out, BeginSynchronizedUpdate);
            let _ = write!(self.out, "{frame}");
            let _ = queue!(self.out, EndSynchronizedUpdate);
            let _ = self.out.flush();
            self.last_live = lines;
            true
        }


        /// The escape sequence that repaints `rows` in place, leaving the caret on `cursor`.
        ///
        /// `up` is how far to climb to reach the first of them and `down` how far to come back to
        /// the caret. Nothing outside `rows` is written, which is what keeps the input line and
        /// its caret stable: the tick runs at 12Hz and a row redrawn without need is a row that
        /// visibly blinks.
        pub(super) fn spinner_frame(
            &self,
            rows: &[Line],
            up: usize,
            down: usize,
            cursor: Option<(usize, usize)>,
        ) -> String {
            let mut frame = String::new();
            if up > 0 {
                frame.push_str(&format!("\u{1b}[{up}A"));
            }
            for (index, line) in rows.iter().enumerate() {
                if index > 0 {
                    frame.push_str("\r\n");
                }
                // `\r` rather than a column escape: it is column 0 in every terminal, with no
                // parameter to interpret. `\u{1b}[K` clears what a previously longer row left behind.
                frame.push_str(&format!("\r\u{1b}[K{}", self.paint(line, self.width)));
            }
            if down > 0 {
                frame.push_str(&format!("\u{1b}[{down}B"));
            }
            let column = cursor.map(|(_, column)| column).unwrap_or(self.cursor_col);
            frame.push_str(&format!("\u{1b}[{}G", column + 1));
            frame
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
                let text = if span.style.italic { self.theme.italic(&text) } else { text };
                let text = if span.style.underline { self.theme.underline(&text) } else { text };
                let text = if span.style.bold { self.theme.bold(&text) } else { text };
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
