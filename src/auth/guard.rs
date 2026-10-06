//! The gate in front of tool execution.
//!
//! Every tool call passes through [`PermissionGate::check`]. A call the policy allows
//! runs immediately; anything else pops the authorization panel. With no terminal the
//! gate refuses — silently approving a dangerous command in a headless run would defeat
//! the whole point of the policy.
//!
//! Every decision returns owned, vetted arguments for one execution.

use std::path::Path;

use crate::auth::policy::{self, Assessment, Dialect};
use crate::ui::auth_panel::{self, PanelRequest};

/// Why a call was refused, in the words the policy used.
#[derive(Debug)]
pub struct Refusal {
    pub reason: String,
}

impl Refusal {
    /// The text the model receives in place of the tool result.
    ///
    /// It states the refusal and the reason, and stops there. The instruction that used to
    /// trail it ("do not rewrite the command to get around this, and do not retry the same
    /// one") was telling the model what it already knows, and it read as a scolding in the
    /// transcript the user has to look at.
    pub fn message(&self) -> String {
        policy::refusal(Some(&self.reason))
    }
}

pub struct PermissionGate {
    /// When false the panel is never shown and everything risky is refused (headless).
    interactive: bool,
    dialect: Dialect,
}

impl PermissionGate {
    pub fn new(interactive: bool, dialect: Dialect) -> Self {
        PermissionGate {
            interactive,
            dialect,
        }
    }

    /// Return the exact arguments that may execute. No cached approval can be replayed.
    pub fn check(
        &self,
        tool: &str,
        input: &serde_json::Value,
        cwd: &Path,
    ) -> Result<serde_json::Value, Refusal> {
        match policy::assess_tool(tool, input, cwd, self.dialect) {
            Assessment::Allow { safe_command } => {
                let mut approved = input.clone();
                if let Some(command) = safe_command {
                    approved["command"] = command.into();
                }
                Ok(approved)
            }
            Assessment::Ask { reason } => {
                if self.interactive
                    && auth_panel::ask(PanelRequest {
                        body: format!("{reason}\n\n{}", panel_body(tool, input)),
                    }) == auth_panel::Decision::Allow
                {
                    Ok(input.clone())
                } else {
                    Err(Refusal { reason })
                }
            }
        }
    }
}

/// What the panel shows: the command itself for `bash`, the resolved arguments for the
/// file tools. The caller prepends the policy reason so the user can assess the request.
fn panel_body(tool: &str, input: &serde_json::Value) -> String {
    if tool == "bash"
        && let Some(command) = input.get("command").and_then(|v| v.as_str())
    {
        return command.to_string();
    }
    serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_receives_the_vetted_absolute_command() {
        let gate = PermissionGate::new(false, Dialect::Zsh);
        let input = serde_json::json!({"command":"git --no-pager diff --stat"});
        let approved = gate.check("bash", &input, &std::env::temp_dir()).unwrap();
        let command = approved["command"].as_str().unwrap();
        assert!(command.starts_with("'/usr/bin/git'"));
        assert!(command.contains("'--no-ext-diff'"));
        assert!(command.contains("'--no-textconv'"));
        assert!(command.contains("core.hooksPath=/dev/null"));
        assert_eq!(input["command"], "git --no-pager diff --stat");
    }

    #[test]
    fn safe_calls_pass_without_asking() {
        let gate = PermissionGate::new(true, Dialect::Zsh);
        let cwd = std::env::temp_dir();
        let input = serde_json::json!({"command": "ls"});
        let result = gate.check("bash", &input, &cwd);
        assert!(result.is_ok());
    }

    #[test]
    fn headless_refuses_what_needs_approval() {
        let gate = PermissionGate::new(false, Dialect::Zsh);
        let cwd = std::env::temp_dir();
        let input = serde_json::json!({"command": "rm -rf /"});
        let error = gate.check("bash", &input, &cwd).unwrap_err();
        assert!(error.message().starts_with("未获得用户授权，操作未执行（"));
        // The refusal says what happened and why, and no more. An instruction to the model
        // about not retrying used to be appended; it is the model's business and the user
        // had to read it in every refusal.
        assert!(!error.message().contains("请勿改写"));
        assert!(!error.message().contains("不要重试"));
    }

    #[test]
    fn the_panel_shows_the_command_for_bash_and_json_for_files() {
        let bash = serde_json::json!({"command": "rm -rf build"});
        assert_eq!(panel_body("bash", &bash), "rm -rf build");
        let write = serde_json::json!({"path": "a.txt", "content": "hi"});
        let body = panel_body("write", &write);
        assert!(body.contains("\"path\": \"a.txt\""));
    }
}
