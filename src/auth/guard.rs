//! Tool policy and session grants for matching calls.

use crate::auth::policy::{self, Assessment, Dialect};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    AllowSession,
    Deny,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ApprovalKey {
    tool: String,
    arguments: String,
    cwd: PathBuf,
    paths: Vec<PathBuf>,
    workspace: PathBuf,
}

#[derive(Debug)]
pub struct ApprovalRequest {
    pub tool: String,
    pub arguments: serde_json::Value,
    pub cwd: PathBuf,
    pub reason: String,
    key: ApprovalKey,
}

#[derive(Debug)]
pub enum GateAction {
    Ready(serde_json::Value),
    Ask(Box<ApprovalRequest>),
}

/// Why a call was refused, in the words the policy used.
#[derive(Debug)]
pub struct Refusal {
    pub reason: String,
    /// The user asked to stop the turn, rather than deny this one call.
    pub stopped: bool,
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
    remembered: HashSet<ApprovalKey>,
}

impl PermissionGate {
    pub fn new(interactive: bool, dialect: Dialect) -> Self {
        PermissionGate {
            interactive,
            mode: PermissionMode::default(),
            dialect,
            remembered: HashSet::new(),
        }
    }

    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.remembered.clear();
        self.mode = mode;
    }

    pub fn clear_approvals(&mut self) {
        self.remembered.clear();
    }

    pub fn assess(
        &self,
        tool: &str,
        input: &serde_json::Value,
        cwd: &Path,
    ) -> Result<GateAction, Refusal> {
        let assessment = policy::assess_tool(tool, input, cwd, self.dialect);
        let mut arguments = input.clone();
        let reason = match assessment {
            Assessment::Allow { safe_command } => {
                if let Some(command) = safe_command {
                    arguments["command"] = command.into();
                }
                return Ok(GateAction::Ready(arguments));
            }
            Assessment::Ask { reason } => reason,
        };
        if !self.interactive {
            return Err(Refusal {
                reason,
                stopped: false,
            });
        }
        let actual_cwd = if tool == "bash" {
            crate::tools::bash::resolve_cwd(&arguments, cwd).unwrap_or_else(|_| cwd.to_path_buf())
        } else {
            cwd.to_path_buf()
        };
        let actual_cwd = actual_cwd.canonicalize().unwrap_or(actual_cwd);
        let mut identity = arguments.clone();
        if let Some(object) = identity.as_object_mut() {
            object.remove("description");
        }
        let paths = referenced_paths(&arguments, cwd);
        let workspace = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let key = ApprovalKey {
            tool: tool.into(),
            arguments: identity.to_string(),
            cwd: actual_cwd.clone(),
            paths,
            workspace,
        };
        if self.mode == PermissionMode::Allow || self.remembered.contains(&key) {
            return Ok(GateAction::Ready(arguments));
        }
        Ok(GateAction::Ask(Box::new(ApprovalRequest {
            tool: tool.into(),
            arguments,
            cwd: actual_cwd,
            reason,
            key,
        })))
    }

    pub fn resolve(
        &mut self,
        request: ApprovalRequest,
        decision: Decision,
    ) -> Result<serde_json::Value, Refusal> {
        match decision {
            Decision::Allow | Decision::AllowSession => {
                if decision == Decision::AllowSession {
                    // Bound memory; dropping remembered grants only causes another question.
                    if self.remembered.len() >= 256 {
                        self.remembered.clear();
                    }
                    self.remembered.insert(request.key);
                }
                Ok(request.arguments)
            }
            Decision::Deny | Decision::Stop => Err(Refusal {
                reason: request.reason,
                stopped: decision == Decision::Stop,
            }),
        }
    }

    /// Noninteractive policy check, also useful to embedders. UI decisions go through assess/resolve.
    pub fn check(
        &self,
        tool: &str,
        input: &serde_json::Value,
        cwd: &Path,
    ) -> Result<serde_json::Value, Refusal> {
        match self.assess(tool, input, cwd)? {
            GateAction::Ready(arguments) => Ok(arguments),
            GateAction::Ask(request) => Err(Refusal {
                reason: request.reason,
                stopped: false,
            }),
        }
    }
}

fn referenced_paths(input: &serde_json::Value, cwd: &Path) -> Vec<PathBuf> {
    fn visit(value: &serde_json::Value, cwd: &Path, paths: &mut Vec<PathBuf>) {
        match value {
            serde_json::Value::Object(object) => {
                for (key, value) in object {
                    if matches!(key.as_str(), "path" | "cwd") {
                        let values = value
                            .as_array()
                            .map(Vec::as_slice)
                            .unwrap_or(std::slice::from_ref(value));
                        for value in values {
                            if let Some(path) = value.as_str() {
                                let path = policy::resolve_tool_path(path, cwd);
                                paths.push(path.canonicalize().unwrap_or(path));
                            }
                        }
                    } else {
                        visit(value, cwd, paths);
                    }
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    visit(value, cwd, paths);
                }
            }
            _ => {}
        }
    }
    let mut paths = Vec::new();
    visit(input, cwd, &mut paths);
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_approval_matches_the_call_and_one_time_approval_is_not_cached() {
        let mut gate = PermissionGate::new(true, Dialect::Zsh);
        let cwd = std::env::temp_dir();
        let input = serde_json::json!({"command":"touch marker"});
        let ask = |gate: &PermissionGate, input: &serde_json::Value, cwd: &Path| match gate
            .assess("bash", input, cwd)
            .unwrap()
        {
            GateAction::Ask(request) => request,
            other => panic!("expected approval: {other:?}"),
        };
        let request = ask(&gate, &input, &cwd);
        gate.resolve(*request, Decision::Allow).unwrap();
        let request = ask(&gate, &input, &cwd);
        gate.resolve(*request, Decision::AllowSession).unwrap();
        assert!(matches!(
            gate.assess("bash", &input, &cwd).unwrap(),
            GateAction::Ready(_)
        ));
        ask(&gate, &serde_json::json!({"command":"touch changed"}), &cwd);
        ask(&gate, &input, Path::new("/"));
        ask(
            &gate,
            &serde_json::json!({"command":"touch marker", "cwd":"/"}),
            &cwd,
        );
        gate.clear_approvals();
        ask(&gate, &input, &cwd);
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
    fn the_default_mode_is_to_ask() {
        // The whole point of the gate. A default of `Allow` would mean a user who never
        // opened `/permissions` silently stopped being asked.
        assert_eq!(PermissionMode::default(), PermissionMode::Ask);
        assert_eq!(
            PermissionGate::new(true, Dialect::Zsh).mode(),
            PermissionMode::Ask
        );
    }

    #[tokio::test]
    async fn auto_allow_runs_flagged_calls_without_a_panel() {
        let dir = crate::tools::temp_dir("approved-call");
        let cwd = dir.join("workspace");
        std::fs::create_dir(&cwd).unwrap();
        let mut gate = PermissionGate::new(true, Dialect::Zsh);
        let input = serde_json::json!({"command": "printf hi > ../outside"});
        assert!(gate.check("bash", &input, &cwd).is_err());
        gate.set_mode(PermissionMode::Allow);
        let approved = gate.check("bash", &input, &cwd).unwrap();
        assert_eq!(approved, input);
        let context = crate::tools::ToolContext {
            cwd,
            shell: "/bin/sh".into(),
            observed: Default::default(),
            progress: None,
        };
        let output = crate::tools::execute("bash", &approved, &context).await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(std::fs::read_to_string(dir.join("outside")).unwrap(), "hi");
        std::fs::remove_dir_all(dir).unwrap();
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
}
