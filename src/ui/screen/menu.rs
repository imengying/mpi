//! The slash-command menu: matching, completion and selection.
//!
//! The menu is derived from the buffer on every keystroke rather than stored, so there is
//! no state to keep in sync.

use super::*;

impl Screen {

        /// The commands whose name starts with what has been typed after the `/`.
        ///
        /// An empty prefix (just `/`) matches everything, which is what makes the menu appear as
        /// soon as the slash is typed. A slash anywhere but the first column is ordinary text —
        /// `/` inside a sentence is not a command.
        pub(super) fn matching_commands(&self) -> Vec<(String, String)> {
            let Some(text) = self.editing.as_ref().map(Editor::text) else {
                return Vec::new();
            };
            let Some(rest) = text.strip_prefix('/') else {
                return Vec::new();
            };
            // The menu describes the command name, which is only being typed while the caret is
            // still inside it. Once the caret moves past the slash into ordinary text, offering
            // to complete a command would hijack the arrow keys for a menu about something the
            // user is no longer writing.
            if !self.caret_in_command_name() {
                return Vec::new();
            }
            // Once there is a space the command name is settled and the argument is being typed,
            // so the menu has nothing left to offer.
            if rest.contains(' ') {
                return Vec::new();
            }
            let prefix = rest.trim();
            self.commands
                .iter()
                .filter(|(name, _)| name.starts_with(prefix))
                .cloned()
                .collect()
        }


        /// Is the caret still in the command name (before the first space)?
        pub(super) fn caret_in_command_name(&self) -> bool {
            let Some(editing) = &self.editing else {
                return false;
            };
            let before: String = editing.text().chars().take(editing.caret()).collect();
            !before.contains(' ')
        }


        /// Refresh the menu from the buffer. Called after every edit.
        pub(super) fn sync_menu(&mut self) {
            let text = self.editing.as_ref().map(Editor::text).unwrap_or_default();
            if let Some(dismissed) = &self.menu_dismissed {
                if *dismissed == text {
                    // Still the buffer Esc dismissed: leave the list hidden. Without this, the
                    // redraw at the end of every keypress would put it straight back.
                    self.menu.clear();
                    self.menu_selected = 0;
                    return;
                }
                // The buffer moved on, so the dismissal no longer applies.
                self.menu_dismissed = None;
            }
            let matches = self.matching_commands();
            if matches.len() == self.menu.len() && matches.iter().zip(&self.menu).all(|(a, b)| a == b) {
                // Same list: keep the highlight where the user put it.
                return;
            }
            self.menu = matches;
            self.menu_selected = 0;
        }


        /// Tab: complete the first candidate, or the highlighted one.
        ///
        /// A unique match is completed in full and a trailing space added, so the user can go
        /// straight on to the argument. Several matches share the longest common prefix, which
        /// is the behaviour a shell user expects; the menu stays open to pick from.
        pub(super) fn complete(&mut self) -> bool {
            self.sync_menu();
            let Some(editing) = self.editing.clone() else {
                return false;
            };
            let text = editing.text();
            let Some(rest) = text.strip_prefix('/') else {
                return false;
            };
            if rest.contains(' ') || self.menu.is_empty() {
                return false;
            }
            let names: Vec<&str> = self.menu.iter().map(|(name, _)| name.as_str()).collect();
            let filled = if names.len() == 1 {
                format!("/{} ", names[0])
            } else {
                let prefix = common_prefix(&names);
                if prefix.len() <= rest.trim().len() {
                    // Nothing more to add; let Tab move through the menu instead.
                    self.menu_selected = (self.menu_selected + 1) % self.menu.len();
                    return true;
                }
                format!("/{prefix}")
            };
            // The completion replaces the whole buffer, and the caret goes to the end of what
            // was inserted — completing is not the place to leave the caret behind in the middle
            // of a word the user did not type.
            self.editing = Some(Editor::from_text(&filled));
            self.sync_menu();
            true
        }


        /// Move the highlight through the menu, wrapping at both ends.
        pub(super) fn move_menu(&mut self, delta: isize) -> bool {
            if self.menu.is_empty() {
                return false;
            }
            let len = self.menu.len() as isize;
            self.menu_selected = ((self.menu_selected as isize + delta).rem_euclid(len)) as usize;
            true
        }


        /// Take the highlighted command as the line to submit.
        ///
        /// The menu exists to save typing, so picking from it *runs* the command. Completing it
        /// with a trailing space instead would make the user press Enter twice to do the thing
        /// they just chose — and the second press is not obviously part of picking a menu item.
        ///
        /// `None` when there is nothing to take.
        pub(super) fn accepted_command(&self) -> Option<String> {
            self.menu
                .get(self.menu_selected)
                .map(|(name, _)| format!("/{name}"))
        }
}
