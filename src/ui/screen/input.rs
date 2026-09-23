//! Reading keys: the prompt, and the same keys while a turn is in flight.
//!
//! There is one entry point for both, which is the point — a second path for "busy" input
//! would be a second set of rules and a second set of bugs.

use super::*;

impl Screen {

        /// Up: one entry further back, or the oldest entry when the walk begins.
        ///
        /// The first press stashes the buffer, so whatever was half-typed comes back when the
        /// walk returns to the bottom. Pressing Up at the oldest entry does nothing rather than
        /// wrapping to the newest: wrapping makes it impossible to tell the top of the history
        /// from the bottom, and a stray key press would then land on a different entry entirely.
        ///
        /// A recalled line is marked as recalled, so the command menu stays shut for it. Without
        /// that, recalling a `/command` pops the menu open — and the menu wants the arrows, which
        /// are the only way back out of the history. The user pressed Up for a previous *line*,
        /// not to be shown a list of commands they did not ask about; the menu returns as soon as
        /// they type, because then it is about something they *are* writing.
        pub(super) fn history_up(&mut self) {
            if self.history.is_empty() {
                return;
            }
            let next = match self.history_index {
                Some(0) => 0,
                Some(index) => index - 1,
                None => {
                    self.history_draft = self.editing.clone();
                    self.history.len() - 1
                }
            };
            self.history_index = Some(next);
            self.editing = Some(Editor::from_text(&self.history[next]));
            self.menu_dismissed = self.editing.as_ref().map(Editor::text);
        }


        /// Down: one entry towards the newest, and past the newest back to the draft.
        ///
        /// The last press lands on a **blank** line holding the draft — not on the newest entry
        /// again, and not on nothing at all. Waiting at the newest entry means the user has to
        /// guess how many entries there are; a blank line is the unambiguous "this is the line
        /// you are writing".
        ///
        /// Recalled entries keep the menu shut, like [`Screen::history_up`] does. The one
        /// exception is the draft: it is the user's own line, so if it opens a menu, that menu
        /// comes back with it.
        pub(super) fn history_down(&mut self) {
            let Some(index) = self.history_index else {
                return;
            };
            if index + 1 < self.history.len() {
                self.history_index = Some(index + 1);
                self.editing = Some(Editor::from_text(&self.history[index + 1]));
                self.menu_dismissed = self.editing.as_ref().map(Editor::text);
                return;
            }
            // Past the newest entry: back to what was being typed before the walk began.
            self.history_index = None;
            self.editing = Some(self.history_draft.take().unwrap_or_default());
            self.menu_dismissed = None;
        }


