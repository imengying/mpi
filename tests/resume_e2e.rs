use std::io::Write;
use std::process::{Command, Stdio};

use mpi::agent::session::{Record, Session};
use mpi::auth::guard::PermissionMode;
use mpi::llm::Message;

#[test]
fn a_saved_conversation_preserves_its_text_through_all_resume_entry_points() {
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("pi-resume-{}", uuid::Uuid::now_v7()));
    let project = root.join("project");
    let store = root.join(".pi/sessions/project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&store).unwrap();
    let id = "019b0000-0000-7000-8000-000000000001";
    let path = store.join(format!("{id}.jsonl"));

    // A complete persisted conversation, including prose attached to a tool call.
    let records = [
        serde_json::json!({"type":"session_meta","id":id,"timestamp":"2026-11-08T12:00:00Z","cwd":project,"model":"test/m"}),
        serde_json::json!({"type":"response_item","id":"user","parent_id":id,"timestamp":"2026-11-08T12:00:01Z","message":{"role":"user","content":[{"type":"text","text":"BEFORE_UPGRADE"}]}}),
        serde_json::json!({"type":"turn_context","id":"turn","parent_id":"user","timestamp":"2026-11-08T12:00:01Z","cwd":project,"model":"test/m","level":"high","permission_mode":"ask"}),
        serde_json::json!({"type":"request_context","id":"request","parent_id":"turn","timestamp":"2026-11-08T12:00:01Z","request_id":"request","provider":"test","model":"m","level":"high","context_window":null,"token_estimate":123,"system_prompt_hash":null,"tools_hash":"tools","cache_hints":true,"app_version":mpi::cli::version()}),
        serde_json::json!({"type":"response_item","id":"call","parent_id":"request","timestamp":"2026-11-08T12:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"EXPLANATION_BEFORE_TOOL"},{"type":"tool_call","id":"c1","name":"bash","arguments":{"command":"echo TOOL_RESULT"}}],"stop_reason":"tool_use"}}),
        serde_json::json!({"type":"response_item","id":"result","parent_id":"call","timestamp":"2026-11-08T12:00:03Z","message":{"role":"tool","tool_call_id":"c1","name":"bash","content":"TOOL_RESULT\n","status":"success","images":[]}}),
        serde_json::json!({"type":"response_item","id":"answer","parent_id":"result","timestamp":"2026-11-08T12:00:04Z","message":{"role":"assistant","content":[{"type":"text","text":"HISTORICAL_REPLY"}],"stop_reason":"stop"}}),
    ];
    let original: String = records.iter().map(|record| format!("{record}\n")).collect();
    std::fs::write(&path, &original).unwrap();
    std::fs::write(
        root.join(".pi/sessions/dirs.json"),
        serde_json::json!({"project":project}).to_string(),
    )
    .unwrap();
    std::fs::write(
        root.join(".pi/config.json"),
        serde_json::json!({
            "providers":[{"name":"test","api":"responses","base_url":"http://127.0.0.1:1/v1","api_key":"unused","models":[{"id":"m"}]}],
            "default_model":"test/m"
        })
        .to_string(),
    )
    .unwrap();

    let session = Session::open(&path).unwrap();
    assert_eq!(session.current_permission_mode(), PermissionMode::Ask);
    assert_eq!(session.context_messages().len(), 4);
    assert!(session.records().iter().any(|record| matches!(
        record,
        Record::RequestContext { app_version, .. } if app_version == mpi::cli::version()
    )));
    drop(session);

    for (args, input) in [
        (vec!["resume"], "/exit\n"),
        (vec!["resume", &id[..8]], "/exit\n"),
        (vec![], "/resume\n/exit\n"),
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_pi"))
            .args(&args)
            .current_dir(&project)
            .env("HOME", &root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("BEFORE_UPGRADE"), "{args:?}: {text}");
        assert_eq!(
            text.matches("EXPLANATION_BEFORE_TOOL").count(),
            1,
            "{args:?}: {text}"
        );
        assert!(
            text.find("EXPLANATION_BEFORE_TOOL").unwrap() < text.find("echo TOOL_RESULT").unwrap(),
            "{args:?}: {text}"
        );
        assert!(text.contains("HISTORICAL_REPLY"), "{args:?}: {text}");
        assert!(text.contains(&format!("pi resume {id}")), "{text}");
        assert!(!text.contains("暂无历史会话"), "{text}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    // Continuing an existing file preserves its bytes and the user's explicit permissions.
    let mut session = Session::open(&path).unwrap();
    session
        .push_turn_context(&project, "test/m", "high", PermissionMode::Allow)
        .unwrap();
    session
        .push_message(Message::user_text("AFTER_UPGRADE"), None, None)
        .unwrap();
    drop(session);
    let resumed = Session::open(&path).unwrap();
    assert_eq!(resumed.current_permission_mode(), PermissionMode::Allow);
    assert_eq!(resumed.user_history(), ["BEFORE_UPGRADE", "AFTER_UPGRADE"]);
    assert_eq!(resumed.context_messages().len(), 5);
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .starts_with(&original)
    );
    drop(resumed);
    std::fs::remove_dir_all(root).unwrap();
}
