//! End-to-end check that a `bash` call's `cwd` reaches the child process.
//!
//! The policy tests cover whether a `cwd` is *allowed*; these cover that the directory it
//! approved is the directory the command actually runs in. The two are separate code paths
//! (`auth::policy::assess_bash_cwd` decides, `tools::bash::resolve_cwd` runs) and a
//! divergence between them is exactly the bug worth catching: a call vetted against one
//! directory but executed in another.

use mpi::tools::{self, ToolContext};
use std::path::PathBuf;

fn dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("pi-cwd-e2e-{}-{}", std::process::id(), name));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn context(cwd: &std::path::Path) -> ToolContext {
    ToolContext {
        cwd: cwd.to_path_buf(),
        shell: "/usr/bin/zsh".to_string(),
    }
}

async fn run(cwd: &std::path::Path, arguments: serde_json::Value) -> tools::ToolOutput {
    tools::execute("bash", &arguments, &context(cwd)).await
}

#[tokio::test]
async fn a_command_without_cwd_runs_in_the_session_directory() {
    let session = dir("plain");
    let out = run(&session, serde_json::json!({"command": "pwd"})).await;
    assert!(!out.is_error, "{}", out.content);
    // zsh reports the resolved path; the temp dir may itself be a symlink on macOS.
    let want = std::fs::canonicalize(&session).unwrap();
    let got = std::fs::canonicalize(out.content.trim()).unwrap();
    assert_eq!(got, want);
}

#[tokio::test]
async fn a_cwd_argument_is_where_the_command_actually_runs() {
    let session = dir("session");
    let elsewhere = dir("elsewhere");
    let out = run(
        &session,
        serde_json::json!({"command": "pwd", "cwd": elsewhere.to_string_lossy()}),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    let want = std::fs::canonicalize(&elsewhere).unwrap();
    let got = std::fs::canonicalize(out.content.trim()).unwrap();
    assert_eq!(got, want, "the command did not run where it said it would");
    assert_ne!(got, std::fs::canonicalize(&session).unwrap());
}

#[tokio::test]
async fn relative_paths_resolve_against_the_cwd_not_the_session() {
    // The point of the parameter: `cat marker` has to find the file in the directory the
    // call named. If it resolved against the session instead, the parameter would be
    // cosmetic while the command silently read somewhere else.
    let session = dir("session-rel");
    let elsewhere = dir("elsewhere-rel");
    std::fs::write(elsewhere.join("marker"), "from-elsewhere\n").unwrap();
    std::fs::write(session.join("marker"), "from-session\n").unwrap();

    let out = run(
        &session,
        serde_json::json!({"command": "cat marker", "cwd": elsewhere.to_string_lossy()}),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.contains("from-elsewhere"), "{}", out.content);
}

#[tokio::test]
async fn a_missing_cwd_is_an_error_rather_than_a_silent_fallback() {
    // Falling back to the session directory would run the command somewhere the caller did
    // not ask for. The refusal has to reach the model so it can correct the path.
    let session = dir("session-missing");
    let out = run(
        &session,
        serde_json::json!({
            "command": "pwd",
            "cwd": session.join("no-such-dir").to_string_lossy(),
        }),
    )
    .await;
    assert!(out.is_error);
    assert!(out.content.contains("cwd"), "{}", out.content);
}

#[tokio::test]
async fn an_empty_cwd_means_the_session_directory() {
    let session = dir("session-empty");
    let out = run(
        &session,
        serde_json::json!({"command": "pwd", "cwd": "   "}),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    let want = std::fs::canonicalize(&session).unwrap();
    let got = std::fs::canonicalize(out.content.trim()).unwrap();
    assert_eq!(got, want);
}

/// The two zsh defaults that turned working commands into failures.
///
/// These go through `tools::execute`, so they cover the whole path a real call takes —
/// including the authorization rewrite, which re-quotes the command before it is handed to
/// the shell. A flag that only works on the un-rewritten form would pass a unit test on
/// `execute_with_shell` and still fail here.
#[tokio::test]
async fn an_unmatched_glob_and_a_run_of_equals_signs_both_reach_the_command() {
    let session = dir("zsh-defaults");
    std::fs::write(session.join("hit.rs"), "needle-in-glob\n").unwrap();

    // `--include=*.rs` is a bash habit; zsh aborts the command on the unmatched glob.
    let globbed = run(
        &session,
        serde_json::json!({"command": "grep -rn needle-in-glob . --include=*.rs"}),
    )
    .await;
    assert!(!globbed.is_error, "{}", globbed.content);
    assert!(
        globbed.content.contains("needle-in-glob"),
        "{}",
        globbed.content
    );

    // `echo ===` as a separator: `=cmd` is an expansion in zsh, not a literal.
    let equals = run(&session, serde_json::json!({"command": "echo ==="})).await;
    assert!(!equals.is_error, "{}", equals.content);
    assert!(equals.content.contains("==="), "{}", equals.content);
}