        /// Read one line of input. `prompt` is only echoed interactively.
        pub fn read_input(&mut self) -> std::io::Result<Action> {
            if !self.interactive {
                let mut buffer = String::new();
                let read = std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut buffer)?;
                if read == 0 {
                    return Ok(Action::Eof);
                }
                return Ok(Action::Line(buffer.trim_end_matches('\n').to_string()));
            }
            // Raw mode is taken here and released only at teardown, so it stays on across the
            // turn that follows: the input line has to keep receiving keys while the model is
            // answering, and a terminal switched back to cooked mode would hold them in the line
            // discipline until the next prompt.
            let _guard = RawGuard::enter()?;
            self.begin_line();
            loop {
                let event = match event::read() {
                    Ok(event) => event,
                    Err(_) => return Ok(Action::Eof),
                };
                match self.absorb_event(event) {
                    Some(action) => return Ok(action),
                    None => continue,
                }
            }
        }


        /// Arm the input line, keeping whatever draft is already in it.
        ///
        /// The composer is armed for the whole session — a turn is a time when the user is very
        /// likely to want to type, so the line they type into has to exist then too. That means
        /// this is called on a buffer that may already hold text the user began writing while the
        /// model was answering, and starting to read the next line must not throw that away: it
        /// is the user's half-written message, and losing it silently is the same bug as losing
        /// keys, one turn later.
        ///
        /// Only the furniture around the line is reset. The buffer is emptied when a line is
        /// submitted (see `handle_key`), which is the one moment it is meant to be emptied.
        pub fn begin_line(&mut self) {
            self.editing.get_or_insert_with(Editor::new);
            self.menu.clear();
            self.menu_selected = 0;
            self.menu_dismissed = None;
            self.render();
        }


        /// Take any typing that has already arrived, without waiting for more.
        ///
        /// This is what keeps the input line alive while the model is answering: the turn loop
        /// calls it between deltas, so a keypress lands in the composer as it is pressed. The
        /// alternative — reading input only when the turn is over — is why typing during a turn
        /// did nothing at all: the bytes sat in the terminal buffer until the next prompt.
        ///
        /// Only already-buffered events are taken, so this never stalls the turn.
        ///
        /// *Every* buffered event is taken, not just one. One per call would tie the input rate
        /// to how often the caller comes round — with nothing but the spinner ticking, that is
        /// one key per frame, and pasting a line would take seconds to appear.
        pub fn poll_input(&mut self) -> Option<Action> {
            if !self.interactive {
                return None;
            }
            let mut pending: Option<Action> = None;
            // The first action ends the drain: it is a submitted line or a Ctrl+O, and the
            // caller acts on it before any more typing is read.
            while pending.is_none() && event::poll(std::time::Duration::ZERO).unwrap_or(false) {
                match event::read() {
                    Ok(event) => pending = self.absorb_event(event),
                    Err(_) => break,
                }
            }
            pending
        }


        /// Take one terminal event.
        ///
        /// Returns `Some(action)` when the line was submitted, when the user asked to interrupt
        /// or to expand something, and `None` when the event only changed what is on screen. The
        /// caller decides what a submitted line means: at the prompt it is the next turn, and
        /// during a turn it is a message queued behind the one in flight.
        pub fn absorb_event(&mut self, event: Event) -> Option<Action> {
            if let Event::Resize(_, _) = event {
                self.refresh_size();
                self.render();
                return None;
            }
            let Event::Key(key) = event else {
                return None;
            };
            if key.kind == KeyEventKind::Release {
                return None;
            }
            // A notice describes the *previous* keystroke, so it is cleared before this one is
            // handled rather than after: a notice the handler sets (a failed paste, say) is about
            // what just happened and has to survive the redraw that shows it. Clearing it after
            // the handler wiped the notice in the very same keystroke that raised it, so it was
            // never on screen at all.
            self.notice = None;
            let action = self.handle_key(key);
            if action.is_some() {
                // The line is on its way out: clear the composer and redraw once without it. The
                // buffer is cleared when the line is taken, but the terminal still shows the
                // frame from the last keystroke, so without this the submitted text sits in the
                // input row until something else repaints.
                self.menu.clear();
                self.menu_selected = 0;
                self.menu_dismissed = None;
                self.render();
                return action;
            }
            self.sync_menu();
            self.render();
            None
        }


        /// Turn one keystroke into an edit, or into the action it stands for.
        ///
        /// This is the whole line editor, and there is exactly one copy of it: the prompt and the
        /// turn loop both feed keys through here, so a key that edits works while the model is
        /// still answering. A second implementation for "typing while busy" would be a second set
        /// of bugs.
        pub(super) fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            match key.code {
                KeyCode::Enter => {
                    let mut line = self.editing.as_ref().map(Editor::text).unwrap_or_default();
                    // The line is on its way out, so clear the buffer now: the caller commits
                    // it to the transcript and redraws, and a buffer that still holds the
                    // submitted text would be drawn again as a fresh prompt.
                    self.editing = Some(Editor::new());
                    // The walk through the history ends with the line it produced. Leaving the
                    // index pointing at a recalled entry would make the next Down continue the
                    // walk from there instead of doing nothing on a fresh, empty line.
                    self.history_index = None;
                    self.history_draft = None;
                    // Enter runs the highlighted command. When the buffer already spells
                    // that command out (`/exit` typed by hand, or a `/model` argument in
                    // progress), it is taken as written so an argument survives.
                    if let Some(accepted) = self.accepted_command() {
                        let typed = line.strip_prefix('/').map(str::trim).unwrap_or_default();
                        let has_argument = typed.contains(' ');
                        if !has_argument {
                            line = accepted;
                        }
                    }
                    self.remember(&line);
                    // Images travel with the line, and an image with no text at all is a
                    // valid message: the user may only want to show a screenshot.
                    if !self.pending_images.is_empty() {
                        let images = std::mem::take(&mut self.pending_images);
                        return Some(Action::LineWithImages(line, images));
                    }
                    return Some(Action::Line(line));
                }
                KeyCode::Char('o') if ctrl => return Some(Action::ToggleExpand),
                KeyCode::Char('c') if ctrl => {
                    let empty = self.editing.as_ref().is_none_or(Editor::is_empty)
                        && self.pending_images.is_empty();
                    if empty {
                        return Some(Action::Interrupt);
                    }
                    self.editing = Some(Editor::new());
                    self.pending_images.clear();
                }
                KeyCode::Char('d') if ctrl => {
                    if self.editing.as_ref().is_none_or(Editor::is_empty) {
                        return Some(Action::Eof);
                    }
                    // Like Ctrl+C, this leaves a non-empty line alone: Ctrl+D means "close
                    // the stream", and the buffer is not part of that.
                }
                KeyCode::Char('v') if ctrl => {
                    // An image on the clipboard wins over text: a screenshot tool usually
                    // leaves both, and the user pressing Ctrl+V after a screenshot means
                    // the picture. Text paste is the consolation path.
                    match image_input::read_clipboard_image() {
                        Ok(image) => {
                            self.pending_images.push(image);
                        }
                        Err(image_input::ImageError::NoImage) => {
                            if let Ok(text) = image_input::read_clipboard_text()
                                && let Some(editor) = &mut self.editing
                            {
                                // One line at a time: the editor has no multiline buffer,
                                // and a raw newline would break the layout.
                                editor.insert(&text.replace(['\n', '\r'], " "));
                            }
                        }
                        Err(err) => {
                            self.notice = Some(format!("粘贴失败：{err}"));
                        }
                    }
                }
                KeyCode::Char('u') if ctrl => {
                    if let Some(editor) = &mut self.editing {
                        editor.delete_to_start();
                    }
                }
                KeyCode::Char('k') if ctrl => {
                    if let Some(editor) = &mut self.editing {
                        editor.delete_to_end();
                    }
                }
                KeyCode::Char('w') if ctrl => {
                    if let Some(editor) = &mut self.editing {
                        editor.delete_word();
                    }
                }
                KeyCode::Char('a') if ctrl => {
                    if let Some(editor) = &mut self.editing {
                        editor.home();
                    }
                }
                KeyCode::Char('e') if ctrl => {
                    if let Some(editor) = &mut self.editing {
                        editor.end();
                    }
                }
                // Left/Right move the caret in characters, so the units match the buffer
                // rather than the screen: a CJK character is one step, not two.
                KeyCode::Left if alt => {
                    if let Some(editor) = &mut self.editing {
                        editor.delete_word();
                    }
                }
                KeyCode::Left => {
                    if let Some(editor) = &mut self.editing {
                        editor.move_caret(-1);
                    }
                }
                KeyCode::Right => {
                    if let Some(editor) = &mut self.editing {
                        editor.move_caret(1);
                    }
                }
                KeyCode::Home => {
                    if let Some(editor) = &mut self.editing {
                        editor.home();
                    }
                }
                KeyCode::End => {
                    if let Some(editor) = &mut self.editing {
                        editor.end();
                    }
                }
                KeyCode::Delete => {
                    if let Some(editor) = &mut self.editing {
                        editor.delete();
                    }
                }
                KeyCode::Char(c) if !ctrl && !alt => {
                    if let Some(editor) = &mut self.editing {
                        editor.insert(&c.to_string());
                    }
                }
                KeyCode::Backspace => {
                    if let Some(editor) = &mut self.editing {
                        editor.backspace();
                    }
                }
                KeyCode::Tab => {
                    self.complete();
                }
                KeyCode::BackTab => {
                    self.move_menu(-1);
                }
                KeyCode::Esc => {
                    // An open menu is what Esc closes first: it is a list the user is being
                    // asked about, and dismissing it must never cost them the answer the model
                    // is halfway through writing.
                    if !self.menu.is_empty() {
                        // Closing the menu must not drop what was typed, and it has to *stay*
                        // closed: the redraw at the end of this very keypress would otherwise put
                        // the list straight back. Typing on starts a fresh command name, so the
                        // menu comes back then.
                        self.menu_dismissed = self.editing.as_ref().map(Editor::text);
                        self.menu.clear();
                        self.menu_selected = 0;
                    } else if self.working.is_some() {
                        // Nothing to dismiss, and something is running: Esc stops it. The
                        // spinner is the only thing on screen that says a turn is in flight, so
                        // it is also what decides whether this key means "stop".
                        return Some(Action::Stop);
                    }
                }
                // The arrows drive the menu when it is up, and the input history otherwise.
                //
                // This is the arrangement the user asked for twice, from both sides: the menu has
                // to be selectable with the arrows, and Down at the end of the history has to
                // reach a blank line. They only conflict because recalling a `/command` used to
                // pop the menu open on it, and then the arrows belonged to a list the user never
                // asked for. The recall is what suppresses the menu (see `history_up`), so a
                // refreshed list here means the user opened it by typing.
                KeyCode::Up if !self.menu.is_empty() => {
                    self.move_menu(-1);
                }
                KeyCode::Down if !self.menu.is_empty() => {
                    self.move_menu(1);
                }
                KeyCode::Up => {
                    self.history_up();
                }
                KeyCode::Down => {
                    self.history_down();
                }
                _ => {}
            }
            None
        }


        /// Add a submitted line to the history, so Up recalls it later.
        pub(super) fn remember(&mut self, line: &str) {
            if line.trim().is_empty() {
                return;
            }
            self.history.push(line.to_string());
            if self.history.len() > HISTORY_LIMIT {
                self.history.remove(0);
            }
        }
}
