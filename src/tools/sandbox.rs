//! Where a tool's full output is written, and what a call is allowed to touch.
//!
//! There used to be an execution sandbox here: Landlock restricted file access and a seccomp
//! filter denied the network syscalls. It was removed on purpose. It applied to `bash` even
//! when the session was set to approve everything, so a command that needed the network or a
//! write outside the project failed inside the sandbox with no way to tell why from the tool
//! result. The permission to run a command is already decided by the policy gate before the
//! call gets here; running it in a second, invisible jail on top of that only moved the
//! failure to a place the user could not see.

use super::{ToolContext, ToolOutput};
use crate::auth::guard::ApprovedCall;
use std::path::PathBuf;

thread_local! {
    static OUTPUT_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Where the untruncated output of a tool call is written.
pub(crate) fn output_dir() -> PathBuf {
    OUTPUT_DIR
        .with(|dir| dir.borrow().clone())
        .unwrap_or_else(std::env::temp_dir)
}

/// Run one approved call.
///
/// The name is kept so the agent loop does not have to know that isolation is gone: it still
/// routes every call through here, and this is the one place that would have to change if a
/// sandbox came back.
pub async fn execute(name: &str, call: ApprovedCall, context: ToolContext) -> ToolOutput {
    super::execute(name, &call.arguments, &context).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_bash_call_can_write_outside_the_project() {
        // The bug this pins: the sandbox denied writes outside the project and network
        // sockets even when the session approved everything, so a command that needed either
        // failed with no way to tell why. Removal has to be behavioural, not just a deleted
        // dependency: this runs a real command through the real entry point.
        let dir = crate::tools::temp_dir("nosandbox");
        let context = ToolContext {
            cwd: dir.clone(),
            shell: "/bin/sh".into(),
            observed: Default::default(),
        };
        let outside = std::env::temp_dir().join(format!("pi-outside-{}", uuid::Uuid::now_v7()));
        let command = format!("printf hi > {}", outside.display());
        let call = ApprovedCall {
            arguments: serde_json::json!({ "command": command }),
            scope: crate::auth::guard::Scope::Workspace,
        };
        let out = execute("bash", call, context).await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "hi");
        let _ = std::fs::remove_file(&outside);
    }
}
