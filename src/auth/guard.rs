//! The gate in front of tool execution.
//!
//! Every tool call passes through [`PermissionGate::check`]. A call the policy allows
//! runs immediately; anything else pops the authorization panel. With no terminal the
//! gate refuses — silently approving a dangerous command in a headless run would defeat
//! the whole point of the policy.
//!
//! Every decision returns owned, vetted arguments for one execution.

use std::path::Path;

use serde::{Deserialize, Serialize};

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

/// Whether a call the policy wants to ask about gets a panel or runs.
///
/// The default is [`PermissionMode::Ask`]: the policy's question is the whole point of the
/// gate, and a session that silently stopped asking would be the one change a user cannot
/// notice from the transcript.
///
/// [`PermissionMode::Allow`] does not widen what the *policy* allows. A call the policy
/// already approved runs either way, and one it wants to ask about runs without the panel —
/// the difference is the question, not the judgement. Nothing about the vetting changes: the
/// rewriting to quoted absolute paths still happens, because that is what keeps the checked
/// command and the executed command the same one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Ask before anything the policy flags (the default).
    #[default]
    Ask,
    /// Run what the policy flags without asking.
    Allow,
}

impl PermissionMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ask => "需要审核",
            Self::Allow => "自动放行",
        }
    }

    /// The second row of the picker: what picking this answer changes.
    ///
    /// The difference between the two entries *is* the decision — both run the same policy,
    /// so the shared half is said once, in the title, and each row states only its own half.
    /// Two rows that both opened with "标注为需要确认的命令" made the reader compare two
    /// long sentences to find the one word that differs.
    pub fn detail(self) -> &'static str {
        match self {
            Self::Ask => "执行前先问你一句",
            Self::Allow => "直接执行，不再询问",
        }
    }
}

pub struct PermissionGate {
    /// When false the panel is never shown and everything risky is refused (headless).
    interactive: bool,
    /// The user's choice from `/permissions`. Only consulted for calls the policy flags.
    mode: PermissionMode,
    dialect: Dialect,
}

impl PermissionGate {
    pub fn new(interactive: bool, dialect: Dialect) -> Self {
        PermissionGate {
            interactive,
            mode: PermissionMode::default(),
            dialect,
        }
    }

    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
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
                // Headless is checked first and wins over the mode: "no terminal" means
                // there is nobody who could have agreed to anything, and a session resumed
                // without a terminal must not inherit the trust of the one that set the
                // mode. This is the invariant the gate exists for, so the mode cannot
                // weaken it.
                if !self.interactive {
                    return Err(Refusal { reason });
                }
                // The mode only decides whether to ask. Note the rewritten arguments are not
                // produced on this path — the policy asked *about* the call rather than
                // approving a rewritten form of it — so an auto-allowed call runs the
                // arguments as written, exactly as one the user just approved does today.
                if self.mode == PermissionMode::Allow {
                    return Ok(input.clone());
                }
                if auth_panel::ask(PanelRequest {
                    body: panel_body(tool, input),
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
/// file tools.
///
/// Nothing else. The policy's reason used to be prepended, and it was a sentence explaining
/// the command the user is looking at — "重定向可能写入文件或执行脚本" above a command whose
/// redirection is in plain sight. The panel asks one question ("run this?"), so it shows one
/// thing: what would run. A reason still reaches the model in the refusal, which is where it
/// can act on it.
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
    fn the_panel_shows_the_call_and_no_commentary_about_it() {
        // The panel asks one question and shows one thing: what would run. The policy's
        // reason is not part of it — "重定向可能写入文件或执行脚本" above a command whose
        // redirection is right there is the same mistake as a category label in the title.
        let command = "cat data.txt > out.txt";
        let body = panel_body("bash", &serde_json::json!({ "command": command }));
        assert_eq!(body, command);

        // The file tools show their arguments, which is all the panel has to say for them.
        let write = panel_body(
            "write",
            &serde_json::json!({ "path": "a.txt", "content": "hi" }),
        );
        assert!(write.contains("\"path\": \"a.txt\""), "{write}");

        // The reason is not lost, it moved: a refusal still carries it, because the model is
        // the one that has to act on it. A headless gate refuses everything, which is the
        // same refusal an interactive "2" produces.
        let refused = PermissionGate::new(false, Dialect::Zsh)
            .check(
                "bash",
                &serde_json::json!({ "command": command }),
                &std::env::temp_dir(),
            )
            .unwrap_err();
        assert!(
            refused.message().contains("未获得用户授权"),
            "{}",
            refused.message()
        );
    }

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

    #[test]
    fn the_default_mode_is_to_ask() {
        // The whole point of the gate. A default of `Allow` would mean a user who never
        // opened `/permissions` silently stopped being asked.
        assert_eq!(PermissionMode::default(), PermissionMode::Ask);
        assert_eq!(
            PermissionGate::new(true, Dialect::Zsh).mode(),
            PermissionMode::Ask
        );
    }

    #[test]
    fn auto_allow_runs_flagged_calls_without_a_panel() {
        // The mode is consulted only where the policy asked, and there the call runs as
        // written rather than being refused. `rm -rf /` is the strongest example the policy
        // has: it asks, and this is what "do not ask" means for it.
        let mut gate = PermissionGate::new(true, Dialect::Zsh);
        let cwd = std::env::temp_dir();
        let input = serde_json::json!({"command": "rm -rf /"});
        assert!(gate.check("bash", &input, &cwd).is_err());
        gate.set_mode(PermissionMode::Allow);
        let approved = gate.check("bash", &input, &cwd).unwrap();
        assert_eq!(approved, input);
    }

    #[test]
    fn auto_allow_still_rewrites_what_the_policy_approved() {
        // The mode changes whether the user is asked, not how a vetted command is prepared.
        // If it short-circuited the `Allow` arm too, an auto-allowed call would run the raw
        // text while an approved one ran the quoted absolute form — two different commands
        // for the same input, and only one of them checked.
        let mut gate = PermissionGate::new(true, Dialect::Zsh);
        gate.set_mode(PermissionMode::Allow);
        let input = serde_json::json!({"command":"git --no-pager diff --stat"});
        let approved = gate.check("bash", &input, &std::env::temp_dir()).unwrap();
        let command = approved["command"].as_str().unwrap();
        assert!(command.starts_with("'/usr/bin/git'"), "{command}");
    }

    #[test]
    fn a_headless_gate_refuses_even_when_the_mode_allows() {
        // The invariant the gate exists for: with no terminal there is nobody who could have
        // agreed, and a session resumed without one must not inherit the trust of the session
        // that chose the mode. Headless is checked before the mode, so this cannot be relaxed
        // by `/permissions`.
        let mut gate = PermissionGate::new(false, Dialect::Zsh);
        gate.set_mode(PermissionMode::Allow);
        let error = gate
            .check(
                "bash",
                &serde_json::json!({"command": "rm -rf /"}),
                &std::env::temp_dir(),
            )
            .unwrap_err();
        assert!(
            error.message().contains("未获得用户授权"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn the_mode_round_trips_through_serde() {
        // It is written into every `turn_context` line, so the spelling is part of the file
        // format: a rename would silently reset every saved session to the default.
        let json = serde_json::to_string(&PermissionMode::Allow).unwrap();
        assert_eq!(json, "\"allow\"");
        assert_eq!(
            serde_json::from_str::<PermissionMode>(&json).unwrap(),
            PermissionMode::Allow
        );
        assert_eq!(
            serde_json::to_string(&PermissionMode::Ask).unwrap(),
            "\"ask\""
        );
    }
}
