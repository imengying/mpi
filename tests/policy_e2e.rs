//! End-to-end checks on the authorization policy, with no model in the loop.
//! These are the invariants the whole gate depends on, so they are asserted directly.

use mpi::auth::policy::{assess_command, assess_tool, Dialect};
use std::path::Path;

fn cwd() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("mpi-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn must_ask(command: &str) -> String {
    let decision = assess_command(command, &cwd(), Dialect::Zsh);
    decision
        .reason()
        .unwrap_or_else(|| panic!("`{command}` was auto-approved but must ask"))
        .to_string()
}

#[test]
fn destructive_commands_always_ask() {
    for command in [
        "rm -rf build/",
        "rm build/out.o",
        "rm -r build",
        "rmdir build",
        "unlink build/out.o",
        "shred -u build/out.o",
        "sudo rm -rf /",
        "dd if=/dev/zero of=/dev/sda",
        "mkfs.ext4 /dev/sda1",
        "git commit -m x",
        "git push",
        "curl http://example.com",
        "wget http://example.com/x",
        "ssh host ls",
        "chmod +x x",
        "mv build /tmp/elsewhere",
        "truncate -s0 build/out.o",
        "tee build/out.o",
        "python3 -c 'import shutil; shutil.rmtree(\"build\")'",
        "sh -c 'rm -rf build'",
        "zsh -c rm -rf build",
    ] {
        let reason = must_ask(command);
        assert!(!reason.is_empty(), "{command} had an empty reason");
    }
}

#[test]
fn only_the_vetted_read_commands_are_auto_approved() {
    for command in [
        "pwd", "ls", "ls -la", "cat Cargo.toml", "head -5 src/main.rs",
        "wc -l src/main.rs", "stat Cargo.toml", "du -sh src", "file Cargo.toml",
        "rg pattern src", "grep -n pattern src/main.rs", "find . -name '*.rs'",
        "sort src/main.rs", "uname -a", "df -h", "echo hi", "printf x",
        "sed -n '1,5p' src/main.rs", "git status", "git log --oneline -5",
        "git diff --stat", "git rev-parse HEAD", "git ls-files",
    ] {
        let decision = assess_command(command, &cwd(), Dialect::Zsh);
        assert!(
            decision.allows(),
            "`{command}` should be auto-approved, but asked: {:?}",
            decision.reason()
        );
    }
}

#[test]
fn deleting_a_directory_inside_the_project_still_asks() {
    // The gate is about the *command*, not the path: `rm` is never auto-approved.
    let dir = cwd();
    let reason = must_ask(&format!("rm -rf {}", dir.join("build").display()));
    assert!(!reason.is_empty());
}

#[test]
fn a_write_inside_the_project_is_allowed_but_outside_it_asks() {
    let dir = cwd();
    let inside = serde_json::json!({"path": "notes.txt", "content": "x"});
    assert!(assess_tool("write", &inside, &dir, Dialect::Zsh).allows());
    let outside = serde_json::json!({"path": "/tmp/elsewhere.txt", "content": "x"});
    assert!(!assess_tool("write", &outside, &dir, Dialect::Zsh).allows());
}

#[test]
fn reading_secrets_asks_but_reading_source_does_not() {
    let dir = cwd();
    for path in ["Cargo.toml", "src/main.rs", "."] {
        let input = serde_json::json!({"path": path});
        assert!(assess_tool("read", &input, &dir, Dialect::Zsh).allows(), "{path}");
    }
    for path in [
        "~/.ssh/id_rsa",
        "~/.pi/agent/auth.json",
        "~/.codex/config.toml",
        ".env",
        "server.pem",
        "/etc/shadow",
    ] {
        let input = serde_json::json!({"path": path});
        assert!(!assess_tool("read", &input, &dir, Dialect::Zsh).allows(), "{path}");
    }
}

#[test]
fn headless_refuses_rather_than_approving() {
    use mpi::auth::guard::PermissionGate;
    let dir = cwd();
    let mut gate = PermissionGate::new(false, Dialect::Zsh);
    let input = serde_json::json!({"command": "rm -rf build"});
    let refusal = futures_block(gate.check("c1", "bash", &input, &dir)).unwrap_err();
    assert_eq!(
        refusal.message(),
        "未获得用户授权，操作未执行（命令会删除文件，需要确认目标）。请勿改写命令绕过授权，也不要重试同一条命令。"
    );
}

#[test]
fn a_directory_without_agents_md_has_no_system_prompt() {
    // mpi ships no prompt of its own, so the "no system message" case is a real, reachable
    // state rather than a degenerate one — and it is the one a fresh directory hits.
    let dir = std::env::temp_dir().join(format!("mpi-e2e-no-agents-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(mpi::agent::r#loop::load_agents_md(&dir).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

fn futures_block<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

#[test]
fn the_policy_reports_a_reason_for_every_refusal() {
    // A refusal without a reason is what makes a model retry the same command forever.
    for command in ["rm -rf /", "sudo ls", "curl x", "cat ~/.ssh/id_rsa", "ls *.rs"] {
        let reason = must_ask(command);
        assert!(!reason.trim().is_empty(), "{command}");
    }
    let _ = Path::new(".");
}
