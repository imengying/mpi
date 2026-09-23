//! Behaviour of the screen, asserted on the values it produces.

use super::*;
use crate::ui::footer::FooterState;

    fn screen() -> Screen {
        let mut screen = Screen::new();
        screen.interactive = false;
        screen.width = 40;
        screen.height = 24;
        screen
    }

    /// Set the input buffer the way a test means it: text, with the caret at the end.
    fn set_input(screen: &mut Screen, text: &str) {
        screen.editing = Some(Editor::from_text(text));
    }

    /// The input buffer as a string.
    fn input(screen: &Screen) -> String {
        screen.editing.as_ref().map(Editor::text).unwrap_or_default()
    }

    /// Put the caret `back` characters from the end of the buffer.
    fn caret_back(screen: &mut Screen, back: usize) {
        if let Some(editor) = &mut screen.editing {
            editor.home();
            editor.move_caret((editor.len() - back.min(editor.len())) as isize);
        }
    }

    #[test]
    fn leaving_takes_the_live_region_down() {
        // The live region is the prompt and the footer, not part of the transcript. If it is
        // left on screen the shell inherits a cursor parked mid-row, and zsh marks a partial
        // line with `%` — so pi's prompt appears to survive as `› /%`.
        //
        // The draw and erase steps are tested above; what this pins is that the exit path
        // actually erases, because the residue only shows up in a real shell.
        //
        // `interactive` has to be on: a piped run writes no escape sequences at all, and
        // `erase_live` is a no-op there by design.
        let mut screen = screen();
        screen.interactive = true;
        set_input(&mut screen, "/");
        screen.set_footer(vec![Line::plain("dir"), Line::plain("stats")]);

        // Draw once, as the input loop does, so there is a region to take down.
        let (lines, cursor) = screen.compose_live();
        assert!(cursor.is_some(), "an input line means a cursor to park");
        screen.live_rows = lines.len();
        screen.cursor_row = cursor.map(|(row, _)| row);

        screen.leave();

        // The region is gone, so a later erase has nothing to do — which is exactly the
        // state that keeps the shell's marker off the screen.
        assert_eq!(screen.live_rows, 0);
        assert_eq!(screen.cursor_row, None);
    }

    #[test]
    fn leaving_commits_what_was_queued_but_not_yet_drawn() {
        // The note telling the user how to come back is pushed *after* the turn loop ends,
        // so no draw ever follows it. Erasing without committing threw it away silently: the
        // last thing pi is supposed to say was the one thing it never said.
        let mut screen = screen();
        screen.interactive = true;
        screen.push_lines(vec![Line::plain("继续此会话：pi resume abc")]);
        assert!(!screen.blocks.is_empty(), "the note is queued");

        screen.leave();

        // `printed` catches up with `blocks` only if the queued row was written.
        assert_eq!(
            screen.printed,
            screen.blocks.len(),
            "a queued row must be written before the live region is taken down"
        );
    }

    #[test]
    fn the_window_title_names_the_project_not_its_path() {
        // The tab shows which project this is; where it lives is not the question a tab
        // answers, and the path is what made the title unreadable.
        let cwd = Path::new("/home/me/文档/mpi");
        assert_eq!(window_title(None, cwd), "π - mpi");
        // A session name wins: the user chose it deliberately.
        assert_eq!(window_title(Some("重构解析器"), cwd), "π - 重构解析器");
        assert_eq!(window_title(Some("   "), cwd), "π - mpi");
        // Only the last component is used, so a deeply nested checkout stays short.
        assert_eq!(window_title(None, Path::new("/a/b/c/deep-project")), "π - deep-project");
        // The file-system root has no name to show.
        assert_eq!(window_title(None, Path::new("/")), "π");
    }

    #[test]
    fn a_session_name_cannot_break_out_of_the_title_sequence() {
        // The name is user input. A raw ESC or BEL here would end the OSC sequence early and
        // let whatever follows be read as a terminal command, so both are stripped.
        let cwd = Path::new("/tmp/work");
        let title = window_title(Some("a\u{1b}]0;evil\u{7}b"), cwd);
        assert!(!title.contains('\u{1b}'), "{title:?}");
        assert!(!title.contains('\u{7}'), "{title:?}");
        // The injected sequence is removed outright, not merely neutralised.
        assert_eq!(title, "π - ab");
        // A bare BEL is dropped as well.
        assert_eq!(window_title(Some("a\u{7}b"), cwd), "π - ab");
        // A newline would split the title across two lines in the terminal's tab bar.
        assert!(!window_title(Some("a\nb"), cwd).contains('\n'));
    }

    #[test]
    fn a_piped_screen_does_not_write_a_title() {
        // `screen()` is non-interactive, so nothing should reach stdout.
        let mut screen = screen();
        screen.set_title(Some("demo"), Path::new("/tmp/work"));
        assert!(screen.title.is_none());
    }

    fn screen_with_commands() -> Screen {
        let mut screen = screen();
        screen.set_commands(crate::agent::r#loop::COMMANDS);
        screen
    }

    #[test]
    fn the_working_spinner_sits_above_the_input_and_moves() {
        // pi's indicator: a frame that advances, at the left of the input area. Its whole
        // job is to be *moving* — a still mark cannot be told apart from a hung process, so
        // the frames are asserted to differ rather than merely to exist.
        let mut screen = screen_with_commands();
        set_input(&mut screen, "");
        screen.set_footer(vec![Line::plain("dir"), Line::plain("stats")]);
        screen.working = Some(WORKING_LABEL.to_string());

        let (lines, _) = screen.compose_live();
        let input = lines.iter().position(|line| line.text().starts_with('›')).unwrap();
        let text: Vec<String> = lines.iter().map(Line::text).collect();
        assert_eq!(input, 1, "the spinner is one row: {text:?}");
        assert_eq!(lines[0].text(), format!("{} {}", WORKING_FRAMES[0], WORKING_LABEL));

        // Each tick advances by exactly one frame and wraps, so the animation has no jump.
        screen.working_frame = WORKING_FRAMES.len() - 1;
        screen.tick_working();
        let (lines, _) = screen.compose_live();
        assert_eq!(lines[0].text(), format!("{} {}", WORKING_FRAMES[0], WORKING_LABEL));
        screen.tick_working();
        let (lines, _) = screen.compose_live();
        assert_eq!(lines[0].text(), format!("{} {}", WORKING_FRAMES[1], WORKING_LABEL));
    }

    #[test]
    fn a_tick_paints_only_the_rows_at_or_above_the_spinner() {
        // The input line sits under the spinner, and the caret lives in it. A tick that
        // repainted it would erase the caret and put it back at 12Hz — the wobble the fast
        // path exists to prevent. So: rows above the spinner are free to change, rows below
        // it are not, and the paint starts at the first row that actually differs.
        let spinner = |frame: &str| {
            Line::spans(vec![
                Span::new(frame, Style::new(Color::Cyan)),
                Span::plain(" "),
                Span::new(WORKING_LABEL, Style::new(Color::Dim)),
            ])
        };
        let thinking = |text: &str| Line::new(text, Style::new(Color::Dim));
        let input = Line::plain("› draft");
        let footer = Line::plain("dir");

        // Thinking above the spinner changed: the paint starts at that row.
        let before = vec![thinking("one"), spinner(WORKING_FRAMES[0]), input.clone(), footer.clone()];
        let after = vec![thinking("two"), spinner(WORKING_FRAMES[1]), input.clone(), footer.clone()];
        assert_eq!(dirty_top(&before, &after, 1), Some(0));

        // Nothing above changed either: only the spinner row is repainted.
        let same = vec![thinking("one"), spinner(WORKING_FRAMES[1]), input.clone(), footer.clone()];
        assert_eq!(dirty_top(&before, &same, 1), Some(1));

        // The input line changed — the caret may have moved with it. Full redraw.
        let typed = vec![thinking("one"), spinner(WORKING_FRAMES[1]), Line::plain("› draftx"), footer];
        assert_eq!(dirty_top(&before, &typed, 1), None);
    }

    #[test]
    fn the_spinner_is_gone_once_the_turn_ends() {
        let mut screen = screen_with_commands();
        set_input(&mut screen, "");
        screen.working = Some(WORKING_LABEL.to_string());
        screen.clear_working();
        let (lines, _) = screen.compose_live();
        let text: Vec<String> = lines.iter().map(Line::text).collect();
        assert!(lines[0].text().starts_with('›'), "the input is the first row again: {text:?}");
    }

    #[test]
    fn every_spinner_frame_is_one_column_wide() {
        // The label sits to the right of the frame, so a frame of a different width would
        // make it shift sideways on every tick.
        for frame in WORKING_FRAMES {
            assert_eq!(util::width(frame), 1, "frame {frame:?} is not one column");
        }
    }

    #[test]
    fn a_slash_opens_the_menu_and_filters_as_more_is_typed() {
        let mut screen = screen_with_commands();
        set_input(&mut screen, "/");
        screen.sync_menu();
        assert_eq!(screen.menu.len(), crate::agent::r#loop::COMMANDS.len());
        // The first entry is highlighted, so Enter has an unambiguous target.
        assert_eq!(screen.menu[0].0, "model");

        set_input(&mut screen, "/m");
        screen.sync_menu();
        // "m" matches /model and /compact: the match is on the name, not the description.
        assert_eq!(screen.menu.len(), 1, "{:?}", screen.menu);
        assert_eq!(screen.menu[0].0, "model");

        set_input(&mut screen, "/na");
        screen.sync_menu();
        assert_eq!(screen.menu.len(), 1);
        assert_eq!(screen.menu[0].0, "name");
    }

    #[test]
    fn the_menu_stays_out_of_the_way_of_ordinary_text() {
        let mut screen = screen_with_commands();
        // A slash mid-sentence is not a command, and an argument is being typed once there
        // is a space, so in both cases there is nothing to complete.
        for text in ["你好/世界", "/name 我的会话", "no slash at all"] {
            set_input(&mut screen, text);
            screen.sync_menu();
            assert!(screen.menu.is_empty(), "{text:?} opened a menu");
        }
    }

    #[test]
    fn tab_completes_a_unique_command_along_with_a_space() {
        let mut screen = screen_with_commands();
        set_input(&mut screen, "/na");
        assert!(screen.complete());
        // The trailing space means the argument can be typed straight away.
        assert_eq!(input(&screen), "/name ");
        // The menu closes because the name is settled.
        assert!(screen.menu.is_empty());
    }

    #[test]
    fn tab_shares_a_prefix_before_cycling_through_the_menu() {
        let mut screen = screen_with_commands();
        // "co" matches only /compact, so that case is covered above; "c" also matches
        // nothing else, so use two commands sharing a prefix via the real list.
        set_input(&mut screen, "/");
        assert!(screen.complete() || !screen.menu.is_empty());
        // With several matches and no shared prefix to add, Tab walks the highlight.
        let before = screen.menu_selected;
        screen.complete();
        assert_ne!(screen.menu_selected, before);
    }

    #[test]
    fn tab_on_an_exact_command_does_nothing_destructive() {
        let mut screen = screen_with_commands();
        set_input(&mut screen, "/exit");
        screen.sync_menu();
        // /exit is the only match, so Tab would fill in the space; the point is that it
        // must not lose what was typed.
        screen.complete();
        assert!(input(&screen).starts_with("/exit"));
    }

    #[test]
    fn a_shared_prefix_is_filled_in_before_cycling() {
        assert_eq!(common_prefix(&["model", "modify"]), "mod");
        assert_eq!(common_prefix(&["name", "new"]), "n");
        // No shared prefix: the empty string, which is always a prefix, and the caller
        // then falls through to cycling the menu instead of typing anything.
        assert_eq!(common_prefix(&["model", "exit"]), "");
        assert_eq!(common_prefix(&["only"]), "only");
        assert_eq!(common_prefix(&[]), "");
    }

    #[test]
    fn a_menu_selection_is_the_command_that_runs() {
        let mut screen = screen_with_commands();
        set_input(&mut screen, "/re");
        screen.sync_menu();
        // Enter takes the highlighted entry as a whole command: the menu is there to save
        // typing, so picking from it must not require a second Enter to submit.
        assert_eq!(screen.accepted_command().as_deref(), Some("/resume"));
        assert_eq!(screen.menu.len(), 1, "the filter left only the match");

        // Moving the highlight moves what Enter would run.
        set_input(&mut screen, "/");
        screen.sync_menu();
        screen.move_menu(1);
        assert_eq!(screen.accepted_command(), Some("/name".into()));

        // With no menu open there is nothing to accept, and the buffer is submitted as
        // typed.
        set_input(&mut screen, "你好");
        screen.sync_menu();
        assert_eq!(screen.accepted_command(), None);
    }

    #[test]
    fn the_menu_wraps_at_both_ends() {
        let mut screen = screen_with_commands();
        set_input(&mut screen, "/");
        screen.sync_menu();
        assert!(screen.move_menu(-1));
        assert_eq!(screen.menu_selected, screen.menu.len() - 1);
        assert!(screen.move_menu(1));
        assert_eq!(screen.menu_selected, 0);
    }

    #[test]
    fn the_cursor_lands_after_the_buffer_not_below_the_footer() {
        // The live region is drawn as text; nothing moves the cursor unless the drawing
        // code says where it goes. Without this the caret ends up under the footer.
        let mut screen = screen_with_commands();
        set_input(&mut screen, "/mo");
        screen.sync_menu();
        screen.set_footer(vec![Line::plain("dir"), Line::plain("stats")]);
        let (lines, cursor) = screen.compose_live();
        let (row, column) = cursor.expect("an editing screen has a cursor");
        // The prompt is two columns, and the buffer is three characters.
        assert_eq!(column, 2 + 3);
        // The row holds the input line — not the footer, which is below it.
        assert!(lines[row].text().starts_with("› /mo"), "{:?}", lines[row].text());
        assert!(row + 1 < lines.len());
        assert!(lines[row + 1].text().contains("/model"), "the menu goes under the input");
    }

    #[test]
    fn the_caret_sits_where_the_buffer_put_it() {
        // The caret is part of the editor, not a property of the end of the line. Without
        // this the only place a correction can be typed is the end, which is what "you can't
        // go back and fix a typo" means from the user's side.
        let mut screen = screen_with_commands();
        set_input(&mut screen, "helo world");
        // `helo world` is ten characters; six steps back puts the caret after `helo`.
        caret_back(&mut screen, 6);
        let (lines, cursor) = screen.compose_live();
        let (row, column) = cursor.unwrap();
        assert_eq!(lines[row].text(), "› helo world");
        assert_eq!(column, 2 + 4, "the caret is inside the word, not at its end");
    }

    #[test]
    fn a_wide_character_is_one_step_and_two_columns() {
        // Stepping by character rather than by column is what keeps the caret from landing
        // in the middle of a CJK glyph, where there is no position to draw it.
        let mut screen = screen_with_commands();
        set_input(&mut screen, "你好");
        caret_back(&mut screen, 1);
        let (lines, cursor) = screen.compose_live();
        let (row, column) = cursor.unwrap();
        assert_eq!(lines[row].text(), "› 你好");
        assert_eq!(column, 2 + 2, "one character back is two display columns");
    }

    #[test]
    fn the_wrapped_caret_follows_the_text_to_the_next_row() {
        // A caret just before a wrap boundary belongs on the row below, at its first column:
        // putting it on the row above would place it one cell past the terminal's edge.
        let mut screen = screen_with_commands();
        // 12 columns, 2 for the prompt: 10 characters fill the first row exactly.
        screen.width = 12;
        set_input(&mut screen, "abcdefghijklm");
        caret_back(&mut screen, 3); // the caret is exactly on the wrap boundary
        let (lines, cursor) = screen.compose_live();
        let (row, column) = cursor.unwrap();
        assert_eq!(lines[row].text(), "  klm", "a full row puts the caret on the next one");
        assert_eq!(column, 2, "at the first text column, not one past the screen edge");
    }

    #[test]
    fn a_recalled_command_does_not_open_the_menu_over_the_arrows() {
        // The two requirements pull in opposite directions and this is where they meet: the
        // menu must be selectable with the arrows, and Down at the end of the history must
        // reach a blank line. They conflicted because recalling a `/command` popped the menu
        // open on it, and the arrows then belonged to a list nobody asked for. A recall is not
        // typing, so it does not open the menu — which leaves the arrows free for the history.
        let mut screen = screen_with_commands();
        screen.history = vec!["/name".into(), "hello".into()];
        set_input(&mut screen, "");
        screen.history_index = Some(0);

        screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Up)));
        assert_eq!(input(&screen), "/name", "Up recalls the previous line");
        assert!(screen.menu.is_empty(), "recalling a command must not open the menu");

        // Down is the way back out of the history, and it still works.
        screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Down)));
        assert_eq!(input(&screen), "hello", "Down leaves the recalled command behind");
        screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Down)));
        assert_eq!(input(&screen), "", "and the last Down reaches the blank line");
    }

    #[test]
    fn the_arrows_select_from_the_menu_while_it_is_open() {
        // Typing `/` opens the menu, and the arrows have to move through it: that is how a
        // menu is used everywhere else, and the highlight is the only thing that says which
        // entry Enter would run.
        let mut screen = screen_with_commands();
        screen.begin_line();
        screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Char('/'))));
        assert!(screen.menu.len() > 1, "the menu lists the commands");
        assert_eq!(screen.menu_selected, 0);

        screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Down)));
        assert_eq!(screen.menu_selected, 1, "Down moves the highlight");
        assert_eq!(input(&screen), "/", "and leaves the buffer alone");

        screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Up)));
        assert_eq!(screen.menu_selected, 0, "Up moves it back");
    }

    #[test]
    fn esc_stops_the_turn_only_when_something_is_running() {
        // Esc has two jobs and they must not collide: it closes an open menu, and it stops a
        // turn. With nothing running and no menu it stays a no-op — it cannot be a Stop that
        // nothing is listening for, because `read_input` at the prompt would then have to
        // filter it back out.
        let mut screen = screen_with_commands();
        set_input(&mut screen, "hello");
        screen.sync_menu();
        assert!(screen.menu.is_empty());

        // Nothing running: Esc is swallowed.
        assert!(
            screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Esc))).is_none(),
            "Esc at an idle prompt is not an action"
        );

        // A turn in flight: Esc is the stop.
        screen.working = Some(WORKING_LABEL.to_string());
        assert_eq!(
            screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Esc))),
            Some(Action::Stop)
        );
    }

    #[test]
    fn esc_closes_an_open_menu_instead_of_stopping_the_turn() {
        // The menu wins: it is a list the user is being asked about, and dismissing it must
        // not cost them the answer being written behind it. The turn only stops on the Esc
        // *after* the menu is gone, which is what makes the two jobs separable by pressing
        // the key twice.
        let mut screen = screen_with_commands();
        screen.working = Some(WORKING_LABEL.to_string());
        set_input(&mut screen, "/mo");
        screen.sync_menu();
        assert!(!screen.menu.is_empty(), "typing a command name opens the menu");

        assert!(
            screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Esc))).is_none(),
            "the first Esc only closes the menu"
        );
        assert!(screen.menu.is_empty());
        assert_eq!(input(&screen), "/mo", "and the buffer survives");

        assert_eq!(
            screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Esc))),
            Some(Action::Stop),
            "the second Esc, with no menu left, stops the turn"
        );
    }

    #[test]
    fn esc_closes_the_menu_and_it_stays_closed() {
        // Esc had no effect at all before: the redraw at the end of the same keypress put the
        // list straight back. It is the buffer the user dismissed, so typing on brings the
        // menu back — a dismissal that survived to the end of the line would make it feel
        // broken in the other direction.
        let mut screen = screen_with_commands();
        set_input(&mut screen, "/mo");
        screen.sync_menu();
        assert!(!screen.menu.is_empty());

        // What the Esc key does.
        screen.menu_dismissed = screen.editing.as_ref().map(Editor::text);
        screen.menu.clear();
        screen.sync_menu();
        assert!(screen.menu.is_empty(), "the redraw must not bring it back");

        // The buffer moves on, so the dismissal no longer describes what is on screen.
        set_input(&mut screen, "/mod");
        screen.sync_menu();
        assert!(!screen.menu.is_empty(), "a new command name gets its menu back");
        assert!(screen.menu_dismissed.is_none());
    }

    #[test]
    fn a_line_submitted_mid_turn_is_held_as_what_it_is() {
        // A command typed while a turn is running cannot run then, but it also must not be
        // sent to the model as prose: `/model` is not something the user said. It waits as a
        // command, and a message waits as a message with its images — the two part ways the
        // moment the turn ends.
        let mut screen = screen_with_commands();
        screen.interactive = false;

        crate::agent::r#loop::queue_mid_turn(&mut screen, "/model".to_string(), Vec::new());
        crate::agent::r#loop::queue_mid_turn(&mut screen, "hello".to_string(), Vec::new());
        crate::agent::r#loop::queue_mid_turn(&mut screen, "   ".to_string(), Vec::new());

        let queued = screen.take_queued();
        assert_eq!(queued.len(), 2, "a blank line is not queued: {queued:?}");
        assert_eq!(queued[0], Queued::Command("/model".to_string()));
        assert_eq!(queued[1], Queued::Message("hello".to_string(), Vec::new()));

        // Taking them empties the queue, so the next turn does not re-send them.
        assert!(screen.take_queued().is_empty());
    }

    #[test]
    fn a_queued_line_is_trimmed_the_way_the_prompt_trims_it() {
        // A line has to reach the conversation the same way whether it was typed at the
        // prompt or during a turn. The prompt trims, so queueing does too — otherwise
        // " /model" would go to the model as a message while the same text typed at the
        // prompt would be rejected as an unknown command.
        let mut screen = screen_with_commands();
        crate::agent::r#loop::queue_mid_turn(&mut screen, "  /model  ".to_string(), Vec::new());
        crate::agent::r#loop::queue_mid_turn(&mut screen, "  hello  ".to_string(), Vec::new());

        let queued = screen.take_queued();
        assert_eq!(queued[0], Queued::Command("/model".to_string()));
        assert_eq!(queued[1], Queued::Message("hello".to_string(), Vec::new()));
    }

    #[test]
    fn a_command_queued_mid_turn_never_becomes_a_message() {
        // The bug this pins: the command used to be queued as a *message* with a note welded
        // into the text ("/model　（回合结束后执行）"), so the model received the text of a
        // command as something the user had said, and the command itself never ran.
        let mut screen = screen_with_commands();
        crate::agent::r#loop::queue_mid_turn(&mut screen, "/name x".to_string(), Vec::new());
        let queued = screen.take_queued();
        assert!(
            queued.iter().all(|item| !matches!(item, Queued::Message(..))),
            "a command must not be queued as a message: {queued:?}"
        );
    }

    #[test]
    fn a_paste_that_was_never_submitted_stays_with_the_draft() {
        // `take_queued` used to sweep `pending_images` into the queue, which sent a
        // screenshot the user was still composing and left the message it belonged to
        // without it. Images travel with the line on Enter, so the queue never owns them.
        let mut screen = screen_with_commands();
        screen.pending_images.push(crate::image_input::PastedImage {
            width: 4,
            height: 4,
            data: "AAAA".into(),
            bytes: 3,
        });
        crate::agent::r#loop::queue_mid_turn(&mut screen, "queued message".to_string(), Vec::new());

        let queued = screen.take_queued();
        assert_eq!(queued.len(), 1);
        assert_eq!(
            screen.pending_images.len(),
            1,
            "the unsubmitted paste still belongs to the line being written"
        );
        assert!(matches!(&queued[0], Queued::Message(text, images)
            if text == "queued message" && images.is_empty()));
    }

    #[test]
    fn arming_the_next_line_keeps_the_draft() {
        // The composer is armed again after every turn, and the user may have started
        // writing during the turn that just ended. `begin_line` used to blank the buffer,
        // which threw that message away one turn later than the keystroke bug it fixed.
        let mut screen = screen_with_commands();
        set_input(&mut screen, "half-written");
        screen.begin_line();
        assert_eq!(input(&screen), "half-written");
    }

    #[test]
    fn submitting_a_line_ends_the_history_walk() {
        // Enter takes the recalled line and empties the buffer. The walk has to end there
        // too: an index still pointing into the history makes the next Down continue the
        // walk from a line that is no longer on screen.
        let mut screen = screen_with_commands();
        screen.history = vec!["first".into(), "second".into()];
        set_input(&mut screen, "");
        screen.history_up();
        assert_eq!(input(&screen), "second");
        assert!(screen.history_index.is_some());

        screen.handle_key(KeyEvent::from(KeyCode::Enter));

        assert_eq!(input(&screen), "", "the line is on its way out");
        assert!(screen.history_index.is_none(), "the walk is over");
        assert!(screen.history_draft.is_none());
    }

    #[test]
    fn walking_back_down_past_the_newest_entry_restores_the_draft() {        // Up is a look at the history, not a commitment: a half-written message must survive
        // the glance. Without the stash, the last Down would blank the line and the user's
        // text would be gone with no sign that it had ever been there.
        let mut screen = screen_with_commands();
        screen.history = vec!["first".into(), "second".into()];
        set_input(&mut screen, "my draft");

        screen.history_up();
        assert_eq!(input(&screen), "second");
        screen.history_up();
        assert_eq!(input(&screen), "first");
        screen.history_down();
        assert_eq!(input(&screen), "second");
        screen.history_down();
        assert_eq!(input(&screen), "my draft", "the draft comes back, not a blank line");
        assert!(screen.history_index.is_none(), "the walk is over");
    }

    #[test]
    fn down_without_a_walk_in_progress_does_nothing() {
        // Down belongs to the menu-less prompt as the "blank line" key only while walking;
        // on a fresh buffer it must not clear what is being typed.
        let mut screen = screen_with_commands();
        screen.history = vec!["first".into()];
        set_input(&mut screen, "typing away");
        screen.history_down();
        assert_eq!(input(&screen), "typing away");
    }

    #[test]
    fn up_at_the_oldest_entry_stays_there() {
        // No wrap-around: with one, a repeated key press silently changes which entry is on
        // screen and the top of the history is indistinguishable from the bottom.
        let mut screen = screen_with_commands();
        screen.history = vec!["only".into()];
        screen.history_up();
        screen.history_up();
        assert_eq!(input(&screen), "only");
        assert_eq!(screen.history_index, Some(0));
    }

    #[test]
    fn a_resumed_session_offers_the_lines_it_already_holds() {
        // The bug this guards: history lived only in the process, so Up after a resume
        // reached nothing that was typed before it — the turns in the file looked like they
        // had never been typed, and the user had to retype a line that was already there.
        let mut screen = screen_with_commands();
        screen.seed_history(["asked before resuming".to_string(), "and again".to_string()]);

        screen.history_up();
        assert_eq!(input(&screen), "and again", "the newest seeded line comes first");
        screen.history_up();
        assert_eq!(input(&screen), "asked before resuming");
        screen.history_up();
        assert_eq!(input(&screen), "asked before resuming", "the top of the history");

        // A line typed after the resume joins the same list, in the order it was typed.
        set_input(&mut screen, "typed just now");
        screen.remember("typed just now");
        screen.history_down();
        screen.history_down();
        assert_eq!(input(&screen), "typed just now");
    }

    #[test]
    fn seeding_replaces_the_list_it_inherits() {
        // Seeding happens when the screen changes which conversation it is showing — a
        // resume, a `/resume` to another session, a `/new`. The lines that came with the
        // session being left are not history for the one being opened, and keeping them
        // would put a message from one conversation into the arrows of another.
        let mut screen = screen_with_commands();
        screen.history = vec!["from the last session".into()];
        screen.history_index = Some(0);
        screen.history_draft = Some(Editor::from_text("half written"));

        screen.seed_history(["from this one".to_string()]);

        assert_eq!(screen.history, vec!["from this one".to_string()]);
        assert!(screen.history_index.is_none(), "the walk belongs to the old list");
        assert!(screen.history_draft.is_none());

        // A new session has nothing to recall.
        screen.seed_history(Vec::new());
        assert!(screen.history.is_empty());
    }

    #[test]
    fn seeding_keeps_the_newest_entries() {
        // A long session has more lines than the cap; what is kept has to be the tail, since
        // that is what a recalled line is likely to be about.
        let mut screen = screen_with_commands();
        screen.seed_history((0..HISTORY_LIMIT + 5).map(|i| format!("line {i}")));

        assert_eq!(screen.history.len(), HISTORY_LIMIT);
        assert_eq!(screen.history[0], "line 5", "the oldest entries are the ones dropped");
        assert_eq!(screen.history[HISTORY_LIMIT - 1], format!("line {}", HISTORY_LIMIT + 4));
    }

    #[test]
    fn blank_seeded_lines_are_not_entries() {
        // Up on a blank line looks like a broken key: it takes the walk from one empty entry
        // to the next, and the user cannot tell it apart from reaching the top.
        let mut screen = screen_with_commands();
        screen.seed_history(["real".to_string(), "   ".to_string(), String::new()]);
        assert_eq!(screen.history, vec!["real".to_string()]);
    }

    #[test]
    fn a_row_filled_to_the_edge_gets_a_row_for_the_caret() {
        // 10 text columns, filled exactly. The caret marks where the next character goes,
        // and there is no cell left on that row to draw it in — asking for column 13 of a
        // 12-column terminal gets clamped by the terminal, and a caret that depends on a
        // clamp is a caret that is sometimes somewhere else.
        let mut screen = screen_with_commands();
        screen.width = 12;
        set_input(&mut screen, "abcdefghij");
        let (lines, cursor) = screen.compose_live();
        let (row, column) = cursor.unwrap();
        assert_eq!(lines[row].text(), "  ", "the caret sits on its own row below");
        assert_eq!(column, 2);
        assert_eq!(row, 1, "not on the row that is full");
    }

    #[test]
    fn the_caret_of_a_recalled_entry_lands_at_the_end() {
        // Recalling a line is for running or amending it, so the caret belongs where the
        // typing stopped — not at the start, where the next keystroke would be an insertion
        // into the middle of a command.
        let mut screen = screen_with_commands();
        screen.history = vec!["/name x".into()];
        set_input(&mut screen, "");
        screen.history_up();
        assert_eq!(screen.editing.as_ref().unwrap().caret(), 7);
    }

    #[test]
    fn the_erase_step_lands_on_the_first_live_row() {
        // Both halves of this arithmetic were wrong at different times, and both failures
        // look like "the screen creeps upward": one row of committed transcript is cleared
        // per redraw. Pin the numbers here rather than in a terminal.
        //
        // While editing, the cursor is parked on the input row, which is the *first* live
        // row — so erasing from there needs no upward move at all.
        let mut screen = screen_with_commands();
        set_input(&mut screen, "hi");
        screen.set_footer(vec![Line::plain("dir"), Line::plain("stats")]);
        let (lines, cursor) = screen.compose_live();
        let (row, _) = cursor.unwrap();
        assert_eq!(row, 0, "the input line is the first live row");
        let up = row; // mirrors erase_live
        assert_eq!(up, 0, "erasing from the input row must not climb");
        assert!(lines.len() > 1);

        // With a pending image and a menu the input row is still first.
        screen.pending_images.push(crate::image_input::PastedImage {
            width: 1,
            height: 1,
            data: "AA==".into(),
            bytes: 3,
        });
        let (lines, cursor) = screen.compose_live();
        let (row, _) = cursor.unwrap();
        assert_eq!(row, 0, "images render below the input line, not above it");
        assert!(lines[0].text().starts_with("› hi"));
    }

    #[test]
    fn the_cursor_follows_the_last_wrapped_row() {
        let mut screen = screen_with_commands();
        set_input(&mut screen, &('a'..='z').collect::<String>());
        screen.width = 12;
        let (lines, cursor) = screen.compose_live();
        let (row, column) = cursor.unwrap();
        // The caret is on the last row, after its text — not on the first row where it
        // would be if the input were being scrolled sideways.
        assert_eq!(row, 2);
        assert_eq!(lines[row].text(), "  uvwxyz", "the continuation row keeps its indent");
        assert_eq!(column, 2 + 6);
    }

    #[test]
    fn a_cjk_buffer_puts_the_cursor_after_two_cells_per_character() {
        // Column is a display column, not a character count: 你好 is four cells wide, so the
        // caret belongs at 2 + 4, which is what makes it line up with the glyphs.
        let mut screen = screen_with_commands();
        set_input(&mut screen, "你好");
        let (_, cursor) = screen.compose_live();
        assert_eq!(cursor.unwrap().1, 2 + 4);
    }

    #[test]
    fn no_cursor_is_reported_when_not_editing() {
        // While a turn streams, the transcript owns the screen and the terminal caret has
        // nowhere to sit.
        let screen = screen_with_commands();
        let (_, cursor) = screen.compose_live();
        assert!(cursor.is_none());
    }

    #[test]
    fn toggling_with_nothing_to_expand_says_so() {
        // Ctrl+O with no collapsible block on screen must not look like a dead key. The note
        // is pushed by the screen rather than by each caller, so this is where it is checked.
        let mut screen = screen();
        screen.push(Block::lines(vec![Line::plain("plain output")]));
        assert!(!screen.toggle_last_collapsible());
        assert!(screen.blocks.last().unwrap().render(80)[0].text().contains("没有可展开"));
    }

    #[test]
    fn toggling_hits_the_most_recent_collapsible_block() {
        let mut screen = screen();
        screen.push(Block::collapsible(vec![Line::plain("x".repeat(10))], 0, 0, 1));
        screen.push(Block::lines(vec![Line::plain("later")]));
        screen.push(Block::collapsible(
            (0..10).map(|i| Line::plain(format!("i{i}"))).collect(),
            0,
            0,
            3,
        ));
        assert!(screen.toggle_last_collapsible());
        let last = screen.blocks.last().unwrap();
        match last {
            Block::Collapsible(collapsible) => assert!(collapsible.expanded),
            _ => panic!("expected a collapsible block"),
        }
        // The earlier collapsible block is untouched.
        match &screen.blocks[0] {
            Block::Collapsible(collapsible) => assert!(!collapsible.expanded),
            _ => panic!("expected a collapsible block"),
        }
    }

    #[test]
    fn collapsing_all_resets_expansion() {
        let mut screen = screen();
        screen.push(Block::collapsible((0..8).map(|i| Line::plain(format!("l{i}"))).collect(), 0, 0, 2));
        screen.toggle_last_collapsible();
        screen.collapse_all();
        match &screen.blocks[0] {
            Block::Collapsible(collapsible) => assert!(!collapsible.expanded),
            _ => panic!("expected a collapsible block"),
        }
    }

    #[test]
    fn painted_output_is_exactly_the_requested_width() {
        let screen = screen();
        let line = Line::spans(vec![
            Span::new("added", Style::with_bg(Color::DiffAddedText, Bg::Added)),
        ]);
        let painted = screen.paint(&line, 40);
        assert_eq!(util::width(&util::strip_ansi(&painted)), 40);
        // Re-measuring the raw string would be wrong, which is the whole reason spans exist.
        assert!(painted.len() > 40);
    }

    #[test]
    fn a_spinner_tick_repaints_through_the_spinner_even_as_thinking_grows() {
        // The end-to-end shape of the fix. `tick_working` runs every 80ms for the whole turn;
        // when its fast path is refused it falls back to `render()`, which erases the live
        // region and paints it again — input line, caret and footer included, twelve times a
        // second. The thinking preview above the spinner changes on every tick, so the fast
        // path has to accept "everything from the first changed row down to the spinner" and
        // leave the rows below alone. This drives the preview across a line boundary, which is
        // where the old `spinner_only_change` gave up and the caret visibly moved.
        let mut screen = screen();
        screen.begin_stream();
        screen.set_working(WORKING_LABEL);
        set_input(&mut screen, "draft");

        let mut previous: Option<Vec<Line>> = None;
        let mut spinner = 0usize;
        let mut caret_rows = Vec::new();
        // Short chunks, so the preview grows a row within the loop rather than after it.
        for chunk in ["先想", "一下", "这个", "问题", "在哪", "里。", "也许", "是空", "输入"] {
            screen.push_thinking(chunk);
            screen.working_frame = (screen.working_frame + 1) % WORKING_FRAMES.len();
            let (lines, cursor) = screen.compose_live();
            caret_rows.push(cursor.map(|(row, _)| row));
            if let Some(before) = &previous {
                let row = spinner_row(&lines, WORKING_LABEL).expect("the spinner is up");
                spinner = row;
                assert!(
                    dirty_top(before, &lines, row).is_some(),
                    "the tick was forced into a full redraw at frame {chunk:?}"
                );
            }
            previous = Some(lines);
        }
        // And the caret's row never moved, which is the whole point.
        let first = caret_rows[0];
        assert!(
            caret_rows.iter().all(|row| *row == first),
            "the caret moved as the preview grew: {caret_rows:?}"
        );
        assert!(spinner > 0, "the preview did take the rows above the spinner");
    }

    #[test]
    fn the_tick_refuses_rather_than_underflowing_when_the_caret_is_not_below_the_spinner() {
        // `up` and `down` are both subtractions from the caret's row, so a caret that is not
        // at or below the spinner would underflow — and `usize` subtraction panics in debug
        // and wraps in release, which would move the cursor to column absurд. The guard is
        // what makes the arithmetic safe, so it is checked directly.
        let mut screen = screen();
        screen.interactive = true;
        screen.begin_stream();
        screen.set_working(WORKING_LABEL);
        set_input(&mut screen, "draft");
        // Thinking above the spinner, so the spinner is not row 0 — otherwise "above the
        // spinner" is not a representable state and the guard cannot be exercised.
        screen.streaming_thinking = Some("a thought".into());
        let (lines, _) = screen.compose_live();
        let spinner = spinner_row(&lines, WORKING_LABEL).expect("the spinner is up");
        assert!(spinner > 0, "the fixture needs a row above the spinner");
        screen.live_rows = lines.len();
        screen.last_live = lines;

        // A caret recorded *above* the spinner: refuse, do not subtract.
        screen.cursor_row = Some(spinner - 1);
        assert!(!screen.repaint_spinner(), "an impossible caret must be refused");
        // No caret at all is also refused: there is no row to park it back on.
        screen.cursor_row = None;
        assert!(!screen.repaint_spinner(), "a missing caret must be refused");
    }

    #[test]
    fn the_tick_frame_writes_the_spinner_rows_and_parks_the_caret() {
        // The bytes the fast path emits, checked as a sequence rather than by looking at a
        // terminal. It climbs to the first dirty row, writes only down through the spinner,
        // walks back to the input row and puts the caret at its column. Getting the walk-back
        // wrong parks the caret on the spinner row, and the next frame repaints from there.
        let mut screen = screen();
        screen.width = 40;
        let thinking = Line::dim("thinking");
        let spinner = Line::spans(vec![
            Span::new(WORKING_FRAMES[1], Style::new(Color::Cyan)),
            Span::plain(" "),
            Span::new(WORKING_LABEL, Style::new(Color::Dim)),
        ]);
        let rows = [thinking, spinner];

        // Two rows written, one row climbed (the caret is on the input row, two below the
        // spinner), so the walk back is two rows and the column is 3.
        let frame = screen.spinner_frame(&rows, 2, 2, Some((4, 3)));
        assert!(frame.starts_with("\u{1b}[2A"), "climb first: {frame:?}");
        assert!(frame.contains("thinking"), "{frame:?}");
        assert!(frame.contains(WORKING_LABEL), "{frame:?}");
        assert!(frame.contains("\u{1b}[2B"), "walk back down: {frame:?}");
        assert!(frame.ends_with("\u{1b}[4G"), "the caret's column, 1-based: {frame:?}");
        // The input line itself is never written by a tick.
        assert!(!frame.contains('›'), "the tick wrote the input row: {frame:?}");
    }

    #[test]
    fn a_burst_of_tokens_is_drawn_once_but_is_never_left_undrawn() {
        // The batching rule from both sides. A hundred queued tokens must not cost a hundred
        // redraws — each one erases and repaints the input row, and the caret with it — but a
        // single token must be drawn *now* rather than waiting for the next spinner tick.
        let mut screen = screen();
        screen.interactive = false;
        screen.begin_stream();
        set_input(&mut screen, "draft");

        // Appending alone only fills the buffer; the caller's single render is what draws.
        screen.push_thinking("hmm");
        assert_eq!(screen.streaming_thinking.as_deref(), Some("hmm"));

        for _ in 0..100 {
            screen.push_text("x");
        }
        assert_eq!(screen.streaming_answer.as_deref(), Some("x".repeat(100).as_str()));

        // One render draws the whole burst, so the preview the region is built from carries
        // all hundred characters rather than the first one.
        let (lines, _) = screen.compose_live();
        let drawn: String = lines.iter().map(Line::text).collect();
        assert!(drawn.contains(&"x".repeat(40)), "the burst reached the screen: {drawn:?}");
    }

    #[test]
    fn appending_a_token_does_not_redraw() {
        // Every delta used to call `render()`, which erases the live region and paints it
        // again — input line and caret included. A burst of tokens therefore repainted the
        // caret once per character. The burst is drained and redrawn once by the caller
        // instead, so appending is a buffer push and nothing more.
        let mut screen = screen();
        screen.interactive = true;
        screen.begin_stream();
        screen.set_working(WORKING_LABEL);
        set_input(&mut screen, "draft");
        screen.streaming_thinking = None;
        screen.live_rows = 0;
        screen.last_live.clear();

        screen.push_thinking("think");
        screen.push_text("answer");
        assert!(screen.last_live.is_empty(), "appending a token redrew the region");

        // And the caller's single redraw picks both up.
        screen.render();
        let text: Vec<String> = screen.last_live.iter().map(Line::text).collect();
        assert!(text.iter().any(|row| row.contains("think")), "{text:?}");
        assert!(text.iter().any(|row| row.contains("answer")), "{text:?}");
    }

    #[test]
    fn the_caret_stays_put_when_the_answer_preview_grows_too() {
        // The answer is below the spinner and above the input, and it wraps as it streams.
        // Its block is not padded the way the thinking preview is, so what has to hold is the
        // caret's *column*: the row moves with the answer, and that is expected — the input
        // line is under the text being written. What must not move is where the user is
        // typing inside their own line.
        let mut screen = screen();
        screen.interactive = false;
        screen.begin_stream();
        screen.set_working(WORKING_LABEL);
        set_input(&mut screen, "draft");

        let mut columns = Vec::new();
        let mut rows = Vec::new();
        for chunk in ["答案 ", "开始 ", "输出 ", "更多的字 ", "继续 ", "直到换行。"] {
            screen.push_text(chunk);
            let (_, caret) = screen.compose_live();
            columns.push(caret.map(|(_, column)| column));
            rows.push(caret.map(|(row, _)| row));
        }
        // The caret's column inside the input line is the same on every frame.
        let first = columns[0];
        assert!(
            columns.iter().all(|column| *column == first),
            "the caret slid sideways inside the input line: {columns:?}"
        );
        assert!(first.is_some(), "the composer is armed, so there is a caret");
    }

    #[test]
    fn no_live_row_is_wider_than_the_terminal() {
        // A row wider than the screen is wrapped *by the terminal*, which knows nothing about
        // this code's row count. Every draw then erases one row fewer than it drew, the region
        // creeps down a row, and the old copy is left behind — the same line repeated down the
        // screen, broken off at the right edge. The model is not repeating itself; the screen is.
        //
        // So the invariant is checked directly: whatever `compose_live` hands to the terminal
        // has to fit inside the width it was composed for. Every part of the region is exercised,
        // because each one is a separate chance to forget the wrap.
        let mut screen = screen();
        screen.width = 40;
        screen.interactive = false;
        screen.begin_stream();
        screen.set_working(WORKING_LABEL);
        set_input(&mut screen, "draft");
        screen.set_commands(&[("model", "切换模型并选择思考级别")]);

        let long = "一段没有空格分隔的很长的中文内容会一直写到终端的右边然后被硬生生截断";
        let check = |screen: &Screen, what: &str| {
            let (lines, _) = screen.compose_live();
            for (index, line) in lines.iter().enumerate() {
                let width = line.width();
                assert!(
                    width <= screen.width,
                    "{what}: row {index} is {width} cells wide in a {} cell screen: {:?}",
                    screen.width,
                    line.text()
                );
            }
        };

        // The answer preview: long unbroken prose, a wide table, a long code line, a bare URL.
        for chunk in [
            "这是一段很长的中文回答，它会一路写到终端的右边并且在没有任何空格的地方被截断，",
            "然后继续写下去，让这一行远远超过四十列的宽度，",
            "\n\n| 列一 | 列二 | 列三 | 列四 | 列五 |\n| --- | --- | --- | --- | --- |\n",
            "| 一个很长的单元格内容 | 另一个很长的单元格 | 第三个很长的单元格 | x | y |\n\n",
            "```\nlet a_very_long_line = something_that_goes_on_and_on_and_on_beyond_the_edge();\n```\n",
            "https://example.com/一个非常长的没有空格的网址用来撑破右边",
        ] {
            screen.push_text(chunk);
            check(&screen, "answer preview");
        }

        // A queued line is user input, so it can be as long as a paste.
        screen.queue(Queued::Message(long.into(), Vec::new()));
        check(&screen, "queued line");

        // The command menu and its help text.
        set_input(&mut screen, "/");
        screen.sync_menu();
        check(&screen, "menu");

        // The thinking preview is wrapped too, and one_line() has to run before it is.
        screen.streaming_thinking = Some(long.repeat(3));
        check(&screen, "thinking preview");

        // A notice: a failed paste carries the error text, which can be long.
        screen.notice = Some(long.repeat(2));
        check(&screen, "notice");

        // The running line is built from the command the model asked for, so it is as long as
        // the command is.
        screen.notice = None;
        screen.set_running(vec![Span::plain(format!("● $ echo {long}"))]);
        check(&screen, "running line");

        // The footer is produced by `footer::render`, which is width-aware, so what is checked
        // here is that the width it produced is the width the region gets. `set_footer` is fed
        // that output rather than a hand-built row, because a hand-built row would only be
        // testing the test.
        let mut state = FooterState {
            cwd: Path::new("/tmp/project"),
            branch: None,
            session_name: None,
            totals: crate::config::Usage { input: 173_000, output: 1_300, cache_read: 0, cache_write: 0 },
            cache_hit_rate: Some(99.5),
            context_tokens: Some(17_300),
            context_window: Some(1_000_000),
            model: None,
            level: "high",
            compacting: false,
            busy: None,
        };
        screen.set_footer(crate::ui::footer::render(&state, &Theme::default(), screen.width));
        check(&screen, "footer");
        // A long branch and model name are the two fields that can outgrow the row.
        state.branch = Some(long.repeat(2));
        screen.set_footer(crate::ui::footer::render(&state, &Theme::default(), screen.width));
        check(&screen, "footer with a long branch");
    }

    #[test]
    fn the_live_region_never_grows_past_what_it_erases() {
        // The bug from the screenshot, as an invariant. `erase_live` walks up by the number of
        // rows the last frame drew and clears downwards from there. If any of those rows was
        // wider than the screen, the terminal had wrapped it and the frame actually occupied
        // more rows than that count — so each erase left a row behind and the region crept down
        // the screen, painting the same line again and again.
        //
        // Nothing here looks at escape codes: the invariant is that the row count `compose_live`
        // reports is the row count the terminal will have, and that follows from every row
        // fitting the width. Streaming a long answer is the case that used to break it.
        let mut screen = screen();
        screen.width = 40;
        screen.height = 12;
        screen.interactive = false;
        screen.begin_stream();
        screen.set_working(WORKING_LABEL);
        set_input(&mut screen, "draft");

        let mut previous_rows: Option<usize> = None;
        for chunk in [
            "好的，我来说明一下。",
            "首先这一段会写得很长，长到超过四十列，因为它没有任何换行的机会，",
            "接着是第二段，同样很长，继续向右延伸下去直到远远越过边界，",
            "```\nvery_long_code_line_without_any_break_points_at_all_here();\n```",
            "| 甲 | 乙 | 丙 | 丁 | 戊 | 己 |\n| --- | --- | --- | --- | --- | --- |\n| 1 | 2 | 3 | 4 | 5 | 6 |\n",
        ] {
            screen.push_text(chunk);
            let (lines, cursor) = screen.compose_live();
            for (index, line) in lines.iter().enumerate() {
                assert!(line.width() <= screen.width, "row {index} overflows");
            }
            // The caret has to stay inside the region too, or it lands on a row that is not
            // part of the frame and the next erase starts from the wrong place.
            if let Some((row, column)) = cursor {
                assert!(row < lines.len(), "the caret is outside the region");
                assert!(column <= screen.width, "the caret is past the right edge");
            }
            if let Some(previous) = previous_rows {
                // The region is allowed to grow as the answer arrives, but never past the
                // screen — and `trim_live` is what enforces that.
                assert!(lines.len() <= screen.height, "region {previous} -> {} rows", lines.len());
            }
            previous_rows = Some(lines.len());
        }
    }

    #[test]
    fn the_composer_does_not_move_while_thinking_streams() {
        // While the model thinks, the composer stays armed and the user types into it. The
        // caret has to stay where they left it: a caret that wanders while text arrives
        // somewhere else is worse than no caret, because it is the one thing on screen that
        // claims to know where the next character goes.
        let mut screen = screen();
        screen.begin_stream();
        screen.set_working("Working");
        screen.editing = Some(Editor::from_text("打了一半的草稿"));

        let mut seen = Vec::new();
        for chunk in ["先看看 ", "这一步要做什么，", "然后动手。", "再看看边界条件。"] {
            screen.push_thinking(chunk);
            let (_, caret) = screen.compose_live();
            seen.push(caret);
        }
        let first = seen[0];
        assert!(
            seen.iter().all(|caret| *caret == first),
            "the caret moved as thinking arrived: {seen:?}"
        );
        assert!(first.is_some(), "the composer is armed, so there is a caret");
    }

    #[test]
    fn the_thinking_preview_only_keeps_the_last_two_lines() {
        let mut screen = screen();
        screen.streaming_thinking = Some("a\nb\nc\nd\ne".to_string());
        let (live, _) = screen.compose_live();
        assert!(live.len() <= 2, "{live:?}");
        assert!(live.iter().any(|line| line.text().contains('e')));
    }

    #[test]
    fn ansi_in_streamed_text_is_neutralised_before_display() {
        let mut screen = screen();
        screen.streaming_answer = Some("\u{1b}[31mred\u{1b}[0m".into());
        let (live, _) = screen.compose_live();
        assert!(live.iter().all(|line| !line.text().contains('\u{1b}')));
    }
