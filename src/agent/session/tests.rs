//! Behaviour of the session file, asserted on what it produces.
//!
//! The split follows the code: records and their validation, the context projection over
//! them, and the file handling around them — appending, locking, recovery and relocation.

use std::io::Write;

use super::store::{find_by_prefix_in, list_in};
use super::*;
use crate::llm::{Block, ThinkingReplay, ToolStatus};

/// The fixtures build no thinking blocks, so both readings agree.
const REPLAY: ThinkingReplay = ThinkingReplay::Dropped;

#[test]
fn pruning_and_repeated_checkpoints_preserve_original_execution_facts() {
    let (mut session, dir) = temp_session("pruned-facts");
    session
        .push_message(Message::user_text("不用 /copy，保留代码缩进"), None, None)
        .unwrap();
    session
        .push_message(
            Message::Assistant {
                content: vec![Block::ToolCall {
                    id: "c".into(),
                    name: "edit".into(),
                    arguments: serde_json::json!({"path":"a.rs"}),
                }],
                stop_reason: Some(StopReason::ToolUse),
            },
            None,
            None,
        )
        .unwrap();
    let original = "重要日志\n".repeat(4000);
    session
        .push_message(
            Message::Tool {
                tool_call_id: "c".into(),
                name: "edit".into(),
                content: original.clone(),
                status: ToolStatus::Error,
            },
            None,
            None,
        )
        .unwrap();
    let outcome =
        crate::agent::compact::prune_tool_results(&session.context_messages(), session.path())
            .unwrap();
    session.push_pruning(outcome).unwrap();
    assert!(session.measured_context_tokens(REPLAY).is_none());
    assert!(session.context_messages()[2].text().contains("中间已裁剪"));
    assert!(session.records().iter().filter_map(Record::message).any(|message| matches!(message, Message::Tool { content, status: ToolStatus::Error, .. } if content == &original)));
    for round in 0..2 {
        let facts = session.checkpoint_facts(4096);
        assert_eq!(facts.user_requests, ["不用 /copy，保留代码缩进"]);
        assert!(
            facts.files.lists().1.is_empty(),
            "failed edit must never become a modified file"
        );
        push_test_compaction(
            &mut session,
            "manual",
            &format!("summary {round}"),
            vec![Message::user_text("模型摘要没有保留用户原话")],
            vec![],
            None,
        );
    }
    let path = session.path().to_path_buf();
    drop(session);
    let resumed = Session::open(&path).unwrap();
    assert_eq!(
        resumed.checkpoint_facts(4096).user_requests,
        ["不用 /copy，保留代码缩进"]
    );
    assert!(resumed.file_operations().lists().1.is_empty());
    assert_eq!(
        resumed.context_messages()[0].text(),
        "模型摘要没有保留用户原话"
    );
    drop(resumed);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_partial_tail_is_listed_and_repaired_before_appending() {
    let (session, dir) = temp_session_with_a_message("partial-tail");
    let path = session.path().to_path_buf();
    drop(session);
    let original = std::fs::read(&path).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"type\":\"response_item\",\"id\":\"cut")
        .unwrap();
    assert_eq!(
        list_in(&dir).len(),
        1,
        "a damaged final write must remain discoverable"
    );
    assert!(
        std::fs::metadata(&path).unwrap().len() > original.len() as u64,
        "listing must not mutate files"
    );
    let mut resumed = Session::open(&path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(!resumed.recovery_notes().is_empty());
    resumed
        .push_message(Message::user_text("继续"), None, None)
        .unwrap();
    drop(resumed);
    let reopened = Session::open(&path).unwrap();
    assert_eq!(reopened.user_history(), vec!["你好", "继续"]);
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn complete_invalid_records_and_interior_damage_are_not_discarded() {
    for (index, tail) in [
        b"{\"type\":\"unknown\"}".as_slice(),
        b"{\"type\":\"response_item\",\n",
        b"broken\n{}\n",
        b"broken\xe4",
    ]
    .into_iter()
    .enumerate()
    {
        let (session, dir) = temp_session_with_a_message(&format!("invalid-tail-{index}"));
        let path = session.path().to_path_buf();
        drop(session);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(tail)
            .unwrap();
        let original = std::fs::read(&path).unwrap();
        assert!(Session::open(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn a_write_cut_inside_a_utf8_character_can_be_recovered() {
    let (mut session, dir) = temp_session_with_a_message("partial-utf8");
    let path = session.path().to_path_buf();
    session
        .push_message(Message::user_text("中文"), None, None)
        .unwrap();
    drop(session);
    let mut bytes = std::fs::read(&path).unwrap();
    let cut = bytes
        .windows("中".len())
        .position(|part| part == "中".as_bytes())
        .unwrap()
        + 1;
    bytes.truncate(cut);
    std::fs::write(&path, bytes).unwrap();
    let resumed = Session::open(&path).unwrap();
    assert_eq!(resumed.user_history(), vec!["你好"]);
    assert!(!resumed.recovery_notes().is_empty());
    drop(resumed);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_unterminated_valid_record_gets_a_newline_before_the_next_write() {
    let (session, dir) = temp_session_with_a_message("no-final-newline");
    let path = session.path().to_path_buf();
    drop(session);
    let mut bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes.pop(), Some(b'\n'));
    std::fs::write(&path, bytes).unwrap();
    let mut resumed = Session::open(&path).unwrap();
    resumed
        .push_message(Message::user_text("继续"), None, None)
        .unwrap();
    drop(resumed);
    let resumed = Session::open(&path).unwrap();
    assert_eq!(resumed.user_history(), vec!["你好", "继续"]);
    drop(resumed);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unfinished_tool_results_are_marked_unknown_once() {
    let (mut session, dir) = temp_session_with_a_message("pending-tools");
    let path = session.path().to_path_buf();
    session
        .push_message(
            Message::Assistant {
                content: vec![
                    Block::ToolCall {
                        id: "first".into(),
                        name: "write".into(),
                        arguments: serde_json::json!({"path":"marker","content":"data"}),
                    },
                    Block::ToolCall {
                        id: "second".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path":"marker"}),
                    },
                ],
                stop_reason: Some(StopReason::ToolUse),
            },
            None,
            Some(StopReason::ToolUse),
        )
        .unwrap();
    session
        .push_message(
            Message::Tool {
                status: ToolStatus::Success,
                tool_call_id: "first".into(),
                name: "write".into(),
                content: "写入成功".into(),
            },
            None,
            None,
        )
        .unwrap();
    drop(session);
    let resumed = Session::open(&path).unwrap();
    let messages = resumed.context_messages();
    crate::llm::validate_tool_history(&messages).unwrap();
    assert!(
        matches!(messages.last(), Some(Message::Tool { tool_call_id, content, .. }) if tool_call_id == "second" && content.contains("执行结果未知"))
    );
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(m, Message::Tool { tool_call_id, .. } if tool_call_id == "first"))
            .count(),
        1
    );
    assert!(
        !dir.join("marker").exists(),
        "recovery must not rerun a tool"
    );
    let count = messages.len();
    drop(resumed);
    let resumed = Session::open(&path).unwrap();
    assert_eq!(resumed.context_messages().len(), count);
    drop(resumed);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_live_session_cannot_be_opened_by_a_second_writer() {
    let (session, dir) = temp_session_with_a_message("writer-lock");
    assert_eq!(
        list_in(&dir).len(),
        1,
        "listing remains read-only while a writer is active"
    );
    assert!(Session::open(session.path()).is_err());
    let path = session.path().to_path_buf();
    drop(session);
    drop(Session::open(&path).unwrap());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn closing_a_session_releases_the_lock_even_with_a_duplicated_descriptor() {
    let (session, dir) = temp_session_with_a_message("duplicated-lock");
    let path = session.path().to_path_buf();
    let Storage::Open(file) = &session.storage else {
        panic!("saved session");
    };
    let inherited = file.try_clone().unwrap();
    drop(session);
    drop(Session::open(&path).unwrap());
    drop(inherited);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn relocation_preserves_the_latest_model_level_and_immutable_header() {
    let (mut session, dir) = temp_session_with_a_message("relocation-model");
    let path = session.path().to_path_buf();
    session
        .push_turn_context(
            &dir,
            "work/changed",
            "high",
            crate::auth::guard::PermissionMode::default(),
        )
        .unwrap();
    let elsewhere = dir.join("elsewhere");
    session.relocate(&elsewhere).unwrap();
    assert_eq!(
        session.current_model(),
        Some(("work/changed".into(), "high".into()))
    );
    assert_eq!(session.header().model, "work/m");
    assert_eq!(session.header().cwd, dir.to_string_lossy());
    drop(session);
    let mut resumed = Session::open(&path).unwrap();
    assert_eq!(
        resumed.current_model(),
        Some(("work/changed".into(), "high".into()))
    );
    assert_eq!(resumed.current_cwd(), Some(elsewhere.clone()));
    let count = resumed.records().len();
    resumed.relocate(&elsewhere).unwrap();
    assert_eq!(resumed.records().len(), count);
    drop(resumed);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn context_pressure_includes_new_input_and_ignores_summary_usage_after_reopen() {
    let (mut session, dir) = temp_session("pressure");
    session
        .push_message(
            Message::assistant_text("answer"),
            Some(Usage {
                input: 100,
                output: 20,
                cache_read: 50,
                cache_write: 0,
            }),
            Some(StopReason::Stop),
        )
        .unwrap();
    let input = Message::user_text("新的输入".repeat(1000));
    let cost = input.estimate_tokens(REPLAY);
    session.push_message(input, None, None).unwrap();
    assert_eq!(session.measured_context_tokens(REPLAY), Some(170 + cost));
    push_test_compaction(
        &mut session,
        "manual",
        "summary",
        vec![Message::user_text("summary")],
        vec![],
        Some(Usage {
            input: 999,
            output: 20,
            cache_read: 0,
            cache_write: 0,
        }),
    );
    let path = session.path().to_path_buf();
    drop(session);
    let reopened = Session::open(&path).unwrap();
    assert_eq!(reopened.measured_context_tokens(REPLAY), None);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn only_the_environment_block_counts_as_having_said_nothing() {
    // What the footer's context field is allowed to call "no conversation yet". A
    // session starts with the block and nothing else, and the first real message — the
    // user's or a resumed history's — is what makes the field worth printing.
    let (mut session, dir) = temp_session("only-environment");
    let block = crate::agent::r#loop::environment_block(&dir, "sid", "/usr/bin/zsh");
    assert!(
        session.context_holds_only_environment(),
        "a session with no records at all has nothing to measure"
    );
    session
        .push_message(Message::user_text(block), None, None)
        .unwrap();
    assert!(session.context_holds_only_environment());
    session
        .push_message(Message::user_text("第一句话"), None, None)
        .unwrap();
    assert!(!session.context_holds_only_environment());
    std::fs::remove_dir_all(dir).unwrap();
}

fn temp_session(name: &str) -> (Session, PathBuf) {
    let dir = std::env::temp_dir().join(format!("pi-session-{}-{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let session = Session::create_in(&dir, &dir, "work/m").unwrap();
    (session, dir)
}

fn push_test_compaction(
    session: &mut Session,
    reason: &str,
    summary: &str,
    replacement_history: Vec<Message>,
    read_files: Vec<String>,
    usage: Option<Usage>,
) {
    let replacement_ids = (0..replacement_history.len())
        .map(|index| format!("test-replacement-{index}"))
        .collect();
    session
        .push_compaction(
            crate::agent::compact::CompactionOutcome {
                compaction_id: "test-compaction".into(),
                summary: summary.to_string(),
                replacement: replacement_history,
                replacement_ids,
                read_files,
                modified_files: vec![],
                shadowed_ids: vec![],
                token_before: 100,
                token_after: 10,
                usage: usage.unwrap_or_default(),
            },
            reason,
            "test-provider",
            "test-model",
            "",
        )
        .unwrap();
}

/// A session that has already said something, and therefore has a file. Tests about
/// listing, finding or deleting a session need one that exists on disk.
fn temp_session_with_a_message(name: &str) -> (Session, PathBuf) {
    let (mut session, dir) = temp_session(name);
    session
        .push_message(Message::user_text("你好"), None, None)
        .unwrap();
    (session, dir)
}

#[test]
fn user_history_holds_what_the_user_typed_and_nothing_else() {
    // Up on a resumed screen reaches back into the conversation. What it must *not*
    // reach is the model's own words: recalling an answer and sending it back would look
    // like the user saying something they never said.
    let (mut session, dir) = temp_session("history");
    session
        .push_message(Message::user_text("第一行\n第二行"), None, None)
        .unwrap();
    session
        .push_message(Message::assistant_text("回答"), None, None)
        .unwrap();
    session
        .push_message(
            Message::Tool {
                status: ToolStatus::Success,
                tool_call_id: "c1".into(),
                name: "bash".into(),
                content: "输出".into(),
            },
            None,
            None,
        )
        .unwrap();
    session
        .push_message(
            Message::System {
                content: "系统".into(),
            },
            None,
            None,
        )
        .unwrap();
    // The environment block is a user message by type but bookkeeping by intent.
    session
        .push_message(
            Message::user_text("<environment>\n工作目录: /tmp"),
            None,
            None,
        )
        .unwrap();

    // Each line of a multi-line message is its own entry: the editor is single-line, so
    // recalling a message that was typed over two lines would drop a newline into a
    // buffer that cannot hold one.
    assert_eq!(session.user_history(), vec!["第一行\n第二行".to_string()]);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn user_history_survives_a_compaction() {
    // Compaction replaces the context with a summary, but the arrows are about what the
    // user typed, and those turns did happen. Reading the records instead of the context
    // is what keeps them reachable.
    let (mut session, dir) = temp_session("history-compacted");
    session
        .push_message(Message::user_text("被压缩掉的话"), None, None)
        .unwrap();
    push_test_compaction(
        &mut session,
        "test",
        "摘要",
        vec![Message::user_text("摘要占位")],
        vec![],
        None,
    );
    assert_eq!(
        session.context_messages().len(),
        1,
        "the summary replaced the turn"
    );
    assert_eq!(session.user_history(), vec!["被压缩掉的话".to_string()]);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_session_can_be_found_by_an_id_prefix() {
    let (session, dir) = temp_session_with_a_message("find");
    let id = session.id().to_string();

    // The full id works, and so does any unique prefix of it — that is what makes the
    // command printed on exit usable without retyping 36 characters.
    assert_eq!(find_by_prefix_in(&dir, &id).unwrap(), session.path());
    assert_eq!(find_by_prefix_in(&dir, &id[..8]).unwrap(), session.path());
    // Whitespace from a sloppy copy-paste is tolerated.
    assert_eq!(
        find_by_prefix_in(&dir, &format!("  {}  ", &id[..8])).unwrap(),
        session.path()
    );

    // An unknown id is an error: silently starting a blank session would hide the typo
    // until the user noticed the missing history.
    assert!(find_by_prefix_in(&dir, "ffffffff").is_err());
    assert!(find_by_prefix_in(&dir, "").is_err());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_ambiguous_prefix_is_refused_rather_than_guessed() {
    let dir = std::env::temp_dir().join(format!("pi-ambig-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Two sessions, so a short prefix can match both. uuids are time-ordered (v7), so
    // sessions created in the same moment share a long leading run — the common prefix
    // is what a user is most likely to type, which is exactly when guessing would be
    // worst.
    // Both sessions must have a file to appear in the list, so both say something.
    for _ in 0..2 {
        let mut session = Session::create_in(&dir, &dir, "work/m").unwrap();
        session
            .push_message(Message::user_text("你好"), None, None)
            .unwrap();
    }

    let ids: Vec<String> = list_in(&dir).into_iter().map(|s| s.id).collect();
    assert_eq!(ids.len(), 2);
    // The longest prefix that still matches both: one character shorter than the point
    // where the two ids diverge.
    let shared = ids
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .iter()
        .fold(usize::MAX, |acc, id| {
            acc.min(
                ids[0]
                    .chars()
                    .zip(id.chars())
                    .take_while(|(x, y)| x == y)
                    .count(),
            )
        });
    let ambiguous = &ids[0][..shared];
    assert!(
        shared > 0 && ids.iter().all(|id| id.starts_with(ambiguous)),
        "the constructor did not produce a shared prefix: {ids:?}"
    );
    let err = find_by_prefix_in(&dir, ambiguous).unwrap_err();
    assert!(err.contains("匹配到"), "{err}");
    assert!(err.contains("请多给几位"), "{err}");

    // A prefix one character longer than the shared run picks exactly one session.
    let unique = &ids[0][..shared + 1];
    assert_eq!(
        find_by_prefix_in(&dir, unique).unwrap(),
        dir_for(&dir, unique)
    );
    // And the full id stays unambiguous.
    assert!(find_by_prefix_in(&dir, &ids[0]).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The path a session with this id prefix lives at.
fn dir_for(dir: &Path, id_prefix: &str) -> PathBuf {
    list_in(dir)
        .into_iter()
        .find(|s| s.id.starts_with(id_prefix))
        .expect("the id names a session")
        .path
}

#[test]
fn deleting_an_unsaved_session_reports_that_there_was_nothing_to_delete() {
    let (mut session, dir) = temp_session("delete-unsaved");
    assert!(!session.is_saved());
    // No file, so nothing was removed — and the caller must be told, or `/delete` would
    // offer an `rm` path that does not exist.
    assert!(!session.delete().unwrap());
    assert!(!session.path().exists());

    // A late write must still not create the file: the user asked for this session to
    // end, and re-creating it minutes later would be exactly what `/delete` prevents.
    let after = session.push_message(Message::user_text("迟到的消息"), None, None);
    assert!(matches!(after, Err(SessionError::Deleted)), "{after:?}");
    assert!(!session.path().exists());
    assert!(list_in(&dir).is_empty());

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_name_alone_does_not_create_a_session_file() {
    // Naming an empty session is not a conversation. It must not leave a file that
    // `/resume` would list with nothing to show.
    let (mut session, dir) = temp_session("name-only");
    session.set_name(Some("只有名字")).unwrap();
    assert!(!session.is_saved(), "a name is not a conversation");
    assert!(list_in(&dir).is_empty());

    // The first message persists the name too, which is what the user set it for.
    session
        .push_message(Message::user_text("你好"), None, None)
        .unwrap();
    assert!(session.is_saved());
    let path = session.path().to_path_buf();
    drop(session);
    let reopened = Session::open(&path).unwrap();
    assert_eq!(reopened.name().as_deref(), Some("只有名字"));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn deleting_removes_the_file_and_refuses_later_writes() {
    let (mut session, dir) = temp_session("delete");
    session
        .push_message(Message::user_text("hello"), None, None)
        .unwrap();
    let path = session.path().to_path_buf();
    assert!(path.is_file());

    assert!(
        session.delete().unwrap(),
        "a saved session had a file to remove"
    );
    assert!(!path.exists(), "the file is gone");

    // A write after the delete must not bring the file back: the user asked for it to
    // be gone, and a late append would quietly recreate it.
    let after = session.push_message(Message::user_text("late"), None, None);
    assert!(matches!(after, Err(SessionError::Deleted)), "{after:?}");
    assert!(!path.exists(), "a refused write must not recreate the file");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn deleting_the_last_session_takes_its_store_directory_with_it() {
    // End to end through the store: create, say something, delete. The session's
    // directory and its row in the table are both gone afterwards.
    let root = std::env::temp_dir().join(format!("pidel{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let store = root.join("store");
    let project = root.join("proj");
    std::fs::create_dir_all(&project).unwrap();

    let mut session = Session::create_in(
        &super::dirs::sessions_dir_in(&store, &project),
        &project,
        "work/m",
    )
    .unwrap();
    session.register_under = Some(store.clone());
    session
        .push_message(Message::user_text("你好"), None, None)
        .unwrap();
    let dir = session.path().parent().unwrap().to_path_buf();
    assert!(dir.is_dir());

    session.delete().unwrap();
    assert!(super::dirs::forget_dir_if_empty_in(&store, &project));
    assert!(!dir.exists());
    assert!(list_in(&store).is_empty());

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_deleted_session_disappears_from_the_resume_list() {
    let (mut session, dir) = temp_session_with_a_message("delete-list");
    session.set_name(Some("要删掉的会话")).unwrap();
    let path = session.path().to_path_buf();
    assert_eq!(list_in(&dir).len(), 1);

    session.delete().unwrap();
    assert!(
        list_in(&dir).is_empty(),
        "the deleted session is no longer offered"
    );
    assert!(!path.exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_header_is_the_first_line_and_is_never_rewritten() {
    let (mut session, dir) = temp_session_with_a_message("header");
    let before = std::fs::read_to_string(session.path()).unwrap();
    session.set_name(Some("我的会话")).unwrap();
    session.set_name(Some("改个名字")).unwrap();
    let after = std::fs::read_to_string(session.path()).unwrap();
    assert!(after.starts_with(&before), "the first line must not change");
    assert_eq!(session.name().as_deref(), Some("改个名字"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_empty_name_clears_the_session_name() {
    let (mut session, dir) = temp_session("clear");
    session.set_name(Some("x")).unwrap();
    session.set_name(None).unwrap();
    assert_eq!(session.name(), None);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn reopening_replays_the_conversation() {
    let (mut session, dir) = temp_session("replay");
    let path = session.path().to_path_buf();
    session
        .push_message(Message::user_text("hello"), None, None)
        .unwrap();
    session
        .push_message(
            Message::assistant_text("hi there"),
            Some(Usage {
                input: 10,
                output: 3,
                cache_read: 0,
                cache_write: 0,
            }),
            Some(StopReason::Stop),
        )
        .unwrap();
    session
        .push_turn_context(
            &dir,
            "work/m",
            "high",
            crate::auth::guard::PermissionMode::default(),
        )
        .unwrap();
    drop(session);

    let reopened = Session::open(&path).unwrap();
    let messages = reopened.context_messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].text(), "hello");
    assert_eq!(messages[1].text(), "hi there");
    // The turn context is stored but is not part of the conversation.
    assert!(
        reopened
            .records()
            .iter()
            .any(|r| matches!(r, Record::TurnContext { .. }))
    );
    assert_eq!(reopened.totals.input, 10);
    assert_eq!(reopened.last_usage.unwrap().output, 3);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_newest_turn_context_names_the_model_in_use() {
    // `/model` changes the model for the rest of the conversation. The header is written
    // once, at creation, and never rewritten — so remembering the choice means reading it
    // back from the turn contexts, and the newest one is the answer.
    let (mut session, dir) = temp_session("model-switch");
    let path = session.path().to_path_buf();
    session
        .push_message(Message::user_text("hello"), None, None)
        .unwrap();
    session
        .push_turn_context(
            &dir,
            "work/first",
            "low",
            crate::auth::guard::PermissionMode::default(),
        )
        .unwrap();
    session
        .push_message(Message::user_text("again"), None, None)
        .unwrap();
    session
        .push_turn_context(
            &dir,
            "work/second",
            "max",
            crate::auth::guard::PermissionMode::default(),
        )
        .unwrap();
    drop(session);

    let reopened = Session::open(&path).unwrap();
    assert_eq!(
        reopened.current_model(),
        Some(("work/second".to_string(), "max".to_string())),
        "the newest recorded turn wins"
    );
    // And it does not leak into the conversation.
    assert_eq!(reopened.context_messages().len(), 2);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn obsolete_session_fields_are_rejected() {
    // Obsolete fields are rejected; the reader has no migration path.
    let dir = std::env::temp_dir().join(format!("pi-oldfile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("old.jsonl");
    std::fs::write(
            &path,
            concat!(
                r#"{"type":"session_meta","format":1,"id":"01a0c9b3-b78c-726b-9e74-fc987dcd42bd","timestamp":"2026-09-22T15:19:53Z","cwd":"/tmp","model":"work/m"}"#,
                "\n",
                r#"{"type":"response_item","parent_id":"a","id":"x1","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
                "\n",
                r#"{"type":"compacted","parent_id":"x1","id":"c1","window_id":"w1","previous_window_id":"w0","reason":"manual","summary":"s","replacement_history":[],"read_files":[],"modified_files":[],"timestamp":"2026-09-22T15:20:00Z"}"#,
                "\n",
            ),
        )
        .unwrap();

    assert!(Session::open(&path).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_session_with_no_turn_context_has_no_recorded_model() {
    // A current session may be saved before the first turn context is recorded.
    let (mut session, dir) = temp_session("no-turn-context");
    let path = session.path().to_path_buf();
    session
        .push_message(Message::user_text("hello"), None, None)
        .unwrap();
    drop(session);

    let reopened = Session::open(&path).unwrap();
    assert_eq!(reopened.current_model(), None);
    assert!(
        !reopened.header().model.is_empty(),
        "the header still names one"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn usage_recorded_before_a_checkpoint_is_not_reused_for_the_threshold() {
    let (mut session, dir) = temp_session("usage");
    session
        .push_message(
            Message::assistant_text("big"),
            Some(Usage {
                input: 900_000,
                output: 100,
                cache_read: 0,
                cache_write: 0,
            }),
            Some(StopReason::Stop),
        )
        .unwrap();
    assert_eq!(session.last_usage.unwrap().input, 900_000);
    push_test_compaction(
        &mut session,
        "manual",
        "summary",
        vec![Message::user_text("hello")],
        vec![],
        None,
    );
    assert!(session.last_usage.is_none());
    assert_eq!(session.context_messages().len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn context_messages_come_from_the_last_checkpoint() {
    let (mut session, dir) = temp_session("checkpoint");
    session
        .push_message(Message::user_text("old one"), None, None)
        .unwrap();
    session
        .push_message(Message::assistant_text("old answer"), None, None)
        .unwrap();
    push_test_compaction(
        &mut session,
        "threshold",
        "summary text",
        vec![Message::user_text("old one")],
        vec!["a.rs".into()],
        None,
    );
    session
        .push_message(Message::user_text("new question"), None, None)
        .unwrap();

    let messages = session.context_messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].text(), "old one");
    assert_eq!(messages[1].text(), "new question");
    // The summary itself is a checkpoint field, not a message in the history.
    assert!(messages.iter().all(|m| !m.text().contains("summary text")));
    let snapshot = session.context_snapshot();
    assert_eq!(snapshot.messages.len(), snapshot.entry_ids.len());
    assert!(snapshot.entry_ids.iter().all(|id| !id.is_empty()));

    // Reopening must produce exactly the same view.
    let path = session.path().to_path_buf();
    drop(session);
    let reopened = Session::open(&path).unwrap();
    assert_eq!(reopened.context_messages().len(), 2);
    let snapshot = reopened.context_snapshot();
    assert_eq!(snapshot.messages.len(), snapshot.entry_ids.len());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_resume_list_skips_the_environment_block() {
    // The environment block is a user message in the file, so treating every user
    // message as the headline would show `<environment> 工作目录: …` instead of what the
    // session was actually about.
    let (mut session, dir) = temp_session("resume-snippet");
    session
        .push_message(Message::user_text("帮我重构 config.rs"), None, None)
        .unwrap();
    session
        .push_message(Message::assistant_text("好，我先读一下"), None, None)
        .unwrap();
    // Session::create writes only the header; the environment block belongs to
    // `Agent::new`, so these two pushes are the whole conversation.
    assert_eq!(session.context_messages().len(), 2);

    let summaries = list_in(&dir);
    assert_eq!(summaries.len(), 1);
    let summary = &summaries[0];
    assert_eq!(
        summary.messages, 2,
        "the environment block must not be counted"
    );
    assert_eq!(summary.snippet, "帮我重构 config.rs", "{}", summary.snippet);
    assert!(!summary.snippet.contains("<environment>"));
    assert_eq!(summary.label(40), "帮我重构 config.rs");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sessions_are_stored_under_the_directory_they_belong_to() {
    // A conversation belongs to the project it happened in. Listing every other
    // project's history buries the relevant ones, and resuming the wrong project's
    // conversation would run its commands against the wrong tree.
    let root = std::env::temp_dir().join(format!("piscope{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let store = root.join("store");
    // The table lives beside the sessions it names, and only gains a directory when one
    // of them actually stores something (see the registration tests below).
    let a = root.join("a");
    let b = root.join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();

    let dir_a = super::dirs::sessions_dir_in(&store, &a);
    let dir_b = super::dirs::sessions_dir_in(&store, &b);
    let mut in_a = Session::create_in(&dir_a, &a, "work/m").unwrap();
    in_a.push_message(Message::user_text("在 a 里"), None, None)
        .unwrap();

    // The two directories are separate stores, so `b` cannot see `a`'s session.
    assert_eq!(list_in(&dir_a).len(), 1);
    assert!(list_in(&dir_b).is_empty());
    // And the id, if it does not match, is an error rather than a guess.
    assert!(find_by_prefix_in(&dir_b, in_a.id()).is_err());
    assert!(find_by_prefix_in(&dir_a, in_a.id()).is_ok());

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn the_same_directory_always_gets_the_same_id() {
    // The id names a directory. Minting a new one per call would scatter one project's
    // sessions across the store, and nothing would be able to find them again.
    let root = std::env::temp_dir().join(format!("pisdir{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let store = root.join("store");
    let project = root.join("proj");
    std::fs::create_dir_all(&project).unwrap();

    let first = super::dirs::register_dir_in(&store, &project);
    assert_eq!(super::dirs::register_dir_in(&store, &project), first);
    assert_eq!(
        super::dirs::dir_id_in(&store, &project).as_deref(),
        Some(first.as_str())
    );
    assert_eq!(
        super::dirs::sessions_dir_in(&store, &project)
            .file_name()
            .unwrap(),
        first.as_str()
    );
    // A table written here is readable by the next process, which is the whole point.
    let index = super::dirs::read_dirs_index_for_test(&store);
    assert_eq!(
        index.get(&first).map(String::as_str),
        Some(project.to_string_lossy().as_ref())
    );

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn merely_looking_at_a_directory_does_not_register_it() {
    // The table says where sessions are, so it must not become a log of everywhere pi
    // has been run. A directory that never produced a session stays out of it, and the
    // id it would have used is not reserved either.
    let root = std::env::temp_dir().join(format!("pilook{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let store = root.join("store");
    let project = root.join("proj");
    std::fs::create_dir_all(&project).unwrap();

    assert!(super::dirs::dir_id_in(&store, &project).is_none());
    // Asking twice gives two ids, which is fine: neither names a real directory.
    let _ = super::dirs::sessions_dir_in(&store, &project);
    let _ = super::dirs::sessions_dir_in(&store, &project);
    assert!(super::dirs::read_dirs_index_for_test(&store).is_empty());
    assert!(super::dirs::dir_id_in(&store, &project).is_none());

    // Creating a session is what registers it, and then the id is stable.
    let id = super::dirs::register_dir_in(&store, &project);
    assert_eq!(
        super::dirs::dir_id_in(&store, &project).as_deref(),
        Some(id.as_str())
    );

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn an_empty_session_never_reaches_the_disk() {
    let (mut session, dir) = temp_session("empty");
    // Starting pi and leaving must not add a session to the list, so nothing is
    // written until there is something to remember.
    assert!(!session.path().exists(), "no file before the first message");
    assert!(list_in(&dir).is_empty());
    assert!(!session.is_saved());

    // The environment block alone does not count: it is bookkeeping every session
    // starts with, not a conversation.
    session
        .push_message(
            Message::user_text("<environment>\n工作目录: /tmp\n</environment>"),
            None,
            None,
        )
        .unwrap();
    assert!(
        !session.path().exists(),
        "the environment block alone is not a session"
    );
    assert!(list_in(&dir).is_empty());

    // The first real message brings the whole beginning with it, in order, so the file
    // is exactly what it would have been had it been written from the start.
    session
        .push_message(Message::user_text("你好"), None, None)
        .unwrap();
    assert!(session.is_saved());
    assert!(session.path().is_file());
    let text = std::fs::read_to_string(session.path()).unwrap();
    let kinds: Vec<&str> = text
        .lines()
        .map(|line| {
            if line.contains("session_meta") {
                "header"
            } else if line.contains("<environment>") {
                "env"
            } else {
                "said"
            }
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["header", "env", "said"],
        "the buffered records are flushed in order, header first"
    );
    assert_eq!(list_in(&dir).len(), 1);
    // Reopening sees the same conversation, header included.
    let path = session.path().to_path_buf();
    let id = session.id().to_string();
    drop(session);
    let reopened = Session::open(&path).unwrap();
    assert_eq!(reopened.id(), id);
    assert_eq!(reopened.context_messages().len(), 2);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn resuming_elsewhere_moves_the_recorded_directory() {
    // The immutable header records where it started; the latest context records moves.
    let (mut session, dir) = temp_session("relocate");
    let elsewhere = std::env::temp_dir().join("pi-relocate-target");
    std::fs::create_dir_all(&elsewhere).unwrap();

    assert!(session.current_cwd().is_none(), "no block yet");
    session.relocate(&elsewhere).unwrap();
    assert_eq!(session.header().cwd, dir.to_string_lossy());
    assert_eq!(session.current_cwd().as_deref(), Some(elsewhere.as_path()));

    let block = crate::agent::r#loop::environment_block(&elsewhere, "sid", "/usr/bin/zsh");
    session
        .push_message(Message::user_text(block), None, None)
        .unwrap();
    assert_eq!(session.current_cwd().as_deref(), Some(elsewhere.as_path()));

    // Relocating to the same place is a no-op.
    let before = session.records().len();
    session.relocate(&elsewhere).unwrap();
    assert_eq!(session.records().len(), before);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn current_cwd_uses_turn_context_instead_of_parsing_conversation_text() {
    let (mut session, dir) = temp_session("cwd-blocks");
    for cwd in ["/tmp/one", "/tmp/two", "/tmp/three"] {
        let block = crate::agent::r#loop::environment_block(Path::new(cwd), "sid", "zsh");
        session
            .push_message(Message::user_text(block), None, None)
            .unwrap();
    }
    assert!(session.current_cwd().is_none());
    session
        .push_turn_context(
            Path::new("/tmp/current"),
            "p/m",
            "high",
            crate::auth::guard::PermissionMode::default(),
        )
        .unwrap();
    assert_eq!(
        session.current_cwd().as_deref(),
        Some(Path::new("/tmp/current")),
        "conversation text is not session metadata"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_permission_mode_is_read_back_from_the_newest_turn() {
    use crate::auth::guard::PermissionMode;
    let (mut session, dir) = temp_session("permission-mode");
    // Nothing recorded yet: the default, which is also what an old file with no such field
    // has to mean.
    assert_eq!(session.current_permission_mode(), PermissionMode::Ask);
    session
        .push_turn_context(Path::new("/tmp/a"), "p/m", "high", PermissionMode::Allow)
        .unwrap();
    assert_eq!(session.current_permission_mode(), PermissionMode::Allow);
    // The newest turn wins: a user who turned it back on must not resume into `Allow`.
    session
        .push_turn_context(Path::new("/tmp/a"), "p/m", "high", PermissionMode::Ask)
        .unwrap();
    assert_eq!(session.current_permission_mode(), PermissionMode::Ask);

    // And it survives a reopen, which is what makes a resume honor the last choice.
    session
        .push_message(Message::user_text("你好"), None, None)
        .unwrap();
    let path = session.path().to_path_buf();
    drop(session);
    let reopened = Session::open(&path).unwrap();
    assert_eq!(reopened.current_permission_mode(), PermissionMode::Ask);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_session_file_without_the_permission_field_still_opens_as_ask() {
    // Files written before `/permissions` existed have no `permission_mode`, and the record
    // denies unknown fields — so the absent *and* the defaulted case both have to parse.
    // Reading such a file back as `Allow` would be a permission change made by an upgrade.
    let dir = std::env::temp_dir().join(format!("pi-perm-legacy-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("legacy.jsonl");
    let line = serde_json::json!({
        "type": "turn_context",
        "parent_id": null,
        "id": "01a11bb4-d484-7656-adc2-8633eea30320",
        "cwd": "/tmp/legacy",
        "model": "p/m",
        "level": "high",
        "timestamp": "2026-10-08T13:29:58Z"
    });
    let header = serde_json::json!({
        "type": "session_meta",
        "id": "01a11bb4-d47b-70e5-a157-23ac6dcfc96f",
        "timestamp": "2026-10-08T13:29:58Z",
        "cwd": "/tmp/legacy",
        "model": "p/m"
    });
    std::fs::write(&path, format!("{header}\n{line}\n")).unwrap();
    let session = Session::open(&path).unwrap();
    assert_eq!(
        session.current_permission_mode(),
        crate::auth::guard::PermissionMode::Ask
    );
    let _ = std::fs::remove_dir_all(dir);
}
